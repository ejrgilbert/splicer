//! Tier-2 adapter generator: builds an adapter component that lifts
//! a target function's canonical-ABI parameters into the cell-array
//! representation and forwards them to the middleware's tier-2
//! `on-call` hook before dispatching to the handler.
//!
//! Status (Phase 2-3, slice 2): on-call invocation is wired in. For
//! each target function the wrapper builds the `call-id` strings,
//! calls `on-call(call_id, empty-list)` via canon-lower-async,
//! awaits via the canon-async runtime intrinsics, then forwards the
//! original args to the handler.
//!
//! `args` is currently always an empty `list<field>` — the
//! cell-construction path that fills it lives in
//! [`super::cells`] but isn't wired into the orchestrator yet (next
//! slice). End-to-end, this means a tier-2 middleware's `on-call`
//! fires with the right call identity but observes no payload data.
//!
//! Async target functions, `on-return`, and `on-trap` remain
//! out-of-scope for this slice.
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
    direct_return_type, emit_cabi_realloc, emit_handler_call, emit_memory_and_globals,
    emit_wrapper_return, empty_function, option_payload_offset, val_types, RecordLayout,
    EXPORT_CABI_REALLOC, EXPORT_INITIALIZE, EXPORT_MEMORY, SLICE_LEN_OFFSET, SLICE_PTR_OFFSET,
};
use super::super::abi::WasmEncoderBindgen;
use super::super::indices::FunctionIndices;
use super::super::mem_layout::StaticLayout;
use super::super::resolve::{
    decode_input_resolve, dispatch_mangling, find_target_interface, hook_callback_mangling,
};
use super::cells::CellLayout;

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
    let mut wit = format!(
        "package {TIER2_ADAPTER_WORLD_PACKAGE};\n\nworld {TIER2_ADAPTER_WORLD_NAME} {{\n"
    );
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

/// Per-function dispatch shape. The on-call hook is called with the
/// `(iface, fn)` strings shared from the same name blob — the
/// interface name is allocated once and reused; each function's name
/// gets its own slot.
/// `task.return` import for one async target function. The wrapper
/// body calls this at the end of an async dispatch to publish the
/// result.
struct TaskReturnImport {
    module: String,
    name: String,
    sig: WasmSignature,
}

struct FuncDispatch {
    is_async: bool,
    /// `task.return` import; `Some` iff `is_async`.
    task_return: Option<TaskReturnImport>,
    /// WIT result type, kept around so async wrappers can drive
    /// `lift_from_memory` to flat-load the result for `task.return`.
    result_ty: Option<Type>,
    import_module: String,
    import_field: String,
    export_name: String,
    /// Wrapper export sig (`AbiVariant::GuestExport`) — the shape
    /// `wit-component`'s validator expects for our exported wrapper.
    export_sig: WasmSignature,
    /// Handler import sig (`AbiVariant::GuestImport`) — the shape
    /// `wit-component`'s validator expects for our import declaration.
    /// May differ from `export_sig` for compound-result functions
    /// (caller-allocates retptr on the import side vs. callee-returns
    /// pointer on the export side).
    import_sig: WasmSignature,
    needs_cabi_post: bool,
    /// Byte offset of the function name within the data segment.
    fn_name_offset: i32,
    fn_name_len: i32,
    /// Per-param lift recipe + wasm flat-slot indexing. Empty for
    /// zero-arg functions.
    params: Vec<ParamLift>,
    /// Byte offset of this function's cells scratch slab. The slab
    /// holds `params.len()` consecutive cells, each [`CELL_SIZE`]
    /// bytes. Cleared (zeroed by the runtime) at module instantiation;
    /// rewritten per-call in the wrapper body.
    cells_buf_offset: u32,
    /// Byte offset of this function's pre-built `field` records in
    /// the data segment. Holds `params.len()` consecutive `field`
    /// records, each [`FIELD_SIZE_BYTES`] bytes. Pointed at by the
    /// `args.list.ptr` field passed to `on-call`.
    fields_buf_offset: u32,
    /// Byte offset of the retptr scratch buffer; `Some` iff the
    /// import sig wants a caller-allocates retptr but the export sig
    /// returns the pointer directly. The wrapper passes this as the
    /// extra trailing arg when calling the import, then loads from it
    /// to produce its own return value.
    retptr_offset: Option<i32>,
    /// How to lift the function's return value into a `cell` at
    /// `result_cells_buf_offset`. `None` for void or compound returns
    /// we don't yet lift (Phase 2-2b territory).
    result_lift: Option<ResultLift>,
    /// Byte offset of the 1-cell result scratch slab. `Some` iff
    /// `result_lift` is `Some` — the wrapper writes the lifted result
    /// here per-call before invoking on-return.
    result_cells_buf_offset: Option<u32>,
    /// Byte offset of the pre-built on-return indirect-params buffer.
    /// `Some` iff the middleware exported `splicer:tier2/after` and
    /// this function has a wired on-return hook.
    after_params_offset: Option<i32>,
}

/// How to extract the function's return value when lifting it for
/// on-return.
#[derive(Clone, Copy)]
enum ResultLift {
    /// Direct primitive (no retptr): source is the captured
    /// result_local — emit_code_section resolves the actual local idx.
    Direct(LiftKind),
    /// `(ptr, len)` pair in retptr scratch (string / `list<u8>`).
    RetptrPair { kind: LiftKind, retptr_offset: i32 },
}

/// Per-parameter lift recipe. `first_local` is the wasm local index
/// of the first flat slot for this param (subsequent slots for
/// multi-slot params live at +1, +2, ...). `name_offset` /
/// `name_len` reference the param name in the shared name blob.
struct ParamLift {
    name_offset: i32,
    name_len: i32,
    kind: LiftKind,
    first_local: u32,
}

/// How to widen a param's flat-form value(s) into the matching
/// `cell` variant payload. One slot per kind unless noted.
#[derive(Clone, Copy)]
enum LiftKind {
    /// `bool` — 1 i32 slot (0/1) → `cell::bool`.
    Bool,
    /// `s8`/`s16`/`s32` — 1 i32 slot, sign-extend → `cell::integer`.
    IntegerSignExt,
    /// `u8`/`u16`/`u32` — 1 i32 slot, zero-extend → `cell::integer`.
    IntegerZeroExt,
    /// `s64`/`u64` — 1 i64 slot, no widen → `cell::integer`.
    Integer64,
    /// `f32` — 1 f32 slot, `f64.promote_f32` → `cell::floating`.
    FloatingF32,
    /// `f64` — 1 f64 slot, no widen → `cell::floating`.
    FloatingF64,
    /// `string` — 2 i32 slots (ptr, len) → `cell::text`.
    Text,
    /// `list<u8>` — 2 i32 slots (ptr, len) → `cell::bytes`.
    Bytes,
}

impl LiftKind {
    /// Number of flat wasm slots this param consumes.
    fn slot_count(self) -> u32 {
        match self {
            LiftKind::Text | LiftKind::Bytes => 2,
            _ => 1,
        }
    }

    /// Classify a WIT param type. Returns `None` for types not yet
    /// supported by Phase 2-2a primitives (compound types, char,
    /// resources, etc.) — caller bails with a clear error.
    fn classify(ty: &Type, resolve: &Resolve) -> Option<LiftKind> {
        match ty {
            Type::Bool => Some(LiftKind::Bool),
            Type::S8 | Type::S16 | Type::S32 => Some(LiftKind::IntegerSignExt),
            Type::U8 | Type::U16 | Type::U32 => Some(LiftKind::IntegerZeroExt),
            Type::S64 | Type::U64 => Some(LiftKind::Integer64),
            Type::F32 => Some(LiftKind::FloatingF32),
            Type::F64 => Some(LiftKind::FloatingF64),
            Type::String => Some(LiftKind::Text),
            // `list<u8>` fast-path: peek through TypeDefKind::List for u8.
            Type::Id(id) => match &resolve.types[*id].kind {
                wit_parser::TypeDefKind::List(elem) if matches!(elem, Type::U8) => {
                    Some(LiftKind::Bytes)
                }
                _ => None,
            },
            // Char (utf-8 encoding required at lift time) and
            // ErrorContext are deferred to follow-up slices.
            Type::Char | Type::ErrorContext => None,
        }
    }
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

// Field names within those records.
const FIELD_NAME: &str = "name";
const FIELD_TREE: &str = "tree";
const TREE_CELLS: &str = "cells";
const TREE_ROOT: &str = "root";
const CALLID_IFACE: &str = "interface-name";
const CALLID_FN: &str = "function-name";

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

/// Variant disc values for `option<T>` — canonical-ABI invariants.
const OPTION_NONE: u8 = 0;
const OPTION_SOME: u8 = 1;

/// Sub-offsets within the on-call indirect-params buffer, derived
/// from the canonical-ABI layouts of the on-call func-params record
/// + nested `call-id` record. Built once per dispatch module so the
/// hook-params populator stays schema-driven.
struct OnCallParamOffsets {
    iface_ptr: u32,
    iface_len: u32,
    fn_ptr: u32,
    fn_len: u32,
    args_ptr: u32,
    args_len: u32,
}

/// Emit wasm that writes the call-id (interface + function name
/// pointers/lengths) and the per-call `list<field>` args pointer/
/// length into the indirect-params buffer at `base_ptr`.
fn emit_populate_hook_params(
    f: &mut Function,
    base_ptr: i32,
    offs: &OnCallParamOffsets,
    iface_name_offset: i32,
    iface_name_len: i32,
    fn_name_offset: i32,
    fn_name_len: i32,
    args_ptr: i32,
    args_len: i32,
) {
    let mut store_i32 = |field_offset: u64, value: i32| {
        f.instructions().i32_const(base_ptr);
        f.instructions().i32_const(value);
        f.instructions().i32_store(MemArg {
            offset: field_offset,
            align: 2,
            memory_index: 0,
        });
    };
    store_i32(offs.iface_ptr as u64, iface_name_offset);
    store_i32(offs.iface_len as u64, iface_name_len);
    store_i32(offs.fn_ptr as u64, fn_name_offset);
    store_i32(offs.fn_len as u64, fn_name_len);
    store_i32(offs.args_ptr as u64, args_ptr);
    store_i32(offs.args_len as u64, args_len);
}

/// Schema-driven layouts + hook descriptors gathered up front so
/// later phases see one bundle instead of a dozen locals.
struct SchemaLayouts {
    size_align: SizeAlign,
    field_layout: RecordLayout,
    tree_layout: RecordLayout,
    cell_layout: CellLayout,
    callid_layout: RecordLayout,
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
    on_call_offs: Option<OnCallParamOffsets>,
    data_segments: Vec<(u32, Vec<u8>)>,
}

/// Single-pass build of a `field` record + its embedded
/// `field-tree` for one (function, param) pair, with cells.ptr
/// pointing at the param's pre-allocated cell.
fn write_field_record(
    blob: &mut Vec<u8>,
    schema: &SchemaLayouts,
    cell_addr: i32,
    name_offset: i32,
    name_len: i32,
) {
    let field_start = blob.len();
    blob.extend(std::iter::repeat(0u8).take(schema.field_layout.size as usize));
    let name_base = field_start + schema.field_layout.offset_of(FIELD_NAME) as usize;
    let tree_base = field_start + schema.field_layout.offset_of(FIELD_TREE) as usize;
    let cells_base = tree_base + schema.tree_layout.offset_of(TREE_CELLS) as usize;
    let root_base = tree_base + schema.tree_layout.offset_of(TREE_ROOT) as usize;
    write_le_i32(blob, name_base + SLICE_PTR_OFFSET as usize, name_offset);
    write_le_i32(blob, name_base + SLICE_LEN_OFFSET as usize, name_len);
    write_le_i32(blob, cells_base + SLICE_PTR_OFFSET as usize, cell_addr);
    write_le_i32(blob, cells_base + SLICE_LEN_OFFSET as usize, 1);
    // Side-table list pairs and `root` stay zero — empty side-tables,
    // root cell at index 0.
    write_le_i32(blob, root_base, 0);
}

/// Build the contiguous fields blob: one `field` record per (fn, param).
fn build_fields_blob(per_func: &[FuncDispatch], schema: &SchemaLayouts) -> Vec<u8> {
    let mut blob: Vec<u8> = Vec::new();
    for fd in per_func {
        for (i, p) in fd.params.iter().enumerate() {
            let cell_addr =
                (fd.cells_buf_offset + i as u32 * schema.cell_layout.size) as i32;
            write_field_record(&mut blob, schema, cell_addr, p.name_offset, p.name_len);
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
    iface_name_offset: i32,
    iface_name_len: i32,
) -> Vec<u8> {
    let Some(after_layout) = schema.on_return_params_layout.as_ref() else {
        return Vec::new();
    };
    let call_off = after_layout.offset_of(ON_RET_CALL);
    let result_off = after_layout.offset_of(ON_RET_RESULT);
    let iface_off = call_off + schema.callid_layout.offset_of(CALLID_IFACE);
    let fn_off = call_off + schema.callid_layout.offset_of(CALLID_FN);
    // result: option<field-tree>. Disc at result_off; tree payload
    // at result_off + option_payload_off.
    let result_disc_off = result_off as usize;
    let tree_base = result_off + schema.option_payload_off;
    let tree_cells_base =
        (tree_base + schema.tree_layout.offset_of(TREE_CELLS)) as usize;
    let tree_root_off_in_after =
        (tree_base + schema.tree_layout.offset_of(TREE_ROOT)) as usize;

    let mut blob: Vec<u8> = Vec::new();
    for fd in per_func {
        let entry_start = blob.len();
        blob.extend(std::iter::repeat(0u8).take(after_layout.size as usize));
        write_le_i32(
            &mut blob,
            entry_start + iface_off as usize + SLICE_PTR_OFFSET as usize,
            iface_name_offset,
        );
        write_le_i32(
            &mut blob,
            entry_start + iface_off as usize + SLICE_LEN_OFFSET as usize,
            iface_name_len,
        );
        write_le_i32(
            &mut blob,
            entry_start + fn_off as usize + SLICE_PTR_OFFSET as usize,
            fd.fn_name_offset,
        );
        write_le_i32(
            &mut blob,
            entry_start + fn_off as usize + SLICE_LEN_OFFSET as usize,
            fd.fn_name_len,
        );
        if let Some(cells_off) = fd.result_cells_buf_offset {
            blob[entry_start + result_disc_off] = OPTION_SOME;
            write_le_i32(
                &mut blob,
                entry_start + tree_cells_base + SLICE_PTR_OFFSET as usize,
                cells_off as i32,
            );
            write_le_i32(
                &mut blob,
                entry_start + tree_cells_base + SLICE_LEN_OFFSET as usize,
                1,
            );
            write_le_i32(&mut blob, entry_start + tree_root_off_in_after, 0);
        } else {
            blob[entry_start + result_disc_off] = OPTION_NONE;
        }
    }
    blob
}

/// Resolve the (call.iface, call.fn, args.list) sub-offsets of the
/// on-call indirect-params buffer from the schema. Used by the
/// wrapper body to populate the buffer per-call.
fn compute_on_call_offsets(schema: &SchemaLayouts) -> Option<OnCallParamOffsets> {
    let l = schema.on_call_params_layout.as_ref()?;
    let call_off = l.offset_of(ON_CALL_CALL);
    let args_off = l.offset_of(ON_CALL_ARGS);
    let iface_base = call_off + schema.callid_layout.offset_of(CALLID_IFACE);
    let fn_base = call_off + schema.callid_layout.offset_of(CALLID_FN);
    Some(OnCallParamOffsets {
        iface_ptr: iface_base + SLICE_PTR_OFFSET,
        iface_len: iface_base + SLICE_LEN_OFFSET,
        fn_ptr: fn_base + SLICE_PTR_OFFSET,
        fn_len: fn_base + SLICE_LEN_OFFSET,
        args_ptr: args_off + SLICE_PTR_OFFSET,
        args_len: args_off + SLICE_LEN_OFFSET,
    })
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
    name_blob: &[u8],
    iface_name_offset: i32,
    iface_name_len: i32,
) -> StaticDataPlan {
    let mut layout = StaticLayout::new();

    layout.place_data(1, name_blob);

    // Cells slabs first — fields records embed pointers to these.
    for fd in per_func.iter_mut() {
        let slab_size = fd.params.len() as u32 * schema.cell_layout.size;
        fd.cells_buf_offset = layout.reserve_scratch(schema.cell_layout.align, slab_size);
    }
    if schema.after_hook.is_some() {
        for fd in per_func.iter_mut() {
            if fd.result_lift.is_some() {
                fd.result_cells_buf_offset = Some(
                    layout.reserve_scratch(schema.cell_layout.align, schema.cell_layout.size),
                );
            }
        }
    }

    // Fields blob (data) — pre-filled with cells.ptr pointing at
    // each param's reserved slab slot.
    let fields_blob = build_fields_blob(per_func, schema);
    let fields_base = layout.place_data(schema.field_layout.align, &fields_blob);
    let mut cursor = fields_base;
    for fd in per_func.iter_mut() {
        fd.fields_buf_offset = cursor;
        cursor += fd.params.len() as u32 * schema.field_layout.size;
    }

    // On-return params blob (data), only when after-hook is wired.
    let after_blob =
        build_after_params_blob(per_func, schema, iface_name_offset, iface_name_len);
    if let Some(al) = schema.on_return_params_layout.as_ref() {
        let after_base = layout.place_data(al.align, &after_blob);
        let mut cursor = after_base;
        for fd in per_func.iter_mut() {
            fd.after_params_offset = Some(cursor as i32);
            cursor += al.size;
        }
    }

    // Scratch slots: event record + on-call indirect-params buffer.
    let event_ptr = layout.reserve_scratch(EVENT_SLOT_ALIGN, EVENT_SLOT_SIZE) as i32;
    let (hook_params_ptr, on_call_offs) = match schema.on_call_params_layout.as_ref() {
        Some(l) => (
            layout.reserve_scratch(l.align, l.size),
            compute_on_call_offsets(schema),
        ),
        None => (0, None),
    };

    // Per-fn retptr scratch — only for funcs whose canonical-ABI
    // shape uses one. Back-fills RetptrPair so the wrapper body knows
    // the load address.
    for (fd, func) in per_func.iter_mut().zip(funcs.iter()) {
        if !(fd.export_sig.retptr || fd.import_sig.retptr) {
            continue;
        }
        let result_ty = func.result.as_ref().expect("retptr → func.result is_some()");
        let size = schema.size_align.size(result_ty).size_wasm32() as u32;
        let align = schema.size_align.align(result_ty).align_wasm32() as u32;
        let off = layout.reserve_scratch(align, size) as i32;
        fd.retptr_offset = Some(off);
        if let Some(ResultLift::RetptrPair { retptr_offset, .. }) = &mut fd.result_lift {
            *retptr_offset = off;
        }
    }

    let bump_start = align_up(layout.end(), 8);
    let data_segments = layout.into_segments();
    StaticDataPlan {
        bump_start,
        event_ptr,
        hook_params_ptr,
        on_call_offs,
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

        let params_lift = classify_func_params(resolve, func, name_blob)?;
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
        let task_return = is_async.then(|| {
            let (module, name, sig) =
                func.task_return_import(resolve, Some(&target_world_key), Mangling::Legacy);
            TaskReturnImport { module, name, sig }
        });

        per_func.push(FuncDispatch {
            is_async,
            task_return,
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
            cells_buf_offset: 0,
            fields_buf_offset: 0,
            retptr_offset: None,
            result_lift,
            result_cells_buf_offset: None,
            after_params_offset: None,
        });
    }
    Ok(per_func)
}

fn classify_func_params(
    resolve: &Resolve,
    func: &WitFunction,
    name_blob: &mut Vec<u8>,
) -> Result<Vec<ParamLift>> {
    let mut params_lift: Vec<ParamLift> = Vec::with_capacity(func.params.len());
    let mut slot_cursor: u32 = 0;
    for param in &func.params {
        let pname = &param.name;
        let ptype = &param.ty;
        let kind = LiftKind::classify(ptype, resolve).ok_or_else(|| {
            anyhow!(
                "tier-2 lift codegen does not yet support param `{pname}` of `{}` \
                 (type {ptype:?}) — only primitives + string + list<u8> are wired today",
                func.name
            )
        })?;
        let name_offset = name_blob.len() as i32;
        let name_len = pname.len() as i32;
        name_blob.extend_from_slice(pname.as_bytes());
        params_lift.push(ParamLift {
            name_offset,
            name_len,
            kind,
            first_local: slot_cursor,
        });
        slot_cursor += kind.slot_count();
    }
    Ok(params_lift)
}

/// Classify the function's return value for on-return lift. Direct
/// primitive returns capture into `result_local`; string / `list<u8>`
/// returns ride retptr. Compound returns we don't yet lift get
/// `None` → `option::none` at runtime.
///
/// For async funcs canon-lower-async always retptr's a non-void
/// result, so even primitive results live at the retptr scratch.
fn classify_result_lift(
    resolve: &Resolve,
    func: &WitFunction,
    export_sig: &WasmSignature,
    import_sig: &WasmSignature,
    is_async: bool,
) -> Option<ResultLift> {
    let kind = LiftKind::classify(func.result.as_ref()?, resolve)?;
    let result_at_retptr = if is_async {
        import_sig.retptr
    } else {
        export_sig.retptr
    };
    if result_at_retptr {
        Some(ResultLift::RetptrPair {
            kind,
            retptr_offset: 0, // back-filled by the layout phase.
        })
    } else {
        Some(ResultLift::Direct(kind))
    }
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

    let field_layout = RecordLayout::for_record_typedef(&size_align, resolve, field_ty_id);
    let tree_layout = RecordLayout::for_record_typedef(&size_align, resolve, field_tree_ty_id);
    let cell_layout = CellLayout::from_resolve(&size_align, resolve, cell_ty_id);
    let callid_layout = RecordLayout::for_record_typedef(&size_align, resolve, call_id_ty);

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

    let iface_name_offset: i32 = 0;
    let iface_name_len = target_interface_name.len() as i32;
    let mut name_blob: Vec<u8> = target_interface_name.as_bytes().to_vec();
    let mut per_func = build_per_func_dispatches(resolve, target_iface, &funcs, &mut name_blob)?;

    let plan = lay_out_static_memory(
        &mut per_func,
        &funcs,
        &schema,
        &name_blob,
        iface_name_offset,
        iface_name_len,
    );

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
    emit_code_section(
        &mut module,
        &per_func,
        &func_idx,
        &schema,
        resolve,
        iface_name_offset,
        iface_name_len,
        plan.hook_params_ptr as i32,
        plan.on_call_offs.as_ref(),
    );
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
                .ok_or_else(|| {
                    anyhow!("`splicer:common/types` is missing typedef `{type_name}`")
                });
        }
    }
    bail!("resolve has no `splicer:common/types` interface — was the common WIT loaded?")
}

fn write_le_i32(buf: &mut [u8], offset: usize, value: i32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn align_up(x: u32, align: u32) -> u32 {
    (x + align - 1) & !(align - 1)
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
    find_tier2_hook(resolve, world_id, &format!("{TIER2_BEFORE}@{TIER2_VERSION}"))
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
            let params = func
                .params
                .iter()
                .map(|p| (p.name.clone(), p.ty))
                .collect();
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
    let mut alloc_one = |ty_section: &mut TypeSection,
                         params: Vec<ValType>,
                         results: Vec<ValType>|
     -> u32 {
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
            fd.task_return.as_ref().map(|tr| {
                alloc_one(&mut types, val_types(&tr.sig.params), val_types(&tr.sig.results))
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
        imports.import(&fd.import_module, &fd.import_field, EntityType::Function(fty));
        next_imp += 1;
    }

    let before_hook_idx = before_hook.map(|h| {
        imports.import(&h.module, &h.name, EntityType::Function(ty.before_hook_ty.unwrap()));
        let idx = next_imp;
        next_imp += 1;
        idx
    });
    let after_hook_idx = after_hook.map(|h| {
        imports.import(&h.module, &h.name, EntityType::Function(ty.after_hook_ty.unwrap()));
        let idx = next_imp;
        next_imp += 1;
        idx
    });

    let async_funcs = canon_async::import_intrinsics(&mut imports, &ty.async_types, event_ptr, || {
        let i = next_imp;
        next_imp += 1;
        i
    });

    // Per-async-fn `task.return` imports. Mirrors tier-1's order:
    // imports come AFTER the canon-async runtime intrinsics. `Some`
    // iff the func is async.
    let mut task_return_idx: Vec<Option<u32>> = vec![None; per_func.len()];
    for (i, fd) in per_func.iter().enumerate() {
        if let Some(tr) = &fd.task_return {
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
fn emit_code_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
    func_idx: &FuncIndices,
    schema: &SchemaLayouts,
    resolve: &Resolve,
    iface_name_offset: i32,
    iface_name_len: i32,
    hook_params_ptr: i32,
    on_call_offs: Option<&OnCallParamOffsets>,
) {
    let mut code = CodeSection::new();
    for (i, fd) in per_func.iter().enumerate() {
        emit_wrapper_function(
            &mut code,
            per_func,
            func_idx,
            schema,
            resolve,
            iface_name_offset,
            iface_name_len,
            hook_params_ptr,
            on_call_offs,
            i,
            fd,
        );
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

/// Locals used by the wrapper body. Allocated once up front so all
/// downstream emit phases (param lift, hook calls, result lift, async
/// task.return load) reference the same indices.
struct WrapperLocals {
    /// Scratch for the cell write address.
    addr: u32,
    /// Packed status from canon-async hook calls.
    st: u32,
    /// Waitable-set handle for the wait loop.
    ws: u32,
    /// Retptr-loaded ptr for Text/Bytes result lift.
    ptr_scratch: u32,
    /// Retptr-loaded len for Text/Bytes result lift.
    len_scratch: u32,
    /// i64 widening source for IntegerSignExt/ZeroExt.
    ext64: u32,
    /// f64 promoted source for FloatingF32.
    ext_f64: u32,
    /// Direct-return value when the export sig has a single flat
    /// result; `None` otherwise.
    result: Option<u32>,
    /// Address local that drives `lift_from_memory` for async
    /// `task.return` flat loads. `None` for sync, void async, and
    /// async with retptr-passthrough task.return.
    tr_addr: Option<u32>,
}

fn alloc_wrapper_locals(locals: &mut FunctionIndices, fd: &FuncDispatch) -> WrapperLocals {
    let addr = locals.alloc_local(ValType::I32);
    let st = locals.alloc_local(ValType::I32);
    let ws = locals.alloc_local(ValType::I32);
    let ptr_scratch = locals.alloc_local(ValType::I32);
    let len_scratch = locals.alloc_local(ValType::I32);
    let ext64 = locals.alloc_local(ValType::I64);
    let ext_f64 = locals.alloc_local(ValType::F64);
    let result = direct_return_type(&fd.export_sig).map(|t| locals.alloc_local(t));
    // Async with a non-retptr-passthrough task.return needs an
    // i32 addr local so `lift_from_memory` can flat-load result
    // values out of the retptr scratch.
    let tr_uses_flat_loads = fd
        .task_return
        .as_ref()
        .is_some_and(|tr| !tr.sig.indirect_params && fd.result_ty.is_some());
    let tr_addr = tr_uses_flat_loads.then(|| locals.alloc_local(ValType::I32));
    WrapperLocals {
        addr,
        st,
        ws,
        ptr_scratch,
        len_scratch,
        ext64,
        ext_f64,
        result,
        tr_addr,
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_wrapper_function(
    code: &mut CodeSection,
    per_func: &[FuncDispatch],
    func_idx: &FuncIndices,
    schema: &SchemaLayouts,
    resolve: &Resolve,
    iface_name_offset: i32,
    iface_name_len: i32,
    hook_params_ptr: i32,
    on_call_offs: Option<&OnCallParamOffsets>,
    i: usize,
    fd: &FuncDispatch,
) {
    let async_funcs = &func_idx.async_funcs;
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
            let mut bindgen =
                WasmEncoderBindgen::new(&schema.size_align, addr_local, &mut locals);
            lift_from_memory(resolve, &mut bindgen, (), result_ty);
            bindgen.into_instructions()
        });

    let mut f = Function::new_with_locals_types(locals.into_locals());

    // ── Phase 1: on-call (only if before-hook wired) ──
    if let Some(before_idx) = func_idx.before_hook_idx {
        for (p_idx, p) in fd.params.iter().enumerate() {
            let cell_addr =
                fd.cells_buf_offset as i32 + p_idx as i32 * schema.cell_layout.size as i32;
            f.instructions().i32_const(cell_addr);
            f.instructions().local_set(lcl.addr);
            emit_lift_param(&mut f, &schema.cell_layout, p, lcl.addr, lcl.ext64, lcl.ext_f64);
        }
        let nargs = fd.params.len() as i32;
        let args_ptr = if nargs == 0 {
            0
        } else {
            fd.fields_buf_offset as i32
        };
        emit_populate_hook_params(
            &mut f,
            hook_params_ptr,
            on_call_offs.expect("before-hook wired → on-call layout computed"),
            iface_name_offset,
            iface_name_len,
            fd.fn_name_offset,
            fd.fn_name_len,
            args_ptr,
            nargs,
        );
        f.instructions().i32_const(hook_params_ptr);
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
    if fd.is_async {
        f.instructions().local_set(lcl.st);
        canon_async::emit_wait_loop(&mut f, lcl.st, lcl.ws, async_funcs);
    } else if let Some(local) = lcl.result {
        f.instructions().local_set(local);
    }

    // ── Phase 3: on-return (only if after-hook wired) ──
    if let Some(after_idx) = func_idx.after_hook_idx {
        if let (Some(rl), Some(cells_off)) =
            (fd.result_lift, fd.result_cells_buf_offset)
        {
            f.instructions().i32_const(cells_off as i32);
            f.instructions().local_set(lcl.addr);
            emit_lift_result(
                &mut f,
                &schema.cell_layout,
                rl,
                lcl.addr,
                lcl.ext64,
                lcl.ext_f64,
                lcl.ptr_scratch,
                lcl.len_scratch,
                lcl.result,
            );
        }
        f.instructions().i32_const(
            fd.after_params_offset
                .expect("has_after → after_params_offset set"),
        );
        canon_async::emit_call_and_wait(&mut f, after_idx, lcl.st, lcl.ws, async_funcs);
    }

    // ── Phase 4: tail. Async fns publish the result via task.return;
    // sync fns return the direct value (or static retptr).
    if fd.is_async {
        emit_task_return(&mut f, fd, func_idx, i, &lcl, task_return_loads.as_deref());
    } else {
        emit_wrapper_return(
            &mut f,
            lcl.result,
            fd.export_sig.retptr,
            fd.retptr_offset,
        );
    }
    f.instructions().end();
    code.function(&f);
    let _ = per_func; // (unused for now; kept on the signature for symmetry)
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
    let imp_task_return = func_idx.task_return_idx[i]
        .expect("async func must have task.return import");
    let tr = fd
        .task_return
        .as_ref()
        .expect("async func must have task_return descriptor");
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

/// Emit the wasm to lift one param into the cell at `addr_local`.
/// `ext64` / `ext_f64` are scratch widening locals (i64 / f64).
fn emit_lift_param(
    f: &mut Function,
    cell_layout: &CellLayout,
    p: &ParamLift,
    addr_local: u32,
    ext64: u32,
    ext_f64: u32,
) {
    emit_lift_kind(
        f,
        cell_layout,
        p.kind,
        p.first_local,
        p.first_local + 1,
        addr_local,
        ext64,
        ext_f64,
    );
}

/// Shared lift body for params and direct-return results. `slot0` /
/// `slot1` are wasm locals carrying the source value(s); for single-
/// slot kinds only `slot0` is used. Multi-slot kinds (Text/Bytes)
/// expect `(ptr, len)` in (slot0, slot1).
fn emit_lift_kind(
    f: &mut Function,
    cell_layout: &CellLayout,
    kind: LiftKind,
    slot0: u32,
    slot1: u32,
    addr_local: u32,
    ext64: u32,
    ext_f64: u32,
) {
    match kind {
        LiftKind::Bool => cell_layout.emit_bool(f, addr_local, slot0),
        LiftKind::IntegerSignExt => {
            f.instructions().local_get(slot0);
            f.instructions().i64_extend_i32_s();
            f.instructions().local_set(ext64);
            cell_layout.emit_integer(f, addr_local, ext64);
        }
        LiftKind::IntegerZeroExt => {
            f.instructions().local_get(slot0);
            f.instructions().i64_extend_i32_u();
            f.instructions().local_set(ext64);
            cell_layout.emit_integer(f, addr_local, ext64);
        }
        LiftKind::Integer64 => cell_layout.emit_integer(f, addr_local, slot0),
        LiftKind::FloatingF32 => {
            f.instructions().local_get(slot0);
            f.instructions().f64_promote_f32();
            f.instructions().local_set(ext_f64);
            cell_layout.emit_floating(f, addr_local, ext_f64);
        }
        LiftKind::FloatingF64 => cell_layout.emit_floating(f, addr_local, slot0),
        LiftKind::Text => cell_layout.emit_text(f, addr_local, slot0, slot1),
        LiftKind::Bytes => cell_layout.emit_bytes(f, addr_local, slot0, slot1),
    }
}

/// Emit the wasm to lift one return value into the cell at `addr_local`.
/// Direct primitive returns read from `result_local`; Text/Bytes
/// returns load `(ptr, len)` from the retptr scratch into `ptr_scratch`
/// / `len_scratch` and lift those.
fn emit_lift_result(
    f: &mut Function,
    cell_layout: &CellLayout,
    result_lift: ResultLift,
    addr_local: u32,
    ext64: u32,
    ext_f64: u32,
    ptr_scratch: u32,
    len_scratch: u32,
    result_local: Option<u32>,
) {
    match result_lift {
        ResultLift::Direct(kind) => {
            let local = result_local.expect("ResultLift::Direct → result_local must be set");
            emit_lift_kind(f, cell_layout, kind, local, local, addr_local, ext64, ext_f64);
        }
        ResultLift::RetptrPair {
            kind,
            retptr_offset,
        } => {
            f.instructions().i32_const(retptr_offset);
            f.instructions().i32_load(MemArg {
                offset: SLICE_PTR_OFFSET as u64,
                align: 2,
                memory_index: 0,
            });
            f.instructions().local_set(ptr_scratch);
            f.instructions().i32_const(retptr_offset);
            f.instructions().i32_load(MemArg {
                offset: SLICE_LEN_OFFSET as u64,
                align: 2,
                memory_index: 0,
            });
            f.instructions().local_set(len_scratch);
            emit_lift_kind(
                f,
                cell_layout,
                kind,
                ptr_scratch,
                len_scratch,
                addr_local,
                ext64,
                ext_f64,
            );
        }
    }
}

fn emit_data_section(module: &mut Module, segments: &[(u32, Vec<u8>)]) {
    if segments.is_empty() {
        return;
    }
    let mut data = DataSection::new();
    for (offset, bytes) in segments {
        data.active(0, &ConstExpr::i32_const(*offset as i32), bytes.iter().copied());
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
