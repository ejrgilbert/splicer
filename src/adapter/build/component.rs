//! The orchestration that turns a list of [`AdapterFunc`]s into a
//! tier-1 adapter Component binary.
//!
//! [`build_adapter_bytes`] is the entry point. It walks through 13
//! linear phases (numbered in the source comments below), each
//! emitting one or more sections of the outer Component:
//!
//! 1–3. **Type / Import / Alias sections.** Copy the split's
//!     closure-filtered type/import/alias bytes verbatim, then either
//!     reuse the handler import (if the split imported it) or build a
//!     fresh handler instance type that references the preamble's
//!     aliased types via `alias outer`.
//! 3b. **`own<T>` types** for each aliased resource (component-level
//!     `own` definitions referencing the aliased resource indices).
//! 3c. **Component-level function types** for the canon-lift phase
//!     (declared after the `own<T>` types so they can reference them).
//!     Uses [`encode_comp_cv`] to materialize compound parameter and
//!     result types into the same `ComponentTypeSection`.
//! 4.  Build and embed the **memory module** (Core module 0).
//! 5.  Instantiate the memory module (Core instance 0).
//! 6.  Alias the memory (and `realloc`) out of the memory instance.
//! 7.  **`canon lower`** — lower the hook funcs, the handler funcs,
//!     and the async builtins into core funcs.
//! 8.  Build and embed the **dispatch module** (Core module 1).
//! 9.  Build the **`env` core instance** (synthesized from the
//!     lowered funcs above) and instantiate the dispatch module
//!     against it (Core instances 1 + 2).
//! 10. Alias the dispatch module's wrapper exports into core funcs.
//! 11. **`canon lift`** the wrapper core funcs back to component-level
//!     funcs of the right component type.
//! 12. Build the **export instance** that re-exports the wrapped
//!     funcs (and any handler-from-split type exports) under the
//!     target interface name.
//! 13. **Export** that instance from the outer Component.

use std::collections::HashMap;

use cviz::model::{InterfaceType, TypeArena, ValueType, ValueTypeId};
use wasm_encoder::{
    Alias, CanonicalFunctionSection, CanonicalOption, Component, ComponentAliasSection,
    ComponentExportKind, ComponentExportSection, ComponentImportSection, ComponentInstanceSection,
    ComponentOuterAliasKind, ComponentSectionId, ComponentTypeRef, ComponentTypeSection,
    ComponentValType, ExportKind, InstanceSection, InstanceType, ModuleSection, PrimitiveValType,
    RawSection,
};

use super::dispatch::{build_dispatch_module, build_mem_module};
use super::encoders::{encode_comp_cv, InstTypeCtx};
use super::mem_layout::MemoryLayoutBuilder;
use crate::adapter::abi::WitBridge;
use crate::adapter::filter::FilteredSections;
use crate::adapter::func::AdapterFunc;
use crate::adapter::indices::ComponentIndices;
use crate::adapter::names;

// ─── Section-emit helpers ──────────────────────────────────────────────────

/// Encode each function's params + result into `inst` (using `ctx` to manage
/// resource handling) and append a typed export for each. The caller may
/// pre-populate `inst` with `alias outer` declarations for resources before
/// calling this.
fn build_handler_inst_type(
    ctx: &mut InstTypeCtx,
    inst: &mut InstanceType,
    funcs: &[AdapterFunc],
    arena: &TypeArena,
) -> anyhow::Result<()> {
    let mut pp_cvs: Vec<Vec<ComponentValType>> = Vec::new();
    let mut pr_cv: Vec<Option<ComponentValType>> = Vec::new();
    for func in funcs.iter() {
        let mut p_cvs = Vec::new();
        for &id in &func.param_type_ids {
            p_cvs.push(ctx.encode_cv(id, inst, arena)?);
        }
        let r_cv = func
            .result_type_id
            .map(|id| ctx.encode_cv(id, inst, arena))
            .transpose()?;
        pp_cvs.push(p_cvs);
        pr_cv.push(r_cv);
    }
    for (fi, func) in funcs.iter().enumerate() {
        let params: Vec<(&str, ComponentValType)> = func
            .param_names
            .iter()
            .zip(pp_cvs[fi].iter())
            .map(|(n, &cv)| (n.as_str(), cv))
            .collect();
        let fn_ty_local_idx = inst.type_count();
        let mut fty = inst.ty().function();
        if func.is_async {
            fty.async_(true);
        }
        fty.params(params.iter().copied()).result(pr_cv[fi]);
        inst.export(&func.name, ComponentTypeRef::Func(fn_ty_local_idx));
    }
    Ok(())
}

/// Append the (optional) before/after/blocking instance types to `types` and
/// return the component-scope type index for each.  Bumps `*type_count` once
/// per emitted instance type.
fn emit_hook_inst_types(
    types: &mut ComponentTypeSection,
    indices: &mut ComponentIndices,
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
) -> (Option<u32>, Option<u32>, Option<u32>) {
    use crate::contract::{TIER1_AFTER_FNS, TIER1_BEFORE_FNS, TIER1_BLOCKING_FNS};

    // All three hooks share the shape `async func(name: string) -> result?`,
    // differing only in the export name and whether they return a bool.
    //
    // The SIGNATURE SHAPE here mirrors `wit/tier1/world.wit` — if the
    // tier-1 hook signatures ever change (e.g. a new param is added),
    // update the `.params([...])` / `.result(...)` call below. The
    // EXPORT NAMES come from the WIT file via `TIER1_*_FNS`, so they
    // stay in sync automatically.
    let string_cv = ComponentValType::Primitive(PrimitiveValType::String);
    let bool_cv = ComponentValType::Primitive(PrimitiveValType::Bool);
    let mut emit_hook_ty = |export_name: &str, result: Option<ComponentValType>| {
        let idx = indices.alloc_ty();
        let mut inst = InstanceType::new();
        inst.ty()
            .function()
            .async_(true)
            .params([("name", string_cv)])
            .result(result);
        inst.export(export_name, ComponentTypeRef::Func(0));
        types.instance(&inst);
        idx
    };
    let before_inst_ty = has_before.then(|| emit_hook_ty(TIER1_BEFORE_FNS[0], None));
    let after_inst_ty = has_after.then(|| emit_hook_ty(TIER1_AFTER_FNS[0], None));
    let blocking_inst_ty = has_blocking.then(|| emit_hook_ty(TIER1_BLOCKING_FNS[0], Some(bool_cv)));
    (before_inst_ty, after_inst_ty, blocking_inst_ty)
}

/// Append (optional) imports for the tier-1 hook instances
/// (e.g. `splicer:tier1/before@0.1.0`) and return the
/// component-scope instance index for each.
///
/// The import names are versioned (`iface@version`) so they match the
/// middleware component's versioned exports exactly — `wac compose`
/// requires an exact name match when wiring instances.
fn emit_hook_imports(
    imports: &mut ComponentImportSection,
    indices: &mut ComponentIndices,
    before_ty: Option<u32>,
    after_ty: Option<u32>,
    blocking_ty: Option<u32>,
) -> (Option<u32>, Option<u32>, Option<u32>) {
    use crate::contract::{
        versioned_interface, TIER1_AFTER, TIER1_BEFORE, TIER1_BLOCKING, TIER1_VERSION,
    };

    let mut import_hook = |ty_idx: u32, iface: &str| {
        let idx = indices.alloc_inst();
        imports.import(
            &versioned_interface(iface, TIER1_VERSION),
            ComponentTypeRef::Instance(ty_idx),
        );
        idx
    };
    let before_inst = before_ty.map(|ty| import_hook(ty, TIER1_BEFORE));
    let after_inst = after_ty.map(|ty| import_hook(ty, TIER1_AFTER));
    let blocking_inst = blocking_ty.map(|ty| import_hook(ty, TIER1_BLOCKING));
    (before_inst, after_inst, blocking_inst)
}

/// Alias the handler funcs (from `handler_inst`) and the optional
/// before/after/blocking funcs (each from its own instance) into the
/// component scope.  Returns `(handler_func_base, before, after, blocking)`.
fn emit_func_aliases(
    aliases: &mut ComponentAliasSection,
    indices: &mut ComponentIndices,
    funcs: &[AdapterFunc],
    handler_inst: u32,
    before_inst: Option<u32>,
    after_inst: Option<u32>,
    blocking_inst: Option<u32>,
) -> (u32, Option<u32>, Option<u32>, Option<u32>) {
    let handler_func_base = indices.func;
    let mut alias_func = |inst_idx: u32, name: &str| {
        let idx = indices.alloc_func();
        aliases.alias(Alias::InstanceExport {
            instance: inst_idx,
            kind: ComponentExportKind::Func,
            name,
        });
        idx
    };

    for func in funcs {
        alias_func(handler_inst, &func.name);
    }
    // Hook fn names come from `wit/tier1/world.wit` via `build.rs` —
    // see [`crate::contract::TIER1_BEFORE_FNS`] etc.
    use crate::contract::{TIER1_AFTER_FNS, TIER1_BEFORE_FNS, TIER1_BLOCKING_FNS};
    let before_comp_func = before_inst.map(|i| alias_func(i, TIER1_BEFORE_FNS[0]));
    let after_comp_func = after_inst.map(|i| alias_func(i, TIER1_AFTER_FNS[0]));
    let blocking_comp_func = blocking_inst.map(|i| alias_func(i, TIER1_BLOCKING_FNS[0]));
    (
        handler_func_base,
        before_comp_func,
        after_comp_func,
        blocking_comp_func,
    )
}

/// Emit the adapter's type / import / alias sections on top of a
/// (consumer or provider) filtered split's raw sections.
///
/// Copies the split's preamble verbatim, seeds the index allocator,
/// and dispatches to the strategy that matches how the split carries
/// the target interface:
///
/// - **Consumer split**: the split already imports `target_interface`.
///   [`emit_imports_consumer_split`] reuses that handler import,
///   emits the hook types/imports/aliases on top, and aliases the
///   handler's resource + compound type exports for later phases.
/// - **Provider split**: the split *exports* `target_interface`. The
///   preamble carries the supporting type imports (types instance,
///   aliased resources). [`emit_imports_provider_split`] builds a
///   fresh handler import type that references those preamble
///   aliases via `alias outer`.
#[allow(clippy::too_many_arguments)]
fn emit_imports_from_split(
    component: &mut Component,
    target_interface: &str,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    arena: &TypeArena,
    iface_ty: &InterfaceType,
    split: &FilteredSections,
    indices: &mut ComponentIndices,
) -> anyhow::Result<ImportsOutcome> {
    // Copy raw type/import/alias sections from the split. Both
    // strategies below assume these are already present.
    for (section_kind, data) in &split.raw_sections {
        // RawSection::id is u8 (wasm-encoder's API), so convert at the boundary.
        component.section(&RawSection {
            id: *section_kind as u8,
            data,
        });
    }
    // The filtered split already consumed `split.type_count` type slots
    // and `split.instance_count` instance slots — seed the allocator
    // so our subsequent emits land on the right indices.
    indices.ty = split.type_count;
    indices.inst = split.instance_count;

    if let Some(handler_idx) = split
        .import_names
        .iter()
        .position(|n| n == target_interface)
    {
        emit_imports_consumer_split(
            component,
            funcs,
            has_before,
            has_after,
            has_blocking,
            arena,
            iface_ty,
            split,
            indices,
            handler_idx as u32,
        )
    } else {
        emit_imports_provider_split(
            component,
            target_interface,
            funcs,
            has_before,
            has_after,
            has_blocking,
            arena,
            iface_ty,
            split,
            indices,
        )
    }
}

/// Consumer-split strategy: the raw sections already include an import
/// for `target_interface`. Reuse its instance index, emit the hook
/// types/imports/aliases, and alias the handler's type exports
/// (resources + compound types) into component scope for the
/// canon-lift / export phases to reference.
#[allow(clippy::too_many_arguments)]
fn emit_imports_consumer_split(
    component: &mut Component,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    arena: &TypeArena,
    iface_ty: &InterfaceType,
    split: &FilteredSections,
    indices: &mut ComponentIndices,
    handler_inst: u32,
) -> anyhow::Result<ImportsOutcome> {
    // Hook types + imports.
    let mut types = ComponentTypeSection::new();
    let (before_inst_ty, after_inst_ty, blocking_inst_ty) =
        emit_hook_inst_types(&mut types, indices, has_before, has_after, has_blocking);
    component.section(&types);

    let mut imports = ComponentImportSection::new();
    let (before_inst, after_inst, blocking_inst) = emit_hook_imports(
        &mut imports,
        indices,
        before_inst_ty,
        after_inst_ty,
        blocking_inst_ty,
    );
    component.section(&imports);

    // Alias funcs from the handler instance + hooks.
    let mut aliases = ComponentAliasSection::new();
    let (handler_func_base, before_comp_func, after_comp_func, blocking_comp_func) =
        emit_func_aliases(
            &mut aliases,
            indices,
            funcs,
            handler_inst,
            before_inst,
            after_inst,
            blocking_inst,
        );
    component.section(&aliases);

    // Build inst_ctx to discover resource exports (needed for sections
    // 3b/3c), but DON'T emit the instance type — it came from the raw
    // sections.
    let mut inst_ctx = InstTypeCtx::new();
    {
        let mut dummy_inst = InstanceType::new();
        build_handler_inst_type(&mut inst_ctx, &mut dummy_inst, funcs, arena)?;
    }

    // Alias ALL type exports from the handler instance — both resources
    // (request, response) and compound types (error-code). This ensures
    // the adapter's exported function type references the same types as
    // the handler import.
    let mut comp_aliased_types: HashMap<ValueTypeId, u32> = HashMap::new();
    let mut comp_resource_indices: Vec<u32> = Vec::new();
    let mut export_aliases = ComponentAliasSection::new();

    // Alias resources.
    for (_vid, export_name, _res_local, _own_local) in &inst_ctx.resource_exports {
        let comp_idx = indices.alloc_ty();
        export_aliases.alias(Alias::InstanceExport {
            instance: handler_inst,
            kind: ComponentExportKind::Type,
            name: export_name,
        });
        comp_resource_indices.push(comp_idx);
    }

    // Alias compound type exports (e.g. error-code).
    if let InterfaceType::Instance(inst) = iface_ty {
        for (export_name, &vid) in &inst.type_exports {
            if !matches!(
                arena.lookup_val(vid),
                ValueType::Resource(_) | ValueType::AsyncHandle
            ) {
                let comp_idx = indices.alloc_ty();
                export_aliases.alias(Alias::InstanceExport {
                    instance: handler_inst,
                    kind: ComponentExportKind::Type,
                    name: export_name,
                });
                comp_aliased_types.insert(vid, comp_idx);
            }
        }
    }

    if indices.ty > split.type_count {
        component.section(&export_aliases);
    }

    Ok(ImportsOutcome {
        handler_func_base,
        before_comp_func,
        after_comp_func,
        blocking_comp_func,
        inst_ctx,
        comp_resource_indices,
        comp_aliased_types,
    })
}

/// Provider-split strategy: the handler isn't imported by the split
/// (it's exported). Build a fresh handler import type whose body
/// references the preamble's aliased types via `alias outer`, then
/// emit the import alongside the hook imports.
///
/// Resources and compound types are wired differently from the
/// consumer case: they come from the preamble (at component scope),
/// not from SubResource exports on the handler instance, so we reuse
/// those preamble indices for `comp_resource_indices` and
/// `comp_aliased_types` instead of aliasing them off the handler
/// instance (which would fail — the preamble-derived resource types
/// aren't SubResource exports on the handler instance type).
#[allow(clippy::too_many_arguments)]
fn emit_imports_provider_split(
    component: &mut Component,
    target_interface: &str,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    arena: &TypeArena,
    iface_ty: &InterfaceType,
    split: &FilteredSections,
    indices: &mut ComponentIndices,
) -> anyhow::Result<ImportsOutcome> {
    // Build map of every interface type export that has a matching
    // aliased type in the preamble. Both resources (request/response)
    // and compound types (error-code, DNS-error-payload, …) are
    // pre-aliased at component scope; the handler instance type body
    // must reference them all via `alias outer` so it's type-identical
    // to what the provider's handler actually exports — otherwise the
    // component model validator sees the import type as a fresh
    // redefinition incompatible with the underlying resource / variant
    // identity.
    let mut outer_aliased: HashMap<ValueTypeId, u32> = HashMap::new();
    if let InterfaceType::Instance(inst_iface) = iface_ty {
        for (name, &vid) in &inst_iface.type_exports {
            if let Some(&comp_idx) = split.aliased_type_exports.get(name) {
                outer_aliased.insert(vid, comp_idx);
            }
        }
    }

    let handler_inst_ty: u32;
    let inst_ctx: InstTypeCtx;
    let mut types = ComponentTypeSection::new();
    {
        // `outer_resources` in the ctx is narrowly used for the
        // own<>/SubResource emission path — only keep resource-kind
        // entries there. Compound types that need alias-outer flow
        // through `alias_locals` below; the encoder picks them up
        // before falling into inline encoding.
        let outer_res_map: HashMap<ValueTypeId, u32> = outer_aliased
            .iter()
            .filter(|(&vid, _)| {
                matches!(
                    arena.lookup_val(vid),
                    ValueType::Resource(_) | ValueType::AsyncHandle
                )
            })
            .map(|(k, v)| (*k, *v))
            .collect();
        let mut ctx = if outer_res_map.is_empty() {
            InstTypeCtx::new()
        } else {
            InstTypeCtx::with_outer_resources(outer_res_map)
        };
        let mut inst = InstanceType::new();

        // Emit `alias outer 1 <comp_idx>` for every aliased type
        // (resource + compound) and record its instance-local index
        // so the encoder can reference it instead of re-encoding.
        for (&vid, &comp_idx) in &outer_aliased {
            let local_idx = inst.type_count();
            inst.alias(Alias::Outer {
                kind: ComponentOuterAliasKind::Type,
                count: 1,
                index: comp_idx,
            });
            ctx.alias_locals.insert(vid, local_idx);
        }

        build_handler_inst_type(&mut ctx, &mut inst, funcs, arena)?;
        handler_inst_ty = indices.alloc_ty();
        inst_ctx = ctx;
        types.instance(&inst);
    }

    let (before_inst_ty, after_inst_ty, blocking_inst_ty) =
        emit_hook_inst_types(&mut types, indices, has_before, has_after, has_blocking);
    component.section(&types);

    // Import handler + hooks.
    let mut imports = ComponentImportSection::new();
    let handler_inst = indices.alloc_inst();
    imports.import(
        target_interface,
        ComponentTypeRef::Instance(handler_inst_ty),
    );
    let (before_inst, after_inst, blocking_inst) = emit_hook_imports(
        &mut imports,
        indices,
        before_inst_ty,
        after_inst_ty,
        blocking_inst_ty,
    );
    component.section(&imports);

    // Alias funcs + resource types.
    let mut aliases = ComponentAliasSection::new();
    let (handler_func_base, before_comp_func, after_comp_func, blocking_comp_func) =
        emit_func_aliases(
            &mut aliases,
            indices,
            funcs,
            handler_inst,
            before_inst,
            after_inst,
            blocking_inst,
        );

    // In the provider-split case the handler's resource types came
    // from the preamble's aliased types (we emitted `alias outer` for
    // them inside the handler instance type). They therefore DON'T
    // appear as SubResource exports on the handler instance, and
    // aliasing them via InstanceExport would fail. Reuse the
    // preamble's component-scope indices directly.
    let mut comp_resource_indices: Vec<u32> = Vec::new();
    for (vid, export_name, _res_local, _own_local) in &inst_ctx.resource_exports {
        if let Some(&comp_idx) = outer_aliased.get(vid) {
            comp_resource_indices.push(comp_idx);
        } else {
            let comp_res_idx = indices.alloc_ty();
            aliases.alias(Alias::InstanceExport {
                instance: handler_inst,
                kind: ComponentExportKind::Type,
                name: export_name,
            });
            comp_resource_indices.push(comp_res_idx);
        }
    }

    // Compound types (error-code, DNS-error-payload, …) came from
    // the preamble too; they're not exports on the handler instance,
    // so reuse those indices for the `comp_aliased_types` map
    // consumed by section 3c.
    let mut comp_aliased_types: HashMap<ValueTypeId, u32> = HashMap::new();
    if let InterfaceType::Instance(inst_iface) = iface_ty {
        for &vid in inst_iface.type_exports.values() {
            if !matches!(
                arena.lookup_val(vid),
                ValueType::Resource(_) | ValueType::AsyncHandle
            ) {
                if let Some(&comp_idx) = outer_aliased.get(&vid) {
                    comp_aliased_types.insert(vid, comp_idx);
                }
            }
        }
    }
    component.section(&aliases);

    Ok(ImportsOutcome {
        handler_func_base,
        before_comp_func,
        after_comp_func,
        blocking_comp_func,
        inst_ctx,
        comp_resource_indices,
        comp_aliased_types,
    })
}

/// State that survives the import-emission phase and is consumed by the
/// post-strategy phases (sections 3b, 3c, canon-lower, canon-lift, export).
///
/// Each `emit_imports_*` strategy returns one of these so the rest of
/// `build_adapter_bytes` can read the indices and counter values it needs
/// without caring which strategy produced them.
struct ImportsOutcome {
    /// Component-scope func index of the first aliased handler func.
    /// Subsequent handler funcs live at `handler_func_base + i`.
    handler_func_base: u32,
    /// Component-scope func indices of the optional middleware hooks.
    before_comp_func: Option<u32>,
    after_comp_func: Option<u32>,
    blocking_comp_func: Option<u32>,
    /// Resource-bookkeeping context populated while encoding the handler
    /// instance type. Used by section 3b to emit `own<T>` types and by
    /// section 12 to re-export type exports for the consumer-split case.
    inst_ctx: InstTypeCtx,
    /// Component-scope type indices of aliased resources, parallel to
    /// `inst_ctx.resource_exports`. Used by section 3b to point each
    /// `own<T>` definition at the right resource type index.
    comp_resource_indices: Vec<u32>,
    /// Maps `ValueTypeId` → component-scope type index for ALL aliased
    /// types (resources + compounds). Section 3c pre-populates the
    /// `encode_comp_cv` cache with this so it reuses aliased indices
    /// instead of emitting fresh inline definitions.
    comp_aliased_types: HashMap<ValueTypeId, u32>,
}

/// Pure data describing the dispatch module's linear-memory layout
/// and the strategy flags that drive the canon-lower / canon-lift /
/// dispatch-module phases.
struct MemoryLayout {
    /// Byte offset reserved for `waitable-set.wait` event output.
    event_ptr: u32,
    /// Byte offset of the bool result slot for `should-block-call`,
    /// or `None` when the blocking hook is not in use.
    block_result_ptr: Option<u32>,
    /// True when any handler function or its compound result needs the
    /// canonical-ABI `realloc` import (string params, complex strings
    /// in results, or any resource handle).
    needs_realloc: bool,
    /// First free byte in linear memory after all static data — the
    /// initial value for the dispatch module's bump pointer.
    bump_start: u32,
    /// True when async machinery is needed at all (an async handler OR
    /// any async-lowered hook). Drives whether the dispatch module
    /// imports the waitable/subtask builtins.
    has_async_machinery: bool,
}

/// Finish the dispatch module's static memory layout: append the
/// fixed post-func slots (event record, optional block result) and
/// compute the bump-allocator start. Also derives the
/// flag-dependent switches (`needs_realloc`, `has_async_machinery`)
/// the canon-lower / dispatch phases consult.
///
/// `layout` is the builder handed over from
/// [`super::func::extract_adapter_funcs`], with the per-func name
/// and result-buffer slots already allocated. This function consumes
/// it.
fn compute_memory_layout(
    funcs: &[AdapterFunc],
    mut layout: MemoryLayoutBuilder,
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
) -> MemoryLayout {
    let has_async = funcs.iter().any(|f| f.is_async);
    let has_async_machinery = has_async || has_before || has_after || has_blocking;

    let event_ptr = layout.alloc_event_slot();
    let block_result_ptr = has_blocking.then(|| layout.alloc_block_result());

    // Realloc is needed by canon lift and canon lower to allocate
    // memory for any value whose canonical-ABI form is a
    // pointer-to-bytes: strings (variable-length UTF-8) and lists.
    // Both dynamic `list<T>` and fixed-size `list<T, N>` are covered
    // by [`super::wit_bridge::WitBridge::has_lists`]. Bare resource
    // handles don't need realloc — they're i32 values on the wire.
    // When needed, realloc lives in the memory module so it's
    // available for both lowering and lifting.
    let needs_realloc = funcs.iter().any(|f| f.canon_needs_realloc());

    let bump_start = layout.finish_as_bump_start();

    MemoryLayout {
        event_ptr,
        block_result_ptr,
        needs_realloc,
        bump_start,
        has_async_machinery,
    }
}

/// State produced by [`emit_memory_provider`] for use by the canon-lower
/// and dispatch phases. The post-section phases reference the memory and
/// realloc by their *core-scope* indices, not the wrapping component
/// instance.
struct MemoryProviderOutcome {
    /// Component-scope core-memory index of the aliased `mem` export.
    /// Threaded into every canon lower/lift `Memory(_)` option.
    mem_core_mem: u32,
    /// Component-scope core-func index of the aliased `realloc` export,
    /// or `None` when `needs_realloc` is false.
    mem_core_realloc: Option<u32>,
}

/// Sections 4 + 5 + 6. Build the memory provider core module, instantiate
/// it as Core instance 0, and alias its `mem` (and optional `realloc`)
/// exports up into component scope so the canon-lower/lift phases can
/// reference them via `CanonicalOption::Memory` / `Realloc`.
fn emit_memory_provider(
    component: &mut Component,
    indices: &mut ComponentIndices,
    needs_realloc: bool,
    bump_start: u32,
) -> MemoryProviderOutcome {
    // Core memory is its own index space — the adapter only has one memory
    // (from this module), so it lives at index 0.
    let core_memory_count: u32 = 0;

    // ── 4. Core module 0: memory provider (+ optional realloc) ─────────
    {
        let mem_module = build_mem_module(needs_realloc, bump_start);
        component.section(&ModuleSection(&mem_module));
    }

    // ── 5. Core instance 0: instantiate mem module ─────────────────────
    let mem_core_inst: u32;
    {
        let mut instances = InstanceSection::new();
        mem_core_inst = indices.alloc_core_inst();
        instances.instantiate::<[(&str, wasm_encoder::ModuleArg); 0], &str>(0u32, []);
        component.section(&instances);
    }

    // ── 6. Alias core memory (and realloc) from mem instance ──────────
    let mem_core_mem: u32;
    let mem_core_realloc: Option<u32>;
    {
        let mut aliases = ComponentAliasSection::new();
        mem_core_mem = core_memory_count;
        aliases.alias(Alias::CoreInstanceExport {
            instance: mem_core_inst,
            kind: ExportKind::Memory,
            name: names::ENV_MEMORY,
        });
        mem_core_realloc = if needs_realloc {
            let idx = indices.alloc_core_func();
            aliases.alias(Alias::CoreInstanceExport {
                instance: mem_core_inst,
                kind: ExportKind::Func,
                name: names::ENV_REALLOC,
            });
            Some(idx)
        } else {
            None
        };
        component.section(&aliases);
    }

    MemoryProviderOutcome {
        mem_core_mem,
        mem_core_realloc,
    }
}

/// State produced by [`emit_canon_lower`]. Each `Option<u32>` is a
/// component-scope core-func index produced by `canon lower` (or
/// `canon waitable.*` / `canon task.return`); the dispatch-instance
/// (env) phase wires these into the dispatch module's named imports.
struct CanonLowerOutcome {
    core_before_func: Option<u32>,
    core_after_func: Option<u32>,
    core_blocking_func: Option<u32>,
    /// First core-func index of the lowered handler funcs. Per-handler
    /// indices live at `core_handler_func_base + i`.
    core_handler_func_base: u32,
    /// Async canonical built-in indices, populated when
    /// `has_async_machinery` is true (otherwise all `None`).
    core_waitable_new: Option<u32>,
    core_waitable_join: Option<u32>,
    core_waitable_wait: Option<u32>,
    core_waitable_drop: Option<u32>,
    core_subtask_drop: Option<u32>,
    /// Per-async-function `task.return` core-func indices, parallel to
    /// `funcs`. `None` for sync funcs.
    core_task_return_funcs: Vec<Option<u32>>,
}

/// Section 7. Lower the hook funcs (before/after/blocking), the handler
/// funcs, and the async canonical built-ins, into core funcs that the
/// dispatch core module imports.
#[allow(clippy::too_many_arguments)]
fn emit_canon_lower(
    component: &mut Component,
    indices: &mut ComponentIndices,
    funcs: &[AdapterFunc],
    handler_func_base: u32,
    before_comp_func: Option<u32>,
    after_comp_func: Option<u32>,
    blocking_comp_func: Option<u32>,
    mem_core_mem: u32,
    mem_core_realloc: Option<u32>,
    has_async_machinery: bool,
    comp_result_cvs: &[Option<ComponentValType>],
) -> CanonLowerOutcome {
    let core_waitable_new: Option<u32>;
    let core_waitable_join: Option<u32>;
    let core_waitable_wait: Option<u32>;
    let core_waitable_drop: Option<u32>;
    let core_subtask_drop: Option<u32>;

    let mut canons = CanonicalFunctionSection::new();

    // Hooks are async-lowered — every tier-1 hook takes a `string`
    // name param (requiring `Memory` + `UTF8`) and fires async, so
    // the three canonicals are shape-identical.
    let mut lower_hook = |comp_f: u32| {
        let idx = indices.alloc_core_func();
        canons.lower(
            comp_f,
            [
                CanonicalOption::Async,
                CanonicalOption::Memory(mem_core_mem),
                CanonicalOption::UTF8,
            ],
        );
        idx
    };
    let core_before_func = before_comp_func.map(&mut lower_hook);
    let core_after_func = after_comp_func.map(&mut lower_hook);
    let core_blocking_func = blocking_comp_func.map(&mut lower_hook);

    // Lower each handler function. The canon options are driven by
    // the `canon_needs_*` helpers on `AdapterFunc` so the memory /
    // realloc / utf8 decisions stay in one place; see those methods
    // for what each flag covers.
    let core_handler_func_base = indices.core_func;
    for (i, func) in funcs.iter().enumerate() {
        indices.core_func += 1;
        let mut opts: Vec<CanonicalOption> = Vec::new();
        if func.is_async {
            opts.push(CanonicalOption::Async);
        }
        if func.canon_needs_memory() {
            opts.push(CanonicalOption::Memory(mem_core_mem));
        }
        if func.canon_needs_utf8() {
            opts.push(CanonicalOption::UTF8);
        }
        if func.canon_needs_realloc() {
            if let Some(ra) = mem_core_realloc {
                opts.push(CanonicalOption::Realloc(ra));
            }
        }
        canons.lower(handler_func_base + i as u32, opts);
    }

    // Async canonical built-ins — emitted whenever async machinery is
    // needed.
    if has_async_machinery {
        core_waitable_new = Some(indices.alloc_core_func());
        canons.waitable_set_new();

        core_waitable_join = Some(indices.alloc_core_func());
        canons.waitable_join();

        core_waitable_wait = Some(indices.alloc_core_func());
        canons.waitable_set_wait(false, mem_core_mem);

        core_waitable_drop = Some(indices.alloc_core_func());
        canons.waitable_set_drop();

        core_subtask_drop = Some(indices.alloc_core_func());
        canons.subtask_drop();
    } else {
        core_waitable_new = None;
        core_waitable_join = None;
        core_waitable_wait = None;
        core_waitable_drop = None;
        core_subtask_drop = None;
    }

    // task.return canonicals — one per async func (void or not).
    // `task.return` lifts the flat return values into a
    // component-level result; it needs Memory + UTF8 whenever the
    // result type pulls bytes through linear memory. Driven by the
    // same `canon_needs_*` helpers as canon-lower.
    let core_task_return_funcs: Vec<Option<u32>> = funcs
        .iter()
        .enumerate()
        .map(|(fi, func)| {
            if func.is_async {
                let idx = indices.alloc_core_func();
                let tr_result_cv = comp_result_cvs[fi];
                let mut opts: Vec<CanonicalOption> = Vec::new();
                if func.canon_needs_memory() {
                    opts.push(CanonicalOption::Memory(mem_core_mem));
                }
                if func.canon_needs_utf8() {
                    opts.push(CanonicalOption::UTF8);
                }
                canons.task_return(tr_result_cv, opts);
                Some(idx)
            } else {
                None
            }
        })
        .collect();

    component.section(&canons);

    CanonLowerOutcome {
        core_before_func,
        core_after_func,
        core_blocking_func,
        core_handler_func_base,
        core_waitable_new,
        core_waitable_join,
        core_waitable_wait,
        core_waitable_drop,
        core_subtask_drop,
        core_task_return_funcs,
    }
}

/// State produced by [`emit_dispatch_phase`] for use by the canon-lift
/// phase. The dispatch core instance's index is what canon-lift aliases
/// the per-handler wrapper exports out of.
struct DispatchPhaseOutcome {
    /// Core-instance index of the dispatch core module's instance
    /// (Core instance 2). The canon-lift phase aliases each handler's
    /// wrapper export out of this instance.
    dispatch_core_inst: u32,
}

/// Collect the `env` core instance's exports.
///
/// The env instance bridges the outer component's canon-lowered
/// funcs (produced by [`emit_canon_lower`]) to the inner dispatch
/// module's imports (declared in [`build_dispatch_module`]). Every
/// name here must match a matching `imports.import("env", ...)` call
/// on the dispatch side — see [`super::names`] and `TIER1_*_ENV_SLOTS`
/// in [`crate::contract`] for the single source of truth for each
/// string.
///
/// Each `Option<u32>` on [`CanonLowerOutcome`] is a "did we produce
/// this canon-lowered func?" switch; when `None`, the corresponding
/// slot is omitted from the env (and the dispatch module also didn't
/// import it). This must-match-on-both-sides contract is why the
/// construction is extracted: it's the one place where the full list
/// of env slots is visible alongside the conditions that gate each.
fn build_env_exports(
    canon_lower: &CanonLowerOutcome,
    funcs: &[AdapterFunc],
    mem_core_mem: u32,
) -> Vec<(String, ExportKind, u32)> {
    use crate::contract::{
        TIER1_AFTER_ENV_SLOTS, TIER1_BEFORE_ENV_SLOTS, TIER1_BLOCKING_ENV_SLOTS,
    };

    let mut env_exports: Vec<(String, ExportKind, u32)> = Vec::new();
    env_exports.push((
        names::ENV_MEMORY.to_string(),
        ExportKind::Memory,
        mem_core_mem,
    ));

    // Middleware hooks (optional per-middleware).
    if let Some(idx) = canon_lower.core_before_func {
        env_exports.push((TIER1_BEFORE_ENV_SLOTS[0].to_string(), ExportKind::Func, idx));
    }
    if let Some(idx) = canon_lower.core_after_func {
        env_exports.push((TIER1_AFTER_ENV_SLOTS[0].to_string(), ExportKind::Func, idx));
    }
    if let Some(idx) = canon_lower.core_blocking_func {
        env_exports.push((
            TIER1_BLOCKING_ENV_SLOTS[0].to_string(),
            ExportKind::Func,
            idx,
        ));
    }

    // Target interface's handler funcs (one per func, always present).
    for (i, _) in funcs.iter().enumerate() {
        env_exports.push((
            names::env_handler_fn(i),
            ExportKind::Func,
            canon_lower.core_handler_func_base + i as u32,
        ));
    }

    // Async builtins (waitable/subtask), emitted only when the dispatch
    // module has any async handler calls or async hook invocations to
    // await. Same set of Option<u32>s drives what the dispatch module
    // imported, so the OR-none shape is identical on both sides.
    if let Some(idx) = canon_lower.core_waitable_new {
        env_exports.push((names::ENV_WAITABLE_NEW.to_string(), ExportKind::Func, idx));
    }
    if let Some(idx) = canon_lower.core_waitable_join {
        env_exports.push((names::ENV_WAITABLE_JOIN.to_string(), ExportKind::Func, idx));
    }
    if let Some(idx) = canon_lower.core_waitable_wait {
        env_exports.push((names::ENV_WAITABLE_WAIT.to_string(), ExportKind::Func, idx));
    }
    if let Some(idx) = canon_lower.core_waitable_drop {
        env_exports.push((names::ENV_WAITABLE_DROP.to_string(), ExportKind::Func, idx));
    }
    if let Some(idx) = canon_lower.core_subtask_drop {
        env_exports.push((names::ENV_SUBTASK_DROP.to_string(), ExportKind::Func, idx));
    }

    // task.return funcs — one per async func (None entries are sync
    // funcs, which don't need task.return).
    for (i, tr_idx) in canon_lower.core_task_return_funcs.iter().enumerate() {
        if let Some(idx) = tr_idx {
            env_exports.push((names::env_task_return_fn(i), ExportKind::Func, *idx));
        }
    }

    env_exports
}

/// Sections 8 + 9. Embed the dispatch core module bytes (built by
/// [`build_dispatch_module`]) and instantiate it against a synthesized
/// `env` core instance whose exports are the canon-lowered hook funcs,
/// handler funcs, and async builtins from section 7.
#[allow(clippy::too_many_arguments)]
fn emit_dispatch_phase(
    component: &mut Component,
    indices: &mut ComponentIndices,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    layout: &MemoryLayout,
    mem_core_mem: u32,
    canon_lower: &CanonLowerOutcome,
    bridge: &WitBridge,
) -> anyhow::Result<DispatchPhaseOutcome> {
    // ── 8. Core module 1: dispatch ─────────────────────────────────────
    {
        let dispatch_bytes = build_dispatch_module(
            funcs,
            has_before,
            has_after,
            has_blocking,
            layout.event_ptr,
            layout.block_result_ptr,
            bridge,
        )?;
        // Use RawSection to embed the pre-built module bytes directly.
        component.section(&RawSection {
            id: ComponentSectionId::CoreModule as u8,
            data: &dispatch_bytes,
        });
    }

    // ── 9. Core instances 1 + 2: env + dispatch ────────────────────────
    let dispatch_core_inst: u32;
    {
        let mut instances = InstanceSection::new();

        // Core instance 1: env (export items that dispatch imports).
        let env_inst = indices.alloc_core_inst();
        let env_exports = build_env_exports(canon_lower, funcs, mem_core_mem);
        instances.export_items(
            env_exports
                .iter()
                .map(|(n, k, i)| (n.as_str(), *k, *i))
                .collect::<Vec<_>>(),
        );

        // Core instance 2: dispatch (instantiate with env).
        dispatch_core_inst = indices.alloc_core_inst();
        instances.instantiate(
            1u32,
            [(
                names::ENV_INSTANCE,
                wasm_encoder::ModuleArg::Instance(env_inst),
            )],
        );

        component.section(&instances);
    }

    Ok(DispatchPhaseOutcome { dispatch_core_inst })
}

/// State produced by [`emit_canon_lift_phase`] for use by the export
/// phase. The wrapped funcs become component-scope funcs that the
/// export instance re-exports under the target interface name.
struct CanonLiftOutcome {
    /// First component-scope func index of the lifted wrapper funcs.
    /// Per-handler indices live at `wrapped_func_base + i`.
    wrapped_func_base: u32,
}

/// Sections 10 + 11. Alias the dispatch core instance's wrapper exports
/// up into core-func space (one per handler), then `canon lift` each
/// wrapper into a component-scope function of the matching target
/// function type from section 3c.
///
/// Note: this phase does NOT advance `indices.core_func` or
/// `indices.func` past the section it emits — the original
/// implementation didn't either, and nothing downstream reads the
/// counters after this point. Preserving that keeps the byte output
/// identical.
#[allow(clippy::too_many_arguments)]
fn emit_canon_lift_phase(
    component: &mut Component,
    indices: &ComponentIndices,
    funcs: &[AdapterFunc],
    dispatch_core_inst: u32,
    target_func_ty_base: u32,
    mem_core_mem: u32,
    mem_core_realloc: Option<u32>,
    needs_realloc: bool,
) -> CanonLiftOutcome {
    // ── 10. Alias core wrapper functions from dispatch instance ────────
    let core_wrapper_func_base: u32;
    {
        let mut aliases = ComponentAliasSection::new();
        core_wrapper_func_base = indices.core_func;
        for func in funcs {
            aliases.alias(Alias::CoreInstanceExport {
                instance: dispatch_core_inst,
                kind: ExportKind::Func,
                name: &func.name,
            });
        }
        component.section(&aliases);
    }

    // ── 11. Canon lift: wrapper core funcs → component funcs ──────────
    // Options driven by the `canon_needs_*` helpers on `AdapterFunc`;
    // see those methods for what each flag covers.
    let wrapped_func_base: u32;
    {
        let mut canons = CanonicalFunctionSection::new();
        wrapped_func_base = indices.func;
        for (i, func) in funcs.iter().enumerate() {
            let mut opts: Vec<CanonicalOption> = if func.is_async {
                vec![CanonicalOption::Async]
            } else {
                vec![]
            };
            if func.canon_needs_memory() {
                opts.push(CanonicalOption::Memory(mem_core_mem));
            }
            if func.canon_needs_utf8() {
                opts.push(CanonicalOption::UTF8);
            }
            if needs_realloc && func.canon_needs_realloc() {
                if let Some(ra) = mem_core_realloc {
                    opts.push(CanonicalOption::Realloc(ra));
                }
            }
            canons.lift(
                core_wrapper_func_base + i as u32,
                target_func_ty_base + i as u32,
                opts,
            );
        }
        component.section(&canons);
    }

    CanonLiftOutcome { wrapped_func_base }
}

/// Sections 12 + 13. Build the export instance that re-exports the
/// adapter-wrapped handler funcs (and any handler-from-consumer-split
/// type exports), then emit a top-level component export pointing the
/// `target_interface` import name at that instance.
///
/// This is the final phase — no outcome state escapes.
#[allow(clippy::too_many_arguments)]
fn emit_export_phase(
    component: &mut Component,
    indices: &ComponentIndices,
    target_interface: &str,
    funcs: &[AdapterFunc],
    iface_ty: &InterfaceType,
    split: &FilteredSections,
    inst_ctx: &InstTypeCtx,
    comp_resource_indices: &[u32],
    comp_aliased_types: &HashMap<ValueTypeId, u32>,
    wrapped_func_base: u32,
) {
    // ── 12. Component instance: export instance for target interface ──
    let export_inst: u32;
    {
        let mut comp_instances = ComponentInstanceSection::new();
        export_inst = indices.inst;

        let mut export_items: Vec<(&str, ComponentExportKind, u32)> = Vec::new();

        // When the handler import came from raw consumer-split sections,
        // re-export its type exports so the adapter's handler export
        // matches what consumers expect.
        let handler_from_split = split.import_names.iter().any(|n| n == target_interface);
        if handler_from_split {
            for (i, (_vid, export_name, _res_local, _own_local)) in
                inst_ctx.resource_exports.iter().enumerate()
            {
                export_items.push((
                    export_name,
                    ComponentExportKind::Type,
                    comp_resource_indices[i],
                ));
            }
            if let InterfaceType::Instance(inst) = iface_ty {
                for (export_name, &vid) in &inst.type_exports {
                    if let Some(&comp_idx) = comp_aliased_types.get(&vid) {
                        export_items.push((export_name, ComponentExportKind::Type, comp_idx));
                    }
                }
            }
        }

        // Export adapter-wrapped functions.
        for (i, func) in funcs.iter().enumerate() {
            export_items.push((
                func.name.as_str(),
                ComponentExportKind::Func,
                wrapped_func_base + i as u32,
            ));
        }

        comp_instances.export_items(export_items);
        component.section(&comp_instances);
    }

    // ── 13. Component export section ──────────────────────────────────
    {
        let mut exports = ComponentExportSection::new();
        exports.export(
            target_interface,
            ComponentExportKind::Instance,
            export_inst,
            None,
        );
        component.section(&exports);
    }
}

/// State produced by [`emit_handler_resource_types`] for use by the
/// canon-lower and canon-lift phases.
struct HandlerTypesOutcome {
    /// Component-level type index of the first canon-lift function type.
    /// Per-function types live at `target_func_ty_base + i`.
    target_func_ty_base: u32,
    /// Per-function component-level result `ComponentValType`. Used by
    /// the canon-lower phase to declare per-function `task.return` types.
    comp_result_cvs: Vec<Option<ComponentValType>>,
}

struct FuncSig {
    params: Vec<(String, ComponentValType)>,
    result: Option<ComponentValType>,
}
impl FuncSig {
    fn new(params: Vec<(String, ComponentValType)>, result: Option<ComponentValType>) -> FuncSig {
        FuncSig { params, result }
    }
}

/// Sections 3b + 3c. Emit the `own<T>` types for each aliased resource
/// (3b) and the per-function component-level function types used by the
/// canon-lift phase (3c). The two sections are tightly coupled because
/// the function types must reference the `own<T>` indices.
fn emit_handler_resource_types(
    component: &mut Component,
    funcs: &[AdapterFunc],
    arena: &TypeArena,
    inst_ctx: &InstTypeCtx,
    comp_resource_indices: &[u32],
    comp_aliased_types: &HashMap<ValueTypeId, u32>,
    indices: &mut ComponentIndices,
) -> anyhow::Result<HandlerTypesOutcome> {
    // ── 3b. Type section B: own<T> types for each aliased resource ─────────
    //
    // Maps ValueTypeId → component-level own<T> type index. Named
    // resources have distinct ValueTypeIds (since TypeArena interns by
    // value), so this correctly maps each distinct resource to its own<T>
    // index.
    let comp_own_by_vid: HashMap<ValueTypeId, u32>;
    {
        let mut own_types = ComponentTypeSection::new();
        let mut own_map: HashMap<ValueTypeId, u32> = HashMap::new();
        for (i, (vid, _export_name, _res_local, _own_local)) in
            inst_ctx.resource_exports.iter().enumerate()
        {
            let comp_res_idx = comp_resource_indices[i];
            let own_idx = indices.alloc_ty();
            own_types.defined_type().own(comp_res_idx);
            own_map.insert(*vid, own_idx);
        }
        comp_own_by_vid = own_map;
        if !inst_ctx.resource_exports.is_empty() {
            component.section(&own_types);
        }
    }

    // ── 3c. Type section C: function types for canon lift ──────────────────
    //
    // Declared here (after aliasing) so they can reference `own<T>`
    // types. Uses `encode_comp_cv` to build compound types (result,
    // variant, etc.) at the component level, referencing `own<T>` types
    // from section 3b.
    let target_func_ty_base: u32;
    let comp_result_cvs: Vec<Option<ComponentValType>>;
    {
        let mut func_types = ComponentTypeSection::new();

        // Pre-populate the cache with aliased compound type indices so
        // `encode_comp_cv` reuses them instead of building fresh
        // definitions (e.g. the aliased error-code type).
        let mut comp_cv_cache: HashMap<ValueTypeId, u32> = HashMap::new();
        for (&vid, &comp_idx) in comp_aliased_types {
            if !matches!(
                arena.lookup_val(vid),
                ValueType::Resource(_) | ValueType::AsyncHandle
            ) {
                comp_cv_cache.insert(vid, comp_idx);
            }
        }

        // First pass: pre-encode all compound types for params and
        // results. This must happen BEFORE setting target_func_ty_base
        // so that compound type definitions are emitted into func_types
        // before the function type declarations.
        let mut pre_encoded: Vec<FuncSig> = Vec::new();
        for func in funcs.iter() {
            let mut params: Vec<(String, ComponentValType)> = Vec::new();
            for (n, &id) in func.param_names.iter().zip(func.param_type_ids.iter()) {
                let cv = encode_comp_cv(
                    id,
                    arena,
                    &mut func_types,
                    &mut indices.ty,
                    &comp_own_by_vid,
                    &mut comp_cv_cache,
                )?;
                params.push((n.clone(), cv));
            }
            let result_cv = func
                .result_type_id
                .map(|id| {
                    encode_comp_cv(
                        id,
                        arena,
                        &mut func_types,
                        &mut indices.ty,
                        &comp_own_by_vid,
                        &mut comp_cv_cache,
                    )
                })
                .transpose()?;
            pre_encoded.push(FuncSig::new(params, result_cv));
        }

        // Second pass: declare function types. target_func_ty_base is
        // set HERE, after all compound types have been added to
        // func_types.
        target_func_ty_base = indices.ty;
        let mut result_cvs: Vec<Option<ComponentValType>> = Vec::new();
        for (func, FuncSig { params, result }) in funcs.iter().zip(pre_encoded.into_iter()) {
            indices.ty += 1;
            result_cvs.push(result);
            let mut fty = func_types.function();
            if func.is_async {
                fty.async_(true);
            }
            fty.params(params.iter().map(|(n, cv)| (n.as_str(), *cv)))
                .result(result);
        }

        comp_result_cvs = result_cvs;
        component.section(&func_types);
    }

    let _ = comp_own_by_vid;
    Ok(HandlerTypesOutcome {
        target_func_ty_base,
        comp_result_cvs,
    })
}

/// Build the full Wasm component binary for the tier-1 adapter.
///
/// Uses `wasm_encoder::Component` with explicit section management so that
/// we can create a component instance from exported items (a capability not
/// exposed as a public method by `ComponentBuilder`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_adapter_bytes(
    target_interface: &str,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    arena: &TypeArena,
    iface_ty: &InterfaceType,
    split: &FilteredSections,
    layout: MemoryLayoutBuilder,
    bridge: &WitBridge,
) -> anyhow::Result<Vec<u8>> {
    let mut component = Component::new();

    // Shared index allocator — threaded by `&mut` through every
    // phase below. Phases no longer return counter values; they
    // mutate `indices` in place.
    let mut indices = ComponentIndices::default();

    // ── 1–3. Type / Import / Alias sections ─────────────────────────────────
    //
    // Copy the consumer (or provider) split's type/import/alias sections
    // verbatim, then either reuse the handler import already present
    // there or build one fresh on top of the preamble's aliased types.
    let ImportsOutcome {
        handler_func_base,
        before_comp_func,
        after_comp_func,
        blocking_comp_func,
        inst_ctx,
        comp_resource_indices,
        comp_aliased_types,
    } = emit_imports_from_split(
        &mut component,
        target_interface,
        funcs,
        has_before,
        has_after,
        has_blocking,
        arena,
        iface_ty,
        split,
        &mut indices,
    )?;

    let HandlerTypesOutcome {
        target_func_ty_base,
        comp_result_cvs,
    } = emit_handler_resource_types(
        &mut component,
        funcs,
        arena,
        &inst_ctx,
        &comp_resource_indices,
        &comp_aliased_types,
        &mut indices,
    )?;

    let layout = compute_memory_layout(funcs, layout, has_before, has_after, has_blocking);

    let MemoryProviderOutcome {
        mem_core_mem,
        mem_core_realloc,
    } = emit_memory_provider(
        &mut component,
        &mut indices,
        layout.needs_realloc,
        layout.bump_start,
    );

    let canon_lower = emit_canon_lower(
        &mut component,
        &mut indices,
        funcs,
        handler_func_base,
        before_comp_func,
        after_comp_func,
        blocking_comp_func,
        mem_core_mem,
        mem_core_realloc,
        layout.has_async_machinery,
        &comp_result_cvs,
    );

    let DispatchPhaseOutcome { dispatch_core_inst } = emit_dispatch_phase(
        &mut component,
        &mut indices,
        funcs,
        has_before,
        has_after,
        has_blocking,
        &layout,
        mem_core_mem,
        &canon_lower,
        bridge,
    )?;

    let CanonLiftOutcome { wrapped_func_base } = emit_canon_lift_phase(
        &mut component,
        &indices,
        funcs,
        dispatch_core_inst,
        target_func_ty_base,
        mem_core_mem,
        mem_core_realloc,
        layout.needs_realloc,
    );

    emit_export_phase(
        &mut component,
        &indices,
        target_interface,
        funcs,
        iface_ty,
        split,
        &inst_ctx,
        &comp_resource_indices,
        &comp_aliased_types,
        wrapped_func_base,
    );

    Ok(component.finish())
}
