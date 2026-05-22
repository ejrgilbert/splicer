//! Emit `WitTyped` impls for user-declared and synthesized types in
//! the wrapper crate.
//!
//! WIT core types (primitives, `Vec<T>`, `Option<T>`, `Result<T, E>`,
//! tuples) have handwritten impls in the SDK. User-declared records,
//! enums, variants, and flags have a different Rust shape per WIT, so
//! their impls are generated per wrapper. The emitter dispatches on
//! [`NamedKind`] — one arm per shape — and delegates field/case
//! payload conversion to the SDK's core impls.
//!
//! # What this emits
//!
//! For a WIT `record point { x: u32, y: u32 }` declared in `ops`:
//!
//! ```ignore
//! impl ::splicer_tool_sdk::WitTyped for bindings::exports::pkg::ops::Point {
//!     fn wave_type() -> Type {
//!         Type::record([
//!             ("x", <u32 as WitTyped>::wave_type()),
//!             ("y", <u32 as WitTyped>::wave_type()),
//!         ]).expect(/* … */)
//!     }
//!     fn to_value(&self) -> Value { /* make_record with each field's to_value */ }
//!     fn from_value(v: &Value) -> Result<Self, BridgeError> {
//!         /* unwrap_record, match field names, construct Self */
//!     }
//! }
//! ```
//!
//! Variants emit `Type::variant(…)` with per-case payload conversion;
//! enums emit `Type::enum_ty(…)` with case-name matching; flags emit
//! `Type::flags(…)` with `bitflags!`-aware encode/decode.

use proc_macro2::TokenStream;
use quote::quote;

use super::ir::{EnumCase, FlagMember, NamedKind, NamedType, RecordField, VariantCase};

/// Emit one `WitTyped` impl block per [`NamedType`].
pub fn emit_wit_typed_impls(types: &[NamedType]) -> Vec<TokenStream> {
    types.iter().map(emit_one).collect()
}

fn emit_one(t: &NamedType) -> TokenStream {
    match &t.kind {
        NamedKind::Record { fields } => emit_record(t, fields),
        NamedKind::Variant { cases } => emit_variant(t, cases),
        NamedKind::Enum { cases } => emit_enum(t, cases),
        NamedKind::Flags { members } => emit_flags(t, members),
    }
}

fn emit_record(t: &NamedType, fields: &[RecordField]) -> TokenStream {
    let type_path = t.rust_path_tokens();

    // wasm-wave rejects both zero-field records and zero-element
    // tuples, so encode the unit shape as a single-field sentinel
    // record. Only reached for synthesized args structs — WIT
    // doesn't allow zero-field records.
    if fields.is_empty() {
        return quote! {
            impl ::splicer_tool_sdk::WitTyped for #type_path {
                fn wave_type() -> ::splicer_tool_sdk::wasm_wave::value::Type {
                    ::splicer_tool_sdk::wasm_wave::value::Type::record([
                        ("unit", ::splicer_tool_sdk::wasm_wave::value::Type::BOOL),
                    ]).expect("single-field record is permitted")
                }
                fn to_value(&self) -> ::splicer_tool_sdk::wasm_wave::value::Value {
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
        };
    }

    let wave_type_fields = fields.iter().map(|f| {
        let wit = &f.wit_name;
        let ty = f.ty.to_tokens();
        quote! { (#wit, <#ty as ::splicer_tool_sdk::WitTyped>::wave_type()) }
    });
    let to_value_fields = fields.iter().map(|f| {
        let wit = &f.wit_name;
        let ty = f.ty.to_tokens();
        let ident = &f.rust_ident;
        quote! { (#wit, <#ty as ::splicer_tool_sdk::WitTyped>::to_value(&self.#ident)) }
    });
    let from_value_inits = fields.iter().map(|f| {
        let ident = &f.rust_ident;
        quote! { let mut #ident = ::core::option::Option::None; }
    });
    let from_value_arms = fields.iter().map(|f| {
        let wit = &f.wit_name;
        let ident = &f.rust_ident;
        let ty = f.ty.to_tokens();
        quote! {
            #wit => #ident = ::core::option::Option::Some(
                <#ty as ::splicer_tool_sdk::WitTyped>::from_value(&v)?
            ),
        }
    });
    let from_value_constructors = fields.iter().map(|f| {
        let wit = &f.wit_name;
        let ident = &f.rust_ident;
        quote! {
            #ident: #ident.ok_or_else(|| ::splicer_tool_sdk::BridgeError::MissingField {
                name: #wit.into(),
            })?,
        }
    });

    quote! {
        impl ::splicer_tool_sdk::WitTyped for #type_path {
            fn wave_type() -> ::splicer_tool_sdk::wasm_wave::value::Type {
                ::splicer_tool_sdk::wasm_wave::value::Type::record([
                    #(#wave_type_fields),*
                ]).expect("WIT record has at least one field")
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
            }
        }
    }
}

fn emit_variant(t: &NamedType, cases: &[VariantCase]) -> TokenStream {
    let type_path = t.rust_path_tokens();
    let type_name = t.rust_ident.to_string();

    let wave_cases = cases.iter().map(|c| {
        let wit = &c.wit_name;
        match &c.payload {
            Some(p) => {
                let ty = p.to_tokens();
                quote! {
                    (#wit, ::core::option::Option::Some(
                        <#ty as ::splicer_tool_sdk::WitTyped>::wave_type()
                    ))
                }
            }
            None => quote! { (#wit, ::core::option::Option::None) },
        }
    });
    let to_arms = cases.iter().map(|c| {
        let wit = &c.wit_name;
        let ident = &c.rust_ident;
        match &c.payload {
            Some(p) => {
                let ty = p.to_tokens();
                quote! {
                    Self::#ident(v) => (#wit, ::core::option::Option::Some(
                        <#ty as ::splicer_tool_sdk::WitTyped>::to_value(v)
                    ))
                }
            }
            None => quote! { Self::#ident => (#wit, ::core::option::Option::None) },
        }
    });
    let from_arms = cases.iter().map(|c| {
        let wit = &c.wit_name;
        let ident = &c.rust_ident;
        let case_label = format!("{type_name}::{ident}");
        match &c.payload {
            Some(p) => {
                let ty = p.to_tokens();
                quote! {
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
                }
            }
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

    quote! {
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
    }
}

fn emit_enum(t: &NamedType, cases: &[EnumCase]) -> TokenStream {
    let type_path = t.rust_path_tokens();
    let type_name = t.rust_ident.to_string();

    let case_names = cases.iter().map(|c| &c.wit_name);
    let to_arms = cases.iter().map(|c| {
        let wit = &c.wit_name;
        let ident = &c.rust_ident;
        quote! { Self::#ident => #wit }
    });
    let from_arms = cases.iter().map(|c| {
        let wit = &c.wit_name;
        let ident = &c.rust_ident;
        quote! { #wit => ::core::result::Result::Ok(Self::#ident) }
    });

    quote! {
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
    }
}

fn emit_flags(t: &NamedType, members: &[FlagMember]) -> TokenStream {
    let type_path = t.rust_path_tokens();

    let wit_names = members.iter().map(|m| &m.wit_name);
    let to_value_checks = members.iter().map(|m| {
        let wit = &m.wit_name;
        let ident = &m.rust_ident;
        quote! {
            if self.contains(Self::#ident) {
                names.push(#wit);
            }
        }
    });
    let from_value_arms = members.iter().map(|m| {
        let wit = &m.wit_name;
        let ident = &m.rust_ident;
        quote! {
            #wit => result |= Self::#ident,
        }
    });

    quote! {
        impl ::splicer_tool_sdk::WitTyped for #type_path {
            fn wave_type() -> ::splicer_tool_sdk::wasm_wave::value::Type {
                ::splicer_tool_sdk::wasm_wave::value::Type::flags([
                    #(#wit_names),*
                ]).expect("WIT flags has at least one member")
            }

            fn to_value(&self) -> ::splicer_tool_sdk::wasm_wave::value::Value {
                let ty = Self::wave_type();
                let mut names: ::std::vec::Vec<&'static str> = ::std::vec::Vec::new();
                #(#to_value_checks)*
                ::splicer_tool_sdk::wasm_wave::value::Value::make_flags(&ty, names)
                    .expect("emitted flag names are in the declared flags type")
            }

            fn from_value(
                value: &::splicer_tool_sdk::wasm_wave::value::Value,
            ) -> ::core::result::Result<Self, ::splicer_tool_sdk::BridgeError> {
                let mut result = Self::empty();
                for name in value.unwrap_flags() {
                    match &*name {
                        #(#from_value_arms)*
                        other => return ::core::result::Result::Err(
                            ::splicer_tool_sdk::BridgeError::UnknownCase {
                                type_kind: ::splicer_tool_sdk::wasm_wave::wasm::WasmTypeKind::Flags,
                                case: other.to_string(),
                            }
                        ),
                    }
                }
                ::core::result::Result::Ok(result)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::typed::bindgen::run_wit_bindgen_rust;
    use crate::adapter::typed::bindings_index::build_bindings_index;
    use crate::adapter::typed::ir::{build_ir, WrapperIR};

    fn normalize(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn build_ir_for(wit: &str) -> WrapperIR {
        let (resolve, world_id, src) = run_wit_bindgen_rust(wit, Some("w")).unwrap();
        let bindings = build_bindings_index(&src).unwrap();
        build_ir(&resolve, world_id, &bindings).unwrap()
    }

    fn impls_str(types: &[NamedType]) -> String {
        let impls = emit_wit_typed_impls(types);
        normalize(&impls.iter().map(|t| t.to_string()).collect::<String>())
    }

    fn user_impls_str(wit: &str) -> String {
        impls_str(&build_ir_for(wit).types)
    }

    #[test]
    fn emits_impl_for_simple_record() {
        let combined = user_impls_str(
            r#"
                package test:pkg@0.1.0;
                interface ops {
                    record point { x: u32, y: u32 }
                    add: func(p: point) -> u32;
                }
                world w { export ops; }
            "#,
        );
        assert!(
            combined.contains("impl :: splicer_tool_sdk :: WitTyped for"),
            "expected an impl block; got: {combined}"
        );
        assert!(combined.contains("\"x\""), "missing field `x`: {combined}");
        assert!(combined.contains("\"y\""), "missing field `y`: {combined}");
        assert!(
            combined.contains("bindings ::"),
            "type path should be `bindings::...`: {combined}"
        );
    }

    #[test]
    fn emits_kebab_case_for_snake_case_field_idents() {
        let combined = user_impls_str(
            r#"
                package test:pkg@0.1.0;
                interface ops {
                    record pet { pet-name: string, age-years: u32 }
                    set: func(p: pet);
                }
                world w { export ops; }
            "#,
        );
        // Rust field idents come out snake_case (`pet_name`); the wave
        // type and field-key strings emit kebab-case.
        assert!(
            combined.contains("\"pet-name\""),
            "want kebab key: {combined}"
        );
        assert!(
            combined.contains("\"age-years\""),
            "want kebab key: {combined}"
        );
        assert!(
            combined.contains("self . pet_name"),
            "self.<snake_case> access expected: {combined}"
        );
    }

    #[test]
    fn emits_impl_for_unit_only_enum() {
        let combined = user_impls_str(
            r#"
                package test:pkg@0.1.0;
                interface ops {
                    enum color { red, green, blue }
                    tag: func(c: color);
                }
                world w { export ops; }
            "#,
        );
        assert!(
            combined.contains("Type :: enum_ty"),
            "unit-only enum should use Type::enum_ty: {combined}"
        );
        for name in ["\"red\"", "\"green\"", "\"blue\""] {
            assert!(combined.contains(name), "missing case {name}: {combined}");
        }
        assert!(
            !combined.contains("Type :: variant"),
            "unit-only enum should not use Type::variant: {combined}"
        );
    }

    #[test]
    fn emits_impl_for_variant_with_payloads() {
        let combined = user_impls_str(
            r#"
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
            "#,
        );
        assert!(
            combined.contains("Type :: variant"),
            "variant should use Type::variant: {combined}"
        );
        for name in ["\"hit\"", "\"miss\"", "\"close\""] {
            assert!(combined.contains(name), "missing case {name}: {combined}");
        }
        assert!(
            combined.contains("as :: splicer_tool_sdk :: WitTyped >"),
            "payload conversion should delegate to WitTyped: {combined}"
        );
    }

    #[test]
    fn variant_with_dashed_case_name_round_trips_kebab() {
        let combined = user_impls_str(
            r#"
                package test:pkg@0.1.0;
                interface ops {
                    variant outcome {
                        not-found,
                        found(u32),
                    }
                    lookup: func() -> outcome;
                }
                world w { export ops; }
            "#,
        );
        assert!(
            combined.contains("\"not-found\""),
            "expected kebab-case case name: {combined}"
        );
        assert!(
            combined.contains("Self :: NotFound"),
            "expected PascalCase Rust variant ident: {combined}"
        );
    }

    #[test]
    fn zero_arg_args_record_uses_sentinel_record_not_empty_tuple() {
        // wasm-wave returns None for empty records and empty tuples,
        // so the zero-arg branch must use a sentinel-field record;
        // emitting `Type::tuple([])` would panic at runtime.
        let ir = build_ir_for(
            r#"
                package test:pkg@0.1.0;
                interface ops {
                    noop: func();
                }
                world w { export ops; }
            "#,
        );
        let combined = impls_str(&ir.args_records);
        assert!(
            combined.contains("Type :: record"),
            "zero-arg args impl must use Type::record (sentinel): {combined}"
        );
        assert!(
            !combined.contains("Type :: tuple ([])"),
            "zero-arg args impl must not use Type::tuple([]) (panics at runtime): {combined}"
        );
    }

    #[test]
    fn emits_impl_for_flags() {
        let combined = user_impls_str(
            r#"
                package test:pkg@0.1.0;
                interface ops {
                    flags perms { read, write, exec-x }
                    check: func(p: perms);
                }
                world w { export ops; }
            "#,
        );
        assert!(
            combined.contains("Type :: flags"),
            "flags should use Type::flags: {combined}"
        );
        // Member idents inside the body are SHOUTING_SNAKE_CASE.
        assert!(
            combined.contains("Self :: READ"),
            "READ member missing: {combined}"
        );
        assert!(
            combined.contains("Self :: EXEC_X"),
            "EXEC_X member missing: {combined}"
        );
        // Wave-side flag names stay kebab-case.
        assert!(
            combined.contains("\"exec-x\""),
            "kebab flag name missing: {combined}"
        );
    }
}
