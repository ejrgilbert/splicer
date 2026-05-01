//! Tier-2 adapter generator: builds an adapter component that lifts
//! a target function's canonical-ABI parameters and result into the
//! cell-array representation and forwards them to the middleware's
//! tier-2 `on-call` / `on-return` hooks around the handler call.
//!
//! Sync and async target functions are both supported; the wrapper
//! body emits `task.return` for async dispatches.
//!
//! Compound type lift (`record`, `variant`, `option`, `result`,
//! `list<T>` for `T != u8`, `tuple`, `enum`, `flags`, `char`) is
//! classified end-to-end through `LiftKind` but `todo!()`s at the
//! codegen layer (`super::cells`) — the gap is visible in code at
//! the lowest level, not hidden behind a fallback.
//!
//! Design conventions intentionally mirror the tier-1 emit path
//! (`super::super::tier1::emit`).

use anyhow::{anyhow, bail, Context, Result};
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, MemArg, Module, TypeSection, ValType,
};
use wit_component::{embed_component_metadata, ComponentEncoder, StringEncoding};
use wit_parser::abi::{AbiVariant, WasmSignature};
use wit_parser::{
    Function as WitFunction, InterfaceId, Mangling, Resolve, SizeAlign, Type, TypeId, WasmExport,
    WasmExportKind, WasmImport, WorldId, WorldItem, WorldKey,
};

use wit_bindgen_core::abi::lift_from_memory;

use super::super::abi::canon_async;
use super::super::abi::emit::{
    emit_cabi_realloc, emit_handler_call, emit_memory_and_globals, emit_wrapper_return,
    empty_function, option_payload_offset, val_types, RecordLayout, EXPORT_CABI_REALLOC,
    EXPORT_INITIALIZE, EXPORT_MEMORY,
};
use super::super::abi::WasmEncoderBindgen;
use super::super::indices::FunctionIndices;
use super::super::mem_layout::StaticLayout;
use super::super::resolve::{
    decode_input_resolve, dispatch_mangling, find_target_interface, hook_callback_mangling,
};
use super::blob::{BlobSlice, RecordWriter};
use super::cells::CellLayout;
use super::lift::{
    alloc_wrapper_locals, build_enum_info_blob, build_record_info_blob, classify_func_params,
    classify_result_lift, emit_lift_param, emit_lift_result, register_enum_strings,
    register_record_strings, ParamLift, ResultLift, WrapperLocals,
};

const TIER2_ADAPTER_WORLD_PACKAGE: &str = "splicer:adapter-tier2";
const TIER2_ADAPTER_WORLD_NAME: &str = "adapter";

/// Generate a tier-2 adapter component.
pub(crate) fn build_tier2_adapter(
    target_interface: &str,
    has_before: bool,
    has_after: bool,
    split_bytes: &[u8],
    common_wit: &str,
    tier2_wit: &str,
) -> Result<Vec<u8>> {
    if !has_before && !has_after {
        bail!(
            "tier-2 adapter generation requires the middleware to export at least \
             one of `splicer:tier2/before` or `splicer:tier2/after` — `trap`-only \
             middleware is planned for a follow-up slice."
        );
    }

    let mut resolve = decode_input_resolve(split_bytes)?;
    let target_iface = find_target_interface(&resolve, target_interface)?;
    require_supported_case(&resolve, target_iface)?;

    resolve
        .push_str("splicer-common.wit", common_wit)
        .context("parse common WIT")?;
    resolve
        .push_str("splicer-tier2.wit", tier2_wit)
        .context("parse tier2 WIT")?;
    let world_pkg = resolve
        .push_str(
            "splicer-adapter-tier2.wit",
            &synthesize_adapter_world_wit(target_interface, has_before, has_after),
        )
        .context("parse synthesized tier-2 adapter world WIT")?;
    let world_id = resolve
        .select_world(&[world_pkg], Some(TIER2_ADAPTER_WORLD_NAME))
        .context("select tier-2 adapter world")?;

    let mut core_module = build_dispatch_module(
        &resolve,
        world_id,
        target_iface,
        target_interface,
        has_before,
        has_after,
    )?;
    embed_component_metadata(&mut core_module, &resolve, world_id, StringEncoding::UTF8)
        .context("embed_component_metadata")?;

    ComponentEncoder::default()
        .validate(true)
        .module(&core_module)
        .context("ComponentEncoder::module")?
        .encode()
        .context("ComponentEncoder::encode")
}

/// Bail on cases that fail before the lift codegen even runs.
fn require_supported_case(resolve: &Resolve, target_iface: InterfaceId) -> Result<()> {
    let iface = &resolve.interfaces[target_iface];
    if iface.functions.is_empty() {
        bail!("interface has no functions");
    }
    Ok(())
}

/// Synthesize the tier-2 adapter world.
fn synthesize_adapter_world_wit(
    target_interface: &str,
    has_before: bool,
    has_after: bool,
) -> String {
    use crate::contract::{versioned_interface, TIER2_AFTER, TIER2_BEFORE, TIER2_VERSION};
    let mut wit =
        format!("package {TIER2_ADAPTER_WORLD_PACKAGE};\n\nworld {TIER2_ADAPTER_WORLD_NAME} {{\n");
    wit.push_str(&format!("    import {target_interface};\n"));
    wit.push_str(&format!("    export {target_interface};\n"));
    if has_before {
        wit.push_str(&format!(
            "    import {};\n",
            versioned_interface(TIER2_BEFORE, TIER2_VERSION)
        ));
    }
    if has_after {
        wit.push_str(&format!(
            "    import {};\n",
            versioned_interface(TIER2_AFTER, TIER2_VERSION)
        ));
    }
    wit.push_str("}\n");
    wit
}

// ─── Dispatch core module ──────────────────────────────────────────

/// `task.return` import for one async target function. The wrapper
/// body calls this at the end of an async dispatch to publish the
/// result.
pub(super) struct TaskReturnImport {
    pub module: String,
    pub name: String,
    pub sig: WasmSignature,
}

/// Sync/async shape of one target function. Holds the
/// `task.return` import directly in the async variant — there's no
/// "async without task.return" or "sync with task.return" state.
pub(super) enum FuncShape {
    Sync,
    Async(TaskReturnImport),
}

impl FuncShape {
    pub(super) fn is_async(&self) -> bool {
        matches!(self, FuncShape::Async(_))
    }
    pub(super) fn task_return(&self) -> Option<&TaskReturnImport> {
        match self {
            FuncShape::Async(tr) => Some(tr),
            FuncShape::Sync => None,
        }
    }
}

/// Per-function on-return hook setup, populated only when the
/// middleware exports `splicer:tier2/after`. Bundles the two
/// always-paired offsets (after-hook indirect-params buffer +
/// optional result-cell scratch) so callers can branch on
/// `Option<AfterSetup>` without separate "is after wired?" /
/// "does this fn have a result?" checks.
pub(super) struct AfterSetup {
    /// Byte offset of the pre-built on-return indirect-params buffer.
    pub params_offset: i32,
    /// Byte offset of the 1-cell result scratch slab. `None` for
    /// void-returning funcs (still need params_offset, but no result
    /// to lift).
    pub result_cells_offset: Option<u32>,
}

pub(super) struct FuncDispatch {
    pub shape: FuncShape,
    /// WIT result type, kept around so async wrappers can drive
    /// `lift_from_memory` to flat-load the result for `task.return`.
    pub result_ty: Option<Type>,
    pub import_module: String,
    pub import_field: String,
    pub export_name: String,
    /// Wrapper export sig (`AbiVariant::GuestExport`) — the shape
    /// `wit-component`'s validator expects for our exported wrapper.
    pub export_sig: WasmSignature,
    /// Handler import sig (`AbiVariant::GuestImport`) — the shape
    /// `wit-component`'s validator expects for our import declaration.
    /// May differ from `export_sig` for compound-result functions
    /// (caller-allocates retptr on the import side vs. callee-returns
    /// pointer on the export side).
    pub import_sig: WasmSignature,
    pub needs_cabi_post: bool,
    /// Byte offset of the function name within the data segment.
    pub fn_name_offset: i32,
    pub fn_name_len: i32,
    /// Per-param lift recipe + wasm flat-slot indexing. Empty for
    /// zero-arg functions. Each param's [`ParamLift::cells_offset`]
    /// holds the offset of its own cells slab — there's no longer a
    /// shared per-fn slab base, since record params consume more than
    /// one cell.
    pub params: Vec<ParamLift>,
    /// Byte offset of this function's pre-built `field` records in
    /// the data segment. Holds `params.len()` consecutive `field`
    /// records, each [`FIELD_SIZE_BYTES`] bytes. Pointed at by the
    /// `args.list.ptr` field passed to `on-call`.
    pub fields_buf_offset: u32,
    /// Byte offset of the retptr scratch buffer; `Some` iff the
    /// import sig wants a caller-allocates retptr but the export sig
    /// returns the pointer directly. The wrapper passes this as the
    /// extra trailing arg when calling the import, then loads from it
    /// to produce its own return value.
    pub retptr_offset: Option<i32>,
    /// How to lift the function's return value into a `cell` for the
    /// on-return hook. `None` for void or compound returns we don't
    /// yet lift (Phase 2-2b territory).
    pub result_lift: Option<ResultLift>,
    /// On-return-hook scaffolding; `Some` iff after-hook is wired.
    pub after: Option<AfterSetup>,
}

// ─── WIT names referenced by codegen ──────────────────────────────
//
// Schema dependencies, named once here so a WIT rename surfaces as
// one or two diffs in this file rather than scattered string
// literals.

// Typedef names in `splicer:common/types`.
const TYPEDEF_FIELD: &str = "field";
const TYPEDEF_FIELD_TREE: &str = "field-tree";
const TYPEDEF_CALL_ID: &str = "call-id";
const TYPEDEF_CELL: &str = "cell";
const TYPEDEF_ENUM_INFO: &str = "enum-info";
const TYPEDEF_RECORD_INFO: &str = "record-info";

// Field names within those records.
const FIELD_NAME: &str = "name";
const FIELD_TREE: &str = "tree";
const TREE_CELLS: &str = "cells";
const TREE_ENUM_INFOS: &str = "enum-infos";
const TREE_RECORD_INFOS: &str = "record-infos";
const TREE_ROOT: &str = "root";
const CALLID_IFACE: &str = "interface-name";
const CALLID_FN: &str = "function-name";
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
const ON_CALL_CALL: &str = "call";
const ON_CALL_ARGS: &str = "args";
const ON_RET_CALL: &str = "call";
const ON_RET_RESULT: &str = "result";

// ─── ABI-anchored constants (not WIT-schema-derivable) ────────────

/// Size + alignment of the `waitable-set.wait` event record slot.
/// This is wit-component runtime ABI, not anything from our WIT.
const EVENT_SLOT_SIZE: u32 = 8;
const EVENT_SLOT_ALIGN: u32 = 4;

/// Per-call values written into the on-call indirect-params buffer.
/// Slice-typed (vs. raw i32 pairs) so callers can't swap ptr/len.
struct OnCallCallSite {
    iface_name: BlobSlice,
    fn_name: BlobSlice,
    args: BlobSlice,
}

/// Emit wasm that writes the call-id (interface + function name
/// pointers/lengths) and the per-call `list<field>` args pointer/
/// length into the indirect-params buffer at `base_ptr`. Field
/// offsets are looked up from the schema at use site so the
/// canonical-ABI numbers stay schema-driven.
fn emit_populate_hook_params(
    f: &mut Function,
    base_ptr: i32,
    schema: &SchemaLayouts,
    site: &OnCallCallSite,
) {
    let on_call = schema
        .on_call_params_layout
        .as_ref()
        .expect("emit_populate_hook_params called only when before-hook wired");
    let call_off = on_call.offset_of(ON_CALL_CALL);
    let args_off = on_call.offset_of(ON_CALL_ARGS);
    let iface_off = call_off + schema.callid_layout.offset_of(CALLID_IFACE);
    let fn_off = call_off + schema.callid_layout.offset_of(CALLID_FN);
    emit_store_slice(f, base_ptr, iface_off, site.iface_name);
    emit_store_slice(f, base_ptr, fn_off, site.fn_name);
    emit_store_slice(f, base_ptr, args_off, site.args);
}

/// Emit two `i32.store`s writing `slice.off` then `slice.len` into
/// the `(ptr, len)` pair starting at `base_ptr + field_off`.
fn emit_store_slice(f: &mut Function, base_ptr: i32, field_off: u32, slice: BlobSlice) {
    use super::super::abi::emit::{SLICE_LEN_OFFSET, SLICE_PTR_OFFSET};
    let store = |f: &mut Function, sub_off: u32, value: i32| {
        f.instructions().i32_const(base_ptr);
        f.instructions().i32_const(value);
        f.instructions().i32_store(MemArg {
            offset: (field_off + sub_off) as u64,
            align: 2,
            memory_index: 0,
        });
    };
    store(f, SLICE_PTR_OFFSET, slice.off as i32);
    store(f, SLICE_LEN_OFFSET, slice.len as i32);
}

/// Schema-driven layouts + hook descriptors gathered up front so
/// later phases see one bundle instead of a dozen locals.
pub(super) struct SchemaLayouts {
    pub size_align: SizeAlign,
    pub field_layout: RecordLayout,
    pub tree_layout: RecordLayout,
    pub cell_layout: CellLayout,
    pub callid_layout: RecordLayout,
    pub enum_info_layout: RecordLayout,
    /// Layout of `record record-info { type-name, fields }` (the
    /// per-record-cell side-table entry).
    pub record_info_layout: RecordLayout,
    /// Layout of one element of `record-info.fields`, an anonymous
    /// `tuple<string, u32>`. Field names are synthetic (see
    /// [`RECORD_FIELD_TUPLE_NAME`] / [`RECORD_FIELD_TUPLE_IDX`]).
    pub record_field_tuple_layout: RecordLayout,
    before_hook: Option<HookImport>,
    after_hook: Option<HookImport>,
    on_call_params_layout: Option<RecordLayout>,
    on_return_params_layout: Option<RecordLayout>,
    /// Byte offset of the `option<field-tree>` payload inside the
    /// option variant.
    option_payload_off: u32,
}

/// Output of the static-memory layout phase: the addresses the
/// emit-code phase needs to reference, plus the data segments
/// ready to feed `emit_data_section`.
struct StaticDataPlan {
    bump_start: u32,
    event_ptr: i32,
    hook_params_ptr: u32,
    data_segments: Vec<(u32, Vec<u8>)>,
}

/// Side-table absolute pointers for one field-tree. Each kind's
/// `BlobSlice` patches into the matching `field-tree.<kind>-infos`
/// list pair; `BlobSlice::EMPTY` leaves the slot zeroed (i.e. the
/// field doesn't carry that kind). Adding a new kind means adding
/// a field here + a [`FieldSideTables::write_to_tree`] line.
#[derive(Clone, Copy, Default)]
struct FieldSideTables {
    enum_infos: BlobSlice,
    record_infos: BlobSlice,
    // flags_infos: BlobSlice,
    // variant_infos: BlobSlice,
    // handle_infos: BlobSlice,
}

impl FieldSideTables {
    fn write_to_tree(&self, blob: &mut [u8], tree: &RecordWriter) {
        tree.write_slice(blob, TREE_ENUM_INFOS, self.enum_infos);
        tree.write_slice(blob, TREE_RECORD_INFOS, self.record_infos);
        // tree.write_slice(blob, TREE_FLAGS_INFOS, self.flags_infos);
        // ...
    }
}

/// Single-pass build of a `field` record + its embedded
/// `field-tree` for one (function, param) pair. `cells` points at
/// the param's contiguous cells slab (`(slab-offset, cell-count)`).
/// `side_tables` patches the field-tree's per-kind-infos lists for
/// any kinds the param's plan carries.
fn write_field_record(
    blob: &mut Vec<u8>,
    schema: &SchemaLayouts,
    cells: BlobSlice,
    name: BlobSlice,
    side_tables: FieldSideTables,
) {
    let field = RecordWriter::extend_zero(blob, &schema.field_layout);
    field.write_slice(blob, FIELD_NAME, name);
    let tree = field.nested(FIELD_TREE, &schema.tree_layout);
    tree.write_slice(blob, TREE_CELLS, cells);
    side_tables.write_to_tree(blob, &tree);
    // Root cell is always `cells[0]` for the plan-builder.
    tree.write_i32(blob, TREE_ROOT, 0);
}

/// Build the contiguous fields blob: one `field` record per
/// (fn, param). `param_side_tables[fn_idx][param_idx]` carries the
/// param's per-kind side-table pointers (or `EMPTY` slots for kinds
/// the param doesn't carry).
fn build_fields_blob(
    per_func: &[FuncDispatch],
    schema: &SchemaLayouts,
    param_side_tables: &[Vec<FieldSideTables>],
) -> Vec<u8> {
    let mut blob: Vec<u8> = Vec::new();
    for (fn_idx, fd) in per_func.iter().enumerate() {
        for (i, p) in fd.params.iter().enumerate() {
            // The field-tree's `cells.ptr` points at the param's
            // slab; `cells.len = plan.cell_count()`. `root` is always
            // 0 because the plan-builder allocates the root cell
            // first into each plan.
            write_field_record(
                &mut blob,
                schema,
                BlobSlice {
                    off: p.cells_offset,
                    len: p.plan.cell_count(),
                },
                p.name,
                param_side_tables[fn_idx][i],
            );
        }
    }
    blob
}

/// Build the contiguous on-return params blob: one record per fn,
/// with `result: option::some(field-tree)` pre-wired for funcs that
/// have a result lift, `option::none` for the rest.
fn build_after_params_blob(
    per_func: &[FuncDispatch],
    schema: &SchemaLayouts,
    iface_name: BlobSlice,
    result_side_tables: &[FieldSideTables],
) -> Vec<u8> {
    let Some(after_layout) = schema.on_return_params_layout.as_ref() else {
        return Vec::new();
    };
    let mut blob: Vec<u8> = Vec::new();
    for (fn_idx, fd) in per_func.iter().enumerate() {
        let entry = RecordWriter::extend_zero(&mut blob, after_layout);
        let call = entry.nested(ON_RET_CALL, &schema.callid_layout);
        call.write_slice(&mut blob, CALLID_IFACE, iface_name);
        call.write_slice(
            &mut blob,
            CALLID_FN,
            BlobSlice {
                off: fd.fn_name_offset as u32,
                len: fd.fn_name_len as u32,
            },
        );
        match fd.after.as_ref().and_then(|a| a.result_cells_offset) {
            Some(cells_off) => {
                entry.write_option_some(&mut blob, ON_RET_RESULT);
                let tree_base =
                    entry.field_offset(ON_RET_RESULT) + schema.option_payload_off as usize;
                let tree = RecordWriter::at(&schema.tree_layout, tree_base);
                tree.write_slice(
                    &mut blob,
                    TREE_CELLS,
                    BlobSlice {
                        off: cells_off,
                        len: 1,
                    },
                );
                result_side_tables[fn_idx].write_to_tree(&mut blob, &tree);
                tree.write_i32(&mut blob, TREE_ROOT, 0);
            }
            None => entry.write_option_none(&mut blob, ON_RET_RESULT),
        }
    }
    blob
}

/// Reserve scratch + place data segments for everything the wrapper
/// body references at runtime: cells slabs, fields blob, on-return
/// params blob, event slot, hook-params buffer, retptr scratch. Each
/// allocation goes through `StaticLayout` so alignment is enforced.
/// Mutates `per_func` to fill in the buffer-offset fields.
fn lay_out_static_memory(
    per_func: &mut [FuncDispatch],
    funcs: &[&WitFunction],
    schema: &SchemaLayouts,
    name_blob: &mut Vec<u8>,
    iface_name: BlobSlice,
) -> StaticDataPlan {
    // Side-table strings get appended to name_blob BEFORE we place
    // it — every side-table-info entry references these string
    // offsets, so they have to land in the data segment first.
    let enum_strings = register_enum_strings(per_func, name_blob);
    let record_strings = register_record_strings(per_func, name_blob);

    let mut layout = StaticLayout::new();

    layout.place_data(1, name_blob);

    // Cells slabs first — fields records embed pointers to these.
    // Each param contributes `plan.cell_count() * cell_size` bytes;
    // record params produce >1 cell, so per-param offsets get
    // recorded individually.
    for fd in per_func.iter_mut() {
        for p in fd.params.iter_mut() {
            let slab_size = p.plan.cell_count() * schema.cell_layout.size;
            p.cells_offset = layout.reserve_scratch(schema.cell_layout.align, slab_size);
        }
    }
    // Per-fn result-cell scratch, when after-hook is wired and the
    // function has a result to lift. Captured in a parallel Vec so
    // we can fold it into `fd.after` alongside `params_offset` once
    // both are known.
    let result_cells_offsets: Vec<Option<u32>> = if schema.after_hook.is_some() {
        per_func
            .iter()
            .map(|fd| {
                fd.result_lift.is_some().then(|| {
                    layout.reserve_scratch(schema.cell_layout.align, schema.cell_layout.size)
                })
            })
            .collect()
    } else {
        vec![None; per_func.len()]
    };

    // Place the per-(fn, field) enum-info side tables. `per_param` /
    // `per_result` come back relative to the blob's start; we
    // translate to absolute by adding the placed blob's data offset.
    let mut enum_info = build_enum_info_blob(per_func, &enum_strings, schema);
    let enum_info_base = layout.place_data(schema.enum_info_layout.align, &enum_info.bytes);
    enum_info.translate_to(enum_info_base);

    // Place the record-info side table. Two segments — the
    // `record-info` entries blob references the `(name, cell-idx)`
    // tuples blob, so we place tuples first, patch the entries' fields
    // pointers, then place entries and translate per-param ranges to
    // absolute.
    let mut record_info = build_record_info_blob(per_func, &record_strings, schema);
    let record_tuples_base =
        layout.place_data(schema.record_field_tuple_layout.align, &record_info.tuples);
    record_info.patch_fields_ptr(record_tuples_base);
    let record_info_base = layout.place_data(schema.record_info_layout.align, &record_info.entries);
    record_info.translate_to(record_info_base);

    // Bundle every kind's per-(fn, param) and per-(fn, result)
    // pointers into one `FieldSideTables` per field-tree, so the
    // blob writers don't grow another arg per kind.
    let param_side_tables: Vec<Vec<FieldSideTables>> = enum_info
        .per_param
        .iter()
        .zip(record_info.per_param_range.iter())
        .map(|(enums, records)| {
            enums
                .iter()
                .zip(records.iter())
                .map(|(&enum_infos, &record_infos)| FieldSideTables {
                    enum_infos,
                    record_infos,
                })
                .collect()
        })
        .collect();
    let result_side_tables: Vec<FieldSideTables> = enum_info
        .per_result
        .iter()
        .zip(record_info.per_result_range.iter())
        .map(|(&enum_infos, &record_infos)| FieldSideTables {
            enum_infos,
            record_infos,
        })
        .collect();
    // Hand the per-(fn, param) per-cell record-info indices to each
    // ParamLift, so the wrapper-body emitter can read them inline.
    for (fd, fn_indices) in per_func.iter_mut().zip(record_info.per_param_cell_idx) {
        for (p, p_indices) in fd.params.iter_mut().zip(fn_indices) {
            p.record_info_cell_idx = p_indices;
        }
    }

    // Fields blob (data) — pre-filled with cells.ptr pointing at
    // each param's reserved slab slot, plus per-kind side-table
    // pointers patched per-param.
    let fields_blob = build_fields_blob(per_func, schema, &param_side_tables);
    let fields_base = layout.place_data(schema.field_layout.align, &fields_blob);
    let mut cursor = fields_base;
    for fd in per_func.iter_mut() {
        fd.fields_buf_offset = cursor;
        cursor += fd.params.len() as u32 * schema.field_layout.size;
    }

    // On-return params blob (data), only when after-hook is wired.
    // First seed `fd.after.result_cells_offset` so the blob builder
    // sees the per-fn result-cells scratch; the params_offset is
    // back-filled below once we know the placed base.
    if schema.after_hook.is_some() {
        for (fd, &result_cells_offset) in per_func.iter_mut().zip(result_cells_offsets.iter()) {
            fd.after = Some(AfterSetup {
                params_offset: 0, // back-filled below
                result_cells_offset,
            });
        }
    }
    let after_blob = build_after_params_blob(per_func, schema, iface_name, &result_side_tables);
    if let Some(al) = schema.on_return_params_layout.as_ref() {
        let after_base = layout.place_data(al.align, &after_blob);
        let mut cursor = after_base;
        for fd in per_func.iter_mut() {
            fd.after
                .as_mut()
                .expect("has_after → fd.after seeded")
                .params_offset = cursor as i32;
            cursor += al.size;
        }
    }

    // Scratch slots: event record + on-call indirect-params buffer.
    let event_ptr = layout.reserve_scratch(EVENT_SLOT_ALIGN, EVENT_SLOT_SIZE) as i32;
    let hook_params_ptr = match schema.on_call_params_layout.as_ref() {
        Some(l) => layout.reserve_scratch(l.align, l.size),
        None => 0,
    };

    // Per-fn retptr scratch — only for funcs whose canonical-ABI
    // shape uses one. Back-fills RetptrPair so the wrapper body knows
    // the load address.
    for (fd, func) in per_func.iter_mut().zip(funcs.iter()) {
        if !(fd.export_sig.retptr || fd.import_sig.retptr) {
            continue;
        }
        let result_ty = func
            .result
            .as_ref()
            .expect("retptr → func.result is_some()");
        let size = schema.size_align.size(result_ty).size_wasm32() as u32;
        let align = schema.size_align.align(result_ty).align_wasm32() as u32;
        let off = layout.reserve_scratch(align, size) as i32;
        fd.retptr_offset = Some(off);
        if let Some(rl) = &mut fd.result_lift {
            rl.set_retptr_offset(off);
        }
    }

    // Align the bump-allocator start past the largest alignment we
    // placed; today that's `cell` (8) but pulling from `cell_layout`
    // keeps it tied to the schema instead of a literal.
    let bump_start = layout.end().next_multiple_of(schema.cell_layout.align);
    let data_segments = layout.into_segments();
    StaticDataPlan {
        bump_start,
        event_ptr,
        hook_params_ptr,
        data_segments,
    }
}

/// Build the per-target-function dispatch records: classify each
/// param, populate the WIT-derived sigs and mangled names, classify
/// the result for on-return lift. Buffer offsets are left as
/// placeholders; the static-memory layout phase fills them in.
/// Appends fn names + param names to `name_blob` as it goes.
fn build_per_func_dispatches(
    resolve: &Resolve,
    target_iface: InterfaceId,
    funcs: &[&WitFunction],
    name_blob: &mut Vec<u8>,
) -> Result<Vec<FuncDispatch>> {
    let target_world_key = WorldKey::Interface(target_iface);
    let mut per_func: Vec<FuncDispatch> = Vec::with_capacity(funcs.len());

    for func in funcs {
        let fn_name_offset = name_blob.len() as i32;
        let fn_name_len = func.name.len() as i32;
        name_blob.extend_from_slice(func.name.as_bytes());

        let params_lift = classify_func_params(resolve, func, name_blob);
        let is_async = func.kind.is_async();
        let (import_variant, export_variant) = if is_async {
            (
                AbiVariant::GuestImportAsync,
                AbiVariant::GuestExportAsyncStackful,
            )
        } else {
            (AbiVariant::GuestImport, AbiVariant::GuestExport)
        };
        let mangling = dispatch_mangling(is_async);

        let (import_module, import_field) = resolve.wasm_import_name(
            mangling,
            WasmImport::Func {
                interface: Some(&target_world_key),
                func,
            },
        );
        let export_name = resolve.wasm_export_name(
            mangling,
            WasmExport::Func {
                interface: Some(&target_world_key),
                func,
                kind: WasmExportKind::Normal,
            },
        );
        let export_sig = resolve.wasm_signature(export_variant, func);
        let import_sig = resolve.wasm_signature(import_variant, func);
        // Async exports never need cabi_post (results land via
        // task.return, not a callee-allocated buffer).
        let needs_cabi_post = !is_async && export_sig.retptr;
        let result_lift = classify_result_lift(resolve, func, &export_sig, &import_sig, is_async);
        let shape = if is_async {
            let (module, name, sig) =
                func.task_return_import(resolve, Some(&target_world_key), Mangling::Legacy);
            FuncShape::Async(TaskReturnImport { module, name, sig })
        } else {
            FuncShape::Sync
        };

        per_func.push(FuncDispatch {
            shape,
            result_ty: func.result,
            import_module,
            import_field,
            export_name,
            export_sig,
            import_sig,
            needs_cabi_post,
            fn_name_offset,
            fn_name_len,
            params: params_lift,
            fields_buf_offset: 0,
            retptr_offset: None,
            result_lift,
            after: None,
        });
    }
    Ok(per_func)
}

fn compute_schema(
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
    let call_id_ty = find_common_typeid(resolve, TYPEDEF_CALL_ID)?;
    let enum_info_ty = find_common_typeid(resolve, TYPEDEF_ENUM_INFO)?;
    let record_info_ty = find_common_typeid(resolve, TYPEDEF_RECORD_INFO)?;

    let field_layout = RecordLayout::for_record_typedef(&size_align, resolve, field_ty_id);
    let tree_layout = RecordLayout::for_record_typedef(&size_align, resolve, field_tree_ty_id);
    let cell_layout = CellLayout::from_resolve(&size_align, resolve, cell_ty_id);
    let callid_layout = RecordLayout::for_record_typedef(&size_align, resolve, call_id_ty);
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

fn build_dispatch_module(
    resolve: &Resolve,
    world_id: WorldId,
    target_iface: InterfaceId,
    target_interface_name: &str,
    has_before: bool,
    has_after: bool,
) -> Result<Vec<u8>> {
    let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
        .functions
        .values()
        .collect();
    let schema = compute_schema(resolve, world_id, has_before, has_after)?;

    let iface_name = BlobSlice {
        off: 0,
        len: target_interface_name.len() as u32,
    };
    let mut name_blob: Vec<u8> = target_interface_name.as_bytes().to_vec();
    let mut per_func = build_per_func_dispatches(resolve, target_iface, &funcs, &mut name_blob)?;

    let plan = lay_out_static_memory(&mut per_func, &funcs, &schema, &mut name_blob, iface_name);

    let mut module = Module::new();
    let type_idx = emit_type_section(
        &mut module,
        &per_func,
        schema.before_hook.as_ref().map(|h| &h.sig),
        schema.after_hook.as_ref().map(|h| &h.sig),
    );
    let func_idx = emit_imports_and_funcs(
        &mut module,
        &per_func,
        &type_idx,
        schema.before_hook.as_ref(),
        schema.after_hook.as_ref(),
        plan.event_ptr,
    );
    emit_memory_and_globals(&mut module, plan.bump_start);
    emit_export_section(
        &mut module,
        &per_func,
        func_idx.wrapper_base,
        func_idx.init_idx,
        func_idx.cabi_realloc_idx,
    );
    let wrapper_ctx = WrapperCtx {
        schema: &schema,
        resolve,
        iface_name,
        hook_params_ptr: plan.hook_params_ptr as i32,
    };
    emit_code_section(&mut module, &per_func, &func_idx, &wrapper_ctx);
    emit_data_section(&mut module, &plan.data_segments);
    Ok(module.finish())
}

/// Locate the `splicer:common/types`.field typedef in the resolved
/// `Resolve`. Returns the typedef id ready for `SizeAlign::size`.
/// Look up a typedef in `splicer:common/types` by name (e.g.
/// `"field"`, `"field-tree"`, `"call-id"`). The tier WITs and the
/// common WIT travel together in this repo, so whichever version
/// got loaded is the canonical one — version is ignored.
fn find_common_typeid(resolve: &Resolve, type_name: &str) -> Result<TypeId> {
    for (id, _) in resolve.interfaces.iter() {
        let qname = match resolve.id_of(id) {
            Some(s) => s,
            None => continue,
        };
        let unversioned = qname.split('@').next().unwrap_or(&qname);
        if unversioned == "splicer:common/types" {
            return resolve.interfaces[id]
                .types
                .get(type_name)
                .copied()
                .ok_or_else(|| anyhow!("`splicer:common/types` is missing typedef `{type_name}`"));
        }
    }
    bail!("resolve has no `splicer:common/types` interface — was the common WIT loaded?")
}

/// Resolved on-call hook info — module + name + signature, all
/// sourced from `Resolve` so wit-component agrees on the canonical
/// names.
struct HookImport {
    module: String,
    name: String,
    sig: WasmSignature,
    /// `(name, type)` per WIT param. Used to derive the
    /// indirect-params buffer's [`RecordLayout`] from the schema.
    params: Vec<(String, Type)>,
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

// ─── Section emission ─────────────────────────────────────────────

struct TypeIndices {
    handler_ty: Vec<u32>,
    wrapper_ty: Vec<u32>,
    before_hook_ty: Option<u32>,
    after_hook_ty: Option<u32>,
    init_ty: u32,
    cabi_post_ty: u32,
    cabi_realloc_ty: u32,
    async_types: canon_async::AsyncTypes,
    /// Per-async-func `task.return` type; `Some(idx)` iff `is_async`.
    task_return_ty: Vec<Option<u32>>,
}

fn emit_type_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
    before_hook_sig: Option<&WasmSignature>,
    after_hook_sig: Option<&WasmSignature>,
) -> TypeIndices {
    let mut types = TypeSection::new();
    let mut next_ty: u32 = 0;
    let mut alloc_one =
        |ty_section: &mut TypeSection, params: Vec<ValType>, results: Vec<ValType>| -> u32 {
            ty_section.ty().function(params, results);
            let idx = next_ty;
            next_ty += 1;
            idx
        };

    let handler_ty: Vec<u32> = per_func
        .iter()
        .map(|fd| {
            alloc_one(
                &mut types,
                val_types(&fd.import_sig.params),
                val_types(&fd.import_sig.results),
            )
        })
        .collect();
    let wrapper_ty: Vec<u32> = per_func
        .iter()
        .map(|fd| {
            alloc_one(
                &mut types,
                val_types(&fd.export_sig.params),
                val_types(&fd.export_sig.results),
            )
        })
        .collect();

    let before_hook_ty = before_hook_sig
        .map(|sig| alloc_one(&mut types, val_types(&sig.params), val_types(&sig.results)));
    let after_hook_ty = after_hook_sig
        .map(|sig| alloc_one(&mut types, val_types(&sig.params), val_types(&sig.results)));
    let init_ty = alloc_one(&mut types, vec![], vec![]);
    let cabi_post_ty = alloc_one(&mut types, vec![ValType::I32], vec![]);
    let cabi_realloc_ty = alloc_one(
        &mut types,
        vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        vec![ValType::I32],
    );

    // Per-async-fn `task.return` types. Allocated BEFORE the canon-
    // async runtime types so `alloc_one` (which captures `next_ty`)
    // is still the sole borrower; the runtime-types closure also
    // captures `next_ty` and the borrow checker rejects two
    // simultaneous mutable captures.
    let task_return_ty: Vec<Option<u32>> = per_func
        .iter()
        .map(|fd| {
            fd.shape.task_return().map(|tr| {
                alloc_one(
                    &mut types,
                    val_types(&tr.sig.params),
                    val_types(&tr.sig.results),
                )
            })
        })
        .collect();

    let async_types = canon_async::emit_types(&mut types, || {
        let i = next_ty;
        next_ty += 1;
        i
    });

    module.section(&types);
    TypeIndices {
        handler_ty,
        wrapper_ty,
        before_hook_ty,
        after_hook_ty,
        init_ty,
        cabi_post_ty,
        cabi_realloc_ty,
        async_types,
        task_return_ty,
    }
}

struct FuncIndices {
    handler_imp_base: u32,
    before_hook_idx: Option<u32>,
    after_hook_idx: Option<u32>,
    async_funcs: canon_async::AsyncFuncs,
    /// Per-async-func `task.return` import index.
    task_return_idx: Vec<Option<u32>>,
    wrapper_base: u32,
    init_idx: u32,
    cabi_realloc_idx: u32,
}

fn emit_imports_and_funcs(
    module: &mut Module,
    per_func: &[FuncDispatch],
    ty: &TypeIndices,
    before_hook: Option<&HookImport>,
    after_hook: Option<&HookImport>,
    event_ptr: i32,
) -> FuncIndices {
    let mut imports = ImportSection::new();
    let mut next_imp: u32 = 0;

    let handler_imp_base = next_imp;
    for (fd, &fty) in per_func.iter().zip(&ty.handler_ty) {
        imports.import(
            &fd.import_module,
            &fd.import_field,
            EntityType::Function(fty),
        );
        next_imp += 1;
    }

    let before_hook_idx = before_hook.map(|h| {
        imports.import(
            &h.module,
            &h.name,
            EntityType::Function(ty.before_hook_ty.unwrap()),
        );
        let idx = next_imp;
        next_imp += 1;
        idx
    });
    let after_hook_idx = after_hook.map(|h| {
        imports.import(
            &h.module,
            &h.name,
            EntityType::Function(ty.after_hook_ty.unwrap()),
        );
        let idx = next_imp;
        next_imp += 1;
        idx
    });

    let async_funcs =
        canon_async::import_intrinsics(&mut imports, &ty.async_types, event_ptr, || {
            let i = next_imp;
            next_imp += 1;
            i
        });

    // Per-async-fn `task.return` imports. Mirrors tier-1's order:
    // imports come AFTER the canon-async runtime intrinsics. `Some`
    // iff the func is async.
    let mut task_return_idx: Vec<Option<u32>> = vec![None; per_func.len()];
    for (i, fd) in per_func.iter().enumerate() {
        if let Some(tr) = fd.shape.task_return() {
            let ty_idx = ty
                .task_return_ty
                .get(i)
                .copied()
                .flatten()
                .expect("async func must have task.return type allocated");
            imports.import(&tr.module, &tr.name, EntityType::Function(ty_idx));
            task_return_idx[i] = Some(next_imp);
            next_imp += 1;
        }
    }

    module.section(&imports);

    let wrapper_base = next_imp;

    let mut fsec = FunctionSection::new();
    for &fty in &ty.wrapper_ty {
        fsec.function(fty);
    }
    fsec.function(ty.init_ty);
    let init_idx = wrapper_base + per_func.len() as u32;
    let mut cabi_post_first_idx = init_idx + 1;
    for fd in per_func {
        if fd.needs_cabi_post {
            fsec.function(ty.cabi_post_ty);
            cabi_post_first_idx += 1;
        }
    }
    fsec.function(ty.cabi_realloc_ty);
    let cabi_realloc_idx = cabi_post_first_idx;
    module.section(&fsec);

    FuncIndices {
        handler_imp_base,
        before_hook_idx,
        after_hook_idx,
        async_funcs,
        task_return_idx,
        wrapper_base,
        init_idx,
        cabi_realloc_idx,
    }
}

fn emit_export_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
    wrapper_base: u32,
    init_idx: u32,
    cabi_realloc_idx: u32,
) {
    let mut exports = ExportSection::new();
    let mut next_post_idx = init_idx + 1;
    for (i, fd) in per_func.iter().enumerate() {
        exports.export(&fd.export_name, ExportKind::Func, wrapper_base + i as u32);
        if fd.needs_cabi_post {
            let post_name = format!("cabi_post_{}", fd.export_name);
            exports.export(&post_name, ExportKind::Func, next_post_idx);
            next_post_idx += 1;
        }
    }
    exports.export(EXPORT_MEMORY, ExportKind::Memory, 0);
    exports.export(EXPORT_CABI_REALLOC, ExportKind::Func, cabi_realloc_idx);
    exports.export(EXPORT_INITIALIZE, ExportKind::Func, init_idx);
    module.section(&exports);
}

/// Wrapper body shape:
///
/// ```text
/// ;; build call-id flat: (iface_ptr, iface_len, fn_ptr, fn_len)
/// i32.const iface_offset
/// i32.const iface_len
/// i32.const fn_offset
/// i32.const fn_len
/// ;; empty list<field> args (ptr=0, len=0)
/// i32.const 0
/// i32.const 0
/// call $on_call               ;; canon-lower-async — returns packed (handle<<4)|status
/// local.set $st
/// ;; wait loop (only if subtask didn't return synchronously)
/// local.get $st
/// i32.const 4
/// i32.shr_u
/// local.set $st               ;; raw subtask handle now
/// local.get $st
/// if
///     call $waitable_set_new
///     local.set $ws
///     local.get $st
///     local.get $ws
///     call $waitable_join
///     local.get $ws
///     i32.const event_ptr
///     call $waitable_set_wait
///     drop                     ;; event code (we don't inspect)
///     local.get $st
///     call $subtask_drop
///     local.get $ws
///     call $waitable_set_drop
/// end
/// ;; pass-through to handler
/// local.get $param_0 ; ... ; local.get $param_N
/// call $handler
/// ```
/// Static context the wrapper-body emitter needs to read per-call
/// from the layout phase. Bundles the schema + memory-layout
/// addresses so the body emitter doesn't take a half-dozen positional args.
struct WrapperCtx<'a> {
    schema: &'a SchemaLayouts,
    resolve: &'a Resolve,
    iface_name: BlobSlice,
    hook_params_ptr: i32,
}

fn emit_code_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
    func_idx: &FuncIndices,
    ctx: &WrapperCtx<'_>,
) {
    let mut code = CodeSection::new();
    for (i, fd) in per_func.iter().enumerate() {
        emit_wrapper_function(&mut code, func_idx, ctx, i, fd);
    }
    code.function(&empty_function());
    for fd in per_func {
        if fd.needs_cabi_post {
            code.function(&empty_function());
        }
    }
    emit_cabi_realloc(&mut code);
    module.section(&code);
}

fn emit_wrapper_function(
    code: &mut CodeSection,
    func_idx: &FuncIndices,
    ctx: &WrapperCtx<'_>,
    i: usize,
    fd: &FuncDispatch,
) {
    let async_funcs = &func_idx.async_funcs;
    let schema = ctx.schema;
    let nparams = fd.export_sig.params.len() as u32;
    let mut locals = FunctionIndices::new(nparams);
    let lcl = alloc_wrapper_locals(&mut locals, fd);

    // For async funcs whose `task.return` takes flat-form params,
    // pre-build the load sequence — `lift_from_memory` may allocate
    // additional bindgen scratch locals, which must happen before
    // the locals list is frozen below.
    let task_return_loads: Option<Vec<wasm_encoder::Instruction<'static>>> =
        lcl.tr_addr.map(|addr_local| {
            let result_ty = fd.result_ty.as_ref().expect("flat loads → result_ty");
            let mut bindgen = WasmEncoderBindgen::new(&schema.size_align, addr_local, &mut locals);
            lift_from_memory(ctx.resolve, &mut bindgen, (), result_ty);
            bindgen.into_instructions()
        });

    let mut f = Function::new_with_locals_types(locals.into_locals());

    // ── Phase 1: on-call (only if before-hook wired) ──
    if let Some(before_idx) = func_idx.before_hook_idx {
        for p in fd.params.iter() {
            emit_lift_param(
                &mut f,
                &schema.cell_layout,
                p.cells_offset,
                &p.plan,
                &p.record_info_cell_idx,
                &lcl,
            );
        }
        let nargs = fd.params.len() as u32;
        let args_off = if nargs == 0 { 0 } else { fd.fields_buf_offset };
        emit_populate_hook_params(
            &mut f,
            ctx.hook_params_ptr,
            schema,
            &OnCallCallSite {
                iface_name: ctx.iface_name,
                fn_name: BlobSlice {
                    off: fd.fn_name_offset as u32,
                    len: fd.fn_name_len as u32,
                },
                args: BlobSlice {
                    off: args_off,
                    len: nargs,
                },
            },
        );
        f.instructions().i32_const(ctx.hook_params_ptr);
        canon_async::emit_call_and_wait(&mut f, before_idx, lcl.st, lcl.ws, async_funcs);
    }

    // ── Phase 2: forward to handler. Bridges callee-returns ↔
    // caller-allocates for compound results via the shared
    // abi/emit helpers. For async, the import returns a packed
    // canon-lower-async status that we wait on.
    emit_handler_call(
        &mut f,
        nparams,
        fd.import_sig.retptr,
        fd.retptr_offset,
        func_idx.handler_imp_base + i as u32,
    );
    if fd.shape.is_async() {
        f.instructions().local_set(lcl.st);
        canon_async::emit_wait_loop(&mut f, lcl.st, lcl.ws, async_funcs);
    } else if let Some(local) = lcl.result {
        f.instructions().local_set(local);
    }

    // ── Phase 3: on-return (only if after-hook wired) ──
    if let (Some(after_idx), Some(after)) = (func_idx.after_hook_idx, fd.after.as_ref()) {
        if let (Some(rl), Some(cells_off)) = (fd.result_lift.as_ref(), after.result_cells_offset) {
            f.instructions().i32_const(cells_off as i32);
            f.instructions().local_set(lcl.addr);
            emit_lift_result(&mut f, &schema.cell_layout, rl.source, &lcl);
        }
        f.instructions().i32_const(after.params_offset);
        canon_async::emit_call_and_wait(&mut f, after_idx, lcl.st, lcl.ws, async_funcs);
    }

    // ── Phase 4: tail. Async fns publish the result via task.return;
    // sync fns return the direct value (or static retptr).
    match &fd.shape {
        FuncShape::Async(_) => {
            emit_task_return(&mut f, fd, func_idx, i, &lcl, task_return_loads.as_deref());
        }
        FuncShape::Sync => {
            emit_wrapper_return(&mut f, lcl.result, fd.export_sig.retptr, fd.retptr_offset);
        }
    }
    f.instructions().end();
    code.function(&f);
}

/// Emit the async tail: call `task.return` with the appropriate
/// args. Three shapes:
/// - void result → no args.
/// - `tr_sig.indirect_params` (compound result) → push retptr scratch.
/// - flat result → load each value from retptr via the pre-built
///   `lift_from_memory` instruction sequence.
fn emit_task_return(
    f: &mut Function,
    fd: &FuncDispatch,
    func_idx: &FuncIndices,
    i: usize,
    lcl: &WrapperLocals,
    task_return_loads: Option<&[wasm_encoder::Instruction<'static>]>,
) {
    let imp_task_return =
        func_idx.task_return_idx[i].expect("async func must have task.return import");
    let FuncShape::Async(tr) = &fd.shape else {
        unreachable!("emit_task_return called only for async funcs")
    };
    if fd.result_ty.is_none() {
        f.instructions().call(imp_task_return);
    } else if tr.sig.indirect_params {
        f.instructions().i32_const(
            fd.retptr_offset
                .expect("async non-void result → retptr_offset"),
        );
        f.instructions().call(imp_task_return);
    } else {
        let addr_local = lcl.tr_addr.expect("flat loads → tr_addr local");
        f.instructions().i32_const(
            fd.retptr_offset
                .expect("async non-void result → retptr_offset"),
        );
        f.instructions().local_set(addr_local);
        for inst in task_return_loads.expect("flat loads → instruction sequence") {
            f.instruction(inst);
        }
        f.instructions().call(imp_task_return);
    }
}

fn emit_data_section(module: &mut Module, segments: &[(u32, Vec<u8>)]) {
    if segments.is_empty() {
        return;
    }
    let mut data = DataSection::new();
    for (offset, bytes) in segments {
        data.active(
            0,
            &ConstExpr::i32_const(*offset as i32),
            bytes.iter().copied(),
        );
    }
    module.section(&data);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tier-2 adapter for a target with a single sync
    /// primitive function (`add(x: u32, y: u32) -> u32`-style), run
    /// it through ComponentEncoder, and validate the resulting
    /// component bytes round-trip through wasmparser. Confirms the
    /// dispatch module — including the on-call invocation + wait
    /// loop — produces structurally valid wasm.
    #[test]
    fn dispatch_module_roundtrips_through_component_encoder() {
        let wat = r#"(component
            (component $inner
                (core module $m
                    (func (export "add") (param i32 i32) (result i32)
                        local.get 0
                        local.get 1
                        i32.add
                    )
                )
                (core instance $i (instantiate $m))
                (alias core export $i "add" (core func $add))
                (type $add-ty (func (param "x" u32) (param "y" u32) (result u32)))
                (func $add-lifted (type $add-ty) (canon lift (core func $add)))
                (instance $api-inst (export "add" (func $add-lifted)))
                (export "my:math/api@1.0.0" (instance $api-inst))
            )
            (instance $api (instantiate $inner))
            (export "my:math/api@1.0.0" (instance $api "my:math/api@1.0.0"))
        )"#;
        let split_bytes = wat::parse_str(wat).expect("WAT must parse");

        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");

        let bytes = build_tier2_adapter(
            "my:math/api@1.0.0",
            true,
            true,
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect("tier-2 adapter generation should succeed");

        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("emitted tier-2 adapter component should validate");
    }
}
