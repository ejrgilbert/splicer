//! Tier-2 adapter generator: builds an adapter component that lifts
//! a target function's canonical-ABI parameters into the cell-array
//! representation and forwards them to the middleware's tier-2
//! `on-call` hook before dispatching to the handler.
//!
//! Status (Phase 2-3, slice 1 of N): sync pass-through — for sync
//! primitive-typed targets the wrapper just forwards args to the
//! handler. The `on-call` import is declared in the world but the
//! wrapper body doesn't yet invoke it; that wiring lands in the next
//! slice. Async target functions bail with a clear message.
//!
//! Reuse strategy: a few helpers (memory + globals, cabi_realloc body,
//! data section) are near-identical to tier-1's. They're inlined here
//! for now with `// TODO: extract` markers; once tier-2 stabilizes
//! we'll lift the genuinely-shared pieces up to `super::super::*`.
//!
//! Design conventions intentionally mirror the tier-1 emit path
//! (`super::super::tier1::emit`) so reading one informs the other.

use anyhow::{bail, Context, Result};
use wasm_encoder::{
    CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection, ImportSection,
    Module, TypeSection, ValType,
};
use wit_component::{embed_component_metadata, ComponentEncoder, StringEncoding};
use wit_parser::abi::{AbiVariant, WasmSignature};
use wit_parser::{
    Function as WitFunction, InterfaceId, LiftLowerAbi, ManglingAndAbi, Resolve, WasmExport,
    WasmExportKind, WasmImport, WorldKey,
};

use super::super::abi::emit::{
    empty_function, emit_cabi_realloc, emit_memory_and_globals, val_types, EXPORT_CABI_REALLOC,
    EXPORT_INITIALIZE, EXPORT_MEMORY,
};
use super::super::resolve::{decode_input_resolve, find_target_interface};

/// Adapter component package + world name. Same convention as tier-1
/// — `wit-component`'s `ComponentEncoder` looks up the world by name
/// to know which import/export wiring to apply.
const TIER2_ADAPTER_WORLD_PACKAGE: &str = "splicer:adapter-tier2";
const TIER2_ADAPTER_WORLD_NAME: &str = "adapter";

/// Generate a tier-2 adapter component.
///
/// First slice: sync pass-through. Async target functions and the
/// `on-call` invocation itself land in follow-up slices.
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

    let mut core_module = build_dispatch_module(&resolve, target_iface, target_interface)?;
    embed_component_metadata(&mut core_module, &resolve, world_id, StringEncoding::UTF8)
        .context("embed_component_metadata")?;

    ComponentEncoder::default()
        .validate(true)
        .module(&core_module)
        .context("ComponentEncoder::module")?
        .encode()
        .context("ComponentEncoder::encode")
}

/// Bail on cases that fail before the lift codegen even runs (empty
/// interfaces, async target functions for now). Out-of-scope **type**
/// shapes surface as `todo!()` panics inside the lift codegen itself
/// in later slices, so each unhandled match arm is the actionable
/// signpost telling the next implementer exactly which case to extend.
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

/// Synthesize the tier-2 adapter world: import + export the target
/// interface and import `splicer:tier2/before` so wit-component
/// wires up the `on-call` hook.
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

/// Per-function dispatch shape — names + sigs sourced from the
/// `Resolve`. Tier-2 v1 only handles sync targets, so this is much
/// simpler than tier-1's equivalent.
struct FuncDispatch {
    /// Handler import module name (target interface's canonical-ABI
    /// module name).
    import_module: String,
    /// Handler import field name (function's canonical-ABI mangled
    /// name).
    import_field: String,
    /// Wrapper export name (mangled the same way).
    export_name: String,
    /// Sync canonical-ABI signature.
    sig: WasmSignature,
    /// `Some` iff the canonical-ABI flat result overflows and the
    /// caller passes a retptr. Forces an empty `cabi_post_<name>`
    /// export (same convention as tier-1).
    needs_cabi_post: bool,
}

fn build_dispatch_module(
    resolve: &Resolve,
    target_iface: InterfaceId,
    target_interface_name: &str,
) -> Result<Vec<u8>> {
    let _ = target_interface_name; // not yet used at this slice (Phase 2-3 next slice
                                   // wires it into call-id strings)
    let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
        .functions
        .values()
        .collect();

    let target_world_key = WorldKey::Interface(target_iface);
    let mangling = ManglingAndAbi::Legacy(LiftLowerAbi::Sync);

    let mut per_func: Vec<FuncDispatch> = Vec::with_capacity(funcs.len());
    for func in &funcs {
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
        let sig = resolve.wasm_signature(AbiVariant::GuestExport, func);
        let needs_cabi_post = sig.retptr;
        per_func.push(FuncDispatch {
            import_module,
            import_field,
            export_name,
            sig,
            needs_cabi_post,
        });
    }

    let mut module = Module::new();
    let (handler_ty_idx, wrapper_ty_idx, init_ty, cabi_post_ty, cabi_realloc_ty) =
        emit_type_section(&mut module, &per_func);
    let (handler_imp_base, wrapper_base, init_idx, cabi_realloc_idx) = emit_imports_and_funcs(
        &mut module,
        &per_func,
        &handler_ty_idx,
        &wrapper_ty_idx,
        init_ty,
        cabi_post_ty,
        cabi_realloc_ty,
    );
    // Phase 2-3 first slice doesn't pre-allocate scratch yet, so the
    // bump allocator starts at offset 0.
    emit_memory_and_globals(&mut module, 0);
    emit_export_section(
        &mut module,
        &per_func,
        wrapper_base,
        init_idx,
        cabi_realloc_idx,
    );
    emit_code_section(&mut module, &per_func, handler_imp_base);
    Ok(module.finish())
}

/// Emit the type section. Returns indices for: per-func handler types,
/// per-func wrapper types, init type, cabi_post type, cabi_realloc type.
fn emit_type_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
) -> (Vec<u32>, Vec<u32>, u32, u32, u32) {
    let mut types = TypeSection::new();
    let mut next_ty: u32 = 0;
    let mut handler_ty_idx = Vec::with_capacity(per_func.len());
    let mut wrapper_ty_idx = Vec::with_capacity(per_func.len());

    // Handler import types — same canonical-ABI sig as the wrapper
    // export (sync pass-through wires them identically).
    for fd in per_func {
        types
            .ty()
            .function(val_types(&fd.sig.params), val_types(&fd.sig.results));
        handler_ty_idx.push(next_ty);
        next_ty += 1;
    }
    // Wrapper export types — currently identical shape to the
    // handler. Kept as a separate slot so future slices can reshape
    // them (e.g. async wrappers carry a retptr the handler doesn't).
    for fd in per_func {
        types
            .ty()
            .function(val_types(&fd.sig.params), val_types(&fd.sig.results));
        wrapper_ty_idx.push(next_ty);
        next_ty += 1;
    }

    // _initialize: () -> ()
    types.ty().function([], []);
    let init_ty = next_ty;
    next_ty += 1;

    // cabi_post_<name>: (i32) -> ()
    types.ty().function([ValType::I32], []);
    let cabi_post_ty = next_ty;
    next_ty += 1;

    // cabi_realloc: (i32, i32, i32, i32) -> i32
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        [ValType::I32],
    );
    let cabi_realloc_ty = next_ty;

    module.section(&types);
    (
        handler_ty_idx,
        wrapper_ty_idx,
        init_ty,
        cabi_post_ty,
        cabi_realloc_ty,
    )
}

/// Emit imports section + function section. Returns:
/// `(handler_imp_base, wrapper_base, init_idx, cabi_realloc_idx)` —
/// indices the exports + code sections need.
///
/// Function index space layout: handlers (0..N), then wrappers
/// (N..2N), then init, then per-func cabi_post (one each, only
/// allocated for funcs with retptr — same convention as tier-1),
/// then cabi_realloc.
fn emit_imports_and_funcs(
    module: &mut Module,
    per_func: &[FuncDispatch],
    handler_ty_idx: &[u32],
    wrapper_ty_idx: &[u32],
    init_ty: u32,
    cabi_post_ty: u32,
    cabi_realloc_ty: u32,
) -> (u32, u32, u32, u32) {
    // Imports: just handler funcs for now. on-call will join here in
    // the next slice.
    let mut imports = ImportSection::new();
    for (fd, &ty) in per_func.iter().zip(handler_ty_idx) {
        imports.import(
            &fd.import_module,
            &fd.import_field,
            EntityType::Function(ty),
        );
    }
    module.section(&imports);

    let n = per_func.len() as u32;
    let handler_imp_base = 0;
    let wrapper_base = n;

    // Function section: wrapper, init, per-func cabi_post (only for
    // retptr funcs), cabi_realloc. Allocate all indices up front.
    let mut fsec = FunctionSection::new();
    for &ty in wrapper_ty_idx {
        fsec.function(ty);
    }
    fsec.function(init_ty);
    let init_idx = wrapper_base + n;
    let mut next_idx = init_idx + 1;
    for fd in per_func {
        if fd.needs_cabi_post {
            fsec.function(cabi_post_ty);
            next_idx += 1;
        }
    }
    fsec.function(cabi_realloc_ty);
    let cabi_realloc_idx = next_idx;
    module.section(&fsec);

    (handler_imp_base, wrapper_base, init_idx, cabi_realloc_idx)
}

/// Emit export section: each wrapper, optional cabi_post per
/// retptr-bearing wrapper, memory, cabi_realloc, _initialize.
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
        exports.export(
            &fd.export_name,
            ExportKind::Func,
            wrapper_base + i as u32,
        );
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

/// Emit code section. Per-func wrapper bodies are pass-through:
/// `local.get` each param, call the handler, return the result.
/// The next slice replaces this with: lift args → call on-call →
/// await → call handler → return.
fn emit_code_section(module: &mut Module, per_func: &[FuncDispatch], handler_imp_base: u32) {
    let mut code = CodeSection::new();
    for (i, fd) in per_func.iter().enumerate() {
        let mut f = Function::new([]);
        for j in 0..fd.sig.params.len() as u32 {
            f.instructions().local_get(j);
        }
        f.instructions()
            .call(handler_imp_base + i as u32);
        f.instructions().end();
        code.function(&f);
    }
    // _initialize — no-op.
    code.function(&empty_function());
    // Per-func cabi_post — empty body.
    for fd in per_func {
        if fd.needs_cabi_post {
            code.function(&empty_function());
        }
    }
    // cabi_realloc — shared bump-allocator implementation.
    emit_cabi_realloc(&mut code);
    module.section(&code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmparser::Validator;

    /// Build a tier-2 pass-through adapter for a target with a single
    /// sync primitive function (`add(x: u32, y: u32) -> u32`-style),
    /// run it through `wit-parser` decode → ComponentEncoder, and
    /// validate the resulting component bytes round-trip through
    /// wasmparser. This is the e2e structural smoke test for the
    /// dispatch module.
    #[test]
    fn pass_through_adapter_roundtrips_through_component_encoder() {
        // The tier-2 adapter expects the input "split" to be a wasm
        // component — synthesize one inline that exports the target
        // interface (`my:math/api@1.0.0` with `add: func(x: u32, y:
        // u32) -> u32`).
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
            true, // has_before
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect("tier-2 adapter generation should succeed");

        Validator::new()
            .validate_all(&bytes)
            .expect("emitted tier-2 adapter component should validate");
    }
}
