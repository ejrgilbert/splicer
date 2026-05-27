//! Build a wrapper crate to a wasm component on disk. Each unique
//! `(target, behavior, strategy)` tuple gets a persistent build dir
//! under `build_root/builds/<key>/` so cargo's incremental compile
//! handles re-run staleness — no custom wasm cache. All wrapper
//! crates share one cargo `target/` (`CARGO_TARGET_DIR=
//! build_root/target/`) so the dep closure (wit-bindgen,
//! splicer-tool-sdk, syn, etc.) compiles once across every wrapper
//! splicer ever builds on this machine.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{anyhow, bail, Context, Result};
use wit_component::ComponentEncoder;

use super::{generate_wrapper_crate, Behavior, GenerateWrapperInput, WrapperCrate};

/// Knobs that don't come from the wrapper inputs but are needed to
/// actually drive cargo.
pub struct BuildConfig<'a> {
    /// Root under which per-build directories live. Each unique
    /// `(target_wit, behavior, strategy)` gets a subdirectory so
    /// cargo's incremental compile stays warm across runs.
    pub build_root: &'a Path,
    /// Path to the wasi_snapshot_preview1 reactor adapter that
    /// `wasm-tools component new` uses to wrap the core module.
    pub adapter_wasm: &'a Path,
    /// Cargo target triple. Defaults to `"wasm32-wasip1"` if `None`.
    pub target: Option<&'a str>,
}

/// Stable name for the per-build directory under `build_root/builds/`.
/// Different `(target, behavior, strategy)` tuples get distinct
/// directories; identical tuples reuse the same one so cargo's
/// incremental compile can amortize across runs. Strategy source
/// changes are picked up by cargo (file mtime), not by this hash.
fn build_dir_key(input: &GenerateWrapperInput<'_>) -> String {
    let mut h = DefaultHasher::new();
    input.target_wit.hash(&mut h);
    input.world_name.hash(&mut h);
    input.interface_qualified_name.hash(&mut h);
    match input.behavior {
        Behavior::Transform => "transform".hash(&mut h),
        Behavior::Virtualize => "virtualize".hash(&mut h),
    }
    input.strategy_crate_name.hash(&mut h);
    input.strategy_crate_path.hash(&mut h);
    input.strategy_type.hash(&mut h);
    input.splicer_tool_sdk_version.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Build a wrapper component for `input`. Reuses the per-key build
/// dir across runs so cargo's incremental compile handles staleness.
/// Returns the path to the produced wasm component, which lives
/// inside the persistent build dir.
pub fn build_wrapper(
    input: &GenerateWrapperInput<'_>,
    config: &BuildConfig<'_>,
) -> Result<PathBuf> {
    require_tool("cargo")?;

    let build_dir = config.build_root.join("builds").join(build_dir_key(input));
    fs::create_dir_all(&build_dir)
        .with_context(|| format!("could not create build dir {}", build_dir.display()))?;

    let generated = generate_wrapper_crate(input)?;
    build_in_dir(&generated, &build_dir, config)
}

/// Write `generated` to `build_dir` and run the cargo + wit-component
/// pipeline. Returns the path to the produced wasm component inside
/// the build dir.
fn build_in_dir(
    generated: &WrapperCrate,
    build_dir: &Path,
    config: &BuildConfig<'_>,
) -> Result<PathBuf> {
    let src_dir = build_dir.join("src");
    fs::create_dir_all(&src_dir)
        .with_context(|| format!("could not create {}", src_dir.display()))?;
    fs::write(build_dir.join("Cargo.toml"), &generated.cargo_toml)
        .context("could not write Cargo.toml")?;
    fs::write(src_dir.join("lib.rs"), &generated.lib_rs).context("could not write src/lib.rs")?;

    let target = config.target.unwrap_or("wasm32-wasip1");
    // Shared target dir across every wrapper build so cargo amortizes
    // the dep closure once instead of per-build. Concurrent splices are
    // serialized by cargo's own `target/debug/.cargo-lock`.
    let cargo_target_dir = config.build_root.join("target");
    let out = run_cargo_build(build_dir, &cargo_target_dir, target)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let hint = trait_bound_hint(&stderr);
        bail!(
            "cargo build failed (exit code {:?}){hint}:\n{stderr}",
            out.status.code(),
        );
    }

    let module_name = generated.crate_name.replace('-', "_");
    let module_wasm = cargo_target_dir
        .join(target)
        .join("release")
        .join(format!("{module_name}.wasm"));
    if !module_wasm.exists() {
        bail!(
            "expected cargo to produce {} but it doesn't exist",
            module_wasm.display()
        );
    }

    let component_wasm = build_dir.join("component.wasm");
    wrap_module_into_component(&module_wasm, config.adapter_wasm, &component_wasm)?;
    Ok(component_wasm)
}

/// Library-side equivalent of `wasm-tools component new <module>
/// --adapt <adapter> --skip-validation -o <out>`. Uses
/// `wit_component::ComponentEncoder` directly so we don't need
/// `wasm-tools` on PATH at splice-time.
fn wrap_module_into_component(
    module_wasm: &Path,
    adapter_wasm: &Path,
    out_wasm: &Path,
) -> Result<()> {
    let module_bytes = fs::read(module_wasm)
        .with_context(|| format!("could not read module {}", module_wasm.display()))?;
    let adapter_bytes = fs::read(adapter_wasm)
        .with_context(|| format!("could not read adapter {}", adapter_wasm.display()))?;
    let component_bytes = ComponentEncoder::default()
        .validate(false)
        .module(&module_bytes)
        .context("ComponentEncoder rejected the core module")?
        .adapter("wasi_snapshot_preview1", &adapter_bytes)
        .context("ComponentEncoder rejected the preview1 adapter")?
        .encode()
        .context("ComponentEncoder failed to produce a component")?;
    fs::write(out_wasm, &component_bytes)
        .with_context(|| format!("could not write component to {}", out_wasm.display()))?;
    Ok(())
}

/// Spot E0277 trait-bound errors in cargo's stderr — usually means
/// the strategy's where-clause doesn't fit the wrapped target's
/// types (e.g. `hello-tier4` needs `R: Default`).
fn trait_bound_hint(stderr: &str) -> &'static str {
    if stderr.contains("E0277") || stderr.contains("trait bound") {
        " — the strategy's trait bounds don't fit the target's types; \
         check the builtin's manifest description for required bounds"
    } else {
        ""
    }
}

fn run_cargo_build(build_dir: &Path, cargo_target_dir: &Path, target: &str) -> Result<Output> {
    Command::new("cargo")
        .args(["build", "--release", "--target", target])
        .current_dir(build_dir)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .output()
        .context("failed to invoke `cargo build`")
}

/// Surface a precise error if cargo is missing on PATH. wit-component
/// runs in-process via the library, so it has no PATH requirement.
fn require_tool(name: &str) -> Result<()> {
    match Command::new(name).arg("--version").output() {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(anyhow!(
            "`{name}` is not on PATH. Tier-3/4 wrappers are compiled with cargo at \
             splice-time; install Rust (https://rustup.rs) before splicing."
        )),
        Err(e) => Err(anyhow::Error::new(e).context(format!("could not probe `{name}` on PATH"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_input<'a>(behavior: Behavior) -> GenerateWrapperInput<'a> {
        GenerateWrapperInput {
            target_wit: "package t:p@0.1.0; interface i { f: func(); } world w { export i; }",
            world_name: Some("w"),
            interface_qualified_name: "t:p/i@0.1.0",
            behavior,
            strategy_crate_name: "s",
            strategy_crate_path: "/p",
            strategy_type: "S",
            splicer_tool_sdk_version: "0.1.0",
        }
    }

    #[test]
    fn trait_bound_hint_fires_on_e0277() {
        let stderr = "error[E0277]: the trait bound `Response: Default` is not satisfied";
        assert!(!trait_bound_hint(stderr).is_empty());
    }

    #[test]
    fn trait_bound_hint_silent_on_unrelated_failures() {
        let stderr = "error: linking with `cc` failed: exit status: 1";
        assert!(trait_bound_hint(stderr).is_empty());
    }

    #[test]
    fn build_dir_key_is_deterministic() {
        let a = build_dir_key(&sample_input(Behavior::Transform));
        let b = build_dir_key(&sample_input(Behavior::Transform));
        assert_eq!(a, b);
    }

    #[test]
    fn build_dir_key_distinguishes_behavior() {
        assert_ne!(
            build_dir_key(&sample_input(Behavior::Transform)),
            build_dir_key(&sample_input(Behavior::Virtualize)),
        );
    }

    #[test]
    fn build_dir_key_distinguishes_target_wit() {
        let mut a = sample_input(Behavior::Transform);
        let other_wit = "package t:p@0.2.0; interface i { f: func(); } world w { export i; }";
        a.target_wit = other_wit;
        assert_ne!(
            build_dir_key(&a),
            build_dir_key(&sample_input(Behavior::Transform)),
        );
    }

    #[test]
    fn build_dir_key_distinguishes_sdk_version() {
        let mut a = sample_input(Behavior::Transform);
        a.splicer_tool_sdk_version = "0.2.0";
        assert_ne!(
            build_dir_key(&a),
            build_dir_key(&sample_input(Behavior::Transform)),
        );
    }

    #[test]
    fn require_tool_returns_clear_error_for_missing_tool() {
        let err = require_tool("a-cli-that-cannot-possibly-exist-zzz").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not on PATH"),
            "expected `not on PATH` in error: {msg}"
        );
    }
}
