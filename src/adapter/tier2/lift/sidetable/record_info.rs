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

use std::collections::HashMap;

use super::super::super::super::abi::emit::{BlobSlice, RecordLayout};
use super::super::super::blob::{RecordWriter, Reloc, Segment, SymRef, SymbolId};
use super::super::super::schema::{
    RECORD_FIELD_TUPLE_IDX, RECORD_FIELD_TUPLE_NAME, RECORD_INFO_FIELDS,
};
use super::super::super::FuncClassified;
use super::super::plan::{Cell, LiftPlan};
use super::{append_string, INFO_TYPE_NAME};

/// Per-record-type strings registered in the shared `name_blob`.
/// Field-name strings dedupe per record type — two params of the
/// same record type reuse the strings. Cross-type collisions (e.g.
/// `"name"` appearing in `person` and `pet`) currently get
/// registered twice; promote to global string-dedup if it shows up
/// in profiling.
pub(crate) struct RecordTypeStrings {
    pub type_name: BlobSlice,
    /// Per field, in WIT declaration order.
    pub field_names: Vec<BlobSlice>,
}

pub(crate) type RecordStringTable = HashMap<String, RecordTypeStrings>;

/// Walk every plan's [`Cell::RecordOf`] (params + compound results);
/// for each record type seen, register its `type-name` + each
/// `field-name` into `name_blob` (deduped per type-name). Result keyed
/// by record type-name.
pub(crate) fn register_record_strings(
    per_func: &[FuncClassified],
    name_blob: &mut Vec<u8>,
) -> RecordStringTable {
    let mut table = RecordStringTable::new();
    let register_plan =
        |plan: &LiftPlan, name_blob: &mut Vec<u8>, table: &mut RecordStringTable| {
            for (type_name, fields) in plan.record_ofs() {
                if !table.contains_key(type_name) {
                    let tn = append_string(name_blob, type_name);
                    let fns = fields
                        .iter()
                        .map(|(name, _)| append_string(name_blob, name))
                        .collect();
                    table.insert(
                        type_name.to_string(),
                        RecordTypeStrings {
                            type_name: tn,
                            field_names: fns,
                        },
                    );
                }
            }
        };
    for fd in per_func {
        for p in &fd.params {
            register_plan(&p.plan, name_blob, &mut table);
        }
        if let Some(c) = fd.result_lift.as_ref().and_then(|rl| rl.compound()) {
            register_plan(&c.plan, name_blob, &mut table);
        }
    }
    table
}

/// Per-(fn, param, plan-cell) record-info side-table indices. Wraps
/// the raw triple-`Vec` so call sites read through [`for_param`]
/// instead of three layers of `[i][j][k]`.
///
/// [`for_param`]: RecordInfoIndices::for_param
pub(crate) struct RecordInfoIndices {
    /// One Vec per function; each holds one Vec per param; each holds
    /// per-plan-cell side-table indices (`None` for non-`RecordOf`
    /// cells). The lift codegen reads this when emitting
    /// `cell::record-of(idx)`.
    per_param: Vec<Vec<Vec<Option<u32>>>>,
}

impl RecordInfoIndices {
    /// Borrow one param's per-cell index map.
    pub(crate) fn for_param(&self, fn_idx: usize, param_idx: usize) -> &[Option<u32>] {
        &self.per_param[fn_idx][param_idx]
    }
}

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
    /// Per (fn, param, plan-cell): record-info side-table index
    /// (`None` for non-`RecordOf` cells). See [`RecordInfoIndices`].
    pub per_param_cell_idx: RecordInfoIndices,
    /// Per (fn): result-side range. `None` for void / non-Compound
    /// results; populated for `Compound` results so the result tree's
    /// `record-infos` slot can patch in.
    pub per_result_range: Vec<Option<SymRef>>,
    /// Per (fn): for each cell of the result's plan, its assigned
    /// record-info side-table index (None for non-`RecordOf` cells).
    /// Empty Vec for non-Compound results.
    pub per_result_cell_idx: Vec<Vec<Option<u32>>>,
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
    strings: &'a RecordStringTable,
}

impl<'a> RecordInfoBuilder<'a> {
    fn new(
        entry_layout: &'a RecordLayout,
        tuple_layout: &'a RecordLayout,
        entries_id: SymbolId,
        tuples_id: SymbolId,
        strings: &'a RecordStringTable,
    ) -> Self {
        Self {
            entries: Vec::new(),
            tuples: Vec::new(),
            entry_relocs: Vec::new(),
            entry_layout,
            tuple_layout,
            entries_id,
            tuples_id,
            strings,
        }
    }

    /// Append entries for one plan's `Cell::RecordOf` cells; returns
    /// the contiguous range [`SymRef`] (into the entries segment) +
    /// the per-cell side-table index map. `None` for plans with no
    /// `RecordOf` cells. Each entry's `fields.ptr` slot gets a
    /// [`Reloc`] into the tuples segment.
    fn append_plan(&mut self, plan: &LiftPlan) -> (Option<SymRef>, Vec<Option<u32>>) {
        let range_start = self.entries.len() as u32;
        let mut count: u32 = 0;
        let mut cell_idx_map: Vec<Option<u32>> = vec![None; plan.cells.len()];
        for (cell_pos, op) in plan.cells.iter().enumerate() {
            let Cell::RecordOf { type_name, fields } = op else {
                continue;
            };
            let s = self
                .strings
                .get(type_name.as_str())
                .expect("register_record_strings registered every record type");
            let side_idx = count;
            cell_idx_map[cell_pos] = Some(side_idx);
            count += 1;

            let tuples_off = self.tuples.len() as u32;
            let tuples_len = fields.len() as u32;
            for (i, (_, child_cell_idx)) in fields.iter().enumerate() {
                let tuple = RecordWriter::extend_zero(&mut self.tuples, self.tuple_layout);
                tuple.write_slice(&mut self.tuples, RECORD_FIELD_TUPLE_NAME, s.field_names[i]);
                tuple.write_i32(
                    &mut self.tuples,
                    RECORD_FIELD_TUPLE_IDX,
                    *child_cell_idx as i32,
                );
            }

            let entry = RecordWriter::extend_zero(&mut self.entries, self.entry_layout);
            entry.write_slice(&mut self.entries, INFO_TYPE_NAME, s.type_name);
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

/// Lay out the per-(fn, param) and per-(fn, compound-result) record-
/// info entries + their (name, cell-idx) tuples arena. Each
/// `Cell::RecordOf` in a plan contributes one entry; the entry's
/// side-table index is its position in that plan's contiguous range.
pub(crate) fn build_record_info_blob(
    per_func: &[FuncClassified],
    strings: &RecordStringTable,
    entry_layout: &RecordLayout,
    tuple_layout: &RecordLayout,
    entries_id: SymbolId,
    tuples_id: SymbolId,
) -> RecordInfoBlobs {
    let mut builder =
        RecordInfoBuilder::new(entry_layout, tuple_layout, entries_id, tuples_id, strings);
    let mut per_param_range: Vec<Vec<Option<SymRef>>> = Vec::with_capacity(per_func.len());
    let mut per_param_cell_idx: Vec<Vec<Vec<Option<u32>>>> = Vec::with_capacity(per_func.len());
    let mut per_result_range: Vec<Option<SymRef>> = Vec::with_capacity(per_func.len());
    let mut per_result_cell_idx: Vec<Vec<Option<u32>>> = Vec::with_capacity(per_func.len());

    for fd in per_func {
        let mut params_ranges = Vec::with_capacity(fd.params.len());
        let mut params_cell_idx = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            let (range, cell_idx_map) = builder.append_plan(&p.plan);
            params_ranges.push(range);
            params_cell_idx.push(cell_idx_map);
        }
        per_param_range.push(params_ranges);
        per_param_cell_idx.push(params_cell_idx);

        let (result_range, result_cell_idx_map) =
            match fd.result_lift.as_ref().and_then(|rl| rl.compound()) {
                Some(c) => builder.append_plan(&c.plan),
                None => (None, Vec::new()),
            };
        per_result_range.push(result_range);
        per_result_cell_idx.push(result_cell_idx_map);
    }

    let (entries_seg, tuples_seg) = builder.finish();
    RecordInfoBlobs {
        entries: entries_seg,
        tuples: tuples_seg,
        per_param_range,
        per_param_cell_idx: RecordInfoIndices {
            per_param: per_param_cell_idx,
        },
        per_result_range,
        per_result_cell_idx,
    }
}
