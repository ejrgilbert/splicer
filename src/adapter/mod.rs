//! Tier-1 adapter generator: wraps a middleware component with
//! before/after/blocking hooks and re-exports the wrapped handler's
//! target interface.
//!
//! Submodules:
//! - [`component`] — orchestrates the full Component binary build
//!   ([`build_adapter_bytes`]) across the 13 numbered phases.
//! - [`dispatch`] — emits the two nested core-Wasm modules: the
//!   memory provider and the per-function dispatch wrappers.
//! - [`encoders`] — recursive value-type encoders for component-level
//!   and instance-level type sections.
//! - [`filter`] — closure-based dependency walker + raw-sections
//!   re-encoder that scopes the split's import preamble to exactly the
//!   sections the target interface transitively depends on; produces
//!   the [`filter::FilteredSections`] that the adapter builder consumes.
//! - [`func`] — the [`AdapterFunc`] value object and the cviz →
//!   `Vec<AdapterFunc>` extraction.
//! - [`mem_layout`] — [`mem_layout::MemoryLayoutBuilder`], the single
//!   byte-offset allocator shared by extraction and build phases.
//! - [`ty`] — canonical-ABI type analysis helpers (flattening,
//!   alignment, resource collection).

use anyhow::Context;
use cviz::model::{InterfaceType, TypeArena};

mod bindgen;
mod bindgen_compat;
mod component;
mod dispatch;
mod encoders;
mod filter;
mod func;
mod indices;
mod mem_layout;
mod names;
#[cfg(test)]
mod tests;
mod ty;
mod wit_bridge;
use component::build_adapter_bytes;
use filter::{extract_filtered_sections, find_handler_deps};
use func::extract_adapter_funcs;
use wit_bridge::WitBridge;

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
    interface_type: Option<&InterfaceType>,
    splits_output_path: &str,
    split_path: &str,
    arena: &TypeArena,
) -> anyhow::Result<String> {
    let iface_ty = interface_type.ok_or_else(|| {
        anyhow::anyhow!(
            "Type information for interface '{}' is required to generate a tier-1 adapter \
             but was not available in the composition graph.",
            target_interface
        )
    })?;

    let bridge = WitBridge::from_cviz(arena);
    let (funcs, layout) = extract_adapter_funcs(iface_ty, &bridge)?;

    let has_before = middleware_interfaces.iter().any(|i| i.contains("/before"));
    let has_after = middleware_interfaces.iter().any(|i| i.contains("/after"));
    let has_blocking = middleware_interfaces
        .iter()
        .any(|i| i.contains("/blocking"));

    // Compute the dependency closure of the target interface in the
    // split. `find_handler_deps` walks the split and finds the target
    // as either an **import** (consumer split — common case) or an
    // **export** (provider split — outermost chain position). Either
    // way, BFS from the target's loc yields the same shape of result:
    // the set of preamble type/import/alias items the target
    // transitively depends on. The only observable difference is that
    // the provider split's filtered output won't include the handler
    // as an instance import (it wasn't one), and the adapter builder
    // will synthesize a fresh handler import type referencing the
    // preamble's types.
    let deps = find_handler_deps(split_path, target_interface)?;
    if deps.not_found() {
        anyhow::bail!(
            "Split at '{}' neither imports nor exports interface '{}'. \
             Please open an issue with a repro at https://github.com/ejrgilbert/splicer/issues",
            split_path,
            target_interface
        );
    }
    let bytes = std::fs::read(split_path)
        .with_context(|| format!("Failed to read split at '{split_path}'"))?;
    let split = extract_filtered_sections(&bytes, &deps)?;

    let bytes = build_adapter_bytes(
        target_interface,
        &funcs,
        has_before,
        has_after,
        has_blocking,
        arena,
        iface_ty,
        &split,
        layout,
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
