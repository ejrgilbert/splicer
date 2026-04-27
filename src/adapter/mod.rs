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

use abi::WitBridge;
use build::{build_adapter_bytes, build_adapter_via_wit_component};
use filter::{extract_filtered_sections, find_handler_deps};
use func::extract_adapter_funcs;

/// WIT/world definitions for the splicer:tier1 hook interfaces. The
/// new emit path embeds this directly into the generated adapter's
/// WIT so wit-component understands the hook imports.
const TIER1_WORLD_WIT: &str = include_str!("../../wit/tier1/world.wit");

/// Feature flag: when `SPLICER_NEW_EMIT=1`, route through the WIT-
/// level emit path (`build_adapter_via_wit_component`) for cases the
/// new path supports, falling back to the legacy path otherwise. Off
/// by default during the rewrite so existing tests keep using the
/// known-good legacy path. Removed once the new path reaches feature
/// parity and the legacy path is deleted.
fn new_emit_enabled() -> bool {
    std::env::var("SPLICER_NEW_EMIT").map_or(false, |v| v == "1")
}

/// Strict-mode counterpart: when set, an unsupported case in the new
/// emit path becomes a hard error instead of a silent fallback to
/// legacy. Useful while developing to make sure the new path is
/// actually being exercised on the cases we expect.
fn new_emit_strict() -> bool {
    std::env::var("SPLICER_NEW_EMIT_STRICT").map_or(false, |v| v == "1")
}

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
    let (funcs, layout) = extract_adapter_funcs(target_interface, iface_ty, &bridge)?;

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
    let split_bytes = std::fs::read(split_path)
        .with_context(|| format!("Failed to read split at '{split_path}'"))?;
    let split = extract_filtered_sections(&split_bytes, &deps)?;

    // New emit path: try wit-component if enabled and the case is in
    // scope. The new path bails for unsupported cases (multi-func,
    // resources, async, compounds — the guards live in
    // `build_adapter_via_wit_component`'s `require_simple_case`); when
    // it bails we transparently fall through to the legacy emitter so
    // partial coverage doesn't break existing tests.
    let bytes = if new_emit_enabled() {
        match build_adapter_via_wit_component(
            target_interface,
            has_before,
            has_after,
            has_blocking,
            &split_bytes,
            TIER1_WORLD_WIT,
        ) {
            Ok(b) => b,
            Err(e) => {
                if new_emit_strict() {
                    return Err(e.context(format!(
                        "new emit path bailed for `{target_interface}` (strict mode on)"
                    )));
                }
                tracing::debug!(
                    target = "splicer::adapter::new_emit",
                    "new emit path bailed for `{target_interface}`: {e}; falling back to legacy"
                );
                build_adapter_bytes(
                    target_interface,
                    &funcs,
                    has_before,
                    has_after,
                    has_blocking,
                    arena,
                    iface_ty,
                    &split,
                    layout,
                    &bridge,
                )?
            }
        }
    } else {
        build_adapter_bytes(
            target_interface,
            &funcs,
            has_before,
            has_after,
            has_blocking,
            arena,
            iface_ty,
            &split,
            layout,
            &bridge,
        )?
    };

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
