//! Tier-2 lift codegen: classify WIT types into cell variants, emit
//! wasm that writes one cell per (param | result), and populate the
//! per-field-tree side tables. See `docs/tiers/lift-codegen.md`.

pub(super) mod classify;
pub(super) mod emit;
pub(super) mod plan;
pub(super) mod sidetable;

pub(super) use classify::{
    classify_func_params, classify_result_lift, InfoCounts, ParamLayout, ParamLift, ResultLayout,
    ResultLift, ResultSource, ResultSourceLayout,
};
pub(super) use emit::{
    alloc_wrapper_locals, emit_lift_compound_prefix, emit_lift_plan, emit_lift_result,
    emit_list_pre_pass, CellSideRefs, FlagsInfoOffsets, HandleInfoOffsets, LiftEmitCtx,
    ListEmitLocals, RecordInfoOffsets, ResultEmitPlan, VariantInfoOffsets, WrapperLocals,
};
pub(super) use plan::{desugar_map_aliases, MapAliases};
pub(super) use sidetable::char_info::{
    build_char_scratch_map, char_scratch_sizes, CharScratchMaps,
};
pub(super) use sidetable::enum_info::build_enum_info_blob;
pub(super) use sidetable::flags_info::{
    build_flags_info_maps, flags_scratch_sizes, FlagsInfoMaps, FlagsRuntimeFill,
};
pub(super) use sidetable::handle_info::{
    build_handle_info_maps, HandleInfoMaps, HandleRuntimeFill,
};
pub(super) use sidetable::record_info::{
    back_fill_record_fields_ptrs, build_record_info_maps, RecordInfoMaps,
};
pub(super) use sidetable::tuple_indices::{build_tuple_indices_blob, TupleIndicesBlob};
pub(super) use sidetable::variant_info::{build_variant_info_maps, VariantInfoMaps};
pub(super) use sidetable::{
    fold_cell_side_data, CellFillSources, CellSideData, CharScratch, SideTableBlob,
};

#[cfg(test)]
mod tests;
