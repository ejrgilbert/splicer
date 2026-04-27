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
//! Today the emitter handles only sync funcs whose result is either
//! void, a single direct flat value, or retptr-shaped (string / list /
//! complex compound). Async funcs and resource constructors / methods
//! / statics fall through to the legacy emit path until those
//! widenings land.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashSet;
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, GlobalSection, GlobalType, ImportSection, MemorySection, MemoryType,
    Module, TypeSection, ValType,
};
use wit_component::{ComponentEncoder, DecodedWasm, StringEncoding, decode, embed_component_metadata};
use wit_parser::abi::{AbiVariant, WasmSignature, WasmType};
use wit_parser::{
    Function as WitFunction, InterfaceId, Resolve, Type, TypeDefKind, TypeId,
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

    let mut core_module =
        build_dispatch_module(&resolve, target_iface, target_interface, has_before, has_after);
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
/// widening step (async, resource constructors / methods / statics)
/// removes one of these constraints.
fn require_supported_case(resolve: &Resolve, target_iface: InterfaceId) -> Result<()> {
    let iface = &resolve.interfaces[target_iface];
    if iface.functions.is_empty() {
        bail!("wit_component_emit: interface has no functions");
    }
    for (name, func) in &iface.functions {
        if func.kind.is_async() {
            bail!("wit_component_emit: async function `{name}` not yet handled");
        }
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

/// Per-function dispatch shape, built from a wit-parser [`WasmSignature`]
/// plus the linear-memory layout. Computed by [`compute_func_dispatches`]
/// before any sections are emitted, so each section emitter knows the
/// indices and offsets it needs without re-deriving them.
struct FuncDispatch {
    /// Function name — used for the import name, the export-side
    /// `<iface>#<name>` mangled name, and the hook-call name argument.
    name: String,
    /// Wrapper-export param signature in core wasm types — the
    /// canonical ABI flat lowering of every WIT param, concatenated.
    /// Mirrors `WasmSignature::params`.
    param_flats: Vec<ValType>,
    /// Result lowering. Drives both wrapper-export and import-handler
    /// signatures.
    result: ResultLowering,
    /// Memory offset of this func's name bytes, for hook calls.
    name_offset: i32,
    /// Length of this func's name in bytes.
    name_len: i32,
    /// Memory offset of this func's retptr scratch buffer. Set only
    /// when [`ResultLowering::Retptr`].
    retptr_offset: Option<i32>,
}

#[derive(Clone)]
enum ResultLowering {
    /// Function returns nothing.
    Void,
    /// Function returns one flat value directly (e.g. `u32`, `s64`).
    /// The contained `ValType` IS the wrapper's lowered result type.
    Direct(ValType),
    /// Function's result has 2+ flat values (string, list, complex
    /// compound). The wrapper's lowered result is a single `i32`
    /// retptr; the import takes an extra `i32` retptr param and writes
    /// the actual flat values to memory at that address.
    Retptr,
}

impl ResultLowering {
    /// Derive from a wit-parser [`WasmSignature`] + the function's
    /// declared result type.
    fn from_signature(func: &WitFunction, sig: &WasmSignature) -> Self {
        match (func.result.is_some(), sig.retptr) {
            (false, _) => ResultLowering::Void,
            (true, true) => ResultLowering::Retptr,
            (true, false) => {
                debug_assert_eq!(
                    sig.results.len(),
                    1,
                    "non-retptr result should flatten to exactly one core value"
                );
                ResultLowering::Direct(wasm_type_to_val(sig.results[0]))
            }
        }
    }
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
    hook_ty: u32,
    init_ty: u32,
    cabi_post_ty: u32,
    cabi_realloc_ty: u32,
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
}

/// Build the dispatch core module. Allocations and offsets flow
/// through [`MemoryLayoutBuilder`], [`DispatchIndices`], and
/// [`FunctionIndices`]; this function is purely a phase orchestrator.
fn build_dispatch_module(
    resolve: &Resolve,
    target_iface: InterfaceId,
    target_interface_name: &str,
    has_before: bool,
    has_after: bool,
) -> Vec<u8> {
    let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
        .functions
        .values()
        .collect();
    let needs_realloc = funcs
        .iter()
        .any(|f| function_uses_caller_alloc(resolve, f));
    let (per_func, name_blob, bump_start) = compute_func_dispatches(resolve, &funcs);
    let mut idx = DispatchIndices::new();

    let mut module = Module::new();
    let type_idx = emit_type_section(&mut module, &mut idx, &per_func);
    let func_idx = emit_imports_section(
        &mut module,
        &mut idx,
        target_interface_name,
        &per_func,
        &type_idx,
        has_before,
        has_after,
    );
    let func_idx = emit_function_section(
        &mut module,
        &mut idx,
        &per_func,
        &type_idx,
        func_idx,
        needs_realloc,
    );
    emit_memory_and_globals(&mut module, bump_start, needs_realloc);
    emit_export_section(&mut module, target_interface_name, &per_func, &func_idx);
    emit_code_section(&mut module, &per_func, &func_idx);
    emit_data_section(&mut module, &name_blob);

    module.finish()
}

/// Phase 1 — derive per-func dispatch shapes, collect name bytes, and
/// reserve memory slots for any retptr-shaped result. Each
/// [`FuncDispatch`] reads its lowering from
/// [`Resolve::wasm_signature`] in the [`AbiVariant::GuestExport`]
/// flavor — the wrapper IS the export-side guest function from
/// wit-component's perspective.
fn compute_func_dispatches(
    resolve: &Resolve,
    funcs: &[&WitFunction],
) -> (Vec<FuncDispatch>, Vec<u8>, u32) {
    let total_name_bytes: u32 = funcs.iter().map(|f| f.name.len() as u32).sum();
    let mut layout = MemoryLayoutBuilder::new(total_name_bytes);
    let mut name_blob: Vec<u8> = Vec::with_capacity(total_name_bytes as usize);
    let mut per_func: Vec<FuncDispatch> = Vec::with_capacity(funcs.len());

    for func in funcs {
        let sig = resolve.wasm_signature(AbiVariant::GuestExport, func);
        let name_offset = layout.alloc_name(func.name.len() as u32) as i32;
        name_blob.extend_from_slice(func.name.as_bytes());
        let result = ResultLowering::from_signature(func, &sig);
        let retptr_offset = matches!(result, ResultLowering::Retptr).then(|| {
            layout.alloc_sync_result(RETPTR_SCRATCH_BYTES, RETPTR_SCRATCH_ALIGN) as i32
        });
        let param_flats: Vec<ValType> = sig.params.iter().copied().map(wasm_type_to_val).collect();
        per_func.push(FuncDispatch {
            name: func.name.clone(),
            param_flats,
            result,
            name_offset,
            name_len: func.name.len() as i32,
            retptr_offset,
        });
    }
    let bump_start = layout.finish_as_bump_start();
    (per_func, name_blob, bump_start)
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

/// Whether any param or result of `func` carries a type whose
/// canonical-ABI lowering goes through caller-allocated linear memory
/// (currently `string` and `list<_>`). Drives whether the dispatch
/// module needs to export `cabi_realloc` + the bump-pointer global.
fn function_uses_caller_alloc(resolve: &Resolve, func: &WitFunction) -> bool {
    let mut seen = HashSet::new();
    for param in &func.params {
        if type_uses_caller_alloc(resolve, &param.ty, &mut seen) {
            return true;
        }
    }
    if let Some(ty) = &func.result {
        if type_uses_caller_alloc(resolve, ty, &mut seen) {
            return true;
        }
    }
    false
}

/// Recursive helper for [`function_uses_caller_alloc`]. Returns true
/// when the type at `ty` is itself caller-allocated, OR contains one
/// (e.g. `record { name: string }`, `list<list<u32>>`).
fn type_uses_caller_alloc(resolve: &Resolve, ty: &Type, seen: &mut HashSet<TypeId>) -> bool {
    match ty {
        Type::String => true,
        Type::Id(id) => {
            if !seen.insert(*id) {
                return false;
            }
            let td = &resolve.types[*id];
            match &td.kind {
                TypeDefKind::List(_) | TypeDefKind::FixedLengthList(_, _) => true,
                TypeDefKind::Type(inner) => type_uses_caller_alloc(resolve, inner, seen),
                TypeDefKind::Option(inner) => type_uses_caller_alloc(resolve, inner, seen),
                TypeDefKind::Record(r) => r
                    .fields
                    .iter()
                    .any(|f| type_uses_caller_alloc(resolve, &f.ty, seen)),
                TypeDefKind::Tuple(t) => t
                    .types
                    .iter()
                    .any(|t| type_uses_caller_alloc(resolve, t, seen)),
                TypeDefKind::Variant(v) => v.cases.iter().any(|c| {
                    c.ty.as_ref()
                        .map(|t| type_uses_caller_alloc(resolve, t, seen))
                        .unwrap_or(false)
                }),
                TypeDefKind::Result(r) => {
                    r.ok.as_ref()
                        .map(|t| type_uses_caller_alloc(resolve, t, seen))
                        .unwrap_or(false)
                        || r.err
                            .as_ref()
                            .map(|t| type_uses_caller_alloc(resolve, t, seen))
                            .unwrap_or(false)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// Phase 2 — emit the type section. Allocates per-func handler-import
/// types, per-func wrapper-export types, and the four singletons
/// (hook, init, cabi_post, cabi_realloc).
fn emit_type_section(
    module: &mut Module,
    idx: &mut DispatchIndices,
    per_func: &[FuncDispatch],
) -> TypeIndices {
    let mut types = TypeSection::new();
    let mut handler_ty: Vec<u32> = Vec::with_capacity(per_func.len());
    let mut wrapper_ty: Vec<u32> = Vec::with_capacity(per_func.len());

    for fd in per_func {
        let (handler_params, handler_result) = handler_signature(fd);
        types.ty().function(handler_params, handler_result);
        handler_ty.push(idx.alloc_ty());

        let (wrapper_params, wrapper_result) = wrapper_signature(fd);
        types.ty().function(wrapper_params, wrapper_result);
        wrapper_ty.push(idx.alloc_ty());
    }

    types.ty().function([ValType::I32, ValType::I32], []);
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

    module.section(&types);
    TypeIndices {
        handler_ty,
        wrapper_ty,
        hook_ty,
        init_ty,
        cabi_post_ty,
        cabi_realloc_ty,
    }
}

/// Lowered handler-import signature: param flats, plus a retptr `i32`
/// when the result is retptr-shaped; result is the Direct value or
/// empty.
fn handler_signature(fd: &FuncDispatch) -> (Vec<ValType>, Vec<ValType>) {
    let mut params = fd.param_flats.clone();
    let result = match &fd.result {
        ResultLowering::Void => vec![],
        ResultLowering::Direct(t) => vec![*t],
        ResultLowering::Retptr => {
            params.push(ValType::I32);
            vec![]
        }
    };
    (params, result)
}

/// Lowered wrapper-export signature: param flats; result is the Direct
/// value, or `i32` retptr, or empty.
fn wrapper_signature(fd: &FuncDispatch) -> (Vec<ValType>, Vec<ValType>) {
    let result = match &fd.result {
        ResultLowering::Void => vec![],
        ResultLowering::Direct(t) => vec![*t],
        ResultLowering::Retptr => vec![ValType::I32],
    };
    (fd.param_flats.clone(), result)
}

/// Phase 3 — emit the import section (per-func handlers + active
/// hooks). Returns a partially-populated [`FuncIndices`] with the
/// import indices filled.
fn emit_imports_section(
    module: &mut Module,
    idx: &mut DispatchIndices,
    target_interface: &str,
    per_func: &[FuncDispatch],
    type_idx: &TypeIndices,
    has_before: bool,
    has_after: bool,
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
    let imp_before = has_before.then(|| {
        imports.import(
            "splicer:tier1/before@0.1.0",
            "before-call",
            EntityType::Function(type_idx.hook_ty),
        );
        idx.alloc_func()
    });
    let imp_after = has_after.then(|| {
        imports.import(
            "splicer:tier1/after@0.1.0",
            "after-call",
            EntityType::Function(type_idx.hook_ty),
        );
        idx.alloc_func()
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
    }
}

/// Phase 4 — emit the function section (defined-function declarations).
fn emit_function_section(
    module: &mut Module,
    idx: &mut DispatchIndices,
    per_func: &[FuncDispatch],
    type_idx: &TypeIndices,
    mut func_idx: FuncIndices,
    needs_realloc: bool,
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
        if matches!(fd.result, ResultLowering::Retptr) {
            fsec.function(type_idx.cabi_post_ty);
            func_idx.cabi_post[i] = Some(idx.alloc_func());
        }
    }

    if needs_realloc {
        fsec.function(type_idx.cabi_realloc_ty);
        func_idx.cabi_realloc = Some(idx.alloc_func());
    }

    module.section(&fsec);
    func_idx
}

/// Phase 5 — memory + global sections.
fn emit_memory_and_globals(module: &mut Module, bump_start: u32, needs_realloc: bool) {
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memory);

    if needs_realloc {
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
    if let Some(idx) = func_idx.cabi_realloc {
        exports.export("cabi_realloc", ExportKind::Func, idx);
    }
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
        );
    }
    code.function(&empty_function());
    for fd in per_func {
        if matches!(fd.result, ResultLowering::Retptr) {
            code.function(&empty_function());
        }
    }
    if func_idx.cabi_realloc.is_some() {
        emit_cabi_realloc(&mut code);
    }
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

/// Emit one wrapper function body.
fn emit_wrapper_body(
    code: &mut CodeSection,
    fd: &FuncDispatch,
    imp_handler: u32,
    imp_before: Option<u32>,
    imp_after: Option<u32>,
) {
    let nparams = fd.param_flats.len() as u32;
    let mut locals = FunctionIndices::new(nparams);
    let result_local = match &fd.result {
        ResultLowering::Direct(t) => Some(locals.alloc_local(*t)),
        ResultLowering::Void | ResultLowering::Retptr => None,
    };
    let mut f = Function::new_with_locals_types(locals.into_locals());

    if let Some(idx) = imp_before {
        push_hook_call(&mut f, fd, idx);
    }
    for p in 0..nparams {
        f.instructions().local_get(p);
    }
    if matches!(fd.result, ResultLowering::Retptr) {
        f.instructions()
            .i32_const(fd.retptr_offset.expect("retptr_offset set"));
    }
    f.instructions().call(imp_handler);
    if let Some(local) = result_local {
        f.instructions().local_set(local);
    }
    if let Some(idx) = imp_after {
        push_hook_call(&mut f, fd, idx);
    }
    match &fd.result {
        ResultLowering::Void => {}
        ResultLowering::Direct(_) => {
            f.instructions()
                .local_get(result_local.expect("Direct → local_set above"));
        }
        ResultLowering::Retptr => {
            f.instructions()
                .i32_const(fd.retptr_offset.expect("retptr_offset set"));
        }
    }
    f.instructions().end();
    code.function(&f);
}

/// Push `(name_ptr, name_len, call hook_idx)` into the wrapper body.
fn push_hook_call(f: &mut Function, fd: &FuncDispatch, hook_idx: u32) {
    f.instructions().i32_const(fd.name_offset);
    f.instructions().i32_const(fd.name_len);
    f.instructions().call(hook_idx);
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
