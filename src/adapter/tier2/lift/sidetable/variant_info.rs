//! Variant-info side-table builder. Per-cell entries laid out like
//! [`super::flags_info`], but each entry has *two* runtime-filled
//! fields: `case-name` (one of N pre-interned case-name slices,
//! selected per call by disc) and `payload` (an `option<u32>`
//! pointing at the active arm's child cell when non-unit). The
//! wrapper's N-way disc dispatch picks which case-name + payload to
//! write per call.

use super::super::super::super::abi::emit::{BlobSlice, RecordLayout};
use super::super::super::blob::{NameInterner, RecordWriter, Segment, SymRef, SymbolId};
use super::super::super::schema::{VARIANT_INFO_CASE_NAME, VARIANT_INFO_PAYLOAD};
use super::super::super::FuncClassified;
use super::super::plan::Cell;
use super::{
    back_fill_per_cell, build_per_cell_side_table, register_side_table_strings, CellEntryWriter,
    PerCellIndices, PerCellSideTableBlob, StringTable, INFO_TYPE_NAME,
};

/// Per-(plan-cell) emit-phase data for one `Cell::Variant`.
#[derive(Clone, Debug)]
pub(crate) struct VariantRuntimeFill {
    /// `cell::variant-case(u32)` payload — range-relative index.
    pub side_table_idx: u32,
    /// Byte offset of *this* entry within the entries segment.
    /// Combined with `entries_base` by [`back_fill_entry_addrs`] to
    /// recover the absolute slot addresses.
    pub entry_seg_off: u32,
    /// Absolute address of the entry's `case-name` `(ptr, len)` slot.
    /// `None` until [`back_fill_entry_addrs`] runs.
    pub case_name_addr: Option<i32>,
    /// Absolute address of the entry's `payload` field's option-disc
    /// byte (option<u32>'s disc at offset 0). `None` until back-fill.
    pub payload_disc_addr: Option<i32>,
    /// Absolute address of the entry's `payload` field's u32 slot
    /// (option<u32>'s value at the schema-derived sub-offset).
    /// `None` until back-fill.
    pub payload_value_addr: Option<i32>,
    /// Pre-interned `(off, len)` of each case-name, in disc order.
    pub case_names: Vec<BlobSlice>,
    /// Per-case child cell idx, in disc order. `None` for unit cases.
    pub per_case_payload: Vec<Option<u32>>,
}

pub(crate) struct VariantInfoBlobs {
    pub entries: Segment,
    pub per_param_range: Vec<Vec<Option<SymRef>>>,
    pub per_result_range: Vec<Option<SymRef>>,
    pub per_cell_fill: PerCellIndices<VariantRuntimeFill>,
}

/// Intern type-name + case-names for every `Cell::Variant` across all
/// param plans + compound result plans. Direct variant-result is
/// Phase 3.
pub(crate) fn register_variant_strings(
    per_func: &[FuncClassified],
    names: &mut NameInterner,
) -> StringTable {
    register_side_table_strings(
        per_func,
        names,
        |plan, visit| plan.variant_infos().for_each(visit),
        // No Direct variant result today — compound walk catches every
        // info via the plan.
        |_| None,
    )
}

/// One variant-info entry per `Cell::Variant`. Runtime-filled
/// fields (`case-name`, `payload`) stay zeroed in the segment;
/// the wrapper patches them per call.
pub(crate) fn build_variant_info_blob(
    per_func: &[FuncClassified],
    strings: &StringTable,
    entry_layout: &RecordLayout,
    entries_id: SymbolId,
) -> VariantInfoBlobs {
    let mut writer = VariantEntryWriter {
        entry_layout,
        strings,
    };
    let PerCellSideTableBlob {
        entries,
        per_param_range,
        per_result_range,
        per_cell_fill,
        per_result_single_fill,
    } = build_per_cell_side_table(per_func, entry_layout, entries_id, &mut writer);
    // `HAS_DIRECT = false` ⇒ framework never produces single fills.
    debug_assert!(per_result_single_fill.is_empty());
    VariantInfoBlobs {
        entries,
        per_param_range,
        per_result_range,
        per_cell_fill,
    }
}

struct VariantEntryWriter<'a> {
    entry_layout: &'a RecordLayout,
    strings: &'a StringTable,
}

impl<'a> CellEntryWriter for VariantEntryWriter<'a> {
    type Fill = VariantRuntimeFill;
    // Variant always retptrs — classify never produces a Direct.
    const HAS_DIRECT: bool = false;

    fn step_cell(
        &mut self,
        entries: &mut Vec<u8>,
        cell: &Cell,
        side_table_idx: u32,
    ) -> Option<VariantRuntimeFill> {
        let Cell::Variant {
            info,
            per_case_payload,
            ..
        } = cell
        else {
            return None;
        };
        let s = self
            .strings
            .get(&info.type_name)
            .expect("register_variant_strings ran for every info");
        debug_assert_eq!(info.item_names.len(), s.items.len());

        let entry_seg_off = entries.len() as u32;
        // `case-name.*` and `payload.*` stay zero — the wrapper
        // patches them per call.
        let entry = RecordWriter::extend_zero(entries, self.entry_layout);
        entry.write_slice(entries, INFO_TYPE_NAME, s.type_name);

        Some(VariantRuntimeFill {
            side_table_idx,
            entry_seg_off,
            case_name_addr: None,
            payload_disc_addr: None,
            payload_value_addr: None,
            case_names: s.items.clone(),
            per_case_payload: per_case_payload.clone(),
        })
    }
}

/// Patch each per-cell `case_name_addr` / `payload_disc_addr` /
/// `payload_value_addr` once the entries segment has a base.
/// `payload_value_off` is the byte offset of the `option<u32>`
/// payload's u32 slot within the option (schema-derived; lives on
/// `SchemaLayouts::variant_info_payload_value_off`).
pub(crate) fn back_fill_entry_addrs(
    fill: &mut PerCellIndices<VariantRuntimeFill>,
    entries_base: u32,
    entry_layout: &RecordLayout,
    payload_value_off: u32,
) {
    let case_name_off = entry_layout.offset_of(VARIANT_INFO_CASE_NAME);
    let payload_off = entry_layout.offset_of(VARIANT_INFO_PAYLOAD);
    back_fill_per_cell(fill, &mut [], |f| {
        // Always-on — see flags_info::back_fill_len_addrs.
        assert!(
            f.case_name_addr.is_none()
                && f.payload_disc_addr.is_none()
                && f.payload_value_addr.is_none(),
            "back_fill_entry_addrs called twice on the same VariantRuntimeFill",
        );
        let entry_off = entries_base + f.entry_seg_off;
        f.case_name_addr = Some((entry_off + case_name_off) as i32);
        f.payload_disc_addr = Some((entry_off + payload_off) as i32);
        f.payload_value_addr = Some((entry_off + payload_off + payload_value_off) as i32);
    });
}
