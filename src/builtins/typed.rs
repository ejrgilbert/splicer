//! Tier-3/4 builtins: Rust strategy crates embedded into the splicer
//! binary. At splice-time the relevant builtin's source is extracted
//! to a cache dir and passed to codegen as the strategy-crate path.
//!
//! TODO: publish builtins to crates.io and have generated wrappers
//! depend on them by version. cargo handles distribution; this file
//! goes away.

use anyhow::{Context, Result};
use include_dir::{include_dir, Dir};
use std::path::{Path, PathBuf};

/// Each shipped tier-3/4 builtin's source tree. Add an entry when
/// shipping a new builtin.
static EMBEDDED: &[(&str, &Dir<'_>)] = &[
    ("hello-tier3", &HELLO_TIER3),
    ("hello-tier4", &HELLO_TIER4),
];

static HELLO_TIER3: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/builtins/hello-tier3");
static HELLO_TIER4: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/builtins/hello-tier4");

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
pub fn read_manifest(name: &str) -> Result<builtin_manifest::Manifest> {
    let dir = lookup(name)?;
    let file = dir
        .get_file("manifest.toml")
        .with_context(|| format!("embedded tier-3/4 builtin '{name}' has no manifest.toml"))?;
    let text = file
        .contents_utf8()
        .context("manifest.toml is not UTF-8")?;
    toml::from_str(text)
        .with_context(|| format!("failed to parse manifest.toml for '{name}'"))
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

fn lookup(name: &str) -> Result<&'static Dir<'static>> {
    EMBEDDED
        .iter()
        .find_map(|(n, d)| (*n == name).then_some(*d))
        .with_context(|| format!("no embedded tier-3/4 builtin named '{name}'"))
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
        use builtin_manifest::Tier;
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
}
