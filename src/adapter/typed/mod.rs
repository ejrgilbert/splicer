//! Per-target wrapper-component source codegen for forward
//! (transform) and virtualize strategies. Sibling to the
//! bytecode-emitting [`super::tier1`] and [`super::tier2`]
//! adapters; emits Rust source that downstream stages compile to
//! wasm via `cargo build`.

mod assemble;
mod bindgen;
mod bindings_index;
mod build;
mod emit_method;
mod emit_wit_typed;
mod ir;
pub(crate) mod target_wit;

pub use assemble::{assemble_cargo_toml, assemble_lib_rs, CargoTomlInputs, WrapperCrateInputs};
pub use bindgen::run_wit_bindgen_rust;
pub use bindings_index::build_bindings_index;
pub use build::{build_wrapper, BuildConfig};
pub use emit_method::{emit_guest, emit_resource_newtypes, EmittedGuest};
pub use emit_wit_typed::emit_wit_typed_impls;
#[allow(unused_imports)]
pub use ir::{build_ir, NamedKind, NamedType, WitTypeRef, WrapperIR};
pub use target_wit::{target_wit_for_codegen, TargetWit};

use anyhow::Result;

/// What the strategy does to the wrapped target — the codegen
/// template's transform/virtualize knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// Strategy transforms the call — receives typed args + result
    /// and may mutate either before forwarding to the wrapped target.
    /// Wrapper imports the target's interface.
    Transform,
    /// Strategy replaces the wrapped target. Wrapper does not import
    /// the target's interface.
    Virtualize,
}

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
    let resource_newtypes = emit_resource_newtypes(&ir);

    let lib_rs = assemble_lib_rs(&WrapperCrateInputs {
        bindings_src: &bindings_src,
        witty_impls: &witty_impls,
        guests: &guests,
        resource_newtypes: &resource_newtypes,
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
        splicer_tool_sdk_version: input.splicer_tool_sdk_version,
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
    /// `splicer-tool-sdk` version (from the strategy crate's own
    /// Cargo.toml) that the wrapper's Cargo.toml depends on. Must
    /// match the version the strategy itself declares so cargo
    /// dedupes the two into a single source.
    pub splicer_tool_sdk_version: &'a str,
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
mod tests;
