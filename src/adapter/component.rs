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
//! 9.  Build the **`env` core instance** (synthesised from the
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
    ComponentExportKind, ComponentExportSection, ComponentImportSection,
    ComponentInstanceSection, ComponentOuterAliasKind, ComponentSectionId, ComponentTypeRef,
    ComponentTypeSection, ComponentValType, ExportKind, InstanceSection, InstanceType,
    ModuleSection, PrimitiveValType, RawSection,
};

use super::dispatch::{build_dispatch_module, build_mem_module};
use super::encoders::{build_types_instance_type, encode_comp_cv, InstTypeCtx};
use super::func::AdapterFunc;
use super::split_imports::SplitImports;
use super::ty::{collect_resource_ids, type_has_resources};

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
) {
    let mut pp_cvs: Vec<Vec<ComponentValType>> = Vec::new();
    let mut pr_cv: Vec<Option<ComponentValType>> = Vec::new();
    for func in funcs.iter() {
        let p_cvs: Vec<ComponentValType> = func
            .param_type_ids
            .iter()
            .map(|&id| ctx.encode_cv(id, inst, arena))
            .collect();
        let r_cv = func
            .result_type_id
            .map(|id| ctx.encode_cv(id, inst, arena));
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

/// Append (optional) imports for the splicer:adapter/{before,after,blocking}
/// instances and return the component-scope instance index for each.
fn emit_hook_imports(
    imports: &mut ComponentImportSection,
    instance_count: &mut u32,
    before_ty: Option<u32>,
    after_ty: Option<u32>,
    blocking_ty: Option<u32>,
) -> (Option<u32>, Option<u32>, Option<u32>) {
    let before_inst = before_ty.map(|ty_idx| {
        let idx = *instance_count;
        *instance_count += 1;
        imports.import("splicer:adapter/before", ComponentTypeRef::Instance(ty_idx));
        idx
    });
    let after_inst = after_ty.map(|ty_idx| {
        let idx = *instance_count;
        *instance_count += 1;
        imports.import("splicer:adapter/after", ComponentTypeRef::Instance(ty_idx));
        idx
    });
    let blocking_inst = blocking_ty.map(|ty_idx| {
        let idx = *instance_count;
        *instance_count += 1;
        imports.import("splicer:adapter/blocking", ComponentTypeRef::Instance(ty_idx));
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

/// Build the full Wasm component binary for the tier-1 adapter.
///
/// Uses `wasm_encoder::Component` with explicit section management so that
/// we can create a component instance from exported items (a capability not
/// exposed as a public method by `ComponentBuilder`).
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
    // ── Index counters ─────────────────────────────────────────────────────
    let mut type_count: u32 = 0;

    let mut func_count: u32 = 0;
    let mut instance_count: u32 = 0;
    let mut core_module_count: u32 = 0;
    let mut core_instance_count: u32 = 0;
    let mut core_func_count: u32 = 0;
    let core_memory_count: u32 = 0;

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

    // DEBUG: trace path selection
    if let InterfaceType::Instance(inst) = iface_ty {
        eprintln!("[adapter] any_has_resources={} has_type_exports={} type_exports={:?}",
            any_has_resources, !inst.type_exports.is_empty(),
            inst.type_exports.keys().collect::<Vec<_>>());
        for func in funcs.iter() {
            eprintln!("[adapter]   func '{}': params={:?} result={:?}",
                func.name,
                func.param_type_ids.iter().map(|&id| format!("{:?}", arena.lookup_val(id))).collect::<Vec<_>>(),
                func.result_type_id.map(|id| format!("{:?}", arena.lookup_val(id))));
        }
    }
    eprintln!("[adapter] resource_ids will be: {:?}", if any_has_resources {
        collect_resource_ids(funcs, arena).iter().map(|(_, n)| n.clone()).collect::<Vec<_>>()
    } else { vec![] });

    // Collect distinct resource ValueTypeIds and their names.
    let resource_ids: Vec<(ValueTypeId, String)> = if any_has_resources {
        collect_resource_ids(funcs, arena)
    } else {
        vec![]
    };

    let mut component = Component::new();

    // ── 1–3. Type / Import / Alias sections ─────────────────────────────────
    //
    // Two code paths:
    //   (A) any_has_resources == true  → types-instance import pattern
    //   (B) any_has_resources == false → original single-instance pattern

    let handler_inst_ty: u32;
    let before_inst_ty: Option<u32>;
    let after_inst_ty: Option<u32>;
    let blocking_inst_ty: Option<u32>;
    let inst_ctx: InstTypeCtx;

    let handler_inst: u32;
    let before_inst: Option<u32>;
    let after_inst: Option<u32>;
    let blocking_inst: Option<u32>;

    let handler_func_base: u32;
    let before_comp_func: Option<u32>;
    let after_comp_func: Option<u32>;
    let blocking_comp_func: Option<u32>;
    let comp_resource_indices: Vec<u32>;
    // Maps vid → comp scope type index for ALL aliased types (resources + compounds).
    let mut comp_aliased_types: HashMap<ValueTypeId, u32> = HashMap::new();

    // The "via separate types interface" strategy requires named type exports
    // (from wirm) so we can build the types instance with proper export names.
    // When type_exports is empty (e.g. the composed component's handler
    // interface didn't resolve resource names), fall back to the inline-resources
    // strategy which uses SubResource exports inside the handler instance type.
    let has_type_exports = match iface_ty {
        InterfaceType::Instance(inst) => !inst.type_exports.is_empty(),
        _ => false,
    };

    if let Some(split) = split_imports {
        eprintln!(
            "[adapter.consumer-split] target={:?} import_names={:?} type_count={} instance_count={}",
            target_interface, split.import_names, split.type_count, split.instance_count
        );
        // ── Strategy: pass-through consumer split imports ─────────────────────
        //
        // Copy the consumer split's type/import/alias sections verbatim.
        // This gives the adapter the same type definitions and pass-through
        // imports (types instance, WASI interfaces) as the consumer.
        //
        // If the split imports the handler, we use that import directly.
        // If the split exports the handler (it both consumes and re-exports
        // the target interface, so the consumer split IS itself a provider),
        // we build the handler import type from the interface type info.

        // Copy raw type/import/alias sections from the split
        for (section_id, data) in &split.raw_sections {
            component.section(&RawSection {
                id: *section_id,
                data,
            });
        }
        type_count = split.type_count;
        instance_count = split.instance_count;

        // Check if the handler is already imported by the split
        let handler_in_split = split.import_names.iter().any(|n| n == target_interface);

        if handler_in_split {
            // Handler import came from the raw sections — find its instance index
            let handler_idx = split
                .import_names
                .iter()
                .position(|n| n == target_interface)
                .unwrap() as u32;
            handler_inst = handler_idx;

            // Hook types + imports
            {
                let mut types = ComponentTypeSection::new();
                let (bt, at, blt) = emit_hook_inst_types(
                    &mut types, &mut type_count, has_before, has_after, has_blocking,
                );
                before_inst_ty = bt;
                after_inst_ty = at;
                blocking_inst_ty = blt;
                component.section(&types);
            }
            {
                let mut imports = ComponentImportSection::new();
                let (bi, ai, bli) = emit_hook_imports(
                    &mut imports, &mut instance_count, before_inst_ty, after_inst_ty, blocking_inst_ty,
                );
                before_inst = bi;
                after_inst = ai;
                blocking_inst = bli;
                component.section(&imports);
            }

            // Alias funcs
            {
                let mut aliases = ComponentAliasSection::new();
                let (dfb, bcf, acf, blcf) = emit_func_aliases(
                    &mut aliases, &mut func_count, funcs, handler_inst,
                    before_inst, after_inst, blocking_inst,
                );
                handler_func_base = dfb;
                before_comp_func = bcf;
                after_comp_func = acf;
                blocking_comp_func = blcf;
                component.section(&aliases);
            }

            // Build inst_ctx to discover resource exports (needed for
            // sections 3b/3c), but don't emit the instance type — it came
            // from the raw sections.
            {
                let mut ctx = InstTypeCtx::new();
                let mut dummy_inst = InstanceType::new();
                build_handler_inst_type(&mut ctx, &mut dummy_inst, funcs, arena);
                inst_ctx = ctx;
            }

            // Alias ALL type exports from the handler instance — both
            // resources (request, response) and compound types (error-code).
            // This ensures the adapter's exported function type references
            // the same types as the handler import.
            {
                let mut aliases = ComponentAliasSection::new();
                let mut res_vec: Vec<u32> = Vec::new();

                // Alias resources
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

                // Alias compound type exports (e.g. error-code)
                if let InterfaceType::Instance(inst) = iface_ty {
                    for (export_name, &vid) in &inst.type_exports {
                        if !matches!(arena.lookup_val(vid), ValueType::Resource(_) | ValueType::AsyncHandle) {
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
            handler_inst_ty = 0; // handler type came from raw sections
        } else {
            // The consumer split exports the handler rather than importing it
            // (it consumes some of the same shared types and re-exports the
            // target interface). Build the handler import type from the
            // interface type information. The raw sections already provide shared
            // type definitions (e.g. wasi:http/types) that the handler references.

            // Build handler instance type using the existing InstTypeCtx logic.
            // The type_count starts from where the raw sections left off.
            let mut types = ComponentTypeSection::new();

            if any_has_resources {
                // Build handler instance type with alias outer for resources
                let mut ctx = InstTypeCtx::with_outer_resources(HashMap::new());
                let mut inst = InstanceType::new();
                // Resources are defined in the raw sections but we don't have
                // their type indices. Use the simple SubResource path instead —
                // the types instance from the raw sections provides the context.
                build_handler_inst_type(&mut ctx, &mut inst, funcs, arena);
                handler_inst_ty = type_count;
                type_count += 1;
                inst_ctx = ctx;
                types.instance(&inst);
            } else {
                let mut ctx = InstTypeCtx::new();
                let mut inst = InstanceType::new();
                build_handler_inst_type(&mut ctx, &mut inst, funcs, arena);
                handler_inst_ty = type_count;
                type_count += 1;
                inst_ctx = ctx;
                types.instance(&inst);
            }

            let (bt, at, blt) = emit_hook_inst_types(
                &mut types, &mut type_count, has_before, has_after, has_blocking,
            );
            before_inst_ty = bt;
            after_inst_ty = at;
            blocking_inst_ty = blt;
            component.section(&types);

            // Import handler + hooks
            {
                let mut imports = ComponentImportSection::new();
                handler_inst = instance_count;
                instance_count += 1;
                imports.import(target_interface, ComponentTypeRef::Instance(handler_inst_ty));
                let (bi, ai, bli) = emit_hook_imports(
                    &mut imports, &mut instance_count, before_inst_ty, after_inst_ty, blocking_inst_ty,
                );
                before_inst = bi;
                after_inst = ai;
                blocking_inst = bli;
                component.section(&imports);
            }

            // Alias funcs + resource types
            {
                let mut aliases = ComponentAliasSection::new();
                let (dfb, bcf, acf, blcf) = emit_func_aliases(
                    &mut aliases, &mut func_count, funcs, handler_inst,
                    before_inst, after_inst, blocking_inst,
                );
                handler_func_base = dfb;
                before_comp_func = bcf;
                after_comp_func = acf;
                blocking_comp_func = blcf;

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
        }
    } else if any_has_resources && has_type_exports {
        // ── Strategy: import via a separate types interface ──────────────────
        //
        // 1a. ComponentTypeSection: hook func types + types instance type
        // 1b. Import types instance
        // 1c. Alias resources from types instance
        // 1d. ComponentTypeSection: handler instance type (alias outer) + hook inst types
        // 2.  Import handler + hooks
        // 3.  Alias funcs from handler + hooks

        let types_interface = derive_types_interface(target_interface)
            .expect("Cannot derive types interface name from target");

        // Get type exports from the interface type (resources + named compound types).
        let iface_type_exports: &BTreeMap<String, ValueTypeId> = match iface_ty {
            InterfaceType::Instance(inst) => &inst.type_exports,
            _ => &BTreeMap::new(), // shouldn't happen
        };
        // We'll use a static empty map as fallback.
        let empty_te: BTreeMap<String, ValueTypeId> = BTreeMap::new();
        let type_exports_ref = if let InterfaceType::Instance(inst) = iface_ty {
            &inst.type_exports
        } else {
            &empty_te
        };

        // Section 1a: hook func types + types instance type
        // The types instance exports resources AND named compound types (records, variants).
        let types_inst_ty: u32;
        {
            let mut types = ComponentTypeSection::new();
            emit_hook_func_types(&mut types);
            type_count += 2;

            types_inst_ty = type_count;
            type_count += 1;
            {
                // Use the export-aware encoder that interleaves type definitions
                // and exports, ensuring references use export indices.
                let inst = build_types_instance_type(type_exports_ref, arena);
                types.instance(&inst);
            }
            component.section(&types);
        }

        // Section 1b: import types instance
        let types_inst: u32;
        {
            let mut imports = ComponentImportSection::new();
            types_inst = instance_count;
            instance_count += 1;
            imports.import(
                &types_interface,
                ComponentTypeRef::Instance(types_inst_ty),
            );
            component.section(&imports);
        }

        // Section 1c: alias ALL type exports from types instance → component scope
        let comp_type_export_base = type_count;
        // Maps export_name → component-scope type index
        let mut comp_type_export_indices: HashMap<String, u32> = HashMap::new();
        {
            let mut aliases = ComponentAliasSection::new();
            for (export_name, _vid) in type_exports_ref {
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

        // Build maps: resource vid → comp scope index, compound vid → comp scope index
        let mut outer_res_map: HashMap<ValueTypeId, u32> = HashMap::new();
        let mut cri: Vec<u32> = Vec::new();
        for (export_name, &vid) in type_exports_ref {
            if let Some(&comp_idx) = comp_type_export_indices.get(export_name) {
                comp_aliased_types.insert(vid, comp_idx);
                if matches!(arena.lookup_val(vid), ValueType::Resource(_) | ValueType::AsyncHandle) {
                    outer_res_map.insert(vid, comp_idx);
                    cri.push(comp_idx);
                }
            }
        }
        comp_resource_indices = cri;

        // Section 1d: handler instance type (with alias outer) + hook inst types
        {
            let mut types = ComponentTypeSection::new();

            // Handler instance type: uses alias outer for ALL type exports.
            handler_inst_ty = type_count;
            type_count += 1;
            {
                let mut ctx = InstTypeCtx::with_outer_resources(outer_res_map);
                let mut inst = InstanceType::new();

                // Emit alias outer for each type export into the handler instance type.
                for (export_name, &vid) in type_exports_ref {
                    if let Some(&comp_idx) = comp_type_export_indices.get(export_name) {
                        let local_idx = inst.type_count();
                        inst.alias(Alias::Outer {
                            kind: ComponentOuterAliasKind::Type,
                            count: 1,
                            index: comp_idx,
                        });
                        if matches!(arena.lookup_val(vid), ValueType::Resource(_) | ValueType::AsyncHandle) {
                            ctx.alias_locals.insert(vid, local_idx);
                        } else {
                            // For compound types, cache the alias index so encode_cv
                            // doesn't re-define them inline.
                            ctx.cache.insert(vid, local_idx);
                        }
                    }
                }

                build_handler_inst_type(&mut ctx, &mut inst, funcs, arena);
                inst_ctx = ctx;
                types.instance(&inst);
            }

            // Hook instance types
            let (bt, at, blt) = emit_hook_inst_types(
                &mut types,
                &mut type_count,
                has_before,
                has_after,
                has_blocking,
            );
            before_inst_ty = bt;
            after_inst_ty = at;
            blocking_inst_ty = blt;

            component.section(&types);
        }

        // Section 2: import handler + hooks
        {
            let mut imports = ComponentImportSection::new();
            handler_inst = instance_count;
            instance_count += 1;
            imports.import(
                target_interface,
                ComponentTypeRef::Instance(handler_inst_ty),
            );
            let (bi, ai, bli) = emit_hook_imports(
                &mut imports,
                &mut instance_count,
                before_inst_ty,
                after_inst_ty,
                blocking_inst_ty,
            );
            before_inst = bi;
            after_inst = ai;
            blocking_inst = bli;
            component.section(&imports);
        }

        // Section 3: alias funcs from handler + hooks (no resource aliases needed)
        {
            let mut aliases = ComponentAliasSection::new();
            let (dfb, bcf, acf, blcf) = emit_func_aliases(
                &mut aliases,
                &mut func_count,
                funcs,
                handler_inst,
                before_inst,
                after_inst,
                blocking_inst,
            );
            handler_func_base = dfb;
            before_comp_func = bcf;
            after_comp_func = acf;
            blocking_comp_func = blcf;
            component.section(&aliases);
        }
    } else {
        // ── Strategy: inline resources (single self-contained instance type) ─

        // Section 1: all types in one section
        {
            let mut types = ComponentTypeSection::new();
            emit_hook_func_types(&mut types);
            type_count += 2;

            // Downstream instance type with SubResource exports.
            handler_inst_ty = type_count;
            type_count += 1;
            {
                let mut ctx = InstTypeCtx::new();
                let mut inst = InstanceType::new();
                build_handler_inst_type(&mut ctx, &mut inst, funcs, arena);
                inst_ctx = ctx;
                types.instance(&inst);
            }

            let (bt, at, blt) = emit_hook_inst_types(
                &mut types,
                &mut type_count,
                has_before,
                has_after,
                has_blocking,
            );
            before_inst_ty = bt;
            after_inst_ty = at;
            blocking_inst_ty = blt;

            component.section(&types);
        }

        // Section 2: imports
        {
            let mut imports = ComponentImportSection::new();
            handler_inst = instance_count;
            instance_count += 1;
            imports.import(
                target_interface,
                ComponentTypeRef::Instance(handler_inst_ty),
            );
            let (bi, ai, bli) = emit_hook_imports(
                &mut imports,
                &mut instance_count,
                before_inst_ty,
                after_inst_ty,
                blocking_inst_ty,
            );
            before_inst = bi;
            after_inst = ai;
            blocking_inst = bli;
            component.section(&imports);
        }

        // Section 3: alias funcs + resource types from handler instance
        {
            let mut aliases = ComponentAliasSection::new();
            let (dfb, bcf, acf, blcf) = emit_func_aliases(
                &mut aliases,
                &mut func_count,
                funcs,
                handler_inst,
                before_inst,
                after_inst,
                blocking_inst,
            );
            handler_func_base = dfb;
            before_comp_func = bcf;
            after_comp_func = acf;
            blocking_comp_func = blcf;

            // Alias resource types from the handler instance.
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
    }

    // ── 3b. Type section B: own<T> types for each aliased resource ─────────

    // Maps ValueTypeId → component-level own<T> type index.
    // Named resources have distinct ValueTypeIds (since TypeArena interns by value),
    // so this correctly maps each distinct resource to its own<T> index.
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
    // Declared here (after aliasing) so they can reference own<T> types.
    //
    // Uses encode_comp_cv to build compound types (result, variant, etc.) at the
    // component level, referencing own<T> types from section 3b.

    let target_func_ty_base: u32;
    // Per-function component-level result CVT (for task.return in section 7).
    let comp_result_cvs: Vec<Option<ComponentValType>>;
    {
        let mut func_types = ComponentTypeSection::new();
        // Pre-populate the cache with aliased compound type indices from the types
        // instance.  This ensures encode_comp_cv uses the aliased indices (e.g. the
        // aliased error-code type) instead of building fresh type definitions.
        // Pre-populate the cache with any aliased compound types so that
        // encode_comp_cv reuses them instead of building fresh definitions.
        let mut comp_cv_cache: HashMap<ValueTypeId, u32> = HashMap::new();
        for (&vid, &comp_idx) in &comp_aliased_types {
            if !matches!(arena.lookup_val(vid), ValueType::Resource(_) | ValueType::AsyncHandle) {
                comp_cv_cache.insert(vid, comp_idx);
            }
        }

        // First pass: pre-encode all compound types for params and results.
        // This must happen BEFORE setting target_func_ty_base so that compound type
        // definitions are emitted into func_types before the function type declarations.
        let mut pre_encoded: Vec<(Vec<(String, ComponentValType)>, Option<ComponentValType>)> =
            Vec::new();
        for func in funcs.iter() {
            let params: Vec<(String, ComponentValType)> = func
                .param_names
                .iter()
                .zip(func.param_type_ids.iter())
                .map(|(n, &id)| {
                    let cv = encode_comp_cv(
                        id,
                        arena,
                        &mut func_types,
                        &mut type_count,
                        &comp_own_by_vid,
                        &mut comp_cv_cache,
                    );
                    (n.clone(), cv)
                })
                .collect();
            let result_cv = func.result_type_id.map(|id| {
                encode_comp_cv(
                    id,
                    arena,
                    &mut func_types,
                    &mut type_count,
                    &comp_own_by_vid,
                    &mut comp_cv_cache,
                )
            });
            pre_encoded.push((params, result_cv));
        }

        // Second pass: declare function types.  target_func_ty_base is set HERE,
        // after all compound types have been added to func_types.
        target_func_ty_base = type_count;
        let mut result_cvs: Vec<Option<ComponentValType>> = Vec::new();
        for (func, (params, result_cv)) in funcs.iter().zip(pre_encoded.into_iter()) {
            type_count += 1;
            result_cvs.push(result_cv);
            let mut fty = func_types.function();
            if func.is_async {
                fty.async_(true);
            }
            fty.params(params.iter().map(|(n, cv)| (n.as_str(), *cv)))
                .result(result_cv);
        }

        comp_result_cvs = result_cvs;
        component.section(&func_types);
    }

    // ── Pre-compute memory layout values needed by both mem module and sections below ──

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

    // Realloc is needed by canon lift (for string params) AND by canon lower (for
    // handler functions with complex result types that contain strings/resources).
    // When needed, it lives in the memory module so it's available for both lowering and lifting.
    let needs_realloc = func_has_strings.iter().any(|&b| b) || any_has_resources;

    // bump_start: first free byte in linear memory after all static data.
    let bump_start: u32 = {
        let after_block = event_ptr + 8 + if has_blocking { 4 } else { 0 };
        (after_block + 7) & !7
    };

    // ── 4. Core module 0: memory provider (+ optional realloc) ───────────

    {
        let mem_module = build_mem_module(needs_realloc, bump_start);
        component.section(&ModuleSection(&mem_module));
        core_module_count += 1;
        let _ = core_module_count;
    }

    // ── 5. Core instance 0: instantiate mem module ────────────────────────

    let mem_core_inst: u32;
    {
        let mut instances = InstanceSection::new();
        mem_core_inst = core_instance_count;
        core_instance_count += 1;
        instances.instantiate::<[(&str, wasm_encoder::ModuleArg); 0], &str>(0u32, []);
        component.section(&instances);
    }

    // ── 6. Alias core memory (and realloc) from mem instance ──────────────

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

    // ── 7. Canon lower: hook funcs + target funcs + async builtins ────────

    let core_before_func: Option<u32>;
    let core_after_func: Option<u32>;
    let core_blocking_func: Option<u32>;
    let core_handler_func_base: u32;

    // Async canonical built-in indices (only valid when has_async).
    let core_waitable_new: Option<u32>;
    let core_waitable_join: Option<u32>;
    let core_waitable_wait: Option<u32>;
    let core_waitable_drop: Option<u32>;
    let core_subtask_drop: Option<u32>;
    // One task.return canonical func per async function that has a result.
    // Indexed parallel to `funcs`; None for sync funcs or async void funcs.
    let core_task_return_funcs: Vec<Option<u32>>;

    {
        let mut canons = CanonicalFunctionSection::new();

        // Hooks are now async: async lower needs Async + Memory (for string params / bool result).
        core_before_func = before_comp_func.map(|comp_f| {
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

        core_after_func = after_comp_func.map(|comp_f| {
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

        core_blocking_func = blocking_comp_func.map(|comp_f| {
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

        // Lower each handler function.
        // For functions with resources/strings: need Memory + UTF8 + Realloc.
        // For async: also need Async flag.
        core_handler_func_base = core_func_count;
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

        // Async canonical built-ins — emitted whenever async machinery is needed.
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

        // task.return canonicals — one per async func (void or not).
        // For functions with resources/complex results: needs Memory + UTF8 for lifting.
        core_task_return_funcs = funcs
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
    }

    // ── 8. Core module 1: dispatch ────────────────────────────────────────

    {
        let dispatch_bytes = build_dispatch_module(
            funcs,
            has_before,
            has_after,
            has_blocking,
            event_ptr,
            block_result_ptr,
            needs_realloc,
            bump_start,
            arena,
        )?;
        // Use RawSection to embed the pre-built module bytes directly.
        component.section(&RawSection {
            id: ComponentSectionId::CoreModule as u8,
            data: &dispatch_bytes,
        });
    }

    // ── 9. Core instances 1 + 2: env + dispatch ───────────────────────────

    let dispatch_core_inst: u32;
    {
        let mut instances = InstanceSection::new();

        // Core instance 1: env (export items that dispatch imports)
        let env_inst = core_instance_count;
        core_instance_count += 1;

        let mut env_exports: Vec<(String, ExportKind, u32)> = Vec::new();
        env_exports.push(("mem".to_string(), ExportKind::Memory, mem_core_mem));
        if let Some(idx) = core_before_func {
            env_exports.push(("before_call".to_string(), ExportKind::Func, idx));
        }
        if let Some(idx) = core_after_func {
            env_exports.push(("after_call".to_string(), ExportKind::Func, idx));
        }
        if let Some(idx) = core_blocking_func {
            env_exports.push(("should_block_call".to_string(), ExportKind::Func, idx));
        }
        for (i, _) in funcs.iter().enumerate() {
            env_exports.push((
                format!("handler_f{i}"),
                ExportKind::Func,
                core_handler_func_base + i as u32,
            ));
        }
        // Async builtins
        if let Some(idx) = core_waitable_new {
            env_exports.push(("waitable_new".to_string(), ExportKind::Func, idx));
        }
        if let Some(idx) = core_waitable_join {
            env_exports.push(("waitable_join".to_string(), ExportKind::Func, idx));
        }
        if let Some(idx) = core_waitable_wait {
            env_exports.push(("waitable_wait".to_string(), ExportKind::Func, idx));
        }
        if let Some(idx) = core_waitable_drop {
            env_exports.push(("waitable_drop".to_string(), ExportKind::Func, idx));
        }
        if let Some(idx) = core_subtask_drop {
            env_exports.push(("subtask_drop".to_string(), ExportKind::Func, idx));
        }
        for (i, tr_idx) in core_task_return_funcs.iter().enumerate() {
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

        // Core instance 2: dispatch (instantiate with env)
        dispatch_core_inst = core_instance_count;
        instances.instantiate(1u32, [("env", wasm_encoder::ModuleArg::Instance(env_inst))]);

        component.section(&instances);
    }

    // ── 10. Alias core wrapper functions from dispatch instance ─────────────

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

    // ── 11. Canon lift: wrapper core funcs → component funcs ──────────────

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

    // ── 12. Component instance: export instance for target interface ───────

    let export_inst: u32;
    {
        let mut comp_instances = ComponentInstanceSection::new();
        export_inst = instance_count;
        // instance_count += 1; // not tracking further

        let mut export_items: Vec<(&str, ComponentExportKind, u32)> = Vec::new();

        // When the handler import came from raw split sections, re-export
        // its type exports so the adapter's handler export matches what
        // consumers expect.
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
                        export_items.push((
                            export_name,
                            ComponentExportKind::Type,
                            comp_idx,
                        ));
                    }
                }
            }
        }

        // Export adapter-wrapped functions
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

    // ── 13. Component export section ──────────────────────────────────────

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

    Ok(component.finish())
}
