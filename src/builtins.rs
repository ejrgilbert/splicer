//! Registry of middleware components shipped with splicer.
//!
//! Each builtin is a pre-built `.wasm` component embedded into the
//! splicer binary at compile time via `include_bytes!`. The `.wasm`
//! files live under `assets/builtins/` and are produced by
//! `make build-builtins` from the source crates under `builtins/`.
//! `build.rs` auto-discovers every `.wasm` in that dir and emits the
//! registry, so adding a builtin is two steps: drop a crate under
//! `builtins/<name>/` and run `make build-builtins`. The next
//! `cargo build` of splicer picks the new `.wasm` up automatically;
//! no source edits in this file.
//!
//! Builtins are referenced from the splice config YAML as
//! `inject: [{ builtin: <name> }]`. The parser populates
//! [`crate::parse::config::Injection::builtin`] with the name; the
//! splice pipeline then calls [`materialize_into`] before contract
//! validation runs to write the embedded bytes to disk under the
//! splits dir, after which the rest of the pipeline treats the
//! injection like any other path-backed middleware.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// `(name, embedded bytes)` for every shipped builtin. Auto-generated
/// from `assets/builtins/*.wasm` by `build.rs`.
const BUILTINS: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/builtins_registry.rs"));

/// Subdirectory under `splits_dir` where materialized builtins are
/// written. Kept separate from sub-component splits so a `make clean`
/// or rerun doesn't tangle the two.
const BUILTIN_SUBDIR: &str = "builtins";

/// Look up a builtin's embedded bytes by name. `None` if no builtin
/// with that name ships with this splicer build.
pub fn lookup(name: &str) -> Option<&'static [u8]> {
    BUILTINS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, bytes)| *bytes)
}

/// Names of every builtin shipped with this splicer build, sorted.
/// Used to render a helpful error when YAML references an unknown
/// builtin.
pub fn known_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = BUILTINS.iter().map(|(n, _)| *n).collect();
    names.sort();
    names
}

/// Write the named builtin's bytes to `<splits_dir>/builtins/<name>.wasm`
/// and return the resulting absolute path. Idempotent: rewrites the
/// file every call (it's a few KB; not worth a stat-and-skip), so
/// callers can invoke this once per splice without ordering concerns.
///
/// Errors when the name is unknown or the file write fails.
pub fn materialize_into(splits_dir: &Path, name: &str) -> Result<PathBuf> {
    let bytes = lookup(name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown builtin '{name}'. Available: [{}]",
            known_names().join(", ")
        )
    })?;
    let dir = splits_dir.join(BUILTIN_SUBDIR);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create builtins dir: {}", dir.display()))?;
    let out = dir.join(format!("{name}.wasm"));
    std::fs::write(&out, bytes)
        .with_context(|| format!("Failed to write builtin to: {}", out.display()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_tier1_is_registered() {
        let bytes = lookup("hello-tier1").expect("hello-tier1 must be registered");
        assert!(bytes.starts_with(b"\0asm"), "embedded bytes must be wasm");
    }

    #[test]
    fn otel_bare_spans_is_registered() {
        let bytes = lookup("otel-bare-spans").expect("otel-bare-spans must be registered");
        assert!(bytes.starts_with(b"\0asm"), "embedded bytes must be wasm");
    }

    #[test]
    fn otel_metrics_is_registered() {
        let bytes = lookup("otel-metrics").expect("otel-metrics must be registered");
        assert!(bytes.starts_with(b"\0asm"), "embedded bytes must be wasm");
    }

    #[test]
    fn unknown_returns_none() {
        assert!(lookup("does-not-exist").is_none());
    }

    #[test]
    fn known_names_includes_hello_tier1() {
        assert!(known_names().contains(&"hello-tier1"));
    }

    #[test]
    fn materialize_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = materialize_into(tmp.path(), "hello-tier1").unwrap();
        assert!(path.exists());
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"\0asm"));
        assert_eq!(path.parent().unwrap(), tmp.path().join("builtins"));
    }

    #[test]
    fn materialize_unknown_errors_with_available() {
        let tmp = tempfile::tempdir().unwrap();
        let err = materialize_into(tmp.path(), "no-such").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown builtin 'no-such'"));
        assert!(msg.contains("hello-tier1"), "should list available: {msg}");
    }
}
