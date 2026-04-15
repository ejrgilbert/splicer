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
//!   re-encoder used to filter a consumer split's import preamble.
//! - [`func`] — the [`AdapterFunc`] value object and the cviz →
//!   `Vec<AdapterFunc>` extraction.
//! - [`split_imports`] — the legacy verbatim-copy path that ships the
//!   consumer split's whole import preamble (used as a fallback
//!   when the closure walker decides the target isn't in the split).
//! - [`ty`] — canonical-ABI type analysis helpers (flattening,
//!   alignment, resource collection).

use anyhow::Context;
use cviz::model::{InterfaceType, TypeArena};

mod component;
mod dispatch;
mod encoders;
mod filter;
mod func;
mod split_imports;
#[cfg(test)]
mod tests;
mod ty;
use component::build_adapter_bytes;
#[allow(unused_imports)]
use filter::{extract_filtered_sections, find_handler_deps, FilteredSections, HandlerDeps};
use func::extract_adapter_funcs;
use split_imports::{extract_split_imports, SplitImports};

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
    splits_path: &str,
    consumer_split_path: Option<&str>,
    arena: &TypeArena,
) -> anyhow::Result<String> {
    let iface_ty = interface_type.ok_or_else(|| {
        anyhow::anyhow!(
            "Type information for interface '{}' is required to generate a tier-1 adapter \
             but was not available in the composition graph.",
            target_interface
        )
    })?;

    let funcs = extract_adapter_funcs(iface_ty, arena)?;

    let has_before = middleware_interfaces.iter().any(|i| i.contains("/before"));
    let has_after = middleware_interfaces.iter().any(|i| i.contains("/after"));
    let has_blocking = middleware_interfaces
        .iter()
        .any(|i| i.contains("/blocking"));

    // Extract the consumer split's import structure if available.
    //
    // We run the closure-based filter (find_handler_deps + the
    // raw-section reencoder) so that fan-in splits — where the target
    // interface is one of several unrelated imports in the split — only
    // contribute the items the target actually depends on. For chain
    // splits the closure is the entire import preamble, so the filter
    // is a (mostly) no-op pass-through that re-encodes the same items
    // it walked. The reencoded bytes may differ at the LEB128 level
    // from the original verbatim bytes but are semantically identical.
    let split_imports = if let Some(path) = consumer_split_path {
        let deps = find_handler_deps(path, target_interface)?;
        if deps.is_empty() {
            // Target import not present in the split (e.g. the split
            // exports the handler instead of importing it). Fall back
            // to the verbatim-copy path so the existing
            // handler-from-export codepath still has the import
            // metadata it needs.
            Some(extract_split_imports(path)?)
        } else {
            let bytes = std::fs::read(path)
                .with_context(|| format!("Failed to read consumer split at '{}'", path))?;
            Some(SplitImports::from(extract_filtered_sections(
                &bytes, &deps,
            )?))
        }
    } else {
        None
    };

    let bytes = build_adapter_bytes(
        target_interface,
        &funcs,
        has_before,
        has_after,
        has_blocking,
        arena,
        iface_ty,
        split_imports.as_ref(),
    )?;

    let out_path = format!(
        "{splits_path}/splicer_adapter_{}_{}.wasm",
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
