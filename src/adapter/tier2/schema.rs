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

use anyhow::{anyhow, bail, Result};
use wit_parser::abi::{AbiVariant, WasmSignature};
use wit_parser::{Resolve, SizeAlign, Type, WasmImport, WorldId, WorldItem};

use super::super::abi::emit::{
    call_id_record_layout, find_common_typeid, option_payload_offset, RecordLayout,
};
use super::super::resolve::hook_callback_mangling;
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
const TYPEDEF_RECORD_INFO: &str = "record-info";

// Field names within those records.
pub(super) const FIELD_NAME: &str = "name";
pub(super) const FIELD_TREE: &str = "tree";
pub(super) const TREE_CELLS: &str = "cells";
pub(super) const TREE_ENUM_INFOS: &str = "enum-infos";
pub(super) const TREE_RECORD_INFOS: &str = "record-infos";
pub(super) const TREE_ROOT: &str = "root";
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
    pub(super) callid_layout: RecordLayout,
    pub(super) enum_info_layout: RecordLayout,
    /// Layout of `record record-info { type-name, fields }` (the
    /// per-record-cell side-table entry).
    pub(super) record_info_layout: RecordLayout,
    /// Layout of one element of `record-info.fields`, an anonymous
    /// `tuple<string, u32>`. Field names are synthetic (see
    /// [`RECORD_FIELD_TUPLE_NAME`] / [`RECORD_FIELD_TUPLE_IDX`]).
    pub(super) record_field_tuple_layout: RecordLayout,
    pub(super) before_hook: Option<HookImport>,
    pub(super) after_hook: Option<HookImport>,
    pub(super) on_call_params_layout: Option<RecordLayout>,
    pub(super) on_return_params_layout: Option<RecordLayout>,
    /// Byte offset of the `option<field-tree>` payload inside the
    /// option variant.
    pub(super) option_payload_off: u32,
}

/// Resolved on-call hook info — module + name + signature, all
/// sourced from `Resolve` so wit-component agrees on the canonical
/// names.
pub(super) struct HookImport {
    pub(super) module: String,
    pub(super) name: String,
    pub(super) sig: WasmSignature,
    /// `(name, type)` per WIT param. Used to derive the
    /// indirect-params buffer's [`RecordLayout`] from the schema.
    params: Vec<(String, Type)>,
}

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
    let record_info_ty = find_common_typeid(resolve, TYPEDEF_RECORD_INFO)?;

    let field_layout = RecordLayout::for_record_typedef(&size_align, resolve, field_ty_id);
    let tree_layout = RecordLayout::for_record_typedef(&size_align, resolve, field_tree_ty_id);
    let cell_layout = CellLayout::from_resolve(&size_align, resolve, cell_ty_id);
    let callid_layout = call_id_record_layout(resolve, &size_align)?;
    let enum_info_layout = RecordLayout::for_record_typedef(&size_align, resolve, enum_info_ty);
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
        .transpose()?;
    let after_hook = has_after
        .then(|| find_on_return_hook(resolve, world_id))
        .transpose()?;

    let on_call_params_layout = before_hook
        .as_ref()
        .map(|h| RecordLayout::for_named_fields(&size_align, &h.params));
    let on_return_params_layout = after_hook
        .as_ref()
        .map(|h| RecordLayout::for_named_fields(&size_align, &h.params));
    let option_payload_off = option_payload_offset(&size_align, &Type::Id(field_tree_ty_id));

    Ok(SchemaLayouts {
        size_align,
        field_layout,
        tree_layout,
        cell_layout,
        callid_layout,
        enum_info_layout,
        record_info_layout,
        record_field_tuple_layout,
        before_hook,
        after_hook,
        on_call_params_layout,
        on_return_params_layout,
        option_payload_off,
    })
}

fn find_on_call_hook(resolve: &Resolve, world_id: WorldId) -> Result<HookImport> {
    use crate::contract::{TIER2_BEFORE, TIER2_VERSION};
    find_tier2_hook(
        resolve,
        world_id,
        &format!("{TIER2_BEFORE}@{TIER2_VERSION}"),
    )
}

fn find_on_return_hook(resolve: &Resolve, world_id: WorldId) -> Result<HookImport> {
    use crate::contract::{TIER2_AFTER, TIER2_VERSION};
    find_tier2_hook(resolve, world_id, &format!("{TIER2_AFTER}@{TIER2_VERSION}"))
}

fn find_tier2_hook(resolve: &Resolve, world_id: WorldId, target_iface: &str) -> Result<HookImport> {
    let world = &resolve.worlds[world_id];
    for (key, item) in &world.imports {
        if let WorldItem::Interface { id, .. } = item {
            if resolve.id_of(*id).as_deref() != Some(target_iface) {
                continue;
            }
            let func = resolve.interfaces[*id]
                .functions
                .values()
                .next()
                .ok_or_else(|| anyhow!("`{target_iface}` has no functions"))?;
            let (module, name) = resolve.wasm_import_name(
                hook_callback_mangling(),
                WasmImport::Func {
                    interface: Some(key),
                    func,
                },
            );
            let sig = resolve.wasm_signature(AbiVariant::GuestImportAsync, func);
            let params = func.params.iter().map(|p| (p.name.clone(), p.ty)).collect();
            return Ok(HookImport {
                module,
                name,
                sig,
                params,
            });
        }
    }
    bail!("synthesized adapter world is missing import of `{target_iface}`")
}
