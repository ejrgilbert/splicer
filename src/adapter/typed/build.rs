//! Build a wrapper crate to a wasm component on disk, with a cache
//! keyed by the inputs. Mirrors splicer's existing builtin build
//! convention from the Makefile: `cargo build --release --target
//! wasm32-wasip1` followed by `wasm-tools component new` to wrap the
//! core module with the WASI preview1 adapter.

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
    /// Directory to cache built wrapper components in. A cache hit
    /// returns the cached path immediately; a miss writes a new entry.
    pub cache_dir: &'a Path,
    /// Path to the wasi_snapshot_preview1 reactor adapter that
    /// `wasm-tools component new` uses to wrap the core module.
    pub adapter_wasm: &'a Path,
    /// Cargo target triple. Defaults to `"wasm32-wasip1"` if `None`.
    pub target: Option<&'a str>,
}

/// Hash-derived cache key for a wrapper-build invocation. Stable
/// within a single rustc version (uses `DefaultHasher`); on rustc
/// upgrade the hash changes, which is fine — it just invalidates the
/// cache. A blake3/sha2 swap is a future hardening item.
pub fn cache_key(input: &GenerateWrapperInput<'_>) -> String {
    let mut h = DefaultHasher::new();
    input.target_wit.hash(&mut h);
    input.world_name.hash(&mut h);
    input.interface_qualified_name.hash(&mut h);
    match input.behavior {
        Behavior::Forward => "forward".hash(&mut h),
        Behavior::Virtualize => "virtualize".hash(&mut h),
    }
    input.strategy_crate_name.hash(&mut h);
    input.strategy_crate_path.hash(&mut h);
    input.strategy_type.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Build a wrapper component for `input`, caching the result. Returns
/// the path to the produced wasm component (in `cache_dir`).
pub fn build_wrapper(input: &GenerateWrapperInput<'_>, config: &BuildConfig<'_>) -> Result<PathBuf> {
    require_tool("cargo")?;

    let key = cache_key(input);
    let cached_wasm = config.cache_dir.join(format!("{key}.wasm"));
    if cached_wasm.exists() {
        return Ok(cached_wasm);
    }

    let generated = generate_wrapper_crate(input)?;
    let tempdir = tempfile::tempdir().context("could not create build tempdir")?;
    let built_wasm = build_in_dir(&generated, tempdir.path(), config)?;

    fs::create_dir_all(config.cache_dir)
        .with_context(|| format!("could not create cache dir {}", config.cache_dir.display()))?;
    fs::copy(&built_wasm, &cached_wasm)
        .with_context(|| format!("could not copy build output to {}", cached_wasm.display()))?;
    Ok(cached_wasm)
}

/// Write `generated` to `build_dir` and run the cargo + wasm-tools
/// pipeline. Returns the path to the produced wasm component (inside
/// the build dir; caller is responsible for copying it out before the
/// dir is cleaned up).
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
    fs::write(src_dir.join("lib.rs"), &generated.lib_rs)
        .context("could not write src/lib.rs")?;

    let target = config.target.unwrap_or("wasm32-wasip1");
    let out = run_cargo_build(build_dir, target)?;
    if !out.status.success() {
        bail!(
            "cargo build failed (exit code {:?}):\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let module_name = generated.crate_name.replace('-', "_");
    let module_wasm = build_dir
        .join("target")
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

fn run_cargo_build(build_dir: &Path, target: &str) -> Result<Output> {
    Command::new("cargo")
        .args(["build", "--release", "--target", target])
        .current_dir(build_dir)
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
        Err(e) => Err(anyhow::Error::new(e)
            .context(format!("could not probe `{name}` on PATH"))),
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
            splicer_tool_sdk_path: "/sdk",
        }
    }

    #[test]
    fn cache_key_is_deterministic() {
        let a = cache_key(&sample_input(Behavior::Forward));
        let b = cache_key(&sample_input(Behavior::Forward));
        assert_eq!(a, b);
    }

    #[test]
    fn cache_key_distinguishes_behavior() {
        assert_ne!(
            cache_key(&sample_input(Behavior::Forward)),
            cache_key(&sample_input(Behavior::Virtualize)),
        );
    }

    #[test]
    fn cache_key_distinguishes_target_wit() {
        let mut a = sample_input(Behavior::Forward);
        let other_wit = "package t:p@0.2.0; interface i { f: func(); } world w { export i; }";
        a.target_wit = other_wit;
        assert_ne!(
            cache_key(&a),
            cache_key(&sample_input(Behavior::Forward)),
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

    /// End-to-end build test. Gated behind SPLICER_BUILD_E2E so it
    /// doesn't run in normal CI (slow; needs cargo + wasm-tools +
    /// wasm32-wasip1 target + paths to the SDK and adapter). Run it
    /// manually with:
    /// `SPLICER_BUILD_E2E=1 cargo test ... e2e_build_tiny_wrapper`.
    #[test]
    fn e2e_build_tiny_wrapper() {
        if std::env::var_os("SPLICER_BUILD_E2E").is_none() {
            return;
        }
        let cache_dir = tempfile::tempdir().unwrap();
        let adapter = std::env::var_os("SPLICER_PREVIEW1_ADAPTER")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("builtins")
                    .join("wasi_snapshot_preview1.reactor.wasm")
            });
        let _ = build_wrapper(
            &sample_input(Behavior::Virtualize),
            &BuildConfig {
                cache_dir: cache_dir.path(),
                adapter_wasm: &adapter,
                target: None,
            },
        )
        .expect("end-to-end build succeeds");
    }
}
