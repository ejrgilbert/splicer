//! Catalogue of middleware components shipped with splicer.
//!
//! Tier-1/2 builtins are pre-built wasm components fetched from OCI
//! (see [`tier1_2`]). Tier-3/4 builtins are Rust strategy crates
//! embedded into the splicer binary (see [`typed`]). The public API
//! ([`known_names`], [`list_with_manifests`], [`resolve_manifest`])
//! unifies both — callers don't think about distribution mechanics.

use anyhow::Result;
use builtin_manifest::Tier;
use std::path::{Path, PathBuf};

mod tier1_2;
mod typed;

pub use tier1_2::materialize_into;

pub(crate) use tier1_2::load_resolved_bytes;

#[cfg(test)]
pub(crate) use tier1_2::{with_fake_builtins, FAKE_BUILTIN_WASM};

/// Names of every user-facing builtin shipped with this splicer
/// build, sorted. Spans both tier-1/2 (OCI-distributed wasm) and
/// tier-3/4 (embedded source crates).
pub fn known_names() -> Vec<&'static str> {
    let mut all: Vec<&'static str> = tier1_2::known_names();
    all.extend(typed::names());
    all.sort();
    all.dedup();
    all
}

/// Resolve every user-facing builtin's manifest and return the pairs
/// in [`known_names`] order. Manifests are `Option` because some
/// tier-1/2 builtins predate the manifest substrate.
///
/// Resolution errors land as `Err(...)` rather than panicking so the
/// caller can render partial output — `splicer builtin` shouldn't
/// crash when one OCI pull misbehaves.
pub fn list_with_manifests() -> Vec<(&'static str, Result<Option<builtin_manifest::Manifest>>)> {
    let mut out = Vec::new();
    for name in known_names() {
        let entry = if typed::is_typed(name) {
            typed::read_manifest(name).map(Some)
        } else {
            tier1_2::manifest_for(name)
        };
        out.push((name, entry));
    }
    out
}

/// Resolve a single builtin's manifest. For tier-3/4 the manifest is
/// always present (every shipped strategy ships one); for tier-1/2 a
/// missing manifest is an error.
pub fn resolve_manifest(name: &str) -> Result<builtin_manifest::Manifest> {
    if typed::is_typed(name) {
        typed::read_manifest(name)
    } else {
        tier1_2::resolve_manifest(name)
    }
}

/// Resolve a builtin to a wasm component at
/// `splits_dir/builtins/<name>.wasm`, picking the distribution path
/// from its manifest tier: tier-1/2 pull pre-built bytes via
/// [`materialize_into`]; tier-3/4 extract the embedded strategy
/// source and run the codegen + cargo build pipeline. Either way the
/// returned absolute path is what the rest of the splice pipeline
/// stamps onto the injection.
pub fn materialize(splits_dir: &Path, name: &str) -> Result<PathBuf> {
    if typed::is_typed(name) {
        match typed::read_manifest(name)?.builtin.tier {
            Tier::Tier3 | Tier::Tier4 => typed::materialize(splits_dir, name),
            other => anyhow::bail!(
                "builtin '{name}' is registered under the tier-3/4 embed but its manifest \
                 declares tier {other:?}"
            ),
        }
    } else {
        materialize_into(splits_dir, name)
    }
}
