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
//!
//! Plugs into [`super::build_per_cell_side_table`] like the other
//! per-cell kinds — the writer just owns an extra tuples buffer +
//! entry-reloc list that the caller stitches onto the result after
//! the framework returns.

use super::super::super::super::abi::emit::RecordLayout;
use super::super::super::blob::{RecordWriter, Reloc, Segment, SymRef, SymbolId};
use super::super::super::schema::{
    RECORD_FIELD_TUPLE_IDX, RECORD_FIELD_TUPLE_NAME, RECORD_INFO_FIELDS,
};
use super::super::super::FuncClassified;
use super::super::plan::Cell;
use super::{
    build_per_cell_side_table, CellEntryWriter, PerCellIndices, PerCellSideTableBlob,
    INFO_TYPE_NAME,
};

/// Output of [`build_record_info_blob`]. Two [`Segment`]s — `entries`
/// carries a [`Reloc`] per record-cell pointing each `fields.ptr` at
/// the matching tuples range; the layout phase resolves both layers
/// in one reloc-pass once each segment has a base.
pub(crate) struct RecordInfoBlobs {
    pub entries: Segment,
    pub tuples: Segment,
    pub per_param_range: Vec<Vec<Option<SymRef>>>,
    pub per_result_range: Vec<Option<SymRef>>,
    /// Per-cell side-table indices: `Some(i)` on `Cell::RecordOf`,
    /// `None` elsewhere.
    pub per_cell_idx: PerCellIndices<u32>,
}

pub(crate) fn build_record_info_blob(
    per_func: &[FuncClassified],
    entry_layout: &RecordLayout,
    tuple_layout: &RecordLayout,
    entries_id: SymbolId,
    tuples_id: SymbolId,
) -> RecordInfoBlobs {
    let mut writer = RecordEntryWriter {
        entry_layout,
        tuple_layout,
        tuples_id,
        tuples: Vec::new(),
        entry_relocs: Vec::new(),
    };
    let PerCellSideTableBlob {
        mut entries,
        per_param_range,
        per_result_range,
        per_cell_fill,
        per_result_single_fill,
    } = build_per_cell_side_table(per_func, entry_layout, entries_id, &mut writer);
    debug_assert!(per_result_single_fill.is_empty());
    // The framework hands back `entries` with empty relocs; stitch
    // the writer's accumulated cross-segment relocs onto it.
    entries.relocs = writer.entry_relocs;
    RecordInfoBlobs {
        entries,
        tuples: Segment {
            id: tuples_id,
            align: tuple_layout.align,
            bytes: writer.tuples,
            relocs: Vec::new(),
        },
        per_param_range,
        per_result_range,
        per_cell_idx: per_cell_fill,
    }
}

struct RecordEntryWriter<'a> {
    entry_layout: &'a RecordLayout,
    tuple_layout: &'a RecordLayout,
    tuples_id: SymbolId,
    /// Owned tuples segment bytes; the framework only owns `entries`.
    tuples: Vec<u8>,
    /// `entries → tuples` relocs accumulated per cell; stitched onto
    /// the entries `Segment` after the framework returns.
    entry_relocs: Vec<Reloc>,
}

impl<'a> CellEntryWriter for RecordEntryWriter<'a> {
    type Fill = u32;
    // `RecordOf` always retptrs — never a sync-flat Direct result.
    const HAS_DIRECT: bool = false;

    fn step_cell(
        &mut self,
        entries: &mut Vec<u8>,
        cell: &Cell,
        side_table_idx: u32,
    ) -> Option<u32> {
        let Cell::RecordOf { type_name, fields } = cell else {
            return None;
        };
        // Append the field tuples first so the entry can record an
        // absolute SymRef into the tuples segment.
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
        let entry = RecordWriter::extend_zero(entries, self.entry_layout);
        entry.write_slice(entries, INFO_TYPE_NAME, *type_name);
        let tuples_ref = (tuples_len > 0).then_some(SymRef {
            target: self.tuples_id,
            off: tuples_off,
            len: tuples_len,
        });
        entry.write_slice_reloc(
            entries,
            &mut self.entry_relocs,
            RECORD_INFO_FIELDS,
            tuples_ref,
        );
        Some(side_table_idx)
    }
}
