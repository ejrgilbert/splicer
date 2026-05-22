//! Per-target wrapper-component source codegen for forward
//! (transform) and virtualize strategies. Sibling to the
//! bytecode-emitting [`super::tier1`] and [`super::tier2`]
//! adapters; emits Rust source that downstream stages compile to
//! wasm via `cargo build`.

mod assemble;
mod behavior_meta;
mod bindgen;
mod bindings_index;
mod build;
mod emit_method;
mod emit_wit_typed;
mod ir;
pub(crate) mod target_wit;

pub use assemble::{assemble_cargo_toml, assemble_lib_rs, CargoTomlInputs, WrapperCrateInputs};
pub use behavior_meta::Behavior;
pub use bindgen::run_wit_bindgen_rust;
pub use bindings_index::build_bindings_index;
pub use build::{build_wrapper, BuildConfig};
pub use emit_method::{emit_guest, EmittedGuest};
pub use emit_wit_typed::emit_wit_typed_impls;
#[allow(unused_imports)]
pub use ir::{build_ir, NamedKind, NamedType, WitTypeRef, WrapperIR};
pub use target_wit::{target_wit_for_codegen, TargetWit};

use anyhow::Result;

/// One-call orchestrator: take a target WIT and a strategy reference,
/// produce the full source of a wrapper crate that compiles to a
/// wasm component.
pub fn generate_wrapper_crate(input: &GenerateWrapperInput<'_>) -> Result<WrapperCrate> {
    // Two complementary views of the same WIT: the Resolve walk drives
    // the IR (what kind of WIT thing is this — record, flags, …); the
    // syn walk indexes wit-bindgen's emitted shapes. The IR consults
    // the index per type to confirm wit-bindgen produced the expected
    // Rust shape, so the walks aren't independent, but they capture
    // distinct slices of the same source.
    let (resolve, world_id, bindings_src) =
        run_wit_bindgen_rust(input.target_wit, input.world_name)?;
    let bindings = build_bindings_index(&bindings_src)?;
    let ir = build_ir(&resolve, world_id, &bindings)?;
    // User-declared types + per-method synthesized args records both
    // ride the same emitter via NamedKind dispatch.
    let user_impls = emit_wit_typed_impls(&ir.types);
    let args_impls = emit_wit_typed_impls(&ir.args_records);
    let witty_impls: Vec<_> = user_impls.into_iter().chain(args_impls).collect();
    let guests: Vec<EmittedGuest> = bindings
        .guest_traits
        .iter()
        .map(|g| emit_guest(g, input.interface_qualified_name, input.behavior, &ir))
        .collect();

    let lib_rs = assemble_lib_rs(&WrapperCrateInputs {
        bindings_src: &bindings_src,
        witty_impls: &witty_impls,
        guests: &guests,
        behavior: input.behavior,
        strategy_crate_name: input.strategy_crate_name,
        strategy_type: input.strategy_type,
    })?;

    let crate_name =
        make_wrapper_crate_name(input.interface_qualified_name, input.strategy_crate_name);
    let cargo_toml = assemble_cargo_toml(&CargoTomlInputs {
        crate_name: &crate_name,
        strategy_crate_name: input.strategy_crate_name,
        strategy_crate_path: input.strategy_crate_path,
        splicer_tool_sdk_path: input.splicer_tool_sdk_path,
    });

    Ok(WrapperCrate {
        crate_name,
        lib_rs,
        cargo_toml,
    })
}

/// Inputs to [`generate_wrapper_crate`].
pub struct GenerateWrapperInput<'a> {
    /// Raw WIT text for the wrapped target.
    pub target_wit: &'a str,
    /// World name to select inside the WIT, if there are multiple.
    pub world_name: Option<&'a str>,
    /// Qualified name of the wrapped interface, e.g.
    /// `"wasi:keyvalue/store@0.2.0"`. Goes into each emitted `CallId`.
    pub interface_qualified_name: &'a str,
    /// Transform (tier-3) or virtualize (tier-4).
    pub behavior: Behavior,
    /// The strategy's Cargo package name.
    pub strategy_crate_name: &'a str,
    /// Filesystem path to the strategy crate's directory.
    pub strategy_crate_path: &'a str,
    /// PascalCase Rust ident of the strategy type to instantiate.
    pub strategy_type: &'a str,
    /// Filesystem path to splicer-tool-sdk (for the wrapper's Cargo.toml dep).
    pub splicer_tool_sdk_path: &'a str,
}

/// Output of [`generate_wrapper_crate`]: the two source strings that
/// make up the wrapper crate, plus the crate name to use on disk.
pub struct WrapperCrate {
    pub crate_name: String,
    pub lib_rs: String,
    pub cargo_toml: String,
}

/// Sanitize `interface@version` + strategy name into a valid Cargo
/// package identifier: lowercase alphanumerics + underscores, plus a
/// short hash suffix of the original inputs so distinct interfaces
/// that sanitize identically (`wasi:http/handler@0.3.0` vs
/// `wasi-http-handler-0-3-0`) get distinct crate names. Without the
/// suffix both would collide on the shared `CARGO_TARGET_DIR`'s
/// `<crate>.wasm` output and the second build would clobber the
/// first.
fn make_wrapper_crate_name(interface: &str, strategy: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect()
    };
    let mut h = DefaultHasher::new();
    interface.hash(&mut h);
    strategy.hash(&mut h);
    let suffix = format!("{:08x}", h.finish() as u32);
    format!(
        "splicer_wrapper_{}_{}_{}",
        sanitize(interface),
        sanitize(strategy),
        suffix,
    )
}

#[cfg(test)]
mod bindgen_contract_tests;
#[cfg(test)]
mod matrix_tests;

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_WIT: &str = r#"
        package test:pkg@0.1.0;
        interface ops {
            add: async func(a: u32, b: u32) -> u32;
        }
        world w { export ops; }
    "#;

    fn input<'a>(behavior: Behavior) -> GenerateWrapperInput<'a> {
        GenerateWrapperInput {
            target_wit: TINY_WIT,
            world_name: Some("w"),
            interface_qualified_name: "test:pkg/ops@0.1.0",
            behavior,
            strategy_crate_name: "my-strategy",
            strategy_crate_path: "/abs/path/to/my-strategy",
            strategy_type: "MyStrategy",
            splicer_tool_sdk_path: "/abs/path/to/splicer-tool-sdk",
        }
    }

    #[test]
    fn transform_pipeline_produces_a_complete_wrapper_crate() {
        let crate_out = generate_wrapper_crate(&input(Behavior::Transform)).unwrap();
        // Crate name reflects both inputs and is a valid Cargo ident.
        assert!(
            crate_out.crate_name.starts_with("splicer_wrapper_"),
            "crate name: {}",
            crate_out.crate_name
        );
        assert!(crate_out.crate_name.contains("my_strategy"));

        // lib.rs has the pipeline's signature pieces.
        let lib = &crate_out.lib_rs;
        assert!(lib.contains("mod bindings"), "missing bindings:\n{lib}");
        assert!(
            lib.contains("TransformStrategy"),
            "transform pipeline should use TransformStrategy:\n{lib}"
        );
        assert!(lib.contains("OnceLock"), "missing OnceLock storage:\n{lib}");
        assert!(
            lib.contains("bindings::export!"),
            "missing component-export hookup:\n{lib}"
        );

        // Cargo.toml is parseable TOML and lists the strategy as a dep.
        let parsed: toml::Value = toml::from_str(&crate_out.cargo_toml).expect("cargo.toml parses");
        assert!(parsed["dependencies"].get("my-strategy").is_some());
    }

    #[test]
    fn virtualize_pipeline_swaps_strategy_trait() {
        let crate_out = generate_wrapper_crate(&input(Behavior::Virtualize)).unwrap();
        assert!(
            crate_out.lib_rs.contains("VirtualizeStrategy"),
            "virtualize pipeline should use VirtualizeStrategy:\n{}",
            crate_out.lib_rs
        );
        assert!(
            !crate_out.lib_rs.contains("TransformStrategy"),
            "virtualize pipeline should not import TransformStrategy:\n{}",
            crate_out.lib_rs
        );
    }

    #[test]
    fn wrapper_crate_name_distinguishes_inputs_that_sanitize_identically() {
        // Both inputs sanitize to "wasi_http_handler_0_3_0" — the hash
        // suffix is what keeps the two crate names distinct.
        let a = make_wrapper_crate_name("wasi:http/handler@0.3.0", "s");
        let b = make_wrapper_crate_name("wasi-http-handler-0-3-0", "s");
        assert_ne!(a, b);
    }
}
