//! WIT-level adapter emission. Replaces the manual component
//! construction in [`super::component`] by delegating all type
//! encoding, canon-lift/lower wiring, instance-type construction, and
//! resource handling to `wit_component::ComponentEncoder`.
//!
//! Splicer's responsibilities here reduce to:
//! 1. Building a `wit_parser::Resolve` that contains the target
//!    interface (decoded from the split) and the tier1 hook imports
//!    (from `wit/tier1/world.wit`).
//! 2. Synthesizing an "adapter" world inside that Resolve that
//!    imports + exports the target interface and imports the active
//!    tier1 hooks.
//! 3. Emitting a dispatch core module whose imports / exports match
//!    the naming contract wit-component expects (verified against the
//!    spike at `examples/wit_component_spike.rs`).
//! 4. Embedding component-type metadata onto that core module and
//!    handing it to `ComponentEncoder` to produce the adapter
//!    component bytes.
//!
//! Today the emitter handles only the simplest case (single primitive
//! function, optional before/after hooks). Will widen incrementally
//! to cover all the cases the legacy emitter handles, then the legacy
//! path goes away.

use anyhow::{Context, Result, bail};
use cviz::model::{InterfaceType, TypeArena, ValueType, ValueTypeId};
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, GlobalSection, GlobalType, ImportSection, MemorySection, MemoryType,
    Module, TypeSection, ValType,
};
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

use super::mem_layout::MemoryLayoutBuilder;
use crate::adapter::func::AdapterFunc;
use crate::adapter::indices::{DispatchIndices, FunctionIndices};

/// Generate the adapter component bytes via the WIT-level emit path.
///
/// `tier1_world_wit` is the contents of `wit/tier1/world.wit` — passed
/// in (rather than read from disk inside this function) so callers
/// can choose between `include_str!` for shipping and a real file
/// read for tests / examples.
///
/// `split_bytes` is the raw bytes of the input split component;
/// `wit_component::decode` extracts its WIT and we look up
/// `target_interface` inside the resulting Resolve.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_adapter_via_wit_component(
    target_interface: &str,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    _has_blocking: bool,
    arena: &TypeArena,
    iface_ty: &InterfaceType,
    split_bytes: &[u8],
    tier1_world_wit: &str,
) -> Result<Vec<u8>> {
    // ── Today's scope guard ──────────────────────────────────────────
    // Widen this guard as we extend coverage. Anything outside the
    // guarded set should fall through to the legacy emit path.
    require_simple_case(funcs, iface_ty, arena)?;

    // ── 1. Build the Resolve ─────────────────────────────────────────
    let mut resolve = Resolve::default();
    resolve
        .push_str("splicer-tier1.wit", tier1_world_wit)
        .context("parse tier1 WIT")?;

    // For the simplest case the split's WIT is consumed via decode.
    // Once the legacy path is gone we can drop the bytes argument and
    // construct the WIT entirely from cviz's data — but using the
    // actual split keeps fidelity with whatever wit-bindgen produced.
    let decoded =
        wit_component::decode(split_bytes).context("decode split component WIT")?;
    let _ = decoded; // see TODO below

    // TODO(rewrite): once `decoded` is plumbed through, locate the
    // target interface by name and re-target the adapter world's
    // import/export against it. For the minimum-viable spike-equivalent
    // case below we synthesize a fresh interface from `funcs` instead.
    let synthetic_pkg = synthesize_target_package(target_interface, funcs, arena, iface_ty)?;
    let pkg_id = resolve
        .push_str("splicer-target.wit", &synthetic_pkg)
        .context("parse synthesized target WIT")?;

    let world_wit =
        synthesize_adapter_world_wit(target_interface, has_before, has_after);
    let _world_pkg_id = resolve
        .push_str("splicer-adapter.wit", &world_wit)
        .context("parse synthesized adapter world WIT")?;
    let _ = pkg_id;

    let world_id = resolve
        .select_world(&[_world_pkg_id], Some("adapter"))
        .context("select adapter world")?;

    // ── 2. Build the dispatch core module ────────────────────────────
    let mut core_module =
        build_simple_dispatch(target_interface, funcs, has_before, has_after, arena)?;
    embed_component_metadata(&mut core_module, &resolve, world_id, StringEncoding::UTF8)
        .context("embed_component_metadata")?;

    // ── 3. Encode → component ────────────────────────────────────────
    let bytes = ComponentEncoder::default()
        .validate(true)
        .module(&core_module)
        .context("ComponentEncoder::module")?
        .encode()
        .context("ComponentEncoder::encode")?;
    Ok(bytes)
}

/// Bail out of the new path for any case it doesn't yet handle. Keeps
/// the rewrite incremental: each widening step removes a constraint
/// from this function.
fn require_simple_case(
    funcs: &[AdapterFunc],
    iface_ty: &InterfaceType,
    arena: &TypeArena,
) -> Result<()> {
    if funcs.is_empty() {
        bail!("wit_component_emit: interface has no functions");
    }
    for f in funcs {
        if f.is_async {
            bail!("wit_component_emit: async functions not yet handled");
        }
        for &id in &f.param_type_ids {
            if !is_emittable_value(arena, id) {
                bail!(
                    "wit_component_emit: param type {:?} not yet handled",
                    arena.lookup_val(id)
                );
            }
        }
        if let Some(rid) = f.result_type_id {
            if !is_emittable_value(arena, rid) {
                bail!(
                    "wit_component_emit: result type {:?} not yet handled",
                    arena.lookup_val(rid)
                );
            }
        }
    }
    if let InterfaceType::Instance(inst) = iface_ty {
        if !inst.type_exports.is_empty() {
            bail!(
                "wit_component_emit: type_exports (resources, named compounds) \
                 not yet handled"
            );
        }
    } else {
        bail!("wit_component_emit: bare-function interfaces not handled");
    }
    Ok(())
}

/// Value types the new emit path can lower today: primitives + strings.
/// Compounds, lists, resources, etc. each unlock as their handling
/// lands.
fn is_emittable_value(arena: &TypeArena, id: cviz::model::ValueTypeId) -> bool {
    matches!(
        arena.lookup_val(id),
        ValueType::Bool
            | ValueType::U8
            | ValueType::S8
            | ValueType::U16
            | ValueType::S16
            | ValueType::U32
            | ValueType::S32
            | ValueType::U64
            | ValueType::S64
            | ValueType::F32
            | ValueType::F64
            | ValueType::Char
            | ValueType::String
    )
}

/// Build a minimal WIT package for the target interface from cviz's
/// type info. Limited to primitive params/results in the simplest-case
/// guard — extends with compounds / resources as the guard widens.
fn synthesize_target_package(
    target_interface: &str,
    funcs: &[AdapterFunc],
    arena: &TypeArena,
    _iface_ty: &InterfaceType,
) -> Result<String> {
    // `target_interface` is "<ns>:<pkg>/<iface>[@<version>]". We need
    // to split it back into pieces to build the WIT package header.
    let parts = split_target(target_interface)?;
    let mut wit = format!(
        "package {hdr};\n\ninterface {iface} {{\n",
        hdr = parts.pkg_header(),
        iface = parts.iface,
    );
    for func in funcs {
        let params: Vec<String> = func
            .param_names
            .iter()
            .zip(func.param_type_ids.iter())
            .map(|(n, &id)| format!("{n}: {}", primitive_wit_type(arena, id)))
            .collect::<Vec<_>>();
        let result = match func.result_type_id {
            Some(id) => format!(" -> {}", primitive_wit_type(arena, id)),
            None => String::new(),
        };
        wit.push_str(&format!(
            "    {}: func({}){result};\n",
            func.name,
            params.join(", "),
        ));
    }
    wit.push_str("}\n");
    Ok(wit)
}

/// Split `<ns>:<pkg>/<iface>[@<version>]` into its components. Version
/// is held separately from the package because WIT package headers use
/// `<ns>:<pkg>[@<version>];` while WIT import / export references use
/// `<ns>:<pkg>/<iface>[@<version>]` — same version, different position.
struct TargetParts {
    pkg: String,        // "<ns>:<pkg>"
    iface: String,      // "<iface>"
    version: Option<String>, // "<version>"
}

impl TargetParts {
    /// `<ns>:<pkg>[@<version>]` — for `package` headers.
    fn pkg_header(&self) -> String {
        match &self.version {
            Some(v) => format!("{}@{v}", self.pkg),
            None => self.pkg.clone(),
        }
    }

    /// `<ns>:<pkg>/<iface>[@<version>]` — for `import` / `export`
    /// references inside a world.
    fn import_path(&self) -> String {
        match &self.version {
            Some(v) => format!("{}/{}@{v}", self.pkg, self.iface),
            None => format!("{}/{}", self.pkg, self.iface),
        }
    }
}

fn split_target(target: &str) -> Result<TargetParts> {
    let (path, version) = match target.split_once('@') {
        Some((p, v)) => (p, Some(v.to_string())),
        None => (target, None),
    };
    let (pkg, iface) = path
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("target interface lacks `<pkg>/<iface>`: {target}"))?;
    Ok(TargetParts {
        pkg: pkg.to_string(),
        iface: iface.to_string(),
        version,
    })
}

/// WIT spelling of a value type. Caller must have checked
/// `is_emittable_value`.
fn primitive_wit_type(arena: &TypeArena, id: cviz::model::ValueTypeId) -> &'static str {
    match arena.lookup_val(id) {
        ValueType::Bool => "bool",
        ValueType::U8 => "u8",
        ValueType::S8 => "s8",
        ValueType::U16 => "u16",
        ValueType::S16 => "s16",
        ValueType::U32 => "u32",
        ValueType::S32 => "s32",
        ValueType::U64 => "u64",
        ValueType::S64 => "s64",
        ValueType::F32 => "f32",
        ValueType::F64 => "f64",
        ValueType::Char => "char",
        ValueType::String => "string",
        _ => unreachable!(
            "primitive_wit_type called on type the guard should have rejected"
        ),
    }
}

/// Canonical-ABI flat lowering of a value type. Strings flatten to two
/// `i32`s (ptr, len); 64-bit primitives to `i64`; floats to their own
/// flat type; everything else (current scope) to `i32`. Used to build
/// the wrapper export signature, the imported handler signature, and
/// the `(name_ptr, name_len)` placement around hook calls.
fn flat_types(arena: &TypeArena, id: cviz::model::ValueTypeId) -> Vec<ValType> {
    match arena.lookup_val(id) {
        ValueType::U64 | ValueType::S64 => vec![ValType::I64],
        ValueType::F32 => vec![ValType::F32],
        ValueType::F64 => vec![ValType::F64],
        ValueType::String => vec![ValType::I32, ValType::I32],
        ValueType::Bool
        | ValueType::U8
        | ValueType::S8
        | ValueType::U16
        | ValueType::S16
        | ValueType::U32
        | ValueType::S32
        | ValueType::Char => vec![ValType::I32],
        other => unreachable!("flat_types called on unsupported type {:?}", other),
    }
}

/// Whether a function's result needs the retptr (sret) form. The
/// canonical-ABI threshold is `MAX_FLAT_RESULTS = 1`: zero or one
/// flat-value results stay direct; two-or-more flat-value results
/// become a single `i32` retptr that the callee writes to.
fn needs_retptr(arena: &TypeArena, result_id: cviz::model::ValueTypeId) -> bool {
    flat_types(arena, result_id).len() > 1
}

/// Bytes of scratch memory reserved per function that returns a
/// retptr-shaped result. Sized to hold the largest result we currently
/// support — string descriptors (`(i32 ptr, i32 len)` = 8 bytes), with
/// 8 bytes of headroom for when we widen to lists / records-with-strings.
const RETPTR_SCRATCH_BYTES: u32 = 16;

/// Alignment of the retptr scratch slot. i32 boundary covers all the
/// post-name slots [`MemoryLayoutBuilder`] hands out today.
const RETPTR_SCRATCH_ALIGN: u32 = 4;

/// Index of the bump-allocator pointer in the dispatch module's global
/// space. There is exactly one global, and `cabi_realloc` is the only
/// reader / writer.
const BUMP_POINTER_GLOBAL: u32 = 0;

/// Synthesize the adapter world: imports + exports the target
/// interface, imports the active tier1 hooks.
fn synthesize_adapter_world_wit(
    target_interface: &str,
    has_before: bool,
    has_after: bool,
) -> String {
    let parts = split_target(target_interface).expect("validated earlier");
    let import_path = parts.import_path();
    // The adapter world lives in its own package so it doesn't clash
    // with the target. Pick a stable, splicer-internal name.
    let mut wit = String::from("package splicer:adapter;\n\nworld adapter {\n");
    wit.push_str(&format!("    import {import_path};\n"));
    wit.push_str(&format!("    export {import_path};\n"));
    if has_before {
        wit.push_str("    import splicer:tier1/before@0.1.0;\n");
    }
    if has_after {
        wit.push_str("    import splicer:tier1/after@0.1.0;\n");
    }
    wit.push_str("}\n");
    wit
}

/// Per-function metadata derived from the canonical-ABI flat lowering.
struct FuncDispatch {
    /// Lowered param `ValType`s (concatenated flats of every WIT param).
    param_flats: Vec<ValType>,
    /// What shape the result takes after lowering. Drives both the
    /// wrapper signature (export side) and the import signature.
    result: ResultLowering,
    /// Memory offset of this func's name bytes, for hook calls.
    name_offset: i32,
    /// Length of this func's name in bytes.
    name_len: i32,
    /// Memory offset of this func's retptr scratch buffer, only set
    /// when `result` is `Retptr`.
    retptr_offset: Option<i32>,
}

#[derive(Clone)]
enum ResultLowering {
    /// Function returns nothing.
    Void,
    /// Function returns one flat value directly. The contained
    /// `ValType` IS the wrapper's lowered result type.
    Direct(ValType),
    /// Function's result has 2+ flat values. The wrapper's lowered
    /// result is a single `i32` retptr; the import takes an extra
    /// `i32` retptr param and writes the actual flat values to memory
    /// at that address.
    Retptr,
}

/// Type-section index allocations. Captures every component-type-section
/// index the dispatch module reserves, in the order they're declared.
/// Per-func entries (`handler_ty`, `wrapper_ty`) are parallel to
/// `funcs[]`; the rest are singletons reused across all funcs.
struct TypeIndices {
    /// Per-func imported-handler signature: param flats + (retptr i32
    /// if Retptr) → Direct result type or empty.
    handler_ty: Vec<u32>,
    /// Per-func exported-wrapper signature: param flats → Direct or
    /// retptr i32 or empty.
    wrapper_ty: Vec<u32>,
    /// Hook signature `(name_ptr i32, name_len i32) → ()`.
    hook_ty: u32,
    /// `_initialize` signature `() → ()`.
    init_ty: u32,
    /// Per-cabi_post signature `(i32) → ()`. Only used when at least
    /// one func has a retptr result.
    cabi_post_ty: u32,
    /// `cabi_realloc` signature
    /// `(old_ptr, old_size, align, new_size) → new_ptr`. Only used
    /// when at least one string is in scope.
    cabi_realloc_ty: u32,
}

/// Function-index allocations across the core module's combined
/// import + defined function space. `imp_*` are imports (allocated
/// first by spec); `wrapper_base` and below are defined functions.
struct FuncIndices {
    /// Per-func imported handler (`<iface> / <fn>`).
    imp_handler: Vec<u32>,
    /// Imported `before-call` hook, when present.
    imp_before: Option<u32>,
    /// Imported `after-call` hook, when present.
    imp_after: Option<u32>,
    /// Index of the first defined wrapper. Wrapper i lives at
    /// `wrapper_base + i`.
    wrapper_base: u32,
    /// Index of the defined `_initialize` function.
    init: u32,
    /// Per-func defined `cabi_post_<iface>#<fn>`. `Some` iff that
    /// func returns a retptr; index of the actual defined function in
    /// that case.
    cabi_post: Vec<Option<u32>>,
    /// Index of the defined `cabi_realloc`. `Some` iff at least one
    /// string is in scope.
    cabi_realloc: Option<u32>,
}

/// Build the dispatch core module — one or more funcs, hook wrapping,
/// canonical-ABI flat lowering for primitives + strings. Strings
/// require:
///   - an exported `cabi_realloc` so the canon-lower side can allocate
///     input string buffers in our memory,
///   - an exported `cabi_post_<iface>#<fn>` per func with a retptr
///     result so the canon-lift side can free the result buffer,
///   - a static retptr scratch slot per such func, where the imported
///     handler writes its result string descriptor and the wrapper
///     hands back the same address.
///
/// The function is structured as one phase per emitted section: lay
/// out memory, allocate type indices (writing the type section as we
/// go), allocate import + defined func indices (writing import +
/// function sections), emit memory / globals / exports, then code +
/// data. All counter bookkeeping is delegated to [`MemoryLayoutBuilder`],
/// [`DispatchIndices`], and [`FunctionIndices`] so this builder never
/// threads its own `&mut u32`s.
fn build_simple_dispatch(
    target_interface: &str,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    arena: &TypeArena,
) -> Result<Vec<u8>> {
    let any_string_anywhere = funcs.iter().any(|f| has_string_anywhere(f, arena));

    // ── Layout: per-func dispatch shape + memory offsets. ───────────
    let (per_func, name_blob, bump_start) = compute_func_dispatches(funcs, arena);

    // ── Indices: a single allocator for ty / func across the whole
    //    core module. Hand it to each phase by &mut so phases never
    //    diverge on "where did we leave the counter?".
    let mut idx = DispatchIndices::new();

    // ── Sections ────────────────────────────────────────────────────
    let mut module = Module::new();

    let type_idx = emit_type_section(
        &mut module,
        &mut idx,
        &per_func,
        any_string_anywhere,
    );
    let func_idx = emit_imports_section(
        &mut module,
        &mut idx,
        target_interface,
        funcs,
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
        any_string_anywhere,
    );
    emit_memory_and_globals(&mut module, bump_start, any_string_anywhere);
    emit_export_section(&mut module, target_interface, funcs, &per_func, &func_idx);
    emit_code_section(&mut module, funcs, &per_func, &func_idx);
    emit_data_section(&mut module, &name_blob);

    Ok(module.finish())
}

/// Phase 1 — derive per-func dispatch shapes, collect name bytes, and
/// reserve memory slots for any retptr-shaped result. Returns the
/// per-func info, the concatenated name bytes (for the data segment),
/// and the bump-allocator base (first byte after the reserved area).
fn compute_func_dispatches(
    funcs: &[AdapterFunc],
    arena: &TypeArena,
) -> (Vec<FuncDispatch>, Vec<u8>, u32) {
    let total_name_bytes: u32 = funcs.iter().map(|f| f.name.len() as u32).sum();
    let mut layout = MemoryLayoutBuilder::new(total_name_bytes);
    let mut name_blob: Vec<u8> = Vec::with_capacity(total_name_bytes as usize);
    let mut per_func: Vec<FuncDispatch> = Vec::with_capacity(funcs.len());

    for func in funcs {
        // Names live in the name region of the layout; concatenate
        // their bytes into the blob in the same order so the data
        // segment matches the offsets we allocate.
        let name_offset = layout.alloc_name(func.name.len() as u32) as i32;
        name_blob.extend_from_slice(func.name.as_bytes());

        let param_flats: Vec<ValType> = func
            .param_type_ids
            .iter()
            .flat_map(|&id| flat_types(arena, id))
            .collect();
        let result = lower_result(arena, func.result_type_id);

        // Retptr-shaped results need a stable scratch slot; reserve
        // one in the post-name region. Direct / Void results don't.
        let retptr_offset = matches!(result, ResultLowering::Retptr).then(|| {
            layout.alloc_sync_result(RETPTR_SCRATCH_BYTES, RETPTR_SCRATCH_ALIGN) as i32
        });

        per_func.push(FuncDispatch {
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

/// Compute the [`ResultLowering`] for a function's result type id (or
/// `None` for void).
fn lower_result(arena: &TypeArena, result_id: Option<ValueTypeId>) -> ResultLowering {
    match result_id {
        None => ResultLowering::Void,
        Some(rid) if needs_retptr(arena, rid) => ResultLowering::Retptr,
        Some(rid) => ResultLowering::Direct(flat_types(arena, rid)[0]),
    }
}

/// Whether any param or result of `func` is a `string`. Drives whether
/// the dispatch module needs to export `cabi_realloc` + the
/// bump-allocator global.
fn has_string_anywhere(func: &AdapterFunc, arena: &TypeArena) -> bool {
    func.param_type_ids
        .iter()
        .chain(func.result_type_id.iter())
        .any(|&id| matches!(arena.lookup_val(id), ValueType::String))
}

/// Phase 2 — emit the type section. Allocates:
///   - per-func handler import type (param flats [+ retptr i32] →
///     direct result),
///   - per-func wrapper export type (param flats → direct result OR
///     retptr i32 OR void),
///   - hook type, init type, cabi_post type, cabi_realloc type
///     (the last two even when not exercised, so type indices are
///     stable across configurations).
fn emit_type_section(
    module: &mut Module,
    idx: &mut DispatchIndices,
    per_func: &[FuncDispatch],
    any_string: bool,
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
    let _ = any_string; // type is always declared; whether to USE it lives in func+export emit.

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

/// Lowered wrapper-export signature: param flats; result is the
/// Direct value, or `i32` retptr, or empty.
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
/// import indices filled; the defined-function indices land in
/// [`emit_function_section`].
fn emit_imports_section(
    module: &mut Module,
    idx: &mut DispatchIndices,
    target_interface: &str,
    funcs: &[AdapterFunc],
    type_idx: &TypeIndices,
    has_before: bool,
    has_after: bool,
) -> FuncIndices {
    let mut imports = ImportSection::new();
    let mut imp_handler: Vec<u32> = Vec::with_capacity(funcs.len());
    for (i, func) in funcs.iter().enumerate() {
        imports.import(
            target_interface,
            func.name.as_str(),
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
        cabi_post: vec![None; funcs.len()],
        cabi_realloc: None,
    }
}

/// Phase 4 — emit the function section (defined-function declarations:
/// per-func wrappers, `_initialize`, per-retptr-func `cabi_post_*`, and
/// `cabi_realloc` when strings are in scope). Returns the input
/// [`FuncIndices`] with the defined-function fields populated.
fn emit_function_section(
    module: &mut Module,
    idx: &mut DispatchIndices,
    per_func: &[FuncDispatch],
    type_idx: &TypeIndices,
    mut func_idx: FuncIndices,
    any_string: bool,
) -> FuncIndices {
    let mut fsec = FunctionSection::new();

    // Wrappers — one per func, in order.
    let wrapper_base = idx.func;
    for &t in &type_idx.wrapper_ty {
        fsec.function(t);
    }
    for _ in per_func {
        idx.alloc_func();
    }
    func_idx.wrapper_base = wrapper_base;

    // _initialize.
    fsec.function(type_idx.init_ty);
    func_idx.init = idx.alloc_func();

    // cabi_post — one per retptr-shaped func.
    for (i, fd) in per_func.iter().enumerate() {
        if matches!(fd.result, ResultLowering::Retptr) {
            fsec.function(type_idx.cabi_post_ty);
            func_idx.cabi_post[i] = Some(idx.alloc_func());
        }
    }

    // cabi_realloc — single, only when strings are in scope.
    if any_string {
        fsec.function(type_idx.cabi_realloc_ty);
        func_idx.cabi_realloc = Some(idx.alloc_func());
    }

    module.section(&fsec);
    func_idx
}

/// Phase 5 — memory + global sections. Memory is always emitted; the
/// bump-pointer global is only emitted when `cabi_realloc` is in
/// scope (which is whenever any string is).
fn emit_memory_and_globals(module: &mut Module, bump_start: u32, any_string: bool) {
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memory);

    if any_string {
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

/// Phase 6 — emit the export section: per-func wrapper +
/// `cabi_post_*` (when retptr), `memory`, `cabi_realloc` (when in
/// scope), `_initialize`.
fn emit_export_section(
    module: &mut Module,
    target_interface: &str,
    funcs: &[AdapterFunc],
    per_func: &[FuncDispatch],
    func_idx: &FuncIndices,
) {
    let mut exports = ExportSection::new();
    for (i, func) in funcs.iter().enumerate() {
        let name = format!("{target_interface}#{}", func.name);
        exports.export(&name, ExportKind::Func, func_idx.wrapper_base + i as u32);
        if let Some(post_idx) = func_idx.cabi_post[i] {
            let post_name = format!("cabi_post_{name}");
            exports.export(&post_name, ExportKind::Func, post_idx);
            let _ = per_func; // pulled in only if we want to read fd from here
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
/// section declared its defined functions: wrappers, then
/// `_initialize`, then per-retptr `cabi_post_*`, then `cabi_realloc`.
fn emit_code_section(
    module: &mut Module,
    _funcs: &[AdapterFunc],
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
    // _initialize: empty body.
    code.function(&empty_function());
    // cabi_post — no-op per retptr-shaped func.
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

/// A function with no locals and an empty body (just `end`). Used for
/// `_initialize` and the no-op `cabi_post_*` exports.
fn empty_function() -> Function {
    let mut f = Function::new_with_locals_types([]);
    f.instructions().end();
    f
}

/// Phase 8 — emit the active data segment containing the concatenated
/// function names. Skipped entirely when there are no funcs.
fn emit_data_section(module: &mut Module, name_blob: &[u8]) {
    if name_blob.is_empty() {
        return;
    }
    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), name_blob.iter().copied());
    module.section(&data);
}

/// Emit one wrapper function body. Layout:
///   [optional before-call(name_ptr, name_len)]
///   load all wrapper params onto stack
///   if Retptr result: push retptr_offset
///   call import_handler
///   if Direct result: stash into local (so after-call doesn't clobber
///                                       the value on the stack)
///   [optional after-call(name_ptr, name_len)]
///   if Direct result: load from local
///   if Retptr result: push retptr_offset
///   end
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
        f.instructions().i32_const(fd.retptr_offset.expect("retptr_offset set"));
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
            f.instructions().local_get(result_local.expect("Direct → local_set"));
        }
        ResultLowering::Retptr => {
            f.instructions().i32_const(fd.retptr_offset.expect("retptr_offset set"));
        }
    }
    f.instructions().end();
    code.function(&f);
}

/// Push `(name_ptr, name_len, call hook_idx)` into the wrapper body.
/// Shared between the `before-call` and `after-call` emission sites.
fn push_hook_call(f: &mut Function, fd: &FuncDispatch, hook_idx: u32) {
    f.instructions().i32_const(fd.name_offset);
    f.instructions().i32_const(fd.name_len);
    f.instructions().call(hook_idx);
}

/// Bump-allocator `cabi_realloc`. Signature `(old_ptr, old_size,
/// align, new_size) -> new_ptr`. Treats every call as a fresh alloc,
/// rounding the bump pointer up to `align` and advancing by
/// `new_size`. No memory growth: assumes the initial 1 page (64 KiB)
/// suffices for the small input/output buffers tier-1 splicer's
/// canonical-ABI passthrough touches. Adequate for the rewrite's
/// first wave; widens to `memory.grow` when realistic workloads
/// demand it.
fn emit_cabi_realloc(code: &mut CodeSection) {
    // The 4 params (old_ptr, old_size, align, new_size) sit at locals
    // 0..3; scratch lives at the first local index above them.
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
