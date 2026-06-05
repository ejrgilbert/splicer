//! WIT-level adapter emission — a thin layer over
//! `wit_component::ComponentEncoder`:
//!
//! 1. Decode the input split's WIT via [`wit_component::decode`].
//! 2. Push the tier1 hook packages + an "adapter" world that
//!    references the target package by name (no type recreation).
//! 3. Emit a dispatch core module whose imports/exports match the
//!    canonical naming contract; all signatures, mangled names, and
//!    result loads come from `wit-parser` / `wit-bindgen-core`.
//! 4. Embed component-type metadata + run [`ComponentEncoder`].
//!
//! Resource-bound functions (constructor / method / static) bail to
//! the legacy emit path; everything else (sync, async-stackful,
//! primitive / string / list / record / variant / option / tuple
//! results) goes through here.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use wasm_encoder::{
    BlockType, CodeSection, EntityType, Function, FunctionSection, ImportSection, MemArg, Module,
    TypeSection, ValType,
};
use wit_bindgen_core::abi::lift_from_memory;
use wit_component::{embed_component_metadata, ComponentEncoder, StringEncoding};
use wit_parser::abi::{AbiVariant, WasmSignature, WasmType};
use wit_parser::{
    Function as WitFunction, InterfaceId, Mangling, Resolve, SizeAlign, Type, TypeId, WasmExport,
    WasmExportKind, WasmImport, WorldKey,
};

use super::super::abi::canon_async::{self, AsyncFuncs, AsyncTypes};
use super::super::abi::emit::{
    build_lower_params_to_memory, call_id_layout, collect_borrow_drops, direct_return_type,
    emit_alloc_call_id, emit_borrow_drops, emit_bump_restore, emit_bump_save, emit_cabi_realloc,
    emit_data_section, emit_export_section, emit_handler_call, emit_memory_and_globals,
    emit_populate_call_id, emit_resource_drop_imports, emit_wrapper_return, empty_function,
    find_imported_hook, require_gate_compatible_func, require_indirect_params_supported_shape,
    require_no_inline_resources, synthesize_adapter_world_wit, val_types, wasm_type_to_val,
    BlobSlice, BumpReset, CallIdLayout, GlobalIndices, HookImport, WrapperExport,
};
use super::super::abi::WasmEncoderBindgen;
use super::super::async_mirror::{synthesize_async_mirror, MIRROR_NAME_MISMATCH_PREFIX};
use super::super::indices::{DispatchIndices, LocalsBuilder};
use super::super::mem_layout::MemoryLayoutBuilder;
use super::super::resolve::{decode_input_resolve, dispatch_mangling, find_target_interface};

/// Generate the adapter component bytes. `target_interface` is the
/// fully-qualified interface name (`<ns>:<pkg>/<iface>[@<ver>]`);
/// `common_world_wit` is the contents of `wit/common/world.wit`
/// (loaded first as a dependency); `tier1_world_wit` is the contents
/// of `wit/tier1/world.wit` (which references `splicer:common`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_adapter(
    target_interface: &str,
    has_before: bool,
    has_after: bool,
    has_gate: bool,
    split_bytes: &[u8],
    common_world_wit: &str,
    tier1_world_wit: &str,
    mirror_export_name: Option<&str>,
) -> Result<Vec<u8>> {
    let mut resolve = decode_input_resolve(split_bytes)?;
    let target_iface = find_target_interface(&resolve, target_interface)?;

    // Bridged path: synthesize the async mirror once into our resolve
    // and lift against it. Lower side stays on the real sync target,
    // so the deepest downstream import wires to the unchanged provider.
    let (lift_iface, lift_interface_name) = match mirror_export_name {
        Some(expected) => {
            let (mirror_id, mirror_qname) = synthesize_async_mirror(&mut resolve, target_iface)
                .with_context(|| format!("synthesize async mirror for `{target_interface}`"))?;
            if mirror_qname != expected {
                bail!(
                    "{MIRROR_NAME_MISMATCH_PREFIX} for `{target_interface}`: \
                     wac wiring expected `{expected}` but tier-1 adapter \
                     synthesized `{mirror_qname}` — both sides should derive \
                     the same hash",
                );
            }
            (mirror_id, mirror_qname)
        }
        None => (target_iface, target_interface.to_string()),
    };
    let lower_iface = target_iface;
    let lower_interface_name = target_interface;

    require_supported_case(&resolve, lift_iface, lower_iface, has_gate)?;

    resolve
        .push_str("splicer-common.wit", common_world_wit)
        .context("parse common WIT")?;
    resolve
        .push_str("splicer-tier1.wit", tier1_world_wit)
        .context("parse tier1 WIT")?;
    let world_pkg = resolve
        .push_str(
            "splicer-adapter.wit",
            &synthesize_adapter_world_wit(
                ADAPTER_WORLD_PACKAGE,
                ADAPTER_WORLD_NAME,
                &lift_interface_name,
                lower_interface_name,
                &tier1_hook_imports(has_before, has_after, has_gate),
            ),
        )
        .context("parse synthesized adapter world WIT")?;
    let world_id = resolve
        .select_world(&[world_pkg], Some(ADAPTER_WORLD_NAME))
        .context("select adapter world")?;

    let mut core_module = build_dispatch_module(
        &resolve,
        world_id,
        lift_iface,
        lower_iface,
        lower_interface_name,
        has_before,
        has_after,
        has_gate,
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

/// Bail on cases the new path doesn't yet handle. Resource-bound
/// functions need a different dispatch shape; the tier-1 `gate` hook
/// on a non-void func is impossible (the adapter can't synthesize a
/// return value when the call is skipped) — same constraint legacy
/// enforces.
fn require_supported_case(
    resolve: &Resolve,
    lift_iface: InterfaceId,
    lower_iface: InterfaceId,
    has_gate: bool,
) -> Result<()> {
    let lift = &resolve.interfaces[lift_iface];
    if lift.functions.is_empty() {
        bail!("interface has no functions");
    }
    require_no_inline_resources(resolve, lift_iface)?;
    if lower_iface != lift_iface {
        require_no_inline_resources(resolve, lower_iface)?;
    }
    let lower = &resolve.interfaces[lower_iface];
    for (name, lift_func) in &lift.functions {
        if has_gate {
            require_gate_compatible_func(resolve, name, lift_func, "1")?;
        }
        // The indirect-params shape is determined by the IMPORT (handler
        // canon-lower) side — that's the bindgen path that
        // `build_lower_params_to_memory` drives. Lift and lower share
        // param/result types by construction (mirror differs only in
        // async-ness), but the cap differs by variant so we check the
        // lower func.
        let lower_func = lower
            .functions
            .get(name)
            .expect("async mirror synth guarantees fn-by-fn parity with lower iface");
        let import_is_async = lower_func.kind.is_async();
        let import_variant = if import_is_async {
            AbiVariant::GuestImportAsync
        } else {
            AbiVariant::GuestImport
        };
        let import_sig = resolve.wasm_signature(import_variant, lower_func);
        if import_sig.indirect_params {
            require_indirect_params_supported_shape(resolve, name, lower_func, import_is_async)?;
        }
    }
    Ok(())
}

/// List the active tier-1 hook interfaces as fully-qualified
/// versioned names (e.g. `"splicer:tier1/before@0.2.0"`), in
/// before/after/gate order.
fn tier1_hook_imports(has_before: bool, has_after: bool, has_gate: bool) -> Vec<String> {
    use crate::contract::{
        versioned_interface, TIER1_AFTER, TIER1_BEFORE, TIER1_GATE, TIER1_VERSION,
    };
    let mut out = Vec::new();
    if has_before {
        out.push(versioned_interface(TIER1_BEFORE, TIER1_VERSION));
    }
    if has_after {
        out.push(versioned_interface(TIER1_AFTER, TIER1_VERSION));
    }
    if has_gate {
        out.push(versioned_interface(TIER1_GATE, TIER1_VERSION));
    }
    out
}

// ─── Dispatch core module ──────────────────────────────────────────

/// `(module, name, sig)` from [`WitFunction::task_return_import`].
struct TaskReturnImport {
    module: String,
    name: String,
    sig: WasmSignature,
}

/// Per-function dispatch shape. All sigs and mangled names come from
/// [`Resolve::wasm_signature`] / [`Resolve::wasm_import_name`] /
/// [`Resolve::wasm_export_name`]; offsets come from
/// [`MemoryLayoutBuilder`].
struct FuncDispatch {
    /// Handler import module — the canonical interface name.
    import_module: String,
    /// Handler import field — `<fn>` (sync) or `[async-lower]<fn>` (async).
    import_field: String,
    /// Wrapper export name — `<iface>#<fn>` (sync) or
    /// `[async-lift-stackful]<iface>#<fn>` (async).
    export_name: String,
    /// True iff the lift (export) side is `async func` — drives the
    /// wrapper body shape (sync vs async-stackful). In the bridged
    /// path the lift side is the async mirror, so this is `true` even
    /// when the lower (handler) side is sync.
    export_is_async: bool,
    /// True iff the lower (handler) side is `async func` — drives
    /// whether the wrapper body wraps the handler call in a wait loop
    /// (canon-lower-async) or treats it as a direct call (sync).
    /// Differs from `export_is_async` only in the bridged path, where
    /// the lift is async (mirror) but the handler is sync (target).
    import_is_async: bool,
    /// Wrapper export sig (`GuestExport` / `GuestExportAsyncStackful`).
    export_sig: WasmSignature,
    /// Handler import sig (`GuestImport` / `GuestImportAsync`).
    import_sig: WasmSignature,
    /// WIT result type for [`lift_from_memory`]; `None` for void.
    result_ty: Option<Type>,
    /// `Some` iff async.
    task_return: Option<TaskReturnImport>,
    /// Offset/len of the target interface's fully-qualified name. The
    /// iface name is allocated once and shared across every
    /// `FuncDispatch` for the same target interface — duplicated here
    /// so hook-emission helpers can read it from `fd` without an
    /// extra parameter.
    iface_name_offset: i32,
    iface_name_len: i32,
    /// Offset/len of this function's name (canonical-ABI form, e.g.
    /// `"handle"`, `"[method]request.body"`).
    fn_name_offset: i32,
    fn_name_len: i32,
    /// Offset of the retptr scratch buffer; set iff `import_sig.retptr`.
    retptr_offset: Option<i32>,
    /// Offset of the indirect-params record buffer; set iff the
    /// canonical-ABI flip is asymmetric — `import_sig.indirect_params
    /// && !export_sig.indirect_params`. This is the async-stackful
    /// 5..16-flat corner where the handler wants a pointer but the
    /// wrapper still receives flat params. The wrapper lowers its
    /// flat locals into this buffer, then passes the pointer.
    ///
    /// The symmetric flips (sync at >16 flat, async at >16 flat —
    /// where wrapper and handler both want a pointer) need no buffer:
    /// the wrapper's local 0 holds the host's pointer and is passed
    /// straight through. Inherits bump's single-active-call assumption
    /// — concurrent invocations would clobber it.
    params_record_offset: Option<i32>,
    /// `(flat_param_idx, resource_type_id)` for each top-level
    /// `borrow<R>` param. The runtime requires us to drop the borrow
    /// before the wrapper returns; see `emit_wrapper_body`.
    borrow_drops: Vec<(u32, TypeId)>,
}

/// Synthesized adapter world's package + world name. The contents
/// don't matter as long as `select_world` and the WIT we push agree
/// on both.
const ADAPTER_WORLD_PACKAGE: &str = "splicer:adapter";
const ADAPTER_WORLD_NAME: &str = "adapter";

/// Single shared `call-id` indirect-params buffer used by every hook
/// callsite in this dispatch module.
struct CallIdBuf {
    offset: i32,
    layout: CallIdLayout,
}

/// Wrapper-body view: counter global to bump + the static buffer.
#[derive(Clone, Copy)]
struct CallIdWiring<'a> {
    counter_global: u32,
    buf: &'a CallIdBuf,
}

/// Type-section indices. `task_return_ty[i]` is `Some` iff func `i` is async.
struct TypeIndices {
    handler_ty: Vec<u32>,
    wrapper_ty: Vec<u32>,
    task_return_ty: Vec<Option<u32>>,
    /// Before/after hook sig: `(ptr, len) -> i32`.
    hook_ty: u32,
    /// Gate hook sig: `(ptr, len, retptr) -> i32`. `Some` iff `gate`
    /// is active.
    gate_hook_ty: Option<u32>,
    init_ty: u32,
    cabi_post_ty: u32,
    cabi_realloc_ty: u32,
    async_runtime: Option<AsyncTypes>,
    /// `(func (param i32))` for `[resource-drop]<R>` imports. `Some`
    /// iff any per_func has borrow params.
    resource_drop_ty: Option<u32>,
}

/// Imports come first (handlers, hooks, async-runtime, task.return),
/// then defined functions starting at `wrapper_base`. `imp_task_return[i]`
/// and `cabi_post[i]` are `Some` only for the per-func cases that need them.
struct FuncIndices {
    imp_handler: Vec<u32>,
    imp_before: Option<u32>,
    imp_after: Option<u32>,
    /// `should-call` import; `Some` iff `gate` is active.
    imp_gate: Option<u32>,
    imp_task_return: Vec<Option<u32>>,
    wrapper_base: u32,
    init: u32,
    /// `Some` iff sync + retptr (async-stackful has no post-return).
    cabi_post: Vec<Option<u32>>,
    /// Always `Some` — `cabi_realloc` is unconditionally exported.
    cabi_realloc: Option<u32>,
    /// Memory offset of the bool slot `should-call` writes its retptr
    /// into. `Some` iff `gate` is active.
    gate_result_ptr: Option<i32>,
    async_runtime: Option<AsyncFuncs>,
    /// `[resource-drop]<R>` import per resource referenced by a borrow
    /// param across `per_func`.
    resource_drop: HashMap<TypeId, u32>,
}

/// Build the dispatch core module — phase orchestrator. `cabi_realloc` + the bump global
/// are emitted unconditionally (matches `wit_component::dummy_module`);
/// the ~30-byte cost is cheaper than a "does any type transitively contain a string/list?" walker.
#[allow(clippy::too_many_arguments)]
fn build_dispatch_module(
    resolve: &Resolve,
    world_id: wit_parser::WorldId,
    lift_iface: InterfaceId,
    lower_iface: InterfaceId,
    lower_interface_name: &str,
    has_before: bool,
    has_after: bool,
    has_gate: bool,
) -> Result<Vec<u8>> {
    // Iterate the lift side — that's the iface the wrapper exports,
    // and the per-side async-ness on the lift drives every "needs
    // async runtime?" decision. In the NotNeeded path, lift == lower
    // so the same funcs come out either way.
    let lift_funcs: Vec<&WitFunction> = resolve.interfaces[lift_iface].functions.values().collect();
    let any_async_lift = lift_funcs.iter().any(|f| f.kind.is_async());
    let needs_async_runtime = has_before || has_after || has_gate || any_async_lift;
    let any_hook = has_before || has_after || has_gate;
    let mut sizes = SizeAlign::default();
    sizes.fill(resolve);
    let callid_layout = any_hook
        .then(|| call_id_layout(resolve, &sizes))
        .transpose()?;
    let plan = compute_func_dispatches(
        resolve,
        &sizes,
        lift_iface,
        lower_iface,
        lower_interface_name,
        &lift_funcs,
        SlotReservations {
            needs_async_runtime,
            has_gate,
            callid_layout,
        },
    )?;
    let hook_imports = collect_hook_imports(resolve, world_id, has_before, has_after, has_gate);
    let mut idx = DispatchIndices::new();

    let mut module = Module::new();
    let type_idx = emit_type_section(&mut module, &mut idx, &plan.per_func, &hook_imports);
    let func_idx = emit_imports_section(
        &mut module,
        &mut idx,
        &plan.per_func,
        &type_idx,
        &hook_imports,
        plan.event_ptr,
        plan.gate_result_ptr,
        resolve,
    );
    let func_idx =
        emit_function_section(&mut module, &mut idx, &plan.per_func, &type_idx, func_idx);
    let globals = emit_memory_and_globals(&mut module, plan.bump_start);
    let wrapper_exports: Vec<WrapperExport<'_>> = plan
        .per_func
        .iter()
        .enumerate()
        .map(|(i, fd)| WrapperExport {
            export_name: &fd.export_name,
            cabi_post_idx: func_idx.cabi_post[i],
        })
        .collect();
    emit_export_section(
        &mut module,
        &wrapper_exports,
        func_idx.wrapper_base,
        func_idx.init,
        func_idx
            .cabi_realloc
            .expect("cabi_realloc is always emitted"),
    );
    let call_id_wiring = plan.call_id_buf.as_ref().map(|buf| CallIdWiring {
        counter_global: globals.call_id_counter,
        buf,
    });
    emit_code_section(
        &mut module,
        resolve,
        &sizes,
        &plan.per_func,
        &lift_funcs,
        &func_idx,
        &globals,
        call_id_wiring,
    );
    let data_segments = if plan.name_blob.is_empty() {
        Vec::new()
    } else {
        vec![(0, plan.name_blob)]
    };
    emit_data_section(&mut module, &data_segments);

    Ok(module.finish())
}

/// Per-func dispatch shapes + the dispatch module's static-memory
/// addresses, returned from [`compute_func_dispatches`].
struct DispatchPlan {
    per_func: Vec<FuncDispatch>,
    name_blob: Vec<u8>,
    /// `Some` iff `SlotReservations::needs_async_runtime`.
    event_ptr: Option<i32>,
    /// `Some` iff `SlotReservations::has_gate`.
    gate_result_ptr: Option<i32>,
    /// `Some` iff any hook is wired (i.e. `callid_layout` is `Some`).
    call_id_buf: Option<CallIdBuf>,
    bump_start: u32,
}

/// Gates for the fixed-slot allocations [`compute_func_dispatches`]
/// makes after the per-func name + retptr slots. Each field controls
/// one slot in the dispatch module's static memory.
struct SlotReservations {
    /// Reserve the canon-async event record. Set when any hook is
    /// wired or the target has any async function.
    needs_async_runtime: bool,
    /// Reserve the gate-result scratch. Set when the middleware
    /// exports the tier-1 `gate` interface.
    has_gate: bool,
    /// `Some` iff any hook is wired — drives the call-id buffer's
    /// size + alignment.
    callid_layout: Option<CallIdLayout>,
}

/// Final layout end (data + scratch + bump-allocator base) must fit
/// in a signed i32 — every offset stored in `DispatchPlan` is `i32`,
/// and the bump global is initialized via `i32.const`. Mirrors tier-2's
/// `LAYOUT_SIZE_BUDGET`.
const LAYOUT_SIZE_BUDGET: u32 = i32::MAX as u32;

/// Phase 1 — per-func dispatch shapes, name bytes, and memory-slot
/// reservations (retptr scratch, event record, call-id buffer).
/// Iface name lives once at the head of memory; each fn name follows.
#[allow(clippy::too_many_arguments)]
fn compute_func_dispatches(
    resolve: &Resolve,
    sizes: &SizeAlign,
    lift_iface: InterfaceId,
    lower_iface: InterfaceId,
    lower_interface_name: &str,
    lift_funcs: &[&WitFunction],
    slots: SlotReservations,
) -> Result<DispatchPlan> {
    let iface_name_bytes = lower_interface_name.len() as u32;
    let total_fn_name_bytes: u32 = lift_funcs.iter().map(|f| f.name.len() as u32).sum();
    let total_name_bytes = iface_name_bytes + total_fn_name_bytes;
    let mut layout = MemoryLayoutBuilder::new(total_name_bytes);
    let mut name_blob: Vec<u8> = Vec::with_capacity(total_name_bytes as usize);
    let mut per_func: Vec<FuncDispatch> = Vec::with_capacity(lift_funcs.len());

    // The iface name placed in the call-id blob is the user-facing
    // (lower-side / target) name — that's what middleware authors see
    // in their hook bodies, regardless of whether the adapter is
    // mirror-lifted underneath.
    let iface_name_offset = layout.alloc_name(iface_name_bytes) as i32;
    name_blob.extend_from_slice(lower_interface_name.as_bytes());

    let lift_world_key = WorldKey::Interface(lift_iface);
    let lower_world_key = WorldKey::Interface(lower_iface);
    let lower = &resolve.interfaces[lower_iface];

    for lift_func in lift_funcs.iter() {
        // Lower-side func has the same name + param/result types by
        // construction — the mirror only differs in async-ness.
        let lower_func = lower
            .functions
            .get(&lift_func.name)
            .expect("async mirror synth guarantees fn-by-fn parity with lower iface");

        // Per-side async-ness drives variant + mangling independently.
        // In the bridged path the lift (mirror) is async and the lower
        // (target) is sync; in the NotNeeded path both agree.
        let export_is_async = lift_func.kind.is_async();
        let import_is_async = lower_func.kind.is_async();
        let export_variant = if export_is_async {
            AbiVariant::GuestExportAsyncStackful
        } else {
            AbiVariant::GuestExport
        };
        let import_variant = if import_is_async {
            AbiVariant::GuestImportAsync
        } else {
            AbiVariant::GuestImport
        };

        // Mangling is computed per side — in the bridged path the
        // import (sync canon-lower) and export (async-stackful
        // canon-lift) live in different ABI namespaces and get
        // different `[…]` prefixes.
        let (import_module, import_field) = resolve.wasm_import_name(
            dispatch_mangling(import_is_async),
            WasmImport::Func {
                interface: Some(&lower_world_key),
                func: lower_func,
            },
        );
        let export_name = resolve.wasm_export_name(
            dispatch_mangling(export_is_async),
            WasmExport::Func {
                interface: Some(&lift_world_key),
                func: lift_func,
                kind: WasmExportKind::Normal,
            },
        );
        let export_sig = resolve.wasm_signature(export_variant, lift_func);
        let import_sig = resolve.wasm_signature(import_variant, lower_func);
        let fn_name_offset = layout.alloc_name(lift_func.name.len() as u32) as i32;
        name_blob.extend_from_slice(lift_func.name.as_bytes());

        // Async-stackful wrappers always retptr the result (the value
        // gets written to memory and handed to `task.return` from
        // there), so any non-void async lift needs a scratch buffer
        // regardless of the handler's variant — same buffer also
        // backs canon-lower-async retptr when both sides are async.
        // Sync wrappers only need a buffer when the sync export sig
        // itself retptr's the result (compound types).
        let retptr_needed = if export_is_async {
            lift_func.result.is_some()
        } else {
            export_sig.retptr
        };
        let retptr_offset = retptr_needed.then(|| {
            let result_ty = lift_func
                .result
                .as_ref()
                .expect("retptr_needed → func.result is_some()");
            let size = sizes.size(result_ty).size_wasm32() as u32;
            let align = sizes.align(result_ty).align_wasm32() as u32;
            layout.alloc_aligned(size, align) as i32
        });
        let asymmetric_indirect = import_sig.indirect_params && !export_sig.indirect_params;
        let params_record_offset = asymmetric_indirect.then(|| {
            let param_types: Vec<Type> = lift_func.params.iter().map(|p| p.ty).collect();
            let info = sizes.record(&param_types);
            let size = info.size.size_wasm32() as u32;
            let align = info.align.align_wasm32() as u32;
            layout.alloc_aligned(size, align) as i32
        });
        // task.return is the lift-side intrinsic for async-stackful
        // wrappers; sourced from the lift func + world key.
        let task_return = export_is_async.then(|| {
            let (module, name, sig) =
                lift_func.task_return_import(resolve, Some(&lift_world_key), Mangling::Legacy);
            TaskReturnImport { module, name, sig }
        });
        let borrow_drops = collect_borrow_drops(resolve, lift_func);
        per_func.push(FuncDispatch {
            import_module,
            import_field,
            export_name,
            export_is_async,
            import_is_async,
            export_sig,
            import_sig,
            result_ty: lift_func.result,
            task_return,
            iface_name_offset,
            iface_name_len: iface_name_bytes as i32,
            fn_name_offset,
            fn_name_len: lift_func.name.len() as i32,
            retptr_offset,
            params_record_offset,
            borrow_drops,
        });
    }
    // [`MemoryLayoutBuilder`] is single-cursor — fixed slots land
    // AFTER per-func name + retptr allocations, in the same order
    // the legacy path uses.
    let event_ptr = slots
        .needs_async_runtime
        .then(|| layout.alloc_event_slot() as i32);
    let gate_result_ptr = slots.has_gate.then(|| layout.alloc_gate_result() as i32);
    let call_id_buf = slots.callid_layout.map(|callid_layout| {
        let offset = layout.alloc_aligned(callid_layout.size(), callid_layout.align()) as i32;
        CallIdBuf {
            offset,
            layout: callid_layout,
        }
    });
    let bump_start = layout.finish_as_bump_start();
    if bump_start > LAYOUT_SIZE_BUDGET {
        bail!("static-data layout end {bump_start} exceeds i32 budget {LAYOUT_SIZE_BUDGET}");
    }
    Ok(DispatchPlan {
        per_func,
        name_blob,
        event_ptr,
        gate_result_ptr,
        call_id_buf,
        bump_start,
    })
}

/// Active tier-1 hook imports. `before` / `after` share a common sig
/// (`(ptr) -> i32`); `gate` has a retptr param for the bool
/// result (`(ptr, retptr) -> i32`).
struct HookImports {
    before: Option<HookImport>,
    after: Option<HookImport>,
    gate: Option<HookImport>,
}

impl HookImports {
    fn any(&self) -> bool {
        self.before.is_some() || self.after.is_some() || self.gate.is_some()
    }
}

/// Resolve tier-1 hook imports through wit-parser so a contract bump
/// (or a `wit/tier1/world.wit` signature change) can't silently
/// desync the dispatch module.
fn collect_hook_imports(
    resolve: &Resolve,
    world_id: wit_parser::WorldId,
    has_before: bool,
    has_after: bool,
    has_gate: bool,
) -> HookImports {
    use crate::contract::{
        versioned_interface, TIER1_AFTER, TIER1_BEFORE, TIER1_GATE, TIER1_VERSION,
    };
    let pick = |active: bool, iface: &str| -> Option<HookImport> {
        active
            .then(|| {
                find_imported_hook(
                    resolve,
                    world_id,
                    &versioned_interface(iface, TIER1_VERSION),
                )
            })
            .flatten()
    };
    HookImports {
        before: pick(has_before, TIER1_BEFORE),
        after: pick(has_after, TIER1_AFTER),
        gate: pick(has_gate, TIER1_GATE),
    }
}

/// Phase 2 — type section: per-func handler + wrapper (+ per-async-func
/// task.return), then the four singletons (hook, init, cabi_post,
/// cabi_realloc), then the async-runtime builtin types when needed.
fn emit_type_section(
    module: &mut Module,
    idx: &mut DispatchIndices,
    per_func: &[FuncDispatch],
    hook_imports: &HookImports,
) -> TypeIndices {
    let mut types = TypeSection::new();
    let mut handler_ty: Vec<u32> = Vec::with_capacity(per_func.len());
    let mut wrapper_ty: Vec<u32> = Vec::with_capacity(per_func.len());
    let mut task_return_ty: Vec<Option<u32>> = vec![None; per_func.len()];

    for (i, fd) in per_func.iter().enumerate() {
        types.ty().function(
            val_types(&fd.import_sig.params),
            val_types(&fd.import_sig.results),
        );
        handler_ty.push(idx.alloc_ty());
        types.ty().function(
            val_types(&fd.export_sig.params),
            val_types(&fd.export_sig.results),
        );
        wrapper_ty.push(idx.alloc_ty());
        if let Some(tr) = &fd.task_return {
            types
                .ty()
                .function(val_types(&tr.sig.params), val_types(&tr.sig.results));
            task_return_ty[i] = Some(idx.alloc_ty());
        }
    }

    // Both hooks share the same `async func(name: string)` shape; pick
    // whichever's active. With neither active the slot is unreferenced
    // and falls back to `() -> ()`.
    let hook_sig = hook_imports
        .before
        .as_ref()
        .or(hook_imports.after.as_ref())
        .map(|h| (val_types(&h.sig.params), val_types(&h.sig.results)))
        .unwrap_or_else(|| (vec![], vec![]));
    types.ty().function(hook_sig.0, hook_sig.1);
    let hook_ty = idx.alloc_ty();
    types.ty().function([], []);
    let init_ty = idx.alloc_ty();
    types.ty().function([ValType::I32], []);
    let cabi_post_ty = idx.alloc_ty();
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        [ValType::I32],
    );
    let cabi_realloc_ty = idx.alloc_ty();

    // Gate hook sig — sourced from the WIT (`should-call:
    // async func(name: string) -> bool` lowered → `(ptr, len, retptr) -> i32`).
    let gate_hook_ty = hook_imports.gate.as_ref().map(|h| {
        types
            .ty()
            .function(val_types(&h.sig.params), val_types(&h.sig.results));
        idx.alloc_ty()
    });

    // Async-runtime types are needed when a hook is active OR any
    // target func is async — both fire the wait loop in the wrapper
    // body. Gating only on hooks would leave a hook-less async-target
    // module unable to await its handler subtask.
    let needs_async_runtime = hook_imports.any() || per_func.iter().any(|f| f.export_is_async);
    let async_runtime =
        needs_async_runtime.then(|| canon_async::emit_types(&mut types, || idx.alloc_ty()));

    // `[resource-drop]<R>`: `(func (param i32))`. Reuse async runtime's
    // void-i32 slot when available; otherwise allocate fresh.
    let needs_resource_drop = per_func.iter().any(|f| !f.borrow_drops.is_empty());
    let resource_drop_ty = needs_resource_drop.then(|| {
        if let Some(art) = &async_runtime {
            art.void_i32_ty
        } else {
            types.ty().function([ValType::I32], []);
            idx.alloc_ty()
        }
    });

    module.section(&types);
    TypeIndices {
        handler_ty,
        wrapper_ty,
        task_return_ty,
        hook_ty,
        gate_hook_ty,
        init_ty,
        cabi_post_ty,
        cabi_realloc_ty,
        async_runtime,
        resource_drop_ty,
    }
}

/// Phase 3 — import section: per-func handlers + hooks + async-runtime
/// builtins + per-async-func task.return. Hook + handler names come
/// from [`Resolve::wasm_import_name`]; the `$root/[waitable-*]` /
/// `[subtask-drop]` builtins are wit-component intrinsics not exposed
/// via wit-parser (mirrors `dummy_module::push_root_async_intrinsics`).
#[allow(clippy::too_many_arguments)]
fn emit_imports_section(
    module: &mut Module,
    idx: &mut DispatchIndices,
    per_func: &[FuncDispatch],
    type_idx: &TypeIndices,
    hook_imports: &HookImports,
    event_ptr: Option<i32>,
    gate_result_ptr: Option<i32>,
    resolve: &Resolve,
) -> FuncIndices {
    let mut imports = ImportSection::new();
    let mut imp_handler: Vec<u32> = Vec::with_capacity(per_func.len());
    for (i, fd) in per_func.iter().enumerate() {
        imports.import(
            &fd.import_module,
            &fd.import_field,
            EntityType::Function(type_idx.handler_ty[i]),
        );
        imp_handler.push(idx.alloc_func());
    }
    // `[resource-drop]<R>` imports — one per unique borrow resource.
    let resource_drop = emit_resource_drop_imports(
        &mut imports,
        resolve,
        per_func,
        |f| &f.borrow_drops,
        type_idx.resource_drop_ty,
        || idx.alloc_func(),
    );
    let mut import_hook = |hook: &HookImport| {
        imports.import(
            &hook.module,
            &hook.name,
            EntityType::Function(type_idx.hook_ty),
        );
        idx.alloc_func()
    };
    let imp_before = hook_imports.before.as_ref().map(&mut import_hook);
    let imp_after = hook_imports.after.as_ref().map(&mut import_hook);
    let imp_gate = hook_imports.gate.as_ref().map(|hook| {
        let ty = type_idx
            .gate_hook_ty
            .expect("gate_hook_ty allocated when gate is active");
        imports.import(&hook.module, &hook.name, EntityType::Function(ty));
        idx.alloc_func()
    });

    let async_runtime = type_idx.async_runtime.as_ref().map(|art| {
        let event_ptr = event_ptr.expect("event_ptr must be set when async_runtime is");
        canon_async::import_intrinsics(&mut imports, art, event_ptr, || idx.alloc_func())
    });

    // task.return: `[export]<iface>` / `[task-return]<fn>` per
    // `Function::task_return_import` (already in `fd.task_return`).
    let mut imp_task_return: Vec<Option<u32>> = vec![None; per_func.len()];
    for (i, fd) in per_func.iter().enumerate() {
        if let Some(tr) = &fd.task_return {
            let ty_idx = type_idx.task_return_ty[i].expect("task_return_ty allocated for async");
            imports.import(&tr.module, &tr.name, EntityType::Function(ty_idx));
            imp_task_return[i] = Some(idx.alloc_func());
        }
    }

    module.section(&imports);

    FuncIndices {
        imp_handler,
        imp_before,
        imp_after,
        imp_gate,
        imp_task_return,
        wrapper_base: 0,
        init: 0,
        cabi_post: vec![None; per_func.len()],
        cabi_realloc: None,
        gate_result_ptr,
        async_runtime,
        resource_drop,
    }
}

/// Phase 4 — function section. `cabi_realloc` is always declared.
fn emit_function_section(
    module: &mut Module,
    idx: &mut DispatchIndices,
    per_func: &[FuncDispatch],
    type_idx: &TypeIndices,
    mut func_idx: FuncIndices,
) -> FuncIndices {
    let mut fsec = FunctionSection::new();

    let wrapper_base = idx.func;
    for &t in &type_idx.wrapper_ty {
        fsec.function(t);
    }
    for _ in per_func {
        idx.alloc_func();
    }
    func_idx.wrapper_base = wrapper_base;

    fsec.function(type_idx.init_ty);
    func_idx.init = idx.alloc_func();

    // `cabi_post_*` only for sync retptr — async-stackful has no
    // post-return contract.
    for (i, fd) in per_func.iter().enumerate() {
        if fd.export_sig.retptr && !fd.export_is_async {
            fsec.function(type_idx.cabi_post_ty);
            func_idx.cabi_post[i] = Some(idx.alloc_func());
        }
    }

    fsec.function(type_idx.cabi_realloc_ty);
    func_idx.cabi_realloc = Some(idx.alloc_func());

    module.section(&fsec);
    func_idx
}

/// Phase 7 — code section, declaration order matches phase 4.
#[allow(clippy::too_many_arguments)]
fn emit_code_section(
    module: &mut Module,
    resolve: &Resolve,
    sizes: &SizeAlign,
    per_func: &[FuncDispatch],
    funcs: &[&WitFunction],
    func_idx: &FuncIndices,
    globals: &GlobalIndices,
    call_id_wiring: Option<CallIdWiring<'_>>,
) {
    debug_assert_eq!(
        per_func.len(),
        funcs.len(),
        "FuncDispatch list and WitFunction list must be index-aligned",
    );
    let gate = func_idx
        .imp_gate
        .zip(func_idx.gate_result_ptr)
        .map(|(import_fn, result_ptr)| GateConfig {
            import_fn,
            result_ptr,
        });
    let mut code = CodeSection::new();
    for (i, fd) in per_func.iter().enumerate() {
        if fd.export_is_async {
            emit_async_wrapper_body(
                &mut code,
                resolve,
                sizes,
                fd,
                funcs[i],
                func_idx.imp_handler[i],
                func_idx.imp_before,
                func_idx.imp_after,
                gate.as_ref(),
                func_idx.imp_task_return[i].expect("async func must have task.return import"),
                func_idx
                    .async_runtime
                    .as_ref()
                    .expect("async runtime imports active when any func is async"),
                &func_idx.resource_drop,
                call_id_wiring,
                globals.bump,
            );
        } else {
            emit_wrapper_body(
                &mut code,
                fd,
                func_idx.imp_handler[i],
                func_idx.imp_before,
                func_idx.imp_after,
                gate.as_ref(),
                func_idx.async_runtime.as_ref(),
                &func_idx.resource_drop,
                call_id_wiring,
                globals.bump,
            );
        }
    }
    code.function(&empty_function());
    for fd in per_func {
        if fd.export_sig.retptr && !fd.export_is_async {
            code.function(&empty_function());
        }
    }
    emit_cabi_realloc(&mut code, globals.bump);
    module.section(&code);
}

/// `should-call` runtime bundle — the import fn index plus the
/// memory offset its retptr writes the bool result into.
struct GateConfig {
    import_fn: u32,
    result_ptr: i32,
}

/// Emit one sync wrapper body. Shape is read off
/// [`FuncDispatch::export_sig`]: `retptr` ⇒ multi-flat / compound,
/// else `results.len() == 1` ⇒ Direct, else Void.
#[allow(clippy::too_many_arguments)]
fn emit_wrapper_body(
    code: &mut CodeSection,
    fd: &FuncDispatch,
    imp_handler: u32,
    imp_before: Option<u32>,
    imp_after: Option<u32>,
    gate: Option<&GateConfig>,
    async_runtime: Option<&AsyncFuncs>,
    resource_drop: &HashMap<TypeId, u32>,
    call_id_wiring: Option<CallIdWiring<'_>>,
    bump_global: u32,
) {
    let nparams = fd.export_sig.params.len() as u32;
    let mut locals = LocalsBuilder::new(nparams);
    let bump_reset = BumpReset {
        global: bump_global,
        saved_local: locals.alloc_local(ValType::I32),
    };
    let result_local = direct_return_type(&fd.export_sig).map(|t| locals.alloc_local(t));
    // Wait-loop scratch (subtask + waitable-set handles); shared
    // across on-call / on-return / gate awaits.
    let wait_locals = async_runtime.map(|_| {
        let st = locals.alloc_local(ValType::I32);
        let ws = locals.alloc_local(ValType::I32);
        (st, ws)
    });
    // i64 call-id local + per-callsite bundle; both gated on hook wired.
    let hook_site = call_id_wiring.map(|w| HookSite {
        fd,
        buf: w.buf,
        id_local: locals.alloc_local(ValType::I64),
    });
    let mut f = Function::new_with_locals_types(locals.freeze().locals);

    emit_bump_save(&mut f, bump_reset);

    if let (Some(w), Some(site)) = (call_id_wiring, hook_site) {
        emit_alloc_call_id(&mut f, w.counter_global, site.id_local);
        // Buffer contents (iface/fn name + id) are identical across
        // before / gate / after — populate once up front.
        populate_hook_call_id(&mut f, &site);
    }
    if let Some(idx) = imp_before {
        emit_hook_call(&mut f, idx, async_runtime, wait_locals, hook_site.unwrap());
    }
    if let Some(g) = gate {
        emit_gate_phase(
            &mut f,
            g,
            async_runtime,
            wait_locals,
            None,
            hook_site.unwrap(),
            bump_reset,
        );
    }
    emit_handler_call(
        &mut f,
        nparams,
        fd.export_sig.retptr,
        fd.retptr_offset,
        imp_handler,
    );
    if let Some(local) = result_local {
        f.instructions().local_set(local);
    }
    if let Some(idx) = imp_after {
        emit_hook_call(&mut f, idx, async_runtime, wait_locals, hook_site.unwrap());
    }
    emit_borrow_drops(&mut f, &fd.borrow_drops, resource_drop);
    emit_bump_restore(&mut f, bump_reset);
    emit_wrapper_return(&mut f, result_local, fd.export_sig.retptr, fd.retptr_offset);
    f.instructions().end();
    code.function(&f);
}

/// Per-callsite bundle for [`emit_hook_call`] / [`emit_gate_phase`].
#[derive(Clone, Copy)]
struct HookSite<'a> {
    fd: &'a FuncDispatch,
    buf: &'a CallIdBuf,
    id_local: u32,
}

/// Between on-call and the handler call: call
/// `should-call(call_ptr, retptr)`, await, and `return` early if
/// it's false (skip the downstream call). The shared call-id buffer
/// is populated by the wrapper body once before any hook fires; this
/// helper just pushes the args and awaits. Async wrappers call
/// `task.return` first (`task_return_for_async`); sync void just
/// returns. Non-void targets paired with `gate` are rejected upstream.
fn emit_gate_phase(
    f: &mut Function,
    gate: &GateConfig,
    async_runtime: Option<&AsyncFuncs>,
    wait_locals: Option<(u32, u32)>,
    task_return_for_async: Option<u32>,
    site: HookSite<'_>,
    bump_reset: BumpReset,
) {
    f.instructions().i32_const(site.buf.offset);
    f.instructions().i32_const(gate.result_ptr);
    f.instructions().call(gate.import_fn);
    let art = async_runtime.expect("async_runtime active when gate is");
    let (st, ws) = wait_locals.expect("wait_locals allocated alongside async_runtime");
    f.instructions().local_set(st);
    canon_async::emit_wait_loop(f, st, ws, art);
    f.instructions().i32_const(gate.result_ptr);
    // Canonical-ABI bool is 1 byte; the slot is i32-sized scratch but
    // only byte 0 carries the value. Use a byte-load to keep the load
    // width consistent with the type and the alignment honest.
    f.instructions().i32_load8_u(wasm_encoder::MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    });
    // `true` (nonzero) → call downstream (fall through). `false` (0)
    // → skip and return; `i32.eqz` inverts so the `if` arm fires on
    // the skip path.
    f.instructions().i32_eqz();
    f.instructions().if_(BlockType::Empty);
    if let Some(tr_fn) = task_return_for_async {
        f.instructions().call(tr_fn);
    }
    emit_bump_restore(f, bump_reset);
    f.instructions().return_();
    f.instructions().end();
}

/// Emit one async-stackful wrapper body. Result is delivered via
/// `task.return` (not the wrapper's return); arg loads come from
/// [`lift_from_memory`] driven by [`WasmEncoderBindgen`].
#[allow(clippy::too_many_arguments)]
/// Store a single flat wasm value (top-of-stack) at the address
/// underneath it on the stack, using the canonical natural alignment
/// for the type. Used by the bridged direct-return path to spill the
/// sync handler's return value into the `retptr_offset` buffer that
/// the async lift's `task.return` then reads.
fn emit_store_flat_value(f: &mut Function, ty: WasmType) {
    match ty {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => {
            f.instructions().i32_store(MemArg {
                offset: 0,
                align: 2,
                memory_index: 0,
            });
        }
        WasmType::I64 | WasmType::PointerOrI64 => {
            f.instructions().i64_store(MemArg {
                offset: 0,
                align: 3,
                memory_index: 0,
            });
        }
        WasmType::F32 => {
            f.instructions().f32_store(MemArg {
                offset: 0,
                align: 2,
                memory_index: 0,
            });
        }
        WasmType::F64 => {
            f.instructions().f64_store(MemArg {
                offset: 0,
                align: 3,
                memory_index: 0,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_async_wrapper_body(
    code: &mut CodeSection,
    resolve: &Resolve,
    sizes: &SizeAlign,
    fd: &FuncDispatch,
    func: &WitFunction,
    imp_handler: u32,
    imp_before: Option<u32>,
    imp_after: Option<u32>,
    gate: Option<&GateConfig>,
    imp_task_return: u32,
    async_runtime: &AsyncFuncs,
    resource_drop: &HashMap<TypeId, u32>,
    call_id_wiring: Option<CallIdWiring<'_>>,
    bump_global: u32,
) {
    let nparams = fd.export_sig.params.len() as u32;
    let mut locals = LocalsBuilder::new(nparams);
    let bump_reset = BumpReset {
        global: bump_global,
        saved_local: locals.alloc_local(ValType::I32),
    };
    // Wait-loop scratch, shared across hook awaits + the handler await.
    let st = locals.alloc_local(ValType::I32);
    let ws = locals.alloc_local(ValType::I32);
    let wait_locals = Some((st, ws));
    // Bridged direct-return spill: when the wrapper is async-lifted but
    // the handler is sync canon-lower with a single flat result, the
    // call leaves that result on the stack. We can't feed it into
    // `task.return`'s flat-load path directly (which reads from
    // `retptr_offset`), so spill to a typed local and store into
    // `retptr_offset` before the task.return logic runs. Sync retptr
    // handlers wrote to `retptr_offset` themselves; sync void
    // handlers leave nothing on the stack — neither needs a spill.
    let bridged_direct_return_spill =
        (!fd.import_is_async && !fd.import_sig.retptr && fd.result_ty.is_some()).then(|| {
            let ty = wasm_type_to_val(fd.import_sig.results[0]);
            locals.alloc_local(ty)
        });
    // task.return is `wasm_signature(GuestImport, fake_func_with_result_as_param)`:
    // small results flatten into params; large results overflow into a
    // single retptr param (`indirect_params=true`). The `retptr` flag is
    // only set for actual *results* and is always false for task.return
    // (whose fake_func has no result), so use `indirect_params` to pick
    // the path.
    let tr_sig = &fd.task_return.as_ref().expect("async has task_return").sig;
    let tr_uses_flat_loads = !tr_sig.indirect_params && fd.result_ty.is_some();
    let tr_addr_local = tr_uses_flat_loads.then(|| locals.alloc_local(ValType::I32));

    // Build the load sequence BEFORE freezing locals — `lift_from_memory`
    // allocates additional scratch (variant disc, joined-payload slots, …).
    let task_return_loads: Option<Vec<wasm_encoder::Instruction<'static>>> =
        tr_addr_local.map(|addr_local| {
            let result_ty = fd.result_ty.as_ref().expect("flat loads → result_ty");
            let mut bindgen = WasmEncoderBindgen::new(sizes, addr_local, &mut locals);
            lift_from_memory(resolve, &mut bindgen, (), result_ty);
            bindgen.into_instructions()
        });
    // Build the params lower-to-memory sequence BEFORE freezing locals,
    // for the same reason as task_return_loads — the bindgen allocates
    // an addr local + per-ValType store scratch through `locals`. Only
    // populated when the asymmetric flip applies: handler wants a
    // pointer (`import_sig.indirect_params`) but the wrapper still has
    // flat locals (`!export_sig.indirect_params`). Async-stackful
    // exports cap at 16-flat, so this is the 5..16 corner.
    let asymmetric_indirect = fd.import_sig.indirect_params && !fd.export_sig.indirect_params;
    let params_lower_seq: Option<Vec<wasm_encoder::Instruction<'static>>> = asymmetric_indirect
        .then(|| {
            let base = fd
                .params_record_offset
                .expect("asymmetric indirect → params_record_offset reserved");
            build_lower_params_to_memory(resolve, sizes, &mut locals, func, base)
        });
    // i64 call-id local + per-callsite bundle; both gated on hook wired.
    let hook_site = call_id_wiring.map(|w| HookSite {
        fd,
        buf: w.buf,
        id_local: locals.alloc_local(ValType::I64),
    });

    let mut f = Function::new_with_locals_types(locals.freeze().locals);

    emit_bump_save(&mut f, bump_reset);

    if let (Some(w), Some(site)) = (call_id_wiring, hook_site) {
        emit_alloc_call_id(&mut f, w.counter_global, site.id_local);
        // Buffer contents (iface/fn name + id) are identical across
        // before / gate / after — populate once up front.
        populate_hook_call_id(&mut f, &site);
    }
    if let Some(idx) = imp_before {
        emit_hook_call(
            &mut f,
            idx,
            Some(async_runtime),
            wait_locals,
            hook_site.unwrap(),
        );
    }
    if let Some(g) = gate {
        // Async-with-result + gate is rejected upstream — reaching
        // here means async-void, so `task.return` runs before the early
        // return.
        emit_gate_phase(
            &mut f,
            g,
            Some(async_runtime),
            wait_locals,
            Some(imp_task_return),
            hook_site.unwrap(),
            bump_reset,
        );
    }

    // Handler call → packed status → wait.
    //
    // Two arg shapes per canon-lower-async:
    //   - direct: push each flat function param.
    //   - indirect: replay the pre-built lower-to-memory sequence,
    //     then push the params record's pointer. The wrapper's flat
    //     function params have already been written into the static
    //     params slot at this point.
    if let Some(seq) = params_lower_seq.as_ref() {
        for inst in seq {
            f.instruction(inst);
        }
        f.instructions().i32_const(
            fd.params_record_offset
                .expect("indirect_params → params_record_offset"),
        );
    } else {
        for p in 0..nparams {
            f.instructions().local_get(p);
        }
    }
    if fd.import_sig.retptr {
        f.instructions()
            .i32_const(fd.retptr_offset.expect("retptr_offset for async retptr"));
    }
    f.instructions().call(imp_handler);
    if fd.import_is_async {
        // Canon-lower-async returns a packed (handle << 4) | status —
        // wait until the subtask resolves before reading the result
        // from the retptr buffer.
        f.instructions().local_set(st);
        canon_async::emit_wait_loop(&mut f, st, ws, async_runtime);
    } else if let Some(spill_local) = bridged_direct_return_spill {
        // Bridged direct-return: sync canon-lower left a single flat
        // result on the stack. The task.return path expects to read
        // from `retptr_offset`, so spill the value there.
        let result_wasm_ty = fd.import_sig.results[0];
        f.instructions().local_set(spill_local);
        f.instructions().i32_const(
            fd.retptr_offset
                .expect("retptr_offset reserved for non-void async lift"),
        );
        f.instructions().local_get(spill_local);
        emit_store_flat_value(&mut f, result_wasm_ty);
    }
    // The remaining bridged cases — sync retptr handler (wrote to
    // retptr_offset itself) and sync void handler (no result) — need
    // nothing here; the task.return logic below picks them up.

    if let Some(idx) = imp_after {
        emit_hook_call(
            &mut f,
            idx,
            Some(async_runtime),
            wait_locals,
            hook_site.unwrap(),
        );
    }

    emit_borrow_drops(&mut f, &fd.borrow_drops, resource_drop);

    emit_bump_restore(&mut f, bump_reset);

    // task.return shape: void (no args), retptr (pass the buffer
    // through — large compound result), or flat (lift each slot via
    // `lift_from_memory`).
    if fd.result_ty.is_none() {
        f.instructions().call(imp_task_return);
    } else if tr_sig.indirect_params {
        f.instructions()
            .i32_const(fd.retptr_offset.expect("retptr_offset for async retptr"));
        f.instructions().call(imp_task_return);
    } else {
        let addr_local = tr_addr_local.expect("flat loads → tr_addr_local");
        f.instructions()
            .i32_const(fd.retptr_offset.expect("retptr_offset for async retptr"));
        f.instructions().local_set(addr_local);
        for inst in task_return_loads
            .as_ref()
            .expect("task_return_loads built for flat loads")
        {
            f.instruction(inst);
        }
        f.instructions().call(imp_task_return);
    }
    f.instructions().end();
    code.function(&f);
}

/// Push the shared call-id buffer address, call the hook
/// (`indirect_params = true`), and await. Caller is responsible for
/// populating the buffer once up front — its contents are identical
/// across before / gate / after callsites.
fn emit_hook_call(
    f: &mut Function,
    hook_idx: u32,
    async_runtime: Option<&AsyncFuncs>,
    wait_locals: Option<(u32, u32)>,
    site: HookSite<'_>,
) {
    f.instructions().i32_const(site.buf.offset);
    let art = async_runtime.expect("async_runtime set when hook imported");
    let (st, ws) = wait_locals.expect("wait_locals allocated with async_runtime");
    canon_async::emit_call_and_wait(f, hook_idx, st, ws, art);
}

/// Lower the call-id record into the shared buffer.
fn populate_hook_call_id(f: &mut Function, site: &HookSite<'_>) {
    emit_populate_call_id(
        f,
        site.buf.offset,
        0,
        &site.buf.layout,
        BlobSlice {
            off: site.fd.iface_name_offset as u32,
            len: site.fd.iface_name_len as u32,
        },
        BlobSlice {
            off: site.fd.fn_name_offset as u32,
            len: site.fd.fn_name_len as u32,
        },
        site.id_local,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `wit`, find `<pkg_name>/<iface_name>`, return its
    /// `InterfaceId`. Drives `require_supported_case` directly from a
    /// WIT source string.
    fn iface_from_wit(wit: &str, pkg_name: &str, iface_name: &str) -> (Resolve, InterfaceId) {
        let mut resolve = Resolve::default();
        resolve.push_str("test.wit", wit).expect("parse test WIT");
        let target = format!("{pkg_name}/{iface_name}");
        let iface_id = resolve
            .interfaces
            .iter()
            .find(|(id, _)| resolve.id_of(*id).as_deref() == Some(&target))
            .map(|(id, _)| id)
            .expect("target interface present");
        (resolve, iface_id)
    }

    /// Inline-resource interface (`resource cat` declared inside the
    /// same interface that uses it) bails with a clear error pointing
    /// at the factored-types fix.
    #[test]
    fn require_supported_case_bails_on_inline_resource() {
        let (resolve, iface_id) = iface_from_wit(
            r#"
            package my:shape@1.0.0;
            interface api {
                resource cat { constructor(); }
                foo: func(x: cat) -> cat;
            }
            "#,
            "my:shape",
            "api@1.0.0",
        );
        let err = require_supported_case(&resolve, iface_id, iface_id, false)
            .expect_err("inline resource should bail");
        let msg = err.to_string();
        assert!(
            msg.contains("declares resource `cat` inline"),
            "error should call out the inline declaration; got: {msg}"
        );
        assert!(
            msg.contains("factored-types pattern"),
            "error should point at the factored-types fix; got: {msg}"
        );
    }

    /// Factored-types: resource in a sibling `types` interface,
    /// referenced via `use types.{{cat}}` in `api`. Accepted.
    #[test]
    fn require_supported_case_accepts_factored_types() {
        let (resolve, iface_id) = iface_from_wit(
            r#"
            package my:shape@1.0.0;
            interface types {
                resource cat { constructor(); }
            }
            interface api {
                use types.{cat};
                foo: func(x: cat) -> cat;
            }
            "#,
            "my:shape",
            "api@1.0.0",
        );
        require_supported_case(&resolve, iface_id, iface_id, false)
            .expect("factored-types should be accepted");
    }

    /// Sanity: value-type-only interfaces (no resources at all) pass.
    #[test]
    fn require_supported_case_accepts_value_types() {
        let (resolve, iface_id) = iface_from_wit(
            r#"
            package my:shape@1.0.0;
            interface api {
                foo: func(x: u32) -> u32;
            }
            "#,
            "my:shape",
            "api@1.0.0",
        );
        require_supported_case(&resolve, iface_id, iface_id, false)
            .expect("value-type interfaces should be accepted");
    }

    /// Indirect-params async fn with a primitive *alias* (`type my-id =
    /// u32`) — `is_primitive_param_ty` follows `TypeDefKind::Type` so
    /// `my-id` is treated as scalar. Pins the alias-following branch.
    #[test]
    fn require_supported_case_accepts_primitive_alias_in_indirect_params() {
        let (resolve, iface_id) = iface_from_wit(
            r#"
            package my:shape@1.0.0;
            interface api {
                type my-id = u32;
                many: async func(a: my-id, b: my-id, c: my-id, d: my-id, e: my-id) -> u32;
            }
            "#,
            "my:shape",
            "api@1.0.0",
        );
        require_supported_case(&resolve, iface_id, iface_id, false)
            .expect("primitive alias in indirect-params position should be accepted");
    }

    /// Records / tuples / enums / flags — and nested combinations —
    /// all pass `is_supported_indirect_params_ty` in indirect-params
    /// position.
    #[test]
    fn require_supported_case_accepts_record_tuple_enum_flags_in_indirect_params() {
        let (resolve, iface_id) = iface_from_wit(
            r#"
            package my:shape@1.0.0;
            interface api {
                record point { x: u32, y: u32 }
                enum color { red, green, blue }
                flags perms { read, write, exec }
                many: async func(
                    p: point,
                    t: tuple<u32, u64>,
                    c: color,
                    f: perms,
                    nested: tuple<point, color>
                ) -> u32;
            }
            "#,
            "my:shape",
            "api@1.0.0",
        );
        require_supported_case(&resolve, iface_id, iface_id, false)
            .expect("record / tuple / enum / flags in indirect-params should be accepted");
    }

    /// Sync indirect-params (params >16 flat). After the gate-flip,
    /// `require_supported_case` must validate without bailing — the
    /// adapter passes the host's params pointer through unchanged,
    /// so the same supported-type predicate applies for sync as for
    /// async.
    #[test]
    fn require_supported_case_accepts_sync_indirect_params() {
        let (resolve, iface_id) = iface_from_wit(
            r#"
            package my:shape@1.0.0;
            interface api {
                wide: func(
                    a: u32, b: u32, c: u32, d: u32, e: u32, f: u32,
                    g: u32, h: u32, i: u32, j: u32, k: u32, l: u32,
                    m: u32, n: u32, o: u32, p: u32, q: u32
                );
            }
            "#,
            "my:shape",
            "api@1.0.0",
        );
        require_supported_case(&resolve, iface_id, iface_id, false)
            .expect("17×u32 sync func should be accepted (params overflow MAX_FLAT_PARAMS)");
    }

    /// Lists, strings, fixed-length-lists, variants, options, results,
    /// handles, and maps — every canonical-ABI value-type kind —
    /// accepted in indirect-params position.
    #[test]
    fn require_supported_case_accepts_full_value_type_space_in_indirect_params() {
        let (resolve, iface_id) = iface_from_wit(
            r#"
            package my:shape@1.0.0;
            interface api {
                variant either { left(u32), right(u64), neither }
                many: async func(
                    s: string,
                    l: list<u32>,
                    fl: list<u32, 4>,
                    o: option<u32>,
                    r: result<u32, string>,
                    v: either,
                    m: map<string, u32>,
                ) -> u32;
            }
            "#,
            "my:shape",
            "api@1.0.0",
        );
        require_supported_case(&resolve, iface_id, iface_id, false)
            .expect("full canonical-ABI value-type space accepted");
    }
}
