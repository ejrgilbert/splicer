use anyhow::Context;
use cviz::model::{InterfaceType, TypeArena, ValueType};
use wasm_encoder::{
    Alias, BlockType, CanonicalFunctionSection, CanonicalOption, CodeSection, Component,
    ComponentAliasSection, ComponentExportKind, ComponentExportSection, ComponentImportSection,
    ComponentInstanceSection, ComponentSectionId, ComponentTypeRef, ComponentTypeSection,
    ComponentValType, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, InstanceSection, InstanceType, Instruction, MemoryType, Module,
    ModuleSection, PrimitiveValType, RawSection, TypeSection, ValType,
};

/// A function in the target interface, fully resolved to both component-level and
/// core-Wasm types for proxy generation.
struct ProxyFunc {
    /// The function's name in the interface.
    name: String,
    /// Whether this function is `async` in the component model.
    is_async: bool,
    /// Parameter names, parallel to `comp_params`. Falls back to `p{i}` when
    /// the cviz model did not carry names (e.g. from JSON input).
    param_names: Vec<String>,
    /// Component-level parameter types (for the lifted wrapper and instance type).
    comp_params: Vec<ComponentValType>,
    /// Single component-level result type, or `None` for void.
    comp_result: Option<ComponentValType>,
    /// Core Wasm parameter types after canonical ABI flattening.
    core_params: Vec<ValType>,
    /// Core Wasm result types after canonical ABI flattening.
    /// For async functions this reflects the sync canonical types (used for task.return).
    core_results: Vec<ValType>,
    /// Byte offset of `name` in the dispatch module's data segment.
    name_offset: u32,
    /// Byte length of `name` (UTF-8).
    name_len: u32,
    /// For async functions that have a result: the byte offset within the dispatch module's
    /// memory where the result will be written by the async-lowered downstream call.
    /// `None` for sync functions or async void functions.
    async_result_mem_offset: Option<u32>,
}

/// Generate a tier-1 proxy component that wraps `middleware_name` and adapts it to
/// export `target_interface`.
///
/// The generated proxy component:
/// - Exports `target_interface` (making it a drop-in replacement for the upstream caller)
/// - Imports the downstream component providing `target_interface`
/// - Imports the middleware via the tier-1 type-erased interface(s)
/// - For each function in `target_interface`:
///   1. Calls `before-call(fn_name)` if the middleware exports it
///   2. Calls `should-block-call(fn_name)` if the middleware exports it; skips
///      the downstream invocation when it returns `true` (void functions only)
///   3. Forwards the call to the downstream (unless blocked)
///   4. Calls `after-call(fn_name)` if the middleware exports it
///
/// Returns the path to the generated proxy `.wasm` file.
pub fn generate_tier1_proxy(
    middleware_name: &str,
    _middleware_path: Option<&str>,
    target_interface: &str,
    middleware_interfaces: &[String],
    interface_type: Option<&InterfaceType>,
    splits_path: &str,
    arena: &TypeArena,
) -> anyhow::Result<String> {
    let iface_ty = interface_type.ok_or_else(|| {
        anyhow::anyhow!(
            "Type information for interface '{}' is required to generate a tier-1 proxy \
             but was not available in the composition graph.",
            target_interface
        )
    })?;

    let funcs = extract_proxy_funcs(iface_ty, arena)?;

    let has_before = middleware_interfaces.iter().any(|i| i.contains("/before"));
    let has_after = middleware_interfaces.iter().any(|i| i.contains("/after"));
    let has_blocking = middleware_interfaces
        .iter()
        .any(|i| i.contains("/blocking"));

    let bytes = build_proxy_bytes(
        target_interface,
        &funcs,
        has_before,
        has_after,
        has_blocking,
    )?;

    let out_path = format!(
        "{splits_path}/splicer_proxy_{}_{}.wasm",
        sanitize_name(middleware_name),
        sanitize_name(target_interface)
    );
    std::fs::write(&out_path, &bytes)
        .with_context(|| format!("Failed to write proxy component to '{}'", out_path))?;

    Ok(out_path)
}

fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

// ─── Type extraction ────────────────────────────────────────────────────────

fn extract_proxy_funcs(
    iface_ty: &InterfaceType,
    arena: &TypeArena,
) -> anyhow::Result<Vec<ProxyFunc>> {
    let inst = match iface_ty {
        InterfaceType::Instance(i) => i,
        InterfaceType::Func(_) => anyhow::bail!(
            "Expected an instance-type interface for tier-1 proxy generation; \
             bare function-type interfaces are not supported."
        ),
    };

    let mut funcs = Vec::new();
    let mut name_offset: u32 = 0;
    // Async result storage lives right after the concatenated function-name bytes,
    // rounded up to 4-byte alignment.
    let total_name_bytes: u32 = inst.functions.keys().map(|n| n.len() as u32).sum();
    let async_result_base: u32 = (total_name_bytes + 3) & !3;
    let mut async_result_cursor: u32 = async_result_base;

    for (name, sig) in &inst.functions {
        let mut param_names = Vec::new();
        let mut comp_params = Vec::new();
        let mut core_params = Vec::new();
        for (i, &id) in sig.params.iter().enumerate() {
            let pname = if i < sig.param_names.len() {
                sig.param_names[i].clone()
            } else {
                format!("p{i}")
            };
            param_names.push(pname);
            let (cv, ct) = primitive_cvt(arena.lookup_val(id), name)?;
            comp_params.push(cv);
            core_params.extend(ct);
        }

        if sig.results.len() > 1 {
            anyhow::bail!(
                "Function '{}' has {} results; only 0 or 1 results are supported \
                 for tier-1 proxy generation in this version.",
                name,
                sig.results.len()
            );
        }
        let (comp_result, core_results) = if sig.results.is_empty() {
            (None, vec![])
        } else {
            let (cv, ct) = primitive_cvt(arena.lookup_val(sig.results[0]), name)?;
            (Some(cv), ct)
        };

        // For async functions with a result, reserve 8 bytes in memory.
        let async_result_mem_offset = if sig.is_async && comp_result.is_some() {
            let off = async_result_cursor;
            async_result_cursor += 8; // conservative: covers i64/f64
            Some(off)
        } else {
            None
        };

        let name_len = name.len() as u32;
        funcs.push(ProxyFunc {
            name: name.clone(),
            is_async: sig.is_async,
            param_names,
            comp_params,
            comp_result,
            core_params,
            core_results,
            name_offset,
            name_len,
            async_result_mem_offset,
        });
        name_offset += name_len;
    }
    Ok(funcs)
}

/// Map a cviz `ValueType` to `(ComponentValType, core_flat_types)`.
///
/// Only primitive types are supported in this version. Complex types (strings in the
/// target interface, records, variants, etc.) produce an error — use `unimplemented!`-style
/// messages so callers can see what needs to be added.
fn primitive_cvt(
    vt: &ValueType,
    fn_name: &str,
) -> anyhow::Result<(ComponentValType, Vec<ValType>)> {
    let (pv, cv) = match vt {
        ValueType::Bool => (PrimitiveValType::Bool, ValType::I32),
        ValueType::S8 => (PrimitiveValType::S8, ValType::I32),
        ValueType::U8 => (PrimitiveValType::U8, ValType::I32),
        ValueType::S16 => (PrimitiveValType::S16, ValType::I32),
        ValueType::U16 => (PrimitiveValType::U16, ValType::I32),
        ValueType::S32 => (PrimitiveValType::S32, ValType::I32),
        ValueType::U32 => (PrimitiveValType::U32, ValType::I32),
        ValueType::S64 => (PrimitiveValType::S64, ValType::I64),
        ValueType::U64 => (PrimitiveValType::U64, ValType::I64),
        ValueType::F32 => (PrimitiveValType::F32, ValType::F32),
        ValueType::F64 => (PrimitiveValType::F64, ValType::F64),
        ValueType::Char => (PrimitiveValType::Char, ValType::I32),
        other => anyhow::bail!(
            "Function '{}' uses type {:?} which is not yet supported for tier-1 proxy \
             generation. Only primitive types (bool, u8..u64, s8..s64, f32, f64, char) \
             are supported in this version.",
            fn_name,
            other
        ),
    };
    Ok((ComponentValType::Primitive(pv), vec![cv]))
}

// ─── Component binary generation ────────────────────────────────────────────

/// Build the full Wasm component binary for the tier-1 proxy.
///
/// Uses `wasm_encoder::Component` with explicit section management so that
/// we can create a component instance from exported items (a capability not
/// exposed as a public method by `ComponentBuilder`).
fn build_proxy_bytes(
    target_interface: &str,
    funcs: &[ProxyFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
) -> anyhow::Result<Vec<u8>> {
    // ── Index counters ─────────────────────────────────────────────────────
    let mut type_count: u32 = 0;
    let mut func_count: u32 = 0;
    let mut instance_count: u32 = 0;
    let mut core_module_count: u32 = 0;
    let mut core_instance_count: u32 = 0;
    let mut core_func_count: u32 = 0;
    let core_memory_count: u32 = 0;

    let mut component = Component::new();

    // ── 1. Component type section ──────────────────────────────────────────
    // Type 0: hook func  func(name: string) -> ()    [before-call / after-call]
    // Type 1: block func func(name: string) -> bool  [should-block-call]
    // Type 2..2+N-1: one func type per target function
    // Type 2+N: downstream instance type
    // Type 2+N+k: one instance type per active tier-1 hook (before / after / blocking)

    let target_func_ty_base: u32;
    let downstream_inst_ty: u32;
    let before_inst_ty: Option<u32>;
    let after_inst_ty: Option<u32>;
    let blocking_inst_ty: Option<u32>;

    {
        let mut types = ComponentTypeSection::new();

        // type 0: async func(name: string) -> ()
        type_count += 1;
        types
            .function()
            .async_(true)
            .params([(
                "name",
                ComponentValType::Primitive(PrimitiveValType::String),
            )])
            .result(None);

        // type 1: async func(name: string) -> bool
        type_count += 1;
        types
            .function()
            .async_(true)
            .params([(
                "name",
                ComponentValType::Primitive(PrimitiveValType::String),
            )])
            .result(Some(ComponentValType::Primitive(PrimitiveValType::Bool)));

        // type 2..2+N-1: one func type per target function
        target_func_ty_base = type_count;
        for func in funcs {
            type_count += 1;
            let params: Vec<(&str, ComponentValType)> = func
                .param_names
                .iter()
                .zip(func.comp_params.iter())
                .map(|(n, &ty)| (n.as_str(), ty))
                .collect();
            let mut fty = types.function();
            if func.is_async {
                fty.async_(true);
            }
            fty.params(params.iter().copied()).result(func.comp_result);
        }

        // downstream instance type: exports each target function
        downstream_inst_ty = type_count;
        type_count += 1;
        {
            let mut inst = InstanceType::new();
            for (fi, func) in funcs.iter().enumerate() {
                let params: Vec<(&str, ComponentValType)> = func
                    .param_names
                    .iter()
                    .zip(func.comp_params.iter())
                    .map(|(n, &ty)| (n.as_str(), ty))
                    .collect();
                let mut fty = inst.ty().function();
                if func.is_async {
                    fty.async_(true);
                }
                fty.params(params.iter().copied()).result(func.comp_result);
                inst.export(&func.name, ComponentTypeRef::Func(fi as u32));
            }
            types.instance(&inst);
        }

        // before instance type: { before-call: async func(name: string) -> () }
        before_inst_ty = if has_before {
            let idx = type_count;
            type_count += 1;
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

        // after instance type
        after_inst_ty = if has_after {
            let idx = type_count;
            type_count += 1;
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

        // blocking instance type: { should-block-call: async func(name: string) -> bool }
        blocking_inst_ty = if has_blocking {
            let idx = type_count;
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

        component.section(&types);
    }

    // ── 2. Component import section ────────────────────────────────────────

    let downstream_inst: u32;
    let before_inst: Option<u32>;
    let after_inst: Option<u32>;
    let blocking_inst: Option<u32>;

    {
        let mut imports = ComponentImportSection::new();

        downstream_inst = instance_count;
        instance_count += 1;
        imports.import(
            target_interface,
            ComponentTypeRef::Instance(downstream_inst_ty),
        );

        before_inst = before_inst_ty.map(|ty_idx| {
            let idx = instance_count;
            instance_count += 1;
            imports.import("splicer:proxy/before", ComponentTypeRef::Instance(ty_idx));
            idx
        });

        after_inst = after_inst_ty.map(|ty_idx| {
            let idx = instance_count;
            instance_count += 1;
            imports.import("splicer:proxy/after", ComponentTypeRef::Instance(ty_idx));
            idx
        });

        blocking_inst = blocking_inst_ty.map(|ty_idx| {
            let idx = instance_count;
            instance_count += 1;
            imports.import("splicer:proxy/blocking", ComponentTypeRef::Instance(ty_idx));
            idx
        });

        component.section(&imports);
    }

    // ── 3. Alias section: component funcs from imported instances ──────────

    // downstream_func_base..+N-1: one alias per target function from downstream instance
    // before_comp_func, after_comp_func, blocking_comp_func: hook function aliases

    let downstream_func_base: u32;
    let before_comp_func: Option<u32>;
    let after_comp_func: Option<u32>;
    let blocking_comp_func: Option<u32>;

    {
        let mut aliases = ComponentAliasSection::new();

        downstream_func_base = func_count;
        for func in funcs {
            aliases.alias(Alias::InstanceExport {
                instance: downstream_inst,
                kind: ComponentExportKind::Func,
                name: &func.name,
            });
            func_count += 1;
        }

        before_comp_func = before_inst.map(|inst_idx| {
            let idx = func_count;
            func_count += 1;
            aliases.alias(Alias::InstanceExport {
                instance: inst_idx,
                kind: ComponentExportKind::Func,
                name: "before-call",
            });
            idx
        });

        after_comp_func = after_inst.map(|inst_idx| {
            let idx = func_count;
            func_count += 1;
            aliases.alias(Alias::InstanceExport {
                instance: inst_idx,
                kind: ComponentExportKind::Func,
                name: "after-call",
            });
            idx
        });

        blocking_comp_func = blocking_inst.map(|inst_idx| {
            let idx = func_count;
            func_count += 1;
            aliases.alias(Alias::InstanceExport {
                instance: inst_idx,
                kind: ComponentExportKind::Func,
                name: "should-block-call",
            });
            idx
        });

        component.section(&aliases);
    }

    // ── 4. Core module 0: memory provider ─────────────────────────────────

    {
        let mem_module = build_mem_module();
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

    // ── 6. Alias core memory from mem instance ────────────────────────────

    let mem_core_mem: u32;
    {
        let mut aliases = ComponentAliasSection::new();
        mem_core_mem = core_memory_count;
        // core_memory_count += 1;
        aliases.alias(Alias::CoreInstanceExport {
            instance: mem_core_inst,
            kind: ExportKind::Memory,
            name: "mem",
        });
        component.section(&aliases);
    }

    // ── 7. Canon lower: hook funcs (need memory) + target funcs ───────────
    //       + async canonical built-ins (if any async funcs present)

    let has_async = funcs.iter().any(|f| f.is_async);
    // Async machinery (waitable-set builtins) is needed whenever target functions are
    // async OR whenever any hook interface exists (hooks are now always async).
    let has_async_machinery = has_async || has_before || has_after || has_blocking;

    // Compute event_ptr: memory offset for waitable-set.wait event output.
    // It sits right after all async result storage slots (8 bytes each).
    let event_ptr: u32 = {
        let total_name_bytes: u32 = funcs.iter().map(|f| f.name_len).sum();
        let async_result_base = (total_name_bytes + 3) & !3;
        let n_result_slots = funcs
            .iter()
            .filter(|f| f.async_result_mem_offset.is_some())
            .count() as u32;
        async_result_base + 8 * n_result_slots
    };

    // For should-block-call (async, returns bool): result is written to a memory slot
    // at event_ptr+8 (the event output itself is 8 bytes).
    let block_result_ptr: Option<u32> = if has_blocking {
        Some(event_ptr + 8)
    } else {
        None
    };

    let core_before_func: Option<u32>;
    let core_after_func: Option<u32>;
    let core_blocking_func: Option<u32>;
    let core_downstream_func_base: u32;

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

        // Lower each downstream function.
        // Sync: no options.  Async: Async option + Memory if the func has a result.
        core_downstream_func_base = core_func_count;
        for (i, func) in funcs.iter().enumerate() {
            core_func_count += 1;
            if func.is_async {
                let opts: Vec<CanonicalOption> = if func.comp_result.is_some() {
                    vec![
                        CanonicalOption::Async,
                        CanonicalOption::Memory(mem_core_mem),
                    ]
                } else {
                    vec![CanonicalOption::Async]
                };
                canons.lower(downstream_func_base + i as u32, opts);
            } else {
                canons.lower(downstream_func_base + i as u32, []);
            }
        }

        // Async canonical built-ins — emitted whenever async machinery is needed
        // (async target functions OR async hooks).
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

        // task.return canonicals — one per async func with a result.
        core_task_return_funcs = funcs
            .iter()
            .map(|func| {
                if func.is_async {
                    if let Some(cv) = func.comp_result {
                        let idx = core_func_count;
                        core_func_count += 1;
                        canons.task_return(Some(cv), []);
                        Some(idx)
                    } else {
                        None // void async: no task.return needed
                    }
                } else {
                    None // sync: no task.return
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
                format!("downstream_f{i}"),
                ExportKind::Func,
                core_downstream_func_base + i as u32,
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

    // ── 10. Alias core wrapper functions from dispatch instance ────────────

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
            let opts: Vec<CanonicalOption> = if func.is_async {
                vec![CanonicalOption::Async]
            } else {
                vec![]
            };
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

        let export_items: Vec<(&str, ComponentExportKind, u32)> = funcs
            .iter()
            .enumerate()
            .map(|(i, func)| {
                (
                    func.name.as_str(),
                    ComponentExportKind::Func,
                    wrapped_func_base + i as u32,
                )
            })
            .collect();
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

// ─── Core module builders ────────────────────────────────────────────────────

/// Build a tiny core Wasm module that exports one 1-page memory as "mem".
/// This memory is shared with the dispatch module for string passing.
fn build_mem_module() -> Module {
    let mut module = Module::new();

    // Memory section: one memory, 1 initial page, no maximum
    {
        let mut mem_section = wasm_encoder::MemorySection::new();
        mem_section.memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&mem_section);
    }

    // Export section: export the memory as "mem"
    {
        let mut exports = ExportSection::new();
        exports.export("mem", ExportKind::Memory, 0);
        module.section(&exports);
    }

    module
}

/// Build the dispatch core Wasm module.
///
/// Handles both sync and async functions in the same module.
///
/// Imports:
///   - `env/mem`                     (memory)
///   - `env/before_call`             (func, if `has_before`)
///   - `env/after_call`              (func, if `has_after`)
///   - `env/should_block_call`       (func, if `has_blocking`)
///   - `env/downstream_f{i}`         (func, for each target function)
///   - async builtins from `env`     (if any async funcs present)
///
/// `event_ptr` is the memory offset reserved for `waitable-set.wait` event output.
fn build_dispatch_module(
    funcs: &[ProxyFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    event_ptr: u32,
    block_result_ptr: Option<u32>,
) -> anyhow::Result<Vec<u8>> {
    let has_async = funcs.iter().any(|f| f.is_async);
    let has_async_machinery = has_async || has_before || has_after || has_blocking;
    let mut module = Module::new();

    // ── Types ──────────────────────────────────────────────────────────────
    //
    // slot 0: hook    (i32, i32) -> ()
    // slot 1: block   (i32, i32) -> i32
    // slot 2..2+N-1:  wrapper types (sync: params→results, async: params→void)
    // if has_async:
    //   slot 2+N..2+N+A-1: downstream async call types (flat_params[,result_ptr]→i32)
    //   slot 2+N+A:   () -> i32          (waitable_set_new)
    //   slot 2+N+A+1: (i32,i32) -> ()   (waitable_join)
    //   slot 2+N+A+2: (i32,i32) -> i32  (waitable_set_wait)
    //   slot 2+N+A+3: (i32) -> ()       (waitable_drop, subtask_drop, task_return_i32)
    //   slot 2+N+A+4: (i64) -> ()       (task_return_i64, if used)
    //   slot 2+N+A+5: (f32) -> ()       (task_return_f32, if used)
    //   slot 2+N+A+6: (f64) -> ()       (task_return_f64, if used)

    let hook_ty: u32 = 0;
    let block_ty: u32 = 1;
    let wrapper_ty_base: u32 = 2;

    // Async downstream call type indices, parallel to funcs (None for sync funcs).
    let mut async_ds_tys: Vec<Option<u32>> = vec![None; funcs.len()];
    // Async builtin type indices.
    let waitable_new_ty: u32;
    let waitable_join_ty: u32;
    let waitable_wait_ty: u32;
    let void_i32_ty: u32; // (i32)->()  shared for drop+task_return_i32
    let void_i64_ty: u32;
    let void_f32_ty: u32;
    let void_f64_ty: u32;

    {
        let mut types = TypeSection::new();
        let mut ty_idx: u32 = 0;

        // slot 0: async-lowered hook (before/after): (ptr, len) -> subtask_handle
        ty_idx += 1;
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);

        // slot 1: async-lowered block (should-block-call): (ptr, len, result_ptr) -> subtask_handle
        ty_idx += 1;
        types
            .ty()
            .function([ValType::I32, ValType::I32, ValType::I32], [ValType::I32]);

        // wrapper types: async wrappers are void, sync wrappers return results
        for func in funcs {
            ty_idx += 1;
            if func.is_async {
                types.ty().function(func.core_params.iter().copied(), []);
            } else {
                types.ty().function(
                    func.core_params.iter().copied(),
                    func.core_results.iter().copied(),
                );
            }
        }

        if has_async_machinery {
            // Async downstream call types: (flat_params..., result_ptr?) -> i32
            let mut async_count: u32 = 0;
            for (i, func) in funcs.iter().enumerate() {
                if func.is_async {
                    async_ds_tys[i] = Some(ty_idx);
                    ty_idx += 1;
                    async_count += 1;
                    let mut params: Vec<ValType> = func.core_params.clone();
                    if func.comp_result.is_some() {
                        params.push(ValType::I32); // result_ptr
                    }
                    types.ty().function(params, [ValType::I32]);
                }
            }
            let _ = async_count;

            waitable_new_ty = ty_idx;
            ty_idx += 1;
            types.ty().function([], [ValType::I32]);

            waitable_join_ty = ty_idx;
            ty_idx += 1;
            types.ty().function([ValType::I32, ValType::I32], []);

            waitable_wait_ty = ty_idx;
            ty_idx += 1;
            types
                .ty()
                .function([ValType::I32, ValType::I32], [ValType::I32]);

            void_i32_ty = ty_idx;
            ty_idx += 1;
            types.ty().function([ValType::I32], []);

            void_i64_ty = ty_idx;
            ty_idx += 1;
            types.ty().function([ValType::I64], []);

            void_f32_ty = ty_idx;
            ty_idx += 1;
            types.ty().function([ValType::F32], []);

            void_f64_ty = ty_idx;
            // ty_idx += 1;
            types.ty().function([ValType::F64], []);
        } else {
            // Placeholders — never used when has_async is false.
            waitable_new_ty = 0;
            waitable_join_ty = 0;
            waitable_wait_ty = 0;
            void_i32_ty = 0;
            void_i64_ty = 0;
            void_f32_ty = 0;
            void_f64_ty = 0;
        }

        module.section(&types);
    }

    // ── Imports ────────────────────────────────────────────────────────────

    let before_import_fn: Option<u32>;
    let after_import_fn: Option<u32>;
    let blocking_import_fn: Option<u32>;
    let downstream_import_fn_base: u32;
    let waitable_new_fn: u32;
    let waitable_join_fn: u32;
    let waitable_wait_fn: u32;
    let waitable_drop_fn: u32;
    let subtask_drop_fn: u32;
    // Per-func task.return import indices (None for sync/void-async).
    let task_return_fns: Vec<Option<u32>>;

    {
        let mut imports = ImportSection::new();
        let mut fn_idx: u32 = 0;

        imports.import(
            "env",
            "mem",
            EntityType::Memory(MemoryType {
                minimum: 1,
                maximum: None,
                memory64: false,
                shared: false,
                page_size_log2: None,
            }),
        );

        before_import_fn = if has_before {
            let idx = fn_idx;
            fn_idx += 1;
            imports.import("env", "before_call", EntityType::Function(hook_ty));
            Some(idx)
        } else {
            None
        };

        after_import_fn = if has_after {
            let idx = fn_idx;
            fn_idx += 1;
            imports.import("env", "after_call", EntityType::Function(hook_ty));
            Some(idx)
        } else {
            None
        };

        blocking_import_fn = if has_blocking {
            let idx = fn_idx;
            fn_idx += 1;
            imports.import("env", "should_block_call", EntityType::Function(block_ty));
            Some(idx)
        } else {
            None
        };

        // Downstream funcs — type is wrapper_ty for sync, async_ds_ty for async.
        downstream_import_fn_base = fn_idx;
        for (i, func) in funcs.iter().enumerate() {
            let ty = if func.is_async {
                async_ds_tys[i].expect("async_ds_ty must be set for async func")
            } else {
                wrapper_ty_base + i as u32
            };
            imports.import("env", &format!("downstream_f{i}"), EntityType::Function(ty));
        }
        fn_idx += funcs.len() as u32;

        // Async builtins — only imported if needed.
        if has_async_machinery {
            waitable_new_fn = fn_idx;
            fn_idx += 1;
            imports.import("env", "waitable_new", EntityType::Function(waitable_new_ty));

            waitable_join_fn = fn_idx;
            fn_idx += 1;
            imports.import(
                "env",
                "waitable_join",
                EntityType::Function(waitable_join_ty),
            );

            waitable_wait_fn = fn_idx;
            fn_idx += 1;
            imports.import(
                "env",
                "waitable_wait",
                EntityType::Function(waitable_wait_ty),
            );

            waitable_drop_fn = fn_idx;
            fn_idx += 1;
            imports.import("env", "waitable_drop", EntityType::Function(void_i32_ty));

            subtask_drop_fn = fn_idx;
            fn_idx += 1;
            imports.import("env", "subtask_drop", EntityType::Function(void_i32_ty));

            // task.return per async func with result.
            let mut trf: Vec<Option<u32>> = vec![None; funcs.len()];
            for (i, func) in funcs.iter().enumerate() {
                if func.is_async && func.comp_result.is_some() {
                    let tr_ty = match func.core_results.first() {
                        Some(ValType::I32) => void_i32_ty,
                        Some(ValType::I64) => void_i64_ty,
                        Some(ValType::F32) => void_f32_ty,
                        Some(ValType::F64) => void_f64_ty,
                        _ => void_i32_ty, // fallback
                    };
                    trf[i] = Some(fn_idx);
                    fn_idx += 1;
                    imports.import(
                        "env",
                        &format!("task_return_f{i}"),
                        EntityType::Function(tr_ty),
                    );
                }
            }
            task_return_fns = trf;
        } else {
            waitable_new_fn = 0;
            waitable_join_fn = 0;
            waitable_wait_fn = 0;
            waitable_drop_fn = 0;
            subtask_drop_fn = 0;
            task_return_fns = vec![None; funcs.len()];
        }

        module.section(&imports);
    }

    // ── Function declarations ──────────────────────────────────────────────

    let wrapper_fn_base: u32;
    {
        let mut fn_section = FunctionSection::new();
        wrapper_fn_base = downstream_import_fn_base
            + funcs.len() as u32
            + if has_async_machinery {
                // waitable_new, join, wait, drop, subtask_drop + task_return funcs
                5 + funcs
                    .iter()
                    .filter(|f| f.is_async && f.comp_result.is_some())
                    .count() as u32
            } else {
                0
            };

        for (i, _) in funcs.iter().enumerate() {
            fn_section.function(wrapper_ty_base + i as u32);
        }
        module.section(&fn_section);
    }

    // ── Export section ────────────────────────────────────────────────────

    {
        let mut exports = ExportSection::new();
        for (i, func) in funcs.iter().enumerate() {
            exports.export(&func.name, ExportKind::Func, wrapper_fn_base + i as u32);
        }
        module.section(&exports);
    }

    // ── Code section ──────────────────────────────────────────────────────

    {
        let mut code_section = CodeSection::new();

        // Macro: await a packed return value from `canon lower async` stored in `$st`.
        //
        // `canon lower async` returns a packed i32:
        //   low 4 bits = Status (Returned=2 means sync-done; Started=1 means pending)
        //   upper 28 bits = raw subtask handle (0 when done synchronously)
        //
        // We shift right by 4 to get the raw handle. Handle=0 → already done, skip wait.
        // Handle != 0 → add to a new waitable-set and block until the event fires.
        // After this macro, `$st` holds the (possibly 0) raw handle (already dropped).
        macro_rules! emit_wait_loop {
            ($f:expr, $st:expr, $ws:expr) => {{
                // Extract raw handle from packed value: handle = packed >> 4
                $f.instruction(&Instruction::LocalGet($st));
                $f.instruction(&Instruction::I32Const(4));
                $f.instruction(&Instruction::I32ShrU);
                $f.instruction(&Instruction::LocalSet($st));
                // If handle != 0 (task is still pending), wait for it.
                $f.instruction(&Instruction::LocalGet($st));
                $f.instruction(&Instruction::If(BlockType::Empty));
                $f.instruction(&Instruction::Call(waitable_new_fn));
                $f.instruction(&Instruction::LocalSet($ws));
                // waitable.join(waitable_handle, set_handle)
                $f.instruction(&Instruction::LocalGet($st));
                $f.instruction(&Instruction::LocalGet($ws));
                $f.instruction(&Instruction::Call(waitable_join_fn));
                $f.instruction(&Instruction::LocalGet($ws));
                $f.instruction(&Instruction::I32Const(event_ptr as i32));
                $f.instruction(&Instruction::Call(waitable_wait_fn));
                $f.instruction(&Instruction::Drop);
                // Drop subtask first (it is a child of ws in the resource table).
                $f.instruction(&Instruction::LocalGet($st));
                $f.instruction(&Instruction::Call(subtask_drop_fn));
                $f.instruction(&Instruction::LocalGet($ws));
                $f.instruction(&Instruction::Call(waitable_drop_fn));
                $f.instruction(&Instruction::End);
            }};
        }

        for (fi, func) in funcs.iter().enumerate() {
            let has_result = func.comp_result.is_some();

            if has_blocking && has_result && !func.is_async {
                anyhow::bail!(
                    "Function '{}' returns a value but the middleware exports \
                     `should-block-call`. Blocking is only supported for void-returning \
                     functions in this version.",
                    func.name
                );
            }
            // ── Async wrapper ──────────────────────────────────────────────
            // Core sig: (flat_params...) -> ()
            // Locals beyond params: [subtask: i32, ws: i32]
            // subtask/ws are reused sequentially for hook and downstream waits.
            let first_local = func.core_params.len() as u32;
            let subtask_local = first_local;
            let ws_local = first_local + 1;
            let locals: Vec<(u32, ValType)> = vec![(2, ValType::I32)];
            let mut f = Function::new(locals);

            // 1. before-call (async: returns subtask handle)
            if let Some(before_fn) = before_import_fn {
                f.instruction(&Instruction::I32Const(func.name_offset as i32));
                f.instruction(&Instruction::I32Const(func.name_len as i32));
                f.instruction(&Instruction::Call(before_fn));
                f.instruction(&Instruction::LocalSet(subtask_local));
                emit_wait_loop!(f, subtask_local, ws_local);
            }

            // 2. blocking (void async only; async: returns subtask + writes bool to result_ptr)
            if let Some(block_fn) = blocking_import_fn {
                if has_result {
                    anyhow::bail!(
                        "Function '{}' is async with a return value and the middleware \
                             exports `should-block-call`. Blocking is not supported for \
                             async functions with results.",
                        func.name
                    );
                }
                let blk_ptr = block_result_ptr.expect("block_result_ptr set when has_blocking");
                f.instruction(&Instruction::I32Const(func.name_offset as i32));
                f.instruction(&Instruction::I32Const(func.name_len as i32));
                f.instruction(&Instruction::I32Const(blk_ptr as i32));
                f.instruction(&Instruction::Call(block_fn));
                f.instruction(&Instruction::LocalSet(subtask_local));
                emit_wait_loop!(f, subtask_local, ws_local);
                // Load the bool result and conditionally return (block the call).
                f.instruction(&Instruction::I32Const(blk_ptr as i32));
                f.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
                    offset: 0,
                    align: 2,
                    memory_index: 0,
                }));
                f.instruction(&Instruction::If(BlockType::Empty));
                f.instruction(&Instruction::Return);
                f.instruction(&Instruction::End);
            }

            let mut result_local_idx = None;
            if func.is_async {
                // 3. Call async-lowered downstream: (flat_params...[, result_ptr]) -> subtask
                let ds_fn = downstream_import_fn_base + fi as u32;
                for (pi, _) in func.core_params.iter().enumerate() {
                    f.instruction(&Instruction::LocalGet(pi as u32));
                }
                if let Some(result_ptr) = func.async_result_mem_offset {
                    f.instruction(&Instruction::I32Const(result_ptr as i32));
                }
                f.instruction(&Instruction::Call(ds_fn));
                f.instruction(&Instruction::LocalSet(subtask_local));
                emit_wait_loop!(f, subtask_local, ws_local);
            } else {
                // ── Sync wrapper ───────────────────────────────────────────────
                let mut locals: Vec<(u32, ValType)> = Vec::new();
                if has_after && has_result {
                    let result_val_type = func.core_results[0];
                    result_local_idx = Some(func.core_params.len() as u32);
                    locals.push((1, result_val_type));
                } else {
                    result_local_idx = None;
                }

                // 3. Call downstream
                let ds_fn = downstream_import_fn_base + fi as u32;
                for (pi, _) in func.core_params.iter().enumerate() {
                    f.instruction(&Instruction::LocalGet(pi as u32));
                }
                f.instruction(&Instruction::Call(ds_fn));

                if let Some(local_idx) = result_local_idx {
                    f.instruction(&Instruction::LocalSet(local_idx));
                }
            }

            // 4. after-call (async: returns subtask handle)
            if let Some(after_fn) = after_import_fn {
                f.instruction(&Instruction::I32Const(func.name_offset as i32));
                f.instruction(&Instruction::I32Const(func.name_len as i32));
                f.instruction(&Instruction::Call(after_fn));
                f.instruction(&Instruction::LocalSet(subtask_local));
                emit_wait_loop!(f, subtask_local, ws_local);
            }

            if func.is_async {
                // 5. task.return with result (if any)
                if let Some(result_ptr) = func.async_result_mem_offset {
                    if let Some(tr_fn) = task_return_fns[fi] {
                        let load_instr = match func.core_results.first() {
                            Some(ValType::I64) => Instruction::I64Load(wasm_encoder::MemArg {
                                offset: 0,
                                align: 3,
                                memory_index: 0,
                            }),
                            Some(ValType::F32) => Instruction::F32Load(wasm_encoder::MemArg {
                                offset: 0,
                                align: 2,
                                memory_index: 0,
                            }),
                            Some(ValType::F64) => Instruction::F64Load(wasm_encoder::MemArg {
                                offset: 0,
                                align: 3,
                                memory_index: 0,
                            }),
                            _ => Instruction::I32Load(wasm_encoder::MemArg {
                                offset: 0,
                                align: 2,
                                memory_index: 0,
                            }),
                        };
                        f.instruction(&Instruction::I32Const(result_ptr as i32));
                        f.instruction(&load_instr);
                        f.instruction(&Instruction::Call(tr_fn));
                    }
                }
            } else {
                // 5. return with result (if any)
                if let Some(local_idx) = result_local_idx {
                    f.instruction(&Instruction::LocalGet(local_idx));
                }
            }

            f.instruction(&Instruction::End);
            code_section.function(&f);
        }

        module.section(&code_section);

        // ── Data section ──────────────────────────────────────────────────────

        {
            let mut data_section = DataSection::new();
            let all_names: Vec<u8> = funcs
                .iter()
                .flat_map(|f| f.name.as_bytes().iter().copied())
                .collect();
            data_section.active(0, &wasm_encoder::ConstExpr::i32_const(0), all_names);
            module.section(&data_section);
        }
    }

    Ok(module.finish().to_vec())
}
