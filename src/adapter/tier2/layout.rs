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

use super::super::abi::emit::{BlobSlice, CALLID_FN, CALLID_IFACE};
use super::super::mem_layout::StaticLayout;
use super::blob::{RecordWriter, RelocPlan, SymbolBases};
use super::lift::{
    build_enum_info_blob, build_record_info_blob, register_enum_strings, register_record_strings,
    ParamLayout, RecordInfoBlobs, ResultLayout, ResultLift, ResultSource, ResultSourceLayout,
    SideTableBlob,
};
use super::schema::{
    SchemaLayouts, FIELD_NAME, FIELD_TREE, ON_RET_CALL, ON_RET_RESULT, TREE_CELLS,
    TREE_ENUM_INFOS, TREE_RECORD_INFOS, TREE_ROOT,
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
/// 65 536 sits well above any realistic shape.
const MAX_FLAT_SLOTS_PER_FN: u32 = 1 << 16;

/// Per-param (and per-result) cell-tree cap. Bounds `cell_count *
/// cell_size` slab sizes and the cell index used as `i32.const` in
/// `emit_lift_plan`.
const MAX_CELLS_PER_PARAM: u32 = 1 << 20;

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

/// Output of the static-memory layout phase: the addresses the
/// emit-code phase needs to reference, plus the data segments
/// ready to feed `emit_data_section`.
pub(super) struct StaticDataPlan {
    pub(super) bump_start: u32,
    pub(super) event_ptr: i32,
    pub(super) hook_params_ptr: u32,
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
    record_infos: BlobSlice,
    // flags_infos: BlobSlice,
    // variant_infos: BlobSlice,
    // handle_infos: BlobSlice,
}

impl FieldSideTables {
    fn write_to_tree(&self, blob: &mut [u8], tree: &RecordWriter) {
        tree.write_slice(blob, TREE_ENUM_INFOS, self.enum_infos);
        tree.write_slice(blob, TREE_RECORD_INFOS, self.record_infos);
        // tree.write_slice(blob, TREE_FLAGS_INFOS, self.flags_infos);
        // ...
    }
}

/// Single-pass build of a `field` record + its embedded
/// `field-tree` for one (function, param) pair. `cells` points at
/// the param's contiguous cells slab (`(slab-offset, cell-count)`).
/// `side_tables` patches the field-tree's per-kind-infos lists for
/// any kinds the param's plan carries.
fn write_field_record(
    blob: &mut Vec<u8>,
    schema: &SchemaLayouts,
    cells: BlobSlice,
    name: BlobSlice,
    side_tables: FieldSideTables,
) {
    let field = RecordWriter::extend_zero(blob, &schema.field_layout);
    field.write_slice(blob, FIELD_NAME, name);
    let tree = field.nested(FIELD_TREE, &schema.tree_layout);
    tree.write_slice(blob, TREE_CELLS, cells);
    side_tables.write_to_tree(blob, &tree);
    // Root cell is always `cells[0]` for the plan-builder.
    tree.write_i32(blob, TREE_ROOT, 0);
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
            // slab; `cells.len = plan.cell_count()`. `root` is always
            // 0 because the plan-builder allocates the root cell
            // first into each plan.
            write_field_record(
                &mut blob,
                schema,
                BlobSlice {
                    off: cells_offsets[fn_idx][i],
                    len: p.plan.cell_count(),
                },
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
    let Some(after_layout) = schema.on_return_params_layout.as_ref() else {
        return Vec::new();
    };
    let mut blob: Vec<u8> = Vec::new();
    for (fn_idx, fd) in per_func.iter().enumerate() {
        let entry = RecordWriter::extend_zero(&mut blob, after_layout);
        let call = entry.nested(ON_RET_CALL, &schema.callid_layout);
        call.write_slice(&mut blob, CALLID_IFACE, iface_name);
        call.write_slice(
            &mut blob,
            CALLID_FN,
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
                // holds the full cell tree); single-cell result
                // (Direct / RetptrPair): len = 1.
                let cells_len = fd
                    .result_lift
                    .as_ref()
                    .and_then(|rl| rl.compound())
                    .map_or(1, |c| c.plan.cell_count());
                tree.write_slice(
                    &mut blob,
                    TREE_CELLS,
                    BlobSlice {
                        off: cells_off,
                        len: cells_len,
                    },
                );
                result_side_tables[fn_idx].write_to_tree(&mut blob, &tree);
                tree.write_i32(&mut blob, TREE_ROOT, 0);
            }
            None => entry.write_option_none(&mut blob, ON_RET_RESULT),
        }
    }
    blob
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
    name_blob: &mut Vec<u8>,
    iface_name: BlobSlice,
) -> Result<(Vec<FuncDispatch>, StaticDataPlan)> {
    let n_funcs = per_func.len();

    check_layout_budget(&per_func)?;

    // Side-table strings get appended to name_blob BEFORE we place
    // it — every side-table-info entry references these string
    // offsets, so they have to land in the data segment first.
    let enum_strings = register_enum_strings(&per_func, name_blob);
    let record_strings = register_record_strings(&per_func, name_blob);

    let mut layout = StaticLayout::new();
    let mut symbols = SymbolBases::new();
    let mut relocs = RelocPlan::new();

    layout.place_data(1, name_blob);

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

    // Build the per-(fn, field) enum-info and record-info side
    // tables. Each builder produces [`Segment`]s carrying their bytes
    // + any in-segment relocs (record-info's `entries` references
    // `tuples`); placement order below is now commutative because
    // every cross-segment ptr is a queued reloc, not a write that
    // gets patched after the fact.
    let enum_info_id = symbols.alloc();
    let record_entries_id = symbols.alloc();
    let record_tuples_id = symbols.alloc();
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
    let RecordInfoBlobs {
        entries: record_entries_seg,
        tuples: record_tuples_seg,
        per_param_range: record_per_param_range_sym,
        per_param_cell_idx,
        per_result_range: record_per_result_range_sym,
        per_result_cell_idx,
    } = build_record_info_blob(
        &per_func,
        &record_strings,
        &schema.record_info_layout,
        &schema.record_field_tuple_layout,
        record_entries_id,
        record_tuples_id,
    );

    // Order doesn't matter for correctness — each placement assigns
    // a base, relocs land later. Tuples-then-entries-then-enums is
    // just a convenient order to coalesce same-alignment segments
    // back-to-back.
    let record_tuples_base = layout.place_data(record_tuples_seg.align, &record_tuples_seg.bytes);
    symbols.set(record_tuples_seg.id, record_tuples_base);
    relocs.record_segment(record_tuples_base, record_tuples_seg.relocs);

    let record_entries_base =
        layout.place_data(record_entries_seg.align, &record_entries_seg.bytes);
    symbols.set(record_entries_seg.id, record_entries_base);
    relocs.record_segment(record_entries_base, record_entries_seg.relocs);

    let enum_info_base = layout.place_data(enum_segment.align, &enum_segment.bytes);
    symbols.set(enum_segment.id, enum_info_base);
    relocs.record_segment(enum_info_base, enum_segment.relocs);

    // Resolve per-(fn, param) and per-(fn, result) [`SymRef`]s to
    // absolute [`BlobSlice`]s now that all three segments have bases.
    let enum_per_param: Vec<Vec<BlobSlice>> = enum_per_param_sym
        .into_iter()
        .map(|v| v.into_iter().map(|s| s.resolve(&symbols)).collect())
        .collect();
    let enum_per_result: Vec<BlobSlice> = enum_per_result_sym
        .into_iter()
        .map(|s| s.resolve(&symbols))
        .collect();
    let record_per_param_range: Vec<Vec<BlobSlice>> = record_per_param_range_sym
        .into_iter()
        .map(|v| v.into_iter().map(|s| s.resolve(&symbols)).collect())
        .collect();
    let record_per_result_range: Vec<BlobSlice> = record_per_result_range_sym
        .into_iter()
        .map(|s| s.resolve(&symbols))
        .collect();

    // Bundle every kind's per-(fn, param) and per-(fn, result)
    // pointers into one `FieldSideTables` per field-tree, so the
    // blob writers don't grow another arg per kind.
    let param_side_tables: Vec<Vec<FieldSideTables>> = enum_per_param
        .iter()
        .zip(record_per_param_range.iter())
        .map(|(enums, records)| {
            enums
                .iter()
                .zip(records.iter())
                .map(|(&enum_infos, &record_infos)| FieldSideTables {
                    enum_infos,
                    record_infos,
                })
                .collect()
        })
        .collect();
    let result_side_tables: Vec<FieldSideTables> = enum_per_result
        .iter()
        .zip(record_per_result_range.iter())
        .map(|(&enum_infos, &record_infos)| FieldSideTables {
            enum_infos,
            record_infos,
        })
        .collect();

    // Fields blob (data) — pre-filled with cells.ptr pointing at
    // each param's reserved slab slot, plus per-kind side-table
    // pointers patched per-param.
    let fields_blob = build_fields_blob(&per_func, schema, &cells_offsets, &param_side_tables);
    let fields_base = layout.place_data(schema.field_layout.align, &fields_blob);
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
    let after_params_offsets: Vec<Option<i32>> = match schema.on_return_params_layout.as_ref() {
        Some(al) => {
            let after_base = layout.place_data(al.align, &after_blob);
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
    let hook_params_ptr = match schema.on_call_params_layout.as_ref() {
        Some(l) => layout.reserve_scratch(l.align, l.size),
        None => 0,
    };

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
            let fn_param_record_idxs = &per_param_cell_idx[i];
            let params: Vec<ParamLayout> = fc
                .params
                .into_iter()
                .enumerate()
                .map(|(p_idx, lift)| ParamLayout {
                    lift,
                    cells_offset: fn_cells_offsets[p_idx],
                    record_info_cell_idx: fn_param_record_idxs[p_idx].clone(),
                })
                .collect();

            let retptr_offset = retptr_offsets[i];
            let result_cells_offset = result_cells_offsets[i];
            let result_lift = fc.result_lift.map(|rl| {
                let ResultLift { source, .. } = rl;
                let layout_source = match source {
                    ResultSource::Direct(kind) => ResultSourceLayout::Direct(kind),
                    ResultSource::RetptrPair(kind) => ResultSourceLayout::RetptrPair {
                        kind,
                        retptr_offset: retptr_offset.expect("RetptrPair → retptr scratch reserved"),
                    },
                    ResultSource::Compound(compound) => ResultSourceLayout::Compound {
                        compound,
                        retptr_offset: retptr_offset.expect("Compound → retptr scratch reserved"),
                        record_info_cell_idx: per_result_cell_idx[i].clone(),
                    },
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
