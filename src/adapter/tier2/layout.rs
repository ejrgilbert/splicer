//! Static-memory layout phase: takes the [`FuncClassified`] list
//! produced by classification, reserves data + scratch slabs for
//! every blob the wrapper body references at runtime, and assembles
//! immutable [`FuncDispatch`] records combining each
//! [`FuncClassified`] with its computed offsets.
//!
//! Phase boundary: [`lay_out_static_memory`] takes ownership of the
//! classify output and returns a fully-built dispatch list. There's
//! no halfway state where some [`FuncDispatch`] fields are
//! placeholders waiting for a later phase to back-fill them.

use anyhow::{bail, Result};
use wit_parser::Function as WitFunction;

use super::super::abi::emit::BlobSlice;
use super::super::mem_layout::StaticLayout;
use super::blob::{resolve, NameInterner, RecordWriter, RelocPlan, Segment, SymRef, SymbolBases};
use super::lift::plan::Cell;
use super::lift::{
    back_fill_flags_len_addrs, back_fill_handle_id_addrs, back_fill_variant_entry_addrs,
    build_char_scratch_map, build_enum_info_blob, build_flags_info_blob, build_handle_info_blob,
    build_record_info_blob, build_tuple_indices_blob, build_variant_info_blob, char_scratch_sizes,
    flags_scratch_sizes, fold_cell_side_data, register_enum_strings, register_flags_strings,
    register_variant_strings, CellFillSources, CellSideData, CharScratchMaps, FlagsInfoBlobs,
    FlagsRuntimeFill, HandleInfoBlobs, ParamLayout, RecordInfoBlobs, ResultLayout, ResultLift,
    ResultSource, ResultSourceLayout, SideTableBlob, TupleIndicesBlob, VariantInfoBlobs,
};
use super::schema::{
    SchemaLayouts, FIELD_NAME, FIELD_TREE, ON_RET_CALL, ON_RET_RESULT, TREE_CELLS, TREE_ENUM_INFOS,
    TREE_FLAGS_INFOS, TREE_HANDLE_INFOS, TREE_RECORD_INFOS, TREE_ROOT, TREE_VARIANT_INFOS,
};
use super::{AfterSetup, FuncClassified, FuncDispatch};

// ─── ABI-anchored constants (not WIT-schema-derivable) ────────────

/// Size + alignment of the `waitable-set.wait` event record slot.
/// This is wit-component runtime ABI, not anything from our WIT.
const EVENT_SLOT_SIZE: u32 = 8;
const EVENT_SLOT_ALIGN: u32 = 4;

// ─── Layout-phase size budget ─────────────────────────────────────
//
// Wasm encodes static-data offsets as `i32.const` in the instruction
// stream, so the layout phase has to keep every offset in signed-i32
// range. One pre-check at the top of `lay_out_static_memory` bounds
// every per-fn / per-param count that downstream `count * size`
// arithmetic multiplies; one post-check at the end verifies the
// final layout end. No per-site checked arithmetic needed.

/// Final layout end (data + scratch + bump-allocator base) must fit
/// in a signed i32.
const LAYOUT_SIZE_BUDGET: u32 = i32::MAX as u32;

/// Per-fn flat-slot count cap. Canonical-ABI direct-call flattens up
/// to 16 args before retptr; nested-record flatten can go past that.
/// 65 536 sits well above any realistic shape. Reduced under
/// `cfg(test)` so the bail can be exercised without generating a WIT
/// at the production limit.
#[cfg(not(test))]
const MAX_FLAT_SLOTS_PER_FN: u32 = 1 << 16;
#[cfg(test)]
const MAX_FLAT_SLOTS_PER_FN: u32 = 16;

/// Per-param (and per-result) cell-tree cap. Bounds `cell_count *
/// cell_size` slab sizes and the cell index used as `i32.const` in
/// `emit_lift_plan`. Reduced under `cfg(test)` so the bail can be
/// exercised without generating a WIT at the production limit.
#[cfg(not(test))]
const MAX_CELLS_PER_PARAM: u32 = 1 << 20;
#[cfg(test)]
const MAX_CELLS_PER_PARAM: u32 = 8;

/// Bound every per-fn / per-param count the layout phase relies on.
/// Once this returns `Ok`, the body of `lay_out_static_memory` can
/// use ordinary `u32` arithmetic; the schema-derived `cell_size` /
/// `field_size` factors are small constants, so the products fit.
fn check_layout_budget(per_func: &[FuncClassified]) -> Result<()> {
    for (fn_idx, fd) in per_func.iter().enumerate() {
        for (p_idx, p) in fd.params.iter().enumerate() {
            if p.plan.flat_slot_count > MAX_FLAT_SLOTS_PER_FN {
                bail!(
                    "fn[{fn_idx}] param[{p_idx}]: flat-slot count {} exceeds budget {MAX_FLAT_SLOTS_PER_FN}",
                    p.plan.flat_slot_count,
                );
            }
            if p.plan.cell_count() > MAX_CELLS_PER_PARAM {
                bail!(
                    "fn[{fn_idx}] param[{p_idx}]: cell count {} exceeds budget {MAX_CELLS_PER_PARAM}",
                    p.plan.cell_count(),
                );
            }
        }
        if let Some(rl) = fd.result_lift.as_ref() {
            let cells = rl.compound().map_or(1, |c| c.plan.cell_count());
            if cells > MAX_CELLS_PER_PARAM {
                bail!(
                    "fn[{fn_idx}] result: cell count {cells} exceeds budget {MAX_CELLS_PER_PARAM}"
                );
            }
        }
    }
    Ok(())
}

/// Wrap the per-fn fills into a `CellSideData` for a single-cell
/// (Direct / RetptrPair) result. Each kind that needs runtime
/// bookkeeping plumbs its per-fn `Option<…>` here; primitives without
/// side data fall through to `None`. New kinds add a match arm + a
/// fill argument.
fn single_cell_side_data(
    cell: &Cell,
    flags_fill: &Option<FlagsRuntimeFill>,
    char_scratch: &Option<i32>,
) -> CellSideData {
    match cell {
        Cell::Flags { .. } => {
            CellSideData::Flags(Box::new(flags_fill.clone().expect(
                "flags single-cell result → flags-info builder must produce a fill",
            )))
        }
        Cell::Char { .. } => CellSideData::Char {
            scratch_addr: char_scratch
                .expect("char single-cell result → char-info builder must produce a scratch addr"),
        },
        _ => CellSideData::None,
    }
}

/// Output of the static-memory layout phase: the addresses the
/// emit-code phase needs to reference, plus the data segments
/// ready to feed `emit_data_section`.
pub(super) struct StaticDataPlan {
    pub(super) bump_start: u32,
    pub(super) event_ptr: i32,
    /// Byte offset of the on-call indirect-params scratch buffer.
    /// `Some` iff `schema.before_hook` is wired (the buffer only
    /// exists to be passed to the before-hook).
    pub(super) hook_params_ptr: Option<u32>,
    pub(super) data_segments: Vec<(u32, Vec<u8>)>,
}

/// Side-table absolute pointers for one field-tree. Each kind's
/// `BlobSlice` patches into the matching `field-tree.<kind>-infos`
/// list pair; `BlobSlice::EMPTY` leaves the slot zeroed (i.e. the
/// field doesn't carry that kind). Adding a new kind means adding
/// a field here + a [`FieldSideTables::write_to_tree`] line.
#[derive(Clone, Copy, Default)]
struct FieldSideTables {
    enum_infos: BlobSlice,
    flags_infos: BlobSlice,
    record_infos: BlobSlice,
    variant_infos: BlobSlice,
    handle_infos: BlobSlice,
}

impl FieldSideTables {
    fn write_to_tree(&self, blob: &mut [u8], tree: &RecordWriter) {
        tree.write_slice(blob, TREE_ENUM_INFOS, self.enum_infos);
        tree.write_slice(blob, TREE_FLAGS_INFOS, self.flags_infos);
        tree.write_slice(blob, TREE_RECORD_INFOS, self.record_infos);
        tree.write_slice(blob, TREE_VARIANT_INFOS, self.variant_infos);
        tree.write_slice(blob, TREE_HANDLE_INFOS, self.handle_infos);
    }
}

/// Single-pass build of a `field` record + its embedded
/// `field-tree` for one (function, param) pair. `cells` points at
/// the param's contiguous cells slab (`(slab-offset, cell-count)`),
/// and `root` is the cell-array index the field-tree should walk
/// from — sourced from the param's [`super::lift::ParamLift::plan`]'s
/// [`super::lift::plan::LiftPlan::root`]. `side_tables` patches the
/// field-tree's per-kind-infos lists for any kinds the param's plan
/// carries.
fn write_field_record(
    blob: &mut Vec<u8>,
    schema: &SchemaLayouts,
    cells: BlobSlice,
    root: u32,
    name: BlobSlice,
    side_tables: FieldSideTables,
) {
    let field = RecordWriter::extend_zero(blob, &schema.field_layout);
    field.write_slice(blob, FIELD_NAME, name);
    let tree = field.nested(FIELD_TREE, &schema.tree_layout);
    tree.write_slice(blob, TREE_CELLS, cells);
    side_tables.write_to_tree(blob, &tree);
    tree.write_i32(blob, TREE_ROOT, root as i32);
}

/// Build the contiguous fields blob: one `field` record per
/// (fn, param). `cells_offsets[fn_idx][param_idx]` is the byte
/// offset of the param's contiguous cells slab; `param_side_tables`
/// is parallel and carries the param's per-kind side-table pointers
/// (or `EMPTY` slots for kinds the param doesn't carry).
fn build_fields_blob(
    per_func: &[FuncClassified],
    schema: &SchemaLayouts,
    cells_offsets: &[Vec<u32>],
    param_side_tables: &[Vec<FieldSideTables>],
) -> Vec<u8> {
    let mut blob: Vec<u8> = Vec::new();
    for (fn_idx, fd) in per_func.iter().enumerate() {
        for (i, p) in fd.params.iter().enumerate() {
            // The field-tree's `cells.ptr` points at the param's
            // slab; `cells.len = plan.cell_count()`; `root` is the
            // index recorded by the plan-builder (children-first
            // ordering means root is the last-pushed cell for
            // compound shapes, `0` for primitives).
            write_field_record(
                &mut blob,
                schema,
                BlobSlice {
                    off: cells_offsets[fn_idx][i],
                    len: p.plan.cell_count(),
                },
                p.plan.root(),
                p.name,
                param_side_tables[fn_idx][i],
            );
        }
    }
    blob
}

/// Build the contiguous on-return params blob: one record per fn,
/// with `result: option::some(field-tree)` pre-wired for funcs that
/// have a result lift, `option::none` for the rest.
/// `result_cells_offsets[fn_idx]` is the byte offset of that fn's
/// 1-cell (Direct/RetptrPair) or N-cell (Compound) result slab;
/// `None` when the after-hook isn't relevant for this fn.
fn build_after_params_blob(
    per_func: &[FuncClassified],
    schema: &SchemaLayouts,
    iface_name: BlobSlice,
    result_cells_offsets: &[Option<u32>],
    result_side_tables: &[FieldSideTables],
) -> Vec<u8> {
    let Some(after_layout) = schema.after_hook.as_ref().map(|h| &h.params_layout) else {
        return Vec::new();
    };
    let mut blob: Vec<u8> = Vec::new();
    for (fn_idx, fd) in per_func.iter().enumerate() {
        let entry = RecordWriter::extend_zero(&mut blob, after_layout);
        schema.callid_layout.store_names_in_blob(
            &mut blob,
            entry.field_offset(ON_RET_CALL),
            iface_name,
            BlobSlice {
                off: fd.fn_name_offset as u32,
                len: fd.fn_name_len as u32,
            },
        );
        match result_cells_offsets[fn_idx] {
            Some(cells_off) => {
                entry.write_option_some(&mut blob, ON_RET_RESULT);
                let tree_base =
                    entry.field_offset(ON_RET_RESULT) + schema.option_payload_off as usize;
                let tree = RecordWriter::at(&schema.tree_layout, tree_base);
                // Compound result: cells.len = plan.cell_count (slab
                // holds the full cell tree) and root = plan.root()
                // (children-first ordering puts the parent at the
                // end of the slab). Single-cell result (Direct /
                // RetptrPair): len = 1, root = 0.
                let (cells_len, root) = fd
                    .result_lift
                    .as_ref()
                    .and_then(|rl| rl.compound())
                    .map_or((1, 0), |c| (c.plan.cell_count(), c.plan.root()));
                tree.write_slice(
                    &mut blob,
                    TREE_CELLS,
                    BlobSlice {
                        off: cells_off,
                        len: cells_len,
                    },
                );
                result_side_tables[fn_idx].write_to_tree(&mut blob, &tree);
                tree.write_i32(&mut blob, TREE_ROOT, root as i32);
            }
            None => entry.write_option_none(&mut blob, ON_RET_RESULT),
        }
    }
    blob
}

/// Place one segment, register its symbol, queue its cross-segment
/// relocs (no-op when `seg.relocs` is empty). Returns the placed base.
fn place_segment(
    layout: &mut StaticLayout,
    symbols: &mut SymbolBases,
    relocs: &mut RelocPlan,
    seg: Segment,
) -> u32 {
    let (base, idx) = layout.place_data(seg.align, &seg.bytes);
    symbols.set(seg.id, base);
    relocs.record_segment(idx, base, seg.relocs);
    base
}

/// Resolve a per-(fn, param) and per-fn pair of `SymRef` grids to
/// absolute `BlobSlice` grids. Mechanical mapping shared by every
/// per-cell side-table kind whose ranges land in `FieldSideTables`.
fn resolve_param_result_ranges(
    symbols: &SymbolBases,
    per_param_sym: Vec<Vec<Option<SymRef>>>,
    per_result_sym: Vec<Option<SymRef>>,
) -> (Vec<Vec<BlobSlice>>, Vec<BlobSlice>) {
    let per_param = per_param_sym
        .into_iter()
        .map(|v| v.into_iter().map(|s| resolve(s, symbols)).collect())
        .collect();
    let per_result = per_result_sym
        .into_iter()
        .map(|s| resolve(s, symbols))
        .collect();
    (per_param, per_result)
}

/// Reserve scratch + place data segments for everything the wrapper
/// body references at runtime, then assemble immutable
/// [`FuncDispatch`] records combining each [`FuncClassified`] with
/// its computed offsets. Each allocation goes through `StaticLayout`
/// so alignment is enforced.
///
/// Phase boundary: this fn takes ownership of the classify output
/// and returns a fully-built dispatch list. There's no halfway state
/// where some FuncDispatch fields are placeholders waiting for a
/// later phase to back-fill them. Reordering the steps inside this
/// fn can still produce wrong offsets, but it can't leave the type
/// system holding a `: 0  // back-filled later` lie.
pub(super) fn lay_out_static_memory(
    per_func: Vec<FuncClassified>,
    funcs: &[&WitFunction],
    schema: &SchemaLayouts,
    mut names: NameInterner,
    iface_name: BlobSlice,
) -> Result<(Vec<FuncDispatch>, StaticDataPlan)> {
    let n_funcs = per_func.len();

    check_layout_budget(&per_func)?;

    // Side-table strings get appended to the name interner BEFORE we
    // place it — every side-table-info entry references these string
    // offsets, so they have to land in the data segment first.
    // Record-info strings are already interned at plan-build time;
    // enum-info and flags-info both need a registration pass here.
    let enum_strings = register_enum_strings(&per_func, &mut names);
    let flags_strings = register_flags_strings(&per_func, &mut names);
    let variant_strings = register_variant_strings(&per_func, &mut names);

    let mut layout = StaticLayout::new();
    let mut symbols = SymbolBases::new();
    let mut relocs = RelocPlan::new();

    let name_blob = names.into_bytes();
    let _ = layout.place_data(1, &name_blob);

    // Cells slabs first — fields records embed pointers to these.
    // Each param contributes `plan.cell_count() * cell_size` bytes;
    // record params produce >1 cell, so per-param offsets get
    // recorded individually.
    let cells_offsets: Vec<Vec<u32>> = per_func
        .iter()
        .map(|fd| {
            fd.params
                .iter()
                .map(|p| {
                    let slab_size = p.plan.cell_count() * schema.cell_layout.size;
                    layout.reserve_scratch(schema.cell_layout.align, slab_size)
                })
                .collect()
        })
        .collect();
    // Per-fn result-cell scratch, when after-hook is wired and the
    // function has a result to lift. Compound results need
    // `plan.cell_count() * cell_size` bytes; primitive single-cell
    // results need just one cell.
    let result_cells_offsets: Vec<Option<u32>> = if schema.after_hook.is_some() {
        per_func
            .iter()
            .map(|fd| {
                fd.result_lift.as_ref().map(|rl| {
                    let cells = rl.compound().map_or(1, |c| c.plan.cell_count());
                    layout
                        .reserve_scratch(schema.cell_layout.align, cells * schema.cell_layout.size)
                })
            })
            .collect()
    } else {
        vec![None; n_funcs]
    };

    // Reserve per-Cell::Flags scratch *before* building the flags-info
    // entries so each entry's `set-flags.ptr` can land as an absolute
    // address (no reloc needed). 4-byte alignment matches string
    // `(ptr, len)` i32 pairs.
    let flags_scratch_addrs: Vec<u32> = flags_scratch_sizes(&per_func)
        .into_iter()
        .map(|n_bytes| layout.reserve_scratch(4, n_bytes))
        .collect();
    // Per-Cell::Char utf-8 scratch — 4 bytes (max sequence length)
    // per cell, byte-aligned (utf-8 stores are i32.store8).
    let char_scratch_addrs: Vec<u32> = char_scratch_sizes(&per_func)
        .into_iter()
        .map(|n_bytes| layout.reserve_scratch(1, n_bytes))
        .collect();
    let CharScratchMaps {
        per_cell: char_scratch_map,
        per_result_single: char_per_result_single,
    } = {
        let mut iter = char_scratch_addrs.iter().copied();
        let maps = build_char_scratch_map(&per_func, &mut iter);
        debug_assert!(iter.next().is_none());
        maps
    };

    // Build the per-(fn, field) enum-info and record-info side
    // tables. Each builder produces [`Segment`]s carrying their bytes
    // + any in-segment relocs (record-info's `entries` references
    // `tuples`); placement order below is now commutative because
    // every cross-segment ptr is a queued reloc, not a write that
    // gets patched after the fact.
    let enum_info_id = symbols.alloc();
    let flags_info_id = symbols.alloc();
    let record_entries_id = symbols.alloc();
    let record_tuples_id = symbols.alloc();
    let tuple_indices_id = symbols.alloc();
    let variant_info_id = symbols.alloc();
    let handle_info_id = symbols.alloc();
    let enum_info = build_enum_info_blob(
        &per_func,
        &enum_strings,
        &schema.enum_info_layout,
        enum_info_id,
    );
    let SideTableBlob {
        segment: enum_segment,
        per_param: enum_per_param_sym,
        per_result: enum_per_result_sym,
    } = enum_info;
    let mut flags_scratch_iter = flags_scratch_addrs.iter().copied();
    let FlagsInfoBlobs {
        entries: flags_entries_seg,
        per_param_range: flags_per_param_range_sym,
        per_result_range: flags_per_result_range_sym,
        per_cell_fill: mut flags_per_cell_fill,
        per_result_single_fill: mut flags_per_result_single_fill,
    } = build_flags_info_blob(
        &per_func,
        &flags_strings,
        &schema.flags_info_layout,
        flags_info_id,
        &mut flags_scratch_iter,
    );
    debug_assert!(
        flags_scratch_iter.next().is_none(),
        "flags scratch reservations must be consumed exactly once per Cell::Flags",
    );
    let RecordInfoBlobs {
        entries: record_entries_seg,
        tuples: record_tuples_seg,
        per_param_range: record_per_param_range_sym,
        per_result_range: record_per_result_range_sym,
        per_cell_idx: record_info_per_cell,
    } = build_record_info_blob(
        &per_func,
        &schema.record_info_layout,
        &schema.record_field_tuple_layout,
        record_entries_id,
        record_tuples_id,
    );
    let TupleIndicesBlob {
        segment: tuple_indices_seg,
        per_cell_idx: tuple_indices_per_cell,
    } = build_tuple_indices_blob(&per_func, tuple_indices_id);
    let VariantInfoBlobs {
        entries: variant_entries_seg,
        per_param_range: variant_per_param_range_sym,
        per_result_range: variant_per_result_range_sym,
        per_cell_fill: mut variant_per_cell_fill,
    } = build_variant_info_blob(
        &per_func,
        &variant_strings,
        &schema.variant_info_layout,
        variant_info_id,
    );
    let HandleInfoBlobs {
        entries: handle_entries_seg,
        per_param_range: handle_per_param_range_sym,
        per_result_range: handle_per_result_range_sym,
        per_cell_fill: mut handle_per_cell_fill,
    } = build_handle_info_blob(&per_func, &schema.handle_info_layout, handle_info_id);

    // Order doesn't matter for correctness — each placement assigns
    // a base, relocs land later. Coalesced same-alignment segments
    // sit back-to-back as a side effect of the order chosen here.
    place_segment(&mut layout, &mut symbols, &mut relocs, record_tuples_seg);
    place_segment(&mut layout, &mut symbols, &mut relocs, record_entries_seg);
    place_segment(&mut layout, &mut symbols, &mut relocs, enum_segment);
    place_segment(&mut layout, &mut symbols, &mut relocs, tuple_indices_seg);

    // Runtime-filled entry segments: place, then back-fill per-cell
    // slot addresses (only knowable after placement).
    let flags_entries_base =
        place_segment(&mut layout, &mut symbols, &mut relocs, flags_entries_seg);
    back_fill_flags_len_addrs(
        &mut flags_per_cell_fill,
        &mut flags_per_result_single_fill,
        flags_entries_base,
        &schema.flags_info_layout,
    );
    let variant_entries_base =
        place_segment(&mut layout, &mut symbols, &mut relocs, variant_entries_seg);
    back_fill_variant_entry_addrs(
        &mut variant_per_cell_fill,
        variant_entries_base,
        &schema.variant_info_layout,
        schema.variant_info_payload_value_off,
    );
    let handle_entries_base =
        place_segment(&mut layout, &mut symbols, &mut relocs, handle_entries_seg);
    back_fill_handle_id_addrs(
        &mut handle_per_cell_fill,
        handle_entries_base,
        &schema.handle_info_layout,
    );

    // Resolve per-(fn, param) and per-fn `SymRef`s to absolute
    // `BlobSlice`s now that every segment has a base. Tuple-indices
    // stay symbolic until the FuncDispatch assembly resolves them
    // per (fn, param | result) via `PerCellIndices::resolve_*`.
    let (enum_per_param, enum_per_result) =
        resolve_param_result_ranges(&symbols, enum_per_param_sym, enum_per_result_sym);
    let (flags_per_param_range, flags_per_result_range) = resolve_param_result_ranges(
        &symbols,
        flags_per_param_range_sym,
        flags_per_result_range_sym,
    );
    let (record_per_param_range, record_per_result_range) = resolve_param_result_ranges(
        &symbols,
        record_per_param_range_sym,
        record_per_result_range_sym,
    );
    let (variant_per_param_range, variant_per_result_range) = resolve_param_result_ranges(
        &symbols,
        variant_per_param_range_sym,
        variant_per_result_range_sym,
    );
    let (handle_per_param_range, handle_per_result_range) = resolve_param_result_ranges(
        &symbols,
        handle_per_param_range_sym,
        handle_per_result_range_sym,
    );

    // Bundle every kind's per-(fn, param) and per-(fn, result)
    // pointers into one `FieldSideTables` per field-tree, so the
    // blob writers don't grow another arg per kind.
    let param_side_tables: Vec<Vec<FieldSideTables>> = (0..n_funcs)
        .map(|fn_idx| {
            (0..per_func[fn_idx].params.len())
                .map(|p_idx| FieldSideTables {
                    enum_infos: enum_per_param[fn_idx][p_idx],
                    flags_infos: flags_per_param_range[fn_idx][p_idx],
                    record_infos: record_per_param_range[fn_idx][p_idx],
                    variant_infos: variant_per_param_range[fn_idx][p_idx],
                    handle_infos: handle_per_param_range[fn_idx][p_idx],
                })
                .collect()
        })
        .collect();
    let result_side_tables: Vec<FieldSideTables> = (0..n_funcs)
        .map(|fn_idx| FieldSideTables {
            enum_infos: enum_per_result[fn_idx],
            flags_infos: flags_per_result_range[fn_idx],
            record_infos: record_per_result_range[fn_idx],
            variant_infos: variant_per_result_range[fn_idx],
            handle_infos: handle_per_result_range[fn_idx],
        })
        .collect();

    // Fields blob (data) — pre-filled with cells.ptr pointing at
    // each param's reserved slab slot, plus per-kind side-table
    // pointers patched per-param.
    let fields_blob = build_fields_blob(&per_func, schema, &cells_offsets, &param_side_tables);
    let (fields_base, _) = layout.place_data(schema.field_layout.align, &fields_blob);
    let fields_buf_offsets: Vec<u32> = {
        let mut cursor = fields_base;
        per_func
            .iter()
            .map(|fd| {
                let here = cursor;
                cursor += fd.params.len() as u32 * schema.field_layout.size;
                here
            })
            .collect()
    };

    // On-return params blob (data), only when after-hook is wired.
    let after_blob = build_after_params_blob(
        &per_func,
        schema,
        iface_name,
        &result_cells_offsets,
        &result_side_tables,
    );
    let after_params_offsets: Vec<Option<i32>> =
        match schema.after_hook.as_ref().map(|h| &h.params_layout) {
            Some(al) => {
                let (after_base, _) = layout.place_data(al.align, &after_blob);
                let mut cursor = after_base;
                (0..n_funcs)
                    .map(|_| {
                        let here = cursor as i32;
                        cursor += al.size;
                        Some(here)
                    })
                    .collect()
            }
            None => vec![None; n_funcs],
        };

    // Scratch slots: event record + on-call indirect-params buffer.
    let event_ptr = layout.reserve_scratch(EVENT_SLOT_ALIGN, EVENT_SLOT_SIZE) as i32;
    let hook_params_ptr = schema
        .before_hook
        .as_ref()
        .map(|h| layout.reserve_scratch(h.params_layout.align, h.params_layout.size));

    // Per-fn retptr scratch — only for funcs whose canonical-ABI
    // shape uses one.
    let retptr_offsets: Vec<Option<i32>> = per_func
        .iter()
        .zip(funcs.iter())
        .map(|(fd, func)| {
            if !(fd.export_sig.retptr || fd.import_sig.retptr) {
                return None;
            }
            let result_ty = func
                .result
                .as_ref()
                .expect("retptr → func.result is_some()");
            let size = schema.size_align.size(result_ty).size_wasm32() as u32;
            let align = schema.size_align.align(result_ty).align_wasm32() as u32;
            Some(layout.reserve_scratch(align, size) as i32)
        })
        .collect();

    // Align the bump-allocator start past the largest alignment we
    // placed; today that's `cell` (8) but pulling from `cell_layout`
    // keeps it tied to the schema instead of a literal.
    let bump_start = layout.end().next_multiple_of(schema.cell_layout.align);
    if bump_start > LAYOUT_SIZE_BUDGET {
        bail!("static-data layout end {bump_start} exceeds i32 budget {LAYOUT_SIZE_BUDGET}");
    }
    let mut data_segments = layout.into_segments();
    // Resolve every queued cross-segment pointer in one pass. Has to
    // happen after `into_segments` so the segments aren't being
    // mutated through the layout's coalescing path.
    relocs.resolve(&symbols, &mut data_segments);

    // Assemble FuncDispatch from each FuncClassified + its offsets.
    // Owns the move from classify-time → post-layout types — every
    // offset is known here, nothing is "0 // back-filled later".
    let dispatches: Vec<FuncDispatch> = per_func
        .into_iter()
        .enumerate()
        .map(|(i, fc)| {
            let fn_cells_offsets = &cells_offsets[i];
            let params: Vec<ParamLayout> = fc
                .params
                .into_iter()
                .enumerate()
                .map(|(p_idx, lift)| {
                    let tuple_slices = tuple_indices_per_cell.resolve_param(i, p_idx, &symbols);
                    let sources = CellFillSources {
                        record_info: record_info_per_cell.for_param(i, p_idx),
                        tuple_indices: &tuple_slices,
                        flags_fill: flags_per_cell_fill.for_param(i, p_idx),
                        variant_fill: variant_per_cell_fill.for_param(i, p_idx),
                        char_scratch: char_scratch_map.for_param(i, p_idx),
                        handle_fill: handle_per_cell_fill.for_param(i, p_idx),
                    };
                    let cell_side = fold_cell_side_data(&lift.plan, &sources);
                    ParamLayout {
                        lift,
                        cells_offset: fn_cells_offsets[p_idx],
                        cell_side,
                    }
                })
                .collect();

            let retptr_offset = retptr_offsets[i];
            let result_cells_offset = result_cells_offsets[i];
            let result_lift = fc.result_lift.map(|rl| {
                let ResultLift { source, .. } = rl;
                let layout_source = match source {
                    ResultSource::Direct(cell) => {
                        let side_data = single_cell_side_data(
                            &cell,
                            &flags_per_result_single_fill[i],
                            &char_per_result_single[i],
                        );
                        ResultSourceLayout::Direct { cell, side_data }
                    }
                    ResultSource::RetptrPair(cell) => {
                        let side_data = single_cell_side_data(
                            &cell,
                            &flags_per_result_single_fill[i],
                            &char_per_result_single[i],
                        );
                        ResultSourceLayout::RetptrPair {
                            cell,
                            retptr_offset: retptr_offset
                                .expect("RetptrPair → retptr scratch reserved"),
                            side_data,
                        }
                    }
                    ResultSource::Compound(compound) => {
                        let tuple_slices = tuple_indices_per_cell.resolve_result(i, &symbols);
                        let sources = CellFillSources {
                            record_info: record_info_per_cell.for_result(i),
                            tuple_indices: &tuple_slices,
                            flags_fill: flags_per_cell_fill.for_result(i),
                            variant_fill: variant_per_cell_fill.for_result(i),
                            char_scratch: char_scratch_map.for_result(i),
                            handle_fill: handle_per_cell_fill.for_result(i),
                        };
                        let cell_side = fold_cell_side_data(&compound.plan, &sources);
                        ResultSourceLayout::Compound {
                            compound,
                            retptr_offset: retptr_offset
                                .expect("Compound → retptr scratch reserved"),
                            cell_side,
                        }
                    }
                };
                ResultLayout {
                    source: layout_source,
                }
            });

            let after = after_params_offsets[i].map(|params_offset| AfterSetup {
                params_offset,
                result_cells_offset,
            });

            FuncDispatch {
                shape: fc.shape,
                result_ty: fc.result_ty,
                import_module: fc.import_module,
                import_field: fc.import_field,
                export_name: fc.export_name,
                export_sig: fc.export_sig,
                import_sig: fc.import_sig,
                needs_cabi_post: fc.needs_cabi_post,
                fn_name_offset: fc.fn_name_offset,
                fn_name_len: fc.fn_name_len,
                params,
                fields_buf_offset: fields_buf_offsets[i],
                retptr_offset,
                result_lift,
                after,
            }
        })
        .collect();

    Ok((
        dispatches,
        StaticDataPlan {
            bump_start,
            event_ptr,
            hook_params_ptr,
            data_segments,
        },
    ))
}

#[cfg(test)]
mod tests {
    //! Each test should be a few lines: build a [`LayoutEnv`] with
    //! [`env`] (or [`env_with`] for hook-wiring variants), then assert
    //! against the dispatches / plan / schema it carries. New
    //! invariants are mostly one-liners over [`LayoutEnv::params`] /
    //! [`LayoutEnv::dispatch`].
    //!
    //! The fixture WIT exposes every retptr/result branch the layout
    //! code takes (no-args, primitive params + result, string param,
    //! string return via retptr, multi-cell record param). Adding a
    //! new branch = adding a function to [`TARGET_WIT`].
    //!
    //! Failure messages are intentionally absent — `cargo test` prints
    //! the test name + line, which is enough to localize.
    use super::super::build_per_func_classified;
    use super::super::lift::ParamLayout;
    use super::super::schema::{compute_schema, SchemaLayouts};
    use super::super::synthesize_adapter_world_wit;
    use super::*;
    use wit_parser::Resolve;

    const TARGET_IFACE: &str = "test:layout-fixture/t@0.0.1";
    const TARGET_WIT: &str = r#"
        package test:layout-fixture@0.0.1;
        interface t {
            record point { x: u32, y: s32 }
            f-noargs: func();
            f-pair-u32: func(a: u32, b: u32) -> u32;
            f-string: func(s: string);
            f-string-result: func(x: u32) -> string;
            f-record: func(p: point) -> bool;
        }
    "#;

    /// Bundle returned by [`env`] / [`env_with`]: a fully-laid-out
    /// dispatch list paired with the schema + plan it was produced
    /// against. Tests destructure `env.dispatches` / `env.plan` /
    /// `env.schema` directly.
    struct LayoutEnv {
        dispatches: Vec<FuncDispatch>,
        plan: StaticDataPlan,
        schema: SchemaLayouts,
    }

    impl LayoutEnv {
        /// `(FuncDispatch, ParamLayout)` pairs across every fn —
        /// the right shape for per-param invariants like alignment
        /// and overlap checks.
        fn params(&self) -> impl Iterator<Item = (&FuncDispatch, &ParamLayout)> {
            self.dispatches
                .iter()
                .flat_map(|fd| fd.params.iter().map(move |p| (fd, p)))
        }

        /// Look up a dispatch by export-name substring. Tests use
        /// the WIT function name as the substring (mangling adds
        /// the interface prefix but preserves the name).
        fn dispatch(&self, name: &str) -> &FuncDispatch {
            self.dispatches
                .iter()
                .find(|fd| fd.export_name.contains(name))
                .unwrap_or_else(|| panic!("no dispatch matching `{name}`"))
        }
    }

    fn env() -> LayoutEnv {
        env_with(true, true)
    }

    fn env_with(has_before: bool, has_after: bool) -> LayoutEnv {
        use crate::contract::{versioned_interface, TIER2_AFTER, TIER2_BEFORE, TIER2_VERSION};
        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");
        let mut resolve = Resolve::new();
        resolve.push_str("test.wit", TARGET_WIT).unwrap();
        resolve.push_str("common.wit", common_wit).unwrap();
        resolve.push_str("tier2.wit", tier2_wit).unwrap();
        let mut hook_ifaces: Vec<String> = Vec::new();
        if has_before {
            hook_ifaces.push(versioned_interface(TIER2_BEFORE, TIER2_VERSION));
        }
        if has_after {
            hook_ifaces.push(versioned_interface(TIER2_AFTER, TIER2_VERSION));
        }
        let world_wit = synthesize_adapter_world_wit(
            "test:layout-fixture-adapter",
            "adapter",
            TARGET_IFACE,
            &hook_ifaces,
        );
        let world_pkg = resolve.push_str("world.wit", &world_wit).unwrap();
        let world_id = resolve.select_world(&[world_pkg], Some("adapter")).unwrap();
        let target_iface =
            super::super::test_utils::iface_by_unversioned_qname(&resolve, "test:layout-fixture/t");
        let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
            .functions
            .values()
            .collect();
        let schema = compute_schema(&resolve, world_id, has_before, has_after).unwrap();
        let mut names = NameInterner::new();
        let iface_name = names.intern(TARGET_IFACE);
        let classified =
            build_per_func_classified(&resolve, target_iface, &funcs, &mut names).unwrap();
        let (dispatches, plan) =
            lay_out_static_memory(classified, &funcs, &schema, names, iface_name).unwrap();
        LayoutEnv {
            dispatches,
            plan,
            schema,
        }
    }

    // ─── Cell-slab placement ──────────────────────────────────────

    #[test]
    fn param_cells_offsets_aligned_to_cell_align() {
        let env = env();
        let align = env.schema.cell_layout.align;
        assert!(env.params().all(|(_, p)| p.cells_offset % align == 0));
    }

    #[test]
    fn param_cells_slabs_dont_overlap() {
        let env = env();
        let cell_size = env.schema.cell_layout.size;
        let mut slabs: Vec<(u32, u32)> = env
            .params()
            .map(|(_, p)| {
                let start = p.cells_offset;
                (start, start + p.lift.plan.cell_count() * cell_size)
            })
            .collect();
        slabs.sort();
        assert!(slabs.windows(2).all(|w| w[0].1 <= w[1].0));
    }

    // ─── Fields-blob placement ────────────────────────────────────

    #[test]
    fn fields_buf_offsets_per_func_are_contiguous() {
        let env = env();
        let fs = env.schema.field_layout.size;
        assert!(env.dispatches.windows(2).all(|w| {
            w[0].fields_buf_offset + (w[0].params.len() as u32) * fs == w[1].fields_buf_offset
        }));
    }

    // ─── After-hook wiring ────────────────────────────────────────

    #[test]
    fn after_setup_absent_when_after_hook_off() {
        assert!(env_with(true, false)
            .dispatches
            .iter()
            .all(|fd| fd.after.is_none()));
    }

    #[test]
    fn after_setup_present_when_after_hook_on() {
        assert!(env_with(true, true)
            .dispatches
            .iter()
            .all(|fd| fd.after.is_some()));
    }

    #[test]
    fn result_cells_offset_set_iff_func_has_result_lift() {
        let env = env();
        for fd in &env.dispatches {
            let after = fd.after.as_ref().unwrap();
            assert_eq!(
                after.result_cells_offset.is_some(),
                fd.result_lift.is_some()
            );
        }
    }

    // ─── Retptr scratch ───────────────────────────────────────────

    #[test]
    fn retptr_offset_set_iff_sig_uses_retptr() {
        let env = env();
        for fd in &env.dispatches {
            assert_eq!(
                fd.retptr_offset.is_some(),
                fd.export_sig.retptr || fd.import_sig.retptr,
            );
        }
    }

    #[test]
    fn fixture_covers_both_retptr_polarities() {
        // Guards [`retptr_offset_set_iff_sig_uses_retptr`] from
        // becoming vacuous if the fixture WIT loses one shape.
        let env = env();
        assert!(env.dispatches.iter().any(|fd| fd.retptr_offset.is_some()));
        assert!(env.dispatches.iter().any(|fd| fd.retptr_offset.is_none()));
    }

    // ─── Post-layout shape ────────────────────────────────────────

    #[test]
    fn dispatch_param_count_matches_wit_param_count() {
        let env = env();
        let counts: Vec<usize> = env.dispatches.iter().map(|fd| fd.params.len()).collect();
        // f-noargs(0), f-pair-u32(2), f-string(1), f-string-result(1), f-record(1)
        assert_eq!(counts, vec![0, 2, 1, 1, 1]);
    }

    // ─── Bump-allocator base ──────────────────────────────────────

    #[test]
    fn bump_start_aligned_to_cell_align() {
        let env = env();
        assert_eq!(env.plan.bump_start % env.schema.cell_layout.align, 0);
    }

    #[test]
    fn bump_start_within_i32_budget() {
        assert!(env().plan.bump_start <= i32::MAX as u32);
    }

    #[test]
    fn data_segments_sit_below_bump_start() {
        let env = env();
        assert!(env
            .plan
            .data_segments
            .iter()
            .all(|(off, bytes)| off + bytes.len() as u32 <= env.plan.bump_start));
    }

    // ─── Fixture sanity (guards the property tests from running on
    // a degenerate WIT) ──────────────────────────────────────────

    #[test]
    fn fixture_includes_void_and_non_void_funcs() {
        let env = env();
        assert!(env.dispatch("f-noargs").result_lift.is_none());
        assert!(env.dispatch("f-pair-u32").result_lift.is_some());
    }

    // ─── Layout-budget bails ──────────────────────────────────────

    /// Drive the same pipeline as [`env_with`] but for an arbitrary
    /// target WIT, returning the `lay_out_static_memory` result so
    /// the budget tests can assert on its `Err`. `target_iface` is
    /// the unversioned qname (`pkg:ns/iface`); the fixture WIT must
    /// declare exactly one matching package + interface.
    fn try_lay_out(target_wit: &str, target_iface_qname: &str) -> Result<()> {
        use crate::contract::{versioned_interface, TIER2_AFTER, TIER2_BEFORE, TIER2_VERSION};
        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");
        let mut resolve = Resolve::new();
        resolve.push_str("test.wit", target_wit).unwrap();
        resolve.push_str("common.wit", common_wit).unwrap();
        resolve.push_str("tier2.wit", tier2_wit).unwrap();
        let hook_ifaces = vec![
            versioned_interface(TIER2_BEFORE, TIER2_VERSION),
            versioned_interface(TIER2_AFTER, TIER2_VERSION),
        ];
        let target_versioned = format!("{target_iface_qname}@0.0.1");
        let world_wit = synthesize_adapter_world_wit(
            "test:budget-fixture-adapter",
            "adapter",
            &target_versioned,
            &hook_ifaces,
        );
        let world_pkg = resolve.push_str("world.wit", &world_wit).unwrap();
        let world_id = resolve.select_world(&[world_pkg], Some("adapter")).unwrap();
        let target_iface =
            super::super::test_utils::iface_by_unversioned_qname(&resolve, target_iface_qname);
        let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
            .functions
            .values()
            .collect();
        let schema = compute_schema(&resolve, world_id, true, true).unwrap();
        let mut names = NameInterner::new();
        let iface_name = names.intern(&target_versioned);
        let classified = build_per_func_classified(&resolve, target_iface, &funcs, &mut names)?;
        lay_out_static_memory(classified, &funcs, &schema, names, iface_name).map(|_| ())
    }

    #[test]
    fn flat_slot_budget_bails_when_param_flatten_exceeds_cap() {
        // `flat_slot_count` is per-param: a record param flattens to
        // one slot per leaf primitive field. `MAX_FLAT_SLOTS_PER_FN
        // + 1` u32 fields pushes one record param over the cap.
        // (The cell-budget check runs after the flat-slot check, so
        // the flat-slot bail fires first even though this shape also
        // exceeds `MAX_CELLS_PER_PARAM`.)
        let n = MAX_FLAT_SLOTS_PER_FN + 1;
        let fields = (0..n)
            .map(|i| format!("f{i}: u32"))
            .collect::<Vec<_>>()
            .join(", ");
        let wit = format!(
            "package test:budget-flat@0.0.1;\n\
             interface t {{\n\
                 record big {{ {fields} }}\n\
                 bloat: func(b: big);\n\
             }}\n"
        );
        let err = try_lay_out(&wit, "test:budget-flat/t")
            .expect_err("flat-slot budget should bail at MAX_FLAT_SLOTS_PER_FN + 1");
        let msg = err.to_string();
        assert!(
            msg.contains("flat-slot count") && msg.contains(&MAX_FLAT_SLOTS_PER_FN.to_string()),
            "bail should name the budget, got: {msg}"
        );
    }

    #[test]
    fn cell_budget_bails_when_record_param_exceeds_cap() {
        // Each leaf field contributes one cell, plus one `RecordOf`
        // for the parent. `MAX_CELLS_PER_PARAM` leaf u32 fields gives
        // `MAX_CELLS_PER_PARAM + 1` cells — one over.
        let n = MAX_CELLS_PER_PARAM;
        let fields = (0..n)
            .map(|i| format!("f{i}: u32"))
            .collect::<Vec<_>>()
            .join(", ");
        let wit = format!(
            "package test:budget-cells@0.0.1;\n\
             interface t {{\n\
                 record big {{ {fields} }}\n\
                 bloat: func(b: big);\n\
             }}\n"
        );
        let err = try_lay_out(&wit, "test:budget-cells/t")
            .expect_err("cell budget should bail at MAX_CELLS_PER_PARAM + 1");
        let msg = err.to_string();
        assert!(
            msg.contains("cell count") && msg.contains(&MAX_CELLS_PER_PARAM.to_string()),
            "bail should name the budget, got: {msg}"
        );
    }
}
