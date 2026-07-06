//! Assemble the per-piece emissions into the two files that make up
//! a wrapper crate: `src/lib.rs` and `Cargo.toml`. Output is plain
//! source strings ready to write to disk; running `cargo build` on
//! the resulting project is a downstream concern.
//!
//! # What this emits
//!
//! The `lib.rs` for a Transform-strategy wrapper looks roughly like:
//!
//! ```ignore
//! mod bindings { /* wit-bindgen-rust output, verbatim */ }
//!
//! use ::splicer_tool_sdk::{CallId, WitTyped};
//! use ::splicer_tool_sdk::wasm_wave::wasm::WasmValue;
//! use ::splicer_tool_sdk::TransformStrategy;
//!
//! pub struct OpsAddArgs { /* … */ }
//! impl ::splicer_tool_sdk::WitTyped for OpsAddArgs { /* … */ }
//! impl ::splicer_tool_sdk::WitTyped for bindings::exports::pkg::ops::Point { /* … */ }
//! // one impl per user-declared type + per synthesized args record
//!
//! static STRATEGY: ::std::sync::OnceLock<my_strategy::MyStrategy> =
//!     ::std::sync::OnceLock::new();
//! fn strategy() -> &'static my_strategy::MyStrategy { /* OnceLock get-or-init */ }
//!
//! struct Wrapper;
//! impl bindings::exports::pkg::ops::Guest for Wrapper { /* method bodies */ }
//!
//! bindings::export!(Wrapper with_types_in bindings);
//! ```
//!
//! Virtualize emits `VirtualizeStrategy` instead of `TransformStrategy`.

use anyhow::{Context, Result};
use proc_macro2::TokenStream;
use quote::quote;

use super::emit_method::EmittedGuest;
use super::Behavior;

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
    /// Per-resource wrapper newtypes
    pub resource_newtypes: &'a [TokenStream],
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
    let resource_newtypes = inputs.resource_newtypes;
    let args_structs: Vec<&TokenStream> = inputs
        .guests
        .iter()
        .flat_map(|g| g.args_structs.iter())
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

        // Args record decls (top-level structs), then `WitTyped`
        // impls for user types and args records alike. Resource
        // wrapper newtypes (Wrapper<R>) live alongside and back the
        // export-side resource table via the impl<GuestR for WrapperR>
        // blocks emitted below.
        #(#args_structs)*
        #(#resource_newtypes)*
        #(#witty_impls)*

        // Strategy storage + `strategy()` accessor.
        ::splicer_tool_sdk::define_strategy_singleton!(
            #strategy_crate_ident::#strategy_type_ident
        );

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
    /// `splicer-tool-sdk` version (from the strategy crate's own
    /// Cargo.toml) that the wrapper depends on. Must match what the
    /// strategy itself declares so cargo dedupes the two into one
    /// source.
    pub splicer_tool_sdk_version: &'a str,
}

/// Assemble the wrapper crate's Cargo.toml. Serialized through the
/// `toml` crate so paths with special chars (esp. Windows backslashes)
/// get escaped correctly instead of breaking the TOML.
pub fn assemble_cargo_toml(inputs: &CargoTomlInputs<'_>) -> String {
    use toml::map::Map;
    use toml::Value;

    let mut package = Map::new();
    package.insert("name".into(), Value::String(inputs.crate_name.into()));
    package.insert("version".into(), Value::String("0.1.0".into()));
    package.insert("edition".into(), Value::String("2021".into()));
    package.insert("publish".into(), Value::Boolean(false));

    let mut lib = Map::new();
    lib.insert(
        "crate-type".into(),
        Value::Array(vec![Value::String("cdylib".into())]),
    );

    let mut strategy_dep = Map::new();
    strategy_dep.insert(
        "path".into(),
        Value::String(inputs.strategy_crate_path.into()),
    );

    let mut dependencies = Map::new();
    dependencies.insert(
        "splicer-tool-sdk".into(),
        Value::String(inputs.splicer_tool_sdk_version.into()),
    );
    dependencies.insert(
        inputs.strategy_crate_name.into(),
        Value::Table(strategy_dep),
    );
    dependencies.insert("wit-bindgen".into(), Value::String("0.57".into()));

    let mut root = Map::new();
    root.insert("package".into(), Value::Table(package));
    root.insert("lib".into(), Value::Table(lib));
    root.insert("dependencies".into(), Value::Table(dependencies));

    if let Some(local_sdk) = local_sdk_path() {
        let mut sdk_patch = Map::new();
        sdk_patch.insert("path".into(), Value::String(local_sdk));
        let mut crates_io = Map::new();
        crates_io.insert("splicer-tool-sdk".into(), Value::Table(sdk_patch));
        let mut patch = Map::new();
        patch.insert("crates-io".into(), Value::Table(crates_io));
        root.insert("patch".into(), Value::Table(patch));
    }

    toml::to_string(&Value::Table(root))
        .expect("toml serialization of strings + booleans + tables is infallible")
}

/// Path the wrapper should patch `splicer-tool-sdk` to, or `None` for
/// crates.io resolution. Three-state precedence:
///
/// - `SPLICER_TOOL_SDK_PATH=""` → opt out (registry, even from workspace builds).
/// - `SPLICER_TOOL_SDK_PATH=/some/path` → explicit override.
/// - Unset + splicer compiled from a workspace with a sibling
///   `splicer-tool-sdk/` → that sibling. Otherwise (crates.io
///   installs) → registry.
pub(crate) fn local_sdk_path() -> Option<String> {
    const WORKSPACE_SDK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/splicer-tool-sdk");
    match std::env::var("SPLICER_TOOL_SDK_PATH").as_deref() {
        Ok("") => None,
        Ok(path) => Some(path.to_string()),
        Err(_) => std::path::Path::new(WORKSPACE_SDK)
            .join("Cargo.toml")
            .exists()
            .then(|| WORKSPACE_SDK.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::typed::bindgen::run_wit_bindgen_rust;
    use crate::adapter::typed::bindings_index::build_bindings_index;
    use crate::adapter::typed::emit_method::emit_guest;
    use crate::adapter::typed::emit_wit_typed::emit_wit_typed_impls;
    use crate::adapter::typed::ir::build_ir;

    const INTERFACE_QN: &str = "test:pkg/ops@0.1.0";

    const TINY_WIT: &str = r#"
        package test:pkg@0.1.0;
        interface ops {
            add: func(a: u32, b: u32) -> u32;
        }
        world w { export ops; }
    "#;

    fn assemble_for_wit(wit: &str, behavior: Behavior) -> String {
        let (resolve, world_id, bindings_src) = run_wit_bindgen_rust(wit, Some("w")).unwrap();
        let bindings = build_bindings_index(&bindings_src).unwrap();
        let ir = build_ir(&resolve, world_id, &bindings, INTERFACE_QN).unwrap();
        let user_impls = emit_wit_typed_impls(&ir.types);
        let args_impls = emit_wit_typed_impls(&ir.args_records);
        let witty_impls: Vec<_> = user_impls.into_iter().chain(args_impls).collect();
        let guests: Vec<EmittedGuest> = bindings
            .guest_traits
            .iter()
            .map(|g| emit_guest(g, INTERFACE_QN, behavior, &ir))
            .collect();

        let inputs = WrapperCrateInputs {
            bindings_src: &bindings_src,
            witty_impls: &witty_impls,
            guests: &guests,
            resource_newtypes: &[],
            behavior,
            strategy_crate_name: "my-strategy",
            strategy_type: "MyStrategy",
        };
        assemble_lib_rs(&inputs).expect("assembly succeeds")
    }

    #[test]
    fn assembled_lib_rs_has_required_pieces() {
        let out = assemble_for_wit(TINY_WIT, Behavior::Transform);

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
        // Strategy storage goes through the SDK macro; codegen
        // emits the invocation with the snake-cased strategy path.
        assert!(
            out.contains("define_strategy_singleton!(my_strategy::MyStrategy)")
                || out.contains("define_strategy_singleton ! (my_strategy :: MyStrategy)"),
            "expected define_strategy_singleton! invocation:\n{out}"
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
        let out = assemble_for_wit(TINY_WIT, Behavior::Virtualize);
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
            splicer_tool_sdk_version: crate::test_consts::SDK_TEST_VERSION,
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
        assert_eq!(
            parsed["dependencies"]["splicer-tool-sdk"].as_str(),
            Some(crate::test_consts::SDK_TEST_VERSION),
            "splicer-tool-sdk must be a plain registry version dep so cargo dedupes it with \
             the strategy crate's own splicer-tool-sdk dep",
        );
        assert!(parsed["dependencies"].get("my-strategy").is_some());
        assert!(parsed["dependencies"].get("wit-bindgen").is_some());
    }

    #[test]
    fn cargo_toml_escapes_paths_with_special_chars() {
        // Windows-style backslashes + a path containing a `"` that
        // would close the TOML string literal under naive `format!`.
        let toml = assemble_cargo_toml(&CargoTomlInputs {
            crate_name: "wrapper",
            strategy_crate_name: "strat",
            strategy_crate_path: r#"C:\Users\me\strat-"with-quote""#,
            splicer_tool_sdk_version: crate::test_consts::SDK_TEST_VERSION,
        });
        let parsed: toml::Value =
            toml::from_str(&toml).expect("Cargo.toml with special-char paths still parses");
        assert_eq!(
            parsed["dependencies"]["strat"]["path"].as_str(),
            Some(r#"C:\Users\me\strat-"with-quote""#),
        );
    }
}
