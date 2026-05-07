//! Handle-info side-table builder. One entry per `Cell::Handle`
//! appearance (own/borrow/stream/future all share this side-table —
//! only the cell-disc differs). `type-name` is baked at plan-build
//! time; only `id: u64` is runtime-filled (zero-extension of the
//! i32 handle bits).

use super::super::super::super::abi::emit::{BlobSlice, RecordLayout};
use super::super::super::blob::{RecordWriter, Segment, SymRef, SymbolId};
use super::super::super::schema::HANDLE_INFO_ID;
use super::super::super::FuncClassified;
use super::super::classify::ResultSource;
use super::super::plan::Cell;
use super::{
    back_fill_per_cell, build_per_cell_side_table, CellEntryWriter, PerCellIndices,
    PerCellSideTableBlob, INFO_TYPE_NAME,
};

/// Per-(plan-cell) emit-phase data for one `Cell::Handle`.
#[derive(Clone, Debug)]
pub(crate) struct HandleRuntimeFill {
    /// Range-relative index — the `cell::{resource,stream,future}-handle(u32)`
    /// payload. Which cell-disc emits is picked from the cell's
    /// `kind` at emit time.
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
    /// Per-fn fill for a Direct (sync flat) `Cell::Handle` result —
    /// no plan to attach it to since `lcl.result` is the source.
    /// Retptr-loaded handle results route through Compound and
    /// register via `per_cell_fill`.
    pub per_result_single_fill: Vec<Option<HandleRuntimeFill>>,
}

/// One `handle-info` entry per `Cell::Handle` (param plan, compound
/// result plan, or Direct sync-flat handle result).
pub(crate) fn build_handle_info_blob(
    per_func: &[FuncClassified],
    entry_layout: &RecordLayout,
    entries_id: SymbolId,
) -> HandleInfoBlobs {
    let mut writer = HandleEntryWriter { entry_layout };
    let PerCellSideTableBlob {
        entries,
        per_param_range,
        per_result_range,
        per_cell_fill,
        per_result_single_fill,
    } = build_per_cell_side_table(per_func, entry_layout, entries_id, &mut writer);
    HandleInfoBlobs {
        entries,
        per_param_range,
        per_result_range,
        per_cell_fill,
        per_result_single_fill,
    }
}

struct HandleEntryWriter<'a> {
    entry_layout: &'a RecordLayout,
}

impl<'a> HandleEntryWriter<'a> {
    /// Write one zeroed entry: `type-name` baked, `id` patched per
    /// call. Returns the matching [`HandleRuntimeFill`].
    fn append_one(
        &mut self,
        entries: &mut Vec<u8>,
        type_name: BlobSlice,
        idx: u32,
    ) -> HandleRuntimeFill {
        let entry_seg_off = entries.len() as u32;
        let entry = RecordWriter::extend_zero(entries, self.entry_layout);
        entry.write_slice(entries, INFO_TYPE_NAME, type_name);
        HandleRuntimeFill {
            side_table_idx: idx,
            entry_seg_off,
            id_addr: None,
        }
    }
}

impl<'a> CellEntryWriter for HandleEntryWriter<'a> {
    type Fill = HandleRuntimeFill;

    fn step_cell(
        &mut self,
        entries: &mut Vec<u8>,
        cell: &Cell,
        side_table_idx: u32,
    ) -> Option<HandleRuntimeFill> {
        let Cell::Handle { type_name, .. } = cell else {
            return None;
        };
        Some(self.append_one(entries, *type_name, side_table_idx))
    }

    fn direct_for(
        &mut self,
        entries: &mut Vec<u8>,
        fd: &FuncClassified,
    ) -> Option<HandleRuntimeFill> {
        let type_name = match &fd.result_lift.as_ref()?.source {
            ResultSource::Direct(Cell::Handle { type_name, .. }) => *type_name,
            _ => return None,
        };
        Some(self.append_one(entries, type_name, 0))
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
    back_fill_per_cell(fill, single_fill, |f| {
        // Always-on — see flags_info::back_fill_len_addrs.
        assert!(
            f.id_addr.is_none(),
            "back_fill_id_addrs called twice on the same HandleRuntimeFill",
        );
        f.id_addr = Some((entries_base + f.entry_seg_off + id_off) as i32);
    });
}
