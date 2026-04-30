//! Tier-2 adapter generator: lifts canonical-ABI values from the
//! target function's parameters/result into the structural cell-array
//! representation defined in `splicer:common/types`, then dispatches
//! the lifted values to the middleware's tier-2 hooks.
//!
//! Status: scaffold + primitives only (Phase 2-2a). Full hook
//! dispatch lands in Phase 2-3; resource/stream/future handle
//! correlation in Phase 2-4.
//!
//! Submodules:
//! - [`cells`] — emit helpers for constructing individual `cell`
//!   variant cases in the canonical-ABI memory layout (one helper
//!   per primitive case so far).

pub(super) mod cells;
pub(super) mod emit;

pub(super) use emit::build_tier2_adapter;
