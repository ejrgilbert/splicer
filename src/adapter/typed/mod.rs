//! Per-target wrapper-component source codegen for forward
//! (transform) and virtualize strategies. Sibling to the
//! bytecode-emitting [`super::tier1`] and [`super::tier2`]
//! adapters; emits Rust source that downstream stages compile to
//! wasm via `cargo build`.
//!
//! Pipeline (stages 3-4 of `docs/TODO/tier3-tier4-substrate.md`):
//! 1. [`read_behavior`] reads the strategy crate's `Cargo.toml`
//!    `[package.metadata.splicer] behavior = ...` declaration.
//! 2. The codegen template emits a Cargo project tailored to the
//!    declared behavior: forward strategies get a wrapper that
//!    imports the target; virtualize strategies get a wrapper that
//!    does not.
//! 3. The cargo build pipeline compiles the project to a wrapper
//!    `.wasm` and caches the result.

mod assemble;
mod behavior_meta;
mod bindgen;
mod bindings_walk;
mod emit_method;
mod emit_wit_typed;

pub use assemble::{
    assemble_cargo_toml, assemble_lib_rs, CargoTomlInputs, WrapperCrateInputs,
};
pub use behavior_meta::{read_behavior, read_behavior_from_str, Behavior, BehaviorReadError};
pub use bindgen::run_wit_bindgen_rust;
pub use bindings_walk::{
    walk_bindings, GuestMethod, GuestTrait, TypeDef, TypeDefKind, WrapperBindings,
};
pub use emit_method::{emit_guest, EmittedGuest};
pub use emit_wit_typed::emit_wit_typed_impls;

use anyhow::Result;

/// One-call orchestrator: take a target WIT and a strategy reference,
/// produce the full source of a wrapper crate that compiles to a
/// wasm component. Threads the six module pipeline:
/// wit-bindgen → walk → emit_wit_typed → emit_method → assemble.
pub fn generate_wrapper_crate(input: &GenerateWrapperInput<'_>) -> Result<WrapperCrate> {
    let bindings_src = run_wit_bindgen_rust(input.target_wit, input.world_name)?;
    let bindings = walk_bindings(&bindings_src)?;
    let witty_impls = emit_wit_typed_impls(&bindings.types);
    let guests: Vec<EmittedGuest> = bindings
        .guest_traits
        .iter()
        .map(|g| emit_guest(g, input.interface_qualified_name, input.behavior))
        .collect();

    let lib_rs = assemble_lib_rs(&WrapperCrateInputs {
        bindings_src: &bindings_src,
        witty_impls: &witty_impls,
        guests: &guests,
        behavior: input.behavior,
        strategy_crate_name: input.strategy_crate_name,
        strategy_type: input.strategy_type,
    })?;

    let crate_name = make_wrapper_crate_name(input.interface_qualified_name, input.strategy_crate_name);
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
    /// Forward (tier-3) or virtualize (tier-4).
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
/// package identifier: lowercase alphanumerics + underscores.
fn make_wrapper_crate_name(interface: &str, strategy: &str) -> String {
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
    format!(
        "splicer_wrapper_{}_{}",
        sanitize(interface),
        sanitize(strategy)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_WIT: &str = r#"
        package test:pkg@0.1.0;
        interface ops {
            add: func(a: u32, b: u32) -> u32;
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
    fn forward_pipeline_produces_a_complete_wrapper_crate() {
        let crate_out = generate_wrapper_crate(&input(Behavior::Forward)).unwrap();
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
            lib.contains("ForwardStrategy"),
            "forward pipeline should use ForwardStrategy:\n{lib}"
        );
        assert!(lib.contains("thread_local"), "missing thread_local:\n{lib}");
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
            !crate_out.lib_rs.contains("ForwardStrategy"),
            "virtualize pipeline should not import ForwardStrategy:\n{}",
            crate_out.lib_rs
        );
    }
}
