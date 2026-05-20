//! Emit the per-method pieces of the wrapper crate: a synthetic
//! args struct (record) per Guest function, its `WitTyped` impl,
//! and the `Guest` trait impl whose method bodies dispatch into the
//! strategy through a `thread_local!` instance.
//!
//! Args structs are splicer-synthesized — they pack a function's
//! positional parameters into a single named-field record so the
//! strategy receives one typed `Args` value and the same `WitTyped`
//! machinery as user-defined types covers them.

use heck::{ToKebabCase, ToUpperCamelCase};
use proc_macro2::TokenStream;
use quote::quote;

use super::behavior_meta::Behavior;
use super::bindings_walk::{GuestMethod, GuestTrait};

/// What [`emit_guest`] produces for a single Guest trait. The wrapper
/// crate's `lib.rs` inlines each piece into the appropriate place
/// (`args_structs` + `args_witty_impls` go alongside the other
/// user-type impls; `guest_impl` goes after them).
pub struct EmittedGuest {
    /// One args struct definition per method.
    pub args_structs: Vec<TokenStream>,
    /// One `WitTyped` impl per args struct.
    pub args_witty_impls: Vec<TokenStream>,
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
) -> EmittedGuest {
    let interface_pascal = g
        .module_path
        .last()
        .cloned()
        .unwrap_or_default()
        .to_upper_camel_case();

    let mut args_structs = Vec::with_capacity(g.methods.len());
    let mut args_witty_impls = Vec::with_capacity(g.methods.len());
    let mut method_bodies = Vec::with_capacity(g.methods.len());

    for method in &g.methods {
        let params = extract_named_params(&method.sig);
        let return_ty = return_type(&method.sig);
        let args_ident = args_struct_ident(&interface_pascal, &method.ident);

        args_structs.push(emit_args_struct(&args_ident, &params));
        args_witty_impls.push(emit_args_witty(&args_ident, &params));
        method_bodies.push(emit_method_body(
            method,
            &args_ident,
            &params,
            &return_ty,
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
        args_witty_impls,
        guest_impl,
    }
}

fn extract_named_params(sig: &syn::Signature) -> Vec<(syn::Ident, syn::Type)> {
    sig.inputs
        .iter()
        .filter_map(|arg| match arg {
            syn::FnArg::Typed(pat_typed) => {
                if let syn::Pat::Ident(pat_ident) = &*pat_typed.pat {
                    Some((pat_ident.ident.clone(), (*pat_typed.ty).clone()))
                } else {
                    None
                }
            }
            // `&self` / `&mut self` don't appear in wit-bindgen Guest
            // trait methods (they're free functions, not methods on a
            // resource); ignore if encountered.
            _ => None,
        })
        .collect()
}

fn return_type(sig: &syn::Signature) -> TokenStream {
    match &sig.output {
        syn::ReturnType::Default => quote!(()),
        syn::ReturnType::Type(_, ty) => quote!(#ty),
    }
}

fn args_struct_ident(interface_pascal: &str, method_ident: &syn::Ident) -> syn::Ident {
    let method_pascal = method_ident.to_string().to_upper_camel_case();
    let name = format!("{interface_pascal}{method_pascal}Args");
    syn::Ident::new(&name, proc_macro2::Span::call_site())
}

fn emit_args_struct(args_ident: &syn::Ident, params: &[(syn::Ident, syn::Type)]) -> TokenStream {
    if params.is_empty() {
        return quote! {
            pub struct #args_ident;
        };
    }
    let fields = params.iter().map(|(name, ty)| quote! { pub #name: #ty });
    quote! {
        pub struct #args_ident {
            #(#fields),*
        }
    }
}

fn emit_args_witty(args_ident: &syn::Ident, params: &[(syn::Ident, syn::Type)]) -> TokenStream {
    // Empty-args case: emit a record with no fields. wasm-wave's
    // `Type::record` won't accept zero fields, so emit a unit-like
    // impl that pretends the value is a tuple with no elements.
    if params.is_empty() {
        return quote! {
            impl ::splicer_tool_sdk::WitTyped for #args_ident {
                fn wave_type() -> ::splicer_tool_sdk::wasm_wave::value::Type {
                    ::splicer_tool_sdk::wasm_wave::value::Type::tuple([])
                        .expect("zero-element tuple is permitted")
                }
                fn to_value(&self) -> ::splicer_tool_sdk::wasm_wave::value::Value {
                    ::splicer_tool_sdk::wasm_wave::value::Value::make_tuple(
                        &Self::wave_type(),
                        ::core::iter::empty::<::splicer_tool_sdk::wasm_wave::value::Value>(),
                    ).expect("empty tuple is consistent with declared type")
                }
                fn from_value(
                    _value: &::splicer_tool_sdk::wasm_wave::value::Value,
                ) -> ::core::result::Result<Self, ::splicer_tool_sdk::BridgeError> {
                    ::core::result::Result::Ok(Self)
                }
            }
        };
    }
    let pairs: Vec<_> = params
        .iter()
        .map(|(name, ty)| (name.clone(), name.to_string().to_kebab_case(), ty.clone()))
        .collect();

    let wave_type_fields = pairs.iter().map(|(_, wit_name, ty)| {
        quote! { (#wit_name, <#ty as ::splicer_tool_sdk::WitTyped>::wave_type()) }
    });
    let to_value_fields = pairs.iter().map(|(ident, wit_name, ty)| {
        quote! { (#wit_name, <#ty as ::splicer_tool_sdk::WitTyped>::to_value(&self.#ident)) }
    });
    let from_value_inits = pairs.iter().map(|(ident, _, _)| {
        quote! { let mut #ident = ::core::option::Option::None; }
    });
    let from_value_arms = pairs.iter().map(|(ident, wit_name, ty)| {
        quote! {
            #wit_name => #ident = ::core::option::Option::Some(
                <#ty as ::splicer_tool_sdk::WitTyped>::from_value(&v)?
            ),
        }
    });
    let from_value_constructors = pairs.iter().map(|(ident, wit_name, _)| {
        quote! {
            #ident: #ident.ok_or_else(|| ::splicer_tool_sdk::BridgeError::MissingField {
                name: #wit_name.into(),
            })?,
        }
    });

    quote! {
        impl ::splicer_tool_sdk::WitTyped for #args_ident {
            fn wave_type() -> ::splicer_tool_sdk::wasm_wave::value::Type {
                ::splicer_tool_sdk::wasm_wave::value::Type::record([
                    #(#wave_type_fields),*
                ]).expect("args struct has at least one field")
            }
            fn to_value(&self) -> ::splicer_tool_sdk::wasm_wave::value::Value {
                ::splicer_tool_sdk::wasm_wave::value::Value::make_record(
                    &Self::wave_type(),
                    [#(#to_value_fields),*],
                ).expect("emitted field values match the declared record type")
            }
            fn from_value(
                value: &::splicer_tool_sdk::wasm_wave::value::Value,
            ) -> ::core::result::Result<Self, ::splicer_tool_sdk::BridgeError> {
                #(#from_value_inits)*
                for (name, v) in value.unwrap_record() {
                    match name.as_ref() {
                        #(#from_value_arms)*
                        _ => {}
                    }
                }
                ::core::result::Result::Ok(Self {
                    #(#from_value_constructors)*
                })
            }
        }
    }
}

fn emit_method_body(
    method: &GuestMethod,
    args_ident: &syn::Ident,
    params: &[(syn::Ident, syn::Type)],
    return_ty: &TokenStream,
    interface_qualified_name: &str,
    behavior: Behavior,
    guest_module_path: &[String],
) -> TokenStream {
    let method_ident = &method.ident;
    let method_name = method_ident.to_string();
    let sig_inputs = &method.sig.inputs;
    let sig_output = &method.sig.output;

    // Construct args { a, b, ... } from the function's positional params.
    let args_construct = if params.is_empty() {
        quote! { #args_ident }
    } else {
        let names = params.iter().map(|(n, _)| n);
        quote! { #args_ident { #(#names),* } }
    };

    // The downstream closure unpacks args back into positional and
    // calls the target import. For tier-3 (forward); skipped entirely
    // for tier-4 (virtualize).
    let target_call = build_target_call(method_ident, params, guest_module_path);

    let dispatch = match behavior {
        Behavior::Forward => {
            quote! {
                <_ as ::splicer_tool_sdk::ForwardStrategy<#args_ident, #return_ty>>::handle(
                    s,
                    call,
                    args,
                    |args| async move { #target_call },
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
            STRATEGY.with_borrow(|s| #dispatch).await
        }
    }
}

/// Build the closure body that calls the wrapped target with the
/// args unpacked: `bindings::test::pkg::ops::add(args.a, args.b)`.
/// The path is the *import* side of the bindings (no `exports::`
/// prefix) — that's where wit-bindgen places the target's import
/// callables.
fn build_target_call(
    method_ident: &syn::Ident,
    params: &[(syn::Ident, syn::Type)],
    guest_module_path: &[String],
) -> TokenStream {
    // The Guest trait lives under `exports::<pkg>::<iface>`. The
    // matching import lives under `<pkg>::<iface>` (no exports
    // prefix). Strip the leading `exports` segment if present.
    let import_segments: Vec<&str> = guest_module_path
        .iter()
        .map(String::as_str)
        .skip_while(|s| *s == "exports")
        .collect();
    let import_path: TokenStream = if import_segments.is_empty() {
        quote!(bindings)
    } else {
        let segs = import_segments
            .iter()
            .map(|s| syn::Ident::new(s, proc_macro2::Span::call_site()));
        quote!(bindings::#(#segs)::*)
    };
    let arg_exprs = params.iter().map(|(n, _)| quote! { args.#n });
    quote! { #import_path::#method_ident(#(#arg_exprs),*) }
}

fn build_module_path(segments: &[String]) -> TokenStream {
    let idents = segments
        .iter()
        .map(|s| syn::Ident::new(s, proc_macro2::Span::call_site()));
    quote!(bindings::#(#idents)::*)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::typed::bindgen::run_wit_bindgen_rust;
    use crate::adapter::typed::bindings_walk::walk_bindings;

    fn normalize(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn emit_for_tiny(behavior: Behavior) -> String {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                add: func(a: u32, b: u32) -> u32;
            }
            world w { export ops; }
        "#;
        let src = run_wit_bindgen_rust(wit, Some("w")).unwrap();
        let bindings = walk_bindings(&src).unwrap();
        let g = &bindings.guest_traits[0];
        let emitted = emit_guest(g, "test:pkg/ops@0.1.0", behavior);
        let mut out = String::new();
        for t in &emitted.args_structs {
            out.push_str(&t.to_string());
        }
        for t in &emitted.args_witty_impls {
            out.push_str(&t.to_string());
        }
        out.push_str(&emitted.guest_impl.to_string());
        normalize(&out)
    }

    #[test]
    fn emits_args_struct_with_fields() {
        let out = emit_for_tiny(Behavior::Forward);
        assert!(
            out.contains("pub struct OpsAddArgs"),
            "expected OpsAddArgs struct: {out}"
        );
        assert!(out.contains("pub a : u32"), "expected field `a: u32`: {out}");
        assert!(out.contains("pub b : u32"), "expected field `b: u32`: {out}");
    }

    #[test]
    fn forward_emission_uses_forward_strategy_and_downstream_closure() {
        let out = emit_for_tiny(Behavior::Forward);
        assert!(
            out.contains("ForwardStrategy"),
            "expected ForwardStrategy dispatch: {out}"
        );
        // The downstream closure calls the import-side bindings path.
        assert!(
            out.contains("bindings :: test :: pkg :: ops :: add"),
            "expected import-side call to bindings::test::pkg::ops::add: {out}"
        );
        // The closure threads args back into positional args.
        assert!(out.contains("args . a"), "expected args.a in closure: {out}");
        assert!(out.contains("args . b"), "expected args.b in closure: {out}");
    }

    #[test]
    fn virtualize_emission_uses_virtualize_strategy_without_closure() {
        let out = emit_for_tiny(Behavior::Virtualize);
        assert!(
            out.contains("VirtualizeStrategy"),
            "expected VirtualizeStrategy dispatch: {out}"
        );
        // No downstream closure — virtualize strategies don't get one.
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
        let out = emit_for_tiny(Behavior::Forward);
        assert!(
            out.contains("\"test:pkg/ops@0.1.0\""),
            "expected qualified interface in CallId: {out}"
        );
        assert!(out.contains("\"add\""), "expected function name in CallId: {out}");
    }

    #[test]
    fn args_struct_witty_impl_emitted() {
        let out = emit_for_tiny(Behavior::Forward);
        assert!(
            out.contains("impl :: splicer_tool_sdk :: WitTyped for OpsAddArgs"),
            "expected WitTyped impl for args struct: {out}"
        );
        // Kebab-case field names appear in the impl.
        assert!(out.contains("\"a\""), "expected kebab-case field name 'a': {out}");
        assert!(out.contains("\"b\""), "expected kebab-case field name 'b': {out}");
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
        let src = run_wit_bindgen_rust(wit, Some("w")).unwrap();
        let bindings = walk_bindings(&src).unwrap();
        let g = &bindings.guest_traits[0];
        let emitted = emit_guest(g, "test:pkg/ops@0.1.0", Behavior::Forward);
        let out = normalize(
            &emitted
                .args_structs
                .iter()
                .chain(&emitted.args_witty_impls)
                .map(|t| t.to_string())
                .collect::<String>(),
        );
        assert!(
            out.contains("pub struct OpsNoopArgs ;"),
            "expected unit struct for zero-arg method: {out}"
        );
        assert!(
            out.contains("Type :: tuple"),
            "expected zero-tuple WitTyped for unit args: {out}"
        );
    }
}
