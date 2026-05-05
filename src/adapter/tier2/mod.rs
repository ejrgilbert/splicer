//! Tier-2 adapter generator: lifts canonical-ABI values from the
//! target function's parameters/result into the structural cell-array
//! representation defined in `splicer:common/types`, then dispatches
//! the lifted values to the middleware's tier-2 hooks.
//!
//! Wired: primitives, `string`, `list<u8>`, `enum`, `record`,
//! `tuple<...>` (both sides, sync + async). Remaining `Cell` variants
//! `todo!()` in [`lift::plan::LiftPlanBuilder`] / [`cells::CellLayout`].
//! Roadmap: `docs/tiers/lift-codegen.md`.
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
//! - [`lift`] — lift classification (`Cell`), per-(param|result)
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
#[cfg(test)]
mod test_utils;
pub(super) mod wrapper_body;

use anyhow::{bail, Context, Result};
use wasm_encoder::Module;
use wit_component::{embed_component_metadata, ComponentEncoder, StringEncoding};
use wit_parser::abi::{AbiVariant, WasmSignature};
use wit_parser::{
    Function as WitFunction, InterfaceId, Mangling, Resolve, Type, WasmExport, WasmExportKind,
    WasmImport, WorldKey,
};

use super::abi::emit::{
    emit_data_section, emit_export_section, emit_memory_and_globals, synthesize_adapter_world_wit,
    BlobSlice,
};
use super::resolve::{decode_input_resolve, dispatch_mangling, find_target_interface};
use blob::NameInterner;
use layout::lay_out_static_memory;
use lift::{classify_func_params, classify_result_lift, ParamLift, ResultLift};
use schema::compute_schema;
use section_emit::{emit_code_section, emit_imports_and_funcs, emit_type_section, wrapper_exports};
use wrapper_body::{AfterHook, BeforeHook, WrapperCtx};

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
            &synthesize_adapter_world_wit(
                TIER2_ADAPTER_WORLD_PACKAGE,
                TIER2_ADAPTER_WORLD_NAME,
                target_interface,
                &tier2_hook_imports(has_before, has_after),
            ),
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

    let mut names = NameInterner::new();
    let iface_name = names.intern(target_interface);
    let classified = build_per_func_classified(&resolve, target_iface, &funcs, &mut names)?;

    let (per_func, plan) = lay_out_static_memory(classified, &funcs, &schema, names, iface_name)?;

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
    // Async funcs whose flat params overflow MAX_FLAT_ASYNC_PARAMS need
    // lower-to-memory; not yet implemented. Mirrors tier-1.
    for (name, func) in &iface.functions {
        if func.kind.is_async() {
            let import_sig = resolve.wasm_signature(AbiVariant::GuestImportAsync, func);
            if import_sig.indirect_params {
                bail!(
                    "async function `{name}` has params that overflow \
                     MAX_FLAT_ASYNC_PARAMS ({}) and require lower-to-memory; \
                     not yet implemented",
                    Resolve::MAX_FLAT_ASYNC_PARAMS
                );
            }
        }
    }
    Ok(())
}

/// Synthesize the tier-2 adapter world.
/// Active tier-2 hook interfaces as fully-qualified versioned names.
fn tier2_hook_imports(has_before: bool, has_after: bool) -> Vec<String> {
    use crate::contract::{versioned_interface, TIER2_AFTER, TIER2_BEFORE, TIER2_VERSION};
    let mut out = Vec::new();
    if has_before {
        out.push(versioned_interface(TIER2_BEFORE, TIER2_VERSION));
    }
    if has_after {
        out.push(versioned_interface(TIER2_AFTER, TIER2_VERSION));
    }
    out
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
        schema.before_hook.as_ref().map(|h| &h.import.sig),
        schema.after_hook.as_ref().map(|h| &h.import.sig),
    );
    let func_idx = emit_imports_and_funcs(
        &mut module,
        per_func,
        &type_idx,
        schema.before_hook.as_ref().map(|h| &h.import),
        schema.after_hook.as_ref().map(|h| &h.import),
        plan.event_ptr,
    );
    let globals = emit_memory_and_globals(&mut module, plan.bump_start);
    let wrapper_exports = wrapper_exports(per_func, func_idx.init_idx);
    emit_export_section(
        &mut module,
        &wrapper_exports,
        func_idx.wrapper_base,
        func_idx.init_idx,
        func_idx.cabi_realloc_idx,
    );
    // Zip the per-build hook pieces (schema layout, import idx,
    // params-buffer offset) into one `Option<BeforeHook>` /
    // `Option<AfterHook>`. The unreachable arms encode the
    // "wired together or not at all" contract that today is only
    // implicit in the construction order of `emit_imports_and_funcs`
    // and `lay_out_static_memory`.
    let before_hook = match (
        schema.before_hook.as_ref(),
        func_idx.before_hook_idx,
        plan.hook_params_ptr,
    ) {
        (Some(h), Some(idx), Some(params_ptr)) => Some(BeforeHook {
            idx,
            layout: &h.params_layout,
            params_ptr: params_ptr as i32,
        }),
        (None, None, None) => None,
        _ => unreachable!("before-hook schema, import idx, and params-ptr wired in lockstep"),
    };
    let after_hook = match (schema.after_hook.as_ref(), func_idx.after_hook_idx) {
        (Some(h), Some(idx)) => Some(AfterHook {
            idx,
            layout: &h.params_layout,
        }),
        (None, None) => None,
        _ => unreachable!("after-hook schema and import idx wired in lockstep"),
    };
    let wrapper_ctx = WrapperCtx {
        schema,
        resolve,
        iface_name,
        before_hook,
        after_hook,
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

/// Per-function on-return hook offsets, populated only when the
/// middleware exports `splicer:tier2/after`. Pairs with the per-build
/// [`wrapper_body::AfterHook`] (import idx + on-return params layout)
/// at emit time; the two `Option`s are populated together so callers
/// branch once on `(Some, Some)` rather than threading separate
/// "is after wired?" / "does this fn have a result?" checks.
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
/// `Vec<FuncDispatch>` with the offsets filled in. Interns fn names,
/// param names, and any record/field names referenced by lift plans
/// into `names` as it goes.
fn build_per_func_classified(
    resolve: &Resolve,
    target_iface: InterfaceId,
    funcs: &[&WitFunction],
    names: &mut NameInterner,
) -> Result<Vec<FuncClassified>> {
    let target_world_key = WorldKey::Interface(target_iface);
    let mut per_func: Vec<FuncClassified> = Vec::with_capacity(funcs.len());

    for func in funcs {
        let fn_name_slice = names.intern(&func.name);

        let params_lift = classify_func_params(resolve, func, names);
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
            names,
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
            fn_name_offset: fn_name_slice.off as i32,
            fn_name_len: fn_name_slice.len as i32,
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

    /// End-to-end test for `Cell::TupleOf`: plan-builder + tuple-
    /// indices side-table + emit-phase `(ptr, len)` const writes,
    /// validated through ComponentEncoder.
    #[test]
    fn dispatch_module_with_tuple_param_roundtrips() {
        // Flat `tuple<u32, s32>` param + void return; no canonical
        // option `memory` needed for the WAT's lift.
        let wat = r#"(component
            (component $inner
                (core module $m
                    (func (export "consume") (param i32 i32))
                )
                (core instance $i (instantiate $m))
                (alias core export $i "consume" (core func $consume))
                (type $consume-ty (func (param "t" (tuple u32 s32))))
                (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
                (instance $api-inst (export "consume" (func $consume-lifted)))
                (export "my:tup/api@1.0.0" (instance $api-inst))
            )
            (instance $api (instantiate $inner))
            (export "my:tup/api@1.0.0" (instance $api "my:tup/api@1.0.0"))
        )"#;
        let split_bytes = wat::parse_str(wat).expect("WAT must parse");

        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");

        let bytes = build_tier2_adapter(
            "my:tup/api@1.0.0",
            true,
            true,
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect("tier-2 adapter generation should succeed for tuple param");

        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("emitted tier-2 adapter component should validate");
    }

    /// End-to-end test for `Cell::Option` as a param: branching emit
    /// dispatch (option-some / option-none) plus the canonical-ABI
    /// `[disc, ...flat(T)]` slot ordering. `option<u32>` keeps the
    /// canon-lift options minimal (no realloc / memory required).
    #[test]
    fn dispatch_module_with_option_param_roundtrips() {
        // option<u32> flat = [i32 disc, i32 value].
        let wat = r#"(component
            (component $inner
                (core module $m
                    (func (export "consume") (param i32 i32))
                )
                (core instance $i (instantiate $m))
                (alias core export $i "consume" (core func $consume))
                (type $consume-ty (func (param "o" (option u32))))
                (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
                (instance $api-inst (export "consume" (func $consume-lifted)))
                (export "my:opt/api@1.0.0" (instance $api-inst))
            )
            (instance $api (instantiate $inner))
            (export "my:opt/api@1.0.0" (instance $api "my:opt/api@1.0.0"))
        )"#;
        let split_bytes = wat::parse_str(wat).expect("WAT must parse");

        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");

        let bytes = build_tier2_adapter(
            "my:opt/api@1.0.0",
            true,
            true,
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect("tier-2 adapter generation should succeed for option param");

        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("emitted tier-2 adapter component should validate");
    }

    /// Single-flat-slot compound result (`tuple<u32>`) — comes back
    /// flat, not via retptr, so `is_compound_result` falls through to
    /// no-lift. Build must succeed (after-hook sees `result:
    /// option::none`); the regression guard is the lack of a panic
    /// from `Compound → retptr scratch reserved`.
    #[test]
    fn dispatch_module_with_single_slot_tuple_result_falls_through() {
        let wat = r#"(component
            (component $inner
                (core module $m
                    (func (export "one-val") (param i32) (result i32)
                        local.get 0
                    )
                )
                (core instance $i (instantiate $m))
                (alias core export $i "one-val" (core func $one))
                (type $one-ty (func (param "x" u32) (result (tuple u32))))
                (func $one-lifted (type $one-ty) (canon lift (core func $one)))
                (instance $api-inst (export "one-val" (func $one-lifted)))
                (export "my:tup1/api@1.0.0" (instance $api-inst))
            )
            (instance $api (instantiate $inner))
            (export "my:tup1/api@1.0.0" (instance $api "my:tup1/api@1.0.0"))
        )"#;
        let split_bytes = wat::parse_str(wat).expect("WAT must parse");
        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");
        let bytes = build_tier2_adapter(
            "my:tup1/api@1.0.0",
            true,
            true,
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect("single-slot tuple result must fall through to no-lift, not panic");
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("emitted adapter component should validate");
    }

    /// End-to-end test for `tuple<...>` as a compound result —
    /// drives `is_compound_result(Tuple) → Compound → lift_from_memory`.
    /// Result flattens to 2 slots → retptr; canon lift's `memory` +
    /// `post-return` options materialize it via the callee-allocates
    /// pattern.
    #[test]
    fn dispatch_module_with_tuple_result_roundtrips() {
        let wat = r#"(component
            (component $inner
                (core module $m
                    (memory (export "memory") 1)
                    (func (export "two-vals") (param i32) (result i32)
                        i32.const 0x1000
                        local.get 0
                        i32.store
                        i32.const 0x1000
                        i32.const -1
                        i32.store offset=4
                        i32.const 0x1000
                    )
                    (func (export "cabi_post_two-vals") (param i32))
                )
                (core instance $i (instantiate $m))
                (alias core export $i "two-vals" (core func $two))
                (alias core export $i "cabi_post_two-vals" (core func $two_post))
                (alias core export $i "memory" (core memory $mem))
                (type $two-ty (func (param "x" u32) (result (tuple u32 s32))))
                (func $two-lifted (type $two-ty)
                    (canon lift (core func $two) (memory $mem)
                        (post-return (func $two_post))))
                (instance $api-inst (export "two-vals" (func $two-lifted)))
                (export "my:tup-ret/api@1.0.0" (instance $api-inst))
            )
            (instance $api (instantiate $inner))
            (export "my:tup-ret/api@1.0.0" (instance $api "my:tup-ret/api@1.0.0"))
        )"#;
        let split_bytes = wat::parse_str(wat).expect("WAT must parse");

        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");

        let bytes = build_tier2_adapter(
            "my:tup-ret/api@1.0.0",
            true,
            true,
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect("tier-2 adapter generation should succeed for tuple result");

        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("emitted tier-2 adapter component should validate");
    }

    /// End-to-end test for `option<T>` as a compound result. Drives
    /// `is_compound_result(Option) → Compound → lift_from_memory` and
    /// the if/else branching emit at the parent Option cell. Result
    /// flattens to 2 slots → retptr; canon lift's `memory` +
    /// `post-return` materialize it via the callee-allocates pattern.
    #[test]
    fn dispatch_module_with_option_result_roundtrips() {
        let wat = r#"(component
            (component $inner
                (core module $m
                    (memory (export "memory") 1)
                    (func (export "maybe-val") (param i32) (result i32)
                        i32.const 0x1000
                        i32.const 1
                        i32.store
                        i32.const 0x1000
                        local.get 0
                        i32.store offset=4
                        i32.const 0x1000
                    )
                    (func (export "cabi_post_maybe-val") (param i32))
                )
                (core instance $i (instantiate $m))
                (alias core export $i "maybe-val" (core func $maybe))
                (alias core export $i "cabi_post_maybe-val" (core func $maybe_post))
                (alias core export $i "memory" (core memory $mem))
                (type $maybe-ty (func (param "x" u32) (result (option u32))))
                (func $maybe-lifted (type $maybe-ty)
                    (canon lift (core func $maybe) (memory $mem)
                        (post-return (func $maybe_post))))
                (instance $api-inst (export "maybe-val" (func $maybe-lifted)))
                (export "my:opt-ret/api@1.0.0" (instance $api-inst))
            )
            (instance $api (instantiate $inner))
            (export "my:opt-ret/api@1.0.0" (instance $api "my:opt-ret/api@1.0.0"))
        )"#;
        let split_bytes = wat::parse_str(wat).expect("WAT must parse");

        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");

        let bytes = build_tier2_adapter(
            "my:opt-ret/api@1.0.0",
            true,
            true,
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect("tier-2 adapter generation should succeed for option result");

        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("emitted tier-2 adapter component should validate");
    }

    /// End-to-end test for `Cell::Flags` as a param. Nominal types
    /// (flags / enum / record) must be `(export … (type …))`'d from
    /// the api instance — otherwise wit-component's decode rejects
    /// the inner instance with `instance not valid to be used as
    /// export`. Anonymous types (option / result / tuple) sidestep
    /// the rule.
    #[test]
    fn dispatch_module_with_flags_param_roundtrips() {
        let wat = r#"(component
            (component $inner
                (core module $m
                    (func (export "consume") (param i32))
                )
                (core instance $i (instantiate $m))
                (alias core export $i "consume" (core func $consume))
                (type $perms (flags "read" "write" "exec"))
                (export $perms-export "fperms" (type $perms))
                (type $consume-ty (func (param "p" $perms-export)))
                (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
                (instance $api-inst
                    (export "fperms" (type $perms-export))
                    (export "consume" (func $consume-lifted)))
                (export "my:fl/api@1.0.0" (instance $api-inst))
            )
            (instance $api (instantiate $inner))
            (export "my:fl/api@1.0.0" (instance $api "my:fl/api@1.0.0"))
        )"#;
        let split_bytes = wat::parse_str(wat).expect("WAT must parse");

        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");

        let bytes = build_tier2_adapter(
            "my:fl/api@1.0.0",
            true,
            true,
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect("tier-2 adapter generation should succeed for flags param");

        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("emitted tier-2 adapter component should validate");
    }

    /// End-to-end test for `Cell::Result` as a param: branching emit
    /// (result-ok / result-err with option<u32> payload) and the
    /// canonical-ABI joined-flat slot sharing across both arms.
    /// `result<u32, u32>` keeps the canon-lift options minimal (no
    /// realloc / memory required) — both arms share the joined slot,
    /// no widening needed.
    #[test]
    fn dispatch_module_with_result_param_roundtrips() {
        // result<u32, u32> flat = [i32 disc, i32 (joined u32/u32)].
        let wat = r#"(component
            (component $inner
                (core module $m
                    (func (export "consume") (param i32 i32))
                )
                (core instance $i (instantiate $m))
                (alias core export $i "consume" (core func $consume))
                (type $consume-ty (func (param "r" (result u32 (error u32)))))
                (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
                (instance $api-inst (export "consume" (func $consume-lifted)))
                (export "my:res/api@1.0.0" (instance $api-inst))
            )
            (instance $api (instantiate $inner))
            (export "my:res/api@1.0.0" (instance $api "my:res/api@1.0.0"))
        )"#;
        let split_bytes = wat::parse_str(wat).expect("WAT must parse");

        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");

        let bytes = build_tier2_adapter(
            "my:res/api@1.0.0",
            true,
            true,
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect("tier-2 adapter generation should succeed for result param");

        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("emitted tier-2 adapter component should validate");
    }

    /// End-to-end test for `result<T, E>` as a compound result.
    /// Drives `is_compound_result(Result) → Compound → lift_from_memory`
    /// and the if/else branching emit at the parent Result cell.
    /// `result<u32, u32>` flattens to 2 slots → retptr; canon lift's
    /// `memory` + `post-return` materialize it via the
    /// callee-allocates pattern.
    #[test]
    fn dispatch_module_with_result_result_roundtrips() {
        let wat = r#"(component
            (component $inner
                (core module $m
                    (memory (export "memory") 1)
                    (func (export "either") (param i32) (result i32)
                        i32.const 0x1000
                        i32.const 0
                        i32.store
                        i32.const 0x1000
                        local.get 0
                        i32.store offset=4
                        i32.const 0x1000
                    )
                    (func (export "cabi_post_either") (param i32))
                )
                (core instance $i (instantiate $m))
                (alias core export $i "either" (core func $either))
                (alias core export $i "cabi_post_either" (core func $either_post))
                (alias core export $i "memory" (core memory $mem))
                (type $either-ty (func (param "x" u32) (result (result u32 (error u32)))))
                (func $either-lifted (type $either-ty)
                    (canon lift (core func $either) (memory $mem)
                        (post-return (func $either_post))))
                (instance $api-inst (export "either" (func $either-lifted)))
                (export "my:res-ret/api@1.0.0" (instance $api-inst))
            )
            (instance $api (instantiate $inner))
            (export "my:res-ret/api@1.0.0" (instance $api "my:res-ret/api@1.0.0"))
        )"#;
        let split_bytes = wat::parse_str(wat).expect("WAT must parse");

        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");

        let bytes = build_tier2_adapter(
            "my:res-ret/api@1.0.0",
            true,
            true,
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect("tier-2 adapter generation should succeed for result result");

        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("emitted tier-2 adapter component should validate");
    }

    /// Single-flat-slot compound result (`result<_, _>`) — flat is
    /// just the i32 disc, comes back direct (not retptr). Pins the
    /// `result_at_retptr` fall-through gate for the Result Compound
    /// branch — must build successfully, after-hook sees
    /// `result: option::none`.
    #[test]
    fn dispatch_module_with_single_slot_result_result_falls_through() {
        let wat = r#"(component
            (component $inner
                (core module $m
                    (func (export "noop") (result i32)
                        i32.const 0
                    )
                )
                (core instance $i (instantiate $m))
                (alias core export $i "noop" (core func $noop))
                (type $noop-ty (func (result (result))))
                (func $noop-lifted (type $noop-ty) (canon lift (core func $noop)))
                (instance $api-inst (export "noop" (func $noop-lifted)))
                (export "my:res1/api@1.0.0" (instance $api-inst))
            )
            (instance $api (instantiate $inner))
            (export "my:res1/api@1.0.0" (instance $api "my:res1/api@1.0.0"))
        )"#;
        let split_bytes = wat::parse_str(wat).expect("WAT must parse");
        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");
        let bytes = build_tier2_adapter(
            "my:res1/api@1.0.0",
            true,
            true,
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect("single-slot result must fall through to no-lift, not panic");
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("emitted adapter component should validate");
    }

    /// Async function whose params flatten to >`MAX_FLAT_ASYNC_PARAMS` (4)
    /// canon-lowers with `indirect_params=true`, but tier-2's
    /// `emit_handler_call` pushes flat params. Until `lower_to_memory`
    /// lands, assert we bail rather than emit invalid wasm. Parallel to
    /// tier-1's `test_adapter_async_indirect_params_bails`.
    #[test]
    fn async_indirect_params_bails() {
        let wat = r#"(component
            (type (;0;) (instance
                (type (;0;) (func async
                    (param "a" u32) (param "b" u32) (param "c" u32)
                    (param "d" u32) (param "e" u32) (result u32)))
                (export "many" (func (type 0)))
            ))
            (import "test:pkg/many@1.0.0" (instance (type 0)))
        )"#;
        let split_bytes = wat::parse_str(wat).expect("WAT must parse");

        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");

        let err = build_tier2_adapter(
            "test:pkg/many@1.0.0",
            true,
            true,
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect_err("async indirect-params should bail until lower_to_memory lands");
        let msg = err.to_string();
        assert!(
            msg.contains("not yet implemented") && msg.contains("MAX_FLAT_ASYNC_PARAMS"),
            "bail should mention the limit and not-yet-implemented, got: {msg}"
        );
    }
}
