//! Tier-3/4 builtins: Rust strategy crates embedded into the splicer
//! binary. At splice-time the relevant builtin's source is extracted
//! to a cache dir, fed into the [`crate::adapter::typed`] codegen +
//! cargo build pipeline, and the resulting wrapper component is
//! dropped under `splits_dir/builtins/<name>.wasm` so the rest of the
//! splice pipeline treats it like any other path-backed middleware.
//!
//! TODO: publish builtins to crates.io and have generated wrappers
//! depend on them by version. cargo handles distribution; this file
//! goes away.

use anyhow::{Context, Result};
use builtin_manifest::{Manifest, Tier};
use heck::ToUpperCamelCase;
use include_dir::{include_dir, Dir};
use std::path::{Path, PathBuf};

use crate::adapter::typed::{
    build_wrapper, target_wit_for_codegen, Behavior, BuildConfig, GenerateWrapperInput, TargetWit,
};

/// Each shipped tier-3/4 builtin's source tree. Add an entry when
/// shipping a new builtin.
static EMBEDDED: &[(&str, &Dir<'_>)] =
    &[("hello-tier3", &HELLO_TIER3), ("hello-tier4", &HELLO_TIER4)];

static HELLO_TIER3: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/builtins/hello-tier3");
static HELLO_TIER4: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/builtins/hello-tier4");

/// Source tree of `splicer-tool-sdk`, embedded so generated wrapper
/// crates can resolve their path-dep on the SDK without the user
/// having splicer's repo on disk. `build.rs` stages a clean subset of
/// `splicer-tool-sdk/` (just `src/`, `Cargo.toml`, `README.md`) under
/// `$OUT_DIR/embedded-sdk/` so the SDK's `target/` dir isn't swept
/// into the splicer binary on developer machines.
///
/// TODO: drop the embed once `splicer-tool-sdk` is published to
/// crates.io and the generated `Cargo.toml` can depend on it by
/// version.
static EMBEDDED_SDK: Dir<'_> = include_dir!("$OUT_DIR/embedded-sdk");

/// Embedded WASI preview1 adapter, used to wrap the cargo-produced
/// core module into a wasm component. Pulled from `builtins/`
/// alongside the strategy crates so splicer doesn't need it on disk
/// at splice-time.
const PREVIEW1_ADAPTER: &[u8] =
    include_bytes!("../../builtins/wasi_snapshot_preview1.reactor.wasm");

/// Subdirectory of the per-process splits dir that materialized
/// builtin wasms land in. Mirrors the constant tier-1/2 uses; kept in
/// sync so users can't tell the two distribution paths apart.
const BUILTIN_SUBDIR: &str = "builtins";

/// Names of every embedded tier-3/4 builtin, in declaration order.
pub fn names() -> Vec<&'static str> {
    EMBEDDED.iter().map(|(n, _)| *n).collect()
}

/// Whether `name` refers to an embedded tier-3/4 builtin.
pub fn is_typed(name: &str) -> bool {
    EMBEDDED.iter().any(|(n, _)| *n == name)
}

/// Read the named builtin's `manifest.toml` directly from the embed,
/// without extracting anything to disk. Used by the `splicer builtin`
/// subcommand surface.
pub fn read_manifest(name: &str) -> Result<Manifest> {
    let dir = lookup(name)?;
    let file = dir
        .get_file("manifest.toml")
        .with_context(|| format!("embedded tier-3/4 builtin '{name}' has no manifest.toml"))?;
    let text = file.contents_utf8().context("manifest.toml is not UTF-8")?;
    toml::from_str(text).with_context(|| format!("failed to parse manifest.toml for '{name}'"))
}

/// Extract the named builtin's source tree into `dest_dir` (created
/// if missing). Idempotent; returns `dest_dir`.
pub fn extract(name: &str, dest_dir: &Path) -> Result<PathBuf> {
    let dir = lookup(name)?;
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("could not create {}", dest_dir.display()))?;
    dir.extract(dest_dir)
        .with_context(|| format!("could not extract '{name}' into {}", dest_dir.display()))?;
    Ok(dest_dir.to_path_buf())
}

/// Extract the embedded `splicer-tool-sdk` source tree to `dest_dir`
/// (created if missing). Generated wrapper crates path-dep on this
/// directory; layout matches the in-repo `splicer-tool-sdk/`.
pub fn extract_sdk(dest_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("could not create {}", dest_dir.display()))?;
    EMBEDDED_SDK.extract(dest_dir).with_context(|| {
        format!(
            "could not extract splicer-tool-sdk into {}",
            dest_dir.display()
        )
    })?;
    Ok(dest_dir.to_path_buf())
}

fn lookup(name: &str) -> Result<&'static Dir<'static>> {
    EMBEDDED
        .iter()
        .find_map(|(n, d)| (*n == name).then_some(*d))
        .with_context(|| format!("no embedded tier-3/4 builtin named '{name}'"))
}

/// Codegen + build the tier-3/4 wrapper specialized to
/// `target_interface`, using the WIT from `split_bytes`. Drops the
/// produced wasm at `splits_dir/builtins/<name>.wasm`.
pub fn materialize(
    splits_dir: &Path,
    name: &str,
    split_bytes: &[u8],
    target_interface: &str,
) -> Result<PathBuf> {
    let manifest = read_manifest(name)?;
    let behavior = behavior_for(&manifest, name)?;
    let target = target_wit_for_codegen(split_bytes, target_interface, behavior)?;
    build_and_install(splits_dir, name, behavior, &target)
}

fn build_and_install(
    splits_dir: &Path,
    name: &str,
    behavior: Behavior,
    target: &TargetWit,
) -> Result<PathBuf> {
    let cache_root = typed_cache_root()?;
    let strategy_dir = cache_root.join("strategies").join(name);
    extract(name, &strategy_dir)?;
    let sdk_dir = cache_root.join("splicer-tool-sdk");
    extract_sdk(&sdk_dir)?;

    let adapter_path = cache_root.join("wasi_snapshot_preview1.reactor.wasm");
    if !adapter_path.exists() {
        std::fs::write(&adapter_path, PREVIEW1_ADAPTER).with_context(|| {
            format!(
                "could not write preview1 adapter to {}",
                adapter_path.display()
            )
        })?;
    }

    let strategy_type = name.to_upper_camel_case();
    let strategy_path_str = strategy_dir
        .to_str()
        .with_context(|| format!("strategy path is not UTF-8: {}", strategy_dir.display()))?;
    let sdk_path_str = sdk_dir
        .to_str()
        .with_context(|| format!("sdk path is not UTF-8: {}", sdk_dir.display()))?;

    let input = GenerateWrapperInput {
        target_wit: &target.wit_text,
        world_name: Some(&target.world_name),
        interface_qualified_name: &target.qualified_name,
        behavior,
        strategy_crate_name: name,
        strategy_crate_path: strategy_path_str,
        strategy_type: &strategy_type,
        splicer_tool_sdk_path: sdk_path_str,
    };
    let build_cache = cache_root.join("build-cache");
    let built = build_wrapper(
        &input,
        &BuildConfig {
            cache_dir: &build_cache,
            adapter_wasm: &adapter_path,
            target: None,
        },
    )?;

    let out_dir = splits_dir.join(BUILTIN_SUBDIR);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("could not create {}", out_dir.display()))?;
    let out = out_dir.join(format!("{name}.wasm"));
    std::fs::copy(&built, &out).with_context(|| {
        format!(
            "could not copy built wrapper {} -> {}",
            built.display(),
            out.display()
        )
    })?;
    Ok(out)
}

fn behavior_for(manifest: &Manifest, name: &str) -> Result<Behavior> {
    match manifest.builtin.tier {
        Tier::Tier3 => Ok(Behavior::Transform),
        Tier::Tier4 => Ok(Behavior::Virtualize),
        other => anyhow::bail!(
            "tier-3/4 codegen invoked on '{name}' but its manifest declares tier {other:?}"
        ),
    }
}

/// `<user-cache>/splicer/typed-builtins/`. Codegen artifacts, extracted
/// strategy crates, and the embedded SDK live here. Survives between
/// splices so cargo's incremental compilation can warm up.
fn typed_cache_root() -> Result<PathBuf> {
    let base = super::user_cache_dir().context(
        "no user cache directory available; \
         set XDG_CACHE_HOME or HOME to enable tier-3/4 codegen",
    )?;
    Ok(base.join("splicer").join("typed-builtins"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_include_both_smoke_builtins() {
        let names = names();
        assert!(names.contains(&"hello-tier3"), "got: {names:?}");
        assert!(names.contains(&"hello-tier4"), "got: {names:?}");
    }

    #[test]
    fn is_typed_recognizes_embedded_names() {
        assert!(is_typed("hello-tier3"));
        assert!(is_typed("hello-tier4"));
        assert!(!is_typed("hello-tier1"));
        assert!(!is_typed("does-not-exist"));
    }

    #[test]
    fn read_manifest_returns_tier3_or_tier4_manifest() {
        for name in names() {
            let manifest = read_manifest(name).unwrap();
            assert!(
                matches!(manifest.builtin.tier, Tier::Tier3 | Tier::Tier4),
                "{name} manifest is not tier-3 or tier-4: got {:?}",
                manifest.builtin.tier
            );
        }
    }

    #[test]
    fn read_manifest_unknown_name_errors() {
        let err = read_manifest("not-a-real-builtin").unwrap_err();
        assert!(err.to_string().contains("not-a-real-builtin"));
    }

    #[test]
    fn extract_writes_manifest_and_lib() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("hello-tier3");
        extract("hello-tier3", &dest).expect("extract succeeds");
        assert!(dest.join("Cargo.toml").exists());
        assert!(dest.join("manifest.toml").exists());
        assert!(dest.join("src/lib.rs").exists());
    }

    #[test]
    fn extract_unknown_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = extract("not-a-real-builtin", tmp.path()).unwrap_err();
        assert!(err.to_string().contains("not-a-real-builtin"));
    }

    #[test]
    fn extract_sdk_writes_cargo_toml_and_lib_rs() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("splicer-tool-sdk");
        extract_sdk(&dest).expect("extract sdk succeeds");
        assert!(dest.join("Cargo.toml").exists());
        assert!(dest.join("src/lib.rs").exists());
    }

    /// End-to-end materialize: synthesizes a tiny composition that
    /// exports an interface, then drives the full strategy + SDK
    /// extract → codegen → cargo → install pipeline against it.
    /// Shells out to cargo, gated behind `--ignored`.
    #[test]
    #[ignore = "shells out to cargo + wasm32-wasip1; run with --ignored"]
    fn materialize_tier3_produces_a_component() {
        use crate::adapter::typed::target_wit::test_fixture::component_from_wit;
        const FIXTURE_WIT: &str = r#"
            package test:demo@0.1.0;
            interface ops {
                add: async func(a: u32, b: u32) -> u32;
            }
            world demo {
                export ops;
            }
        "#;
        let composition = component_from_wit(FIXTURE_WIT, "demo").expect("synthesize fixture");
        let splits = tempfile::tempdir().unwrap();
        let out = materialize(
            splits.path(),
            "hello-tier3",
            &composition,
            "test:demo/ops@0.1.0",
        )
        .expect("materialize");
        assert!(out.ends_with("builtins/hello-tier3.wasm"));
        let bytes = std::fs::read(&out).expect("read");
        assert!(bytes.starts_with(&[0x00, 0x61, 0x73, 0x6d]), "wasm magic");
        let parser = wasmparser::Parser::new(0);
        for payload in parser.parse_all(&bytes) {
            payload.expect("component payload parses");
        }
    }
}
