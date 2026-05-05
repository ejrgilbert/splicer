//! Schema-driven layouts gathered up front so later phases see one
//! bundle ([`SchemaLayouts`]) instead of a dozen locals.
//!
//! Two responsibilities:
//! - Resolve the `splicer:common/types` typedefs the adapter
//!   references (`field`, `field-tree`, `cell`, `call-id`, the
//!   `*-info` records) and compute their canonical-ABI layouts.
//! - Locate the middleware's `splicer:tier2/before` and
//!   `splicer:tier2/after` hook imports and capture their wasm
//!   signatures so the generated wrapper can call them.

use anyhow::{anyhow, Result};
use wit_parser::{Resolve, SizeAlign, Type, WorldId};

use super::super::abi::emit::{
    call_id_layout, find_common_typeid, find_imported_hook, option_payload_offset, CallIdLayout,
    RecordLayout,
};
use super::cells::CellLayout;

// ─── WIT names referenced by codegen ──────────────────────────────
//
// Schema dependencies, named once here so a WIT rename surfaces as
// one or two diffs in this file rather than scattered string
// literals.

// Typedef names in `splicer:common/types`.
const TYPEDEF_FIELD: &str = "field";
const TYPEDEF_FIELD_TREE: &str = "field-tree";
const TYPEDEF_CELL: &str = "cell";
const TYPEDEF_ENUM_INFO: &str = "enum-info";
const TYPEDEF_FLAGS_INFO: &str = "flags-info";
const TYPEDEF_RECORD_INFO: &str = "record-info";

// Field names within those records.
pub(super) const FIELD_NAME: &str = "name";
pub(super) const FIELD_TREE: &str = "tree";
pub(super) const TREE_CELLS: &str = "cells";
pub(super) const TREE_ENUM_INFOS: &str = "enum-infos";
pub(super) const TREE_FLAGS_INFOS: &str = "flags-infos";
pub(super) const TREE_RECORD_INFOS: &str = "record-infos";
pub(super) const TREE_ROOT: &str = "root";
/// Field name on `record flags-info { … }` for the (runtime-filled)
/// list of currently-set flag names.
pub(super) const FLAGS_INFO_SET_FLAGS: &str = "set-flags";
/// Field name on `record record-info { … }` for the (name, cell-idx)
/// tuple list.
pub(super) const RECORD_INFO_FIELDS: &str = "fields";
/// Synthetic field names for the anonymous `tuple<string, u32>` that
/// holds one record's `(field-name, child-cell-idx)` pair. Tuples
/// are positional — these names are only used to look up offsets in
/// the [`RecordLayout`] we synthesize via `for_named_fields`.
pub(super) const RECORD_FIELD_TUPLE_NAME: &str = "name";
pub(super) const RECORD_FIELD_TUPLE_IDX: &str = "idx";

// Field names within the on-call / on-return func-params records.
pub(super) const ON_CALL_CALL: &str = "call";
pub(super) const ON_CALL_ARGS: &str = "args";
pub(super) const ON_RET_CALL: &str = "call";
pub(super) const ON_RET_RESULT: &str = "result";

/// Schema-driven layouts + hook descriptors gathered up front so
/// later phases see one bundle instead of a dozen locals.
pub(super) struct SchemaLayouts {
    pub(super) size_align: SizeAlign,
    pub(super) field_layout: RecordLayout,
    pub(super) tree_layout: RecordLayout,
    pub(super) cell_layout: CellLayout,
    pub(super) callid_layout: CallIdLayout,
    pub(super) enum_info_layout: RecordLayout,
    /// Layout of `record flags-info { type-name, set-flags }` (the
    /// per-flags-cell side-table entry).
    pub(super) flags_info_layout: RecordLayout,
    /// Layout of `record record-info { type-name, fields }` (the
    /// per-record-cell side-table entry).
    pub(super) record_info_layout: RecordLayout,
    /// Layout of one element of `record-info.fields`, an anonymous
    /// `tuple<string, u32>`. Field names are synthetic (see
    /// [`RECORD_FIELD_TUPLE_NAME`] / [`RECORD_FIELD_TUPLE_IDX`]).
    pub(super) record_field_tuple_layout: RecordLayout,
    pub(super) before_hook: Option<HookSchema>,
    pub(super) after_hook: Option<HookSchema>,
    /// Byte offset of the `option<field-tree>` payload inside the
    /// option variant.
    pub(super) option_payload_off: u32,
}

/// One hook's import descriptor + the layout of its params record.
/// Bundled so callers can't forget that "hook wired" and "params
/// layout known" are the same thing.
pub(super) struct HookSchema {
    pub(super) import: HookImport,
    pub(super) params_layout: RecordLayout,
}

// Hook import struct lives in `super::super::abi::emit::HookImport`
// — re-exported below so tier-2 callers don't have to qualify it.
pub(super) use super::super::abi::emit::HookImport;

pub(super) fn compute_schema(
    resolve: &Resolve,
    world_id: WorldId,
    has_before: bool,
    has_after: bool,
) -> Result<SchemaLayouts> {
    let mut size_align = SizeAlign::default();
    size_align.fill(resolve);

    let field_ty_id = find_common_typeid(resolve, TYPEDEF_FIELD)?;
    let field_tree_ty_id = find_common_typeid(resolve, TYPEDEF_FIELD_TREE)?;
    let cell_ty_id = find_common_typeid(resolve, TYPEDEF_CELL)?;
    let enum_info_ty = find_common_typeid(resolve, TYPEDEF_ENUM_INFO)?;
    let flags_info_ty = find_common_typeid(resolve, TYPEDEF_FLAGS_INFO)?;
    let record_info_ty = find_common_typeid(resolve, TYPEDEF_RECORD_INFO)?;

    let field_layout = RecordLayout::for_record_typedef(&size_align, resolve, field_ty_id);
    let tree_layout = RecordLayout::for_record_typedef(&size_align, resolve, field_tree_ty_id);
    let cell_layout = CellLayout::from_resolve(&size_align, resolve, cell_ty_id);
    let callid_layout = call_id_layout(resolve, &size_align)?;
    let enum_info_layout = RecordLayout::for_record_typedef(&size_align, resolve, enum_info_ty);
    let flags_info_layout = RecordLayout::for_record_typedef(&size_align, resolve, flags_info_ty);
    let record_info_layout = RecordLayout::for_record_typedef(&size_align, resolve, record_info_ty);
    // The anonymous `tuple<string, u32>` element of `record-info.fields`
    // — synthesize a RecordLayout for it with positional names so the
    // record-info builder can do `offset_of(RECORD_FIELD_TUPLE_NAME)` /
    // `offset_of(RECORD_FIELD_TUPLE_IDX)`.
    let record_field_tuple_layout = RecordLayout::for_named_fields(
        &size_align,
        &[
            (RECORD_FIELD_TUPLE_NAME.to_string(), Type::String),
            (RECORD_FIELD_TUPLE_IDX.to_string(), Type::U32),
        ],
    );

    let before_hook = has_before
        .then(|| find_on_call_hook(resolve, world_id))
        .transpose()?
        .map(|import| HookSchema {
            params_layout: RecordLayout::for_named_fields(&size_align, &import.params),
            import,
        });
    let after_hook = has_after
        .then(|| find_on_return_hook(resolve, world_id))
        .transpose()?
        .map(|import| HookSchema {
            params_layout: RecordLayout::for_named_fields(&size_align, &import.params),
            import,
        });

    let option_payload_off = option_payload_offset(&size_align, &Type::Id(field_tree_ty_id));

    Ok(SchemaLayouts {
        size_align,
        field_layout,
        tree_layout,
        cell_layout,
        callid_layout,
        enum_info_layout,
        flags_info_layout,
        record_info_layout,
        record_field_tuple_layout,
        before_hook,
        after_hook,
        option_payload_off,
    })
}

fn find_on_call_hook(resolve: &Resolve, world_id: WorldId) -> Result<HookImport> {
    use crate::contract::{TIER2_BEFORE, TIER2_VERSION};
    let qname = format!("{TIER2_BEFORE}@{TIER2_VERSION}");
    find_imported_hook(resolve, world_id, &qname)
        .ok_or_else(|| anyhow!("synthesized adapter world is missing import of `{qname}`"))
}

fn find_on_return_hook(resolve: &Resolve, world_id: WorldId) -> Result<HookImport> {
    use crate::contract::{TIER2_AFTER, TIER2_VERSION};
    let qname = format!("{TIER2_AFTER}@{TIER2_VERSION}");
    find_imported_hook(resolve, world_id, &qname)
        .ok_or_else(|| anyhow!("synthesized adapter world is missing import of `{qname}`"))
}
