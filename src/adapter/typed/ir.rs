//! Resolve-driven IR for the typed codegen pipeline.
//!
//! Each user-declared WIT type produces a [`NamedType`] carrying both
//! the WIT-side kind (record / variant / enum / flags) and the Rust
//! ident wit-bindgen emitted for it, recovered by probing the
//! [`BindingsIndex`]. Per-method args structs are synthesized into
//! the same IR so the emitter dispatches on a single [`NamedKind`]
//! match.

use std::collections::HashSet;

use anyhow::{anyhow, bail, Context, Result};
use heck::{ToShoutySnakeCase, ToSnakeCase, ToUpperCamelCase};
use proc_macro2::{Span, TokenStream};
use quote::quote;
use wit_parser::{
    Function, FunctionKind, Handle, Interface, InterfaceId, Resolve, Type, TypeDefKind, TypeId,
    TypeOwner, WorldId, WorldItem,
};

use super::bindings_index::{bindings_path_tokens, BindingsItem, BindingsPath, WrapperBindings};

/// Per-interface metadata collected from the world: WIT InterfaceId,
/// the Rust module path wit-bindgen emits the iface under, and which
/// side (export/import) it lives on.
///
/// An interface present in both sides of a world is classified as
/// exported — wit-bindgen puts shared types under `exports::` and
/// `pub use`s them on the import side, so the export path is canonical.
struct IfaceEntry {
    id: InterfaceId,
    path: BindingsPath,
    is_export: bool,
}

/// Walk the world and build the IR.
pub fn build_ir(
    resolve: &Resolve,
    world_id: WorldId,
    bindings: &WrapperBindings,
) -> Result<WrapperIR> {
    let world = &resolve.worlds[world_id];

    // Walk exports first, then imports — when an interface is on
    // both sides, the export entry is canonical (wit-bindgen puts
    // shared types under `exports::` and `pub use`s them on the
    // import side). `seen_iface_ids.insert` both records and dedupes.
    let mut ifaces: Vec<IfaceEntry> = Vec::new();
    let mut seen_iface_ids: HashSet<InterfaceId> = HashSet::new();
    let exports = world.exports.iter().map(|(_, item)| (item, true));
    let imports = world.imports.iter().map(|(_, item)| (item, false));
    for (item, is_export) in exports.chain(imports) {
        let id = require_iface(item)?;
        if !seen_iface_ids.insert(id) {
            continue;
        }
        let path = module_path_for_interface(resolve, id, is_export).with_context(|| {
            let side = if is_export { "exported" } else { "imported" };
            format!("could not derive module path for {side} interface")
        })?;
        ifaces.push(IfaceEntry {
            id,
            path,
            is_export,
        });
    }

    // Build the user-declared types list. Dedupe by (path, ident) —
    // a type referenced from multiple interfaces that wit-bindgen
    // `pub use`s under the same Rust path emits once.
    let mut types: Vec<NamedType> = Vec::new();
    let mut resources: Vec<ResourceInfo> = Vec::new();
    let mut seen: HashSet<(BindingsPath, String)> = HashSet::new();
    // Resources are deduped by `TypeId`, not by `(iface_path,
    // ident)` — a resource that gets `use`d into a second iface
    // shares its declaring iface's `TypeId` and would otherwise
    // emit a duplicate wrapper newtype here.
    let mut seen_resource_ids: HashSet<TypeId> = HashSet::new();
    for entry in &ifaces {
        let iface = &resolve.interfaces[entry.id];
        for (wit_name, type_id) in &iface.types {
            // Follow `Type(_)` aliases to find the original
            // declaration. WIT's `use types.{bucket}` materializes
            // as a local alias whose `kind = Type(original_id)` and
            // whose `owner` is the using iface; the resource itself
            // is at the original id with `kind = Resource`.
            let (resolved_id, td) = resolve_through_aliases(resolve, *type_id);
            if matches!(td.kind, TypeDefKind::Resource) {
                if !seen_resource_ids.insert(resolved_id) {
                    continue;
                }
                // Anchor on the *declaring* iface's path (via
                // `TypeOwner`). For wasi-style factored types the
                // declaring iface may be on the wrapper world's
                // import side (e.g. tier-3 wraps `async-bucket` while
                // `bucket` lives in imported `async-bucket-types`);
                // the import-side `IfaceEntry` provides the right
                // module path either way.
                let (declaring_path, is_owned) = match td.owner {
                    TypeOwner::Interface(declaring_id) => {
                        let declaring_entry = ifaces.iter().find(|e| e.id == declaring_id);
                        let path = declaring_entry
                            .map(|e| e.path.clone())
                            .unwrap_or_else(|| entry.path.clone());
                        let owned = declaring_entry.map(|e| e.is_export).unwrap_or(false);
                        (path, owned)
                    }
                    _ => (entry.path.clone(), entry.is_export),
                };
                let rust_ident_str = wit_name.to_upper_camel_case();
                resources.push(ResourceInfo {
                    iface_path: declaring_path,
                    wit_name: wit_name.clone(),
                    rust_ident: syn::Ident::new(&rust_ident_str, Span::call_site()),
                    is_owned,
                });
                continue;
            }
            let nt = build_named_type(resolve, &ifaces, *type_id, wit_name, &entry.path, bindings)?;
            if let Some(nt) = nt {
                let key = (
                    match &nt.location {
                        TypeLocation::InBindings { path } => path.clone(),
                        TypeLocation::TopLevel => Vec::new(),
                    },
                    nt.rust_ident.to_string(),
                );
                if seen.insert(key) {
                    types.push(nt);
                }
            }
        }
    }

    // Synthesize one args record per exported function. Freestanding
    // funcs get `<IfacePascal><FnPascal>Args` (matching what the
    // method emitter looks up); resource methods/constructors/statics
    // get `<ResourcePascal><FnPascal>Args` so the same resource on
    // two interfaces won't collide on a shared interface prefix.
    let mut args_records: Vec<NamedType> = Vec::new();
    let mut fn_sigs: std::collections::HashMap<String, ExportFnSig> =
        std::collections::HashMap::new();
    for entry in ifaces.iter().filter(|e| e.is_export) {
        let iface = &resolve.interfaces[entry.id];
        let iface_pascal = iface
            .name
            .as_ref()
            .ok_or_else(|| anyhow!("exported interface has no name"))?
            .to_upper_camel_case();
        for (fn_name, func) in &iface.functions {
            let prefix = match &func.kind {
                FunctionKind::Freestanding | FunctionKind::AsyncFreestanding => {
                    iface_pascal.clone()
                }
                FunctionKind::Method(id)
                | FunctionKind::AsyncMethod(id)
                | FunctionKind::Static(id)
                | FunctionKind::AsyncStatic(id)
                | FunctionKind::Constructor(id) => resource_pascal(resolve, *id)?,
            };
            // wit-parser names resource fns with the canonical-ABI
            // mangling (e.g. `[method]bucket.get`,
            // `[constructor]bucket`); wit-bindgen emits the Rust
            // trait method without that prefix and with constructors
            // renamed to `new`. The IR pins the wit-bindgen-emitted
            // name as the args-ident source so the two walks line up.
            let rust_method_name = rust_method_name_for(func, fn_name);
            let args = synth_args_record(resolve, &prefix, &rust_method_name, func, &ifaces)?;
            let args_ident = args_struct_ident(&prefix, &rust_method_name);
            let return_ty = func
                .result
                .as_ref()
                .map(|t| type_to_ref(resolve, &ifaces, t))
                .transpose()?;
            let kind = ExportFnKind::from(&func.kind);
            fn_sigs.insert(args_ident.to_string(), ExportFnSig { return_ty, kind });
            args_records.push(args);
        }
    }

    Ok(WrapperIR {
        types,
        resources,
        args_records,
        fn_sigs,
    })
}

/// Walk `Type(_)` aliases to find a type's original declaration.
/// WIT's `use types.{R}` produces a local alias whose `kind =
/// Type(original_id)` and whose `owner` is the using interface; the
/// real `Resource` (or `Record`, etc.) lives at the original id.
fn resolve_through_aliases(
    resolve: &Resolve,
    mut type_id: TypeId,
) -> (TypeId, &wit_parser::TypeDef) {
    loop {
        let td = &resolve.types[type_id];
        if let TypeDefKind::Type(wit_parser::Type::Id(next)) = td.kind {
            type_id = next;
            continue;
        }
        return (type_id, td);
    }
}

fn resource_pascal(resolve: &Resolve, type_id: TypeId) -> Result<String> {
    let td = &resolve.types[type_id];
    let name = td
        .name
        .as_ref()
        .ok_or_else(|| anyhow!("resource type has no name"))?;
    Ok(name.to_upper_camel_case())
}

/// Map a wit-parser function name to the Rust method name
/// wit-bindgen-rust emits for it:
///
/// - `Freestanding` / `AsyncFreestanding`: name is already the WIT
///   ident (e.g. `"open"`).
/// - `Constructor`: wit-bindgen renames every constructor to `new`.
/// - `Method` / `Static` (sync + async): the wit-parser name carries
///   a `[method]<resource>.` or `[static]<resource>.` mangling
///   prefix; strip it to recover the WIT method ident.
fn rust_method_name_for(func: &Function, fallback_name: &str) -> String {
    match &func.kind {
        FunctionKind::Constructor(_) => "new".to_string(),
        FunctionKind::Method(_) | FunctionKind::AsyncMethod(_) => fallback_name
            .rsplit_once('.')
            .map(|(_, m)| m.to_string())
            .unwrap_or_else(|| fallback_name.to_string()),
        FunctionKind::Static(_) | FunctionKind::AsyncStatic(_) => fallback_name
            .rsplit_once('.')
            .map(|(_, m)| m.to_string())
            .unwrap_or_else(|| fallback_name.to_string()),
        FunctionKind::Freestanding | FunctionKind::AsyncFreestanding => fallback_name.to_string(),
    }
}

/// The complete IR for one wrapper crate.
pub struct WrapperIR {
    /// User-declared WIT types reachable from the world. Type
    /// aliases are transparent and not listed; resources are tracked
    /// separately via [`Self::resources`].
    pub types: Vec<NamedType>,
    /// WIT resources declared in exported interfaces. The codegen
    /// emits one wrapper newtype + `GuestBucket`-style impl per
    /// entry; resources do NOT get `WitTyped` impls.
    pub resources: Vec<ResourceInfo>,
    /// One synthesized args record per exported function (freestanding
    /// or resource method/constructor/static).
    pub args_records: Vec<NamedType>,
    /// Per-fn return type + WIT kind, keyed by the synth args struct
    /// ident's string form. Carries enough wit-parser info downstream
    /// so emit_method can branch on the *authoritative* kind instead
    /// of re-deriving from Rust method names.
    pub fn_sigs: std::collections::HashMap<String, ExportFnSig>,
}

pub struct ExportFnSig {
    pub return_ty: Option<WitTypeRef>,
    pub kind: ExportFnKind,
}

/// wit-parser's `FunctionKind` collapsed to what the emitter actually
/// branches on. `Async*` variants fold into their sync counterparts;
/// the async-ness is already encoded in the trait method's syn sig.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum ExportFnKind {
    Freestanding,
    Method,
    Constructor,
    Static,
}

impl From<&FunctionKind> for ExportFnKind {
    fn from(k: &FunctionKind) -> Self {
        match k {
            FunctionKind::Freestanding | FunctionKind::AsyncFreestanding => Self::Freestanding,
            FunctionKind::Method(_) | FunctionKind::AsyncMethod(_) => Self::Method,
            FunctionKind::Static(_) | FunctionKind::AsyncStatic(_) => Self::Static,
            FunctionKind::Constructor(_) => Self::Constructor,
        }
    }
}

/// A WIT resource reachable from the wrapper world.
pub struct ResourceInfo {
    /// `bindings::exports::...::store` for resources the wrapper
    /// owns (the iface that DECLARES them is on the export side);
    /// `bindings::...::store` (no `exports::`) for resources the
    /// wrapper merely USES (declared in an iface only on the
    /// import side, as for tier-3 wraps over factored types where
    /// the inner producer owns the resource).
    pub iface_path: BindingsPath,
    /// Kebab-case WIT name, e.g. `"bucket"`.
    #[allow(dead_code)]
    pub wit_name: String,
    /// PascalCase Rust ident wit-bindgen emits for the resource type.
    pub rust_ident: syn::Ident,
    /// True iff the wrapper EXPORTS the iface that declares this
    /// resource (the wrapper is the canonical type owner and emits
    /// a `WrapperR` newtype + `GuestR` impl). False for resources
    /// the wrapper only consumes from an imported iface.
    pub is_owned: bool,
}

/// A named entity that gets a `WitTyped` impl: either a WIT-declared
/// type or a synthesized args record.
pub struct NamedType {
    pub location: TypeLocation,
    pub rust_ident: syn::Ident,
    pub kind: NamedKind,
}

pub enum TypeLocation {
    /// Inside `mod bindings::<path>` (wit-bindgen output).
    InBindings { path: BindingsPath },
    /// Top-level in the wrapper crate.
    TopLevel,
}

impl NamedType {
    /// Fully-qualified Rust path the emitter writes to refer to this
    /// type. E.g. `bindings::exports::pkg::ops::Point` for a user
    /// type, or `OpsAddArgs` for a top-level synthesized args struct.
    pub fn rust_path_tokens(&self) -> TokenStream {
        let ident = &self.rust_ident;
        match &self.location {
            TypeLocation::InBindings { path } => bindings_path_tokens(path, Some(ident)),
            TypeLocation::TopLevel => quote!(#ident),
        }
    }
}

pub enum NamedKind {
    Record { fields: Vec<RecordField> },
    Variant { cases: Vec<VariantCase> },
    Enum { cases: Vec<EnumCase> },
    Flags { members: Vec<FlagMember> },
}

pub struct RecordField {
    /// Kebab-cased WIT field name (e.g. `"pet-name"`).
    pub wit_name: String,
    /// snake_case Rust ident, with a trailing `_` for Rust-keyword
    /// collisions (wit-bindgen's convention; pinned in bindgen tests).
    pub rust_ident: syn::Ident,
    pub ty: WitTypeRef,
}

pub struct VariantCase {
    pub wit_name: String,
    pub rust_ident: syn::Ident,
    pub payload: Option<WitTypeRef>,
}

pub struct EnumCase {
    pub wit_name: String,
    pub rust_ident: syn::Ident,
}

pub struct FlagMember {
    pub wit_name: String,
    /// SHOUTING_SNAKE_CASE const ident inside the `bitflags!` body.
    pub rust_ident: syn::Ident,
}

pub struct NamedRef {
    pub path: BindingsPath,
    pub rust_ident: syn::Ident,
}

/// A reference to a WIT type at a field, arg, or return position.
pub enum WitTypeRef {
    Primitive(Prim),
    List(Box<WitTypeRef>),
    Option(Box<WitTypeRef>),
    Result {
        ok: Option<Box<WitTypeRef>>,
        err: Option<Box<WitTypeRef>>,
    },
    Tuple(Vec<WitTypeRef>),
    Named(NamedRef),
    Handle(HandleRef),
}

/// Categorizes the four canonical-ABI handle kinds.
#[allow(dead_code)]
pub enum HandleRef {
    ErrorContext,
    Future(Box<WitTypeRef>),
    Stream(Box<WitTypeRef>),
    ResourceOwn(NamedRef),
    ResourceBorrow(NamedRef),
}

impl HandleRef {
    fn to_tokens(&self) -> TokenStream {
        match self {
            HandleRef::ErrorContext => quote!(::wit_bindgen::rt::async_support::ErrorContext),
            // `own<R>` lowers to the bare resource type
            // (`bindings::iface::Bucket`).
            HandleRef::ResourceOwn(nr) => bindings_path_tokens(&nr.path, Some(&nr.rust_ident)),
            // `borrow<R>` lowers to wit-bindgen's `BucketBorrow<'a>`
            // companion struct, with the lifetime hardcoded to `'a`.
            HandleRef::ResourceBorrow(nr) => {
                let borrow_ident =
                    syn::Ident::new(&format!("{}Borrow", nr.rust_ident), Span::call_site());
                let path = bindings_path_tokens(&nr.path, Some(&borrow_ident));
                let lt = syn::Lifetime::new("'a", Span::call_site());
                quote!(#path<#lt>)
            }
            HandleRef::Future(_) | HandleRef::Stream(_) => {
                unreachable!("future/stream handle kind not yet supported")
            }
        }
    }
}

impl WitTypeRef {
    /// Render as Rust source. E.g. `u32`, `Vec<u32>`,
    /// `Option<String>`, `Result<u32, String>`, `(u32, String)`,
    /// `bindings::pkg::ops::Point`.
    pub fn to_tokens(&self) -> TokenStream {
        match self {
            WitTypeRef::Primitive(p) => p.to_tokens(),
            WitTypeRef::List(inner) => {
                let t = inner.to_tokens();
                quote!(::std::vec::Vec<#t>)
            }
            WitTypeRef::Option(inner) => {
                let t = inner.to_tokens();
                quote!(::core::option::Option<#t>)
            }
            WitTypeRef::Result { ok, err } => {
                let ok_ty = match ok {
                    Some(t) => t.to_tokens(),
                    None => quote!(()),
                };
                let err_ty = match err {
                    Some(t) => t.to_tokens(),
                    None => quote!(()),
                };
                quote!(::core::result::Result<#ok_ty, #err_ty>)
            }
            WitTypeRef::Tuple(elems) => {
                let ts: Vec<_> = elems.iter().map(|t| t.to_tokens()).collect();
                if ts.len() == 1 {
                    let t = &ts[0];
                    quote!((#t,))
                } else {
                    quote!((#(#ts),*))
                }
            }
            WitTypeRef::Named(NamedRef { path, rust_ident }) => {
                bindings_path_tokens(path, Some(rust_ident))
            }
            WitTypeRef::Handle(h) => h.to_tokens(),
        }
    }

    /// True if this type tree mentions any [`HandleRef`]. Used by
    /// the emitter to suppress `WitTyped` impl emission when a
    /// synthesized args struct carries a handle (handles aren't
    /// `WitTyped`).
    pub fn contains_handle(&self) -> bool {
        match self {
            WitTypeRef::Primitive(_) | WitTypeRef::Named(_) => false,
            WitTypeRef::Handle(_) => true,
            WitTypeRef::List(inner) | WitTypeRef::Option(inner) => inner.contains_handle(),
            WitTypeRef::Result { ok, err } => {
                ok.as_ref().is_some_and(|t| t.contains_handle())
                    || err.as_ref().is_some_and(|t| t.contains_handle())
            }
            WitTypeRef::Tuple(elems) => elems.iter().any(|t| t.contains_handle()),
        }
    }

    /// True if this type tree contains a `borrow<R>` handle at any depth.
    pub fn contains_borrow(&self) -> bool {
        match self {
            WitTypeRef::Handle(HandleRef::ResourceBorrow(_)) => true,
            WitTypeRef::Handle(_) | WitTypeRef::Primitive(_) | WitTypeRef::Named(_) => false,
            WitTypeRef::List(inner) | WitTypeRef::Option(inner) => inner.contains_borrow(),
            WitTypeRef::Result { ok, err } => {
                ok.as_ref().is_some_and(|t| t.contains_borrow())
                    || err.as_ref().is_some_and(|t| t.contains_borrow())
            }
            WitTypeRef::Tuple(elems) => elems.iter().any(|t| t.contains_borrow()),
        }
    }

    /// True if this type tree contains an `own<R>` handle at any depth.
    /// The codegen rewrites every such position from `Bucket` to
    /// `WrapperBucket` (intermediate type for the strategy) and back
    /// via `Bucket::new(...)` at the export boundary; this predicate
    /// gates the whole wrap.
    pub fn contains_resource_own(&self) -> bool {
        match self {
            WitTypeRef::Handle(HandleRef::ResourceOwn(_)) => true,
            WitTypeRef::Handle(_) | WitTypeRef::Primitive(_) | WitTypeRef::Named(_) => false,
            WitTypeRef::List(inner) | WitTypeRef::Option(inner) => inner.contains_resource_own(),
            WitTypeRef::Result { ok, err } => {
                ok.as_ref().is_some_and(|t| t.contains_resource_own())
                    || err.as_ref().is_some_and(|t| t.contains_resource_own())
            }
            WitTypeRef::Tuple(elems) => elems.iter().any(|t| t.contains_resource_own()),
        }
    }

    /// True if this type tree contains an `own<R>` handle for a
    /// resource the wrapper OWNS. Resources the wrapper only uses
    /// (factored types via an imported `-types` iface) flow through
    /// untouched, so the wrap doesn't fire for them.
    pub fn contains_owned_resource(&self, resources: &[ResourceInfo]) -> bool {
        let owned = |nr: &NamedRef| {
            resources
                .iter()
                .any(|r| r.is_owned && r.rust_ident == nr.rust_ident && r.iface_path == nr.path)
        };
        match self {
            WitTypeRef::Handle(HandleRef::ResourceOwn(nr)) => owned(nr),
            WitTypeRef::Handle(_) | WitTypeRef::Primitive(_) | WitTypeRef::Named(_) => false,
            WitTypeRef::List(inner) | WitTypeRef::Option(inner) => {
                inner.contains_owned_resource(resources)
            }
            WitTypeRef::Result { ok, err } => {
                ok.as_ref()
                    .is_some_and(|t| t.contains_owned_resource(resources))
                    || err
                        .as_ref()
                        .is_some_and(|t| t.contains_owned_resource(resources))
            }
            WitTypeRef::Tuple(elems) => elems.iter().any(|t| t.contains_owned_resource(resources)),
        }
    }
}

#[derive(Copy, Clone)]
pub enum Prim {
    Bool,
    U8,
    U16,
    U32,
    U64,
    S8,
    S16,
    S32,
    S64,
    F32,
    F64,
    Char,
    String,
}
impl Prim {
    fn to_tokens(self) -> TokenStream {
        match self {
            Prim::Bool => quote!(bool),
            Prim::U8 => quote!(u8),
            Prim::U16 => quote!(u16),
            Prim::U32 => quote!(u32),
            Prim::U64 => quote!(u64),
            Prim::S8 => quote!(i8),
            Prim::S16 => quote!(i16),
            Prim::S32 => quote!(i32),
            Prim::S64 => quote!(i64),
            Prim::F32 => quote!(f32),
            Prim::F64 => quote!(f64),
            Prim::Char => quote!(char),
            Prim::String => quote!(::std::string::String),
        }
    }
}

fn build_named_type(
    resolve: &Resolve,
    ifaces: &[IfaceEntry],
    type_id: TypeId,
    wit_name: &str,
    bindings_path: &BindingsPath,
    bindings: &WrapperBindings,
) -> Result<Option<NamedType>> {
    let td = &resolve.types[type_id];
    let rust_ident_str = wit_name.to_upper_camel_case();
    let rust_ident = syn::Ident::new(&rust_ident_str, Span::call_site());

    let kind = match &td.kind {
        TypeDefKind::Record(r) => {
            let item = bindings.index.get(bindings_path, &rust_ident_str);
            match item {
                Some(BindingsItem::Struct) => {}
                _ => bail!(
                    "wit-bindgen did not emit a struct for WIT record {wit_name:?} at \
                     bindings::{} (got {})",
                    bindings_path.join("::"),
                    describe_item(item),
                ),
            };
            let fields = record_fields_from(
                resolve,
                ifaces,
                r.fields.iter().map(|f| (f.name.as_str(), f.ty)),
            )?;
            NamedKind::Record { fields }
        }
        TypeDefKind::Variant(v) => {
            let item = bindings.index.get(bindings_path, &rust_ident_str);
            match item {
                Some(BindingsItem::Enum) => {}
                _ => bail!(
                    "wit-bindgen did not emit an enum for WIT variant {wit_name:?} at \
                     bindings::{} (got {})",
                    bindings_path.join("::"),
                    describe_item(item),
                ),
            };
            let cases = v
                .cases
                .iter()
                .map(|c| {
                    let pascal = c.name.to_upper_camel_case();
                    Ok(VariantCase {
                        wit_name: c.name.clone(),
                        rust_ident: syn::Ident::new(&pascal, Span::call_site()),
                        payload: c
                            .ty
                            .as_ref()
                            .map(|t| type_to_ref(resolve, ifaces, t))
                            .transpose()?,
                    })
                })
                .collect::<Result<_>>()?;
            NamedKind::Variant { cases }
        }
        TypeDefKind::Enum(e) => {
            let item = bindings.index.get(bindings_path, &rust_ident_str);
            match item {
                Some(BindingsItem::Enum) => {}
                _ => bail!(
                    "wit-bindgen did not emit an enum for WIT enum {wit_name:?} at \
                     bindings::{} (got {})",
                    bindings_path.join("::"),
                    describe_item(item),
                ),
            };
            let cases = e
                .cases
                .iter()
                .map(|c| EnumCase {
                    wit_name: c.name.clone(),
                    rust_ident: syn::Ident::new(&c.name.to_upper_camel_case(), Span::call_site()),
                })
                .collect();
            NamedKind::Enum { cases }
        }
        TypeDefKind::Flags(f) => {
            let item = bindings.index.get(bindings_path, &rust_ident_str);
            match item {
                Some(BindingsItem::BitflagsMacro) => {}
                _ => bail!(
                    "wit-bindgen did not emit a bitflags! macro for WIT flags {wit_name:?} \
                     at bindings::{} (got {})",
                    bindings_path.join("::"),
                    describe_item(item),
                ),
            };
            let members = f
                .flags
                .iter()
                .map(|flag| FlagMember {
                    wit_name: flag.name.clone(),
                    rust_ident: syn::Ident::new(
                        &flag.name.to_shouty_snake_case(),
                        Span::call_site(),
                    ),
                })
                .collect();
            NamedKind::Flags { members }
        }
        // Type aliases are transparent: emit no NamedType for the
        // alias, let `<Alias as WitTyped>::…` resolve through the
        // underlying impl.
        TypeDefKind::Type(_) => return Ok(None),
        // Resources don't emit a `NamedType`/`WitTyped` impl — the
        // codegen tracks them via [`WrapperIR::resources`] instead.
        TypeDefKind::Resource => return Ok(None),
        // `Handle(Own/Borrow)` references can show up here only when
        // a type alias names one explicitly (e.g. `type b = bucket;`).
        // We don't currently emit aliases that name handles.
        TypeDefKind::Handle(_) => {
            bail!("handle-type aliases are not supported (encountered {wit_name:?})")
        }
        TypeDefKind::Future(_) | TypeDefKind::Stream(_) => {
            bail!("future and stream types are not supported (encountered {wit_name:?})")
        }
        TypeDefKind::Map(..) | TypeDefKind::FixedLengthList(..) => bail!(
            "{} types are not supported (encountered {wit_name:?})",
            td.kind.as_str()
        ),
        // Structural composites only appear at field/arg position;
        // they're not given a top-level NamedKind.
        TypeDefKind::List(_)
        | TypeDefKind::Option(_)
        | TypeDefKind::Result(_)
        | TypeDefKind::Tuple(_) => bail!(
            "top-level named {} types are not supported (encountered {wit_name:?})",
            td.kind.as_str()
        ),
        TypeDefKind::Unknown => bail!("unresolved WIT type {wit_name:?}"),
    };

    Ok(Some(NamedType {
        location: TypeLocation::InBindings {
            path: bindings_path.clone(),
        },
        rust_ident,
        kind,
    }))
}

fn describe_item(item: Option<&BindingsItem>) -> &'static str {
    match item {
        None => "nothing",
        Some(BindingsItem::Struct) => "Struct",
        Some(BindingsItem::Enum) => "Enum",
        Some(BindingsItem::BitflagsMacro) => "BitflagsMacro",
    }
}

fn type_to_ref(resolve: &Resolve, ifaces: &[IfaceEntry], ty: &Type) -> Result<WitTypeRef> {
    Ok(match ty {
        Type::Bool => WitTypeRef::Primitive(Prim::Bool),
        Type::U8 => WitTypeRef::Primitive(Prim::U8),
        Type::U16 => WitTypeRef::Primitive(Prim::U16),
        Type::U32 => WitTypeRef::Primitive(Prim::U32),
        Type::U64 => WitTypeRef::Primitive(Prim::U64),
        Type::S8 => WitTypeRef::Primitive(Prim::S8),
        Type::S16 => WitTypeRef::Primitive(Prim::S16),
        Type::S32 => WitTypeRef::Primitive(Prim::S32),
        Type::S64 => WitTypeRef::Primitive(Prim::S64),
        Type::F32 => WitTypeRef::Primitive(Prim::F32),
        Type::F64 => WitTypeRef::Primitive(Prim::F64),
        Type::Char => WitTypeRef::Primitive(Prim::Char),
        Type::String => WitTypeRef::Primitive(Prim::String),
        Type::ErrorContext => WitTypeRef::Handle(HandleRef::ErrorContext),
        Type::Id(id) => {
            let td = &resolve.types[*id];
            // Structural composites are rendered inline; named WIT
            // types resolve to a NamedRef pointing at the
            // wit-bindgen-emitted ident.
            match &td.kind {
                TypeDefKind::List(t) => {
                    WitTypeRef::List(Box::new(type_to_ref(resolve, ifaces, t)?))
                }
                TypeDefKind::Option(t) => {
                    WitTypeRef::Option(Box::new(type_to_ref(resolve, ifaces, t)?))
                }
                TypeDefKind::Result(r) => WitTypeRef::Result {
                    ok: r
                        .ok
                        .as_ref()
                        .map(|t| type_to_ref(resolve, ifaces, t).map(Box::new))
                        .transpose()?,
                    err: r
                        .err
                        .as_ref()
                        .map(|t| type_to_ref(resolve, ifaces, t).map(Box::new))
                        .transpose()?,
                },
                TypeDefKind::Tuple(t) => WitTypeRef::Tuple(
                    t.types
                        .iter()
                        .map(|ty| type_to_ref(resolve, ifaces, ty))
                        .collect::<Result<_>>()?,
                ),
                TypeDefKind::Type(inner) => return type_to_ref(resolve, ifaces, inner),
                TypeDefKind::Record(_)
                | TypeDefKind::Variant(_)
                | TypeDefKind::Enum(_)
                | TypeDefKind::Flags(_) => {
                    let (path, rust_ident) = named_ref_for(resolve, ifaces, *id)?;
                    WitTypeRef::Named(NamedRef { path, rust_ident })
                }
                TypeDefKind::Handle(Handle::Own(rid)) => {
                    let (path, rust_ident) = named_ref_for(resolve, ifaces, *rid)?;
                    WitTypeRef::Handle(HandleRef::ResourceOwn(NamedRef { path, rust_ident }))
                }
                TypeDefKind::Handle(Handle::Borrow(rid)) => {
                    let (path, rust_ident) = named_ref_for(resolve, ifaces, *rid)?;
                    WitTypeRef::Handle(HandleRef::ResourceBorrow(NamedRef { path, rust_ident }))
                }
                // A bare `Resource` TypeDefKind can appear only when
                // a freestanding type position references the resource
                // declaration itself — not the canonical Own/Borrow
                // handle. WIT parsing wraps every use-site in a
                // Handle(_), so this should be unreachable.
                TypeDefKind::Resource => {
                    bail!("bare resource type reference outside Handle wrapper not supported")
                }
                TypeDefKind::Future(_) | TypeDefKind::Stream(_) => {
                    bail!("future/stream in field position not supported")
                }
                TypeDefKind::Map(..) | TypeDefKind::FixedLengthList(..) => {
                    bail!("{} in field position not supported", td.kind.as_str())
                }
                TypeDefKind::Unknown => bail!("unresolved type in field position"),
            }
        }
    })
}

fn named_ref_for(
    resolve: &Resolve,
    ifaces: &[IfaceEntry],
    type_id: TypeId,
) -> Result<(BindingsPath, syn::Ident)> {
    let td = &resolve.types[type_id];
    let name = td
        .name
        .as_ref()
        .ok_or_else(|| anyhow!("named-WIT type at field position has no name"))?;
    let path = match td.owner {
        TypeOwner::Interface(iface_id) => ifaces
            .iter()
            .find(|e| e.id == iface_id)
            .ok_or_else(|| {
                anyhow!(
                    "type {name:?} is owned by an interface not reachable from the world's \
                     imports or exports"
                )
            })?
            .path
            .clone(),
        TypeOwner::World(_) => bail!("world-owned types not supported"),
        TypeOwner::None => Vec::new(),
    };
    Ok((
        path,
        syn::Ident::new(&name.to_upper_camel_case(), Span::call_site()),
    ))
}

fn require_iface(item: &WorldItem) -> Result<InterfaceId> {
    match item {
        WorldItem::Interface { id, .. } => Ok(*id),
        WorldItem::Function(_) => {
            bail!("world-level functions (outside an interface) are not supported")
        }
        WorldItem::Type { .. } => bail!("world-level type aliases are not supported"),
    }
}

/// Derive the [`BindingsPath`] wit-bindgen-rust uses for an interface:
/// kebab-case package / namespace / iface segments lower to
/// snake_case, exports nest under an `exports::` prefix, imports
/// don't.
fn module_path_for_interface(
    resolve: &Resolve,
    interface_id: InterfaceId,
    is_export: bool,
) -> Result<BindingsPath> {
    let iface: &Interface = &resolve.interfaces[interface_id];
    let iface_name = iface
        .name
        .as_ref()
        .ok_or_else(|| anyhow!("interface has no name"))?;
    let pkg_id = iface
        .package
        .ok_or_else(|| anyhow!("interface {iface_name:?} has no package"))?;
    let pkg = &resolve.packages[pkg_id];
    let mut path = Vec::new();
    if is_export {
        path.push("exports".to_string());
    }
    path.push(pkg.name.namespace.to_snake_case());
    path.push(pkg.name.name.to_snake_case());
    path.push(iface_name.to_snake_case());
    Ok(path)
}

fn synth_args_record(
    resolve: &Resolve,
    iface_pascal: &str,
    fn_name: &str,
    func: &Function,
    ifaces: &[IfaceEntry],
) -> Result<NamedType> {
    let rust_ident = args_struct_ident(iface_pascal, fn_name);
    // Resource methods carry an implicit `self: borrow<R>` as their
    // first wit-parser param; wit-bindgen-rust emits it as the trait
    // method's `&self` receiver and drops it from the explicit param
    // list. Mirror that here so the args struct matches the receiver-less
    // signature the codegen will reference.
    let skip_first = matches!(
        func.kind,
        FunctionKind::Method(_) | FunctionKind::AsyncMethod(_)
    );
    let params: Vec<_> = if skip_first {
        func.params.iter().skip(1).collect()
    } else {
        func.params.iter().collect()
    };
    let fields = record_fields_from(
        resolve,
        ifaces,
        params.into_iter().map(|p| (p.name.as_str(), p.ty)),
    )?;
    Ok(NamedType {
        location: TypeLocation::TopLevel,
        rust_ident,
        kind: NamedKind::Record { fields },
    })
}

/// Map an iterator of `(wit_name, ty)` pairs onto a `Vec<RecordField>`.
/// Used by both user-record construction and per-method args
/// synthesis — same shape, two source-of-truth WIT types.
fn record_fields_from<'a, I>(
    resolve: &Resolve,
    ifaces: &[IfaceEntry],
    pairs: I,
) -> Result<Vec<RecordField>>
where
    I: IntoIterator<Item = (&'a str, Type)>,
{
    pairs
        .into_iter()
        .map(|(name, ty)| {
            Ok(RecordField {
                wit_name: name.to_string(),
                rust_ident: mirror_field_ident(name),
                ty: type_to_ref(resolve, ifaces, &ty)?,
            })
        })
        .collect()
}

/// Canonical name for the splicer-synthesized args struct of one
/// Guest method: `<InterfacePascal><MethodPascal>Args`.
///
/// Single source of truth: both the IR builder and the method-emitter
/// derive the same ident from this helper, so the two walks can't
/// silently disagree on what to look up.
pub(super) fn args_struct_ident(interface_pascal: &str, method_name: &str) -> syn::Ident {
    let method_pascal = method_name.to_upper_camel_case();
    syn::Ident::new(
        &format!("{interface_pascal}{method_pascal}Args"),
        Span::call_site(),
    )
}

/// Mirror a WIT field/param name onto wit-bindgen's emitted Rust
/// ident: kebab → snake_case, with a trailing `_` for Rust-keyword
/// collisions.
fn mirror_field_ident(wit_name: &str) -> syn::Ident {
    let snake = wit_name.to_snake_case();
    let final_name = if is_rust_keyword(&snake) {
        format!("{snake}_")
    } else {
        snake
    };
    syn::Ident::new(&final_name, Span::call_site())
}

/// True if `s` would collide with a Rust keyword if used unescaped
/// as an identifier. Defers to `syn`, which maintains the keyword set
/// and tracks Rust grammar evolution.
///
/// Precondition: `s` is already syntactically valid identifier text
/// (letters / digits / underscore, no leading digit). heck's
/// snake-case output satisfies this; other inputs would also return
/// `true` here since `syn::Ident::parse` rejects non-ident strings.
pub(super) fn is_rust_keyword(s: &str) -> bool {
    syn::parse_str::<syn::Ident>(s).is_err()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::typed::bindgen::run_wit_bindgen_rust;
    use crate::adapter::typed::bindings_index::build_bindings_index;

    fn build(wit: &str, world: &str) -> WrapperIR {
        let (resolve, world_id, src) = run_wit_bindgen_rust(wit, Some(world)).unwrap();
        let bindings = build_bindings_index(&src).unwrap();
        build_ir(&resolve, world_id, &bindings).unwrap()
    }

    #[test]
    fn record_field_types_capture_primitives_and_compositions() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                record point { x: u32, y: u32, label: string }
                record blob { bytes: list<u8>, tag: option<string> }
                use-p: func(p: point) -> blob;
            }
            world w { export ops; }
        "#;
        let ir = build(wit, "w");
        let point = ir
            .types
            .iter()
            .find(|t| t.rust_ident == "Point")
            .expect("Point in IR");
        match &point.kind {
            NamedKind::Record { fields } => {
                assert_eq!(fields.len(), 3);
                assert_eq!(fields[0].wit_name, "x");
                assert!(matches!(fields[0].ty, WitTypeRef::Primitive(Prim::U32)));
                assert_eq!(fields[2].wit_name, "label");
                assert!(matches!(fields[2].ty, WitTypeRef::Primitive(Prim::String)));
            }
            _ => panic!("expected Record"),
        }
        let blob = ir
            .types
            .iter()
            .find(|t| t.rust_ident == "Blob")
            .expect("Blob in IR");
        match &blob.kind {
            NamedKind::Record { fields } => {
                assert_eq!(fields[0].wit_name, "bytes");
                assert!(
                    matches!(&fields[0].ty, WitTypeRef::List(inner)
                        if matches!(**inner, WitTypeRef::Primitive(Prim::U8))),
                    "expected list<u8>"
                );
                assert_eq!(fields[1].wit_name, "tag");
                assert!(
                    matches!(&fields[1].ty, WitTypeRef::Option(inner)
                        if matches!(**inner, WitTypeRef::Primitive(Prim::String))),
                    "expected option<string>"
                );
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn flags_kind_recovers_kebab_and_shouting_snake_idents() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                flags perms { read, write, exec-x }
                check: func(p: perms);
            }
            world w { export ops; }
        "#;
        let ir = build(wit, "w");
        let perms = ir
            .types
            .iter()
            .find(|t| t.rust_ident == "Perms")
            .expect("Perms in IR");
        match &perms.kind {
            NamedKind::Flags { members } => {
                assert_eq!(members.len(), 3);
                assert_eq!(members[0].wit_name, "read");
                assert_eq!(members[0].rust_ident, "READ");
                assert_eq!(members[2].wit_name, "exec-x");
                assert_eq!(members[2].rust_ident, "EXEC_X");
            }
            _ => panic!("expected Flags"),
        }
    }

    #[test]
    fn variant_kind_captures_unit_and_payload_cases() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                variant outcome { miss, hit(u32), report(string) }
                go: func() -> outcome;
            }
            world w { export ops; }
        "#;
        let ir = build(wit, "w");
        let outcome = ir
            .types
            .iter()
            .find(|t| t.rust_ident == "Outcome")
            .expect("Outcome in IR");
        match &outcome.kind {
            NamedKind::Variant { cases } => {
                assert_eq!(cases.len(), 3);
                assert!(cases[0].payload.is_none());
                assert!(
                    matches!(cases[1].payload, Some(WitTypeRef::Primitive(Prim::U32))),
                    "hit should carry u32"
                );
                assert_eq!(cases[2].rust_ident, "Report");
            }
            _ => panic!("expected Variant"),
        }
    }

    #[test]
    fn enum_kind_collects_unit_cases() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                enum color { red, green, blue }
                tag: func(c: color);
            }
            world w { export ops; }
        "#;
        let ir = build(wit, "w");
        let color = ir
            .types
            .iter()
            .find(|t| t.rust_ident == "Color")
            .expect("Color in IR");
        match &color.kind {
            NamedKind::Enum { cases } => {
                assert_eq!(cases.len(), 3);
                assert_eq!(cases[0].wit_name, "red");
                assert_eq!(cases[0].rust_ident, "Red");
            }
            _ => panic!("expected Enum"),
        }
    }

    #[test]
    fn args_records_synthesized_per_method() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                add: func(a: u32, b: u32) -> u32;
                noop: func();
            }
            world w { export ops; }
        "#;
        let ir = build(wit, "w");
        assert_eq!(ir.args_records.len(), 2);
        let add_args = ir
            .args_records
            .iter()
            .find(|t| t.rust_ident == "OpsAddArgs")
            .expect("OpsAddArgs in args_records");
        match &add_args.kind {
            NamedKind::Record { fields } => assert_eq!(fields.len(), 2),
            _ => panic!("args should be Records"),
        }
        let noop_args = ir
            .args_records
            .iter()
            .find(|t| t.rust_ident == "OpsNoopArgs")
            .expect("OpsNoopArgs in args_records");
        match &noop_args.kind {
            NamedKind::Record { fields } => assert_eq!(fields.len(), 0),
            _ => panic!("args should be Records"),
        }
    }

    #[test]
    fn keyword_field_idents_get_trailing_underscore() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                record %type {
                    %loop: u32,
                    %match: u32,
                }
                use-t: func(t: %type);
            }
            world w { export ops; }
        "#;
        let ir = build(wit, "w");
        let t = ir.types.iter().find(|t| t.rust_ident == "Type").unwrap();
        match &t.kind {
            NamedKind::Record { fields } => {
                let names: Vec<String> = fields.iter().map(|f| f.rust_ident.to_string()).collect();
                assert!(names.contains(&"loop_".to_string()));
                assert!(names.contains(&"match_".to_string()));
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn resources_lift_into_resources_field_with_handle_returns() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                resource thing { }
                make: func() -> thing;
                make-maybe: func() -> result<thing, string>;
            }
            world w { export ops; }
        "#;
        let ir = build(wit, "w");
        // The resource itself surfaces in `resources`, not `types`,
        // and does not get a `NamedType` entry.
        assert_eq!(ir.resources.len(), 1);
        let r = &ir.resources[0];
        assert_eq!(r.wit_name, "thing");
        assert_eq!(r.rust_ident, "Thing");
        assert!(!ir.types.iter().any(|t| t.rust_ident == "Thing"));

        // `make: -> thing` synthesizes args record + lifts return as
        // a ResourceOwn handle. `make-maybe: -> result<thing, ...>`
        // lifts as `Result { ok: ResourceOwn, ... }`.
        let make_args = ir
            .args_records
            .iter()
            .find(|t| t.rust_ident == "OpsMakeArgs")
            .expect("OpsMakeArgs in args_records");
        match &make_args.kind {
            NamedKind::Record { fields } => assert!(fields.is_empty()),
            _ => panic!("args should be Records"),
        }
        // Quick spot-check that args walked without error for the
        // result-shaped return — the test passes through `build()`
        // which would have panicked at IR construction otherwise.
        let _ = ir
            .args_records
            .iter()
            .find(|t| t.rust_ident == "OpsMakeMaybeArgs")
            .expect("OpsMakeMaybeArgs in args_records");
    }

    #[test]
    fn type_alias_is_transparent() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                type %id = u32;
                fetch: func(i: %id) -> %id;
            }
            world w { export ops; }
        "#;
        let ir = build(wit, "w");
        // No NamedType is emitted for the alias.
        assert!(!ir.types.iter().any(|t| t.rust_ident == "Id"));
        // Args record uses u32 directly (the alias is transparent).
        let args = ir
            .args_records
            .iter()
            .find(|t| t.rust_ident == "OpsFetchArgs")
            .unwrap();
        match &args.kind {
            NamedKind::Record { fields } => {
                assert_eq!(fields.len(), 1);
                assert!(matches!(fields[0].ty, WitTypeRef::Primitive(Prim::U32)));
            }
            _ => panic!(),
        }
    }
}
