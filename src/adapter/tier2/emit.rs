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
    Function as WitFunction, InterfaceId, LiftLowerAbi, ManglingAndAbi, Resolve, WasmExport,
    WasmExportKind, WasmImport, WorldId, WorldItem, WorldKey,
};

use super::super::abi::canon_async;
use super::super::abi::emit::{
    empty_function, emit_cabi_realloc, emit_memory_and_globals, val_types, EXPORT_CABI_REALLOC,
    EXPORT_INITIALIZE, EXPORT_MEMORY,
};
use super::super::resolve::{decode_input_resolve, find_target_interface};

const TIER2_ADAPTER_WORLD_PACKAGE: &str = "splicer:adapter-tier2";
const TIER2_ADAPTER_WORLD_NAME: &str = "adapter";

/// Generate a tier-2 adapter component.
pub(crate) fn build_tier2_adapter(
    target_interface: &str,
    has_before: bool,
    split_bytes: &[u8],
    common_wit: &str,
    tier2_wit: &str,
) -> Result<Vec<u8>> {
    if !has_before {
        bail!(
            "tier-2 adapter generation currently requires the middleware to export \
             `splicer:tier2/before` — `after`-only and `trap`-only middleware are \
             planned for a follow-up slice."
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
            &synthesize_adapter_world_wit(target_interface),
        )
        .context("parse synthesized tier-2 adapter world WIT")?;
    let world_id = resolve
        .select_world(&[world_pkg], Some(TIER2_ADAPTER_WORLD_NAME))
        .context("select tier-2 adapter world")?;

    let mut core_module =
        build_dispatch_module(&resolve, world_id, target_iface, target_interface)?;
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
    for (name, func) in &iface.functions {
        if func.kind.is_async() {
            bail!(
                "tier-2 first slice doesn't yet support async target functions; \
                 `{name}` is async"
            );
        }
    }
    Ok(())
}

/// Synthesize the tier-2 adapter world.
fn synthesize_adapter_world_wit(target_interface: &str) -> String {
    use crate::contract::{versioned_interface, TIER2_BEFORE, TIER2_VERSION};
    let mut wit = format!(
        "package {TIER2_ADAPTER_WORLD_PACKAGE};\n\nworld {TIER2_ADAPTER_WORLD_NAME} {{\n"
    );
    wit.push_str(&format!("    import {target_interface};\n"));
    wit.push_str(&format!("    export {target_interface};\n"));
    wit.push_str(&format!(
        "    import {};\n",
        versioned_interface(TIER2_BEFORE, TIER2_VERSION)
    ));
    wit.push_str("}\n");
    wit
}

// ─── Dispatch core module ──────────────────────────────────────────

/// Per-function dispatch shape. The on-call hook is called with the
/// `(iface, fn)` strings shared from the same name blob — the
/// interface name is allocated once and reused; each function's name
/// gets its own slot.
struct FuncDispatch {
    import_module: String,
    import_field: String,
    export_name: String,
    sig: WasmSignature,
    needs_cabi_post: bool,
    /// Byte offset of the function name within the data segment.
    fn_name_offset: i32,
    fn_name_len: i32,
}

/// Byte size of the on-call hook's indirect-params buffer. Matches
/// the canonical-ABI lower of `record { call-id, list<field> }`:
/// two strings (8 B each) + a list pointer/len pair (8 B) = 24 B,
/// 4-aligned. Reserved as a static slot in the adapter's memory
/// layout; populated per-call by [`emit_populate_hook_params`].
///
/// `on-call`'s 6 flat i32s exceed canon-lower-async's
/// MAX_FLAT_ASYNC_PARAMS = 4, so the canonical ABI lowers them into
/// a memory record and hands canon-lower-async a single pointer.
const HOOK_PARAMS_SIZE: u32 = 24;

/// Emit wasm that writes the call-id (interface + function name
/// pointers/lengths) and an empty `list<field>` (ptr=0, len=0) into
/// the indirect-params buffer at `base_ptr`. Field offsets match
/// the canonical-ABI lower of the on-call params record:
///
/// ```text
/// offset  0: interface-name.ptr
/// offset  4: interface-name.len
/// offset  8: function-name.ptr
/// offset 12: function-name.len
/// offset 16: args.list.ptr
/// offset 20: args.list.len
/// ```
fn emit_populate_hook_params(
    f: &mut Function,
    base_ptr: i32,
    iface_name_offset: i32,
    iface_name_len: i32,
    fn_name_offset: i32,
    fn_name_len: i32,
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
    store_i32(0, iface_name_offset);
    store_i32(4, iface_name_len);
    store_i32(8, fn_name_offset);
    store_i32(12, fn_name_len);
    store_i32(16, 0); // args list ptr — empty until cells wire-in (Phase 2-2b)
    store_i32(20, 0); // args list len
}

fn build_dispatch_module(
    resolve: &Resolve,
    world_id: WorldId,
    target_iface: InterfaceId,
    target_interface_name: &str,
) -> Result<Vec<u8>> {
    let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
        .functions
        .values()
        .collect();

    // ── Name blob layout ─────────────────────────────────────────
    // [interface_name][fn_name_0][fn_name_1]...[event_slot pad]
    // The interface name is shared across all funcs (same pointer
    // and length); each function name gets its own slot. Event slot
    // (8 bytes for waitable-set-wait's event record) lands right
    // after, 4-aligned.

    let iface_name_offset: i32 = 0;
    let iface_name_len = target_interface_name.len() as i32;

    let target_world_key = WorldKey::Interface(target_iface);
    let sync_mangling = ManglingAndAbi::Legacy(LiftLowerAbi::Sync);

    let mut name_blob: Vec<u8> = target_interface_name.as_bytes().to_vec();
    let mut per_func: Vec<FuncDispatch> = Vec::with_capacity(funcs.len());

    for func in &funcs {
        let fn_name_offset = name_blob.len() as i32;
        let fn_name_len = func.name.len() as i32;
        name_blob.extend_from_slice(func.name.as_bytes());

        let (import_module, import_field) = resolve.wasm_import_name(
            sync_mangling,
            WasmImport::Func {
                interface: Some(&target_world_key),
                func,
            },
        );
        let export_name = resolve.wasm_export_name(
            sync_mangling,
            WasmExport::Func {
                interface: Some(&target_world_key),
                func,
                kind: WasmExportKind::Normal,
            },
        );
        let sig = resolve.wasm_signature(AbiVariant::GuestExport, func);
        let needs_cabi_post = sig.retptr;

        per_func.push(FuncDispatch {
            import_module,
            import_field,
            export_name,
            sig,
            needs_cabi_post,
            fn_name_offset,
            fn_name_len,
        });
    }

    // Memory layout: name blob, then event slot (8 bytes for the
    // waitable-set-wait event record, 4-aligned), then the on-call
    // hook params buffer (24 bytes, 4-aligned), then bump_start.
    let event_ptr = align_up(name_blob.len() as u32, 4) as i32;
    let hook_params_ptr = (event_ptr as u32) + 8;
    let bump_start = hook_params_ptr + HOOK_PARAMS_SIZE;

    // Find the on-call hook function (its WasmImport name + sig come
    // from `Resolve::wasm_import_name` / `wasm_signature` with the
    // async-callback mangling, same as tier-1's hooks).
    let hook = find_on_call_hook(resolve, world_id)?;

    let mut module = Module::new();
    let type_idx = emit_type_section(&mut module, &per_func, &hook.sig);
    let func_idx = emit_imports_and_funcs(&mut module, &per_func, &type_idx, &hook, event_ptr);
    emit_memory_and_globals(&mut module, bump_start);
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
        iface_name_offset,
        iface_name_len,
        hook_params_ptr as i32,
    );
    emit_data_section(&mut module, &name_blob);
    Ok(module.finish())
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
}

fn find_on_call_hook(resolve: &Resolve, world_id: WorldId) -> Result<HookImport> {
    use crate::contract::{TIER2_BEFORE, TIER2_VERSION};
    let target_iface = format!("{TIER2_BEFORE}@{TIER2_VERSION}");
    let world = &resolve.worlds[world_id];
    for (key, item) in &world.imports {
        if let WorldItem::Interface { id, .. } = item {
            if resolve.id_of(*id).as_deref() != Some(&target_iface) {
                continue;
            }
            let func = resolve.interfaces[*id]
                .functions
                .values()
                .next()
                .ok_or_else(|| anyhow!("splicer:tier2/before has no functions"))?;
            let (module, name) = resolve.wasm_import_name(
                ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback),
                WasmImport::Func {
                    interface: Some(key),
                    func,
                },
            );
            let sig = resolve.wasm_signature(AbiVariant::GuestImportAsync, func);
            return Ok(HookImport { module, name, sig });
        }
    }
    bail!("synthesized adapter world is missing import of {target_iface}");
}

// ─── Section emission ─────────────────────────────────────────────

struct TypeIndices {
    handler_ty: Vec<u32>,
    wrapper_ty: Vec<u32>,
    hook_ty: u32,
    init_ty: u32,
    cabi_post_ty: u32,
    cabi_realloc_ty: u32,
    async_types: canon_async::AsyncTypes,
}

fn emit_type_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
    hook_sig: &WasmSignature,
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
                val_types(&fd.sig.params),
                val_types(&fd.sig.results),
            )
        })
        .collect();
    let wrapper_ty: Vec<u32> = per_func
        .iter()
        .map(|fd| {
            alloc_one(
                &mut types,
                val_types(&fd.sig.params),
                val_types(&fd.sig.results),
            )
        })
        .collect();

    let hook_ty = alloc_one(
        &mut types,
        val_types(&hook_sig.params),
        val_types(&hook_sig.results),
    );
    let init_ty = alloc_one(&mut types, vec![], vec![]);
    let cabi_post_ty = alloc_one(&mut types, vec![ValType::I32], vec![]);
    let cabi_realloc_ty = alloc_one(
        &mut types,
        vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        vec![ValType::I32],
    );

    let async_types = canon_async::emit_types(&mut types, || {
        let i = next_ty;
        next_ty += 1;
        i
    });

    module.section(&types);
    TypeIndices {
        handler_ty,
        wrapper_ty,
        hook_ty,
        init_ty,
        cabi_post_ty,
        cabi_realloc_ty,
        async_types,
    }
}

struct FuncIndices {
    handler_imp_base: u32,
    hook_idx: u32,
    async_funcs: canon_async::AsyncFuncs,
    wrapper_base: u32,
    init_idx: u32,
    cabi_realloc_idx: u32,
}

fn emit_imports_and_funcs(
    module: &mut Module,
    per_func: &[FuncDispatch],
    ty: &TypeIndices,
    hook: &HookImport,
    event_ptr: i32,
) -> FuncIndices {
    let mut imports = ImportSection::new();
    let mut next_imp: u32 = 0;

    let handler_imp_base = next_imp;
    for (fd, &fty) in per_func.iter().zip(&ty.handler_ty) {
        imports.import(&fd.import_module, &fd.import_field, EntityType::Function(fty));
        next_imp += 1;
    }

    imports.import(&hook.module, &hook.name, EntityType::Function(ty.hook_ty));
    let hook_idx = next_imp;
    next_imp += 1;

    let async_funcs = canon_async::import_intrinsics(&mut imports, &ty.async_types, event_ptr, || {
        let i = next_imp;
        next_imp += 1;
        i
    });

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
        hook_idx,
        async_funcs,
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
    iface_name_offset: i32,
    iface_name_len: i32,
    hook_params_ptr: i32,
) {
    let async_funcs = &func_idx.async_funcs;
    let mut code = CodeSection::new();
    for (i, fd) in per_func.iter().enumerate() {
        // Two extra locals: $st (i32, packed status from on-call) +
        // $ws (i32, waitable-set handle for the wait loop).
        let nparams = fd.sig.params.len() as u32;
        let st = nparams;
        let ws = nparams + 1;
        let mut f = Function::new([(2, ValType::I32)]);

        // Populate the indirect-params buffer + call on-call (the
        // canon-lower-async signature is `(params_ptr) -> i32 packed
        // status` because `on-call`'s 6 flat params overflow
        // MAX_FLAT_ASYNC_PARAMS = 4).
        emit_populate_hook_params(
            &mut f,
            hook_params_ptr,
            iface_name_offset,
            iface_name_len,
            fd.fn_name_offset,
            fd.fn_name_len,
        );
        f.instructions().i32_const(hook_params_ptr);
        f.instructions().call(func_idx.hook_idx);
        f.instructions().local_set(st);

        canon_async::emit_wait_loop(&mut f, st, ws, async_funcs);

        // Pass-through to handler with original args.
        for j in 0..nparams {
            f.instructions().local_get(j);
        }
        f.instructions().call(func_idx.handler_imp_base + i as u32);
        f.instructions().end();
        code.function(&f);
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

fn emit_data_section(module: &mut Module, name_blob: &[u8]) {
    if name_blob.is_empty() {
        return;
    }
    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), name_blob.iter().copied());
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
