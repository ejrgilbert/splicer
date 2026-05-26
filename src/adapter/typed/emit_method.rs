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
use proc_macro2::TokenStream;
use quote::quote;

use super::bindings_index::{bindings_path_tokens, GuestMethod, GuestTrait};
use super::ir::{args_struct_ident, NamedKind, NamedType, RecordField, TypeLocation, WrapperIR};
use super::Behavior;

/// What [`emit_guest`] produces for a single Guest trait.
pub struct EmittedGuest {
    /// One args struct definition per method.
    pub args_structs: Vec<TokenStream>,
    /// A single `impl <ModPath>::Guest for Wrapper` block containing
    /// every method body.
    pub guest_impl: TokenStream,
}

/// Emit the args structs and Guest impl for one exported interface.
///
/// `interface_qualified_name` is the `"package:ns/iface@ver"` form
/// of the wrapped interface; it goes into each emitted `CallId`.
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

    let mut args_structs = Vec::with_capacity(g.methods.len());
    let mut method_bodies = Vec::with_capacity(g.methods.len());

    for method in &g.methods {
        let args_ident = args_struct_ident(&interface_pascal, &method.ident.to_string());
        let args_record = find_args_record(ir, &args_ident);

        args_structs.push(emit_args_struct(&args_ident, args_record));
        method_bodies.push(emit_method_body(
            method,
            &args_ident,
            args_record,
            interface_qualified_name,
            behavior,
            &g.module_path,
        ));
    }

    let trait_path = build_module_path(&g.module_path);
    let guest_impl = quote! {
        impl #trait_path::Guest for Wrapper {
            #(#method_bodies)*
        }
    };

    EmittedGuest {
        args_structs,
        guest_impl,
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

fn emit_args_struct(args_ident: &syn::Ident, args_record: &NamedType) -> TokenStream {
    debug_assert!(matches!(args_record.location, TypeLocation::TopLevel));
    let field_tokens = args_fields(args_record).iter().map(|f| {
        let name = &f.rust_ident;
        let ty = f.ty.to_tokens();
        quote! { pub #name: #ty }
    });
    // Named-empty `{}` (not `;`) so the zero-arg `WitTyped` impl can
    // construct via `Self {}`.
    quote! {
        pub struct #args_ident {
            #(#field_tokens),*
        }
    }
}

fn emit_method_body(
    method: &GuestMethod,
    args_ident: &syn::Ident,
    args_record: &NamedType,
    interface_qualified_name: &str,
    behavior: Behavior,
    guest_module_path: &[String],
) -> TokenStream {
    let method_ident = &method.ident;
    let method_name = method_ident.to_string();
    let sig_inputs = &method.sig.inputs;
    let sig_output = &method.sig.output;
    let return_ty = return_type(&method.sig);
    let fields = args_fields(args_record);

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

    // Transform strategies forward to the wrapped target via the
    // closure; virtualize strategies replace the target and never
    // call into it. `.await` only if the Guest method is async.
    let target_call = build_target_call(method_ident, fields, guest_module_path);
    let target_call = if method.sig.asyncness.is_some() {
        quote! { #target_call.await }
    } else {
        target_call
    };

    let dispatch = match behavior {
        Behavior::Transform => {
            // Annotate the closure parameter — qualified
            // `<_ as Trait<…>>::handle` dispatch doesn't propagate
            // into closure inference (E0282).
            quote! {
                <_ as ::splicer_tool_sdk::TransformStrategy<#args_ident, #return_ty>>::handle(
                    s,
                    call,
                    args,
                    |args: #args_ident| async move { #target_call },
                )
            }
        }
        Behavior::Virtualize => {
            quote! {
                <_ as ::splicer_tool_sdk::VirtualizeStrategy<#args_ident, #return_ty>>::handle(
                    s,
                    call,
                    args,
                )
            }
        }
    };

    quote! {
        async fn #method_ident(#sig_inputs) #sig_output {
            let call = ::splicer_tool_sdk::CallId {
                interface_name: #interface_qualified_name.into(),
                function_name: #method_name.into(),
                id: 0,
            };
            let args = #args_construct;
            let s = strategy();
            #dispatch.await
        }
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

    const TINY_WIT: &str = r#"
        package test:pkg@0.1.0;
        interface ops {
            add: func(a: u32, b: u32) -> u32;
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
