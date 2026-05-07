//! Flags-info side-table builder. Per-cell entries laid out like
//! record-info, but each entry's `set-flags` list is *runtime-filled*:
//! `set-flags.ptr` is baked at build time pointing at a per-cell
//! scratch buffer; the wrapper bit-walks the i32 bitmask each call to
//! write `(name_ptr, name_len)` pairs into scratch and patch
//! `set-flags.len`. Static-sized scratch (count is build-time-known)
//! avoids `cabi_realloc` traffic on the hot path.

use super::super::super::super::abi::emit::{
    BlobSlice, RecordLayout, SLICE_LEN_OFFSET, STRING_FLAT_BYTES,
};
use super::super::super::blob::{NameInterner, RecordWriter, Segment, SymRef, SymbolId};
use super::super::super::schema::FLAGS_INFO_SET_FLAGS;
use super::super::super::FuncClassified;
use super::super::plan::{Cell, LiftPlan, NamedListInfo};
use super::{
    back_fill_per_cell, build_per_cell_side_table, register_side_table_strings, CellEntryWriter,
    PerCellIndices, PerCellSideTableBlob, StringTable, INFO_TYPE_NAME,
};

/// Per-(plan-cell) emit-phase data for one `Cell::Flags`.
#[derive(Clone, Debug)]
pub(crate) struct FlagsRuntimeFill {
    /// `cell::flags-set(u32)` payload — range-relative index into the
    /// field-tree's `flags-infos` slice (one slice per (fn, param |
    /// result), so the same idx 0 maps to different segment bytes).
    pub side_table_idx: u32,
    /// Byte offset of *this* entry within the entries segment.
    /// Combined with `entries_base` by [`back_fill_len_addrs`] to
    /// recover the absolute `set-flags.len` slot — `side_table_idx`
    /// alone isn't enough since it's range-relative, not
    /// segment-relative.
    pub entry_seg_off: u32,
    /// Absolute address of the entry's `set-flags.len` slot. `None`
    /// until [`back_fill_len_addrs`] runs; the emit phase `expect`s
    /// `Some` so a missed back-fill surfaces as a build-time panic.
    pub set_flags_len_addr: Option<i32>,
    /// Absolute address of the per-cell `(name_ptr, name_len)` scratch
    /// buffer. Same pointer is baked into the entry's `set-flags.ptr`.
    pub scratch_addr: i32,
    /// Each flag's interned `(off, len)`, in bit-position order.
    pub flag_names: Vec<BlobSlice>,
}

/// Re-export of the framework output under the kind's name; the layout
/// phase destructures by these field names.
pub(crate) struct FlagsInfoBlobs {
    /// `flags-info` entries: `type-name` + `set-flags.ptr` baked at
    /// build time; `set-flags.len` patched per call by the bit-walk.
    pub entries: Segment,
    /// Per-(fn, param) entry range; `None` for params with no flags.
    pub per_param_range: Vec<Vec<Option<SymRef>>>,
    /// Per-fn Compound result range; `None` otherwise.
    pub per_result_range: Vec<Option<SymRef>>,
    /// Per-cell runtime-fill (patched addresses) parallel to plan.cells.
    pub per_cell_fill: PerCellIndices<FlagsRuntimeFill>,
    /// Per-fn Direct (sync flat) flags result fill. Retptr-loaded
    /// results go via `per_cell_fill` instead.
    pub per_result_single_fill: Vec<Option<FlagsRuntimeFill>>,
}

/// Intern type-name + flag-names for every `Cell::Flags` across all
/// param plans, compound result plans, and single-cell flags results.
pub(crate) fn register_flags_strings(
    per_func: &[FuncClassified],
    names: &mut NameInterner,
) -> StringTable {
    register_side_table_strings(
        per_func,
        names,
        |plan, visit| plan.flags_infos().for_each(visit),
        |st| st.flags_info.as_ref(),
    )
}

/// Lay out the flags-info entries (one per `Cell::Flags`, whether
/// inside a plan or a single-cell result). Caller supplies
/// `scratch_addrs`, one pre-reserved address per entry in the same
/// order this fn consumes them — pre-reservation lets each entry's
/// `set-flags.ptr` land as an absolute address without a reloc.
/// Order must match [`flags_scratch_sizes`].
pub(crate) fn build_flags_info_blob(
    per_func: &[FuncClassified],
    strings: &StringTable,
    entry_layout: &RecordLayout,
    entries_id: SymbolId,
    scratch_addrs: &mut impl Iterator<Item = u32>,
) -> FlagsInfoBlobs {
    let mut writer = FlagsEntryWriter {
        entry_layout,
        strings,
        scratch_addrs,
    };
    let PerCellSideTableBlob {
        entries,
        per_param_range,
        per_result_range,
        per_cell_fill,
        per_result_single_fill,
    } = build_per_cell_side_table(per_func, entry_layout, entries_id, &mut writer);
    FlagsInfoBlobs {
        entries,
        per_param_range,
        per_result_range,
        per_cell_fill,
        per_result_single_fill,
    }
}

struct FlagsEntryWriter<'a, I> {
    entry_layout: &'a RecordLayout,
    strings: &'a StringTable,
    scratch_addrs: &'a mut I,
}

impl<'a, I: Iterator<Item = u32>> FlagsEntryWriter<'a, I> {
    /// Write one zeroed entry: `type-name` + `set-flags.ptr` baked,
    /// `set-flags.len` patched per call. Returns the matching
    /// [`FlagsRuntimeFill`].
    fn append_one(
        &mut self,
        entries: &mut Vec<u8>,
        info: &NamedListInfo,
        idx: u32,
    ) -> FlagsRuntimeFill {
        let s = self
            .strings
            .get(&info.type_name)
            .expect("register_flags_strings ran for every info");
        let scratch_addr = self
            .scratch_addrs
            .next()
            .expect("layout phase must reserve one scratch slot per Cell::Flags cell");
        let entry_seg_off = entries.len() as u32;
        let entry = RecordWriter::extend_zero(entries, self.entry_layout);
        entry.write_slice(entries, INFO_TYPE_NAME, s.type_name);
        entry.write_slice(
            entries,
            FLAGS_INFO_SET_FLAGS,
            BlobSlice {
                off: scratch_addr,
                len: 0,
            },
        );
        debug_assert_eq!(info.item_names.len(), s.items.len());
        FlagsRuntimeFill {
            side_table_idx: idx,
            entry_seg_off,
            set_flags_len_addr: None,
            scratch_addr: scratch_addr as i32,
            flag_names: s.items.clone(),
        }
    }
}

impl<'a, I: Iterator<Item = u32>> CellEntryWriter for FlagsEntryWriter<'a, I> {
    type Fill = FlagsRuntimeFill;

    fn step_cell(
        &mut self,
        entries: &mut Vec<u8>,
        cell: &Cell,
        side_table_idx: u32,
    ) -> Option<FlagsRuntimeFill> {
        let Cell::Flags { info, .. } = cell else {
            return None;
        };
        Some(self.append_one(entries, info, side_table_idx))
    }

    fn direct_for(
        &mut self,
        entries: &mut Vec<u8>,
        fd: &FuncClassified,
    ) -> Option<FlagsRuntimeFill> {
        let info = fd.result_lift.as_ref()?.side_table.flags_info.as_ref()?;
        Some(self.append_one(entries, info, 0))
    }
}

/// Patch each per-cell `set_flags_len_addr` once the entries segment
/// has a base. Address is `entries_base + entry_seg_off +
/// offset_of(set-flags) + SLICE_LEN_OFFSET` — `side_table_idx` is
/// range-relative and would only happen to align with segment-relative
/// for the very first range.
pub(crate) fn back_fill_len_addrs(
    fill: &mut PerCellIndices<FlagsRuntimeFill>,
    single_fill: &mut [Option<FlagsRuntimeFill>],
    entries_base: u32,
    entry_layout: &RecordLayout,
) {
    let set_flags_field_off = entry_layout.offset_of(FLAGS_INFO_SET_FLAGS);
    back_fill_per_cell(fill, single_fill, |f| {
        // Always-on (not debug_assert) — the framework collects every
        // back-fill caller, so a double-call would otherwise silently
        // re-overwrite addrs in release.
        assert!(
            f.set_flags_len_addr.is_none(),
            "back_fill_len_addrs called twice on the same FlagsRuntimeFill",
        );
        f.set_flags_len_addr =
            Some((entries_base + f.entry_seg_off + set_flags_field_off + SLICE_LEN_OFFSET) as i32);
    });
}

/// Per-`Cell::Flags` scratch byte count, in the same plan-walk order
/// [`build_flags_info_blob`] consumes addresses. Walking these in
/// sync is load-bearing: a divergence crashes the builder's
/// `scratch_addrs.next()` expect.
pub(crate) fn flags_scratch_sizes(per_func: &[FuncClassified]) -> Vec<u32> {
    let mut sizes = Vec::new();
    for fd in per_func {
        for p in &fd.params {
            collect_flags_sizes(&p.plan, &mut sizes);
        }
        if let Some(rl) = &fd.result_lift {
            if let Some(c) = rl.compound() {
                collect_flags_sizes(&c.plan, &mut sizes);
            } else if let Some(info) = &rl.side_table.flags_info {
                sizes.push(info.item_names.len() as u32 * STRING_FLAT_BYTES);
            }
        }
    }
    sizes
}

fn collect_flags_sizes(plan: &LiftPlan, sizes: &mut Vec<u32>) {
    for cell in &plan.cells {
        if let Cell::Flags { info, .. } = cell {
            sizes.push(info.item_names.len() as u32 * STRING_FLAT_BYTES);
        }
    }
}
