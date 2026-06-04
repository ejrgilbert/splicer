//! Sync→async bridge component generator.
//!
//! When a middleware adapter needs to lift an async-WIT mirror so its
//! hook bodies can suspend, but the target interface itself is sync at
//! WIT, the bridge sits at the head of the chain: it exports the
//! target's sync-WIT interface (so callers see no change) and imports
//! the async-WIT mirror (which the adapter exports). For each
//! function the bridge body sync-canon-lowers the args straight onto
//! the mirror import. The bridge is the only sync-lifted component in
//! the chain; its sync→async canon-lower drives the async callee on a
//! fiber, never attempting to suspend a sync-rooted wasm task.

use anyhow::{Context, Result};

use super::super::abi::emit::{
    emit_cabi_realloc, emit_cabi_realloc_call, emit_export_section, emit_memory_and_globals,
    empty_function, val_types, WrapperExport,
};
use super::super::indices::{DispatchIndices, LocalsBuilder};
use super::super::resolve::{decode_input_resolve, find_target_interface};
use super::{short_hash_hex, synthesize_async_mirror};
use crate::adapter::sanitize_name;
use wasm_encoder::{
    CodeSection, EntityType, Function, FunctionSection, ImportSection, MemArg, Module, TypeSection,
    ValType,
};
use wit_component::{embed_component_metadata, ComponentEncoder, StringEncoding};
use wit_parser::abi::WasmSignature;
use wit_parser::{
    Function as WitFunction, InterfaceId, LiftLowerAbi, ManglingAndAbi, Resolve, SizeAlign,
    WasmExport, WasmExportKind, WasmImport, WorldKey,
};

/// Generate a sync→async bridge component for `target_interface`
/// against the WIT carried in `target_split_path`. Writes the bridge
/// `.wasm` under `splits_output_path` and returns
/// `(bridge_path, async_mirror_qualified_name)`.
///
/// `async_mirror_qualified_name` is the WIT name the bridge imports;
/// the same name must appear on the adapter's export side so wac
/// compose can wire them together. Returning it (instead of forcing
/// callers to re-derive via the same hash) keeps adapter codegen
/// from depending on the bridge module's hashing scheme.
#[allow(dead_code)]
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

/// Memory layout: word 0 (4 bytes) is reserved as the saved-bump
/// snapshot used by retptr-returning wrappers (saved at entry,
/// restored from `cabi_post_<fn>`). The bump allocator starts above
/// that, at 8 (i32-aligned room for future static slots).
const SAVED_BUMP_OFFSET: u64 = 0;
const I32_STORE_LOG2_ALIGN: u32 = 2;
const BUMP_START: u32 = 8;

fn synthesize_bridge_world_wit(target_interface: &str, mirror_qualified: &str) -> String {
    format!(
        "package {BRIDGE_WORLD_PACKAGE};\n\n\
         world {BRIDGE_WORLD_NAME} {{\n\
        \x20   export {target_interface};\n\
        \x20   import {mirror_qualified};\n\
         }}\n",
    )
}

/// Per-function plan — sigs, mangled names, and retptr size/align
/// (when the export uses callee-allocates retptr). Drives both the
/// type-section build and the body emit.
struct FuncPlan {
    export_name: String,
    import_module: String,
    import_field: String,
    export_sig: WasmSignature,
    import_sig: WasmSignature,
    /// `Some` iff `export_sig.retptr` — `(size, align)` of the result
    /// type's canonical-ABI layout, used to `cabi_realloc` the result
    /// buffer at runtime in the wrapper body.
    result_size_align: Option<(u32, u32)>,
}

fn gather_func_plans(
    resolve: &Resolve,
    target_iface_id: InterfaceId,
    mirror_iface_id: InterfaceId,
) -> Result<Vec<FuncPlan>> {
    let mangling = ManglingAndAbi::Legacy(LiftLowerAbi::Sync);
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
            let export_sig = resolve.wasm_signature(mangling.export_variant(), tfunc);
            let import_sig = resolve.wasm_signature(mangling.import_variant(), mfunc);
            let export_name = resolve.wasm_export_name(
                mangling,
                WasmExport::Func {
                    interface: Some(&target_key),
                    func: tfunc,
                    kind: WasmExportKind::Normal,
                },
            );
            let (import_module, import_field) = resolve.wasm_import_name(
                mangling,
                WasmImport::Func {
                    interface: Some(&mirror_key),
                    func: mfunc,
                },
            );
            let result_size_align = if export_sig.retptr {
                let result_ty = tfunc.result.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "retptr=true on `{}` but func.result is None — \
                         wit-parser invariant broken",
                        tfunc.name,
                    )
                })?;
                Some((
                    sizes.size(result_ty).size_wasm32() as u32,
                    sizes.align(result_ty).align_wasm32() as u32,
                ))
            } else {
                None
            };
            Ok(FuncPlan {
                export_name,
                import_module,
                import_field,
                export_sig,
                import_sig,
                result_size_align,
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

    // ── Type section ──
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
        // results as params (matches the convention `dummy_module` /
        // `tier1` follow).
        if p.export_sig.retptr {
            types
                .ty()
                .function(val_types(&p.export_sig.results), std::iter::empty());
            cabi_post_ty.push(Some(idx.alloc_ty()));
        } else {
            cabi_post_ty.push(None);
        }
    }
    types.ty().function(std::iter::empty(), std::iter::empty());
    let init_ty = idx.alloc_ty();
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        [ValType::I32],
    );
    let cabi_realloc_ty = idx.alloc_ty();
    module.section(&types);

    // ── Import section ──
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
    module.section(&imports);

    // ── Function section ──
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

    // ── Memory + globals ──
    let globals = emit_memory_and_globals(&mut module, BUMP_START);

    // ── Export section ──
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

    // ── Code section ──
    let mut code = CodeSection::new();
    for (i, p) in plans.iter().enumerate() {
        let body = if p.export_sig.retptr {
            build_retptr_wrapper_body(
                p.export_sig.params.len() as u32,
                imp_idx[i],
                cabi_realloc_idx,
                p.result_size_align
                    .expect("result_size_align populated whenever retptr=true"),
                globals.bump,
            )
        } else {
            build_non_retptr_wrapper_body(
                p.export_sig.params.len() as u32,
                imp_idx[i],
                p.export_sig.results.len() as u32,
                globals.bump,
            )
        };
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

/// Body for the non-retptr export wrapper: result (if any) is on the
/// wasm stack with no memory lifetime past the wrapper's return.
/// Save bump into a local at entry, restore at exit. The restore
/// pushes the saved value then consumes it via `global.set`, leaving
/// the import's flat result on top of the stack as the wasm-level
/// return value.
fn build_non_retptr_wrapper_body(
    n_params: u32,
    imp_idx: u32,
    _n_results: u32,
    bump_global: u32,
) -> Function {
    let mut locals = LocalsBuilder::new(n_params);
    let saved_bump = locals.alloc_local(ValType::I32);
    let mut f = Function::new_with_locals_types(locals.freeze().locals);
    f.instructions().global_get(bump_global);
    f.instructions().local_set(saved_bump);
    for p in 0..n_params {
        f.instructions().local_get(p);
    }
    f.instructions().call(imp_idx);
    f.instructions().local_get(saved_bump);
    f.instructions().global_set(bump_global);
    f.instructions().end();
    f
}

/// Body for the retptr export wrapper. Sync canon-lift uses
/// callee-allocates (the function returns a pointer it allocated);
/// sync canon-lower uses caller-allocates (we pass a retptr param
/// the import writes into). Bridge:
///
/// 1. Save bump into the static saved-bump slot — `cabi_post_<fn>`
///    will restore from it, freeing the result buffer plus any
///    transient args the host's canon-lift allocated.
/// 2. `cabi_realloc` a result buffer sized to the result type.
/// 3. Pass `(orig_params, retptr)` to the sync canon-lower import.
/// 4. Return the retptr — the export's callee-allocates result.
fn build_retptr_wrapper_body(
    n_export_params: u32,
    imp_idx: u32,
    cabi_realloc_idx: u32,
    result_size_align: (u32, u32),
    bump_global: u32,
) -> Function {
    let (result_size, result_align) = result_size_align;
    let mut locals = LocalsBuilder::new(n_export_params);
    let retptr_local = locals.alloc_local(ValType::I32);
    let mut f = Function::new_with_locals_types(locals.freeze().locals);

    // mem[SAVED_BUMP_OFFSET..+4] = bump
    f.instructions().i32_const(0);
    f.instructions().global_get(bump_global);
    f.instructions().i32_store(MemArg {
        offset: SAVED_BUMP_OFFSET,
        align: I32_STORE_LOG2_ALIGN,
        memory_index: 0,
    });

    emit_cabi_realloc_call(
        &mut f,
        cabi_realloc_idx,
        result_align,
        result_size,
        retptr_local,
    );

    for p in 0..n_export_params {
        f.instructions().local_get(p);
    }
    f.instructions().local_get(retptr_local);
    f.instructions().call(imp_idx);
    f.instructions().local_get(retptr_local);
    f.instructions().end();
    f
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
