//! The orchestration that turns a list of [`AdapterFunc`]s into a
//! tier-1 adapter Component binary.
//!
//! [`build_adapter_bytes`] is the entry point. It walks through 13
//! linear phases (numbered in the source comments below), each
//! emitting one or more sections of the outer Component:
//!
//! 1–3. **Type / Import / Alias sections.** This is the only phase
//!    that branches: it picks one of three strategies depending on
//!    what context it has about the target interface:
//!    - **From consumer split** — `split_imports` was supplied, so
//!      copy the consumer split's raw type/import/alias bytes
//!      verbatim and either reuse the handler import (if the split
//!      imports it) or build a fresh handler instance type
//!      referencing the shared types from the raw sections.
//!    - **Via separate types interface** — the target interface has
//!      resource types and named type exports; emit a separate
//!      types-instance import and an `alias outer` handler instance
//!      type that refers back to it.
//!    - **Inline resources** — no resources; emit one self-contained
//!      handler instance type with `SubResource` exports.
//!
//! 3b. **`own<T>` types** for each aliased resource (component-level
//!     `own` definitions referencing the aliased resource indices).
//! 3c. **Component-level function types** for the canon-lift phase
//!     (declared after the `own<T>` types so they can reference them).
//!     Uses [`encode_comp_cv`] to materialise compound parameter and
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
//!
//! Several small `fn` helpers are nested inside `build_adapter_bytes`
//! for now (`emit_hook_func_types`, `build_handler_inst_type`,
//! `emit_hook_inst_types`, `emit_hook_imports`, `emit_func_aliases`).
//! They are short and only used by the path branches; pulling them
//! to module scope would mean threading more parameters and isn't
//! worth the code-motion churn until the path branches themselves
//! are extracted.

use std::collections::{BTreeMap, HashMap};

use cviz::model::{InterfaceType, TypeArena, ValueType, ValueTypeId};
use wasm_encoder::{
    Alias, CanonicalFunctionSection, CanonicalOption, Component, ComponentAliasSection,
    ComponentExportKind, ComponentExportSection, ComponentImportSection, ComponentInstanceSection,
    ComponentOuterAliasKind, ComponentSectionId, ComponentTypeRef, ComponentTypeSection,
    ComponentValType, ExportKind, InstanceSection, InstanceType, ModuleSection, PrimitiveValType,
    RawSection,
};

use super::dispatch::{build_dispatch_module, build_mem_module};
use super::encoders::{build_types_instance_type, encode_comp_cv, InstTypeCtx};
use super::func::AdapterFunc;
use super::split_imports::SplitImports;
use super::ty::type_has_resources;

/// Derive the "types" interface name from a target interface name.
/// e.g. "wasi:http/handler@0.3.0-rc-2026-01-06" → "wasi:http/types@0.3.0-rc-2026-01-06"
fn derive_types_interface(target: &str) -> Option<String> {
    if let Some(at_pos) = target.find('@') {
        let (path, version) = target.split_at(at_pos);
        if let Some(slash_pos) = path.rfind('/') {
            return Some(format!("{}/types{}", &path[..slash_pos], version));
        }
    }
    if let Some(slash_pos) = target.rfind('/') {
        return Some(format!("{}/types", &target[..slash_pos]));
    }
    None
}

// ─── Section-emit helpers shared by the three path branches ────────────────
//
// Each helper appends a section's worth of items and either updates an
// `&mut counter` for indices it produced or returns the indices directly.
// They're all small enough to inline but extracting them here keeps each
// path branch focused on the wiring it controls rather than repeating the
// hook/types/import/alias scaffolding.

/// Append the two type-erased hook function types to `types`. The caller
/// is responsible for incrementing its own type counter by 2 — they always
/// land at the next two type indices.
fn emit_hook_func_types(types: &mut ComponentTypeSection) {
    // type N: async func(name: string) -> ()
    types
        .function()
        .async_(true)
        .params([(
            "name",
            ComponentValType::Primitive(PrimitiveValType::String),
        )])
        .result(None);
    // type N+1: async func(name: string) -> bool
    types
        .function()
        .async_(true)
        .params([(
            "name",
            ComponentValType::Primitive(PrimitiveValType::String),
        )])
        .result(Some(ComponentValType::Primitive(PrimitiveValType::Bool)));
}

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
    type_count: &mut u32,
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
) -> (Option<u32>, Option<u32>, Option<u32>) {
    let before_inst_ty = if has_before {
        let idx = *type_count;
        *type_count += 1;
        let mut inst = InstanceType::new();
        inst.ty()
            .function()
            .async_(true)
            .params([(
                "name",
                ComponentValType::Primitive(PrimitiveValType::String),
            )])
            .result(None);
        inst.export("before-call", ComponentTypeRef::Func(0));
        types.instance(&inst);
        Some(idx)
    } else {
        None
    };
    let after_inst_ty = if has_after {
        let idx = *type_count;
        *type_count += 1;
        let mut inst = InstanceType::new();
        inst.ty()
            .function()
            .async_(true)
            .params([(
                "name",
                ComponentValType::Primitive(PrimitiveValType::String),
            )])
            .result(None);
        inst.export("after-call", ComponentTypeRef::Func(0));
        types.instance(&inst);
        Some(idx)
    } else {
        None
    };
    let blocking_inst_ty = if has_blocking {
        let idx = *type_count;
        *type_count += 1;
        let mut inst = InstanceType::new();
        inst.ty()
            .function()
            .async_(true)
            .params([(
                "name",
                ComponentValType::Primitive(PrimitiveValType::String),
            )])
            .result(Some(ComponentValType::Primitive(PrimitiveValType::Bool)));
        inst.export("should-block-call", ComponentTypeRef::Func(0));
        types.instance(&inst);
        Some(idx)
    } else {
        None
    };
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
    instance_count: &mut u32,
    before_ty: Option<u32>,
    after_ty: Option<u32>,
    blocking_ty: Option<u32>,
) -> (Option<u32>, Option<u32>, Option<u32>) {
    use crate::contract::{
        versioned_interface, TIER1_AFTER, TIER1_BEFORE, TIER1_BLOCKING, TIER1_VERSION,
    };

    let before_inst = before_ty.map(|ty_idx| {
        let idx = *instance_count;
        *instance_count += 1;
        imports.import(
            &versioned_interface(TIER1_BEFORE, TIER1_VERSION),
            ComponentTypeRef::Instance(ty_idx),
        );
        idx
    });
    let after_inst = after_ty.map(|ty_idx| {
        let idx = *instance_count;
        *instance_count += 1;
        imports.import(
            &versioned_interface(TIER1_AFTER, TIER1_VERSION),
            ComponentTypeRef::Instance(ty_idx),
        );
        idx
    });
    let blocking_inst = blocking_ty.map(|ty_idx| {
        let idx = *instance_count;
        *instance_count += 1;
        imports.import(
            &versioned_interface(TIER1_BLOCKING, TIER1_VERSION),
            ComponentTypeRef::Instance(ty_idx),
        );
        idx
    });
    (before_inst, after_inst, blocking_inst)
}

/// Alias the handler funcs (from `handler_inst`) and the optional
/// before/after/blocking funcs (each from its own instance) into the
/// component scope.  Returns `(handler_func_base, before, after, blocking)`.
fn emit_func_aliases(
    aliases: &mut ComponentAliasSection,
    func_count: &mut u32,
    funcs: &[AdapterFunc],
    handler_inst: u32,
    before_inst: Option<u32>,
    after_inst: Option<u32>,
    blocking_inst: Option<u32>,
) -> (u32, Option<u32>, Option<u32>, Option<u32>) {
    let handler_func_base = *func_count;
    for func in funcs {
        aliases.alias(Alias::InstanceExport {
            instance: handler_inst,
            kind: ComponentExportKind::Func,
            name: &func.name,
        });
        *func_count += 1;
    }
    let before_comp_func = before_inst.map(|inst_idx| {
        let idx = *func_count;
        *func_count += 1;
        aliases.alias(Alias::InstanceExport {
            instance: inst_idx,
            kind: ComponentExportKind::Func,
            name: "before-call",
        });
        idx
    });
    let after_comp_func = after_inst.map(|inst_idx| {
        let idx = *func_count;
        *func_count += 1;
        aliases.alias(Alias::InstanceExport {
            instance: inst_idx,
            kind: ComponentExportKind::Func,
            name: "after-call",
        });
        idx
    });
    let blocking_comp_func = blocking_inst.map(|inst_idx| {
        let idx = *func_count;
        *func_count += 1;
        aliases.alias(Alias::InstanceExport {
            instance: inst_idx,
            kind: ComponentExportKind::Func,
            name: "should-block-call",
        });
        idx
    });
    (
        handler_func_base,
        before_comp_func,
        after_comp_func,
        blocking_comp_func,
    )
}

/// Strategy: pass-through consumer split imports.
///
/// Copies the consumer split's type/import/alias sections verbatim, then
/// either reuses the handler import the split already declares (if the split
/// imports the target interface) or builds a fresh handler import on top of
/// the shared types from the raw sections (if the consumer split itself
/// re-exports the target interface). After the handler is in place, hook
/// instance types/imports and func aliases are added on top.
#[allow(clippy::too_many_arguments)]
fn emit_imports_from_consumer_split(
    component: &mut Component,
    target_interface: &str,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    arena: &TypeArena,
    iface_ty: &InterfaceType,
    split: &SplitImports,
) -> anyhow::Result<ImportsOutcome> {
    // Strategy-internal counters: we start by claiming the index space the
    // raw sections we just copied already consumed.
    let mut type_count: u32;
    let mut instance_count: u32;
    let mut func_count: u32 = 0;

    // Strategy-internal handles to the handler instance import. They never
    // escape because the post-strategy phases only need the handler
    // *function* aliases (in handler_func_base), not the handler instance
    // index itself.
    let handler_inst: u32;
    let inst_ctx: InstTypeCtx;
    let handler_func_base: u32;
    let before_comp_func: Option<u32>;
    let after_comp_func: Option<u32>;
    let blocking_comp_func: Option<u32>;
    let comp_resource_indices: Vec<u32>;
    let mut comp_aliased_types: HashMap<ValueTypeId, u32> = HashMap::new();

    // Copy raw type/import/alias sections from the split.
    for (section_kind, data) in &split.raw_sections {
        // RawSection::id is u8 (wasm-encoder's API), so convert at the boundary.
        component.section(&RawSection {
            id: *section_kind as u8,
            data,
        });
    }
    type_count = split.type_count;
    instance_count = split.instance_count;

    // Check if the handler is already imported by the split.
    let handler_in_split = split.import_names.iter().any(|n| n == target_interface);

    if handler_in_split {
        // Handler import came from the raw sections — find its instance index.
        let handler_idx = split
            .import_names
            .iter()
            .position(|n| n == target_interface)
            .unwrap() as u32;
        handler_inst = handler_idx;

        // Hook types + imports.
        let (before_inst_ty, after_inst_ty, blocking_inst_ty);
        {
            let mut types = ComponentTypeSection::new();
            (before_inst_ty, after_inst_ty, blocking_inst_ty) = emit_hook_inst_types(
                &mut types,
                &mut type_count,
                has_before,
                has_after,
                has_blocking,
            );
            component.section(&types);
        }
        let (before_inst, after_inst, blocking_inst);
        {
            let mut imports = ComponentImportSection::new();
            (before_inst, after_inst, blocking_inst) = emit_hook_imports(
                &mut imports,
                &mut instance_count,
                before_inst_ty,
                after_inst_ty,
                blocking_inst_ty,
            );
            component.section(&imports);
        }

        // Alias funcs from the handler instance + hooks.
        {
            let mut aliases = ComponentAliasSection::new();
            (
                handler_func_base,
                before_comp_func,
                after_comp_func,
                blocking_comp_func,
            ) = emit_func_aliases(
                &mut aliases,
                &mut func_count,
                funcs,
                handler_inst,
                before_inst,
                after_inst,
                blocking_inst,
            );
            component.section(&aliases);
        }

        // Build inst_ctx to discover resource exports (needed for sections
        // 3b/3c), but DON'T emit the instance type — it came from the raw
        // sections.
        {
            let mut ctx = InstTypeCtx::new();
            let mut dummy_inst = InstanceType::new();
            build_handler_inst_type(&mut ctx, &mut dummy_inst, funcs, arena)?;
            inst_ctx = ctx;
        }

        // Alias ALL type exports from the handler instance — both resources
        // (request, response) and compound types (error-code). This ensures
        // the adapter's exported function type references the same types as
        // the handler import.
        {
            let mut aliases = ComponentAliasSection::new();
            let mut res_vec: Vec<u32> = Vec::new();

            // Alias resources.
            for (_vid, export_name, _res_local, _own_local) in &inst_ctx.resource_exports {
                let comp_idx = type_count;
                type_count += 1;
                aliases.alias(Alias::InstanceExport {
                    instance: handler_inst,
                    kind: ComponentExportKind::Type,
                    name: export_name,
                });
                res_vec.push(comp_idx);
            }
            comp_resource_indices = res_vec;

            // Alias compound type exports (e.g. error-code).
            if let InterfaceType::Instance(inst) = iface_ty {
                for (export_name, &vid) in &inst.type_exports {
                    if !matches!(
                        arena.lookup_val(vid),
                        ValueType::Resource(_) | ValueType::AsyncHandle
                    ) {
                        let comp_idx = type_count;
                        type_count += 1;
                        aliases.alias(Alias::InstanceExport {
                            instance: handler_inst,
                            kind: ComponentExportKind::Type,
                            name: export_name,
                        });
                        comp_aliased_types.insert(vid, comp_idx);
                    }
                }
            }

            if type_count > split.type_count {
                component.section(&aliases);
            }
        }
    } else {
        // Provider split: the handler is exported, not imported. The
        // preamble sections provide the supporting imports (types-instance,
        // WASI interfaces) with aliased resource types. Build only the
        // handler import type fresh, referencing those aliased resources
        // via alias-outer.
        let handler_inst_ty: u32;
        let mut types = ComponentTypeSection::new();

        // Build map of every interface type export that has a matching
        // aliased type in the preamble. Both resources (request/response)
        // and compound types (error-code, DNS-error-payload, …) are
        // pre-aliased at component scope; the handler instance type body
        // must reference them all via `alias outer` so it's
        // type-identical to what the provider's handler actually exports
        // — otherwise the component model validator sees the import type
        // as a fresh redefinition incompatible with the underlying
        // resource/variant identity.
        let mut outer_aliased: HashMap<ValueTypeId, u32> = HashMap::new();
        if let InterfaceType::Instance(inst_iface) = iface_ty {
            for (name, &vid) in &inst_iface.type_exports {
                if let Some(&comp_idx) = split.aliased_type_exports.get(name) {
                    outer_aliased.insert(vid, comp_idx);
                }
            }
        }

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
            handler_inst_ty = type_count;
            type_count += 1;
            inst_ctx = ctx;
            types.instance(&inst);
        }

        let (before_inst_ty, after_inst_ty, blocking_inst_ty) = emit_hook_inst_types(
            &mut types,
            &mut type_count,
            has_before,
            has_after,
            has_blocking,
        );
        component.section(&types);

        // Import handler + hooks.
        let (before_inst, after_inst, blocking_inst);
        {
            let mut imports = ComponentImportSection::new();
            handler_inst = instance_count;
            instance_count += 1;
            imports.import(
                target_interface,
                ComponentTypeRef::Instance(handler_inst_ty),
            );
            (before_inst, after_inst, blocking_inst) = emit_hook_imports(
                &mut imports,
                &mut instance_count,
                before_inst_ty,
                after_inst_ty,
                blocking_inst_ty,
            );
            component.section(&imports);
        }

        // Alias funcs + resource types.
        {
            let mut aliases = ComponentAliasSection::new();
            (
                handler_func_base,
                before_comp_func,
                after_comp_func,
                blocking_comp_func,
            ) = emit_func_aliases(
                &mut aliases,
                &mut func_count,
                funcs,
                handler_inst,
                before_inst,
                after_inst,
                blocking_inst,
            );

            // In the provider-split case the handler's resource types
            // came from the preamble's aliased types (we emitted
            // `alias outer` for them inside the handler instance type).
            // They therefore DON'T appear as SubResource exports on the
            // handler instance, and aliasing them via InstanceExport
            // would fail. Reuse the preamble's component-scope indices
            // directly.
            let mut res_vec: Vec<u32> = Vec::new();
            for (vid, export_name, _res_local, _own_local) in &inst_ctx.resource_exports {
                if let Some(&comp_idx) = outer_aliased.get(vid) {
                    res_vec.push(comp_idx);
                } else {
                    let comp_res_idx = type_count;
                    type_count += 1;
                    aliases.alias(Alias::InstanceExport {
                        instance: handler_inst,
                        kind: ComponentExportKind::Type,
                        name: export_name,
                    });
                    res_vec.push(comp_res_idx);
                }
            }
            // Compound types (error-code, DNS-error-payload, …) came
            // from the preamble too; they're not exports on the handler
            // instance, so reuse those indices for the
            // `comp_aliased_types` map consumed by section 3c.
            if let InterfaceType::Instance(inst_iface) = iface_ty {
                for (_name, &vid) in &inst_iface.type_exports {
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
            comp_resource_indices = res_vec;
            component.section(&aliases);
        }
    }

    Ok(ImportsOutcome {
        handler_func_base,
        before_comp_func,
        after_comp_func,
        blocking_comp_func,
        inst_ctx,
        comp_resource_indices,
        comp_aliased_types,
        type_count,
        instance_count,
        func_count,
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
    /// Counter values after the strategy finished. Subsequent phases pick
    /// up from here and continue mutating their own copies.
    type_count: u32,
    instance_count: u32,
    func_count: u32,
}

/// Strategy: import the target interface via a separate types-instance
/// (the WIT-standard split-interface pattern).
///
/// This strategy is used when the target interface has resource types and
/// named type exports. It emits a separate types-instance import (e.g.
/// `wasi:http/types`), aliases all type exports out of it, then declares
/// the handler instance type using `alias outer` references back to those
/// aliased types — so the handler signature shares the *same* resource
/// types as the types interface, instead of declaring fresh `SubResource`
/// definitions.
#[allow(clippy::too_many_arguments)]
fn emit_imports_via_types_iface(
    component: &mut Component,
    target_interface: &str,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    arena: &TypeArena,
    iface_ty: &InterfaceType,
) -> anyhow::Result<ImportsOutcome> {
    let mut type_count: u32 = 0;
    let mut instance_count: u32 = 0;
    let mut func_count: u32 = 0;

    let handler_inst_ty: u32;
    let handler_inst: u32;
    let inst_ctx: InstTypeCtx;
    let handler_func_base: u32;
    let before_comp_func: Option<u32>;
    let after_comp_func: Option<u32>;
    let blocking_comp_func: Option<u32>;
    let mut comp_aliased_types: HashMap<ValueTypeId, u32> = HashMap::new();

    let types_interface = derive_types_interface(target_interface).ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot derive 'types' interface name from target interface '{target_interface}': \
             expected a name with a '/' separator (e.g. 'wasi:http/handler@1.0')"
        )
    })?;

    // We use a static empty map as fallback for the (unreachable in this
    // strategy) non-instance interface case.
    let empty_te: BTreeMap<String, ValueTypeId> = BTreeMap::new();
    let type_exports_ref = if let InterfaceType::Instance(inst) = iface_ty {
        &inst.type_exports
    } else {
        &empty_te
    };

    // Section 1a: hook func types + types instance type.
    // The types instance exports resources AND named compound types
    // (records, variants).
    let types_inst_ty: u32;
    {
        let mut types = ComponentTypeSection::new();
        emit_hook_func_types(&mut types);
        type_count += 2;

        types_inst_ty = type_count;
        type_count += 1;
        {
            // Use the export-aware encoder that interleaves type
            // definitions and exports, ensuring references use export
            // indices.
            let inst = build_types_instance_type(type_exports_ref, arena)?;
            types.instance(&inst);
        }
        component.section(&types);
    }

    // Section 1b: import types instance.
    let types_inst: u32;
    {
        let mut imports = ComponentImportSection::new();
        types_inst = instance_count;
        instance_count += 1;
        imports.import(&types_interface, ComponentTypeRef::Instance(types_inst_ty));
        component.section(&imports);
    }

    // Section 1c: alias ALL type exports from types instance → component
    // scope. Build a name → index map so section 1d can reference each
    // export by its component-scope index.
    let mut comp_type_export_indices: HashMap<String, u32> = HashMap::new();
    {
        let mut aliases = ComponentAliasSection::new();
        for export_name in type_exports_ref.keys() {
            let comp_idx = type_count;
            type_count += 1;
            aliases.alias(Alias::InstanceExport {
                instance: types_inst,
                kind: ComponentExportKind::Type,
                name: export_name,
            });
            comp_type_export_indices.insert(export_name.clone(), comp_idx);
        }
        component.section(&aliases);
    }

    // Build maps: resource vid → comp scope index, compound vid → comp
    // scope index. The resource map gets fed to InstTypeCtx so the handler
    // instance type can reference the aliased resources via alias outer
    // instead of declaring fresh SubResources.
    let mut outer_res_map: HashMap<ValueTypeId, u32> = HashMap::new();
    let mut comp_resource_indices: Vec<u32> = Vec::new();
    for (export_name, &vid) in type_exports_ref {
        if let Some(&comp_idx) = comp_type_export_indices.get(export_name) {
            comp_aliased_types.insert(vid, comp_idx);
            if matches!(
                arena.lookup_val(vid),
                ValueType::Resource(_) | ValueType::AsyncHandle
            ) {
                outer_res_map.insert(vid, comp_idx);
                comp_resource_indices.push(comp_idx);
            }
        }
    }

    // Section 1d: handler instance type (with alias outer) + hook inst types.
    let (before_inst_ty, after_inst_ty, blocking_inst_ty);
    {
        let mut types = ComponentTypeSection::new();

        // Handler instance type: uses alias outer for ALL type exports.
        handler_inst_ty = type_count;
        type_count += 1;
        {
            let mut ctx = InstTypeCtx::with_outer_resources(outer_res_map);
            let mut inst = InstanceType::new();

            // Emit alias outer for each type export into the handler
            // instance type.
            for (export_name, &vid) in type_exports_ref {
                if let Some(&comp_idx) = comp_type_export_indices.get(export_name) {
                    let local_idx = inst.type_count();
                    inst.alias(Alias::Outer {
                        kind: ComponentOuterAliasKind::Type,
                        count: 1,
                        index: comp_idx,
                    });
                    if matches!(
                        arena.lookup_val(vid),
                        ValueType::Resource(_) | ValueType::AsyncHandle
                    ) {
                        ctx.alias_locals.insert(vid, local_idx);
                    } else {
                        // For compound types, cache the alias index so
                        // encode_cv doesn't re-define them inline.
                        ctx.cache.insert(vid, local_idx);
                    }
                }
            }

            build_handler_inst_type(&mut ctx, &mut inst, funcs, arena)?;
            inst_ctx = ctx;
            types.instance(&inst);
        }

        // Hook instance types.
        (before_inst_ty, after_inst_ty, blocking_inst_ty) = emit_hook_inst_types(
            &mut types,
            &mut type_count,
            has_before,
            has_after,
            has_blocking,
        );

        component.section(&types);
    }

    // Section 2: import handler + hooks.
    let (before_inst, after_inst, blocking_inst);
    {
        let mut imports = ComponentImportSection::new();
        handler_inst = instance_count;
        instance_count += 1;
        imports.import(
            target_interface,
            ComponentTypeRef::Instance(handler_inst_ty),
        );
        (before_inst, after_inst, blocking_inst) = emit_hook_imports(
            &mut imports,
            &mut instance_count,
            before_inst_ty,
            after_inst_ty,
            blocking_inst_ty,
        );
        component.section(&imports);
    }

    // Section 3: alias funcs from handler + hooks (no resource aliases
    // needed — section 1c already aliased them).
    {
        let mut aliases = ComponentAliasSection::new();
        (
            handler_func_base,
            before_comp_func,
            after_comp_func,
            blocking_comp_func,
        ) = emit_func_aliases(
            &mut aliases,
            &mut func_count,
            funcs,
            handler_inst,
            before_inst,
            after_inst,
            blocking_inst,
        );
        component.section(&aliases);
    }

    Ok(ImportsOutcome {
        handler_func_base,
        before_comp_func,
        after_comp_func,
        blocking_comp_func,
        inst_ctx,
        comp_resource_indices,
        comp_aliased_types,
        type_count,
        instance_count,
        func_count,
    })
}

/// Strategy: inline resources in a single self-contained handler instance type.
///
/// This is the simplest strategy and the fallback when neither a consumer
/// split nor a separate types interface is available. The handler instance
/// type declares its resources directly via `SubResource` exports, so the
/// adapter doesn't need any external types-instance import. After the
/// handler import goes in, hook instance types/imports and func aliases
/// are layered on top, and resource types are aliased out of the handler
/// instance for use by the section 3b `own<T>` declarations.
fn emit_imports_inline_resources(
    component: &mut Component,
    target_interface: &str,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    arena: &TypeArena,
) -> anyhow::Result<ImportsOutcome> {
    let mut type_count: u32 = 0;
    let mut instance_count: u32 = 0;
    let mut func_count: u32 = 0;

    let handler_inst_ty: u32;
    let handler_inst: u32;
    let inst_ctx: InstTypeCtx;
    let handler_func_base: u32;
    let before_comp_func: Option<u32>;
    let after_comp_func: Option<u32>;
    let blocking_comp_func: Option<u32>;
    let comp_resource_indices: Vec<u32>;
    let comp_aliased_types: HashMap<ValueTypeId, u32> = HashMap::new();

    // Section 1: all types in one section.
    let (before_inst_ty, after_inst_ty, blocking_inst_ty);
    {
        let mut types = ComponentTypeSection::new();
        emit_hook_func_types(&mut types);
        type_count += 2;

        // Handler instance type with SubResource exports.
        handler_inst_ty = type_count;
        type_count += 1;
        {
            let mut ctx = InstTypeCtx::new();
            let mut inst = InstanceType::new();
            build_handler_inst_type(&mut ctx, &mut inst, funcs, arena)?;
            inst_ctx = ctx;
            types.instance(&inst);
        }

        (before_inst_ty, after_inst_ty, blocking_inst_ty) = emit_hook_inst_types(
            &mut types,
            &mut type_count,
            has_before,
            has_after,
            has_blocking,
        );

        component.section(&types);
    }

    // Section 2: imports.
    let (before_inst, after_inst, blocking_inst);
    {
        let mut imports = ComponentImportSection::new();
        handler_inst = instance_count;
        instance_count += 1;
        imports.import(
            target_interface,
            ComponentTypeRef::Instance(handler_inst_ty),
        );
        (before_inst, after_inst, blocking_inst) = emit_hook_imports(
            &mut imports,
            &mut instance_count,
            before_inst_ty,
            after_inst_ty,
            blocking_inst_ty,
        );
        component.section(&imports);
    }

    // Section 3: alias funcs + resource types from the handler instance.
    {
        let mut aliases = ComponentAliasSection::new();
        (
            handler_func_base,
            before_comp_func,
            after_comp_func,
            blocking_comp_func,
        ) = emit_func_aliases(
            &mut aliases,
            &mut func_count,
            funcs,
            handler_inst,
            before_inst,
            after_inst,
            blocking_inst,
        );

        // Alias resource types from the handler instance so section 3b
        // can build `own<T>` definitions referencing them.
        let mut res_vec: Vec<u32> = Vec::new();
        for (_vid, export_name, _res_local, _own_local) in &inst_ctx.resource_exports {
            let comp_res_idx = type_count;
            type_count += 1;
            aliases.alias(Alias::InstanceExport {
                instance: handler_inst,
                kind: ComponentExportKind::Type,
                name: export_name,
            });
            res_vec.push(comp_res_idx);
        }
        comp_resource_indices = res_vec;

        component.section(&aliases);
    }

    Ok(ImportsOutcome {
        handler_func_base,
        before_comp_func,
        after_comp_func,
        blocking_comp_func,
        inst_ctx,
        comp_resource_indices,
        comp_aliased_types,
        type_count,
        instance_count,
        func_count,
    })
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

/// Compute the dispatch module's static memory layout from the function
/// table and the hook flags. Pure: no `Component` mutation, no I/O.
fn compute_memory_layout(
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    func_has_strings: &[bool],
    any_has_resources: bool,
) -> MemoryLayout {
    let has_async = funcs.iter().any(|f| f.is_async);
    let has_async_machinery = has_async || has_before || has_after || has_blocking;

    let event_ptr: u32 = {
        let total_name_bytes: u32 = funcs.iter().map(|f| f.name_len).sum();
        let async_result_base = (total_name_bytes + 3) & !3;
        funcs
            .iter()
            .filter_map(|f| {
                f.async_result_mem_offset
                    .map(|off| off + f.async_result_mem_size)
            })
            .max()
            .unwrap_or(async_result_base)
    };

    let block_result_ptr: Option<u32> = if has_blocking {
        Some(event_ptr + 8)
    } else {
        None
    };

    // Realloc is needed by canon lift (for string params) AND by canon
    // lower (for handler functions with complex result types that contain
    // strings/resources). When needed, it lives in the memory module so
    // it's available for both lowering and lifting.
    let needs_realloc = func_has_strings.iter().any(|&b| b) || any_has_resources;

    // bump_start: first free byte in linear memory after all static data.
    let bump_start: u32 = {
        let after_block = event_ptr + 8 + if has_blocking { 4 } else { 0 };
        (after_block + 7) & !7
    };

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
    /// Counter values after the three sections were emitted. The dispatch
    /// phase needs `core_instance_count` to know where to put the env
    /// instance; the canon-lower phase needs `core_func_count` to know
    /// where the lowered hook funcs go.
    core_instance_count: u32,
    core_func_count: u32,
}

/// Sections 4 + 5 + 6. Build the memory provider core module, instantiate
/// it as Core instance 0, and alias its `mem` (and optional `realloc`)
/// exports up into component scope so the canon-lower/lift phases can
/// reference them via `CanonicalOption::Memory` / `Realloc`.
fn emit_memory_provider(
    component: &mut Component,
    needs_realloc: bool,
    bump_start: u32,
) -> MemoryProviderOutcome {
    // The memory provider phase is the first phase that touches the
    // core-* counters, so they all start at 0 here.
    let mut core_instance_count: u32 = 0;
    let mut core_func_count: u32 = 0;
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
        mem_core_inst = core_instance_count;
        core_instance_count += 1;
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
            name: "mem",
        });
        mem_core_realloc = if needs_realloc {
            let idx = core_func_count;
            core_func_count += 1;
            aliases.alias(Alias::CoreInstanceExport {
                instance: mem_core_inst,
                kind: ExportKind::Func,
                name: "realloc",
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
        core_instance_count,
        core_func_count,
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
    /// Core-func counter after the section was emitted.
    core_func_count: u32,
}

/// Section 7. Lower the hook funcs (before/after/blocking), the handler
/// funcs, and the async canonical built-ins, into core funcs that the
/// dispatch core module imports.
#[allow(clippy::too_many_arguments)]
fn emit_canon_lower(
    component: &mut Component,
    funcs: &[AdapterFunc],
    func_has_strings: &[bool],
    func_has_resources: &[bool],
    handler_func_base: u32,
    before_comp_func: Option<u32>,
    after_comp_func: Option<u32>,
    blocking_comp_func: Option<u32>,
    mem_core_mem: u32,
    mem_core_realloc: Option<u32>,
    has_async_machinery: bool,
    comp_result_cvs: &[Option<ComponentValType>],
    mut core_func_count: u32,
) -> CanonLowerOutcome {
    let core_waitable_new: Option<u32>;
    let core_waitable_join: Option<u32>;
    let core_waitable_wait: Option<u32>;
    let core_waitable_drop: Option<u32>;
    let core_subtask_drop: Option<u32>;

    let mut canons = CanonicalFunctionSection::new();

    // Hooks are async-lowered: needs Async + Memory (for string params /
    // bool result) + UTF8.
    let core_before_func = before_comp_func.map(|comp_f| {
        let idx = core_func_count;
        core_func_count += 1;
        canons.lower(
            comp_f,
            [
                CanonicalOption::Async,
                CanonicalOption::Memory(mem_core_mem),
                CanonicalOption::UTF8,
            ],
        );
        idx
    });

    let core_after_func = after_comp_func.map(|comp_f| {
        let idx = core_func_count;
        core_func_count += 1;
        canons.lower(
            comp_f,
            [
                CanonicalOption::Async,
                CanonicalOption::Memory(mem_core_mem),
                CanonicalOption::UTF8,
            ],
        );
        idx
    });

    let core_blocking_func = blocking_comp_func.map(|comp_f| {
        let idx = core_func_count;
        core_func_count += 1;
        canons.lower(
            comp_f,
            [
                CanonicalOption::Async,
                CanonicalOption::Memory(mem_core_mem),
                CanonicalOption::UTF8,
            ],
        );
        idx
    });

    // Lower each handler function. For functions with resources/strings:
    // need Memory + UTF8 + Realloc. For async: also need Async flag.
    let core_handler_func_base = core_func_count;
    for (i, func) in funcs.iter().enumerate() {
        core_func_count += 1;
        let hs = func_has_strings[i];
        let hr = func_has_resources[i];
        let needs_mem = func.is_async && func.result_type_id.is_some() || hs || hr;
        let needs_utf8 = hs || hr;
        let needs_ra = hr || (hs && func.result_is_complex);
        let mut opts: Vec<CanonicalOption> = Vec::new();
        if func.is_async {
            opts.push(CanonicalOption::Async);
        }
        if needs_mem {
            opts.push(CanonicalOption::Memory(mem_core_mem));
        }
        if needs_utf8 {
            opts.push(CanonicalOption::UTF8);
        }
        if needs_ra {
            if let Some(ra) = mem_core_realloc {
                opts.push(CanonicalOption::Realloc(ra));
            }
        }
        canons.lower(handler_func_base + i as u32, opts);
    }

    // Async canonical built-ins — emitted whenever async machinery is
    // needed.
    if has_async_machinery {
        core_waitable_new = Some(core_func_count);
        core_func_count += 1;
        canons.waitable_set_new();

        core_waitable_join = Some(core_func_count);
        core_func_count += 1;
        canons.waitable_join();

        core_waitable_wait = Some(core_func_count);
        core_func_count += 1;
        canons.waitable_set_wait(false, mem_core_mem);

        core_waitable_drop = Some(core_func_count);
        core_func_count += 1;
        canons.waitable_set_drop();

        core_subtask_drop = Some(core_func_count);
        core_func_count += 1;
        canons.subtask_drop();
    } else {
        core_waitable_new = None;
        core_waitable_join = None;
        core_waitable_wait = None;
        core_waitable_drop = None;
        core_subtask_drop = None;
    }

    // task.return canonicals — one per async func (void or not). For
    // functions with resources/complex results: needs Memory + UTF8 for
    // lifting.
    let core_task_return_funcs: Vec<Option<u32>> = funcs
        .iter()
        .enumerate()
        .map(|(fi, func)| {
            if func.is_async {
                let idx = core_func_count;
                core_func_count += 1;
                let tr_result_cv = comp_result_cvs[fi];
                let hr = func_has_resources[fi];
                let hs = func_has_strings[fi];
                let needs_mem = hr || (hs && func.result_is_complex);
                let mut opts: Vec<CanonicalOption> = Vec::new();
                if needs_mem {
                    opts.push(CanonicalOption::Memory(mem_core_mem));
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
        core_func_count,
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
    /// Core-instance counter after both env (Core instance 1) and
    /// dispatch (Core instance 2) instances were emitted.
    core_instance_count: u32,
}

/// Sections 8 + 9. Embed the dispatch core module bytes (built by
/// [`build_dispatch_module`]) and instantiate it against a synthesized
/// `env` core instance whose exports are the canon-lowered hook funcs,
/// handler funcs, and async builtins from section 7.
#[allow(clippy::too_many_arguments)]
fn emit_dispatch_phase(
    component: &mut Component,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    layout: &MemoryLayout,
    arena: &TypeArena,
    mem_core_mem: u32,
    canon_lower: &CanonLowerOutcome,
    mut core_instance_count: u32,
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
            layout.needs_realloc,
            layout.bump_start,
            arena,
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
        let env_inst = core_instance_count;
        core_instance_count += 1;

        let mut env_exports: Vec<(String, ExportKind, u32)> = Vec::new();
        env_exports.push(("mem".to_string(), ExportKind::Memory, mem_core_mem));
        if let Some(idx) = canon_lower.core_before_func {
            env_exports.push(("before_call".to_string(), ExportKind::Func, idx));
        }
        if let Some(idx) = canon_lower.core_after_func {
            env_exports.push(("after_call".to_string(), ExportKind::Func, idx));
        }
        if let Some(idx) = canon_lower.core_blocking_func {
            env_exports.push(("should_block_call".to_string(), ExportKind::Func, idx));
        }
        for (i, _) in funcs.iter().enumerate() {
            env_exports.push((
                format!("handler_f{i}"),
                ExportKind::Func,
                canon_lower.core_handler_func_base + i as u32,
            ));
        }
        // Async builtins.
        if let Some(idx) = canon_lower.core_waitable_new {
            env_exports.push(("waitable_new".to_string(), ExportKind::Func, idx));
        }
        if let Some(idx) = canon_lower.core_waitable_join {
            env_exports.push(("waitable_join".to_string(), ExportKind::Func, idx));
        }
        if let Some(idx) = canon_lower.core_waitable_wait {
            env_exports.push(("waitable_wait".to_string(), ExportKind::Func, idx));
        }
        if let Some(idx) = canon_lower.core_waitable_drop {
            env_exports.push(("waitable_drop".to_string(), ExportKind::Func, idx));
        }
        if let Some(idx) = canon_lower.core_subtask_drop {
            env_exports.push(("subtask_drop".to_string(), ExportKind::Func, idx));
        }
        for (i, tr_idx) in canon_lower.core_task_return_funcs.iter().enumerate() {
            if let Some(idx) = tr_idx {
                env_exports.push((format!("task_return_f{i}"), ExportKind::Func, *idx));
            }
        }

        instances.export_items(
            env_exports
                .iter()
                .map(|(n, k, i)| (n.as_str(), *k, *i))
                .collect::<Vec<_>>(),
        );

        // Core instance 2: dispatch (instantiate with env).
        // Note: we deliberately do NOT increment core_instance_count for
        // this instance — the original code didn't either, and nothing
        // downstream reads the post-phase value. Preserving the original
        // behaviour keeps the orchestrator's counter handling identical.
        dispatch_core_inst = core_instance_count;
        instances.instantiate(1u32, [("env", wasm_encoder::ModuleArg::Instance(env_inst))]);

        component.section(&instances);
    }

    Ok(DispatchPhaseOutcome {
        dispatch_core_inst,
        core_instance_count,
    })
}

/// State produced by [`emit_canon_lift_phase`] for use by the export
/// phase. The wrapped funcs become component-scope funcs that the
/// export instance re-exports under the target interface name.
struct CanonLiftOutcome {
    /// First component-scope func index of the lifted wrapper funcs.
    /// Per-handler indices live at `wrapped_func_base + i`.
    wrapped_func_base: u32,
    /// Core-func counter after the wrapper aliases were emitted.
    core_func_count: u32,
    /// Component-scope func counter after the lift section was emitted.
    func_count: u32,
}

/// Sections 10 + 11. Alias the dispatch core instance's wrapper exports
/// up into core-func space (one per handler), then `canon lift` each
/// wrapper into a component-scope function of the matching target
/// function type from section 3c.
///
/// Note: this phase does NOT update `core_func_count` or `func_count`
/// — the original implementation didn't either, and nothing downstream
/// reads those counter values after this point. Preserving the original
/// behaviour keeps the byte output identical.
#[allow(clippy::too_many_arguments)]
fn emit_canon_lift_phase(
    component: &mut Component,
    funcs: &[AdapterFunc],
    func_has_strings: &[bool],
    func_has_resources: &[bool],
    dispatch_core_inst: u32,
    target_func_ty_base: u32,
    mem_core_mem: u32,
    mem_core_realloc: Option<u32>,
    needs_realloc: bool,
    core_func_count: u32,
    func_count: u32,
) -> CanonLiftOutcome {
    // ── 10. Alias core wrapper functions from dispatch instance ────────
    let core_wrapper_func_base: u32;
    {
        let mut aliases = ComponentAliasSection::new();
        core_wrapper_func_base = core_func_count;
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
    let wrapped_func_base: u32;
    {
        let mut canons = CanonicalFunctionSection::new();
        wrapped_func_base = func_count;
        for (i, func) in funcs.iter().enumerate() {
            let mut opts: Vec<CanonicalOption> = if func.is_async {
                vec![CanonicalOption::Async]
            } else {
                vec![]
            };
            let hs = func_has_strings[i];
            let hr = func_has_resources[i];
            let needs_mem = hs || hr || func.result_is_complex;
            if needs_mem {
                opts.push(CanonicalOption::Memory(mem_core_mem));
            }
            if hs || hr {
                opts.push(CanonicalOption::UTF8);
            }
            if needs_realloc && (hs || hr) {
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

    CanonLiftOutcome {
        wrapped_func_base,
        core_func_count,
        func_count,
    }
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
    target_interface: &str,
    funcs: &[AdapterFunc],
    iface_ty: &InterfaceType,
    split_imports: Option<&SplitImports>,
    inst_ctx: &InstTypeCtx,
    comp_resource_indices: &[u32],
    comp_aliased_types: &HashMap<ValueTypeId, u32>,
    instance_count: u32,
    wrapped_func_base: u32,
) {
    // ── 12. Component instance: export instance for target interface ──
    let export_inst: u32;
    {
        let mut comp_instances = ComponentInstanceSection::new();
        export_inst = instance_count;

        let mut export_items: Vec<(&str, ComponentExportKind, u32)> = Vec::new();

        // When the handler import came from raw consumer-split sections,
        // re-export its type exports so the adapter's handler export
        // matches what consumers expect.
        let handler_from_split = split_imports
            .map(|s| s.import_names.iter().any(|n| n == target_interface))
            .unwrap_or(false);
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
    type_count_in: u32,
) -> anyhow::Result<HandlerTypesOutcome> {
    let mut type_count = type_count_in;
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
            let own_idx = type_count;
            type_count += 1;
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
                    &mut type_count,
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
                        &mut type_count,
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
        target_func_ty_base = type_count;
        let mut result_cvs: Vec<Option<ComponentValType>> = Vec::new();
        for (func, FuncSig { params, result }) in funcs.iter().zip(pre_encoded.into_iter()) {
            type_count += 1;
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
    let _ = type_count;
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
pub(super) fn build_adapter_bytes(
    target_interface: &str,
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    arena: &TypeArena,
    iface_ty: &InterfaceType,
    split_imports: Option<&SplitImports>,
) -> anyhow::Result<Vec<u8>> {
    // Per-function: does any param/result require Memory+UTF8?
    // Uses deep string check (traverses compound types).
    let func_has_strings: Vec<bool> = funcs.iter().map(|f| f.has_strings(arena)).collect();

    // Per-function: does any param/result contain a resource type?
    let func_has_resources: Vec<bool> = funcs
        .iter()
        .map(|f| {
            f.param_type_ids
                .iter()
                .any(|&id| type_has_resources(id, arena))
                || f.result_type_id
                    .map(|id| type_has_resources(id, arena))
                    .unwrap_or(false)
        })
        .collect();
    let any_has_resources = func_has_resources.iter().any(|&b| b);

    let mut component = Component::new();

    // ── 1–3. Type / Import / Alias sections ─────────────────────────────────
    //
    // Three strategies for emitting the consumer-side import preamble. The
    // strategy is picked based on what we know about the target interface:
    //
    //   - If we have a consumer split's bytes, copy its type/import/alias
    //     sections verbatim and either reuse or rebuild the handler import
    //     on top of them.
    //   - If the target has resources AND named type exports, import a
    //     separate types-instance and `alias outer` it into the handler
    //     instance type (the WIT-standard split-types pattern).
    //   - Otherwise, emit a single self-contained handler instance type
    //     with `SubResource` exports inline.
    //
    // Each strategy returns an `ImportsOutcome` with the indices and
    // counter values the post-strategy phases need.
    let has_type_exports = match iface_ty {
        InterfaceType::Instance(inst) => !inst.type_exports.is_empty(),
        _ => false,
    };

    let ImportsOutcome {
        handler_func_base,
        before_comp_func,
        after_comp_func,
        blocking_comp_func,
        inst_ctx,
        comp_resource_indices,
        comp_aliased_types,
        type_count: type_count_after,
        instance_count: instance_count_after,
        func_count: func_count_after,
    } = if let Some(split) = split_imports {
        emit_imports_from_consumer_split(
            &mut component,
            target_interface,
            funcs,
            has_before,
            has_after,
            has_blocking,
            arena,
            iface_ty,
            split,
        )?
    } else if any_has_resources && has_type_exports {
        emit_imports_via_types_iface(
            &mut component,
            target_interface,
            funcs,
            has_before,
            has_after,
            has_blocking,
            arena,
            iface_ty,
        )?
    } else {
        emit_imports_inline_resources(
            &mut component,
            target_interface,
            funcs,
            has_before,
            has_after,
            has_blocking,
            arena,
        )?
    };

    // ── Index counters ─────────────────────────────────────────────────────
    //
    // `func_count` and `instance_count` are initialized from the
    // import-strategy outcome below — they're declared here only for
    // visibility into the post-strategy phases (`func_count` is read by
    // canon-lift, `instance_count` is read by the export phase). The
    // core-* counters are owned by the phase functions that produce
    // them; the orchestrator only carries the values it has to forward.
    let instance_count = instance_count_after;
    let func_count = func_count_after;

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
        type_count_after,
    )?;

    let layout = compute_memory_layout(
        funcs,
        has_before,
        has_after,
        has_blocking,
        &func_has_strings,
        any_has_resources,
    );

    let MemoryProviderOutcome {
        mem_core_mem,
        mem_core_realloc,
        core_instance_count: core_instance_count_after_mem,
        core_func_count: core_func_count_after_mem,
    } = emit_memory_provider(&mut component, layout.needs_realloc, layout.bump_start);

    let canon_lower = emit_canon_lower(
        &mut component,
        funcs,
        &func_has_strings,
        &func_has_resources,
        handler_func_base,
        before_comp_func,
        after_comp_func,
        blocking_comp_func,
        mem_core_mem,
        mem_core_realloc,
        layout.has_async_machinery,
        &comp_result_cvs,
        core_func_count_after_mem,
    );

    let DispatchPhaseOutcome {
        dispatch_core_inst,
        core_instance_count: _core_instance_count_after_dispatch,
    } = emit_dispatch_phase(
        &mut component,
        funcs,
        has_before,
        has_after,
        has_blocking,
        &layout,
        arena,
        mem_core_mem,
        &canon_lower,
        core_instance_count_after_mem,
    )?;

    let CanonLiftOutcome {
        wrapped_func_base,
        core_func_count: _core_func_count_after_lift,
        func_count: _func_count_after_lift,
    } = emit_canon_lift_phase(
        &mut component,
        funcs,
        &func_has_strings,
        &func_has_resources,
        dispatch_core_inst,
        target_func_ty_base,
        mem_core_mem,
        mem_core_realloc,
        layout.needs_realloc,
        canon_lower.core_func_count,
        func_count,
    );

    emit_export_phase(
        &mut component,
        target_interface,
        funcs,
        iface_ty,
        split_imports,
        &inst_ctx,
        &comp_resource_indices,
        &comp_aliased_types,
        instance_count,
        wrapped_func_base,
    );

    Ok(component.finish())
}
