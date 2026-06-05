//! Emit the per-method pieces of the wrapper: a synthetic args
//! struct per Guest function (packing positional params into one
//! `WitTyped` value), and the `Guest` trait impl whose method bodies
//! dispatch into the strategy.
//!
//! Args-struct field types are taken from the IR so named user types
//! render as the absolute `bindings::<path>::<Ident>` — copying the
//! syn type from the Guest signature would leave the ident unresolved
//! at the top of `lib.rs`.
//!
//! # What this emits
//!
//! For `add: async func(a: u32, b: u32) -> u32` exported by `ops`
//! under a Transform strategy:
//!
//! ```ignore
//! pub struct OpsAddArgs { pub a: u32, pub b: u32 }
//!
//! impl bindings::exports::pkg::ops::Guest for Wrapper {
//!     async fn add(a: u32, b: u32) -> u32 {
//!         let call = ::splicer_tool_sdk::CallId { /* … */ };
//!         let args = OpsAddArgs { a, b };
//!         let s = strategy();
//!         <_ as ::splicer_tool_sdk::TransformStrategy<OpsAddArgs, u32>>::handle(
//!             s, call, args,
//!             |args: OpsAddArgs| async move {
//!                 bindings::pkg::ops::add(args.a, args.b).await
//!             },
//!         ).await
//!     }
//! }
//! ```
//!
//! Virtualize emission drops the closure and dispatches through
//! `VirtualizeStrategy::handle` instead.

use heck::ToUpperCamelCase;
use proc_macro2::{Span, TokenStream};
use quote::quote;

use super::bindings_index::{bindings_path_tokens, GuestMethod, GuestTrait, GuestTraitKind};
use super::ir::{
    args_struct_ident, ExportFnKind, HandleRef, NamedKind, NamedRef, NamedType, RecordField,
    ResourceInfo, TypeLocation, WitTypeRef, WrapperIR,
};
use super::Behavior;

/// What [`emit_guest`] produces for a single Guest trait.
pub struct EmittedGuest {
    /// One args struct definition per method.
    pub args_structs: Vec<TokenStream>,
    /// A single `impl <ModPath>::Guest for Wrapper` block containing
    /// every method body.
    pub guest_impl: TokenStream,
}

/// Emit the args structs and trait impl for one Guest-flavored trait.
///
/// `interface_qualified_name` is the `"package:ns/iface@ver"` form
/// of the wrapped interface; it goes into each emitted `CallId`.
///
/// Interface-level (`GuestTraitKind::Interface`) emissions add a
/// `type <Resource> = Wrapper<Resource>;` assoc-type line for every
/// resource declared in the same interface, and rewrap
/// resource-returning fns through the per-resource wrapper newtype.
///
/// Resource-level (`GuestTraitKind::Resource`) emissions dispatch
/// through the strategy for each method, capturing `&self` by
/// reference in the closure so the args struct stays free of
/// handle-typed fields.
pub fn emit_guest(
    g: &GuestTrait,
    interface_qualified_name: &str,
    behavior: Behavior,
    ir: &WrapperIR,
) -> EmittedGuest {
    // Last segment is the interface name; an empty path would silently
    // derive the wrong args ident and miss the lookup downstream.
    let interface_pascal = g
        .module_path
        .last()
        .expect("Guest trait module path is empty")
        .to_upper_camel_case();

    // For interface-level Guest, the args prefix is `<IfacePascal>`
    // (matching the per-fn `<IfacePascal><FnPascal>Args` synth). For
    // per-resource GuestBucket, it's `<ResourcePascal>` so two
    // resources with the same method name don't collide.
    let args_prefix = match &g.kind {
        GuestTraitKind::Interface => interface_pascal.clone(),
        GuestTraitKind::Resource(ident) => ident.to_string(),
    };

    let mut args_structs = Vec::with_capacity(g.methods.len());
    let mut method_bodies = Vec::with_capacity(g.methods.len());

    for method in &g.methods {
        let args_ident = args_struct_ident(&args_prefix, &method.ident.to_string());
        let args_record = find_args_record(ir, &args_ident);
        let has_borrow = args_has_borrow(args_record);

        args_structs.push(emit_args_struct(&args_ident, args_record, has_borrow));
        method_bodies.push(emit_method_body(
            method,
            &args_ident,
            args_record,
            has_borrow,
            interface_qualified_name,
            behavior,
            &g.module_path,
            &g.kind,
            ir,
        ));
    }

    let trait_ident = &g.trait_ident;
    let trait_path = build_module_path(&g.module_path);
    let impl_target = match &g.kind {
        GuestTraitKind::Interface => quote!(Wrapper),
        GuestTraitKind::Resource(id) => {
            let wrap = wrapper_ident_for(id);
            quote!(#wrap)
        }
    };

    // The interface-level Guest impl carries `type <Resource> =
    // Wrapper<Resource>;` for every resource declared in this iface;
    // wit-bindgen requires the associated type, and the wrapper
    // newtype is what wires the export-side resource table to our
    // per-method dispatch.
    let assoc_types = match &g.kind {
        GuestTraitKind::Interface => ir
            .resources
            .iter()
            .filter(|r| r.iface_path == g.module_path)
            .map(|r| {
                let res = &r.rust_ident;
                let wrap = wrapper_ident_for(&r.rust_ident);
                quote!(type #res = #wrap;)
            })
            .collect::<Vec<_>>(),
        GuestTraitKind::Resource(_) => Vec::new(),
    };

    let guest_impl = quote! {
        impl #trait_path::#trait_ident for #impl_target {
            #(#assoc_types)*
            #(#method_bodies)*
        }
    };

    EmittedGuest {
        args_structs,
        guest_impl,
    }
}

/// Emit the per-resource pieces of the wrapper crate: the wrapper
/// newtype, and for tier-4 the [`WitTypedWithResources`] impl that
/// decodes a `Cell::ResourceHandle` into the newtype.
///
/// Same struct name regardless of tier (`WrapperBucket`); the inner
/// field shape and any companion impls differ:
///
/// - tier-3 (Transform): inner is the wit-bindgen-generated import-side
///   handle (`bindings::<import>::<R>`); method bodies forward to it.
/// - tier-4 (Virtualize): inner is [`MockedResource`](::splicer_tool_sdk::MockedResource);
///   method bodies dispatch to the strategy. The `WitTypedWithResources`
///   impl is what replay-style strategies invoke internally to decode
///   recorded tier-2 trace data.
pub fn emit_resource_newtypes(ir: &WrapperIR, behavior: Behavior) -> Vec<TokenStream> {
    ir.resources
        .iter()
        .map(|r| emit_one_resource_newtype(r, behavior))
        .collect()
}

/// All per-resource items concatenated into one `TokenStream` (the
/// struct decl, and for tier-4 the `WitTypedWithResources` impl). The
/// caller treats this as one unit per resource.
fn emit_one_resource_newtype(r: &ResourceInfo, behavior: Behavior) -> TokenStream {
    let wrap = wrapper_ident_for(&r.rust_ident);
    match behavior {
        Behavior::Transform => {
            let import_path = import_resource_path_tokens(r);
            quote! {
                pub struct #wrap(pub #import_path);
            }
        }
        Behavior::Virtualize => {
            let wit_name = &r.wit_name;
            quote! {
                pub struct #wrap(pub ::splicer_tool_sdk::MockedResource);
                ::splicer_tool_sdk::impl_wit_typed_with_resources_for_wrapper!(
                    #wrap, #wit_name
                );
            }
        }
    }
}

/// `Wrapper<Pascal>` — e.g. `WrapperBucket`. This is the wrapper-crate-local
/// newtype that wraps the import-side resource handle.
fn wrapper_ident_for(resource_pascal: &syn::Ident) -> syn::Ident {
    syn::Ident::new(&format!("Wrapper{resource_pascal}"), Span::call_site())
}

/// Import-side Rust path for the resource: the wrapper world always
/// imports + exports the same interface in Transform mode, so the
/// import-side `Bucket` lives at the same module path with the
/// leading `exports::` segment dropped.
fn import_resource_path_tokens(r: &ResourceInfo) -> TokenStream {
    let import_segs = strip_exports_prefix(&r.iface_path);
    bindings_path_tokens(&import_segs, Some(&r.rust_ident))
}

fn strip_exports_prefix(segs: &[String]) -> Vec<String> {
    if segs.first().map(String::as_str) == Some("exports") {
        segs[1..].to_vec()
    } else {
        segs.to_vec()
    }
}

fn find_args_record<'a>(ir: &'a WrapperIR, args_ident: &syn::Ident) -> &'a NamedType {
    ir.args_records
        .iter()
        .find(|t| t.rust_ident == *args_ident)
        .unwrap_or_else(|| {
            panic!(
                "IR has no synthesized args record for `{args_ident}`; \
                 the Resolve walk and Guest-trait extraction disagree on methods"
            )
        })
}

fn args_fields(t: &NamedType) -> &[RecordField] {
    match &t.kind {
        NamedKind::Record { fields } => fields.as_slice(),
        _ => unreachable!("args records must be NamedKind::Record"),
    }
}

fn emit_args_struct(
    args_ident: &syn::Ident,
    args_record: &NamedType,
    has_borrow: bool,
) -> TokenStream {
    debug_assert!(matches!(args_record.location, TypeLocation::TopLevel));
    let field_tokens = args_fields(args_record).iter().map(|f| {
        let name = &f.rust_ident;
        let ty = f.ty.to_tokens();
        quote! { pub #name: #ty }
    });
    // wit-bindgen renders `borrow<R>` as `BucketBorrow<'a>`; when any
    // field carries one, the struct hosts the `'a` binding. Fields
    // without borrows widen to the same `'a` harmlessly via Rust
    // lifetime variance.
    let generics = if has_borrow { quote!(<'a>) } else { quote!() };
    // Named-empty `{}` (not `;`) so the zero-arg `WitTyped` impl can
    // construct via `Self {}`.
    quote! {
        pub struct #args_ident #generics {
            #(#field_tokens),*
        }
    }
}

fn args_has_borrow(args_record: &NamedType) -> bool {
    args_fields(args_record).iter().any(|f| f.ty.contains_borrow())
}

#[allow(clippy::too_many_arguments)]
fn emit_method_body(
    method: &GuestMethod,
    args_ident: &syn::Ident,
    args_record: &NamedType,
    has_borrow: bool,
    interface_qualified_name: &str,
    behavior: Behavior,
    guest_module_path: &[String],
    trait_kind: &GuestTraitKind,
    ir: &WrapperIR,
) -> TokenStream {
    let method_ident = &method.ident;
    let method_name = method_ident.to_string();
    // wit-bindgen-emitted trait sigs reference iface-local idents
    // (`Bucket`, `BucketBorrow`, …) that resolve inside the iface
    // module but NOT at the wrapper crate's top level where our
    // impl block lives. Rewrite bare resource idents to their
    // absolute `bindings::<iface_path>::<R>` form first.
    let mut sig_inputs = method.sig.inputs.clone();
    let mut sig_output = method.sig.output.clone();
    {
        let mut visitor = AbsolutizeResources { resources: &ir.resources };
        for arg in sig_inputs.iter_mut() {
            syn::visit_mut::VisitMut::visit_fn_arg_mut(&mut visitor, arg);
        }
        syn::visit_mut::VisitMut::visit_return_type_mut(&mut visitor, &mut sig_output);
    }
    let nominal_return_ty = match &sig_output {
        syn::ReturnType::Default => quote!(()),
        syn::ReturnType::Type(_, ty) => quote!(#ty),
    };
    let fields = args_fields(args_record);
    let is_async = method.sig.asyncness.is_some();

    // Both sides of the pairing come from the same kebab→snake mirror,
    // so positional indexing is sound by construction.
    let positional_params = extract_named_params(&method.sig);
    assert_eq!(
        positional_params.len(),
        fields.len(),
        "Guest method `{}`: syn signature has {} params but IR args record has {} fields",
        method_ident,
        positional_params.len(),
        fields.len()
    );
    let inits = fields.iter().enumerate().map(|(i, f)| {
        let field = &f.rust_ident;
        let value = &positional_params[i].0;
        quote! { #field: #value }
    });
    let args_construct = quote! { #args_ident { #(#inits),* } };

    // Authoritative WIT-side kind for this fn (the IR pinned it from
    // wit-parser's FunctionKind).
    let fn_kind = ir
        .fn_sigs
        .get(&args_ident.to_string())
        .map(|s| s.kind)
        .unwrap_or(ExportFnKind::Freestanding);

    // Where the closure body's call lands: interface-level Guest fns
    // forward to the import-side iface module; per-resource methods
    // forward via the captured `&self` (instance method) or the
    // resource's import-side type (constructor / static).
    let target_call = match (trait_kind, fn_kind) {
        (GuestTraitKind::Interface, _) => {
            build_target_call(method_ident, fields, guest_module_path)
        }
        (GuestTraitKind::Resource(_), ExportFnKind::Method) => {
            build_self_call(method_ident, fields)
        }
        (GuestTraitKind::Resource(resource_pascal), ExportFnKind::Constructor) => {
            let wrap = wrapper_ident_for(resource_pascal);
            match behavior {
                Behavior::Transform => {
                    // tier-3: forward to the import-side constructor
                    // and wrap the resulting handle into our newtype.
                    let import_resource =
                        build_import_resource_path(resource_pascal, guest_module_path);
                    let call = build_static_call(&import_resource, method_ident, fields);
                    // Constructor is always sync per the WIT spec, but
                    // guard the await branch defensively in case the
                    // spec grows async constructors later.
                    if is_async {
                        quote!(#wrap(#call.await))
                    } else {
                        quote!(#wrap(#call))
                    }
                }
                Behavior::Virtualize => {
                    // tier-4 has no import side; the sync constructor
                    // can't await an async strategy. The SDK macro
                    // owns the monotonic-counter + Cow shape.
                    let resource_wit_name = ir
                        .resources
                        .iter()
                        .find(|r| &r.rust_ident == resource_pascal)
                        .map(|r| r.wit_name.as_str())
                        .unwrap_or_else(|| {
                            unreachable!(
                                "IR has no resource entry for `{resource_pascal}`; the \
                                 Resolve walk and per-resource Guest extraction disagree"
                            )
                        });
                    quote!(::splicer_tool_sdk::mint_mock_resource!(#wrap, #resource_wit_name))
                }
            }
        }
        (GuestTraitKind::Resource(resource_pascal), ExportFnKind::Static) => {
            match behavior {
                Behavior::Transform => {
                    // tier-3: dispatch through the import-side type
                    // surface and return whatever the WIT declared.
                    let import_resource =
                        build_import_resource_path(resource_pascal, guest_module_path);
                    build_static_call(&import_resource, method_ident, fields)
                }
                Behavior::Virtualize => {
                    // tier-4 has no import side. A static method's
                    // body would need to dispatch through the strategy
                    // (and an async one would need the wrapper method
                    // to be async too, which static-method async-ness
                    // determines).
                    let msg = format!(
                        "tier-4 static methods on resources are not yet supported \
                         (encountered `{}::{}`)",
                        resource_pascal, method_ident,
                    );
                    quote!(::core::compile_error!(#msg))
                }
            }
        }
        (GuestTraitKind::Resource(_), ExportFnKind::Freestanding) => {
            unreachable!("freestanding fn appeared in a per-resource Guest trait")
        }
    };
    // Constructor's wrap-with-await is already inlined above; other
    // calls get the standard sync/async suffix here.
    let constructor_already_handled = matches!(
        (trait_kind, fn_kind),
        (GuestTraitKind::Resource(_), ExportFnKind::Constructor)
    );
    let target_call = if constructor_already_handled {
        target_call
    } else if is_async {
        quote! { #target_call.await }
    } else {
        target_call
    };

    // Resource-returning interface-level fns route through an
    // intermediate type (the wrapper newtype) inside the strategy,
    // then re-wrap to the export-side resource at the boundary.
    let resource_wrap = match trait_kind {
        GuestTraitKind::Interface => {
            let ret = ir
                .fn_sigs
                .get(&args_ident.to_string())
                .and_then(|s| s.return_ty.as_ref());
            ret.and_then(detect_resource_wrap)
        }
        GuestTraitKind::Resource(_) => None,
    };
    let (strategy_r_ty, closure_body, final_wrap) = match &resource_wrap {
        Some(rw) => (
            rw.intermediate_ty.clone(),
            (rw.wrap_to)(&target_call),
            Some((rw.wrap_from)(quote!(intermediate))),
        ),
        None => (nominal_return_ty.clone(), target_call.clone(), None),
    };

    // Args structs with a `borrow<R>` field carry a `<'a>`
    // parameter; instantiate at use sites with the `<'_>` placeholder
    // so the closure / dispatch infers the live lifetime.
    let args_ty: TokenStream = if has_borrow {
        quote!(#args_ident<'_>)
    } else {
        quote!(#args_ident)
    };

    let dispatch = match behavior {
        Behavior::Transform => {
            // Annotate the closure parameter — qualified
            // `<_ as Trait<…>>::handle` dispatch doesn't propagate
            // into closure inference (E0282).
            quote! {
                <_ as ::splicer_tool_sdk::TransformStrategy<#args_ty, #strategy_r_ty>>::handle(
                    s,
                    call,
                    args,
                    |args: #args_ty| async move { #closure_body },
                )
            }
        }
        Behavior::Virtualize => {
            quote! {
                <_ as ::splicer_tool_sdk::VirtualizeStrategy<#args_ty, #strategy_r_ty>>::handle(
                    s,
                    call,
                    args,
                )
            }
        }
    };

    // Sync wrapper methods can't `.await` the async strategy. For
    // those, skip strategy dispatch and direct-delegate the call.
    // This is the documented L2 limitation around tier-3 + sync
    // WIT — primarily exercised by resource constructors (always
    // sync per the WIT spec). When end-user-visible interposition
    // on sync methods is needed, this is the surface that grows.
    let body = if !is_async {
        quote! {
            let args = #args_construct;
            #target_call
        }
    } else {
        match final_wrap {
            Some(wrap) => quote! {
                let call = ::splicer_tool_sdk::CallId {
                    interface_name: #interface_qualified_name.into(),
                    function_name: #method_name.into(),
                    id: 0,
                };
                let args = #args_construct;
                let s = strategy();
                let intermediate = #dispatch.await;
                #wrap
            },
            None => quote! {
                let call = ::splicer_tool_sdk::CallId {
                    interface_name: #interface_qualified_name.into(),
                    function_name: #method_name.into(),
                    id: 0,
                };
                let args = #args_construct;
                let s = strategy();
                #dispatch.await
            },
        }
    };

    if is_async {
        quote! {
            async fn #method_ident(#sig_inputs) #sig_output {
                #body
            }
        }
    } else {
        quote! {
            fn #method_ident(#sig_inputs) #sig_output {
                #body
            }
        }
    }
}

/// Rewrite bare iface-local resource idents (e.g. `Bucket`) inside a
/// syn type into their absolute `bindings::<iface_path>::<R>` form,
/// recursively. wit-bindgen's trait sigs use the bare form because
/// they're emitted *inside* the iface module; our impl block lives
/// at the wrapper crate's top level and needs the absolute path.
struct AbsolutizeResources<'a> {
    resources: &'a [ResourceInfo],
}

impl syn::visit_mut::VisitMut for AbsolutizeResources<'_> {
    fn visit_type_path_mut(&mut self, tp: &mut syn::TypePath) {
        if tp.qself.is_none() && tp.path.segments.len() == 1 {
            let seg_ident = tp.path.segments[0].ident.clone();
            // wit-bindgen emits two distinct types for a WIT resource:
            // the bare ident (`Bucket`) for `own<R>` positions and a
            // `<R>Borrow` companion (`BucketBorrow<'_>`) for `borrow<R>`.
            // Both are iface-module-local in wit-bindgen output, so the
            // wrapper crate top-level reference needs the absolute
            // `bindings::<iface>::…` prefix in either case.
            //
            // Try exact match first: a resource literally named
            // `FooBorrow` in own position must NOT be rewritten as
            // `Foo` via the suffix-strip path.
            let matched = self
                .resources
                .iter()
                .find(|r| r.rust_ident == seg_ident)
                .or_else(|| {
                    seg_ident
                        .to_string()
                        .strip_suffix("Borrow")
                        .and_then(|stripped| {
                            self.resources.iter().find(|r| r.rust_ident == stripped)
                        })
                });
            if let Some(r) = matched {
                let abs = absolute_resource_path(r, &seg_ident);
                // Preserve the original segment's path args (e.g. the
                // `<'_>` carried by `BucketBorrow<'_>`).
                let trailing_args = tp.path.segments[0].arguments.clone();
                tp.path = abs;
                if let Some(last) = tp.path.segments.last_mut() {
                    last.arguments = trailing_args;
                }
            }
        }
        syn::visit_mut::visit_type_path_mut(self, tp);
    }
}

/// Build an absolute `bindings::<iface>::<seg_ident>` path. `seg_ident`
/// is what wit-bindgen used for the terminal segment (`Bucket` for
/// own, `BucketBorrow` for borrow); the IfacePath comes from the
/// resource's declaring interface.
fn absolute_resource_path(r: &ResourceInfo, seg_ident: &syn::Ident) -> syn::Path {
    use syn::punctuated::Punctuated;
    let mut segments: Punctuated<syn::PathSegment, syn::Token![::]> = Punctuated::new();
    segments.push(syn::PathSegment {
        ident: syn::Ident::new("bindings", Span::call_site()),
        arguments: syn::PathArguments::None,
    });
    for s in &r.iface_path {
        segments.push(syn::PathSegment {
            ident: syn::Ident::new(s, Span::call_site()),
            arguments: syn::PathArguments::None,
        });
    }
    segments.push(syn::PathSegment {
        ident: seg_ident.clone(),
        arguments: syn::PathArguments::None,
    });
    syn::Path {
        leading_colon: None,
        segments,
    }
}

/// Build the closure body that forwards a per-resource method call to
/// `self.0.<method>(args.x)` — the import-side handle held by the
/// wrapper newtype is what the strategy ultimately reaches.
fn build_self_call(method_ident: &syn::Ident, fields: &[RecordField]) -> TokenStream {
    let arg_exprs = fields.iter().map(|f| {
        let name = &f.rust_ident;
        quote! { args.#name }
    });
    quote! { self.0.#method_ident(#(#arg_exprs),*) }
}

/// Build the closure body for a constructor / static method:
/// `<import>::<Resource>::<method>(args.x)`.
fn build_static_call(
    import_resource: &TokenStream,
    method_ident: &syn::Ident,
    fields: &[RecordField],
) -> TokenStream {
    let arg_exprs = fields.iter().map(|f| {
        let name = &f.rust_ident;
        quote! { args.#name }
    });
    quote! { #import_resource::#method_ident(#(#arg_exprs),*) }
}

/// Resolve the import-side resource type path from the GuestBucket
/// trait's module path: `bindings::exports::test::resz::store` →
/// `bindings::test::resz::store::Bucket`.
fn build_import_resource_path(
    resource_pascal: &syn::Ident,
    guest_module_path: &[String],
) -> TokenStream {
    assert_eq!(
        guest_module_path.first().map(String::as_str),
        Some("exports"),
        "GuestBucket trait module path must start with `exports`; got {guest_module_path:?}",
    );
    let import_segs: Vec<String> = guest_module_path[1..].to_vec();
    bindings_path_tokens(&import_segs, Some(resource_pascal))
}

/// Recognizes resource-bearing return shapes that the interface-level
/// Guest emitter must rewrap. Covers:
///
/// - bare `own<R>` returns (wrap → `WrapperR`)
/// - `result<own<R>, E>` (wrap → `Result<WrapperR, E>`)
///
/// Other compound shapes (`option<R>`, `tuple<…, R, …>`, `list<R>`)
/// would need per-shape closures; deferred until a test fixture
/// exercises them.
fn detect_resource_wrap(ret: &WitTypeRef) -> Option<ResourceWrap> {
    match ret {
        WitTypeRef::Handle(HandleRef::ResourceOwn(nr)) => Some(make_own_wrap(nr)),
        WitTypeRef::Result {
            ok: Some(inner),
            err,
        } => match inner.as_ref() {
            WitTypeRef::Handle(HandleRef::ResourceOwn(nr)) => {
                Some(make_result_own_wrap(nr, err.as_deref()))
            }
            _ => None,
        },
        _ => None,
    }
}

fn make_own_wrap(nr: &NamedRef) -> ResourceWrap {
    let export_path = bindings_path_tokens(&nr.path, Some(&nr.rust_ident));
    let wrap_ident = wrapper_ident_for(&nr.rust_ident);
    let intermediate_ty = quote!(#wrap_ident);
    let wrap_ident_for_to = wrap_ident.clone();
    let export_path_for_from = export_path.clone();
    ResourceWrap {
        intermediate_ty,
        wrap_to: Box::new(move |inner: &TokenStream| {
            let wrap = &wrap_ident_for_to;
            quote!(#wrap(#inner))
        }),
        wrap_from: Box::new(move |intermediate: TokenStream| {
            let p = &export_path_for_from;
            quote!(#p::new(#intermediate))
        }),
    }
}

fn make_result_own_wrap(nr: &NamedRef, err: Option<&WitTypeRef>) -> ResourceWrap {
    let export_path = bindings_path_tokens(&nr.path, Some(&nr.rust_ident));
    let wrap_ident = wrapper_ident_for(&nr.rust_ident);
    let err_ty = match err {
        Some(t) => t.to_tokens(),
        None => quote!(()),
    };
    let intermediate_ty = quote!(::core::result::Result<#wrap_ident, #err_ty>);
    let wrap_ident_for_to = wrap_ident.clone();
    let export_path_for_from = export_path.clone();
    ResourceWrap {
        intermediate_ty,
        wrap_to: Box::new(move |inner: &TokenStream| {
            let wrap = &wrap_ident_for_to;
            // `.map(WrapperBucket)` reuses the newtype's tuple-struct
            // call-form as a function pointer over the Ok arm.
            quote!((#inner).map(#wrap))
        }),
        wrap_from: Box::new(move |intermediate: TokenStream| {
            let p = &export_path_for_from;
            quote!((#intermediate).map(#p::new))
        }),
    }
}

struct ResourceWrap {
    intermediate_ty: TokenStream,
    /// Converts the import-side call's result into the strategy R
    /// intermediate (e.g. `WrapperBucket(<call>)` or
    /// `<call>.map(WrapperBucket)`).
    wrap_to: Box<dyn Fn(&TokenStream) -> TokenStream>,
    /// Converts the strategy-returned intermediate back into the
    /// nominal Guest return type (e.g.
    /// `bindings::exports::iface::Bucket::new(intermediate)`).
    wrap_from: Box<dyn Fn(TokenStream) -> TokenStream>,
}

fn extract_named_params(sig: &syn::Signature) -> Vec<(syn::Ident, syn::Type)> {
    sig.inputs
        .iter()
        .filter_map(|arg| match arg {
            syn::FnArg::Typed(pat_typed) => match &*pat_typed.pat {
                syn::Pat::Ident(pat_ident) => {
                    Some((pat_ident.ident.clone(), (*pat_typed.ty).clone()))
                }
                other => panic!(
                    "Guest method `{}` has a non-Ident parameter pattern `{}`; \
                     the wrapper codegen expects `Ident: Type`",
                    sig.ident,
                    quote!(#other),
                ),
            },
            syn::FnArg::Receiver(_) => None,
        })
        .collect()
}

fn return_type(sig: &syn::Signature) -> TokenStream {
    match &sig.output {
        syn::ReturnType::Default => quote!(()),
        syn::ReturnType::Type(_, ty) => quote!(#ty),
    }
}

/// Build the closure body that calls the wrapped target with args
/// unpacked, against the import-side path
/// (`bindings::<pkg>::<iface>::<method>`, no `exports::` prefix).
fn build_target_call(
    method_ident: &syn::Ident,
    fields: &[RecordField],
    guest_module_path: &[String],
) -> TokenStream {
    // The Guest trait lives at `exports::<pkg>::<iface>`; the import
    // side drops that prefix.
    assert_eq!(
        guest_module_path.first().map(String::as_str),
        Some("exports"),
        "Guest trait module path must start with `exports`; got {guest_module_path:?}",
    );
    let import_path = bindings_path_tokens(&guest_module_path[1..], None);
    let arg_exprs = fields.iter().map(|f| {
        let name = &f.rust_ident;
        quote! { args.#name }
    });
    quote! { #import_path::#method_ident(#(#arg_exprs),*) }
}

fn build_module_path(segments: &[String]) -> TokenStream {
    bindings_path_tokens(segments, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::typed::bindgen::run_wit_bindgen_rust;
    use crate::adapter::typed::bindings_index::build_bindings_index;
    use crate::adapter::typed::ir::build_ir;

    const INTERFACE_QN: &str = "test:pkg/ops@0.1.0";

    // `async func` deliberately: tier-3 strategy dispatch needs to
    // `.await`, which sync wrapper methods can't host (the L2
    // sync-method limitation). Tests that assert on the strategy
    // path must therefore exercise an async surface.
    const TINY_WIT: &str = r#"
        package test:pkg@0.1.0;
        interface ops {
            add: async func(a: u32, b: u32) -> u32;
        }
        world w { export ops; }
    "#;

    fn normalize(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn emit_for_wit(wit: &str, behavior: Behavior) -> EmittedGuest {
        let (resolve, world_id, src) = run_wit_bindgen_rust(wit, Some("w")).unwrap();
        let bindings = build_bindings_index(&src).unwrap();
        let ir = build_ir(&resolve, world_id, &bindings).unwrap();
        let g = &bindings.guest_traits[0];
        emit_guest(g, INTERFACE_QN, behavior, &ir)
    }

    fn args_structs_str(emitted: &EmittedGuest) -> String {
        normalize(
            &emitted
                .args_structs
                .iter()
                .map(|t| t.to_string())
                .collect::<String>(),
        )
    }

    fn full_emission_str(emitted: &EmittedGuest) -> String {
        let mut out = String::new();
        for t in &emitted.args_structs {
            out.push_str(&t.to_string());
        }
        out.push_str(&emitted.guest_impl.to_string());
        normalize(&out)
    }

    #[test]
    fn emits_args_struct_with_fields() {
        let out = full_emission_str(&emit_for_wit(TINY_WIT, Behavior::Transform));
        assert!(
            out.contains("pub struct OpsAddArgs"),
            "expected OpsAddArgs struct: {out}"
        );
        assert!(
            out.contains("pub a : u32"),
            "expected field `a: u32`: {out}"
        );
        assert!(
            out.contains("pub b : u32"),
            "expected field `b: u32`: {out}"
        );
    }

    #[test]
    fn forward_emission_uses_forward_strategy_and_downstream_closure() {
        let out = full_emission_str(&emit_for_wit(TINY_WIT, Behavior::Transform));
        assert!(
            out.contains("TransformStrategy"),
            "expected TransformStrategy dispatch: {out}"
        );
        assert!(
            out.contains("bindings :: test :: pkg :: ops :: add"),
            "expected import-side call to bindings::test::pkg::ops::add: {out}"
        );
        assert!(
            out.contains("args . a"),
            "expected args.a in closure: {out}"
        );
        assert!(
            out.contains("args . b"),
            "expected args.b in closure: {out}"
        );
    }

    #[test]
    fn virtualize_emission_uses_virtualize_strategy_without_closure() {
        let out = full_emission_str(&emit_for_wit(TINY_WIT, Behavior::Virtualize));
        assert!(
            out.contains("VirtualizeStrategy"),
            "expected VirtualizeStrategy dispatch: {out}"
        );
        assert!(
            !out.contains("async move"),
            "virtualize emission should not contain a downstream closure: {out}"
        );
        assert!(
            !out.contains("bindings :: test :: pkg :: ops :: add"),
            "virtualize emission should not call the target import: {out}"
        );
    }

    #[test]
    fn call_id_carries_interface_and_function_names() {
        let out = full_emission_str(&emit_for_wit(TINY_WIT, Behavior::Transform));
        assert!(
            out.contains("\"test:pkg/ops@0.1.0\""),
            "expected qualified interface in CallId: {out}"
        );
        assert!(
            out.contains("\"add\""),
            "expected function name in CallId: {out}"
        );
    }

    #[test]
    fn handles_zero_arg_function() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                noop: func();
            }
            world w { export ops; }
        "#;
        let out = args_structs_str(&emit_for_wit(wit, Behavior::Transform));
        assert!(
            out.contains("pub struct OpsNoopArgs { }"),
            "expected named-empty struct for zero-arg method: {out}"
        );
    }

    #[test]
    fn args_struct_with_named_user_type_uses_absolute_bindings_path() {
        // Named user types reach the args-struct decl from a scope
        // where the local Rust ident isn't visible, so field types
        // must resolve via the absolute `bindings::…::Ident` path.
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                record point { x: u32, y: u32 }
                place: func(p: point);
            }
            world w { export ops; }
        "#;
        let decls = args_structs_str(&emit_for_wit(wit, Behavior::Transform));
        assert!(
            decls.contains("bindings :: exports :: test :: pkg :: ops :: Point")
                || decls.contains("bindings::exports::test::pkg::ops::Point"),
            "args struct field should use absolute bindings:: path: {decls}"
        );
    }
}
