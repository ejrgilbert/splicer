//! Tier-2 lift codegen: classifying WIT types into cell variants,
//! emitting the wasm that writes one cell per (param | result),
//! and laying out the per-field-tree side tables (`enum-infos`,
//! `record-infos`; `flags-infos` / `variant-infos` / `handle-infos`
//! join as their lift codegen lands).
//!
//! See [`docs/tiers/lift-codegen.md`](../../../../docs/tiers/lift-codegen.md)
//! for the cross-tier design (data flow, invariants, why the plan
//! data structure exists).
//!
//! Three layers, one submodule each:
//! - [`plan`] — [`plan::LiftPlanBuilder`] walks a WIT type and emits
//!   a flat [`plan::LiftPlan`] of [`plan::Cell`]s in allocation order
//!   — `cells[0]` is the root, child cells follow their parents. The
//!   plan owns the cell-index space; side-table contributions
//!   reference cells by `Vec`-position into the same plan.
//! - [`classify`] — wraps a plan into per-(param | result) lift
//!   recipes ([`classify::ParamLift`], [`classify::ResultLift`]) plus
//!   the side-table info needed to populate per-tree side tables.
//!   The layout phase wraps these into [`classify::ParamLayout`] /
//!   [`classify::ResultLayout`] once cells-slab + retptr-scratch
//!   offsets are known.
//! - [`sidetable`] — precompute the per-field-tree side tables
//!   (enum-info / record-info) at adapter-build time. Walks the
//!   per-param plans for nominal `Cell` cases.
//! - [`emit`] — walks `plan.cells` and emits one wasm cell per
//!   [`plan::Cell`] via `emit_cell_op`. [`emit::emit_lift_result`]
//!   handles single-cell direct/retptr-pair result lifts;
//!   compound result lifts reuse [`emit::emit_lift_plan`] over
//!   retptr-loaded synth locals.

pub(super) mod classify;
pub(super) mod emit;
pub(super) mod plan;
pub(super) mod sidetable;

// Re-exports for the rest of `tier2::*`. External code keeps doing
// `use super::lift::{ParamLayout, ...}`.
pub(super) use classify::{
    classify_func_params, classify_result_lift, ParamLayout, ParamLift, ResultLayout, ResultLift,
    ResultSource, ResultSourceLayout,
};
pub(super) use emit::{
    alloc_wrapper_locals, emit_lift_compound_prefix, emit_lift_plan, emit_lift_result,
    ResultEmitPlan, WrapperLocals,
};
pub(super) use sidetable::enum_info::{build_enum_info_blob, register_enum_strings};
pub(super) use sidetable::record_info::{
    build_record_info_blob, register_record_strings, RecordInfoBlobs,
};
pub(super) use sidetable::SideTableBlob;

#[cfg(test)]
mod tests;
