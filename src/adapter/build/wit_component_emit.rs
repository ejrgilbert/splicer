//! WIT-level adapter emission. The adapter generator is a thin layer
//! over `wit_component::ComponentEncoder`:
//!
//! 1. Decode the input split via [`wit_component::decode`] — this is
//!    the source of truth for the target interface's shape.
//! 2. Add the tier1 hook packages and an "adapter" world that
//!    references the existing target package as both an import and
//!    an export. The world's `import` / `export` paths resolve
//!    against packages already in the [`Resolve`].
//! 3. Emit a dispatch core module whose imports / exports match the
//!    naming contract `wit-component` expects (verified against
//!    `examples/wit_component_spike.rs`). Canonical-ABI flat
//!    lowering for params and results comes from
//!    [`Resolve::wasm_signature`] — no parallel computation in
//!    splicer.
//! 4. Embed component-type metadata onto the core module and hand
//!    it to `ComponentEncoder` to produce the adapter component
//!    bytes.
//!
//! This emit path is intentionally independent of cviz's `AdapterFunc`
//! / `WitBridge`. cviz still owns the composition-graph reasoning that
//! decides *where* to splice; once a splice point is picked and the
//! relevant component bytes are extracted as `split_bytes`, adapter
//! generation runs entirely on `wit-parser` + `wit-component`.
//!
//! Today the emitter handles only sync target funcs whose result is
//! either void, a single direct flat value, or retptr-shaped (string /
//! list / complex compound). Async target funcs and resource
//! constructors / methods / statics fall through to the legacy emit
//! path until those widenings land. The tier-1 hooks themselves are
//! always async (per `wit/tier1/world.wit`), so the dispatch module
//! always emits the `[async-lower]` import shape and a wait loop.

use anyhow::{Context, Result, anyhow, bail};
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection,
    Function, FunctionSection, GlobalSection, GlobalType, ImportSection, MemorySection,
    MemoryType, Module, TypeSection, ValType,
};
use wit_component::{ComponentEncoder, DecodedWasm, StringEncoding, decode, embed_component_metadata};
use wit_parser::abi::{AbiVariant, WasmSignature, WasmType};
use wit_parser::{
    Function as WitFunction, InterfaceId, LiftLowerAbi, ManglingAndAbi, Resolve, WasmImport,
    WorldItem,
};

use super::mem_layout::MemoryLayoutBuilder;
use crate::adapter::indices::{DispatchIndices, FunctionIndices};

/// Generate the adapter component bytes via the WIT-level emit path.
///
/// `target_interface` names the interface to wrap (e.g.
/// `"wasi:http/handler@0.3.0-rc-2026-01-06"`). It must exist as an
/// interface in the WIT decoded from `split_bytes`.
///
/// `tier1_world_wit` is the contents of `wit/tier1/world.wit` — passed
/// in rather than read from disk so callers can choose between
/// `include_str!` for shipping and a real file read for tests.
pub(crate) fn build_adapter_via_wit_component(
    target_interface: &str,
    has_before: bool,
    has_after: bool,
    _has_blocking: bool,
    split_bytes: &[u8],
    tier1_world_wit: &str,
) -> Result<Vec<u8>> {
    let mut resolve = decode_input_resolve(split_bytes)?;
    let target_iface = find_target_interface(&resolve, target_interface)?;

    require_supported_case(&resolve, target_iface)?;

    resolve
        .push_str("splicer-tier1.wit", tier1_world_wit)
        .context("parse tier1 WIT")?;
    let world_pkg = resolve
        .push_str(
            "splicer-adapter.wit",
            &synthesize_adapter_world_wit(target_interface, has_before, has_after),
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

/// Decode the input split's existing WIT into a [`Resolve`]. Bails if
/// the bytes decode to a WIT package rather than a component (we only
/// adapt real components).
fn decode_input_resolve(split_bytes: &[u8]) -> Result<Resolve> {
    match decode(split_bytes).context("wit_component::decode split")? {
        DecodedWasm::Component(resolve, _world) => Ok(resolve),
        DecodedWasm::WitPackage(_, _) => bail!(
            "wit_component_emit: split bytes decoded to a WIT package; \
             expected a component"
        ),
    }
}

/// Locate the target interface in the decoded [`Resolve`] by its
/// fully-qualified name (`<ns>:<pkg>/<iface>[@<version>]`).
fn find_target_interface(resolve: &Resolve, target_interface: &str) -> Result<InterfaceId> {
    resolve
        .interfaces
        .iter()
        .find(|(id, _)| resolve.id_of(*id).as_deref() == Some(target_interface))
        .map(|(id, _)| id)
        .ok_or_else(|| {
            anyhow!(
                "wit_component_emit: interface `{target_interface}` not found in \
                 the decoded WIT; available: {:?}",
                resolve
                    .interfaces
                    .iter()
                    .filter_map(|(id, _)| resolve.id_of(id))
                    .collect::<Vec<_>>()
            )
        })
}

/// Bail out of the new path for any case it doesn't yet handle. Each
/// widening step (async target funcs, resource constructors / methods
/// / statics) removes one of these constraints.
fn require_supported_case(resolve: &Resolve, target_iface: InterfaceId) -> Result<()> {
    let iface = &resolve.interfaces[target_iface];
    if iface.functions.is_empty() {
        bail!("wit_component_emit: interface has no functions");
    }
    for (name, func) in &iface.functions {
        if func.kind.resource().is_some() {
            // Constructors, methods, and statics use a different
            // dispatch shape (host-side resource rep, special export
            // names) we'll cover in a follow-up.
            bail!(
                "wit_component_emit: resource-bound function `{name}` ({:?}) \
                 not yet handled",
                func.kind
            );
        }
        if func.kind.is_async() {
            // Async target funcs need [async-lift-stackful] export
            // names + per-func task.return imports + retptr-form
            // handler call lifted via `lift_from_memory`. Tracked as
            // the next widening for this path.
            bail!(
                "wit_component_emit: async target function `{name}` not yet \
                 handled by the new emit path"
            );
        }
    }
    Ok(())
}

/// Synthesize the adapter world. Imports + exports the target
/// interface (referenced by name in the existing Resolve, so no type
/// recreation), and imports the active tier1 hooks.
fn synthesize_adapter_world_wit(
    target_interface: &str,
    has_before: bool,
    has_after: bool,
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
    wit.push_str("}\n");
    wit
}

// ─── Dispatch core module ──────────────────────────────────────────

/// Per-function dispatch shape: the canonical-ABI signatures from
/// [`Resolve::wasm_signature`] plus a few derived offsets the section
/// emitters need. Stored verbatim so the type / import / wrapper-body
/// phases all read from the same authoritative `WasmSignature`
/// instances rather than re-categorizing them into a parallel enum.
struct FuncDispatch {
    /// Function name — used for the import field name and the
    /// `<iface>#<name>` mangled export name.
    name: String,
    /// `wasm_signature(GuestExport, func)` — drives the wrapper export
    /// type slot and the wrapper body's param-pushing loop.
    export_sig: WasmSignature,
    /// `wasm_signature(GuestImport, func)` — drives the handler import
    /// type slot.
    import_sig: WasmSignature,
    /// Memory offset of this func's hook-call name bytes (the
    /// fully-qualified `<iface>#<name>` string the middleware sees).
    name_offset: i32,
    /// Length of this func's hook-call name in bytes.
    name_len: i32,
    /// Memory offset of this func's retptr scratch buffer. Set iff
    /// `export_sig.retptr` (i.e. the result is multi-flat / string /
    /// list / compound).
    retptr_offset: Option<i32>,
}

impl FuncDispatch {
    /// Single flat result type for the Direct case (single non-retptr
    /// value). `None` for void or retptr-shaped results.
    fn direct_result(&self) -> Option<ValType> {
        if !self.export_sig.retptr && self.export_sig.results.len() == 1 {
            Some(wasm_type_to_val(self.export_sig.results[0]))
        } else {
            None
        }
    }
}

/// Convert a slice of wit-parser [`WasmType`]s to a `Vec<ValType>` for
/// `wasm-encoder` consumption. Used everywhere a section emitter feeds
/// a [`WasmSignature`] into a [`TypeSection`] / [`Function`] sig.
fn val_types(types: &[WasmType]) -> Vec<ValType> {
    types.iter().copied().map(wasm_type_to_val).collect()
}

/// Bytes of scratch memory reserved per function that returns a
/// retptr-shaped result. Sized for the largest result we currently
/// support (string descriptor: `(ptr i32, len i32)` = 8 bytes), padded
/// to 16 for headroom when we widen to records-with-strings / option
/// of-string / multi-i32 compound layouts.
const RETPTR_SCRATCH_BYTES: u32 = 16;

/// Alignment of the retptr scratch slot. i32 boundary covers all the
/// post-name slots [`MemoryLayoutBuilder`] hands out today.
const RETPTR_SCRATCH_ALIGN: u32 = 4;

/// Index of the bump-allocator pointer in the dispatch module's global
/// space. There is exactly one global; `cabi_realloc` is the only
/// reader / writer.
const BUMP_POINTER_GLOBAL: u32 = 0;

/// Type-section index allocations.
struct TypeIndices {
    /// Per-func imported-handler signature.
    handler_ty: Vec<u32>,
    /// Per-func exported-wrapper signature.
    wrapper_ty: Vec<u32>,
    /// Async-lowered hook signature: `(ptr, len) -> packed_status`.
    /// Used by both `before-call` and `after-call`.
    hook_ty: u32,
    init_ty: u32,
    cabi_post_ty: u32,
    cabi_realloc_ty: u32,
    /// Type indices for the async-runtime builtins. Populated only
    /// when at least one tier-1 hook is active.
    async_runtime: Option<AsyncRuntimeTypes>,
}

/// Type indices for the canon-async runtime builtins imported from
/// `$root` (waitable / subtask intrinsics). Reserved as a contiguous
/// block right after the per-func types so the import section can
/// reference them in declaration order.
struct AsyncRuntimeTypes {
    /// `() -> i32` — `[waitable-set-new]`.
    waitable_new_ty: u32,
    /// `(i32, i32) -> ()` — `[waitable-join]`.
    waitable_join_ty: u32,
    /// `(i32, i32) -> i32` — `[waitable-set-wait]`.
    waitable_wait_ty: u32,
    /// `(i32) -> ()` — shared by `[waitable-set-drop]` and
    /// `[subtask-drop]`.
    void_i32_ty: u32,
}

/// Function-index allocations across the core module's combined
/// import + defined function space. Imports come first, then defined
/// functions starting at `wrapper_base`.
struct FuncIndices {
    imp_handler: Vec<u32>,
    imp_before: Option<u32>,
    imp_after: Option<u32>,
    wrapper_base: u32,
    init: u32,
    /// Per-func defined `cabi_post_<iface>#<fn>` index. `Some` iff
    /// that func returns a retptr.
    cabi_post: Vec<Option<u32>>,
    /// `Some` iff at least one string/list is in scope.
    cabi_realloc: Option<u32>,
    /// Async-runtime builtin func indices + the event-record offset
    /// the wait loop writes into. `Some` iff any hook is active.
    async_runtime: Option<AsyncRuntimeFuncs>,
}

/// Function indices and scratch offset for the async-runtime builtins.
/// Captured once at import time so [`emit_wait_loop`] doesn't need to
/// re-derive them per wrapper.
struct AsyncRuntimeFuncs {
    waitable_new: u32,
    waitable_join: u32,
    waitable_wait: u32,
    waitable_drop: u32,
    subtask_drop: u32,
    /// Memory offset of the event-record slot
    /// `[waitable-set-wait]` writes its completion event into.
    event_ptr: i32,
}

/// Build the dispatch core module. Allocations and offsets flow
/// through [`MemoryLayoutBuilder`], [`DispatchIndices`], and
/// [`FunctionIndices`]; this function is purely a phase orchestrator.
///
/// `cabi_realloc` + the bump global are emitted unconditionally —
/// `wit_component::dummy_module` does the same, and the per-call cost
/// (~30 bytes) is far cheaper than maintaining a "does any param /
/// result transitively contain a string or list?" walker.
fn build_dispatch_module(
    resolve: &Resolve,
    world_id: wit_parser::WorldId,
    target_iface: InterfaceId,
    target_interface_name: &str,
    has_before: bool,
    has_after: bool,
) -> Vec<u8> {
    let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
        .functions
        .values()
        .collect();
    // Tier-1 hooks are always async per `wit/tier1/world.wit`, so any
    // active hook means we need the canon-async runtime builtins.
    let needs_async_runtime = has_before || has_after;
    let (per_func, name_blob, event_ptr, bump_start) = compute_func_dispatches(
        resolve,
        target_interface_name,
        &funcs,
        needs_async_runtime,
    );
    let hook_imports = collect_hook_imports(resolve, world_id, has_before, has_after);
    let mut idx = DispatchIndices::new();

    let mut module = Module::new();
    let type_idx = emit_type_section(&mut module, &mut idx, &per_func, &hook_imports);
    let func_idx = emit_imports_section(
        &mut module,
        &mut idx,
        target_interface_name,
        &per_func,
        &type_idx,
        &hook_imports,
        event_ptr,
    );
    let func_idx = emit_function_section(&mut module, &mut idx, &per_func, &type_idx, func_idx);
    emit_memory_and_globals(&mut module, bump_start);
    emit_export_section(&mut module, target_interface_name, &per_func, &func_idx);
    emit_code_section(&mut module, &per_func, &func_idx);
    emit_data_section(&mut module, &name_blob);

    module.finish()
}

/// Phase 1 — derive per-func dispatch shapes, collect name bytes, and
/// reserve memory slots for any retptr-shaped result + the async-event
/// record (when hooks are active). Both per-func sigs come from
/// [`Resolve::wasm_signature`] (no parallel categorization).
///
/// The "name" bytes the data segment carries are the fully-qualified
/// `<iface>#<func>` form. That's the string the middleware sees as
/// the hook's `name: string` argument; matching the legacy emit
/// path's contract keeps existing tier-1 middlewares working
/// unchanged. `event_ptr` is `Some` iff `needs_async_runtime`, so the
/// caller can pass it straight into [`AsyncRuntimeFuncs::event_ptr`].
fn compute_func_dispatches(
    resolve: &Resolve,
    target_interface_name: &str,
    funcs: &[&WitFunction],
    needs_async_runtime: bool,
) -> (Vec<FuncDispatch>, Vec<u8>, Option<i32>, u32) {
    let qualified_names: Vec<String> = funcs
        .iter()
        .map(|f| format!("{target_interface_name}#{}", f.name))
        .collect();
    let total_name_bytes: u32 = qualified_names.iter().map(|n| n.len() as u32).sum();
    let mut layout = MemoryLayoutBuilder::new(total_name_bytes);
    let mut name_blob: Vec<u8> = Vec::with_capacity(total_name_bytes as usize);
    let mut per_func: Vec<FuncDispatch> = Vec::with_capacity(funcs.len());

    for (func, qualified_name) in funcs.iter().zip(qualified_names.iter()) {
        let export_sig = resolve.wasm_signature(AbiVariant::GuestExport, func);
        let import_sig = resolve.wasm_signature(AbiVariant::GuestImport, func);
        let name_offset = layout.alloc_name(qualified_name.len() as u32) as i32;
        name_blob.extend_from_slice(qualified_name.as_bytes());
        let retptr_offset = export_sig.retptr.then(|| {
            layout.alloc_sync_result(RETPTR_SCRATCH_BYTES, RETPTR_SCRATCH_ALIGN) as i32
        });
        per_func.push(FuncDispatch {
            name: func.name.clone(),
            export_sig,
            import_sig,
            name_offset,
            name_len: qualified_name.len() as i32,
            retptr_offset,
        });
    }
    // The event slot must come AFTER per-func name bytes / retptr
    // buffers in the post-name region — it's the same single-cursor
    // ordering the legacy path uses.
    let event_ptr = needs_async_runtime.then(|| layout.alloc_event_slot() as i32);
    let bump_start = layout.finish_as_bump_start();
    (per_func, name_blob, event_ptr, bump_start)
}

/// Map wit-parser's [`WasmType`] to wasm-encoder's [`ValType`]. Pointer
/// / length / pointer-or-i64 collapse to their canonical concrete
/// types on wasm32 (i32 / i64).
fn wasm_type_to_val(wt: WasmType) -> ValType {
    match wt {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => ValType::I32,
        WasmType::I64 | WasmType::PointerOrI64 => ValType::I64,
        WasmType::F32 => ValType::F32,
        WasmType::F64 => ValType::F64,
    }
}

/// One tier-1 hook import resolved against the [`Resolve`]: the
/// canonical-ABI `(module, name)` pair from
/// [`Resolve::wasm_import_name`] and the [`WasmSignature`] from
/// [`Resolve::wasm_signature`] under [`AbiVariant::GuestImportAsync`].
/// Captured once at the entry to [`build_dispatch_module`] so the
/// type / import phases each get the values straight rather than
/// hardcoding `"[async-lower]before-call"` and `(i32,i32) -> i32`.
struct HookImport {
    module: String,
    name: String,
    sig: WasmSignature,
}

/// Resolved hook imports for the active tier-1 hooks. `before` /
/// `after` are populated iff the corresponding hook is active.
struct HookImports {
    before: Option<HookImport>,
    after: Option<HookImport>,
}

impl HookImports {
    fn any(&self) -> bool {
        self.before.is_some() || self.after.is_some()
    }
}

/// Look up the active tier-1 hook interfaces in `resolve` and produce
/// the canonical-ABI core import names + signatures via
/// [`Resolve::wasm_import_name`] / [`Resolve::wasm_signature`].
///
/// Why go through wit-parser instead of hardcoding `"[async-lower]<fn>"`:
/// the `[async-lower]` prefix and the `(i32,i32) -> i32` shape are
/// wit-component contract — sourcing both from the resolve means a
/// future contract bump (or a `wit/tier1/world.wit` signature change)
/// can't silently desync the dispatch module.
fn collect_hook_imports(
    resolve: &Resolve,
    world_id: wit_parser::WorldId,
    has_before: bool,
    has_after: bool,
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
    }
}

/// Phase 2 — emit the type section. Allocates per-func handler-import
/// types, per-func wrapper-export types, the four singletons (hook,
/// init, cabi_post, cabi_realloc), and — when async hooks are active
/// — the canon-async runtime builtin types.
fn emit_type_section(
    module: &mut Module,
    idx: &mut DispatchIndices,
    per_func: &[FuncDispatch],
    hook_imports: &HookImports,
) -> TypeIndices {
    let mut types = TypeSection::new();
    let mut handler_ty: Vec<u32> = Vec::with_capacity(per_func.len());
    let mut wrapper_ty: Vec<u32> = Vec::with_capacity(per_func.len());

    for fd in per_func {
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
    }

    // Hook signature — sourced from the WIT (both hooks share the
    // same `async func(name: string)` shape, so the first available
    // sig wins; `before` first, then `after`). Without a hook
    // active, nothing references this slot, so it falls back to a
    // dummy `() -> ()`.
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

    let async_runtime = hook_imports.any().then(|| {
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
        hook_ty,
        init_ty,
        cabi_post_ty,
        cabi_realloc_ty,
        async_runtime,
    }
}

/// Phase 3 — emit the import section (per-func handlers + active
/// hooks + canon-async runtime builtins from `$root`). Returns a
/// partially-populated [`FuncIndices`] with the import indices filled.
///
/// Hook import names come from [`Resolve::wasm_import_name`] under
/// `Legacy(AsyncCallback)` mangling — that's what produces the
/// `[async-lower]<fn>` field name wit-component expects for an
/// async-declared WIT function. The `[waitable-set-*]` /
/// `[subtask-drop]` builtins are root-level wit-component intrinsics
/// not exposed via wit-parser; their names mirror
/// `dummy_module::push_root_async_intrinsics`.
#[allow(clippy::too_many_arguments)]
fn emit_imports_section(
    module: &mut Module,
    idx: &mut DispatchIndices,
    target_interface: &str,
    per_func: &[FuncDispatch],
    type_idx: &TypeIndices,
    hook_imports: &HookImports,
    event_ptr: Option<i32>,
) -> FuncIndices {
    let mut imports = ImportSection::new();
    let mut imp_handler: Vec<u32> = Vec::with_capacity(per_func.len());
    for (i, fd) in per_func.iter().enumerate() {
        imports.import(
            target_interface,
            fd.name.as_str(),
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

    module.section(&imports);

    FuncIndices {
        imp_handler,
        imp_before,
        imp_after,
        wrapper_base: 0,
        init: 0,
        cabi_post: vec![None; per_func.len()],
        cabi_realloc: None,
        async_runtime,
    }
}

/// Phase 4 — emit the function section (defined-function declarations).
/// `cabi_realloc` is always declared (we always export it; see
/// [`build_dispatch_module`]'s docstring).
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

    for (i, fd) in per_func.iter().enumerate() {
        if fd.export_sig.retptr {
            fsec.function(type_idx.cabi_post_ty);
            func_idx.cabi_post[i] = Some(idx.alloc_func());
        }
    }

    fsec.function(type_idx.cabi_realloc_ty);
    func_idx.cabi_realloc = Some(idx.alloc_func());

    module.section(&fsec);
    func_idx
}

/// Phase 5 — memory + global sections. The bump pointer is always
/// emitted (paired with the always-emitted `cabi_realloc`).
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

/// Phase 6 — emit the export section.
fn emit_export_section(
    module: &mut Module,
    target_interface: &str,
    per_func: &[FuncDispatch],
    func_idx: &FuncIndices,
) {
    let mut exports = ExportSection::new();
    for (i, fd) in per_func.iter().enumerate() {
        let name = format!("{target_interface}#{}", fd.name);
        exports.export(&name, ExportKind::Func, func_idx.wrapper_base + i as u32);
        if let Some(post_idx) = func_idx.cabi_post[i] {
            let post_name = format!("cabi_post_{name}");
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

/// Phase 7 — emit the code section, in the same order the function
/// section declared its defined functions.
fn emit_code_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
    func_idx: &FuncIndices,
) {
    let mut code = CodeSection::new();
    for (i, fd) in per_func.iter().enumerate() {
        emit_wrapper_body(
            &mut code,
            fd,
            func_idx.imp_handler[i],
            func_idx.imp_before,
            func_idx.imp_after,
            func_idx.async_runtime.as_ref(),
        );
    }
    code.function(&empty_function());
    for fd in per_func {
        if fd.export_sig.retptr {
            code.function(&empty_function());
        }
    }
    emit_cabi_realloc(&mut code);
    module.section(&code);
}

/// Phase 8 — emit the active data segment carrying the concatenated
/// function names.
fn emit_data_section(module: &mut Module, name_blob: &[u8]) {
    if name_blob.is_empty() {
        return;
    }
    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), name_blob.iter().copied());
    module.section(&data);
}

/// Emit one wrapper function body. Reads the canon-ABI shape directly
/// off [`FuncDispatch::export_sig`] — `retptr` distinguishes the
/// retptr (compound / multi-flat) case; otherwise `results.len() == 1`
/// is the Direct (single flat) case and `results.is_empty()` is Void.
fn emit_wrapper_body(
    code: &mut CodeSection,
    fd: &FuncDispatch,
    imp_handler: u32,
    imp_before: Option<u32>,
    imp_after: Option<u32>,
    async_runtime: Option<&AsyncRuntimeFuncs>,
) {
    let nparams = fd.export_sig.params.len() as u32;
    let mut locals = FunctionIndices::new(nparams);
    let result_local = fd.direct_result().map(|t| locals.alloc_local(t));
    // The wait loop needs two scratch i32 locals (subtask handle +
    // waitable-set handle). Allocate them up-front when async hooks
    // are active so both before- and after-call can reuse the same
    // slots; harmless when neither hook is present.
    let wait_locals = async_runtime.map(|_| {
        let st = locals.alloc_local(ValType::I32);
        let ws = locals.alloc_local(ValType::I32);
        (st, ws)
    });
    let mut f = Function::new_with_locals_types(locals.into_locals());

    if let Some(idx) = imp_before {
        emit_hook_call(&mut f, fd, idx, async_runtime, wait_locals);
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

/// Call an async-lowered hook with `(name_ptr, name_len)` and await
/// the returned packed subtask handle via [`emit_wait_loop`]. Both
/// `async_runtime` and `wait_locals` must be `Some` whenever a hook
/// is active — `emit_imports_section` and the local-allocation block
/// in [`emit_wrapper_body`] guarantee that.
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

/// Await a packed return value from `canon lower async` currently
/// stored in local `st`. Mirrors the legacy emit path's
/// `dispatch::emit_wait_loop`, factored to read its function indices
/// out of [`AsyncRuntimeFuncs`].
///
/// `canon lower async` returns a packed i32: low 4 bits are the
/// Status tag (`Returned=2` means sync-done; `Started=1` means
/// pending) and the upper 28 bits hold the raw subtask handle (`0`
/// when sync-done). After this helper:
///
/// - Local `st` holds the raw subtask handle (`packed >> 4`).
/// - If the handle is nonzero, a fresh waitable-set has been
///   created, the subtask joined into it, the wait completed, and
///   both the subtask and the waitable-set dropped.
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

/// A function with no locals and an empty body (just `end`). Used for
/// `_initialize` and the no-op `cabi_post_*` exports.
fn empty_function() -> Function {
    let mut f = Function::new_with_locals_types([]);
    f.instructions().end();
    f
}

/// Bump-allocator `cabi_realloc`. Signature
/// `(old_ptr, old_size, align, new_size) -> new_ptr`. Treats every
/// call as a fresh alloc, rounding the bump pointer up to `align` and
/// advancing by `new_size`.
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
