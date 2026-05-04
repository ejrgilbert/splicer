//! Tier-2 adapter generator: lifts canonical-ABI values from the
//! target function's parameters/result into the structural cell-array
//! representation defined in `splicer:common/types`, then dispatches
//! the lifted values to the middleware's tier-2 hooks.
//!
//! Status: scaffold + primitives only. Compound kinds, full hook
//! dispatch, and resource/stream/future handle correlation are
//! tracked in `docs/tiers/lift-codegen.md`.
//!
//! Submodules:
//! - [`blob`] — typed data-segment packing helpers (`BlobSlice`,
//!   `RecordWriter`); the data-side analogue of [`cells::CellLayout`].
//! - [`cells`] — emit helpers for constructing individual `cell`
//!   variant cases in the canonical-ABI memory layout (one helper
//!   per primitive case so far).
//! - [`lift`] — lift classification (`LiftKind`), per-(param|result)
//!   lift descriptors, side-table population, and the wasm-encoder
//!   codegen that writes one cell per lifted value.
//! - [`emit`] — dispatch-module orchestration: schema layouts,
//!   wrapper-body emission, section emitters.

pub(super) mod blob;
pub(super) mod cells;
pub(super) mod emit;
pub(super) mod lift;

pub(super) use emit::build_tier2_adapter;
