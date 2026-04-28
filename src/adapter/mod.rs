//! Tier-1 adapter generator: wraps a middleware component with
//! before/after/blocking hooks and re-exports the wrapped handler's
//! target interface.
//!
//! Submodules:
//! - [`emit`] — entry point ([`emit::build_adapter`]) that synthesizes
//!   the adapter world's WIT, builds a dispatch core module, and
//!   hands everything to `wit_component::ComponentEncoder`.
//! - [`mem_layout`] — byte-offset allocator for the dispatch module's
//!   scratch memory.
//! - [`indices`] — index trackers for the dispatch module's type /
//!   function / local namespaces.
//! - [`abi`] — `wit_bindgen_core::abi::Bindgen` impl + a couple of
//!   verbatim wit-bindgen-core helpers needed to drive
//!   `lift_from_memory` from `wasm-encoder`.

use anyhow::Context;

mod abi;
mod emit;
mod indices;
mod mem_layout;
#[cfg(test)]
mod tests;

use emit::build_adapter;

/// WIT/world definitions for the splicer:tier1 hook interfaces.
/// Embedded directly into the generated adapter's WIT so
/// wit-component understands the hook imports.
const TIER1_WORLD_WIT: &str = include_str!("../../wit/tier1/world.wit");

/// Generate a tier-1 adapter component that wraps `middleware_name`
/// and adapts it to export `target_interface`.
///
/// The generated adapter:
/// - Exports `target_interface` (drop-in replacement for the upstream caller).
/// - Imports `target_interface` from the handler-providing component.
/// - Imports the middleware via the tier-1 hook interfaces (the
///   subset matched in `middleware_interfaces`).
/// - For each function in `target_interface`: calls `before-call` →
///   `should-block-call` (early-return when true; void funcs only) →
///   the handler → `after-call`.
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

    let split_bytes = std::fs::read(split_path)
        .with_context(|| format!("Failed to read split at '{split_path}'"))?;
    let bytes = build_adapter(
        target_interface,
        has_before,
        has_after,
        has_blocking,
        &split_bytes,
        TIER1_WORLD_WIT,
    )?;

    let out_path = format!(
        "{splits_output_path}/splicer_adapter_{}_{}.wasm",
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
