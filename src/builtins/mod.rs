//! Catalogue of middleware components shipped with splicer.
//!
//! Tier-1/2 builtins are pre-built wasm components fetched from OCI
//! (see [`tier1_2`]). Tier-3/4 builtins ship as Rust strategy crates
//! embedded into the splicer binary (added in a follow-up). Both
//! kinds expose the same shared public API
//! ([`known_names`], [`list_with_manifests`], [`resolve_manifest`]);
//! callers don't need to think about the distribution mechanism.

mod tier1_2;

pub use tier1_2::{known_names, list_with_manifests, materialize_into, resolve_manifest};

pub(crate) use tier1_2::load_resolved_bytes;

#[cfg(test)]
pub(crate) use tier1_2::{with_fake_builtins, FAKE_BUILTIN_WASM};
