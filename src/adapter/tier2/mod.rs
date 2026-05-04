//! Tier-2 adapter generator: lifts canonical-ABI values from the
//! target function's parameters/result into the structural cell-array
//! representation defined in `splicer:common/types`, then dispatches
//! the lifted values to the middleware's tier-2 hooks.
//!
//! Status: scaffold + primitives only. Compound kinds, full hook
//! dispatch, and resource/stream/future handle correlation are
//! tracked in `docs/tiers/lift-codegen.md`.
//!
//! Pipeline (driven by [`build_dispatch_module`]):
//! 1. Classify — [`build_per_func_classified`] walks each target
//!    function's params/result and produces a [`FuncClassified`]
//!    list. No static-memory offsets yet.
//! 2. Layout — [`layout::lay_out_static_memory`] reserves data +
//!    scratch slabs, populates side-table blobs, and assembles the
//!    immutable [`FuncDispatch`] list (classify data + offsets).
//! 3. Emit — [`section_emit`] writes the wasm sections;
//!    [`wrapper_body::emit_wrapper_function`] generates each
//!    wrapper's body.
//!
//! Submodules:
//! - [`blob`] — typed data-segment packing helpers (`BlobSlice`,
//!   `RecordWriter`); the data-side analogue of [`cells::CellLayout`].
//! - [`cells`] — emit helpers for constructing individual `cell`
//!   variant cases in the canonical-ABI memory layout (one helper
//!   per primitive case so far).
//! - [`lift`] — lift classification (`LiftKind`), per-(param|result)
//!   lift descriptors, side-table population, and the wasm-encoder
//!   codegen that writes one cell per lifted value.
//! - [`schema`] — `splicer:common/types` typedef layouts + tier-2
//!   hook-import resolution.
//! - [`layout`] — static-memory layout phase (`lay_out_static_memory`
//!   + blob builders).
//! - [`section_emit`] — wasm section emitters (types, imports,
//!   exports, code, data).
//! - [`wrapper_body`] — per-wrapper body generation.

pub(super) mod blob;
pub(super) mod cells;
pub(super) mod layout;
pub(super) mod lift;
pub(super) mod schema;
pub(super) mod section_emit;
pub(super) mod wrapper_body;

use anyhow::{bail, Context, Result};
use wasm_encoder::Module;
use wit_component::{embed_component_metadata, ComponentEncoder, StringEncoding};
use wit_parser::abi::{AbiVariant, WasmSignature};
use wit_parser::{
    Function as WitFunction, InterfaceId, Mangling, Resolve, Type, WasmExport, WasmExportKind,
    WasmImport, WorldKey,
};

use super::abi::emit::{emit_memory_and_globals, BlobSlice};
use super::resolve::{decode_input_resolve, dispatch_mangling, find_target_interface};
use layout::lay_out_static_memory;
use lift::{classify_func_params, classify_result_lift, ParamLift, ResultLift};
use schema::compute_schema;
use section_emit::{
    emit_code_section, emit_data_section, emit_export_section, emit_imports_and_funcs,
    emit_type_section,
};
use wrapper_body::WrapperCtx;

const TIER2_ADAPTER_WORLD_PACKAGE: &str = "splicer:adapter-tier2";
const TIER2_ADAPTER_WORLD_NAME: &str = "adapter";

/// Generate a tier-2 adapter component.
pub(super) fn build_tier2_adapter(
    target_interface: &str,
    has_before: bool,
    has_after: bool,
    split_bytes: &[u8],
    common_wit: &str,
    tier2_wit: &str,
) -> Result<Vec<u8>> {
    if !has_before && !has_after {
        bail!(
            "tier-2 adapter generation requires the middleware to export at least \
             one of `splicer:tier2/before` or `splicer:tier2/after` — `trap`-only \
             middleware is planned for a follow-up slice."
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
            &synthesize_adapter_world_wit(target_interface, has_before, has_after),
        )
        .context("parse synthesized tier-2 adapter world WIT")?;
    let world_id = resolve
        .select_world(&[world_pkg], Some(TIER2_ADAPTER_WORLD_NAME))
        .context("select tier-2 adapter world")?;

    let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
        .functions
        .values()
        .collect();
    let schema = compute_schema(&resolve, world_id, has_before, has_after)?;

    let iface_name = BlobSlice {
        off: 0,
        len: target_interface.len() as u32,
    };
    let mut name_blob: Vec<u8> = target_interface.as_bytes().to_vec();
    let classified = build_per_func_classified(&resolve, target_iface, &funcs, &mut name_blob)?;

    let (per_func, plan) =
        lay_out_static_memory(classified, &funcs, &schema, &mut name_blob, iface_name)?;

    let mut core_module = build_dispatch_module(&resolve, &schema, &per_func, plan, iface_name);
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
    Ok(())
}

/// Synthesize the tier-2 adapter world.
fn synthesize_adapter_world_wit(
    target_interface: &str,
    has_before: bool,
    has_after: bool,
) -> String {
    use crate::contract::{versioned_interface, TIER2_AFTER, TIER2_BEFORE, TIER2_VERSION};
    let mut wit =
        format!("package {TIER2_ADAPTER_WORLD_PACKAGE};\n\nworld {TIER2_ADAPTER_WORLD_NAME} {{\n");
    wit.push_str(&format!("    import {target_interface};\n"));
    wit.push_str(&format!("    export {target_interface};\n"));
    if has_before {
        wit.push_str(&format!(
            "    import {};\n",
            versioned_interface(TIER2_BEFORE, TIER2_VERSION)
        ));
    }
    if has_after {
        wit.push_str(&format!(
            "    import {};\n",
            versioned_interface(TIER2_AFTER, TIER2_VERSION)
        ));
    }
    wit.push_str("}\n");
    wit
}

/// Drive the section emitters in the right order to produce the
/// dispatch core module bytes.
fn build_dispatch_module(
    resolve: &Resolve,
    schema: &schema::SchemaLayouts,
    per_func: &[FuncDispatch],
    plan: layout::StaticDataPlan,
    iface_name: BlobSlice,
) -> Vec<u8> {
    let mut module = Module::new();
    let type_idx = emit_type_section(
        &mut module,
        per_func,
        schema.before_hook.as_ref().map(|h| &h.sig),
        schema.after_hook.as_ref().map(|h| &h.sig),
    );
    let func_idx = emit_imports_and_funcs(
        &mut module,
        per_func,
        &type_idx,
        schema.before_hook.as_ref(),
        schema.after_hook.as_ref(),
        plan.event_ptr,
    );
    let globals = emit_memory_and_globals(&mut module, plan.bump_start);
    emit_export_section(
        &mut module,
        per_func,
        func_idx.wrapper_base,
        func_idx.init_idx,
        func_idx.cabi_realloc_idx,
    );
    let wrapper_ctx = WrapperCtx {
        schema,
        resolve,
        iface_name,
        hook_params_ptr: plan.hook_params_ptr as i32,
        call_id_counter_global: globals.call_id_counter,
    };
    emit_code_section(&mut module, per_func, &func_idx, &wrapper_ctx, &globals);
    emit_data_section(&mut module, &plan.data_segments);
    module.finish()
}

// ─── Phase data shared across submodules ──────────────────────────

// Phase-data types (`FuncShape`, `FuncClassified`, `FuncDispatch`,
// etc.) carry tier2-internal types from `lift` (`ParamLift`,
// `ResultLayout`, …) which are `pub(super)` from inside `lift`,
// i.e. visible in `crate::adapter::tier2`. To make the field
// visibility match what they expose (and silence the
// "type X is more private than item Y" lint), the struct itself is
// scoped to `pub(in crate::adapter::tier2)`; individual fields can
// stay plain `pub` because the struct visibility narrows them.

/// `task.return` import for one async target function. The wrapper
/// body calls this at the end of an async dispatch to publish the
/// result.
pub(in crate::adapter::tier2) struct TaskReturnImport {
    pub module: String,
    pub name: String,
    pub sig: WasmSignature,
}

/// Sync/async shape of one target function. Holds the
/// `task.return` import directly in the async variant — there's no
/// "async without task.return" or "sync with task.return" state.
pub(in crate::adapter::tier2) enum FuncShape {
    Sync,
    Async(TaskReturnImport),
}

impl FuncShape {
    /// Classify a function as sync or async, eagerly resolving the
    /// `task.return` import for the async case.
    fn classify(resolve: &Resolve, target_world_key: &WorldKey, func: &WitFunction) -> Self {
        if func.kind.is_async() {
            let (module, name, sig) =
                func.task_return_import(resolve, Some(target_world_key), Mangling::Legacy);
            FuncShape::Async(TaskReturnImport { module, name, sig })
        } else {
            FuncShape::Sync
        }
    }

    fn is_async(&self) -> bool {
        matches!(self, FuncShape::Async(_))
    }

    pub fn task_return(&self) -> Option<&TaskReturnImport> {
        match self {
            FuncShape::Async(tr) => Some(tr),
            FuncShape::Sync => None,
        }
    }

    /// `(import_variant, export_variant)` to pass to
    /// `Resolve::wasm_signature` for this shape.
    fn abi_variants(&self) -> (AbiVariant, AbiVariant) {
        match self {
            FuncShape::Async(_) => (
                AbiVariant::GuestImportAsync,
                AbiVariant::GuestExportAsyncStackful,
            ),
            FuncShape::Sync => (AbiVariant::GuestImport, AbiVariant::GuestExport),
        }
    }

    /// Whether the wrapper export needs a `cabi_post_*` companion.
    /// Async exports never do (results land via `task.return`); sync
    /// exports do iff the export sig retptr's the result.
    fn needs_cabi_post(&self, export_sig: &WasmSignature) -> bool {
        match self {
            FuncShape::Async(_) => false,
            FuncShape::Sync => export_sig.retptr,
        }
    }

    /// Whether the function's result lives at retptr scratch (vs.
    /// flat return-value slots). Async funcs use the import-sig
    /// retptr (canon-lower-async always retptr's a non-void result);
    /// sync funcs use the export-sig retptr.
    fn result_at_retptr(&self, export_sig: &WasmSignature, import_sig: &WasmSignature) -> bool {
        match self {
            FuncShape::Async(_) => import_sig.retptr,
            FuncShape::Sync => export_sig.retptr,
        }
    }
}

/// Per-function on-return hook setup, populated only when the
/// middleware exports `splicer:tier2/after`. Bundles the two
/// always-paired offsets (after-hook indirect-params buffer +
/// optional result-cell scratch) so callers can branch on
/// `Option<AfterSetup>` without separate "is after wired?" /
/// "does this fn have a result?" checks.
pub(in crate::adapter::tier2) struct AfterSetup {
    /// Byte offset of the pre-built on-return indirect-params buffer.
    pub params_offset: i32,
    /// Byte offset of the 1-cell result scratch slab. `None` for
    /// void-returning funcs (still need params_offset, but no result
    /// to lift).
    pub result_cells_offset: Option<u32>,
}

/// Classify-phase per-function output. Holds everything the layout
/// phase needs to compute static-memory offsets, but no offsets
/// itself. The layout phase consumes a `Vec<FuncClassified>` by
/// value and returns a `Vec<FuncDispatch>` whose offsets are filled
/// in once and immutable thereafter. This split is what makes
/// "back-fill across phase boundaries" structurally impossible:
/// there's nowhere on `FuncClassified` to write a layout offset to.
pub(in crate::adapter::tier2) struct FuncClassified {
    pub shape: FuncShape,
    /// WIT result type, kept around so async wrappers can drive
    /// `lift_from_memory` to flat-load the result for `task.return`.
    pub result_ty: Option<Type>,
    pub import_module: String,
    pub import_field: String,
    pub export_name: String,
    /// Wrapper export sig (`AbiVariant::GuestExport`).
    pub export_sig: WasmSignature,
    /// Handler import sig (`AbiVariant::GuestImport`).
    pub import_sig: WasmSignature,
    pub needs_cabi_post: bool,
    /// Byte offset of the function name within the data segment.
    pub fn_name_offset: i32,
    pub fn_name_len: i32,
    /// Per-param classify-time lift recipe (no offsets).
    pub params: Vec<ParamLift>,
    /// Classify-time return-value lift recipe (no offsets).
    pub result_lift: Option<ResultLift>,
}

/// Layout-phase per-function output: the classify data plus every
/// static-memory offset the emit phase needs. Constructed once at
/// the end of `lay_out_static_memory`; read-only thereafter.
pub(in crate::adapter::tier2) struct FuncDispatch {
    pub shape: FuncShape,
    /// WIT result type, kept around so async wrappers can drive
    /// `lift_from_memory` to flat-load the result for `task.return`.
    pub result_ty: Option<Type>,
    pub import_module: String,
    pub import_field: String,
    pub export_name: String,
    /// Wrapper export sig (`AbiVariant::GuestExport`) — the shape
    /// `wit-component`'s validator expects for our exported wrapper.
    pub export_sig: WasmSignature,
    /// Handler import sig (`AbiVariant::GuestImport`) — the shape
    /// `wit-component`'s validator expects for our import declaration.
    /// May differ from `export_sig` for compound-result functions
    /// (caller-allocates retptr on the import side vs. callee-returns
    /// pointer on the export side).
    pub import_sig: WasmSignature,
    pub needs_cabi_post: bool,
    /// Byte offset of the function name within the data segment.
    pub fn_name_offset: i32,
    pub fn_name_len: i32,
    /// Per-param post-layout lift recipe (classify data + offsets).
    /// Empty for zero-arg functions. Each `ParamLayout::cells_offset`
    /// holds the offset of its own cells slab — there's no shared
    /// per-fn slab base, since record params consume more than one
    /// cell.
    pub params: Vec<lift::ParamLayout>,
    /// Byte offset of this function's pre-built `field` records in
    /// the data segment. Holds `params.len()` consecutive `field`
    /// records, each [`schema::SchemaLayouts::field_layout`]`.size`
    /// bytes. Pointed at by the `args.list.ptr` field passed to
    /// `on-call`.
    pub fields_buf_offset: u32,
    /// Byte offset of the retptr scratch buffer; `Some` iff the
    /// import sig wants a caller-allocates retptr but the export sig
    /// returns the pointer directly. The wrapper passes this as the
    /// extra trailing arg when calling the import, then loads from it
    /// to produce its own return value.
    pub retptr_offset: Option<i32>,
    /// How to lift the function's return value into a `cell` for the
    /// on-return hook. `None` for void or compound returns we don't
    /// yet lift.
    pub result_lift: Option<lift::ResultLayout>,
    /// On-return-hook scaffolding; `Some` iff after-hook is wired.
    pub after: Option<AfterSetup>,
}

/// Build the per-target-function classify records: classify each
/// param, populate the WIT-derived sigs and mangled names, classify
/// the result for on-return lift. Output has no static-memory
/// offsets — those are computed by [`layout::lay_out_static_memory`],
/// which consumes the `Vec<FuncClassified>` and returns a parallel
/// `Vec<FuncDispatch>` with the offsets filled in. Appends fn names
/// + param names to `name_blob` as it goes.
fn build_per_func_classified(
    resolve: &Resolve,
    target_iface: InterfaceId,
    funcs: &[&WitFunction],
    name_blob: &mut Vec<u8>,
) -> Result<Vec<FuncClassified>> {
    let target_world_key = WorldKey::Interface(target_iface);
    let mut per_func: Vec<FuncClassified> = Vec::with_capacity(funcs.len());

    for func in funcs {
        let fn_name_offset = name_blob.len() as i32;
        let fn_name_len = func.name.len() as i32;
        name_blob.extend_from_slice(func.name.as_bytes());

        let params_lift = classify_func_params(resolve, func, name_blob);
        let shape = FuncShape::classify(resolve, &target_world_key, func);
        let (import_variant, export_variant) = shape.abi_variants();
        let mangling = dispatch_mangling(shape.is_async());

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
        let needs_cabi_post = shape.needs_cabi_post(&export_sig);
        let result_lift = classify_result_lift(
            resolve,
            func,
            shape.result_at_retptr(&export_sig, &import_sig),
        );

        per_func.push(FuncClassified {
            shape,
            result_ty: func.result,
            import_module,
            import_field,
            export_name,
            export_sig,
            import_sig,
            needs_cabi_post,
            fn_name_offset,
            fn_name_len,
            params: params_lift,
            result_lift,
        });
    }
    Ok(per_func)
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
