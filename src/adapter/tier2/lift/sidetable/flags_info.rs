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
use super::{ensure_registered, PerCellIndices, StringTable, INFO_TYPE_NAME};

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

/// Output of [`build_flags_info_blob`]. Mirrors
/// [`super::record_info::RecordInfoBlobs`] plus per-cell
/// [`FlagsRuntimeFill`] data the emit phase consumes.
pub(crate) struct FlagsInfoBlobs {
    /// `flags-info` entries (one per `Cell::Flags`). `type-name` and
    /// `set-flags.ptr` are baked at build time; `set-flags.len` is
    /// patched at runtime by the wrapper bit-walk.
    pub entries: Segment,
    pub per_param_range: Vec<Vec<Option<SymRef>>>,
    pub per_result_range: Vec<Option<SymRef>>,
    pub per_cell_fill: PerCellIndices<FlagsRuntimeFill>,
    /// Per-fn fill for a single-cell flags result (Direct or RetptrPair
    /// — both lift one `Cell::Flags`, with no plan to attach it to).
    /// `Some` when the func's result classifies as a single
    /// `Cell::Flags`.
    pub per_result_single_fill: Vec<Option<FlagsRuntimeFill>>,
}

/// Intern type-name + flag-names for every `Cell::Flags` across all
/// param plans, compound result plans, and single-cell flags results.
pub(crate) fn register_flags_strings(
    per_func: &[FuncClassified],
    names: &mut NameInterner,
) -> StringTable {
    let mut table = StringTable::new();
    for fd in per_func {
        for p in &fd.params {
            for info in p.plan.flags_infos() {
                ensure_registered(&mut table, names, info);
            }
        }
        if let Some(rl) = &fd.result_lift {
            // Compound + single-cell are mutually exclusive (compound
            // path leaves SideTableInfo empty); `else if` makes that
            // explicit.
            if let Some(c) = rl.compound() {
                for info in c.plan.flags_infos() {
                    ensure_registered(&mut table, names, info);
                }
            } else if let Some(info) = &rl.side_table.flags_info {
                ensure_registered(&mut table, names, info);
            }
        }
    }
    table
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
    let mut builder = FlagsInfoBuilder::new(entry_layout, entries_id);
    let mut per_param_range: Vec<Vec<Option<SymRef>>> = Vec::with_capacity(per_func.len());
    let mut per_param_fill: Vec<Vec<Vec<Option<FlagsRuntimeFill>>>> =
        Vec::with_capacity(per_func.len());
    let mut per_result_range: Vec<Option<SymRef>> = Vec::with_capacity(per_func.len());
    let mut per_result_fill: Vec<Vec<Option<FlagsRuntimeFill>>> =
        Vec::with_capacity(per_func.len());
    let mut per_result_single_fill: Vec<Option<FlagsRuntimeFill>> =
        Vec::with_capacity(per_func.len());

    for fd in per_func {
        let mut params_ranges = Vec::with_capacity(fd.params.len());
        let mut params_fill = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            let (range, fill_map) = builder.append_plan(&p.plan, strings, scratch_addrs);
            params_ranges.push(range);
            params_fill.push(fill_map);
        }
        per_param_range.push(params_ranges);
        per_param_fill.push(params_fill);

        // Result side: Compound (record/tuple/option/result with
        // possibly-nested flags) walks the plan; single-cell flags
        // (Direct or RetptrPair) appends one entry inline.
        let (result_range, result_fill_map, single_fill) = match fd.result_lift.as_ref() {
            Some(rl) => match (rl.compound(), rl.side_table.flags_info.as_ref()) {
                (Some(c), None) => {
                    let (range, fill) = builder.append_plan(&c.plan, strings, scratch_addrs);
                    (range, fill, None)
                }
                (None, Some(info)) => {
                    let (range, fill) = builder.append_direct(info, strings, scratch_addrs);
                    (range, Vec::new(), Some(fill))
                }
                (None, None) => (None, Vec::new(), None),
                // classify_result_lift leaves SideTableInfo empty
                // when returning Compound; if both fire, the invariant
                // is broken and entries would double-allocate.
                (Some(_), Some(_)) => unreachable!(
                    "Compound result has populated SideTableInfo — classify invariant broken",
                ),
            },
            None => (None, Vec::new(), None),
        };
        per_result_range.push(result_range);
        per_result_fill.push(result_fill_map);
        per_result_single_fill.push(single_fill);
    }

    FlagsInfoBlobs {
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

struct FlagsInfoBuilder<'a> {
    entries: Vec<u8>,
    entry_layout: &'a RecordLayout,
    entries_id: SymbolId,
}

impl<'a> FlagsInfoBuilder<'a> {
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
        scratch_addrs: &mut impl Iterator<Item = u32>,
    ) -> (Option<SymRef>, Vec<Option<FlagsRuntimeFill>>) {
        let range_start = self.entries.len() as u32;
        let mut count: u32 = 0;
        let mut fill_map: Vec<Option<FlagsRuntimeFill>> = vec![None; plan.cells.len()];
        for (cell_pos, op) in plan.cells.iter().enumerate() {
            let Cell::Flags { info, .. } = op else {
                continue;
            };
            let fill = self.append_one(info, strings, scratch_addrs, count);
            count += 1;
            fill_map[cell_pos] = Some(fill);
        }
        let range = (count > 0).then_some(SymRef {
            target: self.entries_id,
            off: range_start,
            len: count,
        });
        (range, fill_map)
    }

    /// Append one entry for a single-cell flags result (Direct or
    /// RetptrPair). Mirrors [`Self::append_plan`] for the no-plan case.
    fn append_direct(
        &mut self,
        info: &NamedListInfo,
        strings: &StringTable,
        scratch_addrs: &mut impl Iterator<Item = u32>,
    ) -> (Option<SymRef>, FlagsRuntimeFill) {
        let range_start = self.entries.len() as u32;
        let fill = self.append_one(info, strings, scratch_addrs, 0);
        let range = Some(SymRef {
            target: self.entries_id,
            off: range_start,
            len: 1,
        });
        (range, fill)
    }

    /// Shared body: write one zeroed entry, fill type-name +
    /// set-flags.ptr, return the matching [`FlagsRuntimeFill`].
    fn append_one(
        &mut self,
        info: &NamedListInfo,
        strings: &StringTable,
        scratch_addrs: &mut impl Iterator<Item = u32>,
        side_table_idx: u32,
    ) -> FlagsRuntimeFill {
        let s = strings
            .get(&info.type_name)
            .expect("register_flags_strings ran for every info");
        let scratch_addr = scratch_addrs
            .next()
            .expect("layout phase must reserve one scratch slot per Cell::Flags cell");

        let entry_seg_off = self.entries.len() as u32;
        // `set-flags.len` stays zero — the wrapper patches it per call.
        let entry = RecordWriter::extend_zero(&mut self.entries, self.entry_layout);
        entry.write_slice(&mut self.entries, INFO_TYPE_NAME, s.type_name);
        entry.write_slice(
            &mut self.entries,
            FLAGS_INFO_SET_FLAGS,
            BlobSlice {
                off: scratch_addr,
                len: 0,
            },
        );

        debug_assert_eq!(info.item_names.len(), s.items.len());
        FlagsRuntimeFill {
            side_table_idx,
            entry_seg_off,
            set_flags_len_addr: None,
            scratch_addr: scratch_addr as i32,
            flag_names: s.items.clone(),
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
    let patch_one = |f: &mut FlagsRuntimeFill| {
        debug_assert!(
            f.set_flags_len_addr.is_none(),
            "back_fill_len_addrs called twice on the same FlagsRuntimeFill",
        );
        f.set_flags_len_addr =
            Some((entries_base + f.entry_seg_off + set_flags_field_off + SLICE_LEN_OFFSET) as i32);
    };
    let patch_row = |row: &mut [Option<FlagsRuntimeFill>]| {
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
