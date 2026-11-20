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

use super::{generate_wrapper_crate, Behavior, GenerateWrapperInput};

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

/// Result of compiling a wrapper crate.
pub enum BuildOutcome {
    /// Wrapper compiled to a component wasm at this path.
    Built(PathBuf),
    /// Cargo failed on an unsatisfied trait bound, meaning the strategy
    /// doesn't fit this interface; callers may skip rather than fail.
    BoundMismatch {
        strategy: String,
        interface: String,
        /// Unsatisfied bound as rustc reported it, when parseable.
        bound: Option<String>,
        stderr: String,
    },
}

/// Build a wrapper component for `input`. Reuses the per-key build
/// dir across runs so cargo's incremental compile handles staleness.
/// A trait-bound failure yields [`BuildOutcome::BoundMismatch`] (the
/// strategy doesn't fit this interface); any other failure is `Err`.
pub fn build_wrapper(
    input: &GenerateWrapperInput<'_>,
    config: &BuildConfig<'_>,
) -> Result<BuildOutcome> {
    require_tool("cargo")?;
    let target = config.target.unwrap_or("wasm32-wasip1");
    let generated = generate_wrapper_crate(input)?;
    match compile_sources(
        &build_dir_key(input),
        &generated.crate_name,
        &generated.lib_rs,
        &generated.cargo_toml,
        config.build_root,
        config.adapter_wasm,
        target,
    )? {
        Ok(path) => Ok(BuildOutcome::Built(path)),
        Err(stderr) => Ok(BuildOutcome::BoundMismatch {
            strategy: input.strategy_crate_name.to_string(),
            interface: input.interface_qualified_name.to_string(),
            bound: unsatisfied_bound(&stderr),
            stderr,
        }),
    }
}

/// Build a pre-written crate (lib.rs + Cargo.toml) to a component wasm.
/// Any compile error is a hard failure (no skip path).
pub fn build_crate_source(
    crate_name: &str,
    lib_rs: &str,
    cargo_toml: &str,
    build_root: &Path,
    adapter_wasm: &Path,
    target: Option<&str>,
) -> Result<PathBuf> {
    require_tool("cargo")?;
    let mut h = DefaultHasher::new();
    crate_name.hash(&mut h);
    lib_rs.hash(&mut h);
    let key = format!("{:016x}", h.finish());
    let target = target.unwrap_or("wasm32-wasip1");
    compile_sources(
        &key,
        crate_name,
        lib_rs,
        cargo_toml,
        build_root,
        adapter_wasm,
        target,
    )?
    .map_err(|stderr| anyhow::anyhow!("crate `{crate_name}` failed to compile:\n{stderr}"))
}

fn compile_sources(
    key: &str,
    crate_name: &str,
    lib_rs: &str,
    cargo_toml: &str,
    build_root: &Path,
    adapter_wasm: &Path,
    target: &str,
) -> Result<Result<PathBuf, String>> {
    let build_dir = build_root.join("builds").join(key);
    fs::create_dir_all(&build_dir)
        .with_context(|| format!("could not create build dir {}", build_dir.display()))?;
    let src_dir = build_dir.join("src");
    fs::create_dir_all(&src_dir)
        .with_context(|| format!("could not create {}", src_dir.display()))?;
    fs::write(build_dir.join("Cargo.toml"), cargo_toml).context("could not write Cargo.toml")?;
    fs::write(src_dir.join("lib.rs"), lib_rs).context("could not write src/lib.rs")?;

    // Shared target dir so cargo amortizes the dep closure once across
    // every wrapper build. Concurrent splices are serialized by cargo's
    // own `target/debug/.cargo-lock`.
    let cargo_target_dir = build_root.join("target");
    let out = run_cargo_build(&build_dir, &cargo_target_dir, target)?;
    if !out.status.success() {
        return Ok(Err(String::from_utf8_lossy(&out.stderr).into_owned()));
    }

    let module_name = crate_name.replace('-', "_");
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
    wrap_module_into_component(&module_wasm, adapter_wasm, &component_wasm)?;
    Ok(Ok(component_wasm))
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

fn unsatisfied_bound(stderr: &str) -> Option<String> {
    stderr.lines().find_map(|line| {
        line.trim()
            .strip_prefix("error[E0277]: the trait bound `")?
            .strip_suffix("` is not satisfied")
            .map(str::to_string)
    })
}

/// Verify the strategy crate compiles on its own, independent of any
/// target interface.
pub fn smoke_check_strategy(
    strategy_dir: &Path,
    build_root: &Path,
    target: Option<&str>,
) -> Result<()> {
    require_tool("cargo")?;
    let target = target.unwrap_or("wasm32-wasip1");
    let cargo_target_dir = build_root.join("target");
    let mut cmd = Command::new("cargo");
    cmd.args(["check", "--target", target])
        .current_dir(strategy_dir)
        .env("CARGO_TARGET_DIR", &cargo_target_dir)
        // Plain stderr: this output is surfaced verbatim in error messages.
        .env("CARGO_TERM_COLOR", "never");
    if let Some(sdk) = super::assemble::local_sdk_path() {
        let val = toml::Value::String(sdk);
        cmd.arg("--config")
            .arg(format!("patch.crates-io.splicer-tool-sdk.path={val}"));
    }
    let out = cmd
        .output()
        .context("failed to invoke `cargo check` on strategy crate")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "strategy crate at {} does not compile standalone:\n{stderr}",
            strategy_dir.display()
        );
    }
    Ok(())
}

fn run_cargo_build(build_dir: &Path, cargo_target_dir: &Path, target: &str) -> Result<Output> {
    Command::new("cargo")
        .args(["build", "--release", "--target", target])
        .current_dir(build_dir)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        // force plain output
        .env("CARGO_TERM_COLOR", "never")
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
            splicer_tool_sdk_version: crate::test_consts::SDK_TEST_VERSION,
        }
    }

    #[test]
    fn unsatisfied_bound_extracts_when_present_else_none() {
        let with_bound =
            "error[E0277]: the trait bound `Response: HasArbitraryErr` is not satisfied";
        assert_eq!(
            unsatisfied_bound(with_bound).as_deref(),
            Some("Response: HasArbitraryErr")
        );
        // No E0277 bound line: the warning just omits the bound.
        assert!(unsatisfied_bound("error: linking with `cc` failed").is_none());
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
        a.splicer_tool_sdk_version = "99.99.99-distinct";
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
