//! Handle-info side-table builder. One entry per `Cell::Handle`
//! appearance (two `own<R>` params need two distinct `id` slots).
//! `type-name` is baked at plan-build time; only `id: u64` is
//! runtime-filled (zero-extension of the i32 handle bits).

use super::super::super::super::abi::emit::{BlobSlice, RecordLayout};
use super::super::super::blob::{RecordWriter, Segment, SymRef, SymbolId};
use super::super::super::schema::HANDLE_INFO_ID;
use super::super::super::FuncClassified;
use super::super::classify::ResultSource;
use super::super::plan::{Cell, LiftPlan};
use super::{walk_per_cell_plans, PerCellIndices, PerCellPlanWalk, INFO_TYPE_NAME};

/// Per-(plan-cell) emit-phase data for one `Cell::Handle`.
#[derive(Clone, Debug)]
pub(crate) struct HandleRuntimeFill {
    /// Range-relative index — the `cell::resource-handle(u32)` payload.
    pub side_table_idx: u32,
    /// Entry's byte offset within the entries segment. Combined with
    /// `entries_base` by [`back_fill_id_addrs`] (range-relative
    /// `side_table_idx` alone won't recover the absolute slot).
    pub entry_seg_off: u32,
    /// Absolute address of the `id: u64` slot. `None` until back-fill;
    /// emit-time `expect` turns missed back-fill into a build panic.
    pub id_addr: Option<i32>,
}

pub(crate) struct HandleInfoBlobs {
    pub entries: Segment,
    pub per_param_range: Vec<Vec<Option<SymRef>>>,
    pub per_result_range: Vec<Option<SymRef>>,
    pub per_cell_fill: PerCellIndices<HandleRuntimeFill>,
    /// Per-fn fill for a single-cell handle result (Direct or
    /// RetptrPair — both lift one `Cell::Handle`, with no plan to
    /// attach it to). `Some` when the func's result classifies as a
    /// single `Cell::Handle`. Mirrors flags's per-result-single shape.
    pub per_result_single_fill: Vec<Option<HandleRuntimeFill>>,
}

/// One `handle-info` entry per `Cell::Handle` (param plan, compound
/// result plan, or single-cell Direct/RetptrPair handle result). The
/// param + compound-result walks run via [`walk_per_cell_plans`];
/// single-cell handle results need a separate per-fn pass since the
/// `Cell::Handle` lives directly on `ResultSource::Direct` /
/// `ResultSource::RetptrPair`, not inside a plan.
pub(crate) fn build_handle_info_blob(
    per_func: &[FuncClassified],
    entry_layout: &RecordLayout,
    entries_id: SymbolId,
) -> HandleInfoBlobs {
    let mut builder = HandleInfoBuilder::new(entry_layout, entries_id);
    let PerCellPlanWalk {
        per_param_range,
        per_param_fill,
        per_result_range: walked_result_range,
        per_result_fill,
    } = walk_per_cell_plans(per_func, |plan| builder.append_plan(plan));
    // walk_per_cell_plans only visits compound results; layer
    // single-cell handle results on top, in fn order. classify
    // guarantees these two paths are mutually exclusive (a result is
    // either Compound or Direct/RetptrPair, never both).
    let mut per_result_range: Vec<Option<SymRef>> = Vec::with_capacity(per_func.len());
    let mut per_result_single_fill: Vec<Option<HandleRuntimeFill>> =
        Vec::with_capacity(per_func.len());
    for (fn_idx, fd) in per_func.iter().enumerate() {
        let walked = walked_result_range[fn_idx];
        let single_handle = fd.result_lift.as_ref().and_then(|rl| match &rl.source {
            ResultSource::Direct(Cell::Handle { type_name, .. })
            | ResultSource::RetptrPair(Cell::Handle { type_name, .. }) => Some(*type_name),
            _ => None,
        });
        match (walked, single_handle) {
            (Some(_), Some(_)) => unreachable!(
                "Compound + single-cell handle result on same fn — classify invariant broken"
            ),
            (None, Some(type_name)) => {
                let (range, fill) = builder.append_direct(type_name);
                per_result_range.push(Some(range));
                per_result_single_fill.push(Some(fill));
            }
            (range, None) => {
                per_result_range.push(range);
                per_result_single_fill.push(None);
            }
        }
    }
    HandleInfoBlobs {
        entries: builder.finish(),
        per_param_range,
        per_result_range,
        per_cell_fill: PerCellIndices {
            per_param: per_param_fill,
            per_result: per_result_fill,
        },
        per_result_single_fill,
    }
}

struct HandleInfoBuilder<'a> {
    entries: Vec<u8>,
    entry_layout: &'a RecordLayout,
    entries_id: SymbolId,
}

impl<'a> HandleInfoBuilder<'a> {
    fn new(entry_layout: &'a RecordLayout, entries_id: SymbolId) -> Self {
        Self {
            entries: Vec::new(),
            entry_layout,
            entries_id,
        }
    }

    fn append_plan(&mut self, plan: &LiftPlan) -> (Option<SymRef>, Vec<Option<HandleRuntimeFill>>) {
        let range_start = self.entries.len() as u32;
        let mut count: u32 = 0;
        let mut fill_map: Vec<Option<HandleRuntimeFill>> = vec![None; plan.cells.len()];
        for (cell_pos, op) in plan.cells.iter().enumerate() {
            let Cell::Handle { type_name, .. } = op else {
                continue;
            };
            let fill = self.append_one(*type_name, count);
            fill_map[cell_pos] = Some(fill);
            count += 1;
        }
        let range = (count > 0).then_some(SymRef {
            target: self.entries_id,
            off: range_start,
            len: count,
        });
        (range, fill_map)
    }

    /// Append one entry for a single-cell handle result (Direct or
    /// RetptrPair). Mirrors flags's no-plan case.
    fn append_direct(&mut self, type_name: BlobSlice) -> (SymRef, HandleRuntimeFill) {
        let range_start = self.entries.len() as u32;
        let fill = self.append_one(type_name, 0);
        let range = SymRef {
            target: self.entries_id,
            off: range_start,
            len: 1,
        };
        (range, fill)
    }

    /// Shared body: write one zeroed entry, fill type-name (`id`
    /// stays zero — the wrapper patches it per call), return the
    /// matching [`HandleRuntimeFill`].
    fn append_one(&mut self, type_name: BlobSlice, side_table_idx: u32) -> HandleRuntimeFill {
        let entry_seg_off = self.entries.len() as u32;
        let entry = RecordWriter::extend_zero(&mut self.entries, self.entry_layout);
        entry.write_slice(&mut self.entries, INFO_TYPE_NAME, type_name);
        HandleRuntimeFill {
            side_table_idx,
            entry_seg_off,
            id_addr: None,
        }
    }

    fn finish(self) -> Segment {
        Segment {
            id: self.entries_id,
            align: self.entry_layout.align,
            bytes: self.entries,
            relocs: Vec::new(),
        }
    }
}

/// Patch each per-cell `id_addr = entries_base + entry_seg_off +
/// offset_of(id)` once the entries segment has a base.
pub(crate) fn back_fill_id_addrs(
    fill: &mut PerCellIndices<HandleRuntimeFill>,
    single_fill: &mut [Option<HandleRuntimeFill>],
    entries_base: u32,
    entry_layout: &RecordLayout,
) {
    let id_off = entry_layout.offset_of(HANDLE_INFO_ID);
    let patch_one = |f: &mut HandleRuntimeFill| {
        debug_assert!(
            f.id_addr.is_none(),
            "back_fill_id_addrs called twice on the same HandleRuntimeFill",
        );
        f.id_addr = Some((entries_base + f.entry_seg_off + id_off) as i32);
    };
    let patch_row = |row: &mut [Option<HandleRuntimeFill>]| {
        for slot in row.iter_mut() {
            if let Some(f) = slot.as_mut() {
                patch_one(f);
            }
        }
    };
    for fn_row in fill.per_param.iter_mut() {
        for param_row in fn_row.iter_mut() {
            patch_row(param_row);
        }
    }
    for fn_row in fill.per_result.iter_mut() {
        patch_row(fn_row);
    }
    patch_row(single_fill);
}
