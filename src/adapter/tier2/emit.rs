//! Tier-2 adapter generator: builds an adapter component that lifts
//! a target function's canonical-ABI parameters into the cell-array
//! representation and forwards them to the middleware's tier-2
//! `on-call` hook before dispatching to the handler.
//!
//! Status: scaffold only (Phase 2-3 first slice). The WIT plumbing
//! (decode split, find target, synthesize adapter world) is in
//! place; the dispatch core module is not yet emitted. Subsequent
//! commits add lift + cell construction + hook invocation.
//!
//! Scope: this slice supports `splicer:tier2/before.on-call` only,
//! for primitive-typed target functions (`bool`, integer widths,
//! `f32` / `f64`, `char`, `string`, `list<u8>`). `on-return`, `on-trap`,
//! and compound types are explicit non-goals here — they land after
//! the e2e primitive slice ships.
//!
//! Design conventions intentionally mirror the tier-1 emit path
//! (`src/adapter/emit.rs`) so reading one informs the other.

use anyhow::{bail, Context, Result};
use wit_component::{ComponentEncoder, StringEncoding};
use wit_parser::{InterfaceId, Resolve};

use super::super::shared::{decode_input_resolve, find_target_interface};

/// Adapter component package + world name. Same convention as tier-1
/// — `wit-component`'s `ComponentEncoder` looks up the world by name
/// to know which import/export wiring to apply.
const TIER2_ADAPTER_WORLD_PACKAGE: &str = "splicer:adapter-tier2";
const TIER2_ADAPTER_WORLD_NAME: &str = "adapter";

/// Generate a tier-2 adapter component that wraps a middleware
/// exporting the `splicer:tier2/before` hook interface and adapts
/// it to interpose on `target_interface`.
///
/// Phase 2-3 scope: primitives only, `on-call` only. Bails cleanly
/// on compound target types or on middleware exporting `after` /
/// `trap` (those land in follow-up slices).
///
/// `split_bytes` is the consumer split the adapter inherits its
/// import preamble from — same role tier-1 uses it for. `common_wit`
/// + `tier2_wit` are the embedded WIT packages so wit-component
/// understands the cell + call-id + hook interface types without
/// network resolution.
pub(crate) fn build_tier2_adapter(
    target_interface: &str,
    has_before: bool,
    split_bytes: &[u8],
    common_wit: &str,
    tier2_wit: &str,
) -> Result<Vec<u8>> {
    if !has_before {
        // The first slice only wires up `before.on-call`. A middleware
        // that exports only `after` or `trap` is valid tier-2 but
        // produces an adapter with nothing to do on entry — defer
        // those configurations to the next slice rather than emit a
        // structurally-degenerate adapter here.
        bail!(
            "tier-2 adapter generation currently requires the middleware to export \
             `splicer:tier2/before` — `after`-only and `trap`-only middleware are \
             planned for a follow-up slice."
        );
    }

    let mut resolve = decode_input_resolve(split_bytes)?;
    let target_iface = find_target_interface(&resolve, target_interface)?;
    require_supported_case(&resolve, target_iface)?;

    resolve
        .push_str("splicer-common.wit", common_wit)
        .context("parse common WIT")?;
    resolve
        .push_str("splicer-tier2.wit", tier2_wit)
        .context("parse tier2 WIT")?;
    let _world_pkg = resolve
        .push_str(
            "splicer-adapter-tier2.wit",
            &synthesize_adapter_world_wit(target_interface),
        )
        .context("parse synthesized tier-2 adapter world WIT")?;

    // TODO(phase 2-3): build_dispatch_module — for each target
    // function, lift params into cells, call on-call, forward to
    // handler, return the handler's result. Not yet emitted; until
    // then fail loudly so callers don't silently get an empty
    // adapter.
    let _ = TIER2_ADAPTER_WORLD_PACKAGE;
    let _ = TIER2_ADAPTER_WORLD_NAME;
    let _ = target_iface;
    let _ = ComponentEncoder::default();
    let _ = StringEncoding::UTF8;
    bail!(
        "tier-2 adapter dispatch module not yet implemented — only WIT \
         plumbing is in place. See Phase 2-3 task in \
         docs/TODO/adapter-comp-planning.md."
    );
}

/// Bail on cases that fail before the lift codegen even runs (empty
/// interfaces, etc.). Out-of-scope **type** shapes — e.g. compound
/// types that the cell emitters don't cover yet — surface as
/// `todo!()` panics inside the lift codegen itself, so each
/// unhandled match arm is the actionable signpost telling the next
/// implementer exactly which case to extend.
fn require_supported_case(resolve: &Resolve, target_iface: InterfaceId) -> Result<()> {
    let iface = &resolve.interfaces[target_iface];
    if iface.functions.is_empty() {
        bail!("interface has no functions");
    }
    Ok(())
}

/// Synthesize the tier-2 adapter world: import + export the target
/// interface and import `splicer:tier2/before` so wit-component
/// wires up the `on-call` hook.
fn synthesize_adapter_world_wit(target_interface: &str) -> String {
    use crate::contract::{versioned_interface, TIER2_BEFORE, TIER2_VERSION};
    let mut wit = format!(
        "package {TIER2_ADAPTER_WORLD_PACKAGE};\n\nworld {TIER2_ADAPTER_WORLD_NAME} {{\n"
    );
    wit.push_str(&format!("    import {target_interface};\n"));
    wit.push_str(&format!("    export {target_interface};\n"));
    wit.push_str(&format!(
        "    import {};\n",
        versioned_interface(TIER2_BEFORE, TIER2_VERSION)
    ));
    wit.push_str("}\n");
    wit
}
