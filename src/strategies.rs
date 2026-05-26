//! Tier-3/4 strategy compilation. Two distribution paths flow through
//! the same codegen pipeline:
//!
//! 1. **Embedded builtins** — strategy crate source compiled into the
//!    splicer binary via `include_dir!`. At splice-time the source is
//!    extracted to a cache dir before cargo runs.
//! 2. **User-supplied** — strategy crate directory on disk, referenced
//!    from the splice YAML via `name:` + `path:`. Used in place,
//!    no extraction.
//!
//! Both routes converge on a [`PreparedStrategy`] and the shared
//! [`materialize_from_prepared`] core, which feeds
//! [`crate::adapter::typed`] (codegen primitives) + cargo to build a
//! per-target wrapper component. The component is dropped under
//! `splits_dir/builtins/<name>.wasm` so the rest of the splice
//! pipeline treats it like any other path-backed middleware.
//!
//! TODO: publish builtins to crates.io and have generated wrappers
//! depend on them by version. cargo handles distribution; the embed
//! and `prepare_builtin_strategy` go away.

use anyhow::{Context, Result};
use builtin_protocol::{Manifest, Tier};
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
const PREVIEW1_ADAPTER: &[u8] = include_bytes!("../builtins/wasi_snapshot_preview1.reactor.wasm");

/// Subdirectory of the per-process splits dir that materialized
/// builtin wasms land in. Mirrors the constant tier-1/2 uses; kept in
/// sync so users can't tell the two distribution paths apart.
const BUILTIN_SUBDIR: &str = "builtins";

/// Names of every embedded tier-3/4 builtin, in declaration order.
pub fn names() -> Vec<&'static str> {
    EMBEDDED.iter().map(|(n, _)| *n).collect()
}

/// Whether `name` refers to an embedded tier-3/4 builtin.
pub fn is_embedded_builtin(name: &str) -> bool {
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

/// Where a tier-3/4 strategy's source comes from at splice time.
/// Each variant maps to one `prepare_*` step; the rest of the
/// pipeline is source-agnostic.
pub enum Tier3_4Source<'a> {
    /// Embedded builtin, referenced by name. The strategy crate's
    /// source + the SDK are extracted from the splicer binary to a
    /// cache dir before cargo runs.
    Builtin(&'a str),
    /// User-supplied strategy crate on disk. `wac_name` is the YAML
    /// `name:` field (the WAC variable identifier and the output
    /// filename stem); `strategy_dir` is the crate root containing
    /// `Cargo.toml` + `manifest.toml` + `src/`. The Cargo package
    /// name and Rust struct ident are read from the on-disk
    /// `Cargo.toml`, so YAML and crate names don't have to match.
    User {
        wac_name: &'a str,
        strategy_dir: &'a Path,
    },
}

/// True iff `path` looks like a user-supplied tier-3/4 strategy
/// crate root: an existing directory containing a `manifest.toml`.
/// The manifest check is what distinguishes a strategy crate from a
/// stray directory the user pointed at by accident — the contents
/// (tier 3 vs 4) are validated downstream by [`read_user_manifest`].
pub fn is_user_strategy_dir(path: &Path) -> bool {
    path.is_dir() && path.join("manifest.toml").is_file()
}

/// Codegen + build a tier-3/4 wrapper specialized to
/// `target_interface`, sourcing the strategy from `source`. Drops
/// the produced wasm at `splits_dir/builtins/<name>.wasm` (where
/// `name` is the builtin name or the YAML `name:` field, per
/// variant). Returns the wasm path and the resolved tier so the
/// caller can cache it on the injection without re-reading the
/// manifest.
pub fn materialize_tier3_4(
    splits_dir: &Path,
    source: Tier3_4Source<'_>,
    split_bytes: &[u8],
    target_interface: &str,
) -> Result<(PathBuf, Tier)> {
    let prep = match source {
        Tier3_4Source::Builtin(name) => prepare_builtin_strategy(name)?,
        Tier3_4Source::User {
            wac_name,
            strategy_dir,
        } => prepare_user_strategy(wac_name, strategy_dir)?,
    };
    materialize_from_prepared(splits_dir, prep, split_bytes, target_interface)
}

/// Per-source inputs to the shared tier-3/4 codegen pipeline.
/// Either an embedded builtin (`prepare_builtin_strategy`) or a
/// user-supplied directory (`prepare_user_strategy`) yields one of
/// these — everything downstream operates on the same shape so the
/// two distribution paths share their entire tail.
struct PreparedStrategy {
    /// Manifest declares the tier (validated into [`Behavior`] by
    /// [`behavior_for`]) and is returned to the caller via
    /// `manifest.builtin.tier`.
    manifest: Manifest,
    /// File-name stem for the materialized wrapper under
    /// `splits_dir/builtins/<out_name>.wasm`. For builtins this is
    /// the builtin name; for user crates it's the YAML `name:` field.
    out_name: String,
    /// On-disk strategy crate root (`Cargo.toml` + `manifest.toml`
    /// + `src/`).
    strategy_dir: PathBuf,
    /// `splicer-tool-sdk` source root the wrapper crate's
    /// `Cargo.toml` references; must canonicalize identically to the
    /// path the strategy crate itself uses or cargo treats them as
    /// distinct sources.
    sdk_dir: PathBuf,
    /// Cargo `[package].name` of the strategy crate — the dep key
    /// the wrapper's `Cargo.toml` uses to reference it.
    strategy_crate_name: String,
}

/// Extract the embedded builtin's source + the embedded SDK to the
/// per-process cache, returning the inputs the shared codegen
/// pipeline needs. The builtin name doubles as the Cargo package
/// name and the output filename stem (the embed convention).
fn prepare_builtin_strategy(name: &str) -> Result<PreparedStrategy> {
    let manifest = read_manifest(name)?;
    let cache_root = typed_cache_root()?;
    let strategy_dir = cache_root.join("strategies").join(name);
    extract(name, &strategy_dir)?;
    let sdk_dir = cache_root.join("splicer-tool-sdk");
    extract_sdk(&sdk_dir)?;
    Ok(PreparedStrategy {
        manifest,
        out_name: name.to_string(),
        strategy_dir,
        sdk_dir,
        strategy_crate_name: name.to_string(),
    })
}

/// Read the user's strategy crate metadata (manifest, Cargo package
/// name, SDK path) and assemble the inputs the shared codegen
/// pipeline needs. The output filename stem is the caller-supplied
/// `wac_name` (YAML identifier), so users aren't forced to align
/// YAML and crate names.
fn prepare_user_strategy(wac_name: &str, strategy_dir: &Path) -> Result<PreparedStrategy> {
    let manifest = read_user_manifest(strategy_dir)?;
    let meta = read_user_strategy_metadata(strategy_dir)?;
    Ok(PreparedStrategy {
        manifest,
        out_name: wac_name.to_string(),
        strategy_dir: strategy_dir.to_path_buf(),
        sdk_dir: meta.sdk_dir,
        strategy_crate_name: meta.crate_name,
    })
}

/// Source-agnostic core of the tier-3/4 codegen pipeline. Given a
/// fully-resolved [`PreparedStrategy`], drives the manifest → WIT →
/// wrapper-crate → cargo build → install sequence and returns the
/// produced wasm path + the strategy's declared tier.
fn materialize_from_prepared(
    splits_dir: &Path,
    prep: PreparedStrategy,
    split_bytes: &[u8],
    target_interface: &str,
) -> Result<(PathBuf, Tier)> {
    let behavior = behavior_for(&prep.manifest, &prep.out_name)?;
    let target = target_wit_for_codegen(split_bytes, target_interface, behavior)?;
    let cache_root = typed_cache_root()?;
    let adapter_path = ensure_preview1_adapter(&cache_root)?;
    let strategy_type = prep.strategy_crate_name.to_upper_camel_case();

    let plan = BuildPlan {
        out_name: &prep.out_name,
        strategy_dir: &prep.strategy_dir,
        sdk_dir: &prep.sdk_dir,
        strategy_crate_name: &prep.strategy_crate_name,
        strategy_type: &strategy_type,
        adapter_path: &adapter_path,
        cache_root: &cache_root,
    };
    let path = run_codegen_build(splits_dir, &plan, behavior, &target)?;
    Ok((path, prep.manifest.builtin.tier))
}

/// Read `manifest.toml` from the user-supplied strategy directory.
fn read_user_manifest(strategy_dir: &Path) -> Result<Manifest> {
    let manifest_path = strategy_dir.join("manifest.toml");
    let text = std::fs::read_to_string(&manifest_path).with_context(|| {
        format!(
            "could not read manifest.toml at {}; user-supplied tier-3/4 strategy crates must \
             ship a manifest.toml at their crate root",
            manifest_path.display()
        )
    })?;
    toml::from_str(&text).with_context(|| format!("failed to parse {}", manifest_path.display()))
}

/// Cargo-side metadata read from a user-supplied strategy crate's
/// `Cargo.toml`.
#[derive(Debug)]
struct UserStrategyMetadata {
    /// `[package].name` — the dep name the wrapper's Cargo.toml
    /// references.
    crate_name: String,
    /// Canonical absolute path of the strategy's
    /// `splicer-tool-sdk` dep. Threaded into the wrapper crate's
    /// Cargo.toml at the same path so cargo dedupes the two sources
    /// into one crate (mismatched paths produce E0308 in the
    /// wrapper's generated code).
    sdk_dir: PathBuf,
}

/// Parse the user's `Cargo.toml` to extract the strategy's Cargo
/// package name and the absolute path of its `splicer-tool-sdk`
/// dep. The SDK path is canonicalized so the wrapper's own dep
/// resolves to the same crate.
///
/// Today `splicer-tool-sdk` isn't on crates.io, so a path dep is the
/// only supported form. Once it's published this function can be
/// relaxed to accept registry/version deps and propagate the
/// version into the wrapper's `Cargo.toml`.
fn read_user_strategy_metadata(strategy_dir: &Path) -> Result<UserStrategyMetadata> {
    let cargo_path = strategy_dir.join("Cargo.toml");
    let cargo_text = std::fs::read_to_string(&cargo_path)
        .with_context(|| format!("could not read {}", cargo_path.display()))?;
    let parsed: toml::Value = toml::from_str(&cargo_text)
        .with_context(|| format!("failed to parse {}", cargo_path.display()))?;

    let crate_name = parsed
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .with_context(|| {
            format!(
                "{} has no [package].name; user strategy crates must declare a package name",
                cargo_path.display()
            )
        })?
        .to_string();

    let sdk_rel = parsed
        .get("dependencies")
        .and_then(|d| d.get("splicer-tool-sdk"))
        .and_then(|d| match d {
            toml::Value::Table(t) => t.get("path").and_then(|p| p.as_str()),
            _ => None,
        })
        .with_context(|| {
            format!(
                "user strategy crate at {} must declare `splicer-tool-sdk = {{ path = \"...\" }}` \
                 in [dependencies] (registry deps will be accepted once splicer-tool-sdk is \
                 published to crates.io)",
                cargo_path.display()
            )
        })?;

    let sdk_abs = strategy_dir.join(sdk_rel);
    let sdk_dir = std::fs::canonicalize(&sdk_abs).with_context(|| {
        format!(
            "could not canonicalize splicer-tool-sdk path '{}' (resolved to {}) from {}",
            sdk_rel,
            sdk_abs.display(),
            cargo_path.display(),
        )
    })?;

    Ok(UserStrategyMetadata {
        crate_name,
        sdk_dir,
    })
}

/// Concrete inputs to [`run_codegen_build`]: where the strategy crate
/// lives, where the `splicer-tool-sdk` it depends on lives, and the
/// Rust idents the wrapper crate will emit. Decoupled from the embed
/// so both the builtin (extracted-to-cache) and user-supplied
/// (on-disk path) tier-3/4 paths share the same cargo + install
/// plumbing.
struct BuildPlan<'a> {
    /// File-name stem under `splits_dir/builtins/<>.wasm` for the
    /// produced wrapper. Conventionally the WAC-var name.
    out_name: &'a str,
    /// Strategy crate's on-disk root (`Cargo.toml` + `manifest.toml`
    /// + `src/`).
    strategy_dir: &'a Path,
    /// `splicer-tool-sdk` source root. MUST resolve to the SAME
    /// canonical path the strategy crate's own `splicer-tool-sdk`
    /// dep resolves to, or cargo treats the two as distinct crates
    /// and the build fails on type mismatch.
    sdk_dir: &'a Path,
    /// Cargo `[package].name` of the strategy crate — what the
    /// wrapper's `Cargo.toml` references in `[dependencies]`.
    strategy_crate_name: &'a str,
    /// PascalCase Rust ident of the strategy struct the wrapper
    /// instantiates.
    strategy_type: &'a str,
    /// preview1 reactor adapter on disk (used to wrap the core
    /// module into a component).
    adapter_path: &'a Path,
    /// Persistent build root passed through to `BuildConfig`. Cargo
    /// reuses incremental state across calls keyed by this.
    cache_root: &'a Path,
}

/// Run the wrapper-crate codegen + cargo build for `plan`, then
/// install the produced wasm at `splits_dir/builtins/<out_name>.wasm`.
/// All the path resolution is the caller's responsibility — this
/// function just drives the build pipeline.
fn run_codegen_build(
    splits_dir: &Path,
    plan: &BuildPlan<'_>,
    behavior: Behavior,
    target: &TargetWit,
) -> Result<PathBuf> {
    let strategy_path_str = plan.strategy_dir.to_str().with_context(|| {
        format!(
            "strategy path is not UTF-8: {}",
            plan.strategy_dir.display()
        )
    })?;
    let sdk_path_str = plan
        .sdk_dir
        .to_str()
        .with_context(|| format!("sdk path is not UTF-8: {}", plan.sdk_dir.display()))?;

    let input = GenerateWrapperInput {
        target_wit: &target.wit_text,
        world_name: Some(&target.world_name),
        interface_qualified_name: &target.qualified_name,
        behavior,
        strategy_crate_name: plan.strategy_crate_name,
        strategy_crate_path: strategy_path_str,
        strategy_type: plan.strategy_type,
        splicer_tool_sdk_path: sdk_path_str,
    };
    let built = build_wrapper(
        &input,
        &BuildConfig {
            build_root: plan.cache_root,
            adapter_wasm: plan.adapter_path,
            target: None,
        },
    )?;

    let out_dir = splits_dir.join(BUILTIN_SUBDIR);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("could not create {}", out_dir.display()))?;
    let out = out_dir.join(format!("{}.wasm", plan.out_name));
    std::fs::copy(&built, &out).with_context(|| {
        format!(
            "could not copy built wrapper {} -> {}",
            built.display(),
            out.display()
        )
    })?;
    Ok(out)
}

/// Write the embedded preview1 reactor adapter under `cache_root` if
/// it isn't already there, returning the resolved path. Idempotent
/// across splices.
fn ensure_preview1_adapter(cache_root: &Path) -> Result<PathBuf> {
    let path = cache_root.join("wasi_snapshot_preview1.reactor.wasm");
    if !path.exists() {
        std::fs::write(&path, PREVIEW1_ADAPTER)
            .with_context(|| format!("could not write preview1 adapter to {}", path.display()))?;
    }
    Ok(path)
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
    let base = crate::builtins::user_cache_dir().context(
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
    fn is_embedded_builtin_recognizes_embedded_names() {
        assert!(is_embedded_builtin("hello-tier3"));
        assert!(is_embedded_builtin("hello-tier4"));
        assert!(!is_embedded_builtin("hello-tier1"));
        assert!(!is_embedded_builtin("does-not-exist"));
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

    /// Build a minimal user-form strategy directory at `dest`. Used by
    /// the parser tests below and by the `--ignored` end-to-end test.
    /// `sdk_path` is whatever should appear in the strategy's
    /// `splicer-tool-sdk = { path = "..." }` line; tests use either a
    /// real SDK source (extracted via [`extract_sdk`]) or a stub
    /// directory.
    #[cfg(test)]
    fn write_user_strategy_dir(
        dest: &Path,
        crate_name: &str,
        tier: u8,
        sdk_path: &str,
    ) -> std::io::Result<()> {
        std::fs::create_dir_all(dest.join("src"))?;
        std::fs::write(
            dest.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{crate_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n\
                 \n[workspace]\n\n[lib]\n\n[dependencies]\nsplicer-tool-sdk = {{ path = \"{sdk_path}\" }}\n"
            ),
        )?;
        std::fs::write(
            dest.join("manifest.toml"),
            format!("[builtin]\ndescription = \"test strategy\"\ntier = {tier}\n"),
        )?;
        std::fs::write(dest.join("src/lib.rs"), "// stub\n")?;
        Ok(())
    }

    #[test]
    fn is_user_strategy_dir_requires_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("strat");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"s\"\n").unwrap();
        // Cargo.toml alone isn't enough — could be any rust crate.
        assert!(!is_user_strategy_dir(&dir));
        std::fs::write(dir.join("manifest.toml"), "[builtin]\n").unwrap();
        assert!(is_user_strategy_dir(&dir));
        // .wasm path is not a strategy dir.
        let wasm = tmp.path().join("mw.wasm");
        std::fs::write(&wasm, b"\0asm\x0d\0\0\0").unwrap();
        assert!(!is_user_strategy_dir(&wasm));
        // Non-existent path is not a strategy dir.
        assert!(!is_user_strategy_dir(&tmp.path().join("ghost")));
    }

    #[test]
    fn read_user_strategy_metadata_pulls_package_name_and_sdk_path() {
        let tmp = tempfile::tempdir().unwrap();
        // Real SDK so canonicalize() succeeds — the test asserts that
        // the resolved sdk_dir points at the same canonical path.
        let sdk_dir = tmp.path().join("sdk");
        extract_sdk(&sdk_dir).expect("extract sdk");
        let strat = tmp.path().join("strat");
        write_user_strategy_dir(&strat, "my-strategy", 3, "../sdk").unwrap();

        let meta = read_user_strategy_metadata(&strat).expect("read metadata");
        assert_eq!(meta.crate_name, "my-strategy");
        assert_eq!(
            meta.sdk_dir,
            std::fs::canonicalize(&sdk_dir).expect("canonicalize sdk")
        );
    }

    #[test]
    fn read_user_strategy_metadata_errors_without_sdk_path_dep() {
        let tmp = tempfile::tempdir().unwrap();
        let strat = tmp.path().join("strat");
        std::fs::create_dir_all(strat.join("src")).unwrap();
        std::fs::write(
            strat.join("Cargo.toml"),
            // No splicer-tool-sdk dep at all.
            "[package]\nname = \"s\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\n",
        )
        .unwrap();
        std::fs::write(strat.join("manifest.toml"), "[builtin]\ntier = 3\n").unwrap();
        let err = read_user_strategy_metadata(&strat).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("splicer-tool-sdk"),
            "error should name the missing dep: {msg}"
        );
    }

    #[test]
    fn read_user_manifest_classifies_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let strat = tmp.path().join("strat");
        write_user_strategy_dir(&strat, "s", 4, "../sdk").unwrap();
        let manifest = read_user_manifest(&strat).expect("read manifest");
        assert!(matches!(manifest.builtin.tier, Tier::Tier4));
    }

    #[test]
    fn read_user_manifest_errors_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_user_manifest(tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("manifest.toml"));
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
        let (out, tier) = materialize_tier3_4(
            splits.path(),
            Tier3_4Source::Builtin("hello-tier3"),
            &composition,
            "test:demo/ops@0.1.0",
        )
        .expect("materialize");
        assert_eq!(tier, Tier::Tier3);
        assert!(out.ends_with("builtins/hello-tier3.wasm"));
        let bytes = std::fs::read(&out).expect("read");
        assert!(bytes.starts_with(&[0x00, 0x61, 0x73, 0x6d]), "wasm magic");
        let parser = wasmparser::Parser::new(0);
        for payload in parser.parse_all(&bytes) {
            payload.expect("component payload parses");
        }
    }

    /// Mirror of `materialize_tier3_produces_a_component` for the
    /// user-form path: extract the smoke builtin's source to a temp
    /// dir, then drive [`materialize_tier3_4`] against it with a
    /// [`Tier3_4Source::User`] (so the embed codepath never runs).
    /// Validates the parser + codegen pipeline composes correctly
    /// from raw on-disk inputs.
    #[test]
    #[ignore = "shells out to cargo + wasm32-wasip1; run with --ignored"]
    fn materialize_user_tier3_produces_a_component() {
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

        // Stage a fake user strategy crate from the hello-tier3 embed,
        // alongside an extracted SDK at a known relative path.
        let tmp = tempfile::tempdir().unwrap();
        let sdk_dir = tmp.path().join("sdk");
        extract_sdk(&sdk_dir).expect("extract sdk");
        let strat = tmp.path().join("hello-tier3");
        extract("hello-tier3", &strat).expect("extract strategy");
        // Rewrite the strategy's Cargo.toml so its splicer-tool-sdk path
        // resolves under tmp (the embedded `../../splicer-tool-sdk`
        // wouldn't exist here).
        let cargo_text = std::fs::read_to_string(strat.join("Cargo.toml")).unwrap();
        let cargo_text = cargo_text.replace("../../splicer-tool-sdk", "../sdk");
        std::fs::write(strat.join("Cargo.toml"), cargo_text).unwrap();

        let splits = tempfile::tempdir().unwrap();
        let (out, tier) = materialize_tier3_4(
            splits.path(),
            Tier3_4Source::User {
                wac_name: "my-greeter",
                strategy_dir: &strat,
            },
            &composition,
            "test:demo/ops@0.1.0",
        )
        .expect("materialize_tier3_4(user)");
        assert_eq!(tier, Tier::Tier3);
        // Output file name is the YAML name (here `my-greeter`),
        // not the Cargo package name (`hello-tier3`).
        assert!(out.ends_with("builtins/my-greeter.wasm"));
        let bytes = std::fs::read(&out).expect("read");
        assert!(bytes.starts_with(&[0x00, 0x61, 0x73, 0x6d]), "wasm magic");
    }
}
