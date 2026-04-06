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
    core_results: Vec<ValType>,
    /// Byte offset of `name` in the dispatch module's data segment.
    name_offset: u32,
    /// Byte length of `name` (UTF-8).
    name_len: u32,
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
    let mut offset: u32 = 0;
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

        let name_len = name.len() as u32;
        funcs.push(ProxyFunc {
            name: name.clone(),
            param_names,
            comp_params,
            comp_result,
            core_params,
            core_results,
            name_offset: offset,
            name_len,
        });
        offset += name_len;
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

        // type 0: func(name: string) -> ()
        type_count += 1;
        types
            .function()
            .params([(
                "name",
                ComponentValType::Primitive(PrimitiveValType::String),
            )])
            .result(None);

        // type 1: func(name: string) -> bool
        type_count += 1;
        types
            .function()
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
            types
                .function()
                .params(params.iter().copied())
                .result(func.comp_result);
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
                inst.ty()
                    .function()
                    .params(params.iter().copied())
                    .result(func.comp_result);
                inst.export(&func.name, ComponentTypeRef::Func(fi as u32));
            }
            types.instance(&inst);
        }

        // before instance type: { before-call: func(name: string) -> () }
        before_inst_ty = if has_before {
            let idx = type_count;
            type_count += 1;
            let mut inst = InstanceType::new();
            inst.ty()
                .function()
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

        // blocking instance type: { should-block-call: func(name: string) -> bool }
        blocking_inst_ty = if has_blocking {
            let idx = type_count;
            let mut inst = InstanceType::new();
            inst.ty()
                .function()
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

    let core_before_func: Option<u32>;
    let core_after_func: Option<u32>;
    let core_blocking_func: Option<u32>;
    let core_downstream_func_base: u32;

    {
        let mut canons = CanonicalFunctionSection::new();

        core_before_func = before_comp_func.map(|comp_f| {
            let idx = core_func_count;
            core_func_count += 1;
            canons.lower(
                comp_f,
                [CanonicalOption::Memory(mem_core_mem), CanonicalOption::UTF8],
            );
            idx
        });

        core_after_func = after_comp_func.map(|comp_f| {
            let idx = core_func_count;
            core_func_count += 1;
            canons.lower(
                comp_f,
                [CanonicalOption::Memory(mem_core_mem), CanonicalOption::UTF8],
            );
            idx
        });

        core_blocking_func = blocking_comp_func.map(|comp_f| {
            let idx = core_func_count;
            core_func_count += 1;
            canons.lower(
                comp_f,
                [CanonicalOption::Memory(mem_core_mem), CanonicalOption::UTF8],
            );
            idx
        });

        // Lower each downstream function (pure primitives → no memory needed)
        core_downstream_func_base = core_func_count;
        for (i, _func) in funcs.iter().enumerate() {
            core_func_count += 1;
            canons.lower(downstream_func_base + i as u32, []);
        }

        component.section(&canons);
    }

    // ── 8. Core module 1: dispatch ────────────────────────────────────────

    {
        let dispatch_bytes = build_dispatch_module(funcs, has_before, has_after, has_blocking)?;
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
        for (i, _func) in funcs.iter().enumerate() {
            // For pure primitive types no canonical options needed for lifting.
            canons.lift(
                core_wrapper_func_base + i as u32,
                target_func_ty_base + i as u32,
                [],
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
/// Imports:
///   - `env/mem`                     (memory)
///   - `env/before_call`             (func, if `has_before`)
///   - `env/after_call`              (func, if `has_after`)
///   - `env/should_block_call`       (func, if `has_blocking`)
///   - `env/downstream_f{i}`         (func, for each target function)
///
/// Has a data section with all function names concatenated (at known offsets/lengths)
/// so that wrapper functions can pass the function name string to the hook funcs.
///
/// Exports one wrapper function per target function (same name as the original).
fn build_dispatch_module(
    funcs: &[ProxyFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
) -> anyhow::Result<Vec<u8>> {
    let mut module = Module::new();

    // ── Types ──────────────────────────────────────────────────────────────
    // type 0: hook call   (i32, i32) -> ()    [before-call / after-call]
    // type 1: block call  (i32, i32) -> i32   [should-block-call, bool as i32]
    // type 2..2+N-1: one type per target function

    let hook_core_ty: u32 = 0;
    let block_core_ty: u32 = 1;
    let target_core_ty_base: u32 = 2;

    {
        let mut types = TypeSection::new();

        // type 0: (i32, i32) -> ()
        types.ty().function([ValType::I32, ValType::I32], []);

        // type 1: (i32, i32) -> i32
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);

        // type 2..2+N-1: one per target function
        for func in funcs {
            types.ty().function(
                func.core_params.iter().copied(),
                func.core_results.iter().copied(),
            );
        }

        module.section(&types);
    }

    // ── Imports ────────────────────────────────────────────────────────────
    // Track function import indices in order.

    let before_import_fn: Option<u32>;
    let after_import_fn: Option<u32>;
    let blocking_import_fn: Option<u32>;
    let downstream_import_fn_base: u32;

    {
        let mut imports = ImportSection::new();
        let mut fn_idx: u32 = 0;

        // memory import
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
            imports.import("env", "before_call", EntityType::Function(hook_core_ty));
            Some(idx)
        } else {
            None
        };

        after_import_fn = if has_after {
            let idx = fn_idx;
            fn_idx += 1;
            imports.import("env", "after_call", EntityType::Function(hook_core_ty));
            Some(idx)
        } else {
            None
        };

        blocking_import_fn = if has_blocking {
            let idx = fn_idx;
            fn_idx += 1;
            imports.import(
                "env",
                "should_block_call",
                EntityType::Function(block_core_ty),
            );
            Some(idx)
        } else {
            None
        };

        downstream_import_fn_base = fn_idx;
        for (i, _func) in funcs.iter().enumerate() {
            imports.import(
                "env",
                &format!("downstream_f{i}"),
                EntityType::Function(target_core_ty_base + i as u32),
            );
        }

        module.section(&imports);
    }

    // ── Function declarations ──────────────────────────────────────────────

    let wrapper_fn_base: u32;
    {
        let mut fn_section = FunctionSection::new();
        // The first N defined functions (not imported) are wrappers.
        // Defined functions start at index: downstream_import_fn_base + funcs.len()
        wrapper_fn_base = downstream_import_fn_base + funcs.len() as u32;

        for (i, _) in funcs.iter().enumerate() {
            fn_section.function(target_core_ty_base + i as u32);
        }
        module.section(&fn_section);
    }

    // ── Export section: one export per wrapper function ───────────────────

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

        for (fi, func) in funcs.iter().enumerate() {
            let has_result = func.comp_result.is_some();

            // Validate blocking constraint: should-block-call is only supported for
            // void-returning functions in this version.
            if has_blocking && has_result {
                anyhow::bail!(
                    "Function '{}' returns a value but the middleware exports \
                     `should-block-call`. Blocking is only supported for void-returning \
                     functions in this version. To support non-void blocking, the proxy \
                     generator needs to synthesize a \"blocked\" return value (e.g. an \
                     error variant for result<T,E> types).",
                    func.name
                );
            }

            // Locals: if has_after AND has_result, we need a local to save the result
            // across the downstream call so we can call after-call before returning.
            let result_local_idx: Option<u32>;
            let mut locals: Vec<(u32, ValType)> = Vec::new();
            if has_after && has_result {
                // One local per result type (we only support 1 result in this version)
                let result_val_type = func.core_results[0];
                result_local_idx = Some(func.core_params.len() as u32);
                locals.push((1, result_val_type));
            } else {
                result_local_idx = None;
            }

            let mut f = Function::new(locals);

            // 1. Call before-call("funcname") if present
            if let Some(before_fn) = before_import_fn {
                f.instruction(&Instruction::I32Const(func.name_offset as i32));
                f.instruction(&Instruction::I32Const(func.name_len as i32));
                f.instruction(&Instruction::Call(before_fn));
            }

            // 2. Call should-block-call("funcname") if present (void functions only)
            if let Some(block_fn) = blocking_import_fn {
                f.instruction(&Instruction::I32Const(func.name_offset as i32));
                f.instruction(&Instruction::I32Const(func.name_len as i32));
                f.instruction(&Instruction::Call(block_fn));
                // if non-zero (true = block), return without calling downstream
                f.instruction(&Instruction::If(BlockType::Empty));
                f.instruction(&Instruction::Return);
                f.instruction(&Instruction::End);
            }

            // 3. Call downstream function with all params
            let ds_fn = downstream_import_fn_base + fi as u32;
            for (pi, _) in func.core_params.iter().enumerate() {
                f.instruction(&Instruction::LocalGet(pi as u32));
            }
            f.instruction(&Instruction::Call(ds_fn));

            // Save result to local if we have after-call and a return value
            if let Some(local_idx) = result_local_idx {
                f.instruction(&Instruction::LocalSet(local_idx));
            }

            // 4. Call after-call("funcname") if present
            if let Some(after_fn) = after_import_fn {
                f.instruction(&Instruction::I32Const(func.name_offset as i32));
                f.instruction(&Instruction::I32Const(func.name_len as i32));
                f.instruction(&Instruction::Call(after_fn));
            }

            // Push saved result back onto stack
            if let Some(local_idx) = result_local_idx {
                f.instruction(&Instruction::LocalGet(local_idx));
            }

            f.instruction(&Instruction::End);
            code_section.function(&f);
        }

        module.section(&code_section);

        // ── Data section: function name strings ───────────────────────────────

        {
            let mut data_section = DataSection::new();
            // One active data segment with all names concatenated at offset 0.
            let all_names: Vec<u8> = funcs
                .iter()
                .flat_map(|f| f.name.as_bytes().iter().copied())
                .collect();
            data_section.active(
                0, // memory 0
                &wasm_encoder::ConstExpr::i32_const(0),
                all_names,
            );
            module.section(&data_section);
        }
    }

    Ok(module.finish().to_vec())
}
