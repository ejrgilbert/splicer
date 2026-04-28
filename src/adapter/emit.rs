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

use anyhow::{anyhow, bail, Context, Result};
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection,
    Function, FunctionSection, GlobalSection, GlobalType, ImportSection, MemorySection, MemoryType,
    Module, TypeSection, ValType,
};
use wit_bindgen_core::abi::lift_from_memory;
use wit_component::{
    decode, embed_component_metadata, ComponentEncoder, DecodedWasm, StringEncoding,
};
use wit_parser::abi::{AbiVariant, WasmSignature, WasmType};
use wit_parser::{
    Function as WitFunction, InterfaceId, LiftLowerAbi, Mangling, ManglingAndAbi, Resolve,
    SizeAlign, Type, WasmExport, WasmExportKind, WasmImport, WorldItem, WorldKey,
};

use super::abi::WasmEncoderBindgen;
use super::indices::{DispatchIndices, FunctionIndices};
use super::mem_layout::MemoryLayoutBuilder;

/// Generate the adapter component bytes. `target_interface` is the
/// fully-qualified interface name (`<ns>:<pkg>/<iface>[@<ver>]`);
/// `tier1_world_wit` is the contents of `wit/tier1/world.wit`.
pub(crate) fn build_adapter(
    target_interface: &str,
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    split_bytes: &[u8],
    tier1_world_wit: &str,
) -> Result<Vec<u8>> {
    let mut resolve = decode_input_resolve(split_bytes)?;
    let target_iface = find_target_interface(&resolve, target_interface)?;

    require_supported_case(&resolve, target_iface, has_blocking)?;

    resolve
        .push_str("splicer-tier1.wit", tier1_world_wit)
        .context("parse tier1 WIT")?;
    let world_pkg = resolve
        .push_str(
            "splicer-adapter.wit",
            &synthesize_adapter_world_wit(target_interface, has_before, has_after, has_blocking),
        )
        .context("parse synthesized adapter world WIT")?;
    let world_id = resolve
        .select_world(&[world_pkg], Some("adapter"))
        .context("select adapter world")?;

    let mut core_module = build_dispatch_module(
        &resolve,
        world_id,
        target_iface,
        target_interface,
        has_before,
        has_after,
        has_blocking,
    );
    embed_component_metadata(&mut core_module, &resolve, world_id, StringEncoding::UTF8)
        .context("embed_component_metadata")?;

    ComponentEncoder::default()
        .validate(true)
        .module(&core_module)
        .context("ComponentEncoder::module")?
        .encode()
        .context("ComponentEncoder::encode")
}

/// Decode the input split's WIT into a [`Resolve`]; bail if the bytes
/// decode to a WIT package rather than a component. `wit_component::decode`
/// panics on splits that import + re-export a resource-bearing instance
/// (https://github.com/bytecodealliance/wasm-tools/issues/2506); catch
/// it and surface a structured error so the process doesn't die.
fn decode_input_resolve(split_bytes: &[u8]) -> Result<Resolve> {
    let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode(split_bytes)))
        .map_err(|_| {
            anyhow!(
                "wit-parser panic during component decode — likely the import + re-export \
                 of a resource-bearing instance (upstream issue \
                 https://github.com/bytecodealliance/wasm-tools/issues/2506). The new emit \
                 path can't proceed until that's fixed upstream."
            )
        })?
        .context("wit_component::decode split")?;
    match decoded {
        DecodedWasm::Component(resolve, _world) => Ok(resolve),
        DecodedWasm::WitPackage(_, _) => bail!(
            "split bytes decoded to a WIT package; \
             expected a component"
        ),
    }
}

/// Find the target interface by its fully-qualified name.
fn find_target_interface(resolve: &Resolve, target_interface: &str) -> Result<InterfaceId> {
    resolve
        .interfaces
        .iter()
        .find(|(id, _)| resolve.id_of(*id).as_deref() == Some(target_interface))
        .map(|(id, _)| id)
        .ok_or_else(|| {
            anyhow!(
                "interface `{target_interface}` not found in \
                 the decoded WIT; available: {:?}",
                resolve
                    .interfaces
                    .iter()
                    .filter_map(|(id, _)| resolve.id_of(id))
                    .collect::<Vec<_>>()
            )
        })
}

/// Bail on cases the new path doesn't yet handle. Resource-bound
/// functions need a different dispatch shape; tier-1 blocking on a
/// non-void func is impossible (the adapter can't synthesize a return
/// value when the call is skipped) — same constraint legacy enforces.
fn require_supported_case(
    resolve: &Resolve,
    target_iface: InterfaceId,
    has_blocking: bool,
) -> Result<()> {
    let iface = &resolve.interfaces[target_iface];
    if iface.functions.is_empty() {
        bail!("interface has no functions");
    }
    for (name, func) in &iface.functions {
        if func.kind.resource().is_some() {
            bail!(
                "resource-bound function `{name}` ({:?}) \
                 not yet handled",
                func.kind
            );
        }
        if has_blocking && func.result.is_some() {
            bail!(
                "Function '{name}' returns a value but the middleware exports \
                 `should-block-call`. Tier-1 blocking is only supported for \
                 void-returning functions because the adapter cannot synthesize \
                 a return value when the call is blocked."
            );
        }
        // Async funcs whose params overflow `MAX_FLAT_ASYNC_PARAMS = 4`
        // canon-lower with `indirect_params = true` — the handler takes a
        // single params-pointer instead of flat values. The wrapper export
        // (`GuestExportAsyncStackful`, capped at `MAX_FLAT_PARAMS = 16`)
        // still receives flat, so we'd need to lower-to-memory before the
        // handler call. Driving `wit_bindgen_core::abi::lower_to_memory`
        // requires extending `WasmEncoderBindgen` with the store-side
        // `AbiInst` variants — not yet implemented.
        if func.kind.is_async() {
            let import_sig = resolve.wasm_signature(AbiVariant::GuestImportAsync, func);
            if import_sig.indirect_params {
                bail!(
                    "async function `{name}` has params that overflow \
                     MAX_FLAT_ASYNC_PARAMS (4) and require lower-to-memory; \
                     not yet implemented"
                );
            }
        }
    }
    Ok(())
}

/// Synthesize the adapter world: import + export the target interface
/// by name (no type recreation), and import the active tier1 hooks.
fn synthesize_adapter_world_wit(
    target_interface: &str,
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
) -> String {
    let mut wit = String::from("package splicer:adapter;\n\nworld adapter {\n");
    wit.push_str(&format!("    import {target_interface};\n"));
    wit.push_str(&format!("    export {target_interface};\n"));
    if has_before {
        wit.push_str("    import splicer:tier1/before@0.1.0;\n");
    }
    if has_after {
        wit.push_str("    import splicer:tier1/after@0.1.0;\n");
    }
    if has_blocking {
        wit.push_str("    import splicer:tier1/blocking@0.1.0;\n");
    }
    wit.push_str("}\n");
    wit
}

// ─── Dispatch core module ──────────────────────────────────────────

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
    is_async: bool,
    /// Wrapper export sig (`GuestExport` / `GuestExportAsyncStackful`).
    export_sig: WasmSignature,
    /// Handler import sig (`GuestImport` / `GuestImportAsync`).
    import_sig: WasmSignature,
    /// WIT result type for [`lift_from_memory`]; `None` for void.
    result_ty: Option<Type>,
    /// `Some` iff async.
    task_return: Option<TaskReturnImport>,
    /// Offset of the `<iface>#<fn>` string the middleware sees.
    name_offset: i32,
    name_len: i32,
    /// Offset of the retptr scratch buffer; set iff `import_sig.retptr`.
    retptr_offset: Option<i32>,
}

/// `(module, name, sig)` from [`WitFunction::task_return_import`].
struct TaskReturnImport {
    module: String,
    name: String,
    sig: WasmSignature,
}

impl FuncDispatch {
    /// Single flat result for the Direct (non-retptr, non-void) case.
    fn direct_result(&self) -> Option<ValType> {
        if !self.export_sig.retptr && self.export_sig.results.len() == 1 {
            Some(wasm_type_to_val(self.export_sig.results[0]))
        } else {
            None
        }
    }
}

/// wit-parser [`WasmType`]s → wasm-encoder [`ValType`]s.
fn val_types(types: &[WasmType]) -> Vec<ValType> {
    types.iter().copied().map(wasm_type_to_val).collect()
}

/// The dispatch module has one global — the bump pointer.
const BUMP_POINTER_GLOBAL: u32 = 0;

/// Type-section indices. `task_return_ty[i]` is `Some` iff func `i` is async.
struct TypeIndices {
    handler_ty: Vec<u32>,
    wrapper_ty: Vec<u32>,
    task_return_ty: Vec<Option<u32>>,
    /// Before/after hook sig: `(ptr, len) -> i32`.
    hook_ty: u32,
    /// Blocking hook sig: `(ptr, len, retptr) -> i32`. `Some` iff
    /// blocking is active.
    block_hook_ty: Option<u32>,
    init_ty: u32,
    cabi_post_ty: u32,
    cabi_realloc_ty: u32,
    async_runtime: Option<AsyncRuntimeTypes>,
}

/// Canon-async runtime builtin types (`$root/[waitable-*]`,
/// `[subtask-drop]`).
struct AsyncRuntimeTypes {
    /// `() -> i32`.
    waitable_new_ty: u32,
    /// `(i32, i32) -> ()`.
    waitable_join_ty: u32,
    /// `(i32, i32) -> i32`.
    waitable_wait_ty: u32,
    /// `(i32) -> ()` — shared by `[waitable-set-drop]` + `[subtask-drop]`.
    void_i32_ty: u32,
}

/// Imports come first (handlers, hooks, async-runtime, task.return),
/// then defined functions starting at `wrapper_base`. `imp_task_return[i]`
/// and `cabi_post[i]` are `Some` only for the per-func cases that need them.
struct FuncIndices {
    imp_handler: Vec<u32>,
    imp_before: Option<u32>,
    imp_after: Option<u32>,
    /// `should-block-call` import; `Some` iff blocking is active.
    imp_block: Option<u32>,
    imp_task_return: Vec<Option<u32>>,
    wrapper_base: u32,
    init: u32,
    /// `Some` iff sync + retptr (async-stackful has no post-return).
    cabi_post: Vec<Option<u32>>,
    /// Always `Some` — `cabi_realloc` is unconditionally exported.
    cabi_realloc: Option<u32>,
    /// Memory offset of the bool slot `should-block-call` writes its
    /// retptr into. `Some` iff blocking is active.
    block_result_ptr: Option<i32>,
    async_runtime: Option<AsyncRuntimeFuncs>,
}

/// Canon-async runtime builtin indices + the wait-event scratch
/// offset. Populated whenever any hook is active or any target func
/// is async.
struct AsyncRuntimeFuncs {
    waitable_new: u32,
    waitable_join: u32,
    waitable_wait: u32,
    waitable_drop: u32,
    subtask_drop: u32,
    event_ptr: i32,
}

/// Build the dispatch core module — phase orchestrator. `cabi_realloc` + the bump global
/// are emitted unconditionally (matches `wit_component::dummy_module`);
/// the ~30-byte cost is cheaper than a "does any type transitively contain a string/list?" walker.
fn build_dispatch_module(
    resolve: &Resolve,
    world_id: wit_parser::WorldId,
    target_iface: InterfaceId,
    target_interface_name: &str,
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
) -> Vec<u8> {
    let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
        .functions
        .values()
        .collect();
    // Any hook OR any async target needs the canon-async builtins
    // to await its subtask handle.
    let any_async_target = funcs.iter().any(|f| f.kind.is_async());
    let needs_async_runtime = has_before || has_after || has_blocking || any_async_target;
    let mut sizes = SizeAlign::default();
    sizes.fill(resolve);
    let (per_func, name_blob, event_ptr, block_result_ptr, bump_start) = compute_func_dispatches(
        resolve,
        &sizes,
        target_iface,
        target_interface_name,
        &funcs,
        needs_async_runtime,
        has_blocking,
    );
    let hook_imports = collect_hook_imports(resolve, world_id, has_before, has_after, has_blocking);
    let mut idx = DispatchIndices::new();

    let mut module = Module::new();
    let type_idx = emit_type_section(&mut module, &mut idx, &per_func, &hook_imports);
    let func_idx = emit_imports_section(
        &mut module,
        &mut idx,
        &per_func,
        &type_idx,
        &hook_imports,
        event_ptr,
        block_result_ptr,
    );
    let func_idx = emit_function_section(&mut module, &mut idx, &per_func, &type_idx, func_idx);
    emit_memory_and_globals(&mut module, bump_start);
    emit_export_section(&mut module, &per_func, &func_idx);
    emit_code_section(&mut module, resolve, &sizes, &per_func, &func_idx);
    emit_data_section(&mut module, &name_blob);

    module.finish()
}

/// Phase 1 — derive per-func dispatch shapes, collect name bytes, and
/// reserve memory slots for retptr scratch + the async-event record.
/// Hook-call name bytes are the fully-qualified `<iface>#<fn>` form
/// (the string the middleware receives as `name: string`).
/// `event_ptr` is `Some` iff `needs_async_runtime`.
#[allow(clippy::too_many_arguments)]
fn compute_func_dispatches(
    resolve: &Resolve,
    sizes: &SizeAlign,
    target_iface: InterfaceId,
    target_interface_name: &str,
    funcs: &[&WitFunction],
    needs_async_runtime: bool,
    has_blocking: bool,
) -> (Vec<FuncDispatch>, Vec<u8>, Option<i32>, Option<i32>, u32) {
    let qualified_names: Vec<String> = funcs
        .iter()
        .map(|f| format!("{target_interface_name}#{}", f.name))
        .collect();
    let total_name_bytes: u32 = qualified_names.iter().map(|n| n.len() as u32).sum();
    let mut layout = MemoryLayoutBuilder::new(total_name_bytes);
    let mut name_blob: Vec<u8> = Vec::with_capacity(total_name_bytes as usize);
    let mut per_func: Vec<FuncDispatch> = Vec::with_capacity(funcs.len());

    let target_world_key = WorldKey::Interface(target_iface);

    for (func, qualified_name) in funcs.iter().zip(qualified_names.iter()) {
        let is_async = func.kind.is_async();
        let (import_variant, export_variant) = if is_async {
            (
                AbiVariant::GuestImportAsync,
                AbiVariant::GuestExportAsyncStackful,
            )
        } else {
            (AbiVariant::GuestImport, AbiVariant::GuestExport)
        };
        // Same mangling on both sides → matched `[async-lower]<fn>` /
        // `[async-lift-stackful]<iface>#<fn>` pair (or no prefix for sync).
        let mangling = ManglingAndAbi::Legacy(if is_async {
            LiftLowerAbi::AsyncStackful
        } else {
            LiftLowerAbi::Sync
        });
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
        let name_offset = layout.alloc_name(qualified_name.len() as u32) as i32;
        name_blob.extend_from_slice(qualified_name.as_bytes());
        // Sync: retptr iff the export sig says so. Async: canon-lower-async
        // always retptr's a non-void result.
        let retptr_needed = if is_async {
            import_sig.retptr
        } else {
            export_sig.retptr
        };
        let retptr_offset = retptr_needed.then(|| {
            // Exact canonical-ABI size + alignment of the result type
            // — anything smaller than the type's natural alignment
            // (e.g. 4-byte buffer for an i64-bearing variant) traps
            // with "unaligned pointer" inside `lift_from_memory` /
            // wit-bindgen's async runtime.
            let result_ty = func
                .result
                .as_ref()
                .expect("retptr_needed → func.result is_some()");
            let size = sizes.size(result_ty).size_wasm32() as u32;
            let align = sizes.align(result_ty).align_wasm32() as u32;
            layout.alloc_retptr_scratch(size, align) as i32
        });
        let task_return = is_async.then(|| {
            let (module, name, sig) =
                func.task_return_import(resolve, Some(&target_world_key), Mangling::Legacy);
            TaskReturnImport { module, name, sig }
        });
        per_func.push(FuncDispatch {
            import_module,
            import_field,
            export_name,
            is_async,
            export_sig,
            import_sig,
            result_ty: func.result,
            task_return,
            name_offset,
            name_len: qualified_name.len() as i32,
            retptr_offset,
        });
    }
    // [`MemoryLayoutBuilder`] is single-cursor — fixed slots land
    // AFTER per-func name + retptr allocations, in the same order
    // the legacy path uses.
    let event_ptr = needs_async_runtime.then(|| layout.alloc_event_slot() as i32);
    let block_result_ptr = has_blocking.then(|| layout.alloc_block_result() as i32);
    let bump_start = layout.finish_as_bump_start();
    (per_func, name_blob, event_ptr, block_result_ptr, bump_start)
}

/// wit-parser [`WasmType`] → wasm-encoder [`ValType`] (wasm32:
/// `Pointer`/`Length` → i32, `PointerOrI64` → i64).
fn wasm_type_to_val(wt: WasmType) -> ValType {
    match wt {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => ValType::I32,
        WasmType::I64 | WasmType::PointerOrI64 => ValType::I64,
        WasmType::F32 => ValType::F32,
        WasmType::F64 => ValType::F64,
    }
}

/// One tier-1 hook import — `(module, name)` from
/// [`Resolve::wasm_import_name`] + sig from [`Resolve::wasm_signature`].
struct HookImport {
    module: String,
    name: String,
    sig: WasmSignature,
}

/// Active tier-1 hook imports. `before` / `after` share a common sig
/// (`(ptr, len) -> i32`); `blocking` has a retptr param for the bool
/// result (`(ptr, len, retptr) -> i32`).
struct HookImports {
    before: Option<HookImport>,
    after: Option<HookImport>,
    blocking: Option<HookImport>,
}

impl HookImports {
    fn any(&self) -> bool {
        self.before.is_some() || self.after.is_some() || self.blocking.is_some()
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
    has_blocking: bool,
) -> HookImports {
    let world = &resolve.worlds[world_id];
    let resolve_one = |iface_name: &str| -> Option<HookImport> {
        world.imports.iter().find_map(|(key, item)| {
            let WorldItem::Interface { id, .. } = item else {
                return None;
            };
            if resolve.id_of(*id).as_deref() != Some(iface_name) {
                return None;
            }
            // Tier-1 interfaces have exactly one function each.
            let func = resolve.interfaces[*id].functions.values().next()?;
            let (module, name) = resolve.wasm_import_name(
                ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback),
                WasmImport::Func {
                    interface: Some(key),
                    func,
                },
            );
            let sig = resolve.wasm_signature(AbiVariant::GuestImportAsync, func);
            Some(HookImport { module, name, sig })
        })
    };
    HookImports {
        before: has_before
            .then(|| resolve_one("splicer:tier1/before@0.1.0"))
            .flatten(),
        after: has_after
            .then(|| resolve_one("splicer:tier1/after@0.1.0"))
            .flatten(),
        blocking: has_blocking
            .then(|| resolve_one("splicer:tier1/blocking@0.1.0"))
            .flatten(),
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

    // Blocking hook sig — sourced from the WIT (`should-block-call:
    // async func(name: string) -> bool` lowered → `(ptr, len, retptr) -> i32`).
    let block_hook_ty = hook_imports.blocking.as_ref().map(|h| {
        types
            .ty()
            .function(val_types(&h.sig.params), val_types(&h.sig.results));
        idx.alloc_ty()
    });

    // Async-runtime types are needed when a hook is active OR any
    // target func is async — both fire the wait loop in the wrapper
    // body. Gating only on hooks would leave a hook-less async-target
    // module unable to await its handler subtask.
    let needs_async_runtime = hook_imports.any() || per_func.iter().any(|f| f.is_async);
    let async_runtime = needs_async_runtime.then(|| {
        types.ty().function([], [ValType::I32]);
        let waitable_new_ty = idx.alloc_ty();
        types.ty().function([ValType::I32, ValType::I32], []);
        let waitable_join_ty = idx.alloc_ty();
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
        let waitable_wait_ty = idx.alloc_ty();
        types.ty().function([ValType::I32], []);
        let void_i32_ty = idx.alloc_ty();
        AsyncRuntimeTypes {
            waitable_new_ty,
            waitable_join_ty,
            waitable_wait_ty,
            void_i32_ty,
        }
    });

    module.section(&types);
    TypeIndices {
        handler_ty,
        wrapper_ty,
        task_return_ty,
        hook_ty,
        block_hook_ty,
        init_ty,
        cabi_post_ty,
        cabi_realloc_ty,
        async_runtime,
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
    block_result_ptr: Option<i32>,
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
    let imp_block = hook_imports.blocking.as_ref().map(|hook| {
        let ty = type_idx
            .block_hook_ty
            .expect("block_hook_ty allocated when blocking is active");
        imports.import(&hook.module, &hook.name, EntityType::Function(ty));
        idx.alloc_func()
    });

    let async_runtime = type_idx.async_runtime.as_ref().map(|art| {
        let event_ptr = event_ptr.expect("event_ptr must be set when async_runtime is");
        imports.import(
            "$root",
            "[waitable-set-new]",
            EntityType::Function(art.waitable_new_ty),
        );
        let waitable_new = idx.alloc_func();
        imports.import(
            "$root",
            "[waitable-join]",
            EntityType::Function(art.waitable_join_ty),
        );
        let waitable_join = idx.alloc_func();
        imports.import(
            "$root",
            "[waitable-set-wait]",
            EntityType::Function(art.waitable_wait_ty),
        );
        let waitable_wait = idx.alloc_func();
        imports.import(
            "$root",
            "[waitable-set-drop]",
            EntityType::Function(art.void_i32_ty),
        );
        let waitable_drop = idx.alloc_func();
        imports.import(
            "$root",
            "[subtask-drop]",
            EntityType::Function(art.void_i32_ty),
        );
        let subtask_drop = idx.alloc_func();
        AsyncRuntimeFuncs {
            waitable_new,
            waitable_join,
            waitable_wait,
            waitable_drop,
            subtask_drop,
            event_ptr,
        }
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
        imp_block,
        imp_task_return,
        wrapper_base: 0,
        init: 0,
        cabi_post: vec![None; per_func.len()],
        cabi_realloc: None,
        block_result_ptr,
        async_runtime,
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
        if fd.export_sig.retptr && !fd.is_async {
            fsec.function(type_idx.cabi_post_ty);
            func_idx.cabi_post[i] = Some(idx.alloc_func());
        }
    }

    fsec.function(type_idx.cabi_realloc_ty);
    func_idx.cabi_realloc = Some(idx.alloc_func());

    module.section(&fsec);
    func_idx
}

/// Phase 5 — memory + bump-pointer global (paired with `cabi_realloc`).
fn emit_memory_and_globals(module: &mut Module, bump_start: u32) {
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memory);

    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(bump_start as i32),
    );
    module.section(&globals);
}

/// Phase 6 — export section. Wrapper names come from
/// [`Resolve::wasm_export_name`] via [`FuncDispatch::export_name`].
fn emit_export_section(module: &mut Module, per_func: &[FuncDispatch], func_idx: &FuncIndices) {
    let mut exports = ExportSection::new();
    for (i, fd) in per_func.iter().enumerate() {
        exports.export(
            &fd.export_name,
            ExportKind::Func,
            func_idx.wrapper_base + i as u32,
        );
        if let Some(post_idx) = func_idx.cabi_post[i] {
            let post_name = format!("cabi_post_{}", fd.export_name);
            exports.export(&post_name, ExportKind::Func, post_idx);
        }
    }
    exports.export("memory", ExportKind::Memory, 0);
    let realloc_idx = func_idx
        .cabi_realloc
        .expect("cabi_realloc is always emitted");
    exports.export("cabi_realloc", ExportKind::Func, realloc_idx);
    exports.export("_initialize", ExportKind::Func, func_idx.init);
    module.section(&exports);
}

/// Phase 7 — code section, declaration order matches phase 4.
fn emit_code_section(
    module: &mut Module,
    resolve: &Resolve,
    sizes: &SizeAlign,
    per_func: &[FuncDispatch],
    func_idx: &FuncIndices,
) {
    let blocking =
        func_idx
            .imp_block
            .zip(func_idx.block_result_ptr)
            .map(|(import_fn, result_ptr)| BlockingConfig {
                import_fn,
                result_ptr,
            });
    let mut code = CodeSection::new();
    for (i, fd) in per_func.iter().enumerate() {
        if fd.is_async {
            emit_async_wrapper_body(
                &mut code,
                resolve,
                sizes,
                fd,
                func_idx.imp_handler[i],
                func_idx.imp_before,
                func_idx.imp_after,
                blocking.as_ref(),
                func_idx.imp_task_return[i].expect("async func must have task.return import"),
                func_idx
                    .async_runtime
                    .as_ref()
                    .expect("async runtime imports active when any func is async"),
            );
        } else {
            emit_wrapper_body(
                &mut code,
                fd,
                func_idx.imp_handler[i],
                func_idx.imp_before,
                func_idx.imp_after,
                blocking.as_ref(),
                func_idx.async_runtime.as_ref(),
            );
        }
    }
    code.function(&empty_function());
    for fd in per_func {
        if fd.export_sig.retptr && !fd.is_async {
            code.function(&empty_function());
        }
    }
    emit_cabi_realloc(&mut code);
    module.section(&code);
}

/// Phase 8 — active data segment with the concatenated `<iface>#<fn>`
/// names the hooks see.
fn emit_data_section(module: &mut Module, name_blob: &[u8]) {
    if name_blob.is_empty() {
        return;
    }
    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), name_blob.iter().copied());
    module.section(&data);
}

/// `should-block-call` runtime bundle — the import fn index plus the
/// memory offset its retptr writes the bool result into.
struct BlockingConfig {
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
    blocking: Option<&BlockingConfig>,
    async_runtime: Option<&AsyncRuntimeFuncs>,
) {
    let nparams = fd.export_sig.params.len() as u32;
    let mut locals = FunctionIndices::new(nparams);
    let result_local = fd.direct_result().map(|t| locals.alloc_local(t));
    // Wait-loop scratch (subtask + waitable-set handles); shared
    // across before- / after-call / blocking awaits.
    let wait_locals = async_runtime.map(|_| {
        let st = locals.alloc_local(ValType::I32);
        let ws = locals.alloc_local(ValType::I32);
        (st, ws)
    });
    let mut f = Function::new_with_locals_types(locals.into_locals());

    if let Some(idx) = imp_before {
        emit_hook_call(&mut f, fd, idx, async_runtime, wait_locals);
    }
    if let Some(blk) = blocking {
        // Sync void early-return: matches legacy `emit_blocking_phase`.
        // `require_supported_case` already rejects sync + non-void
        // when blocking is active, so we don't need the
        // local-restoration song-and-dance here.
        emit_blocking_phase(&mut f, fd, blk, async_runtime, wait_locals, None);
    }
    for p in 0..nparams {
        f.instructions().local_get(p);
    }
    if fd.export_sig.retptr {
        f.instructions()
            .i32_const(fd.retptr_offset.expect("retptr_offset set"));
    }
    f.instructions().call(imp_handler);
    if let Some(local) = result_local {
        f.instructions().local_set(local);
    }
    if let Some(idx) = imp_after {
        emit_hook_call(&mut f, fd, idx, async_runtime, wait_locals);
    }
    if let Some(local) = result_local {
        f.instructions().local_get(local);
    } else if fd.export_sig.retptr {
        f.instructions()
            .i32_const(fd.retptr_offset.expect("retptr_offset set"));
    }
    f.instructions().end();
    code.function(&f);
}

/// Phase 2 (between before-call and the handler call): call
/// `should-block-call(name, retptr)`, await the subtask, load the
/// bool, and `return` early if it's true. For async wrappers a
/// `task.return` import index is supplied and called with no args
/// before the return (async-stackful must call task.return before
/// `End`); sync void wrappers just return.
///
/// Mirrors legacy `dispatch::emit_blocking_phase`.
/// `require_supported_case` already rejects non-void blocking, so
/// neither branch needs to fabricate a return value.
fn emit_blocking_phase(
    f: &mut Function,
    fd: &FuncDispatch,
    blk: &BlockingConfig,
    async_runtime: Option<&AsyncRuntimeFuncs>,
    wait_locals: Option<(u32, u32)>,
    task_return_for_async: Option<u32>,
) {
    f.instructions().i32_const(fd.name_offset);
    f.instructions().i32_const(fd.name_len);
    f.instructions().i32_const(blk.result_ptr);
    f.instructions().call(blk.import_fn);
    let art = async_runtime.expect("async_runtime active when blocking is");
    let (st, ws) = wait_locals.expect("wait_locals allocated alongside async_runtime");
    f.instructions().local_set(st);
    emit_wait_loop(f, st, ws, art);
    f.instructions().i32_const(blk.result_ptr);
    f.instructions().i32_load(wasm_encoder::MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    });
    f.instructions().if_(BlockType::Empty);
    if let Some(tr_fn) = task_return_for_async {
        f.instructions().call(tr_fn);
    }
    f.instructions().return_();
    f.instructions().end();
}

/// Emit one async-stackful wrapper body. Result is delivered via
/// `task.return` (not the wrapper's return); arg loads come from
/// [`lift_from_memory`] driven by [`WasmEncoderBindgen`].
#[allow(clippy::too_many_arguments)]
fn emit_async_wrapper_body(
    code: &mut CodeSection,
    resolve: &Resolve,
    sizes: &SizeAlign,
    fd: &FuncDispatch,
    imp_handler: u32,
    imp_before: Option<u32>,
    imp_after: Option<u32>,
    blocking: Option<&BlockingConfig>,
    imp_task_return: u32,
    async_runtime: &AsyncRuntimeFuncs,
) {
    let nparams = fd.export_sig.params.len() as u32;
    let mut locals = FunctionIndices::new(nparams);
    // Wait-loop scratch, shared across hook awaits + the handler await.
    let st = locals.alloc_local(ValType::I32);
    let ws = locals.alloc_local(ValType::I32);
    let wait_locals = Some((st, ws));
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

    let mut f = Function::new_with_locals_types(locals.into_locals());

    if let Some(idx) = imp_before {
        emit_hook_call(&mut f, fd, idx, Some(async_runtime), wait_locals);
    }
    if let Some(blk) = blocking {
        // Async-with-result + blocking is rejected in `require_supported_case`
        // (no way to fabricate the result), so reaching here means the
        // wrapper is async-void — we still need a `task.return` call
        // before returning early.
        emit_blocking_phase(
            &mut f,
            fd,
            blk,
            Some(async_runtime),
            wait_locals,
            Some(imp_task_return),
        );
    }

    // Handler call → packed status → wait.
    for p in 0..nparams {
        f.instructions().local_get(p);
    }
    if fd.import_sig.retptr {
        f.instructions()
            .i32_const(fd.retptr_offset.expect("retptr_offset for async retptr"));
    }
    f.instructions().call(imp_handler);
    f.instructions().local_set(st);
    emit_wait_loop(&mut f, st, ws, async_runtime);

    if let Some(idx) = imp_after {
        emit_hook_call(&mut f, fd, idx, Some(async_runtime), wait_locals);
    }

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

/// Call hook with `(name_ptr, name_len)` and await its packed subtask
/// handle. `async_runtime` + `wait_locals` are `Some` whenever a hook
/// is active.
fn emit_hook_call(
    f: &mut Function,
    fd: &FuncDispatch,
    hook_idx: u32,
    async_runtime: Option<&AsyncRuntimeFuncs>,
    wait_locals: Option<(u32, u32)>,
) {
    f.instructions().i32_const(fd.name_offset);
    f.instructions().i32_const(fd.name_len);
    f.instructions().call(hook_idx);
    let art = async_runtime.expect("async_runtime must be set when a hook is imported");
    let (st, ws) = wait_locals.expect("wait_locals allocated alongside async_runtime");
    f.instructions().local_set(st);
    emit_wait_loop(f, st, ws, art);
}

/// Await a packed `canon lower async` status in local `st`. The packed
/// i32 is `(handle << 4) | status_tag` (tag: 1=Started, 2=Returned);
/// after this helper `st` holds the raw handle, and if it was nonzero
/// the subtask has been joined into a fresh waitable-set, waited on,
/// and both handles dropped.
fn emit_wait_loop(f: &mut Function, st: u32, ws: u32, art: &AsyncRuntimeFuncs) {
    f.instructions().local_get(st);
    f.instructions().i32_const(4);
    f.instructions().i32_shr_u();
    f.instructions().local_set(st);
    f.instructions().local_get(st);
    f.instructions().if_(BlockType::Empty);
    f.instructions().call(art.waitable_new);
    f.instructions().local_set(ws);
    f.instructions().local_get(st);
    f.instructions().local_get(ws);
    f.instructions().call(art.waitable_join);
    f.instructions().local_get(ws);
    f.instructions().i32_const(art.event_ptr);
    f.instructions().call(art.waitable_wait);
    f.instructions().drop();
    f.instructions().local_get(st);
    f.instructions().call(art.subtask_drop);
    f.instructions().local_get(ws);
    f.instructions().call(art.waitable_drop);
    f.instructions().end();
}

/// No-op function body — used for `_initialize` and `cabi_post_*`.
fn empty_function() -> Function {
    let mut f = Function::new_with_locals_types([]);
    f.instructions().end();
    f
}

/// Bump-allocator `cabi_realloc(old_ptr, old_size, align, new_size)`.
/// Every call is a fresh alloc — `old_ptr`/`old_size` are ignored.
fn emit_cabi_realloc(code: &mut CodeSection) {
    const PARAM_COUNT: u32 = 4;
    const ALIGN_LOCAL: u32 = 2;
    const NEW_SIZE_LOCAL: u32 = 3;
    let mut locals = FunctionIndices::new(PARAM_COUNT);
    let scratch = locals.alloc_local(ValType::I32);
    let mut f = Function::new_with_locals_types(locals.into_locals());

    // scratch = (global.bump + (align - 1)) & ~(align - 1)
    f.instructions().global_get(BUMP_POINTER_GLOBAL);
    f.instructions().local_get(ALIGN_LOCAL);
    f.instructions().i32_const(1);
    f.instructions().i32_sub();
    f.instructions().i32_add();
    f.instructions().local_get(ALIGN_LOCAL);
    f.instructions().i32_const(1);
    f.instructions().i32_sub();
    f.instructions().i32_const(-1);
    f.instructions().i32_xor();
    f.instructions().i32_and();
    f.instructions().local_set(scratch);

    // global.bump = scratch + new_size
    f.instructions().local_get(scratch);
    f.instructions().local_get(NEW_SIZE_LOCAL);
    f.instructions().i32_add();
    f.instructions().global_set(BUMP_POINTER_GLOBAL);

    // return scratch
    f.instructions().local_get(scratch);
    f.instructions().end();
    code.function(&f);
}
