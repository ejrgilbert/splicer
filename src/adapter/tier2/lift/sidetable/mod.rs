//! Side-table population: per-tree info records that the cell
//! codegen references by adapter-build-time-known indices.
//!
//! All side-table kinds (enum / flags / variant / record) share the
//! same shape and lifecycle:
//!   1. Walk every (fn, param | result); for each lift carrying an
//!      info of this kind, dedup-register the strings (type-name +
//!      item-names) into the shared name_blob.
//!   2. Lay out one entry record per item in declaration order, into
//!      one contiguous side-table data segment.
//!   3. Hand back per-(fn, param) and per-(fn, result) [`SymRef`]
//!      pointers tagged with the segment's [`SymbolId`]; the layout
//!      phase resolves them to absolute [`BlobSlice`]s after every
//!      segment has a base.
//!
//! The kind-specific bits (where to find the info on `SideTableInfo`,
//! which `RecordLayout` to use, what the item-name field is called)
//! are passed in via [`SideTableSpec`] + an extractor closure for the
//! enum-style kinds in [`enum_info`]; record-info has its own builder
//! shape (entries + tuples arena) in [`record_info`].

use std::collections::HashMap;

use super::super::super::abi::emit::{BlobSlice, RecordLayout};
use super::super::blob::{
    resolve, NameInterner, RecordWriter, Segment, SymRef, SymbolBases, SymbolId,
};
use super::super::FuncClassified;
use super::classify::SideTableInfo;
use super::plan::{Cell, LiftPlan, NamedListInfo};

pub(super) mod char_info;
pub(super) mod enum_info;
pub(super) mod flags_info;
pub(super) mod handle_info;
pub(super) mod record_info;
pub(super) mod tuple_indices;
pub(super) mod variant_info;

use flags_info::FlagsRuntimeFill;
use handle_info::HandleRuntimeFill;
use variant_info::VariantRuntimeFill;

/// Per-plan-cell side-table data the emit phase reads. One entry per
/// `plan.cells` position; `None` for cells that lift purely from flat
/// slots. Heavy payloads (Flags, eventually Variant) are Boxed so the
/// enum stays ~16 bytes — adding a kind = one variant + one
/// [`super::emit::emit_cell_op`] arm.
#[derive(Clone, Debug)]
pub(crate) enum CellSideData {
    None,
    /// `cell::record-of(u32)` payload — build-time-known side-table idx.
    Record {
        idx: u32,
    },
    /// `cell::tuple-of(list<u32>)` payload — `(off, len)` of the
    /// child-index array in the tuple-indices segment.
    Tuple {
        slice: BlobSlice,
    },
    /// `cell::flags-set(u32)` payload + the addresses the wrapper
    /// bit-walk patches at runtime.
    Flags(Box<FlagsRuntimeFill>),
    /// `cell::variant-case(u32)` payload + the addresses the wrapper
    /// disc-dispatch patches at runtime (case-name + payload option).
    Variant(Box<VariantRuntimeFill>),
    /// Scratch-buffer source for `Cell::Char`'s utf-8 encoder. The
    /// wrapper writes 1–4 bytes into the buffer and emits
    /// `cell::text(scratch, len)`. See [`CharScratch`] for which kind
    /// of buffer this points at.
    Char {
        scratch: CharScratch,
    },
    /// `cell::{resource,stream,future}-handle(u32)` payload + the
    /// wrapper-patched `id` slot address. The cell's `kind` picks
    /// the disc; the side-table layout is identical across all
    /// three. Boxed for the same reason as Flags/Variant.
    Handle(Box<HandleRuntimeFill>),
}

/// Where a `Cell::Char`'s utf-8 scratch buffer lives. Two cases
/// because the buffer base reaches the encoder differently:
///
/// - `Static`: a 4-byte slab the layout phase reserved for this
///   plan-cell. The emit phase stages the const into the wrapper's
///   shared scratch-addr local before each char-cell write.
/// - `Prestaged`: the cell sits inside a `list<char>` element body;
///   the per-iteration emit code has already computed
///   `list_scratch_base + j*4` into the same shared local before
///   calling [`super::emit::emit_cell_op`]. Static slabs aren't
///   reserved for these cells — list length is runtime-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CharScratch {
    Static { scratch_addr: i32 },
    Prestaged,
}

/// Per-cell fill maps for one (fn, param | result), each parallel to
/// `plan.cells` and sourced from the matching side-table builder.
/// Bundled to keep [`fold_cell_side_data`]'s signature stable as new
/// kinds land — adding one here + the matching `fold_cell_side_data`
/// arm is the full change.
pub(crate) struct CellFillSources<'a> {
    pub record_info: &'a [Option<u32>],
    pub tuple_indices: &'a [Option<BlobSlice>],
    pub flags_fill: &'a [Option<FlagsRuntimeFill>],
    pub variant_fill: &'a [Option<VariantRuntimeFill>],
    pub char_scratch: &'a [Option<i32>],
    pub handle_fill: &'a [Option<HandleRuntimeFill>],
}

/// Fold the per-builder per-cell maps into one [`Vec<CellSideData>`]
/// parallel to `plan.cells`. Single match-on-`Cell` is the only place
/// that decides "this cell wants that kind's bookkeeping."
///
/// **Outer plan only.** Element-plan side data is produced by
/// [`super::emit::elem_cell_side_data`] (Prestaged scratch); recursing
/// here would double-fold list-element chars with stale `Static`
/// addresses. `char_scratch_sizes` / `build_char_scratch_map` follow
/// the same rule.
pub(crate) fn fold_cell_side_data(
    plan: &LiftPlan,
    sources: &CellFillSources<'_>,
) -> Vec<CellSideData> {
    let n = plan.cells.len();
    debug_assert_eq!(sources.record_info.len(), n);
    debug_assert_eq!(sources.tuple_indices.len(), n);
    debug_assert_eq!(sources.flags_fill.len(), n);
    debug_assert_eq!(sources.variant_fill.len(), n);
    debug_assert_eq!(sources.char_scratch.len(), n);
    debug_assert_eq!(sources.handle_fill.len(), n);
    plan.cells
        .iter()
        .enumerate()
        .map(|(i, cell)| match cell {
            // Side-data-bearing kinds.
            Cell::RecordOf { .. } => CellSideData::Record {
                idx: sources.record_info[i].expect("RecordOf cell missing record-info idx"),
            },
            Cell::TupleOf { .. } => CellSideData::Tuple {
                slice: sources.tuple_indices[i].expect("TupleOf cell missing tuple-indices slice"),
            },
            Cell::Flags { .. } => CellSideData::Flags(Box::new(
                sources.flags_fill[i]
                    .clone()
                    .expect("Flags cell missing runtime-fill bundle"),
            )),
            Cell::Variant { .. } => CellSideData::Variant(Box::new(
                sources.variant_fill[i]
                    .clone()
                    .expect("Variant cell missing runtime-fill bundle"),
            )),
            Cell::Char { .. } => CellSideData::Char {
                scratch: CharScratch::Static {
                    scratch_addr: sources.char_scratch[i].expect("Char cell missing scratch addr"),
                },
            },
            Cell::Handle { .. } => CellSideData::Handle(Box::new(
                sources.handle_fill[i]
                    .clone()
                    .expect("Handle cell missing runtime-fill bundle"),
            )),
            // Wired primitives + control-flow cells that read purely
            // from flat slots — no side-table contribution. Listed
            // explicitly (no `_` catchall) so adding a new wired
            // variant forces a fold-arm decision at compile time,
            // mirroring [`super::emit::emit_cell_op`].
            Cell::Bool { .. }
            | Cell::IntegerSignExt { .. }
            | Cell::IntegerZeroExt { .. }
            | Cell::Integer64 { .. }
            | Cell::FloatingF32 { .. }
            | Cell::FloatingF64 { .. }
            | Cell::Text { .. }
            | Cell::Bytes { .. }
            | Cell::EnumCase { .. }
            | Cell::Option { .. }
            | Cell::Result { .. }
            | Cell::ListOf { .. } => CellSideData::None,
        })
        .collect()
}

// ─── Generic per-cell side-table builder ─────────────────────────
//
// Flags / handle / variant share one shape: per-(plan-cell) entries in
// a contiguous segment, an optional Direct (sync-flat) result entry
// per fn, plus a per-cell `Fill` carrying the runtime-patched slot
// addresses. Each kind plugs in via [`CellEntryWriter`]; the framework
// owns the loop, the bytes vec, and the SymRef bookkeeping.

/// Per-kind entry writer. Plug in `step_cell` (match this kind's
/// `Cell` variant, write one entry, return the fill) and, for kinds
/// with a sync-flat Direct path, set `HAS_DIRECT = true` and provide
/// `direct_for`.
pub(super) trait CellEntryWriter {
    type Fill;

    /// Whether this kind ever produces a Direct (sync flat) result.
    /// `false` (variant) skips the per-fn `single_fill` allocation
    /// and `direct_for` calls entirely.
    const HAS_DIRECT: bool = true;

    fn step_cell(
        &mut self,
        entries: &mut Vec<u8>,
        cell: &Cell,
        side_table_idx: u32,
    ) -> Option<Self::Fill>;

    fn direct_for(&mut self, _entries: &mut Vec<u8>, _fd: &FuncClassified) -> Option<Self::Fill> {
        None
    }
}

/// Output of [`build_per_cell_side_table`]. Each kind re-exports it
/// under its own alias if any callers care about the name.
pub(crate) struct PerCellSideTableBlob<F> {
    pub entries: Segment,
    pub per_param_range: Vec<Vec<Option<SymRef>>>,
    pub per_result_range: Vec<Option<SymRef>>,
    pub per_cell_fill: PerCellIndices<F>,
    /// Per-fn Direct (sync flat) fill. Length `per_func.len()` for
    /// kinds with `HAS_DIRECT`; empty `Vec` otherwise.
    pub per_result_single_fill: Vec<Option<F>>,
}

/// Build one per-cell side-table segment. Per fn: walk param plans
/// then the compound-result plan (if any) OR call `direct_for` (sync-
/// flat result of this kind). The two result-side branches are
/// mutually exclusive — `classify_result_lift` routes Compound and
/// Direct apart.
pub(super) fn build_per_cell_side_table<W: CellEntryWriter>(
    per_func: &[FuncClassified],
    entry_layout: &RecordLayout,
    entries_id: SymbolId,
    writer: &mut W,
) -> PerCellSideTableBlob<W::Fill> {
    let mut entries: Vec<u8> = Vec::new();
    let mut per_param_range: Vec<Vec<Option<SymRef>>> = Vec::with_capacity(per_func.len());
    let mut per_param_fill: Vec<Vec<Vec<Option<W::Fill>>>> = Vec::with_capacity(per_func.len());
    let mut per_result_range: Vec<Option<SymRef>> = Vec::with_capacity(per_func.len());
    let mut per_result_fill: Vec<Vec<Option<W::Fill>>> = Vec::with_capacity(per_func.len());
    // Skip the per-fn single-fill allocation entirely for kinds
    // without a Direct path.
    let mut per_result_single_fill: Vec<Option<W::Fill>> = if W::HAS_DIRECT {
        Vec::with_capacity(per_func.len())
    } else {
        Vec::new()
    };

    for fd in per_func {
        let mut params_ranges = Vec::with_capacity(fd.params.len());
        let mut params_fill = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            let (range, fill_map) = scan_plan(&mut entries, entries_id, &p.plan, writer);
            params_ranges.push(range);
            params_fill.push(fill_map);
        }
        per_param_range.push(params_ranges);
        per_param_fill.push(params_fill);

        let compound_plan = fd.result_lift.as_ref().and_then(|rl| rl.compound());
        let (result_range, result_fill, single) = match compound_plan {
            Some(c) => {
                let (range, fill) = scan_plan(&mut entries, entries_id, &c.plan, writer);
                (range, fill, None)
            }
            None if W::HAS_DIRECT => {
                let range_start = entries.len() as u32;
                match writer.direct_for(&mut entries, fd) {
                    Some(fill) => (
                        Some(SymRef {
                            target: entries_id,
                            off: range_start,
                            len: 1,
                        }),
                        Vec::new(),
                        Some(fill),
                    ),
                    None => (None, Vec::new(), None),
                }
            }
            None => (None, Vec::new(), None),
        };
        per_result_range.push(result_range);
        per_result_fill.push(result_fill);
        if W::HAS_DIRECT {
            per_result_single_fill.push(single);
        } else {
            debug_assert!(single.is_none());
        }
    }

    PerCellSideTableBlob {
        entries: Segment {
            id: entries_id,
            align: entry_layout.align,
            bytes: entries,
            relocs: Vec::new(),
        },
        per_param_range,
        per_result_range,
        per_cell_fill: PerCellIndices {
            per_param: per_param_fill,
            per_result: per_result_fill,
        },
        per_result_single_fill,
    }
}

/// Walk one plan's cells, asking the writer to handle each, accumulating
/// `Some` fills into a contiguous range. Returns `(range, fill_map)`;
/// `range` is `None` when no cell of this kind appeared.
fn scan_plan<W: CellEntryWriter>(
    entries: &mut Vec<u8>,
    entries_id: SymbolId,
    plan: &LiftPlan,
    writer: &mut W,
) -> (Option<SymRef>, Vec<Option<W::Fill>>) {
    let range_start = entries.len() as u32;
    let mut count: u32 = 0;
    let mut fill_map: Vec<Option<W::Fill>> = (0..plan.cells.len()).map(|_| None).collect();
    for (cell_pos, cell) in plan.cells.iter().enumerate() {
        if let Some(fill) = writer.step_cell(entries, cell, count) {
            count += 1;
            fill_map[cell_pos] = Some(fill);
        }
    }
    let range = (count > 0).then_some(SymRef {
        target: entries_id,
        off: range_start,
        len: count,
    });
    (range, fill_map)
}

/// Apply `patch` to every `Some` fill across the per-cell grid + the
/// per-fn `single_fill` overlay. Pass `&mut []` for `single_fill` for
/// kinds without a Direct path (variant).
pub(super) fn back_fill_per_cell<F>(
    fill: &mut PerCellIndices<F>,
    single_fill: &mut [Option<F>],
    mut patch: impl FnMut(&mut F),
) {
    for fn_row in fill.per_param.iter_mut() {
        for param_row in fn_row.iter_mut() {
            for slot in param_row.iter_mut() {
                if let Some(f) = slot.as_mut() {
                    patch(f);
                }
            }
        }
    }
    for fn_row in fill.per_result.iter_mut() {
        for slot in fn_row.iter_mut() {
            if let Some(f) = slot.as_mut() {
                patch(f);
            }
        }
    }
    for slot in single_fill.iter_mut() {
        if let Some(f) = slot.as_mut() {
            patch(f);
        }
    }
}

// ─── Per-cell side-table indices ─────────────────────────────────
//
// Each builder produces its own `PerCellIndices<T>` (record-info: u32,
// tuple-indices: SymRef, flags: FlagsRuntimeFill, variant:
// VariantRuntimeFill). The layout phase folds these into one
// `Vec<CellSideData>` per (fn, param | result) via
// [`fold_cell_side_data`].

/// Per-(fn, param) and per-(fn, result) per-plan-cell `Option<T>`
/// map. Internal nesting is `Vec<Vec<Vec<…>>>` / `Vec<Vec<…>>` but
/// hidden behind [`Self::for_param`] / [`Self::for_result`].
pub(crate) struct PerCellIndices<T> {
    pub(super) per_param: Vec<Vec<Vec<Option<T>>>>,
    pub(super) per_result: Vec<Vec<Option<T>>>,
}

impl<T> PerCellIndices<T> {
    pub(crate) fn for_param(&self, fn_idx: usize, param_idx: usize) -> &[Option<T>] {
        &self.per_param[fn_idx][param_idx]
    }

    /// Per-cell map for one fn's compound result. Empty slice for
    /// non-compound (or void) results.
    pub(crate) fn for_result(&self, fn_idx: usize) -> &[Option<T>] {
        &self.per_result[fn_idx]
    }
}

impl PerCellIndices<SymRef> {
    /// Resolve one (fn, param)'s symbolic cell slots to absolute
    /// [`BlobSlice`]s. Length matches that param's `plan.cells.len()`.
    pub(crate) fn resolve_param(
        &self,
        fn_idx: usize,
        param_idx: usize,
        symbols: &SymbolBases,
    ) -> Vec<Option<BlobSlice>> {
        resolve_cell_syms(self.for_param(fn_idx, param_idx), symbols)
    }

    pub(crate) fn resolve_result(
        &self,
        fn_idx: usize,
        symbols: &SymbolBases,
    ) -> Vec<Option<BlobSlice>> {
        resolve_cell_syms(self.for_result(fn_idx), symbols)
    }
}

fn resolve_cell_syms(syms: &[Option<SymRef>], symbols: &SymbolBases) -> Vec<Option<BlobSlice>> {
    syms.iter()
        .map(|s| s.map(|s| resolve(Some(s), symbols)))
        .collect()
}

// ─── WIT names referenced by lift codegen ─────────────────────────
//
// Side-table-info records in `splicer:common/types` share the same
// shape: `record { type-name: string, <item>-name: string }`. Field
// names for each kind are passed to [`SideTableSpec`].
pub(super) const INFO_TYPE_NAME: &str = "type-name";

/// Per-side-table-kind configuration. Plug-in points for adding a
/// new kind: provide the `RecordLayout` for one entry record + the
/// item-name field name, and pass an extractor closure that pulls
/// this kind's info off `SideTableInfo`.
pub(super) struct SideTableSpec<'a> {
    /// Layout of one entry record (e.g. `splicer:common/types.enum-info`).
    pub entry_layout: &'a RecordLayout,
    /// Field name on the entry record for the per-item identifier
    /// (e.g. `"case-name"` for enum-info, `"flag-name"` for flags-info).
    pub item_name_field: &'static str,
}

/// Where each registered type's strings live in the name blob.
/// Keyed by type-name to dedupe across multiple uses of the same
/// type across params / results / functions.
pub(crate) type StringTable = HashMap<String, NamedListStrings>;

pub(crate) struct NamedListStrings {
    pub(super) type_name: BlobSlice,
    pub(super) items: Vec<BlobSlice>, // per item, in declaration order
}

/// Output of [`build_side_table_blob`]: the entry-record [`Segment`]
/// plus per-(fn, param) and per-(fn, result) [`SymRef`]s into it.
/// `None` marks "no entries for this slot" — params/results that
/// don't carry this side-table kind. Resolution to absolute
/// [`BlobSlice`]s happens once the segment's base is known.
pub(crate) struct SideTableBlob {
    pub segment: Segment,
    pub per_param: Vec<Vec<Option<SymRef>>>,
    pub per_result: Vec<Option<SymRef>>,
}

/// Intern this kind's `NamedListInfo` strings (the interner dedupes
/// across funcs / params / results). Returns the per-type string
/// offsets so the side-table builder can stitch entries without
/// re-scanning the blob.
///
/// `visit_plan_infos` calls its callback per info in the plan —
/// visitor lets callers pipe `plan.flags_infos()` etc. through
/// without an interim `Vec`. `from_result` covers sync-flat Direct
/// results whose cell never made it into a plan.
pub(super) fn register_side_table_strings(
    per_func: &[FuncClassified],
    names: &mut NameInterner,
    visit_plan_infos: impl Fn(&LiftPlan, &mut dyn FnMut(&NamedListInfo)),
    from_result: impl Fn(&SideTableInfo) -> Option<&NamedListInfo>,
) -> StringTable {
    let mut table = StringTable::new();
    for fd in per_func {
        for p in &fd.params {
            visit_plan_infos(&p.plan, &mut |info| {
                ensure_registered(&mut table, names, info)
            });
        }
        if let Some(rl) = &fd.result_lift {
            // Compound results: walk the plan (catches infos nested
            // inside list element plans, etc., symmetric with params).
            // Direct results: side-table info already carries the cell.
            match rl.compound() {
                Some(c) => visit_plan_infos(&c.plan, &mut |info| {
                    ensure_registered(&mut table, names, info)
                }),
                None => {
                    if let Some(info) = from_result(&rl.side_table) {
                        ensure_registered(&mut table, names, info);
                    }
                }
            }
        }
    }
    table
}

pub(super) fn ensure_registered(
    table: &mut StringTable,
    names: &mut NameInterner,
    info: &NamedListInfo,
) {
    if table.contains_key(&info.type_name) {
        return;
    }
    let type_name = names.intern(&info.type_name);
    let items = info.item_names.iter().map(|n| names.intern(n)).collect();
    table.insert(
        info.type_name.clone(),
        NamedListStrings { type_name, items },
    );
}

/// Lay out one per-case-kind side table. For per-case kinds (enum,
/// variant) the side-table index is the runtime disc, so entries
/// are laid out one-per-case in WIT declaration order. The cell at
/// runtime points at the contiguous per-(param|result) range via
/// `(blob_off, len)`.
///
/// `from_plan` returns the (at most one for enum) info for a param's
/// plan that contributes to this side table. (When records of enums
/// land, this may yield multiple infos per plan — the builder
/// handles that by appending one contiguous range per plan-cell.)
pub(super) fn build_side_table_blob(
    per_func: &[FuncClassified],
    strings: &StringTable,
    spec: &SideTableSpec<'_>,
    segment_id: SymbolId,
    from_plan: impl Fn(&LiftPlan) -> Option<&NamedListInfo>,
    from_result: impl Fn(&SideTableInfo) -> Option<&NamedListInfo>,
) -> SideTableBlob {
    let mut bytes: Vec<u8> = Vec::new();
    let mut per_param: Vec<Vec<Option<SymRef>>> = Vec::with_capacity(per_func.len());
    let mut per_result: Vec<Option<SymRef>> = Vec::with_capacity(per_func.len());
    for fd in per_func {
        let mut params = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            params.push(append_entries(
                &mut bytes,
                strings,
                spec,
                segment_id,
                from_plan(&p.plan),
            ));
        }
        per_param.push(params);
        // Compound results: derive from the plan (catches infos nested
        // inside list element plans). Direct results: side-table info
        // carries the single cell. Mirrors register_side_table_strings.
        let result_info = fd.result_lift.as_ref().and_then(|r| match r.compound() {
            Some(c) => from_plan(&c.plan),
            None => from_result(&r.side_table),
        });
        per_result.push(append_entries(
            &mut bytes,
            strings,
            spec,
            segment_id,
            result_info,
        ));
    }
    SideTableBlob {
        segment: Segment {
            id: segment_id,
            align: spec.entry_layout.align,
            bytes,
            relocs: Vec::new(),
        },
        per_param,
        per_result,
    }
}

fn append_entries(
    blob: &mut Vec<u8>,
    strings: &StringTable,
    spec: &SideTableSpec<'_>,
    segment_id: SymbolId,
    info: Option<&NamedListInfo>,
) -> Option<SymRef> {
    let info = info?;
    let s = strings
        .get(&info.type_name)
        .expect("register_side_table_strings ran for every info");
    let blob_off = blob.len() as u32;
    let len = info.item_names.len() as u32;
    for item_idx in 0..info.item_names.len() {
        let entry = RecordWriter::extend_zero(blob, spec.entry_layout);
        entry.write_slice(blob, INFO_TYPE_NAME, s.type_name);
        entry.write_slice(blob, spec.item_name_field, s.items[item_idx]);
    }
    Some(SymRef {
        target: segment_id,
        off: blob_off,
        len,
    })
}
