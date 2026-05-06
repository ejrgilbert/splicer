//! Record-info side-table builder.
//!
//! Different shape from enum-info: enum-info's side table has one
//! entry per case (laid out per-type, indexed by runtime disc).
//! record-info's side table has one entry per *record cell instance*
//! (laid out per-(fn, param), indexed by an adapter-build-time-known
//! constant). Each entry's `fields: list<tuple<string, u32>>` lives
//! in a separate tuples blob; the record-info entry stores a slice
//! pointer into it. Two segments to place, two layers of pointer
//! patching.

use super::super::super::super::abi::emit::RecordLayout;
use super::super::super::blob::{RecordWriter, Reloc, Segment, SymRef, SymbolId};
use super::super::super::schema::{
    RECORD_FIELD_TUPLE_IDX, RECORD_FIELD_TUPLE_NAME, RECORD_INFO_FIELDS,
};
use super::super::super::FuncClassified;
use super::super::plan::{Cell, LiftPlan};
use super::{walk_per_cell_plans, PerCellIndices, PerCellPlanWalk, INFO_TYPE_NAME};

/// Output of [`build_record_info_blob`]. Two [`Segment`]s — the
/// `entries` segment carries one [`Reloc`] per record-cell, pointing
/// each entry's `fields.ptr` at the matching range inside the
/// `tuples` segment. Per-(fn, param) range pointers are [`SymRef`]s
/// into `entries`; the layout phase resolves both layers in one
/// reloc-pass once each segment has a base.
pub(crate) struct RecordInfoBlobs {
    /// `record-info` entries: one entry per `Cell::RecordOf` across
    /// all plans, laid out per-(fn, param) in plan order. Carries
    /// relocs for each entry's `fields.ptr` → tuples segment.
    pub entries: Segment,
    /// `(name, cell-idx)` tuples arena, referenced from each entry's
    /// `fields: list<tuple<string, u32>>`.
    pub tuples: Segment,
    /// Per (fn, param): the param's contiguous record-info range,
    /// targeting the entries segment. `None` for params with no
    /// `RecordOf` cells.
    pub per_param_range: Vec<Vec<Option<SymRef>>>,
    /// Per (fn): result-side range. `None` for void / non-Compound
    /// results; populated for `Compound` results so the result tree's
    /// `record-infos` slot can patch in.
    pub per_result_range: Vec<Option<SymRef>>,
    /// Per-cell side-table indices: `Some(i)` on `Cell::RecordOf`
    /// cells, `None` elsewhere. Indexed via
    /// [`PerCellIndices::for_param`] / [`PerCellIndices::for_result`].
    pub per_cell_idx: PerCellIndices<u32>,
}

/// Accumulator for [`build_record_info_blob`]: bundles the two
/// segment buffers + reloc list + their static layouts/ids so each
/// plan can be appended via a one-arg method.
struct RecordInfoBuilder<'a> {
    entries: Vec<u8>,
    tuples: Vec<u8>,
    entry_relocs: Vec<Reloc>,
    entry_layout: &'a RecordLayout,
    tuple_layout: &'a RecordLayout,
    entries_id: SymbolId,
    tuples_id: SymbolId,
}

impl<'a> RecordInfoBuilder<'a> {
    fn new(
        entry_layout: &'a RecordLayout,
        tuple_layout: &'a RecordLayout,
        entries_id: SymbolId,
        tuples_id: SymbolId,
    ) -> Self {
        Self {
            entries: Vec::new(),
            tuples: Vec::new(),
            entry_relocs: Vec::new(),
            entry_layout,
            tuple_layout,
            entries_id,
            tuples_id,
        }
    }

    /// Append entries for one plan's `Cell::RecordOf` cells; returns
    /// the contiguous range [`SymRef`] (into the entries segment) +
    /// the per-cell side-table index map. `None` for plans with no
    /// `RecordOf` cells. Each entry's `fields.ptr` slot gets a
    /// [`Reloc`] into the tuples segment. Type-name and field-name
    /// strings are read straight off each cell's pre-interned
    /// [`super::super::super::super::abi::emit::BlobSlice`]s.
    fn append_plan(&mut self, plan: &LiftPlan) -> (Option<SymRef>, Vec<Option<u32>>) {
        let range_start = self.entries.len() as u32;
        let mut count: u32 = 0;
        let mut cell_idx_map: Vec<Option<u32>> = vec![None; plan.cells.len()];
        for (cell_pos, op) in plan.cells.iter().enumerate() {
            let Cell::RecordOf { type_name, fields } = op else {
                continue;
            };
            let side_idx = count;
            cell_idx_map[cell_pos] = Some(side_idx);
            count += 1;

            let tuples_off = self.tuples.len() as u32;
            let tuples_len = fields.len() as u32;
            for (field_name, child_cell_idx) in fields {
                let tuple = RecordWriter::extend_zero(&mut self.tuples, self.tuple_layout);
                tuple.write_slice(&mut self.tuples, RECORD_FIELD_TUPLE_NAME, *field_name);
                tuple.write_i32(
                    &mut self.tuples,
                    RECORD_FIELD_TUPLE_IDX,
                    *child_cell_idx as i32,
                );
            }

            let entry = RecordWriter::extend_zero(&mut self.entries, self.entry_layout);
            entry.write_slice(&mut self.entries, INFO_TYPE_NAME, *type_name);
            let tuples_ref = (tuples_len > 0).then_some(SymRef {
                target: self.tuples_id,
                off: tuples_off,
                len: tuples_len,
            });
            entry.write_slice_reloc(
                &mut self.entries,
                &mut self.entry_relocs,
                RECORD_INFO_FIELDS,
                tuples_ref,
            );
        }
        let range = (count > 0).then_some(SymRef {
            target: self.entries_id,
            off: range_start,
            len: count,
        });
        (range, cell_idx_map)
    }

    /// Finalize into the two segments. Tuples carry no relocs of
    /// their own — every cross-segment pointer originates in the
    /// entries segment.
    fn finish(self) -> (Segment, Segment) {
        (
            Segment {
                id: self.entries_id,
                align: self.entry_layout.align,
                bytes: self.entries,
                relocs: self.entry_relocs,
            },
            Segment {
                id: self.tuples_id,
                align: self.tuple_layout.align,
                bytes: self.tuples,
                relocs: Vec::new(),
            },
        )
    }
}

/// One `record-info` entry per `Cell::RecordOf` (param + compound
/// result plans). The entry's side-table index is its position in
/// that plan's contiguous range.
pub(crate) fn build_record_info_blob(
    per_func: &[FuncClassified],
    entry_layout: &RecordLayout,
    tuple_layout: &RecordLayout,
    entries_id: SymbolId,
    tuples_id: SymbolId,
) -> RecordInfoBlobs {
    let mut builder = RecordInfoBuilder::new(entry_layout, tuple_layout, entries_id, tuples_id);
    let PerCellPlanWalk {
        per_param_range,
        per_param_fill: per_param_cell_idx,
        per_result_range,
        per_result_fill: per_result_cell_idx,
    } = walk_per_cell_plans(per_func, |plan| builder.append_plan(plan));
    let (entries_seg, tuples_seg) = builder.finish();
    RecordInfoBlobs {
        entries: entries_seg,
        tuples: tuples_seg,
        per_param_range,
        per_result_range,
        per_cell_idx: PerCellIndices {
            per_param: per_param_cell_idx,
            per_result: per_result_cell_idx,
        },
    }
}
