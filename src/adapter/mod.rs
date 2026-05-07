//! Splicer's adapter generators. Each tier lives in its own
//! submodule; this top-level file is just the public-API entry
//! points (`generate_tier1_adapter`, `generate_tier2_adapter`) and
//! the embedded WIT package strings consumed by both.
//!
//! Submodules:
//! - [`tier1`] — name-only-hooks adapter (currently shipped).
//! - [`tier2`] — observation-hooks adapter (in progress, Phase 2-3).
//! - [`abi`] — canonical-ABI infrastructure (Bindgen impl, verbatim
//!   wit-bindgen-core helpers, wasm-encoder emit helpers).
//! - [`resolve`] — split decode + target-interface lookup.
//! - [`indices`] — index trackers for dispatch-module type / function
//!   / local namespaces.
//! - [`mem_layout`] — byte-offset allocator for the dispatch module's
//!   scratch memory.

use anyhow::Context;

mod abi;
mod indices;
mod mem_layout;
mod resolve;
mod tier1;
mod tier2;

use tier1::build_adapter;
use tier2::build_tier2_adapter;

/// WIT/world definitions for the splicer:tier1 hook interfaces.
/// Embedded directly into the generated adapter's WIT so
/// wit-component understands the hook imports.
const TIER1_WORLD_WIT: &str = include_str!("../../wit/tier1/world.wit");

/// WIT/world definitions for the splicer:tier2 hook interfaces.
/// Loaded alongside `COMMON_WORLD_WIT` when generating tier-2
/// adapters.
const TIER2_WORLD_WIT: &str = include_str!("../../wit/tier2/world.wit");

/// Shared types referenced by every tier's WIT (`call-id`, `cell`,
/// `field-tree`, side-table records). Loaded into the resolve
/// before any tier WIT so the `use splicer:common/types.{...};`
/// clauses inside each tier resolve.
const COMMON_WORLD_WIT: &str = include_str!("../../wit/common/world.wit");

/// Generate a tier-1 adapter component that wraps `middleware_name`
/// and adapts it to export `target_interface`.
///
/// The generated adapter:
/// - Exports `target_interface` (drop-in replacement for the upstream caller).
/// - Imports `target_interface` from the handler-providing component.
/// - Imports the middleware via the tier-1 hook interfaces (the
///   subset matched in `middleware_interfaces`).
/// - For each function in `target_interface`: calls `on-call` →
///   `should-block` (early-return when true; void funcs only) →
///   the handler → `on-return`.
///
/// Returns the path to the generated `.wasm`.
pub fn generate_tier1_adapter(
    middleware_name: &str,
    target_interface: &str,
    middleware_interfaces: &[String],
    splits_output_path: &str,
    split_path: &str,
) -> anyhow::Result<String> {
    let has_before = middleware_interfaces.iter().any(|i| i.contains("/before"));
    let has_after = middleware_interfaces.iter().any(|i| i.contains("/after"));
    let has_blocking = middleware_interfaces
        .iter()
        .any(|i| i.contains("/blocking"));

    write_adapter(
        middleware_name,
        target_interface,
        splits_output_path,
        split_path,
        "splicer_adapter",
        |split_bytes| {
            build_adapter(
                target_interface,
                has_before,
                has_after,
                has_blocking,
                split_bytes,
                COMMON_WORLD_WIT,
                TIER1_WORLD_WIT,
            )
        },
    )
}

/// Generate a tier-2 adapter component. Mirrors
/// [`generate_tier1_adapter`] in shape — different hook interface
/// package, different lift codegen.
///
/// Phase 2-3 scope: middleware must export `splicer:tier2/before`;
/// targets are restricted to primitive-typed parameters and result.
/// Bails cleanly on out-of-scope cases until subsequent slices land.
pub fn generate_tier2_adapter(
    middleware_name: &str,
    target_interface: &str,
    middleware_interfaces: &[String],
    splits_output_path: &str,
    split_path: &str,
) -> anyhow::Result<String> {
    let has_before = middleware_interfaces.iter().any(|i| i.contains("/before"));
    let has_after = middleware_interfaces.iter().any(|i| i.contains("/after"));

    write_adapter(
        middleware_name,
        target_interface,
        splits_output_path,
        split_path,
        "splicer_adapter_tier2",
        |split_bytes| {
            build_tier2_adapter(
                target_interface,
                has_before,
                has_after,
                split_bytes,
                COMMON_WORLD_WIT,
                TIER2_WORLD_WIT,
            )
        },
    )
}

/// Shared "read split bytes → call tier-specific bytes builder →
/// write adapter `.wasm`" plumbing for the public `generate_*` entry
/// points. Each tier's wrapper supplies the bytes builder and the
/// filename prefix; everything else (path resolution, I/O,
/// sanitization) is the same.
fn write_adapter(
    middleware_name: &str,
    target_interface: &str,
    splits_output_path: &str,
    split_path: &str,
    out_name_prefix: &str,
    build_bytes: impl FnOnce(&[u8]) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<String> {
    let split_bytes = std::fs::read(split_path)
        .with_context(|| format!("Failed to read split at '{split_path}'"))?;
    let bytes = build_bytes(&split_bytes)?;
    let out_path = format!(
        "{splits_output_path}/{out_name_prefix}_{}_{}.wasm",
        sanitize_name(middleware_name),
        sanitize_name(target_interface)
    );
    std::fs::write(&out_path, &bytes)
        .with_context(|| format!("Failed to write adapter component to '{}'", out_path))?;
    Ok(out_path)
}

fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}
