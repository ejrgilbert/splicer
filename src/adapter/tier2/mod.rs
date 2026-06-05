//! Tier-2 adapter generator: lift canonical-ABI values from the
//! target function's params/result into the cell-array representation
//! in `splicer:common/types`, then dispatch to the middleware's
//! tier-2 hooks. Pipeline: classify → layout → emit. See
//! `docs/tiers/lift-codegen.md`.

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
    Function as WitFunction, InterfaceId, Mangling, Resolve, Type, TypeId, WasmExport,
    WasmExportKind, WasmImport, WorldKey,
};

use super::abi::emit::{
    collect_borrow_drops, emit_data_section, emit_export_section, emit_memory_and_globals,
    require_gate_compatible_func, require_indirect_params_supported_shape,
    require_no_inline_resources, synthesize_adapter_world_wit, BlobSlice,
};
use super::async_mirror::{synthesize_async_mirror, MIRROR_NAME_MISMATCH_PREFIX};
use super::resolve::{decode_input_resolve, dispatch_mangling, find_target_interface};
use blob::NameInterner;
use layout::lay_out_static_memory;
use lift::{
    classify_func_params, classify_result_lift, desugar_map_aliases, MapAliases, ParamLift,
    ResultLift,
};
use schema::compute_schema;
use section_emit::{
    emit_code_section, emit_imports_and_funcs, emit_type_section, wrapper_exports, HookImports,
};
use wrapper_body::{AfterHook, BeforeHook, GateHook, WrapperCtx};

const TIER2_ADAPTER_WORLD_PACKAGE: &str = "splicer:adapter-tier2";
const TIER2_ADAPTER_WORLD_NAME: &str = "adapter";

/// Generate a tier-2 adapter component.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_tier2_adapter(
    target_interface: &str,
    has_before: bool,
    has_after: bool,
    has_gate: bool,
    split_bytes: &[u8],
    common_wit: &str,
    tier2_wit: &str,
    mirror_export_name: Option<&str>,
) -> Result<Vec<u8>> {
    if !has_before && !has_after && !has_gate {
        bail!(
            "tier-2 adapter generation requires the middleware to export at least \
             one of `splicer:tier2/before`, `splicer:tier2/after`, or \
             `splicer:tier2/gate` — `trap`-only middleware is planned for a \
             follow-up slice."
        );
    }

    let mut resolve = decode_input_resolve(split_bytes)?;
    let target_iface = find_target_interface(&resolve, target_interface)?;

    // Bridged path: synthesize the async mirror and lift against it
    // (matches tier-1). Lower side stays on the real sync target.
    let (lift_iface, lift_interface_name) = match mirror_export_name {
        Some(expected) => {
            let (mirror_id, mirror_qname) = synthesize_async_mirror(&mut resolve, target_iface)
                .with_context(|| format!("synthesize async mirror for `{target_interface}`"))?;
            if mirror_qname != expected {
                bail!(
                    "{MIRROR_NAME_MISMATCH_PREFIX} for `{target_interface}`: \
                     wac wiring expected `{expected}` but tier-2 adapter \
                     synthesized `{mirror_qname}` — both sides should derive \
                     the same hash",
                );
            }
            (mirror_id, mirror_qname)
        }
        None => (target_iface, target_interface.to_string()),
    };
    let lower_iface = target_iface;
    let lower_interface_name = target_interface;

    require_supported_case(&resolve, lift_iface, lower_iface, has_gate)?;

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
                &lift_interface_name,
                lower_interface_name,
                &tier2_hook_imports(has_before, has_after, has_gate),
            ),
        )
        .context("parse synthesized tier-2 adapter world WIT")?;
    let world_id = resolve
        .select_world(&[world_pkg], Some(TIER2_ADAPTER_WORLD_NAME))
        .context("select tier-2 adapter world")?;

    // Map(K,V) lift desugars to list<tuple<K,V>>; allocate the
    // synthetic tuple typedefs once, before classify takes any
    // immutable borrows of `resolve`.
    let map_aliases = desugar_map_aliases(&mut resolve);

    // Iterate the lift side — those become wrapper exports. In the
    // bridged path the lift's funcs are async by construction.
    let lift_funcs: Vec<&WitFunction> = resolve.interfaces[lift_iface].functions.values().collect();
    let schema = compute_schema(&resolve, world_id, has_before, has_after, has_gate)?;

    let mut names = NameInterner::new();
    // Call-id reports the user-facing (lower / target) iface name so
    // middleware authors see the interface they wrote in YAML,
    // regardless of whether the adapter is mirror-lifted underneath.
    let iface_name = names.intern(lower_interface_name);
    let classified = build_per_func_classified(
        &resolve,
        lift_iface,
        lower_iface,
        &lift_funcs,
        &mut names,
        &map_aliases,
    )?;

    let (per_func, plan) =
        lay_out_static_memory(classified, &lift_funcs, &schema, names, iface_name)?;

    let mut core_module =
        build_dispatch_module(&resolve, &schema, &per_func, &lift_funcs, plan, iface_name)?;
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
fn require_supported_case(
    resolve: &Resolve,
    lift_iface: InterfaceId,
    lower_iface: InterfaceId,
    has_gate: bool,
) -> Result<()> {
    let lift = &resolve.interfaces[lift_iface];
    if lift.functions.is_empty() {
        bail!("interface has no functions");
    }
    require_no_inline_resources(resolve, lift_iface)?;
    if lower_iface != lift_iface {
        require_no_inline_resources(resolve, lower_iface)?;
    }
    let lower = &resolve.interfaces[lower_iface];
    for (name, lift_func) in &lift.functions {
        if has_gate {
            require_gate_compatible_func(resolve, name, lift_func, "2")?;
        }
        // Indirect-params cap is variant-dependent; check the lower
        // (import / canon-lower) side that drives the bindgen pass.
        let lower_func = lower
            .functions
            .get(name)
            .expect("async mirror synth guarantees fn-by-fn parity with lower iface");
        let import_is_async = lower_func.kind.is_async();
        let import_variant = if import_is_async {
            AbiVariant::GuestImportAsync
        } else {
            AbiVariant::GuestImport
        };
        let import_sig = resolve.wasm_signature(import_variant, lower_func);
        if import_sig.indirect_params {
            require_indirect_params_supported_shape(resolve, name, lower_func, import_is_async)?;
        }
    }
    Ok(())
}

/// Active tier-2 hook interfaces as fully-qualified versioned names.
fn tier2_hook_imports(has_before: bool, has_after: bool, has_gate: bool) -> Vec<String> {
    use crate::contract::{
        versioned_interface, TIER2_AFTER, TIER2_BEFORE, TIER2_GATE, TIER2_VERSION,
    };
    let mut out = Vec::new();
    if has_before {
        out.push(versioned_interface(TIER2_BEFORE, TIER2_VERSION));
    }
    if has_after {
        out.push(versioned_interface(TIER2_AFTER, TIER2_VERSION));
    }
    if has_gate {
        out.push(versioned_interface(TIER2_GATE, TIER2_VERSION));
    }
    out
}

/// Produce the dispatch core module bytes.
fn build_dispatch_module(
    resolve: &Resolve,
    schema: &schema::SchemaLayouts,
    per_func: &[FuncDispatch],
    funcs: &[&WitFunction],
    plan: layout::StaticDataPlan,
    iface_name: BlobSlice,
) -> Result<Vec<u8>> {
    let mut module = Module::new();
    let hooks = HookImports {
        before: schema.before_hook.as_ref().map(|h| &h.import),
        after: schema.after_hook.as_ref().map(|h| &h.import),
        gate: schema.gate_hook.as_ref().map(|h| &h.import),
    };
    let type_idx = emit_type_section(&mut module, per_func, &hooks);
    let func_idx = emit_imports_and_funcs(
        &mut module,
        resolve,
        per_func,
        &type_idx,
        hooks,
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
    // The args-shape buffer (`hook_params_ptr`) is shared between
    // before + gate; either wired side latches onto it. The
    // unreachable arms encode the "wired together or not at all"
    // contract per hook.
    let hook_params_ptr = plan.hook_params_ptr.map(|p| p as i32);
    let before_hook = match (
        schema.before_hook.as_ref(),
        func_idx.before_hook_idx,
        hook_params_ptr,
    ) {
        (Some(h), Some(idx), Some(params_ptr)) => Some(BeforeHook {
            idx,
            layout: &h.params_layout,
            params_ptr,
        }),
        (None, None, _) => None,
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
    let gate_hook = match (
        schema.gate_hook.as_ref(),
        func_idx.gate_hook_idx,
        hook_params_ptr,
        plan.gate_result_ptr,
    ) {
        (Some(h), Some(idx), Some(params_ptr), Some(result_ptr)) => Some(GateHook {
            idx,
            layout: &h.params_layout,
            params_ptr,
            result_ptr,
        }),
        (None, None, _, None) => None,
        _ => unreachable!(
            "gate-hook schema, import idx, params-ptr, and result-ptr wired in lockstep"
        ),
    };
    let wrapper_ctx = WrapperCtx {
        schema,
        resolve,
        iface_name,
        before_hook,
        after_hook,
        gate_hook,
        call_id_counter_global: globals.call_id_counter,
        bump_global: globals.bump,
    };
    emit_code_section(
        &mut module,
        per_func,
        funcs,
        &func_idx,
        &wrapper_ctx,
        &globals,
    )?;
    emit_data_section(&mut module, &plan.data_segments);
    Ok(module.finish())
}

// ─── Phase data shared across submodules ──────────────────────────
//
// Structs scoped to `pub(in crate::adapter::tier2)` so their `pub`
// fields can carry types only visible to that scope.

/// `task.return` import for one async target function.
pub(in crate::adapter::tier2) struct TaskReturnImport {
    pub module: String,
    pub name: String,
    pub sig: WasmSignature,
}

/// Per-side sync/async. Flags can disagree on the bridged path
/// (async lift, sync lower); they always agree otherwise.
pub(in crate::adapter::tier2) struct FuncShape {
    export_is_async: bool,
    import_is_async: bool,
    /// `Some` iff `export_is_async` (invariant enforced by
    /// constructors; field private to prevent struct-literal misuse).
    task_return: Option<TaskReturnImport>,
}

impl FuncShape {
    fn classify(
        resolve: &Resolve,
        lift_world_key: &WorldKey,
        lift_func: &WitFunction,
        lower_func: &WitFunction,
    ) -> Self {
        let export_is_async = lift_func.kind.is_async();
        let import_is_async = lower_func.kind.is_async();
        let task_return = export_is_async.then(|| {
            let (module, name, sig) =
                lift_func.task_return_import(resolve, Some(lift_world_key), Mangling::Legacy);
            TaskReturnImport { module, name, sig }
        });
        Self {
            export_is_async,
            import_is_async,
            task_return,
        }
    }

    /// All-sync stub for unit tests that don't want to build a full
    /// `Resolve` for `classify`.
    #[cfg(test)]
    pub(in crate::adapter::tier2) fn sync_stub() -> Self {
        Self {
            export_is_async: false,
            import_is_async: false,
            task_return: None,
        }
    }

    pub fn is_export_async(&self) -> bool {
        self.export_is_async
    }

    pub fn is_import_async(&self) -> bool {
        self.import_is_async
    }

    pub fn task_return(&self) -> Option<&TaskReturnImport> {
        debug_assert_eq!(
            self.export_is_async,
            self.task_return.is_some(),
            "FuncShape invariant: task_return.is_some() ⇔ export_is_async",
        );
        self.task_return.as_ref()
    }

    /// `(import_variant, export_variant)` — per-side. In the bridged
    /// path the two sides disagree.
    fn abi_variants(&self) -> (AbiVariant, AbiVariant) {
        let import = if self.import_is_async {
            AbiVariant::GuestImportAsync
        } else {
            AbiVariant::GuestImport
        };
        let export = if self.export_is_async {
            AbiVariant::GuestExportAsyncStackful
        } else {
            AbiVariant::GuestExport
        };
        (import, export)
    }

    /// `cabi_post_*` companion: only sync-lift retptr exports.
    /// Async lifts deliver via `task.return`.
    fn needs_cabi_post(&self, export_sig: &WasmSignature) -> bool {
        !self.export_is_async && export_sig.retptr
    }

    /// Result in `retptr_offset` memory (true) vs. a direct-return
    /// local (false) — driven by the import side.
    fn result_at_retptr(&self, import_sig: &WasmSignature) -> bool {
        import_sig.retptr
    }
}

/// Per-function on-return hook offsets, populated when the middleware
/// exports `splicer:tier2/after`. Result cells are `cabi_realloc`'d
/// per call by the wrapper body.
pub(in crate::adapter::tier2) struct AfterSetup {
    /// Byte offset of the pre-built on-return indirect-params buffer.
    pub params_offset: i32,
}

/// Classify-phase per-function output. No static-memory offsets — the
/// layout phase consumes a `Vec<FuncClassified>` and returns a parallel
/// `Vec<FuncDispatch>` with offsets filled in, so back-fill across
/// phase boundaries is structurally impossible.
pub(in crate::adapter::tier2) struct FuncClassified {
    pub shape: FuncShape,
    /// WIT result type — async wrappers drive `lift_from_memory` to
    /// flat-load the result for `task.return`.
    pub result_ty: Option<Type>,
    pub import_module: String,
    pub import_field: String,
    pub export_name: String,
    pub export_sig: WasmSignature,
    pub import_sig: WasmSignature,
    pub needs_cabi_post: bool,
    pub fn_name_offset: i32,
    pub fn_name_len: i32,
    pub params: Vec<ParamLift>,
    pub result_lift: Option<ResultLift>,
    /// Top-level `borrow<R>` params as `(flat_idx, resource_id)`. The
    /// wrapper must `[resource-drop]<R>` each one before returning —
    /// the canon-ABI runtime checks every borrow lifted on entry is
    /// dropped on exit.
    pub borrow_drops: Vec<(u32, TypeId)>,
}

/// Layout-phase per-function output: classify data + every static-
/// memory offset the emit phase needs. Read-only after construction.
pub(in crate::adapter::tier2) struct FuncDispatch {
    pub shape: FuncShape,
    pub result_ty: Option<Type>,
    pub import_module: String,
    pub import_field: String,
    pub export_name: String,
    pub export_sig: WasmSignature,
    /// Handler import sig. May differ from `export_sig` for compound-
    /// result functions (caller-allocates retptr on the import side
    /// vs. callee-returns pointer on the export side).
    pub import_sig: WasmSignature,
    pub needs_cabi_post: bool,
    pub fn_name_offset: i32,
    pub fn_name_len: i32,
    /// Per-param post-layout lift recipe. Each param's cells slab is
    /// `cabi_realloc`'d per call — no static slab base.
    pub params: Vec<lift::ParamLayout>,
    /// Byte offset of this function's pre-built `field` records in the
    /// data segment; pointed at by `args.list.ptr` passed to `on-call`.
    pub fields_buf_offset: u32,
    /// Retptr scratch; `Some` iff the import sig wants a
    /// caller-allocates retptr but the export sig returns the pointer
    /// directly.
    pub retptr_offset: Option<i32>,
    /// Indirect-params record buffer; `Some` iff the canonical-ABI
    /// flip is asymmetric (`import_sig.indirect_params &&
    /// !export_sig.indirect_params` — async-stackful 5..=16 flat).
    /// Symmetric flips pass `local 0` straight through. Inherits
    /// bump's single-active-call assumption.
    pub params_record_offset: Option<i32>,
    /// `None` for void or compound returns we don't yet lift.
    pub result_lift: Option<lift::ResultLayout>,
    pub after: Option<AfterSetup>,
    pub borrow_drops: Vec<(u32, TypeId)>,
}

/// Build per-target-function classify records. Interns fn names,
/// param names, and any record/field names referenced by lift plans.
/// `lift_funcs` is the iteration set — these become wrapper exports.
/// `lower_iface` provides the matching downstream handler funcs
/// looked up by name. In the `NotNeeded` path both ifaces are the
/// real target; in the bridged path the lift is the async mirror.
fn build_per_func_classified(
    resolve: &Resolve,
    lift_iface: InterfaceId,
    lower_iface: InterfaceId,
    lift_funcs: &[&WitFunction],
    names: &mut NameInterner,
    map_aliases: &MapAliases,
) -> Result<Vec<FuncClassified>> {
    let lift_world_key = WorldKey::Interface(lift_iface);
    let lower_world_key = WorldKey::Interface(lower_iface);
    let lower = &resolve.interfaces[lower_iface];
    let mut per_func: Vec<FuncClassified> = Vec::with_capacity(lift_funcs.len());

    for lift_func in lift_funcs {
        let fn_name_slice = names.intern(&lift_func.name);

        let lower_func = lower
            .functions
            .get(&lift_func.name)
            .expect("async mirror synth guarantees fn-by-fn parity with lower iface");

        // Lift and lower share params by construction; classify on lift.
        let params_lift = classify_func_params(resolve, lift_func, names, map_aliases)?;
        let shape = FuncShape::classify(resolve, &lift_world_key, lift_func, lower_func);
        let (import_variant, export_variant) = shape.abi_variants();

        // Per-side mangling — ABI namespaces diverge on bridged path.
        let (import_module, import_field) = resolve.wasm_import_name(
            dispatch_mangling(shape.is_import_async()),
            WasmImport::Func {
                interface: Some(&lower_world_key),
                func: lower_func,
            },
        );
        let export_name = resolve.wasm_export_name(
            dispatch_mangling(shape.is_export_async()),
            WasmExport::Func {
                interface: Some(&lift_world_key),
                func: lift_func,
                kind: WasmExportKind::Normal,
            },
        );
        let export_sig = resolve.wasm_signature(export_variant, lift_func);
        let import_sig = resolve.wasm_signature(import_variant, lower_func);
        let needs_cabi_post = shape.needs_cabi_post(&export_sig);
        let result_lift = classify_result_lift(
            resolve,
            lift_func,
            shape.result_at_retptr(&import_sig),
            names,
            map_aliases,
        )?;

        let borrow_drops = collect_borrow_drops(resolve, lift_func);

        per_func.push(FuncClassified {
            shape,
            result_ty: lift_func.result,
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
            borrow_drops,
        });
    }
    Ok(per_func)
}

#[cfg(test)]
mod tests;
