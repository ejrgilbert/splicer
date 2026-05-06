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
    /// Per-cell utf-8 scratch buffer base for `Cell::Char`. The
    /// wrapper utf-8-encodes the i32 code point into this buffer
    /// (1–4 bytes) and emits `cell::text(scratch_addr, len)`.
    Char {
        scratch_addr: i32,
    },
    /// `cell::{resource,stream,future}-handle(u32)` payload + the
    /// wrapper-patched `id` slot address. The cell's `kind` picks
    /// the disc; the side-table layout is identical across all
    /// three. Boxed for the same reason as Flags/Variant.
    Handle(Box<HandleRuntimeFill>),
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
                scratch_addr: sources.char_scratch[i].expect("Char cell missing scratch addr"),
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
            | Cell::Result { .. } => CellSideData::None,
            // Un-wired — plan-builder `todo!()`s before constructing
            // these. Reaching this arm means an un-wired variant
            // slipped through the plan-builder's gate.
            Cell::ListOf => {
                unreachable!("fold_cell_side_data reached un-wired Cell variant {cell:?}")
            }
        })
        .collect()
}

// ─── Per-cell-fill plan walk (record / variant / handle) ─────────
//
// Shared outer loop: walk every (fn, param) plan + the compound-
// result plan (if any), collecting `(range, per-cell-fill)` from
// each `append_plan` call. Flags has its own loop because of the
// single-cell-result branch; tuple-indices has no range concept.

pub(super) struct PerCellPlanWalk<T> {
    pub per_param_range: Vec<Vec<Option<SymRef>>>,
    pub per_param_fill: Vec<Vec<Vec<Option<T>>>>,
    pub per_result_range: Vec<Option<SymRef>>,
    pub per_result_fill: Vec<Vec<Option<T>>>,
}

pub(super) fn walk_per_cell_plans<T>(
    per_func: &[FuncClassified],
    mut append_plan: impl FnMut(&LiftPlan) -> (Option<SymRef>, Vec<Option<T>>),
) -> PerCellPlanWalk<T> {
    let mut per_param_range: Vec<Vec<Option<SymRef>>> = Vec::with_capacity(per_func.len());
    let mut per_param_fill: Vec<Vec<Vec<Option<T>>>> = Vec::with_capacity(per_func.len());
    let mut per_result_range: Vec<Option<SymRef>> = Vec::with_capacity(per_func.len());
    let mut per_result_fill: Vec<Vec<Option<T>>> = Vec::with_capacity(per_func.len());

    for fd in per_func {
        let mut params_ranges = Vec::with_capacity(fd.params.len());
        let mut params_fill = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            let (range, fill_map) = append_plan(&p.plan);
            params_ranges.push(range);
            params_fill.push(fill_map);
        }
        per_param_range.push(params_ranges);
        per_param_fill.push(params_fill);

        let (result_range, result_fill_map) =
            match fd.result_lift.as_ref().and_then(|rl| rl.compound()) {
                Some(c) => append_plan(&c.plan),
                None => (None, Vec::new()),
            };
        per_result_range.push(result_range);
        per_result_fill.push(result_fill_map);
    }

    PerCellPlanWalk {
        per_param_range,
        per_param_fill,
        per_result_range,
        per_result_fill,
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

/// Walk every param / result; for each lift that surfaces a
/// [`NamedListInfo`] of this kind, intern its strings into `names`
/// (the interner already dedupes, so type-name + item-names that
/// recur across functions share one copy in the blob). Returns the
/// per-type string offsets so the side-table builder can stitch
/// entries together without re-scanning the blob.
///
/// `from_plan` extracts the kind's infos from a per-param
/// [`LiftPlan`] (multiple infos possible if the plan has multiple
/// nominal cells of this kind). `from_result` reads the kind's
/// info off the result's [`SideTableInfo`] (single info, since
/// results today are single-cell).
pub(super) fn register_side_table_strings(
    per_func: &[FuncClassified],
    names: &mut NameInterner,
    from_plan: impl Fn(&LiftPlan) -> Vec<&NamedListInfo>,
    from_result: impl Fn(&SideTableInfo) -> Option<&NamedListInfo>,
) -> StringTable {
    let mut table = StringTable::new();
    for fd in per_func {
        for p in &fd.params {
            for info in from_plan(&p.plan) {
                ensure_registered(&mut table, names, info);
            }
        }
        if let Some(rl) = &fd.result_lift {
            if let Some(info) = from_result(&rl.side_table) {
                ensure_registered(&mut table, names, info);
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
        let result_info = fd
            .result_lift
            .as_ref()
            .and_then(|r| from_result(&r.side_table));
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
