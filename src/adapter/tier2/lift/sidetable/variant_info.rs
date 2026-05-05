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
use super::super::plan::{Cell, LiftPlan};
use super::{ensure_registered, PerCellIndices, StringTable, INFO_TYPE_NAME};

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

/// Output of [`build_variant_info_blob`]. Mirrors
/// [`super::flags_info::FlagsInfoBlobs`] minus the scratch field
/// (variant case-names live in the shared name blob, not per-cell
/// scratch).
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
    let mut table = StringTable::new();
    for fd in per_func {
        for p in &fd.params {
            for info in p.plan.variant_infos() {
                ensure_registered(&mut table, names, info);
            }
        }
        if let Some(rl) = &fd.result_lift {
            if let Some(c) = rl.compound() {
                for info in c.plan.variant_infos() {
                    ensure_registered(&mut table, names, info);
                }
            }
        }
    }
    table
}

/// Lay out the variant-info entries (one per `Cell::Variant`). All
/// runtime-filled fields (`case-name` slot, `payload` slot) stay
/// zeroed in the segment; the emit phase patches them per call.
pub(crate) fn build_variant_info_blob(
    per_func: &[FuncClassified],
    strings: &StringTable,
    entry_layout: &RecordLayout,
    entries_id: SymbolId,
) -> VariantInfoBlobs {
    let mut builder = VariantInfoBuilder::new(entry_layout, entries_id);
    let mut per_param_range: Vec<Vec<Option<SymRef>>> = Vec::with_capacity(per_func.len());
    let mut per_param_fill: Vec<Vec<Vec<Option<VariantRuntimeFill>>>> =
        Vec::with_capacity(per_func.len());
    let mut per_result_range: Vec<Option<SymRef>> = Vec::with_capacity(per_func.len());
    let mut per_result_fill: Vec<Vec<Option<VariantRuntimeFill>>> =
        Vec::with_capacity(per_func.len());

    for fd in per_func {
        let mut params_ranges = Vec::with_capacity(fd.params.len());
        let mut params_fill = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            let (range, fill_map) = builder.append_plan(&p.plan, strings);
            params_ranges.push(range);
            params_fill.push(fill_map);
        }
        per_param_range.push(params_ranges);
        per_param_fill.push(params_fill);

        // Compound result plans can nest `Cell::Variant`. Direct
        // variant-result lands in Phase 3.
        let (result_range, result_fill_map) =
            match fd.result_lift.as_ref().and_then(|rl| rl.compound()) {
                Some(c) => builder.append_plan(&c.plan, strings),
                None => (None, Vec::new()),
            };
        per_result_range.push(result_range);
        per_result_fill.push(result_fill_map);
    }

    VariantInfoBlobs {
        entries: builder.finish(),
        per_param_range,
        per_result_range,
        per_cell_fill: PerCellIndices {
            per_param: per_param_fill,
            per_result: per_result_fill,
        },
    }
}

struct VariantInfoBuilder<'a> {
    entries: Vec<u8>,
    entry_layout: &'a RecordLayout,
    entries_id: SymbolId,
}

impl<'a> VariantInfoBuilder<'a> {
    fn new(entry_layout: &'a RecordLayout, entries_id: SymbolId) -> Self {
        Self {
            entries: Vec::new(),
            entry_layout,
            entries_id,
        }
    }

    fn append_plan(
        &mut self,
        plan: &LiftPlan,
        strings: &StringTable,
    ) -> (Option<SymRef>, Vec<Option<VariantRuntimeFill>>) {
        let range_start = self.entries.len() as u32;
        let mut count: u32 = 0;
        let mut fill_map: Vec<Option<VariantRuntimeFill>> = vec![None; plan.cells.len()];
        for (cell_pos, op) in plan.cells.iter().enumerate() {
            let Cell::Variant {
                info,
                per_case_payload,
                ..
            } = op
            else {
                continue;
            };
            let s = strings
                .get(&info.type_name)
                .expect("register_variant_strings ran for every info");
            debug_assert_eq!(info.item_names.len(), s.items.len());

            let entry_seg_off = self.entries.len() as u32;
            // `case-name.*` and `payload.*` stay zero — the wrapper
            // patches them per call.
            let entry = RecordWriter::extend_zero(&mut self.entries, self.entry_layout);
            entry.write_slice(&mut self.entries, INFO_TYPE_NAME, s.type_name);

            fill_map[cell_pos] = Some(VariantRuntimeFill {
                side_table_idx: count,
                entry_seg_off,
                case_name_addr: None,
                payload_disc_addr: None,
                payload_value_addr: None,
                case_names: s.items.clone(),
                per_case_payload: per_case_payload.clone(),
            });
            count += 1;
        }
        let range = (count > 0).then_some(SymRef {
            target: self.entries_id,
            off: range_start,
            len: count,
        });
        (range, fill_map)
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
    let patch_one = |f: &mut VariantRuntimeFill| {
        debug_assert!(
            f.case_name_addr.is_none()
                && f.payload_disc_addr.is_none()
                && f.payload_value_addr.is_none(),
            "back_fill_entry_addrs called twice on the same VariantRuntimeFill",
        );
        let entry_off = entries_base + f.entry_seg_off;
        f.case_name_addr = Some((entry_off + case_name_off) as i32);
        f.payload_disc_addr = Some((entry_off + payload_off) as i32);
        f.payload_value_addr = Some((entry_off + payload_off + payload_value_off) as i32);
    };
    let patch_row = |row: &mut [Option<VariantRuntimeFill>]| {
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
}
