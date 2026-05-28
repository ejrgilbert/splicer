//! `#[derive(WitTyped)]` for user-authored Rust types.
//!
//! `WitTyped` carries a type's WIT shape plus both conversion
//! directions to/from a wasm-wave `Value`, which is how a Rust value
//! flows through splicer's cells wire format. The SDK hand-writes the
//! impls for WIT core types (primitives, `Vec`, `Option`, `Result`,
//! tuples), and splicer's adapter codegen emits them for the types
//! `wit-bindgen` generates inside a wrapper crate. This derive covers
//! the third source: types whose Rust definition the user owns.
//!
//! # When you need this
//!
//! You do **not** need this for a splicer-generated tier-3/4 wrapper
//! component. There, splicer runs `wit-bindgen` against the target WIT
//! and auto-emits `WitTyped` for every generated type, so a strategy's
//! `Args` / `R` are already `WitTyped` with nothing to derive.
//!
//! Reach for the derive only when you own the type definition:
//!
//! 1. A plain struct or enum you declare by hand (a fixture, a config
//!    record, a model type) and want to push through cells / WAVE.
//! 2. A component **you** author with your own `wit_bindgen::generate!`
//!    call (e.g. a handwritten wrapper that splicer composes in). You
//!    cannot edit the generated types, but you can tell the generator
//!    to derive `WitTyped` on all of them via `additional_derives`.
//!
//! ```ignore
//! // A struct you wrote by hand.
//! use splicer_tool_sdk::WitTyped; // re-export, needs the `derive` feature
//!
//! #[derive(WitTyped)]
//! struct MockResponse {
//!     status: u32,
//!     body: String,
//! }
//! ```
//!
//! ```ignore
//! // Every wit-bindgen-generated type in a component you author.
//! wit_bindgen::generate!({
//!     world: "my-world",
//!     additional_derives: [splicer_tool_sdk::WitTyped],
//! });
//! ```
//!
//! # Shape mapping
//!
//! The Rust shape determines the WIT shape:
//!
//! - **named-field struct** maps to a `record`. Each field name is
//!   kebab-cased for the WIT side (`pet_name` becomes `pet-name`).
//! - **enum with all-unit variants** maps to an `enum`.
//! - **enum where any variant carries a payload** maps to a
//!   `variant`. A variant case carries at most one payload type, so
//!   each Rust variant must be unit (`Foo`) or a single-field tuple
//!   (`Foo(T)`).
//!
//! A `#[wit(name = "...")]` attribute on a field or variant overrides
//! the kebab-cased name when the Rust identifier and the WIT name
//! cannot be derived from one another (keyword escapes, abbreviations).
//!
//! Generic type parameters each gain a `WitTyped` bound on the
//! generated impl.

use heck::ToKebabCase;
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::ext::IdentExt;
use syn::{parse_quote, Data, DeriveInput, Fields, Variant};

#[proc_macro_derive(WitTyped, attributes(wit))]
pub fn derive_wit_typed(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    match &input.data {
        Data::Struct(s) => expand_record(&input, &s.fields),
        Data::Enum(e) => expand_enum(&input, &e.variants),
        Data::Union(u) => Err(syn::Error::new_spanned(
            u.union_token,
            "WitTyped cannot be derived for unions; WIT has no union shape",
        )),
    }
}

/// The WIT name for a member: an explicit `#[wit(name = "...")]`, else
/// the kebab-cased Rust identifier.
fn wit_name_for(ident: &syn::Ident, attrs: &[syn::Attribute]) -> syn::Result<String> {
    if let Some(name) = wit_rename(attrs)? {
        return Ok(name);
    }
    Ok(ident.unraw().to_string().to_kebab_case())
}

/// Parse `#[wit(name = "...")]`, returning the override if present.
fn wit_rename(attrs: &[syn::Attribute]) -> syn::Result<Option<String>> {
    let mut rename = None;
    for attr in attrs {
        if !attr.path().is_ident("wit") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                let lit: syn::LitStr = meta.value()?.parse()?;
                rename = Some(lit.value());
                Ok(())
            } else {
                Err(meta.error("unknown `wit` attribute; expected `name = \"...\"`"))
            }
        })?;
    }
    Ok(rename)
}

/// Clone the input's generics and add a `WitTyped` bound to every type
/// parameter so the generated impl covers generic types.
fn bounded_generics(input: &DeriveInput) -> syn::Generics {
    let mut generics = input.generics.clone();
    for param in &mut generics.params {
        if let syn::GenericParam::Type(tp) = param {
            tp.bounds.push(parse_quote!(::splicer_tool_sdk::WitTyped));
        }
    }
    generics
}

/// Wrap three shape-specific method bodies in the `WitTyped` impl
/// skeleton: generic bounds, the trait signatures, and the `WasmValue`
/// import that the `make_*` / `unwrap_*` calls in `to_value` /
/// `from_value` resolve through. The `from_value` body binds `value`.
fn wit_typed_impl(
    input: &DeriveInput,
    wave_type_body: TokenStream2,
    to_value_body: TokenStream2,
    from_value_body: TokenStream2,
) -> TokenStream2 {
    let name = &input.ident;
    let generics = bounded_generics(input);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    quote! {
        impl #impl_generics ::splicer_tool_sdk::WitTyped for #name #ty_generics #where_clause {
            fn wave_type() -> ::splicer_tool_sdk::wasm_wave::value::Type {
                #wave_type_body
            }

            fn to_value(&self) -> ::splicer_tool_sdk::wasm_wave::value::Value {
                use ::splicer_tool_sdk::wasm_wave::wasm::WasmValue as _;
                #to_value_body
            }

            fn from_value(
                value: &::splicer_tool_sdk::wasm_wave::value::Value,
            ) -> ::core::result::Result<Self, ::splicer_tool_sdk::BridgeError> {
                use ::splicer_tool_sdk::wasm_wave::wasm::WasmValue as _;
                #from_value_body
            }
        }
    }
}

fn expand_record(input: &DeriveInput, fields: &Fields) -> syn::Result<TokenStream2> {
    let named = match fields {
        Fields::Named(named) => &named.named,
        Fields::Unit => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "WitTyped on a struct requires named fields (a WIT record); unit structs \
                 have no record shape",
            ));
        }
        Fields::Unnamed(_) => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "WitTyped on a struct requires named fields (a WIT record); tuple structs \
                 have no record shape",
            ));
        }
    };

    // wasm-wave rejects zero-field records and zero-element tuples, so
    // a fieldless record encodes as a single-field sentinel record.
    // Spelled out (not via `wit_typed_impl`) because `from_value`
    // ignores its argument here, unlike every other shape.
    if named.is_empty() {
        let name = &input.ident;
        let generics = bounded_generics(input);
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        return Ok(quote! {
            impl #impl_generics ::splicer_tool_sdk::WitTyped for #name #ty_generics #where_clause {
                fn wave_type() -> ::splicer_tool_sdk::wasm_wave::value::Type {
                    ::splicer_tool_sdk::wasm_wave::value::Type::record([
                        ("unit", ::splicer_tool_sdk::wasm_wave::value::Type::BOOL),
                    ]).expect("single-field record is permitted")
                }
                fn to_value(&self) -> ::splicer_tool_sdk::wasm_wave::value::Value {
                    use ::splicer_tool_sdk::wasm_wave::wasm::WasmValue as _;
                    ::splicer_tool_sdk::wasm_wave::value::Value::make_record(
                        &Self::wave_type(),
                        [(
                            "unit",
                            ::splicer_tool_sdk::wasm_wave::value::Value::make_bool(true),
                        )],
                    ).expect("sentinel field matches the declared record type")
                }
                fn from_value(
                    _value: &::splicer_tool_sdk::wasm_wave::value::Value,
                ) -> ::core::result::Result<Self, ::splicer_tool_sdk::BridgeError> {
                    ::core::result::Result::Ok(Self {})
                }
            }
        });
    }

    let mut field_idents = Vec::with_capacity(named.len());
    let mut wit_names = Vec::with_capacity(named.len());
    let mut field_tys = Vec::with_capacity(named.len());
    for field in named {
        let ident = field.ident.as_ref().expect("named field has an ident");
        field_idents.push(ident.clone());
        wit_names.push(wit_name_for(ident, &field.attrs)?);
        field_tys.push(field.ty.clone());
    }

    let wave_type_fields = wit_names.iter().zip(&field_tys).map(|(wit, ty)| {
        quote! { (#wit, <#ty as ::splicer_tool_sdk::WitTyped>::wave_type()) }
    });
    let to_value_fields = wit_names
        .iter()
        .zip(&field_tys)
        .zip(&field_idents)
        .map(|((wit, ty), ident)| {
            quote! { (#wit, <#ty as ::splicer_tool_sdk::WitTyped>::to_value(&self.#ident)) }
        });
    let from_value_inits = field_idents
        .iter()
        .map(|ident| quote! { let mut #ident = ::core::option::Option::None; });
    let from_value_arms = wit_names
        .iter()
        .zip(&field_tys)
        .zip(&field_idents)
        .map(|((wit, ty), ident)| {
            quote! {
                #wit => #ident = ::core::option::Option::Some(
                    <#ty as ::splicer_tool_sdk::WitTyped>::from_value(&v)?
                ),
            }
        });
    let from_value_constructors = wit_names.iter().zip(&field_idents).map(|(wit, ident)| {
        quote! {
            #ident: #ident.ok_or_else(|| ::splicer_tool_sdk::BridgeError::MissingField {
                name: #wit.into(),
            })?,
        }
    });

    let wave_type_body = quote! {
        ::splicer_tool_sdk::wasm_wave::value::Type::record([
            #(#wave_type_fields),*
        ]).expect("WIT record has at least one field")
    };
    let to_value_body = quote! {
        ::splicer_tool_sdk::wasm_wave::value::Value::make_record(
            &Self::wave_type(),
            [#(#to_value_fields),*],
        ).expect("emitted field values match the declared record type")
    };
    let from_value_body = quote! {
        #(#from_value_inits)*
        for (name, v) in value.unwrap_record() {
            match &*name {
                #(#from_value_arms)*
                other => return ::core::result::Result::Err(
                    ::splicer_tool_sdk::BridgeError::UnknownCase {
                        type_kind: ::splicer_tool_sdk::wasm_wave::wasm::WasmTypeKind::Record,
                        case: other.to_string(),
                    }
                ),
            }
        }
        ::core::result::Result::Ok(Self {
            #(#from_value_constructors)*
        })
    };
    Ok(wit_typed_impl(
        input,
        wave_type_body,
        to_value_body,
        from_value_body,
    ))
}

/// The single payload type of a variant case, or `None` for a unit
/// case. Errors on shapes a WIT variant case cannot express (named
/// fields, or more than one positional field).
fn variant_payload(variant: &Variant) -> syn::Result<Option<syn::Type>> {
    match &variant.fields {
        Fields::Unit => Ok(None),
        Fields::Unnamed(unnamed) if unnamed.unnamed.len() == 1 => {
            Ok(Some(unnamed.unnamed[0].ty.clone()))
        }
        Fields::Unnamed(_) => Err(syn::Error::new_spanned(
            variant,
            "a WIT variant case carries at most one payload type; use a single-field \
             tuple variant `Case(T)` or wrap the fields in a record type",
        )),
        Fields::Named(_) => Err(syn::Error::new_spanned(
            variant,
            "WIT variant cases cannot have named fields; use a single-field tuple \
             variant `Case(T)` referencing a named record type",
        )),
    }
}

fn expand_enum(
    input: &DeriveInput,
    variants: &syn::punctuated::Punctuated<Variant, syn::Token![,]>,
) -> syn::Result<TokenStream2> {
    if variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "WitTyped cannot be derived for an empty enum; a WIT enum/variant has at \
             least one case",
        ));
    }

    // Collect each case's WIT name, Rust ident, and payload (if any).
    let mut cases = Vec::with_capacity(variants.len());
    for variant in variants {
        let wit = wit_name_for(&variant.ident, &variant.attrs)?;
        let payload = variant_payload(variant)?;
        cases.push((wit, variant.ident.clone(), payload));
    }

    let any_payload = cases.iter().any(|(_, _, p)| p.is_some());
    if any_payload {
        expand_variant(input, &cases)
    } else {
        expand_unit_enum(input, &cases)
    }
}

type Case = (String, syn::Ident, Option<syn::Type>);

fn expand_variant(input: &DeriveInput, cases: &[Case]) -> syn::Result<TokenStream2> {
    let type_name = input.ident.to_string();

    let wave_cases = cases.iter().map(|(wit, _, payload)| match payload {
        Some(ty) => quote! {
            (#wit, ::core::option::Option::Some(
                <#ty as ::splicer_tool_sdk::WitTyped>::wave_type()
            ))
        },
        None => quote! { (#wit, ::core::option::Option::None) },
    });
    let to_arms = cases.iter().map(|(wit, ident, payload)| match payload {
        Some(ty) => quote! {
            Self::#ident(v) => (#wit, ::core::option::Option::Some(
                <#ty as ::splicer_tool_sdk::WitTyped>::to_value(v)
            ))
        },
        None => quote! { Self::#ident => (#wit, ::core::option::Option::None) },
    });
    let from_arms = cases.iter().map(|(wit, ident, payload)| {
        let case_label = format!("{type_name}::{ident}");
        match payload {
            Some(ty) => quote! {
                #wit => {
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
                #wit => {
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

    let wave_type_body = quote! {
        ::splicer_tool_sdk::wasm_wave::value::Type::variant([
            #(#wave_cases),*
        ]).expect("WIT variant has at least one case")
    };
    let to_value_body = quote! {
        let (case, payload): (&str, ::core::option::Option<_>) = match self {
            #(#to_arms),*
        };
        ::splicer_tool_sdk::wasm_wave::value::Value::make_variant(
            &Self::wave_type(),
            case,
            payload,
        ).expect("emitted case is in the declared variant")
    };
    let from_value_body = quote! {
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
    };
    Ok(wit_typed_impl(
        input,
        wave_type_body,
        to_value_body,
        from_value_body,
    ))
}

fn expand_unit_enum(input: &DeriveInput, cases: &[Case]) -> syn::Result<TokenStream2> {
    let type_name = input.ident.to_string();

    let wave_cases = cases.iter().map(|(wit, _, _)| wit);
    let to_arms = cases
        .iter()
        .map(|(wit, ident, _)| quote! { Self::#ident => #wit });
    let from_arms = cases
        .iter()
        .map(|(wit, ident, _)| quote! { #wit => ::core::result::Result::Ok(Self::#ident) });

    let wave_type_body = quote! {
        ::splicer_tool_sdk::wasm_wave::value::Type::enum_ty([
            #(#wave_cases),*
        ]).expect("WIT enum has at least one case")
    };
    let to_value_body = quote! {
        let case: &str = match self { #(#to_arms),* };
        ::splicer_tool_sdk::wasm_wave::value::Value::make_enum(&Self::wave_type(), case)
            .expect("emitted case is in the declared enum")
    };
    let from_value_body = quote! {
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
    };
    Ok(wit_typed_impl(
        input,
        wave_type_body,
        to_value_body,
        from_value_body,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalize(ts: &TokenStream2) -> String {
        ts.to_string().split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn expand_str(input: TokenStream2) -> String {
        let di: DeriveInput = syn::parse2(input).expect("parse derive input");
        normalize(&expand(di).expect("expand succeeds"))
    }

    fn expand_err(input: TokenStream2) -> String {
        let di: DeriveInput = syn::parse2(input).expect("parse derive input");
        match expand(di) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn record_emits_kebab_field_names_and_snake_access() {
        let out = expand_str(quote! {
            struct Pet { pet_name: String, age_years: u32 }
        });
        assert!(out.contains("impl :: splicer_tool_sdk :: WitTyped for Pet"));
        assert!(out.contains("\"pet-name\""), "want kebab key: {out}");
        assert!(out.contains("\"age-years\""), "want kebab key: {out}");
        assert!(out.contains("self . pet_name"), "want snake access: {out}");
        assert!(out.contains("Type :: record"), "want record: {out}");
    }

    #[test]
    fn record_field_rename_overrides_kebab() {
        let out = expand_str(quote! {
            struct Cfg { #[wit(name = "x-ray")] xray: u32 }
        });
        assert!(out.contains("\"x-ray\""), "want override name: {out}");
        assert!(out.contains("self . xray"), "want field access: {out}");
    }

    #[test]
    fn empty_struct_uses_sentinel_record() {
        let out = expand_str(quote! { struct Empty {} });
        assert!(out.contains("Type :: record"), "want sentinel record: {out}");
        assert!(out.contains("\"unit\""), "want sentinel field: {out}");
        assert!(!out.contains("Type :: tuple"), "must not use tuple: {out}");
    }

    #[test]
    fn all_unit_enum_uses_enum_ty() {
        let out = expand_str(quote! {
            enum Color { Red, Green, Blue }
        });
        assert!(out.contains("Type :: enum_ty"), "want enum_ty: {out}");
        assert!(!out.contains("Type :: variant"), "must not be variant: {out}");
        for c in ["\"red\"", "\"green\"", "\"blue\""] {
            assert!(out.contains(c), "missing case {c}: {out}");
        }
    }

    #[test]
    fn enum_with_payload_uses_variant_and_kebab_cases() {
        let out = expand_str(quote! {
            enum Outcome { NotFound, Found(u32), Report(String) }
        });
        assert!(out.contains("Type :: variant"), "want variant: {out}");
        assert!(out.contains("\"not-found\""), "want kebab case: {out}");
        assert!(out.contains("Self :: NotFound"), "want pascal ident: {out}");
        assert!(
            out.contains("as :: splicer_tool_sdk :: WitTyped >"),
            "payload should delegate to WitTyped: {out}"
        );
    }

    #[test]
    fn variant_case_rename_overrides_kebab() {
        let out = expand_str(quote! {
            enum E { #[wit(name = "k-v")] KeyValue(u32), Other }
        });
        assert!(out.contains("\"k-v\""), "want override case name: {out}");
        assert!(out.contains("Self :: KeyValue"), "want pascal ident: {out}");
    }

    #[test]
    fn generic_param_gains_wittyped_bound() {
        let out = expand_str(quote! {
            struct Wrapper<T> { inner: T }
        });
        // The bound shows up on the impl generics.
        assert!(
            out.contains("T : :: splicer_tool_sdk :: WitTyped"),
            "want WitTyped bound on T: {out}"
        );
    }

    #[test]
    fn union_is_rejected() {
        let msg = expand_err(quote! {
            union U { a: u32, b: f32 }
        });
        assert!(msg.contains("union"), "want union rejection: {msg}");
    }

    #[test]
    fn tuple_struct_is_rejected() {
        let msg = expand_err(quote! { struct Meters(u32); });
        assert!(msg.contains("named fields"), "want named-field hint: {msg}");
    }

    #[test]
    fn multi_field_variant_is_rejected() {
        let msg = expand_err(quote! {
            enum E { Pair(u32, u32) }
        });
        assert!(
            msg.contains("at most one payload"),
            "want single-payload hint: {msg}"
        );
    }

    #[test]
    fn named_field_variant_is_rejected() {
        let msg = expand_err(quote! {
            enum E { Rec { x: u32 } }
        });
        assert!(
            msg.contains("named fields"),
            "want named-field rejection: {msg}"
        );
    }
}
