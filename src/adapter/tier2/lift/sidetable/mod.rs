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
use super::super::blob::{RecordWriter, Segment, SymRef, SymbolId};
use super::super::FuncClassified;
use super::classify::SideTableInfo;
use super::plan::{LiftPlan, NamedListInfo};

pub(super) mod enum_info;
pub(super) mod record_info;

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
/// [`NamedListInfo`] of this kind, append its strings to `name_blob`
/// (deduped per type-name). Returns the per-type string offsets so
/// the side-table builder can stitch entries together without
/// re-scanning `name_blob`.
///
/// `from_plan` extracts the kind's infos from a per-param
/// [`LiftPlan`] (multiple infos possible if the plan has multiple
/// nominal cells of this kind). `from_result` reads the kind's
/// info off the result's [`SideTableInfo`] (single info, since
/// results today are single-cell).
pub(super) fn register_side_table_strings(
    per_func: &[FuncClassified],
    name_blob: &mut Vec<u8>,
    from_plan: impl Fn(&LiftPlan) -> Vec<&NamedListInfo>,
    from_result: impl Fn(&SideTableInfo) -> Option<&NamedListInfo>,
) -> StringTable {
    let mut table = StringTable::new();
    for fd in per_func {
        for p in &fd.params {
            for info in from_plan(&p.plan) {
                ensure_registered(&mut table, name_blob, info);
            }
        }
        if let Some(rl) = &fd.result_lift {
            if let Some(info) = from_result(&rl.side_table) {
                ensure_registered(&mut table, name_blob, info);
            }
        }
    }
    table
}

fn ensure_registered(table: &mut StringTable, name_blob: &mut Vec<u8>, info: &NamedListInfo) {
    if table.contains_key(&info.type_name) {
        return;
    }
    let type_name = append_string(name_blob, &info.type_name);
    let items = info
        .item_names
        .iter()
        .map(|n| append_string(name_blob, n))
        .collect();
    table.insert(
        info.type_name.clone(),
        NamedListStrings { type_name, items },
    );
}

pub(super) fn append_string(name_blob: &mut Vec<u8>, s: &str) -> BlobSlice {
    let off = name_blob.len() as u32;
    name_blob.extend_from_slice(s.as_bytes());
    BlobSlice {
        off,
        len: s.len() as u32,
    }
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
