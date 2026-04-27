//! Tier-1 adapter generator: wraps a middleware component with
//! before/after/blocking hooks and re-exports the wrapped handler's
//! target interface.
//!
//! The module is organized into two layered groups plus a handful of
//! cross-cutting root files:
//!
//! - [`abi`] — canonical-ABI abstraction: type translation to
//!   `wit-parser`, `Bindgen` impl for lift/lower codegen, verbatim
//!   copies of a couple of private `wit-bindgen-core` helpers.
//! - [`build`] — wasm binary emission: outer Component orchestration,
//!   inner dispatch core module, component-level type encoders, and
//!   the byte-offset allocator for the dispatch module's scratch
//!   memory.
//! - [`filter`] — closure-based dependency walker + raw-sections
//!   re-encoder that scopes the split's import preamble to exactly the
//!   sections the target interface transitively depends on.
//! - [`func`] — the [`func::AdapterFunc`] value object and the cviz
//!   → `Vec<AdapterFunc>` extraction.
//! - [`indices`] — running-index trackers for component, core-module,
//!   and function-local namespaces.
//! - [`names`] — stable import/export name strings.

use anyhow::Context;
use cviz::model::{InterfaceType, TypeArena};

mod abi;
mod build;
mod filter;
mod func;
mod indices;
mod names;
#[cfg(test)]
mod tests;

use build::build_adapter_via_wit_component;

/// WIT/world definitions for the splicer:tier1 hook interfaces.
/// Embedded directly into the generated adapter's WIT so
/// wit-component understands the hook imports.
const TIER1_WORLD_WIT: &str = include_str!("../../wit/tier1/world.wit");

/// Generate a tier-1 adapter component that wraps `middleware_name` and adapts it to
/// export `target_interface`.
///
/// The generated adapter component:
/// - Exports `target_interface` (making it a drop-in replacement for the upstream caller)
/// - Imports `target_interface` from the handler-providing component
/// - Imports the middleware via the tier-1 type-erased interface(s)
/// - For each function in `target_interface`:
///   1. Calls `before-call(fn_name)` if the middleware exports it
///   2. Calls `should-block-call(fn_name)` if the middleware exports it; skips
///      the handler invocation when it returns `true` (void functions only)
///   3. Forwards the call to the handler (unless blocked)
///   4. Calls `after-call(fn_name)` if the middleware exports it
///
/// Returns the path to the generated adapter `.wasm` file.
#[allow(clippy::too_many_arguments)]
pub fn generate_tier1_adapter(
    middleware_name: &str,
    target_interface: &str,
    middleware_interfaces: &[String],
    _interface_type: Option<&InterfaceType>,
    splits_output_path: &str,
    split_path: &str,
    _arena: &TypeArena,
) -> anyhow::Result<String> {
    let has_before = middleware_interfaces.iter().any(|i| i.contains("/before"));
    let has_after = middleware_interfaces.iter().any(|i| i.contains("/after"));
    let has_blocking = middleware_interfaces
        .iter()
        .any(|i| i.contains("/blocking"));

    let split_bytes = std::fs::read(split_path)
        .with_context(|| format!("Failed to read split at '{split_path}'"))?;
    let bytes = build_adapter_via_wit_component(
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
