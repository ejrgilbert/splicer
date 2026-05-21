//! Assemble the per-piece emissions into the two files that make up
//! a wrapper crate: `src/lib.rs` and `Cargo.toml`. Output is plain
//! source strings ready to write to disk; running `cargo build` on
//! the resulting project is a downstream concern.

use anyhow::{Context, Result};
use proc_macro2::TokenStream;
use quote::quote;

use super::behavior_meta::Behavior;
use super::emit_method::EmittedGuest;

/// Inputs to [`assemble_lib_rs`]. Each piece comes from one of the
/// emitter modules; this assembler just stitches them together with
/// the surrounding scaffolding.
pub struct WrapperCrateInputs<'a> {
    /// Raw bindings source produced by
    /// [`super::bindgen::run_wit_bindgen_rust`].
    pub bindings_src: &'a str,
    /// Per-user-type `WitTyped` impls from
    /// [`super::emit_wit_typed::emit_wit_typed_impls`].
    pub witty_impls: &'a [TokenStream],
    /// Per-Guest emissions from [`super::emit_method::emit_guest`].
    pub guests: &'a [EmittedGuest],
    pub behavior: Behavior,
    /// The strategy crate's Cargo package name (kebab- or snake-case).
    pub strategy_crate_name: &'a str,
    /// The PascalCase Rust ident of the strategy type to instantiate.
    pub strategy_type: &'a str,
}

/// Assemble a complete `lib.rs` source string for the wrapper crate.
pub fn assemble_lib_rs(inputs: &WrapperCrateInputs<'_>) -> Result<String> {
    let bindings_file =
        syn::parse_file(inputs.bindings_src).context("could not parse bindings source as Rust")?;
    let bindings_items = &bindings_file.items;

    let strategy_crate_ident = syn::Ident::new(
        &inputs.strategy_crate_name.replace('-', "_"),
        proc_macro2::Span::call_site(),
    );
    let strategy_type_ident = syn::Ident::new(inputs.strategy_type, proc_macro2::Span::call_site());

    // The behavior-specific use exposes whichever strategy trait the
    // emitted method bodies dispatch through.
    let strategy_trait_use = match inputs.behavior {
        Behavior::Transform => quote!(
            use ::splicer_tool_sdk::TransformStrategy;
        ),
        Behavior::Virtualize => quote!(
            use ::splicer_tool_sdk::VirtualizeStrategy;
        ),
    };

    let witty_impls = inputs.witty_impls;
    let args_structs: Vec<&TokenStream> = inputs
        .guests
        .iter()
        .flat_map(|g| g.args_structs.iter())
        .collect();
    let args_witty_impls: Vec<&TokenStream> = inputs
        .guests
        .iter()
        .flat_map(|g| g.args_witty_impls.iter())
        .collect();
    let guest_impls: Vec<&TokenStream> = inputs.guests.iter().map(|g| &g.guest_impl).collect();

    let assembled = quote! {
        // wit-bindgen output, inlined verbatim under `mod bindings`.
        // Wrapping in this module is what gives the rest of the
        // emissions their `bindings::...` prefix consistency.
        mod bindings {
            #(#bindings_items)*
        }

        use ::splicer_tool_sdk::{CallId, WitTyped};
        // `WasmValue` (a trait) needs to be in scope so emitted
        // `Value::make_record(...)` / `value.unwrap_record()` calls
        // resolve to the trait methods rather than a (non-existent)
        // inherent impl on `wasm_wave::value::Value`.
        use ::splicer_tool_sdk::wasm_wave::wasm::WasmValue;
        #strategy_trait_use

        // Per-user-type WitTyped impls (records, enums, variants).
        #(#witty_impls)*

        // Per-method args structs and their WitTyped impls.
        #(#args_structs)*
        #(#args_witty_impls)*

        // One shared strategy instance for the whole wrapper. Stored
        // as `OnceLock<S>` so the strategy reference has `'static`
        // lifetime — avoids the borrow-across-`.await` lifetime
        // conflict that `thread_local! { RefCell<S> }` would trip.
        // The strategy type must impl `Default` (for lazy init) and
        // `Sync` (for static storage); strategies needing interior
        // mutability can wrap their state in atomic / lock primitives.
        static STRATEGY: ::std::sync::OnceLock<#strategy_crate_ident::#strategy_type_ident> =
            ::std::sync::OnceLock::new();

        fn strategy() -> &'static #strategy_crate_ident::#strategy_type_ident {
            STRATEGY.get_or_init(
                <#strategy_crate_ident::#strategy_type_ident as ::core::default::Default>::default
            )
        }

        // The user-facing Guest impl target. Each `impl Guest for Wrapper`
        // block (one per exported interface) dispatches into STRATEGY.
        struct Wrapper;

        #(#guest_impls)*

        bindings::export!(Wrapper with_types_in bindings);
    };

    // Round-trip through syn so prettyplease can format the output.
    let parsed = syn::parse2::<syn::File>(assembled)
        .context("assembled wrapper lib.rs is not parseable Rust")?;
    Ok(prettyplease::unparse(&parsed))
}

/// Inputs to [`assemble_cargo_toml`].
pub struct CargoTomlInputs<'a> {
    pub crate_name: &'a str,
    pub strategy_crate_name: &'a str,
    pub strategy_crate_path: &'a str,
    pub splicer_tool_sdk_path: &'a str,
}

/// Assemble the wrapper crate's Cargo.toml.
pub fn assemble_cargo_toml(inputs: &CargoTomlInputs<'_>) -> String {
    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
splicer-tool-sdk = {{ path = "{sdk}" }}
"{strategy}" = {{ path = "{strategy_path}" }}
wit-bindgen = "0.57"
"#,
        name = inputs.crate_name,
        sdk = inputs.splicer_tool_sdk_path,
        strategy = inputs.strategy_crate_name,
        strategy_path = inputs.strategy_crate_path,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::typed::bindgen::run_wit_bindgen_rust;
    use crate::adapter::typed::bindings_walk::walk_bindings;
    use crate::adapter::typed::emit_method::emit_guest;
    use crate::adapter::typed::emit_wit_typed::emit_wit_typed_impls;

    const TINY_WIT: &str = r#"
        package test:pkg@0.1.0;
        interface ops {
            add: func(a: u32, b: u32) -> u32;
        }
        world w { export ops; }
    "#;

    fn assemble_for_tiny(behavior: Behavior) -> String {
        let bindings_src = run_wit_bindgen_rust(TINY_WIT, Some("w")).unwrap();
        let bindings = walk_bindings(&bindings_src).unwrap();
        let witty_impls = emit_wit_typed_impls(&bindings.types);
        let guests: Vec<EmittedGuest> = bindings
            .guest_traits
            .iter()
            .map(|g| emit_guest(g, "test:pkg/ops@0.1.0", behavior))
            .collect();

        let inputs = WrapperCrateInputs {
            bindings_src: &bindings_src,
            witty_impls: &witty_impls,
            guests: &guests,
            behavior,
            strategy_crate_name: "my-strategy",
            strategy_type: "MyStrategy",
        };
        assemble_lib_rs(&inputs).expect("assembly succeeds")
    }

    #[test]
    fn assembled_lib_rs_has_required_pieces() {
        let out = assemble_for_tiny(Behavior::Transform);

        // The bindings get wrapped in `mod bindings`.
        assert!(
            out.contains("mod bindings"),
            "expected `mod bindings`:\n{out}"
        );
        // Shared use statements.
        assert!(
            out.contains("use ::splicer_tool_sdk::TransformStrategy"),
            "forward dispatch expects TransformStrategy use:\n{out}"
        );
        // The strategy is stored in a OnceLock<S> so the `&S`
        // handed to handle() has `'static` lifetime (avoids
        // borrow-across-await with thread_local!{RefCell<S>}).
        assert!(
            out.contains("OnceLock"),
            "expected OnceLock storage:\n{out}"
        );
        assert!(
            out.contains("my_strategy :: MyStrategy") || out.contains("my_strategy::MyStrategy"),
            "expected snake-cased strategy path:\n{out}"
        );
        // The wrapper struct + Guest impl.
        assert!(out.contains("struct Wrapper"), "missing Wrapper:\n{out}");
        assert!(
            out.contains("bindings::export!"),
            "expected bindings::export! line:\n{out}"
        );
    }

    #[test]
    fn virtualize_behavior_imports_virtualize_strategy() {
        let out = assemble_for_tiny(Behavior::Virtualize);
        assert!(
            out.contains("use ::splicer_tool_sdk::VirtualizeStrategy"),
            "virtualize dispatch expects VirtualizeStrategy use:\n{out}"
        );
        assert!(
            !out.contains("use ::splicer_tool_sdk::TransformStrategy"),
            "virtualize emission should not import TransformStrategy:\n{out}"
        );
    }

    #[test]
    fn cargo_toml_lists_required_deps() {
        let toml = assemble_cargo_toml(&CargoTomlInputs {
            crate_name: "splicer_wrapper_test_pkg_ops_my_strategy",
            strategy_crate_name: "my-strategy",
            strategy_crate_path: "/abs/path/to/my-strategy",
            splicer_tool_sdk_path: "/abs/path/to/splicer-tool-sdk",
        });
        // Parse the result as TOML to catch syntax errors.
        let parsed: toml::Value = toml::from_str(&toml).expect("Cargo.toml parses");
        assert_eq!(
            parsed["package"]["name"].as_str(),
            Some("splicer_wrapper_test_pkg_ops_my_strategy")
        );
        assert_eq!(
            parsed["lib"]["crate-type"]
                .as_array()
                .unwrap()
                .first()
                .and_then(|v| v.as_str()),
            Some("cdylib")
        );
        assert!(parsed["dependencies"].get("splicer-tool-sdk").is_some());
        assert!(parsed["dependencies"].get("my-strategy").is_some());
        assert!(parsed["dependencies"].get("wit-bindgen").is_some());
    }
}
