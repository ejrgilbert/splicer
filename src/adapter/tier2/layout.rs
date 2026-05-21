//! Static-memory layout phase: takes the `FuncClassified` list,
//! reserves data + scratch slabs, and returns an immutable
//! `FuncDispatch` list with every offset filled in.

use anyhow::{bail, Result};
use wit_parser::{Function as WitFunction, Type};

use super::super::abi::emit::BlobSlice;
use super::super::mem_layout::StaticLayout;
use super::blob::{resolve, NameInterner, RecordWriter, RelocPlan, Segment, SymRef, SymbolBases};
use super::lift::plan::Cell;
use super::lift::{
    back_fill_record_fields_ptrs, build_char_scratch_map, build_enum_info_blob,
    build_flags_info_maps, build_handle_info_maps, build_record_info_maps,
    build_tuple_indices_blob, build_variant_info_maps, char_scratch_sizes, flags_scratch_sizes,
    fold_cell_side_data, CellFillSources, CellSideData, CharScratch, CharScratchMaps,
    FlagsInfoMaps, FlagsRuntimeFill, HandleInfoMaps, HandleRuntimeFill, InfoCounts, ParamLayout,
    RecordInfoMaps, ResultLayout, ResultLift, ResultSource, ResultSourceLayout, SideTableBlob,
    TupleIndicesBlob, VariantInfoMaps,
};
use super::schema::{
    SchemaLayouts, FIELD_NAME, FIELD_TREE, ON_RET_CALL, ON_RET_RESULT, TREE_CELLS, TREE_ENUM_INFOS,
    TREE_FLAGS_INFOS, TREE_HANDLE_INFOS, TREE_RECORD_INFOS, TREE_ROOT, TREE_VARIANT_INFOS,
};
use super::{AfterSetup, FuncClassified, FuncDispatch, FuncShape};

// ─── ABI-anchored constants (not WIT-schema-derivable) ────────────

/// `waitable-set.wait` event record slot (wit-component runtime ABI).
const EVENT_SLOT_SIZE: u32 = 8;
const EVENT_SLOT_ALIGN: u32 = 4;

// ─── Layout-phase size budget ─────────────────────────────────────
//
// Wasm encodes static-data offsets as `i32.const`, so every offset
// must fit in signed-i32. One pre-check bounds the per-fn/param
// counts; one post-check verifies the final end.

/// Final layout end must fit in signed i32.
const LAYOUT_SIZE_BUDGET: u32 = i32::MAX as u32;

/// Per-fn flat-slot count cap. Reduced under `cfg(test)` to exercise
/// the bail without a WIT at the production limit. Plan-builder
/// pre-bails on `list<T, N>` with `N` over this cap so a hostile
/// integer literal can't overflow `bump_flat_slot`'s u32 counter
/// before layout's post-build check fires.
#[cfg(not(test))]
pub(in crate::adapter::tier2) const MAX_FLAT_SLOTS_PER_FN: u32 = 1 << 16;
#[cfg(test)]
pub(in crate::adapter::tier2) const MAX_FLAT_SLOTS_PER_FN: u32 = 16;

/// Per-param (and per-result) cell-tree cap. Plan-builder pre-bails
/// on oversized `list<T, N>` for the same reason as above.
#[cfg(not(test))]
pub(in crate::adapter::tier2) const MAX_CELLS_PER_PARAM: u32 = 1 << 20;
#[cfg(test)]
pub(in crate::adapter::tier2) const MAX_CELLS_PER_PARAM: u32 = 8;

/// Bound per-fn / per-param counts so downstream `u32` arithmetic fits.
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

/// Per-fn fills for a Direct (sync flat) result.
struct SingleCellFills<'a> {
    flags_fill: &'a Option<FlagsRuntimeFill>,
    char_scratch: &'a Option<i32>,
    handle_fill: &'a Option<HandleRuntimeFill>,
}

/// Wrap per-fn fills into a `CellSideData` for a Direct result.
/// Compound/un-wired kinds are unreachable — classify_result_lift
/// routes them through Compound.
fn single_cell_side_data(cell: &Cell, fills: &SingleCellFills<'_>) -> CellSideData {
    match cell {
        Cell::Flags { .. } => CellSideData::Flags(Box::new(
            fills
                .flags_fill
                .clone()
                .expect("flags-info fill must exist"),
        )),
        Cell::Char { .. } => CellSideData::Char {
            scratch: CharScratch::Static {
                scratch_addr: fills.char_scratch.expect("char-info scratch must exist"),
            },
        },
        Cell::Handle { .. } => CellSideData::Handle(Box::new(
            fills
                .handle_fill
                .clone()
                .expect("handle-info fill must exist"),
        )),
        Cell::Bool { .. }
        | Cell::IntegerSignExt { .. }
        | Cell::IntegerZeroExt { .. }
        | Cell::Integer64 { .. }
        | Cell::FloatingF32 { .. }
        | Cell::FloatingF64 { .. }
        | Cell::Text { .. }
        | Cell::Bytes { .. }
        | Cell::EnumCase { .. } => CellSideData::None,
        Cell::RecordOf { .. }
        | Cell::TupleOf { .. }
        | Cell::Option { .. }
        | Cell::Result { .. }
        | Cell::Variant { .. }
        | Cell::ListOf { .. } => {
            unreachable!("single_cell_side_data reached unsupported result Cell {cell:?}")
        }
    }
}

/// Output of the static-memory layout phase.
pub(super) struct StaticDataPlan {
    pub(super) bump_start: u32,
    pub(super) event_ptr: i32,
    /// On-call indirect-params scratch; `Some` iff before-hook is wired.
    pub(super) hook_params_ptr: Option<u32>,
    pub(super) data_segments: Vec<(u32, Vec<u8>)>,
}

/// Side-table absolute pointers for one field-tree.
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
/// `field-tree` for one (function, param) pair. `cells` is
/// `(0, cell-count)` — the wrapper body patches `cells.ptr` per
/// call after `cabi_realloc`. `root` is the cell-array index the
/// field-tree should walk from — sourced from the param's
/// [`super::lift::ParamLift::plan`]'s [`super::lift::plan::LiftPlan::root`].
/// `side_tables` patches the field-tree's per-kind-infos lists for
/// any kinds the param's plan carries.
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
/// (fn, param). `cells.ptr` is left at `0` — wrapper body patches it
/// per call after `cabi_realloc`.
fn build_fields_blob(
    per_func: &[FuncClassified],
    schema: &SchemaLayouts,
    param_side_tables: &[Vec<FieldSideTables>],
) -> Vec<u8> {
    let mut blob: Vec<u8> = Vec::new();
    for (fn_idx, fd) in per_func.iter().enumerate() {
        for (i, p) in fd.params.iter().enumerate() {
            write_field_record(
                &mut blob,
                schema,
                BlobSlice {
                    off: 0,
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

/// On-return params blob: one record per fn. `result` is
/// `some(field-tree)` pre-wired for funcs with a result lift,
/// `none` otherwise. `cells.ptr` patched per call.
fn build_after_params_blob(
    per_func: &[FuncClassified],
    schema: &SchemaLayouts,
    iface_name: BlobSlice,
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
        if fd.result_lift.is_some() {
            entry.write_option_some(&mut blob, ON_RET_RESULT);
            let tree_base = entry.field_offset(ON_RET_RESULT) + schema.option_payload_off as usize;
            let tree = RecordWriter::at(&schema.tree_layout, tree_base);
            // Compound: cells.len = plan.cell_count, root = plan.root.
            // Direct: len = 1, root = 0.
            let (cells_len, root) = fd
                .result_lift
                .as_ref()
                .and_then(|rl| rl.compound())
                .map_or((1, 0), |c| (c.plan.cell_count(), c.plan.root()));
            tree.write_slice(
                &mut blob,
                TREE_CELLS,
                BlobSlice {
                    off: 0,
                    len: cells_len,
                },
            );
            result_side_tables[fn_idx].write_to_tree(&mut blob, &tree);
            tree.write_i32(&mut blob, TREE_ROOT, root as i32);
        } else {
            entry.write_option_none(&mut blob, ON_RET_RESULT);
        }
    }
    blob
}

/// Place a segment, register its symbol, queue relocs.
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

/// Resolve `SymRef` grids to absolute `BlobSlice` grids.
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

/// Reserve scratch + place data segments, then assemble immutable
/// `FuncDispatch` records. Takes ownership of the classify output and
/// returns a fully-built dispatch list — no back-fill state possible.
pub(super) fn lay_out_static_memory(
    mut per_func: Vec<FuncClassified>,
    funcs: &[&WitFunction],
    schema: &SchemaLayouts,
    names: NameInterner,
    iface_name: BlobSlice,
) -> Result<(Vec<FuncDispatch>, StaticDataPlan)> {
    let n_funcs = per_func.len();
    debug_assert_eq!(
        per_func.len(),
        funcs.len(),
        "FuncClassified list and WitFunction list must be index-aligned",
    );

    check_layout_budget(&per_func)?;

    let mut layout = StaticLayout::new();
    let mut symbols = SymbolBases::new();
    let mut relocs = RelocPlan::new();

    let name_blob = names.into_bytes();
    let _ = layout.place_data(1, &name_blob);

    // Reserve flags scratch before building flags-info so each entry's
    // `set-flags.ptr` lands as an absolute address (no reloc).
    let flags_scratch_addrs: Vec<u32> = flags_scratch_sizes(&per_func)
        .into_iter()
        .map(|n_bytes| layout.reserve_scratch(4, n_bytes))
        .collect();
    // Char scratch: 4 bytes per cell, byte-aligned (i32.store8).
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

    // Per-(fn, field) enum-info / record-info side tables. Builders
    // emit `Segment`s with in-segment relocs; placement order is
    // commutative because every cross-segment ptr is a queued reloc.
    let enum_info_id = symbols.alloc();
    let record_tuples_id = symbols.alloc();
    let tuple_indices_id = symbols.alloc();
    let enum_info = build_enum_info_blob(&mut per_func, &schema.enum_info_layout, enum_info_id);
    let SideTableBlob {
        segment: enum_segment,
        per_param: enum_per_param_sym,
        per_result: enum_per_result_sym,
    } = enum_info;
    // Flags-info entries are per-call (wrapper allocates the buffer);
    // only the set-flags scratch slabs are baked statically.
    let mut flags_scratch_iter = flags_scratch_addrs.iter().copied();
    let FlagsInfoMaps {
        per_cell_fill: flags_per_cell_fill,
        per_result_single_fill: flags_per_result_single_fill,
        per_param_count: flags_per_param_count,
        per_result_count: flags_per_result_count,
    } = build_flags_info_maps(&per_func, &mut flags_scratch_iter);
    debug_assert!(
        flags_scratch_iter.next().is_none(),
        "flags scratch reservations must be consumed once per Cell::Flags",
    );
    // Record-info entries are per-call; static records' field-tuples
    // stay baked in `record_tuples_seg` (list-element records get
    // their own per-call tuples buffer).
    let RecordInfoMaps {
        tuples: record_tuples_seg,
        per_cell_fill: mut record_per_cell_fill,
        per_param_count: record_per_param_count,
        per_result_count: record_per_result_count,
    } = build_record_info_maps(
        &per_func,
        &schema.record_field_tuple_layout,
        record_tuples_id,
    );
    let TupleIndicesBlob {
        segment: tuple_indices_seg,
        per_cell_idx: tuple_indices_per_cell,
    } = build_tuple_indices_blob(&per_func, tuple_indices_id);
    // Variant-info entries are per-call.
    let VariantInfoMaps {
        per_cell_fill: variant_per_cell_fill,
        per_param_count: variant_per_param_count,
        per_result_count: variant_per_result_count,
    } = build_variant_info_maps(&per_func);
    // Handle-info entries are per-call.
    let HandleInfoMaps {
        per_cell_fill: handle_per_cell_fill,
        per_result_single_fill: handle_per_result_single_fill,
        per_param_count: handle_per_param_count,
        per_result_count: handle_per_result_count,
    } = build_handle_info_maps(&per_func);

    // Placement order is commutative; each placement assigns a base
    // and relocs land later.
    let record_tuples_base =
        place_segment(&mut layout, &mut symbols, &mut relocs, record_tuples_seg);
    place_segment(&mut layout, &mut symbols, &mut relocs, enum_segment);
    place_segment(&mut layout, &mut symbols, &mut relocs, tuple_indices_seg);

    // Static record fills' `fields_ptr` is segment-relative; rebase now.
    back_fill_record_fields_ptrs(&mut record_per_cell_fill, record_tuples_base);

    let (enum_per_param, enum_per_result) =
        resolve_param_result_ranges(&symbols, enum_per_param_sym, enum_per_result_sym);

    // Bundle every kind's per-(fn, param) and per-(fn, result)
    // pointers into one `FieldSideTables` per field-tree, so the
    // blob writers don't grow another arg per kind.
    let param_side_tables: Vec<Vec<FieldSideTables>> = (0..n_funcs)
        .map(|fn_idx| {
            (0..per_func[fn_idx].params.len())
                .map(|p_idx| FieldSideTables {
                    enum_infos: enum_per_param[fn_idx][p_idx],
                    flags_infos: BlobSlice {
                        off: 0,
                        len: flags_per_param_count[fn_idx][p_idx],
                    },
                    record_infos: BlobSlice {
                        off: 0,
                        len: record_per_param_count[fn_idx][p_idx],
                    },
                    variant_infos: BlobSlice {
                        off: 0,
                        len: variant_per_param_count[fn_idx][p_idx],
                    },
                    handle_infos: BlobSlice {
                        off: 0,
                        len: handle_per_param_count[fn_idx][p_idx],
                    },
                })
                .collect()
        })
        .collect();
    let result_side_tables: Vec<FieldSideTables> = (0..n_funcs)
        .map(|fn_idx| FieldSideTables {
            enum_infos: enum_per_result[fn_idx],
            flags_infos: BlobSlice {
                off: 0,
                len: flags_per_result_count[fn_idx],
            },
            record_infos: BlobSlice {
                off: 0,
                len: record_per_result_count[fn_idx],
            },
            variant_infos: BlobSlice {
                off: 0,
                len: variant_per_result_count[fn_idx],
            },
            handle_infos: BlobSlice {
                off: 0,
                len: handle_per_result_count[fn_idx],
            },
        })
        .collect();

    // `cells.ptr` left zero (patched per call); side-table pointers baked.
    let fields_blob = build_fields_blob(&per_func, schema, &param_side_tables);
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
    let after_blob = build_after_params_blob(&per_func, schema, iface_name, &result_side_tables);
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

    // Per-fn retptr scratch (only when sig uses one).
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

    // Async indirect-params scratch (canon-lower-async overflowed
    // MAX_FLAT_ASYNC_PARAMS).
    let params_record_offsets: Vec<Option<i32>> = per_func
        .iter()
        .zip(funcs.iter())
        .map(|(fd, func)| {
            if !(matches!(fd.shape, FuncShape::Async(_)) && fd.import_sig.indirect_params) {
                return None;
            }
            let param_types: Vec<Type> = func.params.iter().map(|p| p.ty).collect();
            let info = schema.size_align.record(&param_types);
            let size = info.size.size_wasm32() as u32;
            let align = info.align.align_wasm32() as u32;
            Some(layout.reserve_scratch(align, size) as i32)
        })
        .collect();

    // Bump-allocator start aligned to the schema's max (`cell`).
    let bump_start = layout.end().next_multiple_of(schema.cell_layout.align);
    if bump_start > LAYOUT_SIZE_BUDGET {
        bail!("static-data layout end {bump_start} exceeds i32 budget {LAYOUT_SIZE_BUDGET}");
    }
    let mut data_segments = layout.into_segments();
    // After `into_segments` so segments aren't being mutated.
    relocs.resolve(&symbols, &mut data_segments);

    let dispatches: Vec<FuncDispatch> = per_func
        .into_iter()
        .enumerate()
        .map(|(i, fc)| {
            let params: Vec<ParamLayout> = fc
                .params
                .into_iter()
                .enumerate()
                .map(|(p_idx, lift)| {
                    let tuple_slices = tuple_indices_per_cell.resolve_param(i, p_idx, &symbols);
                    let sources = CellFillSources {
                        record_fill: record_per_cell_fill.for_param(i, p_idx),
                        tuple_indices: &tuple_slices,
                        flags_fill: flags_per_cell_fill.for_param(i, p_idx),
                        variant_fill: variant_per_cell_fill.for_param(i, p_idx),
                        char_scratch: char_scratch_map.for_param(i, p_idx),
                        handle_fill: handle_per_cell_fill.for_param(i, p_idx),
                    };
                    let cell_side = fold_cell_side_data(&lift.plan, &sources);
                    ParamLayout {
                        lift,
                        cell_side,
                        info_counts: InfoCounts {
                            handle: handle_per_param_count[i][p_idx],
                            flags: flags_per_param_count[i][p_idx],
                            record: record_per_param_count[i][p_idx],
                            variant: variant_per_param_count[i][p_idx],
                        },
                    }
                })
                .collect();

            let retptr_offset = retptr_offsets[i];
            let result_lift = fc.result_lift.map(|rl| {
                let ResultLift { source, .. } = rl;
                let layout_source = match source {
                    ResultSource::Direct(cell) => {
                        let fills = SingleCellFills {
                            flags_fill: &flags_per_result_single_fill[i],
                            char_scratch: &char_per_result_single[i],
                            handle_fill: &handle_per_result_single_fill[i],
                        };
                        let side_data = single_cell_side_data(&cell, &fills);
                        ResultSourceLayout::Direct { cell, side_data }
                    }
                    ResultSource::Compound(compound) => {
                        let tuple_slices = tuple_indices_per_cell.resolve_result(i, &symbols);
                        let sources = CellFillSources {
                            record_fill: record_per_cell_fill.for_result(i),
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
                    info_counts: InfoCounts {
                        handle: handle_per_result_count[i],
                        flags: flags_per_result_count[i],
                        record: record_per_result_count[i],
                        variant: variant_per_result_count[i],
                    },
                }
            });

            let after = after_params_offsets[i].map(|params_offset| AfterSetup { params_offset });

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
                params_record_offset: params_record_offsets[i],
                result_lift,
                after,
                borrow_drops: fc.borrow_drops,
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
    //! Layout-phase tests: build a `LayoutEnv`, assert against its
    //! dispatches / plan / schema.
    use super::super::build_per_func_classified;
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

    /// Fully-laid-out dispatch list paired with its schema + plan.
    struct LayoutEnv {
        dispatches: Vec<FuncDispatch>,
        plan: StaticDataPlan,
        schema: SchemaLayouts,
    }

    impl LayoutEnv {
        /// Find a dispatch by export-name substring (e.g. the WIT fn name).
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
        let map_aliases = super::super::lift::desugar_map_aliases(&mut resolve);
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
            build_per_func_classified(&resolve, target_iface, &funcs, &mut names, &map_aliases)
                .unwrap();
        let (dispatches, plan) =
            lay_out_static_memory(classified, &funcs, &schema, names, iface_name).unwrap();
        LayoutEnv {
            dispatches,
            plan,
            schema,
        }
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
        let map_aliases = super::super::lift::desugar_map_aliases(&mut resolve);
        let target_iface =
            super::super::test_utils::iface_by_unversioned_qname(&resolve, target_iface_qname);
        let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
            .functions
            .values()
            .collect();
        let schema = compute_schema(&resolve, world_id, true, true).unwrap();
        let mut names = NameInterner::new();
        let iface_name = names.intern(&target_versioned);
        let classified =
            build_per_func_classified(&resolve, target_iface, &funcs, &mut names, &map_aliases)?;
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
