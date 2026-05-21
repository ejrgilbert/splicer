//! Tier-3/4 builtin source crates embedded into the splicer binary.
//! Extracted at splice-time and passed to codegen as the
//! strategy-crate path.
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
pub fn embedded_names() -> Vec<&'static str> {
    EMBEDDED.iter().map(|(n, _)| *n).collect()
}

/// Extract the named builtin's source tree into `dest_dir` (created
/// if missing). Idempotent; returns `dest_dir`.
pub fn extract(name: &str, dest_dir: &Path) -> Result<PathBuf> {
    let dir = EMBEDDED
        .iter()
        .find_map(|(n, d)| (*n == name).then_some(*d))
        .with_context(|| format!("no embedded tier-3/4 builtin named '{name}'"))?;
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("could not create {}", dest_dir.display()))?;
    dir.extract(dest_dir)
        .with_context(|| format!("could not extract '{name}' into {}", dest_dir.display()))?;
    Ok(dest_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_names_include_both_smoke_builtins() {
        let names = embedded_names();
        assert!(names.contains(&"hello-tier3"), "got: {names:?}");
        assert!(names.contains(&"hello-tier4"), "got: {names:?}");
    }

    #[test]
    fn extract_writes_manifest_and_lib() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("hello-tier3");
        extract("hello-tier3", &dest).expect("extract succeeds");
        assert!(dest.join("Cargo.toml").exists(), "Cargo.toml not written");
        assert!(dest.join("manifest.toml").exists(), "manifest.toml not written");
        assert!(dest.join("src/lib.rs").exists(), "src/lib.rs not written");
    }

    #[test]
    fn extract_unknown_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = extract("not-a-real-builtin", tmp.path()).unwrap_err();
        assert!(err.to_string().contains("not-a-real-builtin"));
    }

    #[test]
    fn extracted_manifest_declares_a_behavior_field() {
        // Catches a contributor dropping the field before splice
        // time tries to read it.
        let tmp = tempfile::tempdir().unwrap();
        for name in embedded_names() {
            let dest = tmp.path().join(name);
            extract(name, &dest).unwrap();
            let manifest_text = std::fs::read_to_string(dest.join("manifest.toml")).unwrap();
            assert!(
                manifest_text.contains("behavior"),
                "{name}/manifest.toml is missing a `behavior` field"
            );
        }
    }
}
