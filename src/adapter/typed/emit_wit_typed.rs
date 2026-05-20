//! Emit `WitTyped` impls for record types a user defines in their
//! target WIT.
//!
//! `WitTyped` impls split by where the type comes from. **WIT core
//! types** (primitives, `Vec<T>`, `Option<T>`, `Result<T, E>`) have
//! hand-written impls in `splicer_tool_sdk::wave_bridge`, shared
//! across every wrapper crate. **User-defined types** (records,
//! enums, variants) have a different Rust shape per WIT, so splicer
//! generates their impls per wrapper crate. This module is that
//! generator.
//!
//! Generated impls delegate to the core-type impls: a record
//! `Point { x: u32 }` gets a `to_value` body that calls
//! `<u32 as WitTyped>::to_value(&self.x)`, letting the SDK's `u32`
//! impl handle the actual encoding.
//!
//! Emitted impls reference types as `bindings::<path>::<Type>`. The
//! generated wrapper crate's `lib.rs` nests wit-bindgen's output
//! under `mod bindings`, and our additions live alongside.

use heck::ToKebabCase;
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Fields, ItemEnum, ItemStruct};

use super::bindings_walk::{TypeDef, TypeDefKind};

/// Emit one `impl WitTyped` block per supported [`TypeDef`].
/// Tuple/unit structs and variant cases with named fields are
/// skipped (they don't appear in wit-bindgen output for value-typed
/// WITs).
pub fn emit_wit_typed_impls(types: &[TypeDef]) -> Vec<TokenStream> {
    types.iter().filter_map(emit_one).collect()
}

fn emit_one(td: &TypeDef) -> Option<TokenStream> {
    match &td.kind {
        TypeDefKind::Struct(s) => emit_record(&td.module_path, s),
        TypeDefKind::Enum(e) => emit_enum_like(&td.module_path, e),
    }
}

/// Dispatch on the Rust enum's shape: if every variant is unit, the
/// type came from a WIT `enum` declaration (emit using `Value::Enum`
/// constructors). Otherwise it came from a WIT `variant` and at
/// least one case carries a payload (emit using `Value::Variant`).
fn emit_enum_like(module_path: &[String], e: &ItemEnum) -> Option<TokenStream> {
    let all_unit = e.variants.iter().all(|v| matches!(v.fields, Fields::Unit));
    if all_unit {
        emit_unit_enum(module_path, e)
    } else {
        emit_variant(module_path, e)
    }
}

/// Emit a `WitTyped` impl for a Rust enum that came from a WIT `enum`.
/// Every variant is unit; the WitTyped impl maps each to a kebab-case
/// case name via `Value::make_enum` / `Value::unwrap_enum`.
fn emit_unit_enum(module_path: &[String], e: &ItemEnum) -> Option<TokenStream> {
    let type_ident = &e.ident;
    let type_name = type_ident.to_string();
    let type_path = build_bindings_path(module_path, type_ident);

    let cases: Vec<(syn::Ident, String)> = e
        .variants
        .iter()
        .map(|v| (v.ident.clone(), v.ident.to_string().to_kebab_case()))
        .collect();
    if cases.is_empty() {
        return None;
    }

    let case_names = cases.iter().map(|(_, name)| name);
    let to_arms = cases.iter().map(|(ident, name)| {
        quote! { Self::#ident => #name }
    });
    let from_arms = cases.iter().map(|(ident, name)| {
        quote! { #name => ::core::result::Result::Ok(Self::#ident) }
    });

    Some(quote! {
        impl ::splicer_tool_sdk::WitTyped for #type_path {
            fn wave_type() -> ::splicer_tool_sdk::wasm_wave::value::Type {
                ::splicer_tool_sdk::wasm_wave::value::Type::enum_ty([
                    #(#case_names),*
                ]).expect("WIT enum has at least one case")
            }

            fn to_value(&self) -> ::splicer_tool_sdk::wasm_wave::value::Value {
                let case: &str = match self { #(#to_arms),* };
                ::splicer_tool_sdk::wasm_wave::value::Value::make_enum(&Self::wave_type(), case)
                    .expect("emitted case is in the declared enum")
            }

            fn from_value(
                value: &::splicer_tool_sdk::wasm_wave::value::Value,
            ) -> ::core::result::Result<Self, ::splicer_tool_sdk::BridgeError> {
                let case = value.unwrap_enum();
                match &*case {
                    #(#from_arms,)*
                    other => ::core::result::Result::Err(
                        ::splicer_tool_sdk::BridgeError::UnknownCase {
                            type_kind: ::splicer_tool_sdk::wasm_wave::wasm::WasmTypeKind::Enum,
                            case: format!("{} (in {})", other, #type_name),
                        }
                    ),
                }
            }
        }
    })
}

/// Emit a `WitTyped` impl for a Rust enum that came from a WIT `variant`.
/// Each variant is either unit or carries a single unnamed payload;
/// the WitTyped impl maps each to `Value::make_variant` / `unwrap_variant`.
fn emit_variant(module_path: &[String], e: &ItemEnum) -> Option<TokenStream> {
    let type_ident = &e.ident;
    let type_name = type_ident.to_string();
    let type_path = build_bindings_path(module_path, type_ident);

    // Per case: (rust_variant_ident, wit_kebab_case_name, optional_payload_type)
    let cases: Vec<(syn::Ident, String, Option<syn::Type>)> = e
        .variants
        .iter()
        .filter_map(|v| {
            let kebab = v.ident.to_string().to_kebab_case();
            match &v.fields {
                Fields::Unit => Some((v.ident.clone(), kebab, None)),
                Fields::Unnamed(unnamed) if unnamed.unnamed.len() == 1 => {
                    let ty = unnamed.unnamed.first().unwrap().ty.clone();
                    Some((v.ident.clone(), kebab, Some(ty)))
                }
                // Multi-field tuple variants and struct-named variants
                // don't appear in wit-bindgen output for WIT variants
                // (which only allow zero-or-one unnamed payload). Skip
                // the whole emit if we hit one — the user can address
                // it explicitly.
                _ => None,
            }
        })
        .collect();
    if cases.is_empty() {
        return None;
    }

    let wave_cases = cases.iter().map(|(_, name, payload)| match payload {
        Some(ty) => quote! {
            (#name, ::core::option::Option::Some(
                <#ty as ::splicer_tool_sdk::WitTyped>::wave_type()
            ))
        },
        None => quote! { (#name, ::core::option::Option::None) },
    });
    let to_arms = cases.iter().map(|(ident, name, payload)| match payload {
        Some(ty) => quote! {
            Self::#ident(v) => (#name, ::core::option::Option::Some(
                <#ty as ::splicer_tool_sdk::WitTyped>::to_value(v)
            ))
        },
        None => quote! { Self::#ident => (#name, ::core::option::Option::None) },
    });
    let from_arms = cases.iter().map(|(ident, name, payload)| {
        let case_label = format!("{type_name}::{ident}");
        match payload {
            Some(ty) => quote! {
                #name => {
                    let payload = payload.ok_or_else(|| {
                        ::splicer_tool_sdk::BridgeError::MissingField {
                            name: format!("{} payload", #case_label),
                        }
                    })?;
                    ::core::result::Result::Ok(Self::#ident(
                        <#ty as ::splicer_tool_sdk::WitTyped>::from_value(&payload)?
                    ))
                }
            },
            None => quote! {
                #name => {
                    if payload.is_some() {
                        return ::core::result::Result::Err(
                            ::splicer_tool_sdk::BridgeError::ExpectedTypeShape(
                                "unit variant case received an unexpected payload"
                            )
                        );
                    }
                    ::core::result::Result::Ok(Self::#ident)
                }
            },
        }
    });

    Some(quote! {
        impl ::splicer_tool_sdk::WitTyped for #type_path {
            fn wave_type() -> ::splicer_tool_sdk::wasm_wave::value::Type {
                ::splicer_tool_sdk::wasm_wave::value::Type::variant([
                    #(#wave_cases),*
                ]).expect("WIT variant has at least one case")
            }

            fn to_value(&self) -> ::splicer_tool_sdk::wasm_wave::value::Value {
                let (case, payload): (&str, ::core::option::Option<_>) = match self {
                    #(#to_arms),*
                };
                ::splicer_tool_sdk::wasm_wave::value::Value::make_variant(
                    &Self::wave_type(),
                    case,
                    payload,
                ).expect("emitted case is in the declared variant")
            }

            fn from_value(
                value: &::splicer_tool_sdk::wasm_wave::value::Value,
            ) -> ::core::result::Result<Self, ::splicer_tool_sdk::BridgeError> {
                let (case, payload) = value.unwrap_variant();
                match &*case {
                    #(#from_arms,)*
                    other => ::core::result::Result::Err(
                        ::splicer_tool_sdk::BridgeError::UnknownCase {
                            type_kind: ::splicer_tool_sdk::wasm_wave::wasm::WasmTypeKind::Variant,
                            case: format!("{} (in {})", other, #type_name),
                        }
                    ),
                }
            }
        }
    })
}

/// Emit a `WitTyped` impl for a wit-bindgen-generated record struct.
fn emit_record(module_path: &[String], s: &ItemStruct) -> Option<TokenStream> {
    let fields = match &s.fields {
        Fields::Named(named) => &named.named,
        // Tuple structs and unit structs aren't records.
        _ => return None,
    };

    let type_ident = &s.ident;
    let type_path = build_bindings_path(module_path, type_ident);

    // Pre-compute the (rust_ident, wit_kebab_name, field_type) tuples
    // we'll splice into each method body.
    let fs: Vec<(syn::Ident, String, syn::Type)> = fields
        .iter()
        .filter_map(|f| {
            let ident = f.ident.as_ref()?;
            let wit_name = ident.to_string().to_kebab_case();
            Some((ident.clone(), wit_name, f.ty.clone()))
        })
        .collect();
    if fs.is_empty() {
        return None;
    }

    let wave_type_fields = fs.iter().map(|(_, wit_name, ty)| {
        quote! { (#wit_name, <#ty as ::splicer_tool_sdk::WitTyped>::wave_type()) }
    });
    let to_value_fields = fs.iter().map(|(ident, wit_name, ty)| {
        quote! { (#wit_name, <#ty as ::splicer_tool_sdk::WitTyped>::to_value(&self.#ident)) }
    });
    let from_value_inits = fs.iter().map(|(ident, _, _)| {
        quote! { let mut #ident = ::core::option::Option::None; }
    });
    let from_value_arms = fs.iter().map(|(ident, wit_name, ty)| {
        quote! {
            #wit_name => #ident = ::core::option::Option::Some(
                <#ty as ::splicer_tool_sdk::WitTyped>::from_value(&v)?
            ),
        }
    });
    let from_value_constructors = fs.iter().map(|(ident, wit_name, _)| {
        quote! {
            #ident: #ident.ok_or_else(|| ::splicer_tool_sdk::BridgeError::MissingField {
                name: #wit_name.into(),
            })?,
        }
    });

    Some(quote! {
        impl ::splicer_tool_sdk::WitTyped for #type_path {
            fn wave_type() -> ::splicer_tool_sdk::wasm_wave::value::Type {
                ::splicer_tool_sdk::wasm_wave::value::Type::record([
                    #(#wave_type_fields),*
                ]).expect("wit-bindgen records have at least one field")
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
                    match &*name {
                        #(#from_value_arms)*
                        _ => {}
                    }
                }
                ::core::result::Result::Ok(Self {
                    #(#from_value_constructors)*
                })
            }
        }
    })
}

/// Build `bindings::<seg1>::<seg2>::...::<type_ident>`.
fn build_bindings_path(module_path: &[String], type_ident: &syn::Ident) -> TokenStream {
    let segments: Vec<syn::Ident> = module_path
        .iter()
        .map(|s| syn::Ident::new(s, proc_macro2::Span::call_site()))
        .collect();
    if segments.is_empty() {
        quote!(bindings::#type_ident)
    } else {
        quote!(bindings::#(#segments)::*::#type_ident)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::typed::bindgen::run_wit_bindgen_rust;
    use crate::adapter::typed::bindings_walk::walk_bindings;

    fn normalize(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn emits_impl_for_simple_record() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                record point { x: u32, y: u32 }
                add: func(p: point) -> u32;
            }
            world w { export ops; }
        "#;
        let src = run_wit_bindgen_rust(wit, Some("w")).unwrap();
        let bindings = walk_bindings(&src).unwrap();
        let impls = emit_wit_typed_impls(&bindings.types);

        let combined = normalize(&impls.iter().map(|t| t.to_string()).collect::<String>());
        assert!(
            combined.contains("impl :: splicer_tool_sdk :: WitTyped for"),
            "expected an impl block; got: {combined}"
        );
        // Field names should appear in their kebab-case form.
        assert!(combined.contains("\"x\""), "missing field `x`: {combined}");
        assert!(combined.contains("\"y\""), "missing field `y`: {combined}");
        // The type path should be prefixed with bindings::.
        assert!(
            combined.contains("bindings ::"),
            "type path should be `bindings::...`: {combined}"
        );
    }

    #[test]
    fn emits_kebab_case_for_snake_case_field_idents() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                record pet { pet-name: string, age-years: u32 }
                set: func(p: pet);
            }
            world w { export ops; }
        "#;
        let src = run_wit_bindgen_rust(wit, Some("w")).unwrap();
        let bindings = walk_bindings(&src).unwrap();
        let impls = emit_wit_typed_impls(&bindings.types);
        let combined = normalize(&impls.iter().map(|t| t.to_string()).collect::<String>());

        // Rust field identifiers come out as snake_case from wit-bindgen
        // (`pet_name`), but we emit the WIT kebab-case in the impl.
        assert!(combined.contains("\"pet-name\""), "want kebab-case key: {combined}");
        assert!(combined.contains("\"age-years\""), "want kebab-case key: {combined}");
        assert!(
            combined.contains("self . pet_name"),
            "self.<snake_case> access expected: {combined}"
        );
    }

    #[test]
    fn emits_impl_for_unit_only_enum() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                enum color { red, green, blue }
                tag: func(c: color);
            }
            world w { export ops; }
        "#;
        let src = run_wit_bindgen_rust(wit, Some("w")).unwrap();
        let bindings = walk_bindings(&src).unwrap();
        let combined = normalize(
            &emit_wit_typed_impls(&bindings.types)
                .iter()
                .map(|t| t.to_string())
                .collect::<String>(),
        );

        assert!(
            combined.contains("Type :: enum_ty"),
            "unit-only enum should use Type::enum_ty: {combined}"
        );
        // All kebab-case case names should appear.
        for name in ["\"red\"", "\"green\"", "\"blue\""] {
            assert!(combined.contains(name), "missing case {name}: {combined}");
        }
        // Should NOT use the variant constructors.
        assert!(
            !combined.contains("Type :: variant"),
            "unit-only enum should not use Type::variant: {combined}"
        );
    }

    #[test]
    fn emits_impl_for_variant_with_payloads() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                record entry { key: string, value: u32 }
                variant outcome {
                    hit(entry),
                    miss,
                    close(u32),
                }
                lookup: func(name: string) -> outcome;
            }
            world w { export ops; }
        "#;
        let src = run_wit_bindgen_rust(wit, Some("w")).unwrap();
        let bindings = walk_bindings(&src).unwrap();
        let combined = normalize(
            &emit_wit_typed_impls(&bindings.types)
                .iter()
                .map(|t| t.to_string())
                .collect::<String>(),
        );

        // Variant emission should use the variant constructors.
        assert!(
            combined.contains("Type :: variant"),
            "variant should use Type::variant: {combined}"
        );
        // Case names appear kebab-cased.
        for name in ["\"hit\"", "\"miss\"", "\"close\""] {
            assert!(combined.contains(name), "missing case {name}: {combined}");
        }
        // Payload-carrying cases should delegate to their payload type's WitTyped impl.
        assert!(
            combined.contains("as :: splicer_tool_sdk :: WitTyped >"),
            "payload conversion should delegate to WitTyped: {combined}"
        );
    }

    #[test]
    fn variant_with_dashed_case_name_round_trips_kebab() {
        let wit = r#"
            package test:pkg@0.1.0;
            interface ops {
                variant outcome {
                    not-found,
                    found(u32),
                }
                lookup: func() -> outcome;
            }
            world w { export ops; }
        "#;
        let src = run_wit_bindgen_rust(wit, Some("w")).unwrap();
        let bindings = walk_bindings(&src).unwrap();
        let combined = normalize(
            &emit_wit_typed_impls(&bindings.types)
                .iter()
                .map(|t| t.to_string())
                .collect::<String>(),
        );
        // wit-bindgen produces PascalCase `NotFound`; emission must
        // map it back to kebab-case `not-found`.
        assert!(
            combined.contains("\"not-found\""),
            "expected kebab-case case name in output: {combined}"
        );
        assert!(
            combined.contains("Self :: NotFound"),
            "expected PascalCase Rust variant ident in output: {combined}"
        );
    }
}
