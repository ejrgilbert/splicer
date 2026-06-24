//! Sync-to-async bridge component generator.
#![allow(dead_code)]

use anyhow::{Context, Result};

use super::super::abi::canon_async::{self, AsyncFuncs};
use super::super::abi::emit::{
    emit_cabi_realloc, emit_cabi_realloc_call, emit_export_section, emit_memory_and_globals,
    empty_function, val_types, WrapperExport,
};
use super::super::indices::{DispatchIndices, LocalsBuilder};
use super::super::resolve::{decode_input_resolve, dispatch_mangling, find_target_interface};
use super::{short_hash_hex, synthesize_async_mirror};
use crate::adapter::sanitize_name;
use wasm_encoder::{
    CodeSection, EntityType, Function, FunctionSection, ImportSection, MemArg, Module, TypeSection,
    ValType,
};
use wit_component::{embed_component_metadata, ComponentEncoder, StringEncoding};
use wit_parser::abi::{AbiVariant, WasmSignature, WasmType};
use wit_parser::{
    Function as WitFunction, InterfaceId, LiftLowerAbi, ManglingAndAbi, Resolve, SizeAlign, Type,
    WasmExport, WasmExportKind, WasmImport, WorldKey,
};

/// Generate a sync-to-async bridge component for `target_interface`
/// against the WIT carried in `target_split_path`. Writes the bridge
/// `.wasm` under `splits_output_path` and returns `(bridge_path,
/// async_mirror_qualified_name)`.
pub fn generate_sync_async_bridge(
    target_interface: &str,
    splits_output_path: &str,
    target_split_path: &str,
) -> Result<(String, String)> {
    let split_bytes = std::fs::read(target_split_path)
        .with_context(|| format!("read target split at `{target_split_path}`"))?;
    let (bytes, mirror_qualified) = build_bridge_bytes(target_interface, &split_bytes)?;
    let out_path = format!(
        "{splits_output_path}/splicer_bridge_{}_{}.wasm",
        sanitize_name(target_interface),
        short_hash_hex(target_interface),
    );
    std::fs::write(&out_path, &bytes)
        .with_context(|| format!("write bridge component to `{out_path}`"))?;
    Ok((out_path, mirror_qualified))
}

fn build_bridge_bytes(target_interface: &str, split_bytes: &[u8]) -> Result<(Vec<u8>, String)> {
    let mut resolve = decode_input_resolve(split_bytes)?;
    let target_iface_id = find_target_interface(&resolve, target_interface)?;
    let (mirror_iface_id, mirror_qualified) =
        synthesize_async_mirror(&mut resolve, target_iface_id)
            .with_context(|| format!("synthesize async mirror for `{target_interface}`"))?;

    let bridge_world_wit = synthesize_bridge_world_wit(target_interface, &mirror_qualified);
    let bridge_pkg_id = resolve
        .push_str("splicer-bridge.wit", &bridge_world_wit)
        .with_context(|| format!("parse bridge world WIT:\n{bridge_world_wit}"))?;
    let world_id = resolve
        .select_world(&[bridge_pkg_id], Some(BRIDGE_WORLD_NAME))
        .context("select bridge world")?;

    let mut core = build_bridge_module(&resolve, target_iface_id, mirror_iface_id)?;
    embed_component_metadata(&mut core, &resolve, world_id, StringEncoding::UTF8)
        .context("embed_component_metadata")?;

    let bytes = ComponentEncoder::default()
        .validate(true)
        .module(&core)
        .context("ComponentEncoder::module")?
        .encode()
        .context("ComponentEncoder::encode")?;
    Ok((bytes, mirror_qualified))
}

const BRIDGE_WORLD_PACKAGE: &str = "splicer:bridge@0.0.1";
const BRIDGE_WORLD_NAME: &str = "bridge";

/// Static memory layout:
///   - `[0..4)`  saved bump snapshot for retptr exports (`cabi_post_<fn>`
///     restores from here).
///   - `[4..16)` event scratch; `[waitable-set-wait]` writes the
///     `(event_kind, handle, retval)` triple here.
///
/// The bump allocator starts at [`BUMP_START`].
const SAVED_BUMP_OFFSET: u64 = 0;
const EVENT_PTR_OFFSET: i32 = 4;
const I32_STORE_LOG2_ALIGN: u32 = 2;
const BUMP_START: u32 = 16;

fn synthesize_bridge_world_wit(target_interface: &str, mirror_qualified: &str) -> String {
    format!(
        "package {BRIDGE_WORLD_PACKAGE};\n\n\
         world {BRIDGE_WORLD_NAME} {{\n\
        \x20   export {target_interface};\n\
        \x20   import {mirror_qualified};\n\
         }}\n",
    )
}

/// Per-function plan: sigs, mangled names, and result-buffer
/// size/align used by both the type-section build and the body emit.
///
/// The export side is sync canon-lift (caller sees a normal sync call);
/// the import side is async canon-lower against the mirror, so the
/// import sig always returns a packed `(handle << 4) | status` i32 and
/// appends a retptr param the callee writes into for any function with a result.
struct FuncPlan {
    export_name: String,
    import_module: String,
    import_field: String,
    export_sig: WasmSignature,
    import_sig: WasmSignature,
    /// `Some((size, align))` whenever the function has a result type.
    result_size_align: Option<(u32, u32)>,
    /// Target's WIT result type.
    result_ty: Option<Type>,
}

fn gather_func_plans(
    resolve: &Resolve,
    target_iface_id: InterfaceId,
    mirror_iface_id: InterfaceId,
) -> Result<Vec<FuncPlan>> {
    let export_mangling = ManglingAndAbi::Legacy(LiftLowerAbi::Sync);
    let import_mangling = dispatch_mangling(true);
    let target_key = WorldKey::Interface(target_iface_id);
    let mirror_key = WorldKey::Interface(mirror_iface_id);

    let target_funcs: Vec<&WitFunction> = resolve.interfaces[target_iface_id]
        .functions
        .values()
        .collect();
    let mirror_funcs: Vec<&WitFunction> = resolve.interfaces[mirror_iface_id]
        .functions
        .values()
        .collect();
    if target_funcs.len() != mirror_funcs.len() {
        anyhow::bail!(
            "async mirror has {} funcs, target has {} — synth invariant broken",
            mirror_funcs.len(),
            target_funcs.len(),
        );
    }

    let mut sizes = SizeAlign::default();
    sizes.fill(resolve);

    target_funcs
        .iter()
        .zip(&mirror_funcs)
        .map(|(tfunc, mfunc)| {
            let export_sig = resolve.wasm_signature(export_mangling.export_variant(), tfunc);
            let import_sig = resolve.wasm_signature(AbiVariant::GuestImportAsync, mfunc);
            let export_name = resolve.wasm_export_name(
                export_mangling,
                WasmExport::Func {
                    interface: Some(&target_key),
                    func: tfunc,
                    kind: WasmExportKind::Normal,
                },
            );
            let (import_module, import_field) = resolve.wasm_import_name(
                import_mangling,
                WasmImport::Func {
                    interface: Some(&mirror_key),
                    func: mfunc,
                },
            );
            let result_size_align = tfunc.result.as_ref().map(|result_ty| {
                (
                    sizes.size(result_ty).size_wasm32() as u32,
                    sizes.align(result_ty).align_wasm32() as u32,
                )
            });
            Ok(FuncPlan {
                export_name,
                import_module,
                import_field,
                export_sig,
                import_sig,
                result_size_align,
                result_ty: tfunc.result,
            })
        })
        .collect()
}

fn build_bridge_module(
    resolve: &Resolve,
    target_iface_id: InterfaceId,
    mirror_iface_id: InterfaceId,
) -> Result<Vec<u8>> {
    let plans = gather_func_plans(resolve, target_iface_id, mirror_iface_id)?;

    let mut module = Module::new();
    let mut idx = DispatchIndices::new();

    // -- Type section --
    let mut types = TypeSection::new();
    let mut handler_ty: Vec<u32> = Vec::with_capacity(plans.len());
    let mut wrapper_ty: Vec<u32> = Vec::with_capacity(plans.len());
    let mut cabi_post_ty: Vec<Option<u32>> = Vec::with_capacity(plans.len());
    for p in &plans {
        types.ty().function(
            val_types(&p.import_sig.params),
            val_types(&p.import_sig.results),
        );
        handler_ty.push(idx.alloc_ty());
        types.ty().function(
            val_types(&p.export_sig.params),
            val_types(&p.export_sig.results),
        );
        wrapper_ty.push(idx.alloc_ty());
        // sync canon-lift's `cabi_post_<fn>` takes the export's flat
        // results as params.
        if p.export_sig.retptr {
            types
                .ty()
                .function(val_types(&p.export_sig.results), std::iter::empty());
            cabi_post_ty.push(Some(idx.alloc_ty()));
        } else {
            cabi_post_ty.push(None);
        }
    }
    let async_types = canon_async::emit_types(&mut types, || idx.alloc_ty());
    types.ty().function(std::iter::empty(), std::iter::empty());
    let init_ty = idx.alloc_ty();
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        [ValType::I32],
    );
    let cabi_realloc_ty = idx.alloc_ty();
    module.section(&types);

    // -- Import section --
    let mut imports = ImportSection::new();
    let mut imp_idx: Vec<u32> = Vec::with_capacity(plans.len());
    for (i, p) in plans.iter().enumerate() {
        imports.import(
            &p.import_module,
            &p.import_field,
            EntityType::Function(handler_ty[i]),
        );
        imp_idx.push(idx.alloc_func());
    }
    let async_funcs =
        canon_async::import_intrinsics(&mut imports, &async_types, EVENT_PTR_OFFSET, || {
            idx.alloc_func()
        });
    module.section(&imports);

    // -- Function section --
    let mut fsec = FunctionSection::new();
    let wrapper_base = idx.func;
    for &ty in &wrapper_ty {
        fsec.function(ty);
        idx.alloc_func();
    }
    fsec.function(init_ty);
    let init_idx = idx.alloc_func();
    let mut cabi_post_idx: Vec<Option<u32>> = Vec::with_capacity(plans.len());
    for &cpt in &cabi_post_ty {
        if let Some(t) = cpt {
            fsec.function(t);
            cabi_post_idx.push(Some(idx.alloc_func()));
        } else {
            cabi_post_idx.push(None);
        }
    }
    fsec.function(cabi_realloc_ty);
    let cabi_realloc_idx = idx.alloc_func();
    module.section(&fsec);

    // -- Memory + globals --
    let globals = emit_memory_and_globals(&mut module, BUMP_START);

    // -- Export section --
    let wrappers: Vec<WrapperExport<'_>> = plans
        .iter()
        .enumerate()
        .map(|(i, p)| WrapperExport {
            export_name: &p.export_name,
            cabi_post_idx: cabi_post_idx[i],
        })
        .collect();
    emit_export_section(
        &mut module,
        &wrappers,
        wrapper_base,
        init_idx,
        cabi_realloc_idx,
    );

    // -- Code section --
    let mut code = CodeSection::new();
    for (i, p) in plans.iter().enumerate() {
        let body =
            build_async_lower_body(p, imp_idx[i], cabi_realloc_idx, globals.bump, &async_funcs);
        code.function(&body);
    }
    code.function(&empty_function());
    for p in &plans {
        if p.export_sig.retptr {
            code.function(&build_cabi_post_body(globals.bump));
        }
    }
    emit_cabi_realloc(&mut code, globals.bump);
    module.section(&code);

    Ok(module.finish())
}

/// Body for the bridge's exported sync wrapper. Does async canon-lower
/// onto the mirror import, blocks on the resulting subtask, then
/// delivers the result through the export's sync canon-lift shape:
///
/// 1. Save bump — into a local for non-retptr exports (restore inline
///    before flat return), into the static `SAVED_BUMP_OFFSET` slot
///    for retptr exports (`cabi_post_<fn>` restores).
/// 2. If the function has a result, `cabi_realloc` a retptr buffer of
///    the canonical result size/align and pass it tail-position to the
///    async-lower import.
/// 3. Call the import — it returns a packed `(handle << 4) | status_tag`
///    i32. `emit_wait_loop` drains the subtask via the `$root` async
///    intrinsics; after it returns the result buffer is populated.
/// 4. For retptr exports return the buffer pointer (host reads then
///    invokes `cabi_post_<fn>`). For non-retptr exports load the flat
///    result from the buffer using a load instruction matched to the
///    target's WIT type (so sub-byte signedness is preserved), then
///    restore bump.
fn build_async_lower_body(
    p: &FuncPlan,
    imp_idx: u32,
    cabi_realloc_idx: u32,
    bump_global: u32,
    art: &AsyncFuncs,
) -> Function {
    let n_export_params = p.export_sig.params.len() as u32;
    let export_retptr = p.export_sig.retptr;
    let mut locals = LocalsBuilder::new(n_export_params);
    // For non-retptr exports the bump restore happens inline; for
    // retptr exports `cabi_post_<fn>` reads from the static slot.
    let saved_bump_local = (!export_retptr).then(|| locals.alloc_local(ValType::I32));
    let retptr_local = locals.alloc_local(ValType::I32);
    let st = locals.alloc_local(ValType::I32);
    let ws = locals.alloc_local(ValType::I32);
    let mut f = Function::new_with_locals_types(locals.freeze().locals);

    // (1) Save bump.
    if let Some(sb) = saved_bump_local {
        f.instructions().global_get(bump_global);
        f.instructions().local_set(sb);
    } else {
        f.instructions().i32_const(SAVED_BUMP_OFFSET as i32);
        f.instructions().global_get(bump_global);
        f.instructions().i32_store(MemArg {
            offset: 0,
            align: I32_STORE_LOG2_ALIGN,
            memory_index: 0,
        });
    }

    // (2) Allocate the retptr buffer if there's a result.
    if let Some((size, align)) = p.result_size_align {
        emit_cabi_realloc_call(&mut f, cabi_realloc_idx, align, size, retptr_local);
    }

    // (3) Push params (+ retptr) and call the async-lower import.
    for px in 0..n_export_params {
        f.instructions().local_get(px);
    }
    if p.result_size_align.is_some() {
        f.instructions().local_get(retptr_local);
    }
    canon_async::emit_call_and_wait(&mut f, imp_idx, st, ws, art);

    // (4) Deliver the result through the export's shape.
    if export_retptr {
        f.instructions().local_get(retptr_local);
    } else if let Some(result_ty) = p.result_ty.as_ref() {
        let load_kind = flat_load_kind(result_ty, p.export_sig.results.first().copied())
            .expect("non-retptr export with result → primitive flat-load shape");
        f.instructions().local_get(retptr_local);
        emit_flat_load(&mut f, load_kind);
        let sb = saved_bump_local.expect("non-retptr → saved_bump_local");
        f.instructions().local_get(sb);
        f.instructions().global_set(bump_global);
    } else {
        let sb = saved_bump_local.expect("non-retptr → saved_bump_local");
        f.instructions().local_get(sb);
        f.instructions().global_set(bump_global);
    }
    f.instructions().end();
    f
}

/// Load-instruction selector for the non-retptr export path. Picks
/// the narrowest typed load that matches the target's WIT result type
/// so canonical-ABI sub-byte sign/zero extension is preserved at the
/// bridge's wasm-level return.
#[derive(Clone, Copy)]
enum FlatLoad {
    I32,
    I32Load8U,
    I32Load8S,
    I32Load16U,
    I32Load16S,
    I64,
    F32,
    F64,
}

fn flat_load_kind(result_ty: &Type, flat: Option<WasmType>) -> Result<FlatLoad> {
    let load = match (result_ty, flat) {
        (Type::Bool | Type::U8, Some(WasmType::I32)) => FlatLoad::I32Load8U,
        (Type::S8, Some(WasmType::I32)) => FlatLoad::I32Load8S,
        (Type::U16, Some(WasmType::I32)) => FlatLoad::I32Load16U,
        (Type::S16, Some(WasmType::I32)) => FlatLoad::I32Load16S,
        (Type::U32 | Type::S32 | Type::Char, Some(WasmType::I32)) => FlatLoad::I32,
        (Type::U64 | Type::S64, Some(WasmType::I64)) => FlatLoad::I64,
        (Type::F32, Some(WasmType::F32)) => FlatLoad::F32,
        (Type::F64, Some(WasmType::F64)) => FlatLoad::F64,
        _ => anyhow::bail!(
            "bridge non-retptr load not supported for result_ty={result_ty:?}, flat={flat:?}"
        ),
    };
    Ok(load)
}

fn emit_flat_load(f: &mut Function, kind: FlatLoad) {
    let arg0 = MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    };
    let arg1 = MemArg {
        offset: 0,
        align: 1,
        memory_index: 0,
    };
    let arg2 = MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    };
    let arg3 = MemArg {
        offset: 0,
        align: 3,
        memory_index: 0,
    };
    match kind {
        FlatLoad::I32Load8U => f.instructions().i32_load8_u(arg0),
        FlatLoad::I32Load8S => f.instructions().i32_load8_s(arg0),
        FlatLoad::I32Load16U => f.instructions().i32_load16_u(arg1),
        FlatLoad::I32Load16S => f.instructions().i32_load16_s(arg1),
        FlatLoad::I32 => f.instructions().i32_load(arg2),
        FlatLoad::I64 => f.instructions().i64_load(arg3),
        FlatLoad::F32 => f.instructions().f32_load(arg2),
        FlatLoad::F64 => f.instructions().f64_load(arg3),
    };
}

/// `cabi_post_<fn>` body: restore bump from the static saved-bump
/// slot, freeing the result buffer and any transient args allocated
/// during the corresponding wrapper call.
fn build_cabi_post_body(bump_global: u32) -> Function {
    let mut f = Function::new_with_locals_types([]);
    f.instructions().i32_const(0);
    f.instructions().i32_load(MemArg {
        offset: SAVED_BUMP_OFFSET,
        align: I32_STORE_LOG2_ALIGN,
        memory_index: 0,
    });
    f.instructions().global_set(bump_global);
    f.instructions().end();
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::typed::target_wit::test_fixture::component_from_wit;

    /// Sync interface with primitive-only params/result — exercises
    /// the non-retptr passthrough path.
    const SYNC_PRIMS_WIT: &str = r#"
        package test:demo@0.1.0;
        interface ops {
            add: func(a: u32, b: u32) -> u32;
            ping: func();
        }
        world demo {
            export ops;
        }
    "#;

    /// Sync interface that returns a string — exercises the retptr
    /// path (cabi_post emission + static-slot bump save/restore).
    const SYNC_STRING_WIT: &str = r#"
        package test:demo@0.1.0;
        interface ops {
            greet: func(name: string) -> string;
        }
        world demo { export ops; }
    "#;

    #[test]
    fn bridge_for_primitive_iface_encodes_and_validates() {
        let target_component = component_from_wit(SYNC_PRIMS_WIT, "demo").expect("synth fixture");
        let (bytes, _mirror) = build_bridge_bytes("test:demo/ops@0.1.0", &target_component)
            .expect("build bridge bytes");
        let _decoded = wit_component::decode(&bytes).expect("decode bridge component");
    }

    #[test]
    fn bridge_imports_mirror_and_exports_target() {
        let target_component = component_from_wit(SYNC_PRIMS_WIT, "demo").expect("synth fixture");
        let (bytes, mirror_qualified) =
            build_bridge_bytes("test:demo/ops@0.1.0", &target_component)
                .expect("build bridge bytes");
        assert!(
            mirror_qualified.starts_with("splicer:async-mirror-")
                && mirror_qualified.contains("/ops@"),
            "unexpected mirror name: {mirror_qualified}"
        );
        let decoded = wit_component::decode(&bytes).expect("decode bridge component");
        let wit_component::DecodedWasm::Component(resolve, world_id) = decoded else {
            panic!("expected component")
        };
        let world = &resolve.worlds[world_id];
        let imports: Vec<String> = world
            .imports
            .keys()
            .filter_map(|k| match k {
                wit_parser::WorldKey::Interface(id) => resolve.id_of(*id),
                _ => None,
            })
            .collect();
        let exports: Vec<String> = world
            .exports
            .keys()
            .filter_map(|k| match k {
                wit_parser::WorldKey::Interface(id) => resolve.id_of(*id),
                _ => None,
            })
            .collect();
        assert!(
            exports.iter().any(|e| e == "test:demo/ops@0.1.0"),
            "expected target export, got: {exports:?}"
        );
        assert!(
            imports.iter().any(|i| i == &mirror_qualified),
            "expected import matching returned mirror name {mirror_qualified}, got: {imports:?}"
        );
    }

    /// Retptr path: a string-returning function should encode + validate
    /// end-to-end through `ComponentEncoder::validate(true)` — the
    /// retptr-buffer alloc + sig translation must produce a
    /// well-formed component.
    #[test]
    fn bridge_for_string_return_encodes_and_emits_cabi_post() {
        let target_component = component_from_wit(SYNC_STRING_WIT, "demo").expect("synth fixture");
        let (bytes, _) = build_bridge_bytes("test:demo/ops@0.1.0", &target_component)
            .expect("build bridge bytes");
        let _decoded = wit_component::decode(&bytes).expect("decode bridge component");
    }

    /// Non-retptr export must NOT emit a `cabi_post_*` (tier-1 /
    /// tier-2 invariant). Walk the decoded component's core module
    /// exports.
    #[test]
    fn non_retptr_target_omits_cabi_post() {
        let target_component = component_from_wit(SYNC_PRIMS_WIT, "demo").expect("synth fixture");
        let (bytes, _) = build_bridge_bytes("test:demo/ops@0.1.0", &target_component)
            .expect("build bridge bytes");
        let core_exports = extract_core_export_names(&bytes);
        assert!(
            !core_exports.iter().any(|n| n.starts_with("cabi_post_")),
            "non-retptr exports should not produce cabi_post_*; got: {core_exports:?}"
        );
    }

    /// Retptr export MUST emit a `cabi_post_*` (callee allocates the
    /// result buffer; the post-return shim is what restores bump).
    #[test]
    fn retptr_target_emits_cabi_post() {
        let target_component = component_from_wit(SYNC_STRING_WIT, "demo").expect("synth fixture");
        let (bytes, _) = build_bridge_bytes("test:demo/ops@0.1.0", &target_component)
            .expect("build bridge bytes");
        let core_exports = extract_core_export_names(&bytes);
        assert!(
            core_exports.iter().any(|n| n.starts_with("cabi_post_")),
            "retptr export should produce cabi_post_*; got: {core_exports:?}"
        );
    }

    #[test]
    fn bridge_resource_bound_target_bails() {
        const WIT: &str = r#"
            package test:demo@0.1.0;
            interface ops {
                resource client {
                    constructor(name: string);
                    ping: func();
                }
            }
            world demo { export ops; }
        "#;
        let target_component = component_from_wit(WIT, "demo").expect("synth fixture");
        let err = build_bridge_bytes("test:demo/ops@0.1.0", &target_component).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("resource-bound function"),
            "expected mirror-synth bail, got: {msg}"
        );
    }

    /// Walk a component's bytes, find the first embedded core module,
    /// collect its export names. Used by the cabi_post emission tests
    /// to assert against the actual bytecode (instead of the WAT
    /// pre-encode form, which doesn't exist in the wasm-encoder pipeline).
    fn extract_core_export_names(component_bytes: &[u8]) -> Vec<String> {
        use wasmparser::{Parser, Payload};
        let mut names = Vec::new();
        for payload in Parser::new(0).parse_all(component_bytes) {
            let payload = payload.expect("parse component payload");
            if let Payload::ModuleSection {
                unchecked_range, ..
            } = payload
            {
                let module_bytes = &component_bytes[unchecked_range];
                for sub in Parser::new(0).parse_all(module_bytes) {
                    if let Payload::ExportSection(reader) = sub.expect("parse module payload") {
                        for export in reader {
                            names.push(export.expect("export").name.to_string());
                        }
                        return names;
                    }
                }
            }
        }
        names
    }
}
