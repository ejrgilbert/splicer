//! Schema-driven layouts + hook import descriptors gathered up front
//! into one `SchemaLayouts` bundle for later phases.

use anyhow::{anyhow, Result};
use wit_parser::{Resolve, SizeAlign, Type, WorldId};

use super::super::abi::emit::{
    call_id_layout, find_common_typeid, find_imported_hook, option_payload_offset, CallIdLayout,
    RecordLayout,
};
use super::cells::CellLayout;

// ─── WIT names referenced by codegen ──────────────────────────────
// Named once so a WIT rename surfaces in one or two diffs.

// Typedef names in `splicer:common/types`.
const TYPEDEF_FIELD: &str = "field";
const TYPEDEF_FIELD_TREE: &str = "field-tree";
const TYPEDEF_CELL: &str = "cell";
const TYPEDEF_ENUM_INFO: &str = "enum-info";
const TYPEDEF_FLAGS_INFO: &str = "flags-info";
const TYPEDEF_RECORD_INFO: &str = "record-info";
const TYPEDEF_VARIANT_INFO: &str = "variant-info";
const TYPEDEF_HANDLE_INFO: &str = "handle-info";

// Field names within those records.
pub(super) const FIELD_NAME: &str = "name";
pub(super) const FIELD_TREE: &str = "tree";
pub(super) const TREE_CELLS: &str = "cells";
pub(super) const TREE_ENUM_INFOS: &str = "enum-infos";
pub(super) const TREE_FLAGS_INFOS: &str = "flags-infos";
pub(super) const TREE_RECORD_INFOS: &str = "record-infos";
pub(super) const TREE_VARIANT_INFOS: &str = "variant-infos";
pub(super) const TREE_HANDLE_INFOS: &str = "handle-infos";
pub(super) const TREE_ROOT: &str = "root";
pub(super) const FLAGS_INFO_SET_FLAGS: &str = "set-flags";
/// `case-name` and `payload` are runtime-filled per call.
pub(super) const VARIANT_INFO_CASE_NAME: &str = "case-name";
pub(super) const VARIANT_INFO_PAYLOAD: &str = "payload";
/// `type-name` baked at build time; `id` patched per call.
pub(super) const HANDLE_INFO_TYPE_NAME: &str = "type-name";
pub(super) const HANDLE_INFO_ID: &str = "id";
pub(super) const RECORD_INFO_FIELDS: &str = "fields";
/// Synthetic names for the anonymous `tuple<string, u32>` element of
/// `record-info.fields`; only used to look up offsets via `for_named_fields`.
pub(super) const RECORD_FIELD_TUPLE_NAME: &str = "name";
pub(super) const RECORD_FIELD_TUPLE_IDX: &str = "idx";

// Field names within the on-call / on-return func-params records.
pub(super) const ON_CALL_CALL: &str = "call";
pub(super) const ON_CALL_ARGS: &str = "args";
pub(super) const ON_RET_CALL: &str = "call";
pub(super) const ON_RET_RESULT: &str = "result";

/// Schema-driven layouts + hook descriptors.
pub(super) struct SchemaLayouts {
    pub(super) size_align: SizeAlign,
    pub(super) field_layout: RecordLayout,
    pub(super) tree_layout: RecordLayout,
    pub(super) cell_layout: CellLayout,
    pub(super) callid_layout: CallIdLayout,
    pub(super) enum_info_layout: RecordLayout,
    pub(super) flags_info_layout: RecordLayout,
    pub(super) record_info_layout: RecordLayout,
    pub(super) variant_info_layout: RecordLayout,
    /// Offset of `option<u32>` payload's u32 slot inside the
    /// variant-info `payload` field.
    pub(super) variant_info_payload_value_off: u32,
    pub(super) handle_info_layout: RecordLayout,
    /// Anonymous `tuple<string, u32>` element of `record-info.fields`,
    /// keyed by synthetic names.
    pub(super) record_field_tuple_layout: RecordLayout,
    pub(super) before_hook: Option<HookSchema>,
    pub(super) after_hook: Option<HookSchema>,
    /// Gate hook; uses the same params record as `before_hook` (call
    /// + args) but with an extra retptr param for the bool result.
    pub(super) gate_hook: Option<HookSchema>,
    /// Offset of `option<field-tree>` payload inside the option variant.
    pub(super) option_payload_off: u32,
}

/// Hook import + params record layout. Bundled so "hook wired" and
/// "params layout known" stay synonymous.
pub(super) struct HookSchema {
    pub(super) import: HookImport,
    pub(super) params_layout: RecordLayout,
}

pub(super) use super::super::abi::emit::HookImport;

pub(super) fn compute_schema(
    resolve: &Resolve,
    world_id: WorldId,
    has_before: bool,
    has_after: bool,
    has_gate: bool,
) -> Result<SchemaLayouts> {
    let mut size_align = SizeAlign::default();
    size_align.fill(resolve);

    let field_ty_id = find_common_typeid(resolve, TYPEDEF_FIELD)?;
    let field_tree_ty_id = find_common_typeid(resolve, TYPEDEF_FIELD_TREE)?;
    let cell_ty_id = find_common_typeid(resolve, TYPEDEF_CELL)?;
    let enum_info_ty = find_common_typeid(resolve, TYPEDEF_ENUM_INFO)?;
    let flags_info_ty = find_common_typeid(resolve, TYPEDEF_FLAGS_INFO)?;
    let record_info_ty = find_common_typeid(resolve, TYPEDEF_RECORD_INFO)?;
    let variant_info_ty = find_common_typeid(resolve, TYPEDEF_VARIANT_INFO)?;
    let handle_info_ty = find_common_typeid(resolve, TYPEDEF_HANDLE_INFO)?;

    let field_layout = RecordLayout::for_record_typedef(&size_align, resolve, field_ty_id);
    let tree_layout = RecordLayout::for_record_typedef(&size_align, resolve, field_tree_ty_id);
    let cell_layout = CellLayout::from_resolve(&size_align, resolve, cell_ty_id);
    let callid_layout = call_id_layout(resolve, &size_align)?;
    let enum_info_layout = RecordLayout::for_record_typedef(&size_align, resolve, enum_info_ty);
    let flags_info_layout = RecordLayout::for_record_typedef(&size_align, resolve, flags_info_ty);
    let record_info_layout = RecordLayout::for_record_typedef(&size_align, resolve, record_info_ty);
    let variant_info_layout =
        RecordLayout::for_record_typedef(&size_align, resolve, variant_info_ty);
    let handle_info_layout = RecordLayout::for_record_typedef(&size_align, resolve, handle_info_ty);
    // Disc byte at +0; u32 idx at +variant_info_payload_value_off.
    let variant_info_payload_value_off = option_payload_offset(&size_align, &Type::U32);
    // Synthesize a RecordLayout for the anonymous `tuple<string, u32>`
    // so the record-info builder can look fields up by name.
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
    let gate_hook = has_gate
        .then(|| find_should_call_hook(resolve, world_id))
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
        variant_info_layout,
        variant_info_payload_value_off,
        handle_info_layout,
        record_field_tuple_layout,
        before_hook,
        after_hook,
        gate_hook,
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

fn find_should_call_hook(resolve: &Resolve, world_id: WorldId) -> Result<HookImport> {
    use crate::contract::{TIER2_GATE, TIER2_VERSION};
    let qname = format!("{TIER2_GATE}@{TIER2_VERSION}");
    find_imported_hook(resolve, world_id, &qname)
        .ok_or_else(|| anyhow!("synthesized adapter world is missing import of `{qname}`"))
}
