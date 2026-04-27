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
use cviz::model::{InterfaceType, TypeArena, ValueType};
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, MemorySection, MemoryType, Module, TypeSection, ValType,
};
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

use crate::adapter::func::AdapterFunc;

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
    let mut core_module = build_simple_dispatch(target_interface, funcs, has_before, has_after)?;
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
            if !is_primitive(arena, id) {
                bail!("wit_component_emit: non-primitive param types not yet handled");
            }
        }
        if let Some(rid) = f.result_type_id {
            if !is_primitive(arena, rid) {
                bail!("wit_component_emit: non-primitive result type not yet handled");
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

fn is_primitive(arena: &TypeArena, id: cviz::model::ValueTypeId) -> bool {
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

/// WIT spelling of a primitive value type. Caller must have checked
/// `is_primitive`.
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
        _ => unreachable!("primitive_wit_type called on non-primitive — \
                          require_simple_case should have rejected it"),
    }
}

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

/// Build the dispatch core module for one or more funcs with primitive
/// params + primitive (or unit) result, optional before/after hooks.
/// Mirrors `examples/wit_component_spike.rs::build_dispatch_core_module`,
/// generalized to N funcs.
///
/// Layout — types come first (one per func, plus shared hook + init
/// types), then imports (handlers + hooks), then defined funcs (one
/// wrapper per handler + `_initialize`), memory, exports (one per
/// wrapper + memory + `_initialize`), code, data (concatenated func
/// names with per-func offset/len recorded for the hook calls).
fn build_simple_dispatch(
    target_interface: &str,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
) -> Result<Vec<u8>> {
    let mut module = Module::new();

    // ── Type section ────────────────────────────────────────────────
    // One type per func (signatures may differ); one shared hook type
    // `(i32, i32) -> ()`; one `_initialize` type `() -> ()`.
    let mut types = TypeSection::new();
    let mut func_ty_idx: Vec<u32> = Vec::with_capacity(funcs.len());
    for (i, func) in funcs.iter().enumerate() {
        let core_params: Vec<ValType> = func
            .param_type_ids
            .iter()
            .map(|_| ValType::I32) // primitives mostly fit i32; widen when we cover i64/f32/f64
            .collect();
        let core_result: Vec<ValType> = match func.result_type_id {
            Some(_) => vec![ValType::I32],
            None => vec![],
        };
        types.ty().function(core_params, core_result);
        func_ty_idx.push(i as u32);
    }
    let hook_ty: u32 = funcs.len() as u32;
    types.ty().function([ValType::I32, ValType::I32], []);
    let init_ty: u32 = hook_ty + 1;
    types.ty().function([], []);
    module.section(&types);

    // ── Import section ──────────────────────────────────────────────
    let mut imports = ImportSection::new();
    let mut next_func_idx: u32 = 0;

    // Per-func handler imports. Each gets its own type.
    let mut imp_handler_idx: Vec<u32> = Vec::with_capacity(funcs.len());
    for (i, func) in funcs.iter().enumerate() {
        imports.import(
            target_interface,
            func.name.as_str(),
            EntityType::Function(func_ty_idx[i]),
        );
        imp_handler_idx.push(next_func_idx);
        next_func_idx += 1;
    }

    let imp_before = if has_before {
        imports.import(
            "splicer:tier1/before@0.1.0",
            "before-call",
            EntityType::Function(hook_ty),
        );
        let i = next_func_idx;
        next_func_idx += 1;
        Some(i)
    } else {
        None
    };
    let imp_after = if has_after {
        imports.import(
            "splicer:tier1/after@0.1.0",
            "after-call",
            EntityType::Function(hook_ty),
        );
        let i = next_func_idx;
        next_func_idx += 1;
        Some(i)
    } else {
        None
    };
    module.section(&imports);

    // ── Function section ────────────────────────────────────────────
    // One wrapper per func (matching its handler's type) plus
    // _initialize. Wrappers are contiguous from `wrapper_base`.
    let mut fsec = FunctionSection::new();
    let wrapper_base = next_func_idx;
    for (i, _) in funcs.iter().enumerate() {
        fsec.function(func_ty_idx[i]);
    }
    fsec.function(init_ty);
    module.section(&fsec);
    let init_idx = wrapper_base + funcs.len() as u32;

    // ── Memory section ──────────────────────────────────────────────
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memory);

    // ── Export section ──────────────────────────────────────────────
    let mut exports = ExportSection::new();
    for (i, func) in funcs.iter().enumerate() {
        let export_name = format!("{target_interface}#{}", func.name);
        exports.export(&export_name, ExportKind::Func, wrapper_base + i as u32);
    }
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("_initialize", ExportKind::Func, init_idx);
    module.section(&exports);

    // ── Code + Data sections ────────────────────────────────────────
    // Walk funcs, emitting:
    //   - one wrapper body per func (with the right per-func name
    //     offset/len for hook calls)
    //   - one shared data segment carrying all func name bytes
    //     concatenated, with per-func (offset, len) recorded.
    let mut code = CodeSection::new();
    let mut name_blob: Vec<u8> = Vec::new();
    let mut name_offsets: Vec<(i32, i32)> = Vec::with_capacity(funcs.len());
    for func in funcs {
        let off = name_blob.len() as i32;
        let len = func.name.len() as i32;
        name_blob.extend_from_slice(func.name.as_bytes());
        name_offsets.push((off, len));
    }

    for (i, func) in funcs.iter().enumerate() {
        let has_result = func.result_type_id.is_some();
        let locals: Vec<(u32, ValType)> = if has_result {
            vec![(1, ValType::I32)]
        } else {
            vec![]
        };
        let mut wrapper = Function::new(locals);
        let result_local: u32 = func.param_type_ids.len() as u32;
        let (name_off, name_len) = name_offsets[i];

        if let Some(idx) = imp_before {
            wrapper.instructions().i32_const(name_off);
            wrapper.instructions().i32_const(name_len);
            wrapper.instructions().call(idx);
        }
        for p in 0..(func.param_type_ids.len() as u32) {
            wrapper.instructions().local_get(p);
        }
        wrapper.instructions().call(imp_handler_idx[i]);
        if has_result {
            wrapper.instructions().local_set(result_local);
        }
        if let Some(idx) = imp_after {
            wrapper.instructions().i32_const(name_off);
            wrapper.instructions().i32_const(name_len);
            wrapper.instructions().call(idx);
        }
        if has_result {
            wrapper.instructions().local_get(result_local);
        }
        wrapper.instructions().end();
        code.function(&wrapper);
    }

    let mut init = Function::new(vec![]);
    init.instructions().end();
    code.function(&init);
    module.section(&code);

    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), name_blob.iter().copied());
    module.section(&data);

    Ok(module.finish())
}
