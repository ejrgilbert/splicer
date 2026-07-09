//! Emit the per-method pieces of the wrapper: a synthetic args
//! struct per Guest function (packing positional params into one
//! `WitTyped` value), and the `Guest` trait impl whose method bodies
//! dispatch into the strategy.
//!
//! Args-struct field types come from the IR so named user types
//! resolve as absolute `bindings::<path>::<Ident>` paths; copying
//! syn types from the Guest signature would leave them unresolved
//! at the wrapper crate's top level.
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

use super::bindings_index::{
    bindings_path_tokens, strip_exports_prefix, GuestMethod, GuestTrait, GuestTraitKind,
};
use super::ir::{
    args_struct_ident, wrapper_ident_for, BridgeResourceInfo, ExportFnKind, HandleRef, NamedKind,
    NamedRef, NamedType, RecordField, ResourceInfo, TypeLocation, WitTypeRef, WrapperIR,
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
/// Interface-level emissions add `type <Resource> = Wrapper<Resource>`
/// assoc-types for every resource in the same interface, and rewrap
/// resource-returning fns through the per-resource wrapper newtype.
///
/// Resource-level emissions dispatch through the strategy per method,
/// capturing `&self` in the closure so the args struct stays free of
/// handle-typed fields.
pub fn emit_guest(
    g: &GuestTrait,
    interface_qualified_name: &str,
    behavior: Behavior,
    ir: &WrapperIR,
) -> EmittedGuest {
    // Interface name is the deepest module-path segment.
    let interface_pascal = g
        .module_path
        .last()
        .expect("Guest trait module path is empty")
        .to_upper_camel_case();

    // Interface-level Guest uses `<IfacePascal>` (matching the
    // `<IfacePascal><FnPascal>Args` synth); per-resource GuestBucket
    // uses `<ResourcePascal>` so same-named methods don't collide.
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

    // wit-bindgen requires a `type <Resource> = Wrapper<Resource>`
    // assoc-type per resource on the interface-level Guest impl; the
    // wrapper newtype wires the export-side resource table to dispatch.
    let assoc_types = match &g.kind {
        GuestTraitKind::Interface => ir
            .resources
            .iter()
            .filter(|r| r.is_owned && r.iface_path == g.module_path)
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
/// field shape and companion impls differ:
///
/// - tier-3 (Transform): inner is the wit-bindgen-generated import-side
///   handle (`bindings::<import>::<R>`); method bodies forward to it.
/// - tier-4 (Virtualize): inner is [`MockedResource`](::splicer_tool_sdk::MockedResource);
///   method bodies dispatch through the strategy.
pub fn emit_resource_newtypes(ir: &WrapperIR, behavior: Behavior) -> Vec<TokenStream> {
    ir.resources
        .iter()
        .filter(|r| r.is_owned)
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

/// Import-side Rust path for the resource. In normal Transform mode the
/// wrapper world imports and exports the same interface, so the
/// import-side type sits at the same module path with the leading
/// `exports::` segment dropped. In T' mode the owned resource's
/// `inner_type_path` points directly at the raw (import-side) type.
fn import_resource_path_tokens(r: &ResourceInfo) -> TokenStream {
    let import_segs = r
        .inner_type_path
        .as_ref()
        .cloned()
        .unwrap_or_else(|| strip_exports_prefix(&r.iface_path));
    bindings_path_tokens(&import_segs, Some(&r.rust_ident))
}

/// Import-side path for constructors and statics. In T' mode the GuestBucket
/// trait's module path points to the T' interface (splicer:wrapper), not the
/// raw resource; use the IR's `inner_type_path` override when present.
fn import_resource_path_for_ctor(
    resource_pascal: &syn::Ident,
    guest_module_path: &[String],
    ir: &WrapperIR,
) -> TokenStream {
    if let Some(raw_path) = ir
        .resources
        .iter()
        .find(|r| r.is_owned && &r.rust_ident == resource_pascal)
        .and_then(|r| r.inner_type_path.as_ref())
    {
        bindings_path_tokens(raw_path, Some(resource_pascal))
    } else {
        build_import_resource_path(resource_pascal, guest_module_path)
    }
}

/// Emit the fixed `impl <bridge>::Guest for Wrapper` block for T' mode.
/// Uses hard-wired `wrap`/`unwrap` bodies (not strategy dispatch).
pub fn emit_bridge_guest_impl(bridge_resources: &[BridgeResourceInfo]) -> Option<TokenStream> {
    if bridge_resources.is_empty() {
        return None;
    }
    let bridge_path = bindings_path_tokens(&bridge_resources[0].bridge_module_path, None);
    let methods: Vec<TokenStream> = bridge_resources
        .iter()
        .map(|br| {
            let raw = bindings_path_tokens(&br.raw_resource_path, Some(&br.raw_resource_ident));
            let exp =
                bindings_path_tokens(&br.export_resource_path, Some(&br.export_resource_ident));
            let wrap_wrapper = &br.wrapper_ident;
            // WIT names are `wrap-{name}` / `unwrap-{name}` (kebab); Rust uses underscores.
            let snake = br.wit_name.replace('-', "_");
            let wrap_fn =
                syn::Ident::new(&format!("wrap_{snake}"), proc_macro2::Span::call_site());
            let unwrap_fn =
                syn::Ident::new(&format!("unwrap_{snake}"), proc_macro2::Span::call_site());
            quote! {
                fn #wrap_fn(inner: #raw) -> #exp {
                    #exp::new(#wrap_wrapper(inner))
                }
                fn #unwrap_fn(w: #exp) -> #raw {
                    w.into_inner::<#wrap_wrapper>().0
                }
            }
        })
        .collect();
    Some(quote! {
        impl #bridge_path::Guest for Wrapper {
            #(#methods)*
        }
    })
}

/// Emit a delegation `impl original_iface::Guest for Wrapper` for T' mode.
///
/// When the T' wrapper also exports the original target interface (for WAC
/// routing), the generated impl receives raw resource handles from the host,
/// wraps them into T' types via the bridge, calls the T' Guest impl, and
/// unwraps the T' result back to raw types. There is no strategy dispatch;
/// the strategy runs inside the T' Guest impl that this delegates to.
pub fn emit_delegation_guest_impl(
    original_g: &GuestTrait,
    t_prime_g: &GuestTrait,
    bridge_resources: &[BridgeResourceInfo],
    ir: &WrapperIR,
) -> Option<TokenStream> {
    if bridge_resources.is_empty() || !matches!(original_g.kind, GuestTraitKind::Interface) {
        return None;
    }
    let bridge_path = bindings_path_tokens(&bridge_resources[0].bridge_module_path, None);
    let t_prime_path = build_module_path(&t_prime_g.module_path);
    let t_prime_trait_ident = &t_prime_g.trait_ident;
    let original_path = build_module_path(&original_g.module_path);
    let original_trait_ident = &original_g.trait_ident;

    let t_prime_iface_pascal = t_prime_g
        .module_path
        .last()
        .expect("T' Guest trait module path is empty")
        .to_upper_camel_case();

    let method_impls: Vec<TokenStream> = original_g
        .methods
        .iter()
        .zip(t_prime_g.methods.iter())
        .map(|(orig_method, t_prime_method)| {
            let method_ident = &orig_method.ident;
            let is_async = orig_method.sig.asyncness.is_some();

            let mut sig_inputs = orig_method.sig.inputs.clone();
            let mut sig_output = orig_method.sig.output.clone();
            {
                // Use import-side (non-owned) resources: the original-interface
                // delegation signature uses raw types, not T' types.
                let mut visitor = AbsolutizeResources {
                    resources: &ir.resources,
                    types: &ir.types,
                    prefer_import_side: true,
                };
                for arg in sig_inputs.iter_mut() {
                    syn::visit_mut::VisitMut::visit_fn_arg_mut(&mut visitor, arg);
                }
                syn::visit_mut::VisitMut::visit_return_type_mut(&mut visitor, &mut sig_output);
            }

            let params = extract_named_params(&orig_method.sig);

            // Get the T' args record to learn which params are resource handles.
            let args_ident =
                args_struct_ident(&t_prime_iface_pascal, &t_prime_method.ident.to_string());
            let t_prime_fields: &[RecordField] = ir
                .args_records
                .iter()
                .find(|t| t.rust_ident == args_ident)
                .map(|t| match &t.kind {
                    NamedKind::Record { fields } => fields.as_slice(),
                    _ => &[],
                })
                .unwrap_or(&[]);

            // Build wrapped call args: resource params go through bridge wrap.
            let call_args: Vec<TokenStream> = params
                .iter()
                .enumerate()
                .map(|(i, (param_ident, _param_ty))| {
                    if let Some(field) = t_prime_fields.get(i) {
                        if let Some(wrap_expr) =
                            bridge_wrap_expr(field, bridge_resources, &bridge_path)
                        {
                            return quote!(#wrap_expr(#param_ident));
                        }
                    }
                    quote!(#param_ident)
                })
                .collect();

            let t_prime_call = if is_async {
                quote!(
                    <Wrapper as #t_prime_path::#t_prime_trait_ident>::#method_ident(
                        #(#call_args),*
                    ).await
                )
            } else {
                quote!(
                    <Wrapper as #t_prime_path::#t_prime_trait_ident>::#method_ident(
                        #(#call_args),*
                    )
                )
            };

            // Unwrap any resource handles in the return type.
            let fn_sig_key = args_ident.to_string();
            let return_ty = ir.fn_sigs.get(&fn_sig_key).and_then(|s| s.return_ty.as_ref());
            let body = delegation_return_expr(t_prime_call, return_ty, bridge_resources, &bridge_path);

            if is_async {
                quote! { async fn #method_ident(#sig_inputs) #sig_output { #body } }
            } else {
                quote! { fn #method_ident(#sig_inputs) #sig_output { #body } }
            }
        })
        .collect();

    Some(quote! {
        impl #original_path::#original_trait_ident for Wrapper {
            #(#method_impls)*
        }
    })
}

/// If `field` holds a T' resource own-handle that has a bridge wrap function,
/// return a turbofish call expression fragment `<Wrapper as bridge::Guest>::wrap_xxx`.
/// The caller appends the raw param: `bridge_wrap_expr(field, ...)(raw_param)`.
fn bridge_wrap_expr(
    field: &RecordField,
    bridge_resources: &[BridgeResourceInfo],
    bridge_path: &TokenStream,
) -> Option<TokenStream> {
    let (path, ident) = match &field.ty {
        WitTypeRef::Handle(HandleRef::ResourceOwn(nr)) => (&nr.path, &nr.rust_ident),
        _ => return None,
    };
    let br = bridge_resources
        .iter()
        .find(|br| br.export_resource_path == *path && br.export_resource_ident == *ident)?;
    let snake = br.wit_name.replace('-', "_");
    let wrap_fn = syn::Ident::new(&format!("wrap_{snake}"), Span::call_site());
    Some(quote!(<Wrapper as #bridge_path::Guest>::#wrap_fn))
}

/// Build the return expression for a delegation method, unwrapping any T'
/// resource handles in the result back to raw types via the bridge.
fn delegation_return_expr(
    t_prime_call: TokenStream,
    return_ty: Option<&WitTypeRef>,
    bridge_resources: &[BridgeResourceInfo],
    bridge_path: &TokenStream,
) -> TokenStream {
    match return_ty {
        None => t_prime_call,
        Some(WitTypeRef::Handle(HandleRef::ResourceOwn(nr))) => {
            if let Some(br) = find_bridge_for_export_resource(nr, bridge_resources) {
                let snake = br.wit_name.replace('-', "_");
                let unwrap_fn = syn::Ident::new(&format!("unwrap_{snake}"), Span::call_site());
                let r = syn::Ident::new("_r", Span::call_site());
                quote! {
                    let #r = #t_prime_call;
                    <Wrapper as #bridge_path::Guest>::#unwrap_fn(#r)
                }
            } else {
                t_prime_call
            }
        }
        Some(WitTypeRef::Result { ok, .. }) => {
            if let Some(inner) = ok {
                if let WitTypeRef::Handle(HandleRef::ResourceOwn(nr)) = inner.as_ref() {
                    if let Some(br) = find_bridge_for_export_resource(nr, bridge_resources) {
                        let snake = br.wit_name.replace('-', "_");
                        let unwrap_fn =
                            syn::Ident::new(&format!("unwrap_{snake}"), Span::call_site());
                        let r = syn::Ident::new("_r", Span::call_site());
                        return quote! {
                            match #t_prime_call {
                                Ok(#r) => Ok(<Wrapper as #bridge_path::Guest>::#unwrap_fn(#r)),
                                Err(e) => Err(e),
                            }
                        };
                    }
                }
            }
            t_prime_call
        }
        _ => t_prime_call,
    }
}

fn find_bridge_for_export_resource<'a>(
    nr: &NamedRef,
    bridge_resources: &'a [BridgeResourceInfo],
) -> Option<&'a BridgeResourceInfo> {
    bridge_resources
        .iter()
        .find(|br| br.export_resource_path == nr.path && br.export_resource_ident == nr.rust_ident)
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
    args_fields(args_record)
        .iter()
        .any(|f| f.ty.contains_borrow())
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
    // wit-bindgen trait sigs use iface-local idents (`Bucket`,
    // `BucketBorrow`, ...) that resolve inside the iface module but
    // not at the wrapper crate's top level. Rewrite them to absolute
    // `bindings::<iface_path>::<R>` paths first.
    let mut sig_inputs = method.sig.inputs.clone();
    let mut sig_output = method.sig.output.clone();
    {
        let mut visitor = AbsolutizeResources {
            resources: &ir.resources,
            types: &ir.types,
            prefer_import_side: false,
        };
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
    // forward via `&self` (instance) or the resource's import-side
    // type (constructor / static).
    let target_call = match (trait_kind, fn_kind) {
        (GuestTraitKind::Interface, _) => match behavior {
            Behavior::Transform => build_target_call(is_async, method_ident, fields, ir),
            // Virtualize doesn't forward
            Behavior::Virtualize => quote!(unreachable!()),
        },
        (GuestTraitKind::Resource(_), ExportFnKind::Method) => {
            build_self_call(is_async, method_ident, fields)
        }
        (GuestTraitKind::Resource(resource_pascal), ExportFnKind::Constructor) => {
            let wrap = wrapper_ident_for(resource_pascal);
            match behavior {
                Behavior::Transform => {
                    // tier-3: forward to the import-side constructor
                    // and wrap the resulting handle into our newtype.
                    let import_resource =
                        import_resource_path_for_ctor(resource_pascal, guest_module_path, ir);
                    let call =
                        build_static_call(is_async, &import_resource, method_ident, fields, &ir.resources);
                    if is_async {
                        quote!(#wrap(#call.await))
                    } else {
                        quote!(#wrap(#call))
                    }
                }
                Behavior::Virtualize => {
                    // tier-4 has no import side; sync constructors
                    // can't await an async strategy, so mint a fresh
                    // mock handle via the SDK macro.
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
                        import_resource_path_for_ctor(resource_pascal, guest_module_path, ir);
                    build_static_call(is_async, &import_resource, method_ident, fields, &ir.resources)
                }
                Behavior::Virtualize => {
                    // tier-4 static: no downstream import, so target_call
                    // is a placeholder; SyncVirtualizeStrategy dispatch
                    // below ignores it and routes through the strategy.
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
    let constructor_already_handled = matches!(
        (trait_kind, fn_kind),
        (GuestTraitKind::Resource(_), ExportFnKind::Constructor)
    );
    let target_call = if !constructor_already_handled && is_async {
        quote! { #target_call.await }
    } else {
        target_call
    };

    let resource_wrap = ir
        .fn_sigs
        .get(&args_ident.to_string())
        .and_then(|s| s.return_ty.as_ref())
        .and_then(|r| {
            build_resource_wrap(r, target_call.clone(), quote!(intermediate), &ir.resources)
        });
    let (strategy_r_ty, closure_body, final_wrap) = match resource_wrap {
        Some(rw) => (rw.intermediate_ty, rw.forward_expr, Some(rw.reverse_expr)),
        None => (nominal_return_ty.clone(), target_call.clone(), None),
    };

    // Args structs with a `borrow<R>` field carry a `<'a>` param;
    // instantiate at use sites with `<'_>` so the closure / dispatch
    // infers the live lifetime.
    let args_ty: TokenStream = if has_borrow {
        quote!(#args_ident<'_>)
    } else {
        quote!(#args_ident)
    };

    let dispatch = match behavior {
        Behavior::Transform => {
            // Annotate the closure parameter; qualified
            // `<_ as Trait<…>>::handle` dispatch doesn't propagate
            // through closure inference (E0282).
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

    let body = if !is_async {
        if matches!(fn_kind, ExportFnKind::Constructor) {
            // Constructors return a newtype-wrapped handle; strategy dispatch
            // can't apply the generic R return path.
            quote! {
                let args = #args_construct;
                #target_call
            }
        } else {
            let sync_dispatch = match behavior {
                Behavior::Transform => quote! {
                    <_ as ::splicer_tool_sdk::SyncTransformStrategy<#args_ty, #strategy_r_ty>>::handle(
                        s, call, args, |args: #args_ty| { #closure_body },
                    )
                },
                Behavior::Virtualize => quote! {
                    <_ as ::splicer_tool_sdk::SyncVirtualizeStrategy<#args_ty, #strategy_r_ty>>::handle(
                        s, call, args,
                    )
                },
            };
            match &final_wrap {
                Some(wrap) => quote! {
                    let call = ::splicer_tool_sdk::CallId {
                        interface_name: #interface_qualified_name.into(),
                        function_name: #method_name.into(),
                        id: 0,
                    };
                    let args = #args_construct;
                    let s = strategy();
                    let intermediate = #sync_dispatch;
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
                    #sync_dispatch
                },
            }
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

/// Rewrite bare iface-local type idents inside a syn type to their
/// absolute `bindings::<iface_path>::<R>` form, recursively.
///
/// Covers resource types (`Bucket`, `BucketBorrow<'_>`) and
/// non-resource named types (`ErrorCode`, user records / enums /
/// variants / flags) reached via `use` from a sibling types iface.
struct AbsolutizeResources<'a> {
    resources: &'a [ResourceInfo],
    types: &'a [NamedType],
    /// When true, prefer import-side (non-owned) resources over
    /// export-side (owned) ones for name resolution. Used by the
    /// delegation impl where the signature uses raw (original) types.
    prefer_import_side: bool,
}

impl syn::visit_mut::VisitMut for AbsolutizeResources<'_> {
    fn visit_type_path_mut(&mut self, tp: &mut syn::TypePath) {
        if tp.qself.is_none() && tp.path.segments.len() > 1 && tp.path.segments[0].ident == "_rt" {
            // wit-bindgen emits `_rt::X` (`_rt::String`, `_rt::Vec`,
            // `_rt::Box`) which only resolves inside `mod bindings`.
            // Strip the prefix; std prelude re-exports the targets.
            let mut segs = std::mem::take(&mut tp.path.segments)
                .into_iter()
                .skip(1)
                .collect::<syn::punctuated::Punctuated<_, _>>();
            std::mem::swap(&mut tp.path.segments, &mut segs);
        }
        if tp.qself.is_none() && tp.path.segments.len() == 1 {
            let seg_ident = tp.path.segments[0].ident.clone();
            // wit-bindgen emits two types per WIT resource: the bare
            // `Bucket` for `own<R>` and `BucketBorrow<'_>` for
            // `borrow<R>`. Both need the absolute `bindings::<iface>`
            // prefix at the wrapper crate root.
            //
            // Exact match first: a resource literally named `FooBorrow`
            // in own position must not be rewritten as `Foo` via the
            // suffix-strip path.
            let find_resource = |ident: &syn::Ident| -> Option<&ResourceInfo> {
                if self.prefer_import_side {
                    // Prefer non-owned (import-side) resources so the
                    // delegation impl signature uses raw types, not T' types.
                    self.resources
                        .iter()
                        .find(|r| r.rust_ident == *ident && !r.is_owned)
                        .or_else(|| self.resources.iter().find(|r| r.rust_ident == *ident))
                } else {
                    self.resources.iter().find(|r| r.rust_ident == *ident)
                }
            };
            let resource_match = find_resource(&seg_ident).or_else(|| {
                seg_ident
                    .to_string()
                    .strip_suffix("Borrow")
                    .and_then(|stripped| {
                        let stripped_ident =
                            syn::Ident::new(stripped, proc_macro2::Span::call_site());
                        find_resource(&stripped_ident)
                    })
            });
            if let Some(r) = resource_match {
                let abs = absolute_resource_path(&r.iface_path, &seg_ident);
                let trailing_args = tp.path.segments[0].arguments.clone();
                tp.path = abs;
                if let Some(last) = tp.path.segments.last_mut() {
                    last.arguments = trailing_args;
                }
            } else if let Some(nt) = self.types.iter().find(|t| t.rust_ident == seg_ident) {
                // Non-resource named types (records, variants, enums,
                // flags) reached via `use types.{ErrorCode};` live in
                // the declaring iface's module. Rewrite only when the
                // type is in a bindings module; synthesized args
                // records keep their bare ident at the wrapper root.
                if let TypeLocation::InBindings { path } = &nt.location {
                    let abs = absolute_resource_path(path, &seg_ident);
                    let trailing_args = tp.path.segments[0].arguments.clone();
                    tp.path = abs;
                    if let Some(last) = tp.path.segments.last_mut() {
                        last.arguments = trailing_args;
                    }
                }
            }
        }
        syn::visit_mut::visit_type_path_mut(self, tp);
    }
}

/// Build an absolute `bindings::<iface>::<seg_ident>` path. `seg_ident`
/// is what wit-bindgen used for the terminal segment (`Bucket` for
/// own, `BucketBorrow` for borrow, `ErrorCode` for a used variant);
/// `iface_path` comes from the declaring interface's IR entry.
fn absolute_resource_path(iface_path: &[String], seg_ident: &syn::Ident) -> syn::Path {
    use syn::punctuated::Punctuated;
    let mut segments: Punctuated<syn::PathSegment, syn::Token![::]> = Punctuated::new();
    segments.push(syn::PathSegment {
        ident: syn::Ident::new("bindings", Span::call_site()),
        arguments: syn::PathArguments::None,
    });
    for s in iface_path {
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

fn field_arg_exprs(fields: &[RecordField], is_async: bool) -> Vec<TokenStream> {
    fields
        .iter()
        .map(|f| {
            let name = &f.rust_ident;
            if f.ty.needs_borrow_at_import_call(is_async) {
                quote! { &args.#name }
            } else {
                quote! { args.#name }
            }
        })
        .collect()
}

/// Like `field_arg_exprs` but also unwraps T' export resources to their original
/// import-side handles for the downstream call. For any arg field whose type is
/// `own<R>` where R is a locally-owned (T') resource, emits
/// `args.field.into_inner::<WrapperR>().0` instead of `args.field`.
fn field_arg_exprs_for_transform(
    fields: &[RecordField],
    is_async: bool,
    resources: &[ResourceInfo],
) -> Vec<TokenStream> {
    fields
        .iter()
        .map(|f| {
            let name = &f.rust_ident;
            if f.ty.needs_borrow_at_import_call(is_async) {
                return quote! { &args.#name };
            }
            if let WitTypeRef::Handle(HandleRef::ResourceOwn(nr)) = &f.ty {
                let owned = resources.iter().find(|r| {
                    r.is_owned && r.rust_ident == nr.rust_ident && r.iface_path == nr.path
                });
                // Only unwrap in T' mode (inner_type_path set): the WrapperR
                // newtype holds the raw handle and must be unwrapped before
                // forwarding. In normal tier-3, the export-side resource IS the
                // handle — no unwrapping needed.
                if let Some(r) = owned.filter(|r| r.inner_type_path.is_some()) {
                    let wrap_ident = wrapper_ident_for(&r.rust_ident);
                    return quote! { args.#name.into_inner::<#wrap_ident>().0 };
                }
            }
            quote! { args.#name }
        })
        .collect()
}

/// Build the closure body that forwards a per-resource method call to
/// `self.0.<method>(args.x)`. `self.0` is the import-side handle held
/// by the wrapper newtype.
fn build_self_call(
    is_async: bool,
    method_ident: &syn::Ident,
    fields: &[RecordField],
) -> TokenStream {
    let arg_exprs = field_arg_exprs(fields, is_async);
    quote! { self.0.#method_ident(#(#arg_exprs),*) }
}

/// Build the closure body for a constructor / static method:
/// `<import>::<Resource>::<method>(args.x)`.
fn build_static_call(
    is_async: bool,
    import_resource: &TokenStream,
    method_ident: &syn::Ident,
    fields: &[RecordField],
    resources: &[ResourceInfo],
) -> TokenStream {
    let arg_exprs = field_arg_exprs_for_transform(fields, is_async, resources);
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

/// Compute the wrap transforms for a return type tree mentioning
/// `own<R>` at any depth. `None` if nothing needs rewriting, or if
/// every `own<R>` is a resource the wrapper merely uses (factored
/// types from an imported types iface); those pass through via
/// wit-bindgen's type identity flow.
fn build_resource_wrap(
    ret: &WitTypeRef,
    source_for_forward: TokenStream,
    source_for_reverse: TokenStream,
    resources: &[ResourceInfo],
) -> Option<CompoundWrap> {
    if !ret.contains_owned_resource(resources) {
        return None;
    }
    Some(build_wrap_at(
        ret,
        source_for_forward,
        source_for_reverse,
        resources,
    ))
}

struct CompoundWrap {
    /// Strategy R, with resource leaves rewritten to `WrapperR`.
    intermediate_ty: TokenStream,
    /// Closure-body expression (tier-3 only; computed for tier-4 but unused).
    forward_expr: TokenStream,
    /// Final-wrap expression: `Bucket::new(...)` at each resource leaf.
    reverse_expr: TokenStream,
}

fn build_wrap_at(
    ret: &WitTypeRef,
    src_fwd: TokenStream,
    src_rev: TokenStream,
    resources: &[ResourceInfo],
) -> CompoundWrap {
    // Non-(owned-)resource subtrees pass through; lets compounds
    // recurse over only the leaves that actually need rewriting.
    if !ret.contains_owned_resource(resources) {
        return CompoundWrap {
            intermediate_ty: ret.to_tokens(),
            forward_expr: src_fwd,
            reverse_expr: src_rev,
        };
    }
    match ret {
        WitTypeRef::Handle(HandleRef::ResourceOwn(nr)) => {
            // Non-owned resources pass through via the arm above;
            // reaching here means this leaf is owned.
            let wrap_ident = wrapper_ident_for(&nr.rust_ident);
            let export_path = bindings_path_tokens(&nr.path, Some(&nr.rust_ident));
            CompoundWrap {
                intermediate_ty: quote!(#wrap_ident),
                forward_expr: quote!(#wrap_ident(#src_fwd)),
                reverse_expr: quote!(#export_path::new(#src_rev)),
            }
        }
        WitTypeRef::Option(inner) => {
            let inner_wrap = build_wrap_at(inner, quote!(x), quote!(x), resources);
            let inner_ty = inner_wrap.intermediate_ty;
            let inner_fwd = inner_wrap.forward_expr;
            let inner_rev = inner_wrap.reverse_expr;
            CompoundWrap {
                intermediate_ty: quote!(::core::option::Option<#inner_ty>),
                forward_expr: quote!((#src_fwd).map(|x| #inner_fwd)),
                reverse_expr: quote!((#src_rev).map(|x| #inner_rev)),
            }
        }
        WitTypeRef::Result { ok, err } => {
            // Each Result arm either recurses (typed payload) or
            // returns `()` (unit payload, `result<_>` / `result<_, _>`).
            let arm = |t: Option<&WitTypeRef>| match t {
                Some(inner) => {
                    let w = build_wrap_at(inner, quote!(x), quote!(x), resources);
                    (w.intermediate_ty, w.forward_expr, w.reverse_expr)
                }
                None => (quote!(()), quote!(()), quote!(())),
            };
            let (ok_ty, ok_fwd, ok_rev) = arm(ok.as_deref());
            let (err_ty, err_fwd, err_rev) = arm(err.as_deref());
            let intermediate_ty = quote!(::core::result::Result<#ok_ty, #err_ty>);
            let arm_pat = |is_ok: bool, has_payload: bool| {
                let payload = if has_payload { quote!(x) } else { quote!(()) };
                if is_ok {
                    quote!(::core::result::Result::Ok(#payload))
                } else {
                    quote!(::core::result::Result::Err(#payload))
                }
            };
            let ok_pat = arm_pat(true, ok.is_some());
            let err_pat = arm_pat(false, err.is_some());
            let forward_expr = quote! {
                match #src_fwd {
                    #ok_pat => ::core::result::Result::Ok(#ok_fwd),
                    #err_pat => ::core::result::Result::Err(#err_fwd),
                }
            };
            let reverse_expr = quote! {
                match #src_rev {
                    #ok_pat => ::core::result::Result::Ok(#ok_rev),
                    #err_pat => ::core::result::Result::Err(#err_rev),
                }
            };
            CompoundWrap {
                intermediate_ty,
                forward_expr,
                reverse_expr,
            }
        }
        WitTypeRef::List(inner) => {
            let inner_wrap = build_wrap_at(inner, quote!(x), quote!(x), resources);
            let inner_ty = inner_wrap.intermediate_ty;
            let inner_fwd = inner_wrap.forward_expr;
            let inner_rev = inner_wrap.reverse_expr;
            CompoundWrap {
                intermediate_ty: quote!(::std::vec::Vec<#inner_ty>),
                forward_expr: quote!(
                    (#src_fwd).into_iter().map(|x| #inner_fwd).collect::<::std::vec::Vec<_>>()
                ),
                reverse_expr: quote!(
                    (#src_rev).into_iter().map(|x| #inner_rev).collect::<::std::vec::Vec<_>>()
                ),
            }
        }
        WitTypeRef::Tuple(elems) => {
            // Bind the tuple so each `__t.i` can move independently.
            let elem_wraps: Vec<CompoundWrap> = elems
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let idx = syn::Index::from(i);
                    build_wrap_at(e, quote!(__t.#idx), quote!(__t.#idx), resources)
                })
                .collect();
            let elem_tys: Vec<&TokenStream> =
                elem_wraps.iter().map(|w| &w.intermediate_ty).collect();
            let intermediate_ty = if elem_tys.len() == 1 {
                let t = &elem_tys[0];
                quote!((#t,))
            } else {
                quote!((#(#elem_tys),*))
            };
            let fwd_elems: Vec<&TokenStream> = elem_wraps.iter().map(|w| &w.forward_expr).collect();
            let rev_elems: Vec<&TokenStream> = elem_wraps.iter().map(|w| &w.reverse_expr).collect();
            let fwd_tuple = if fwd_elems.len() == 1 {
                let e = &fwd_elems[0];
                quote!((#e,))
            } else {
                quote!((#(#fwd_elems),*))
            };
            let rev_tuple = if rev_elems.len() == 1 {
                let e = &rev_elems[0];
                quote!((#e,))
            } else {
                quote!((#(#rev_elems),*))
            };
            let forward_expr = quote!({ let __t = #src_fwd; #fwd_tuple });
            let reverse_expr = quote!({ let __t = #src_rev; #rev_tuple });
            CompoundWrap {
                intermediate_ty,
                forward_expr,
                reverse_expr,
            }
        }
        _ => unreachable!("build_wrap_at: unsupported resource-bearing shape"),
    }
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

/// Build the closure body that calls the wrapped target with args
/// unpacked, against the import-side path pinned on the IR.
fn build_target_call(
    is_async: bool,
    method_ident: &syn::Ident,
    fields: &[RecordField],
    ir: &WrapperIR,
) -> TokenStream {
    let import_path = ir
        .target_import_path
        .as_ref()
        .expect("IR has no target_import_path; tier-3 Transform must import the wrapped target");
    let import_path = bindings_path_tokens(import_path, None);
    let arg_exprs = field_arg_exprs_for_transform(fields, is_async, &ir.resources);
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

    // `async func` deliberately: strategy dispatch is async, so
    // tests that assert on the strategy path need an async surface.
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
        let ir = build_ir(&resolve, world_id, &bindings, INTERFACE_QN).unwrap();
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
