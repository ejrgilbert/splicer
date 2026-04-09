use anyhow::Context;
use cviz::model::{InterfaceType, TypeArena, ValueType, ValueTypeId};
use std::collections::{BTreeMap, HashMap};
use wasm_encoder::{
    Alias, BlockType, CanonicalFunctionSection, CanonicalOption, CodeSection, Component,
    ComponentAliasSection, ComponentExportKind, ComponentExportSection,
    ComponentImportSection, ComponentInstanceSection, ComponentOuterAliasKind,
    ComponentSectionId, ComponentTypeRef, ComponentTypeSection, ComponentValType, DataSection,
    EntityType, ExportKind, ExportSection, Function, FunctionSection, ImportSection,
    InstanceSection, InstanceType, Instruction, MemoryType, Module, ModuleSection,
    PrimitiveValType, RawSection, TypeBounds, TypeSection, ValType,
};

mod split_imports;
use split_imports::{extract_split_imports, SplitImports};

/// A function in the target interface, fully resolved to both component-level and
/// core-Wasm types for adapter generation.
struct AdapterFunc {
    /// The function's name in the interface.
    name: String,
    /// Whether this function is `async` in the component model.
    is_async: bool,
    /// Parameter names, parallel to `param_type_ids`. Falls back to `p{i}` when
    /// the cviz model did not carry names (e.g. from JSON input).
    param_names: Vec<String>,
    /// Original ValueTypeIds for each parameter (for component-level type encoding).
    param_type_ids: Vec<ValueTypeId>,
    /// Original ValueTypeId for the result (for component-level type encoding).
    result_type_id: Option<ValueTypeId>,
    /// True when the result type requires pointer-based passing (>MAX_FLAT_RESULTS flat values).
    result_is_complex: bool,
    /// Core Wasm parameter types after canonical ABI flattening.
    core_params: Vec<ValType>,
    /// Core Wasm result types after canonical ABI flattening.
    /// For async functions this reflects the sync canonical types (used for task.return type).
    /// For complex results this is `[I32]` (the pointer type for task.return).
    core_results: Vec<ValType>,
    /// Byte offset of `name` in the dispatch module's data segment.
    name_offset: u32,
    /// Byte length of `name` (UTF-8).
    name_len: u32,
    /// For async functions that have a result: the byte offset within the dispatch module's
    /// memory where the result will be written by the async-lowered downstream call.
    /// `None` for sync functions or async void functions.
    async_result_mem_offset: Option<u32>,
    /// Number of bytes reserved for the async result in linear memory.
    /// 8 for simple (fits in one register), 512 for complex (pointer-based) results.
    async_result_mem_size: u32,
}

impl AdapterFunc {
    /// Returns true if any parameter or the result contains a string type (deep check).
    fn has_strings(&self, arena: &TypeArena) -> bool {
        self.param_type_ids.iter().any(|&id| type_has_strings(id, arena))
            || self
                .result_type_id
                .is_some_and(|id| type_has_strings(id, arena))
    }
}

/// Generate a tier-1 adapter component that wraps `middleware_name` and adapts it to
/// export `target_interface`.
///
/// The generated adapter component:
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
/// Returns the path to the generated adapter `.wasm` file.
pub fn generate_tier1_adapter(
    middleware_name: &str,
    _middleware_path: Option<&str>,
    target_interface: &str,
    middleware_interfaces: &[String],
    interface_type: Option<&InterfaceType>,
    splits_path: &str,
    downstream_split_path: Option<&str>,
    arena: &TypeArena,
) -> anyhow::Result<String> {
    let iface_ty = interface_type.ok_or_else(|| {
        anyhow::anyhow!(
            "Type information for interface '{}' is required to generate a tier-1 adapter \
             but was not available in the composition graph.",
            target_interface
        )
    })?;

    let funcs = extract_adapter_funcs(iface_ty, arena)?;

    let has_before = middleware_interfaces.iter().any(|i| i.contains("/before"));
    let has_after = middleware_interfaces.iter().any(|i| i.contains("/after"));
    let has_blocking = middleware_interfaces
        .iter()
        .any(|i| i.contains("/blocking"));

    // Extract the downstream split's import structure if available.
    eprintln!("[adapter] downstream_split_path={:?}", downstream_split_path);
    let split_imports = downstream_split_path
        .map(extract_split_imports)
        .transpose()?;

    let bytes = build_adapter_bytes(
        target_interface,
        &funcs,
        has_before,
        has_after,
        has_blocking,
        arena,
        iface_ty,
        split_imports.as_ref(),
    )?;

    let out_path = format!(
        "{splits_path}/splicer_adapter_{}_{}.wasm",
        sanitize_name(middleware_name),
        sanitize_name(target_interface)
    );
    std::fs::write(&out_path, &bytes)
        .with_context(|| format!("Failed to write adapter component to '{}'", out_path))?;

    Ok(out_path)
}

fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

// ─── Type helpers ────────────────────────────────────────────────────────────

/// Returns the canonical ABI flat core types for a component value type.
fn flat_types_for(id: ValueTypeId, arena: &TypeArena) -> Vec<ValType> {
    match arena.lookup_val(id) {
        ValueType::Bool
        | ValueType::U8
        | ValueType::S8
        | ValueType::U16
        | ValueType::S16
        | ValueType::U32
        | ValueType::S32
        | ValueType::Char
        | ValueType::ErrorContext => vec![ValType::I32],
        ValueType::S64 | ValueType::U64 => vec![ValType::I64],
        ValueType::F32 => vec![ValType::F32],
        ValueType::F64 => vec![ValType::F64],
        ValueType::String => vec![ValType::I32, ValType::I32],
        ValueType::Resource(_) | ValueType::AsyncHandle => vec![ValType::I32],
        ValueType::Enum(_) => vec![ValType::I32],
        ValueType::Flags(names) => {
            let n_words = (names.len() + 31) / 32;
            vec![ValType::I32; n_words]
        }
        ValueType::Record(fields) => fields
            .iter()
            .flat_map(|(_, id)| flat_types_for(*id, arena))
            .collect(),
        ValueType::Tuple(ids) => ids
            .iter()
            .flat_map(|id| flat_types_for(*id, arena))
            .collect(),
        ValueType::List(_) | ValueType::FixedSizeList(..) | ValueType::Map(..) => {
            vec![ValType::I32, ValType::I32]
        }
        ValueType::Option(inner) => {
            let inner_flat = flat_types_for(*inner, arena);
            let mut result = vec![ValType::I32]; // discriminant
            result.extend(join_flat_lists(&[inner_flat]));
            result
        }
        ValueType::Result { ok, err } => {
            let ok_flat = ok.map(|id| flat_types_for(id, arena)).unwrap_or_default();
            let err_flat = err.map(|id| flat_types_for(id, arena)).unwrap_or_default();
            let mut result = vec![ValType::I32]; // discriminant
            result.extend(join_flat_lists(&[ok_flat, err_flat]));
            result
        }
        ValueType::Variant(cases) => {
            let case_flats: Vec<Vec<ValType>> = cases
                .iter()
                .filter_map(|(_, opt_id)| opt_id.map(|id| flat_types_for(id, arena)))
                .collect();
            let mut result = vec![ValType::I32]; // discriminant
            result.extend(join_flat_lists(&case_flats));
            result
        }
    }
}

/// Element-wise join of multiple flat type vectors (canonical ABI widening rule).
fn join_flat_lists(lists: &[Vec<ValType>]) -> Vec<ValType> {
    let max_len = lists.iter().map(|l| l.len()).max().unwrap_or(0);
    let mut result = Vec::with_capacity(max_len);
    for i in 0..max_len {
        let mut joined = ValType::I32;
        for list in lists {
            let vt = list.get(i).copied().unwrap_or(ValType::I32);
            joined = join_val_type(joined, vt);
        }
        result.push(joined);
    }
    result
}

fn join_val_type(a: ValType, b: ValType) -> ValType {
    match (a, b) {
        (ValType::I32, ValType::I32) => ValType::I32,
        (ValType::F32, ValType::F32) => ValType::F32,
        (ValType::F64, ValType::F64) => ValType::F64,
        (ValType::I64, _) | (_, ValType::I64) => ValType::I64,
        (ValType::F64, _) | (_, ValType::F64) => ValType::F64,
        _ => ValType::I32,
    }
}

/// Returns true if the type (or any type it transitively contains) is a string.
fn type_has_strings(id: ValueTypeId, arena: &TypeArena) -> bool {
    match arena.lookup_val(id) {
        ValueType::String => true,
        ValueType::Record(fields) => fields
            .iter()
            .any(|(_, id)| type_has_strings(*id, arena)),
        ValueType::Tuple(ids) => ids.iter().any(|id| type_has_strings(*id, arena)),
        ValueType::Variant(cases) => cases
            .iter()
            .any(|(_, opt_id)| opt_id.map(|id| type_has_strings(id, arena)).unwrap_or(false)),
        ValueType::Option(inner) => type_has_strings(*inner, arena),
        ValueType::Result { ok, err } => {
            ok.map(|id| type_has_strings(id, arena)).unwrap_or(false)
                || err.map(|id| type_has_strings(id, arena)).unwrap_or(false)
        }
        ValueType::List(inner) | ValueType::FixedSizeList(inner, _) => {
            type_has_strings(*inner, arena)
        }
        _ => false,
    }
}

/// Returns true if the type (or any type it transitively contains) is a resource.
fn type_has_resources(id: ValueTypeId, arena: &TypeArena) -> bool {
    match arena.lookup_val(id) {
        ValueType::Resource(_) | ValueType::AsyncHandle => true,
        ValueType::Record(fields) => fields
            .iter()
            .any(|(_, id)| type_has_resources(*id, arena)),
        ValueType::Tuple(ids) => ids.iter().any(|id| type_has_resources(*id, arena)),
        ValueType::Variant(cases) => cases
            .iter()
            .any(|(_, opt_id)| opt_id.map(|id| type_has_resources(id, arena)).unwrap_or(false)),
        ValueType::Option(inner) => type_has_resources(*inner, arena),
        ValueType::Result { ok, err } => {
            ok.map(|id| type_has_resources(id, arena)).unwrap_or(false)
                || err.map(|id| type_has_resources(id, arena)).unwrap_or(false)
        }
        ValueType::List(inner) | ValueType::FixedSizeList(inner, _) => {
            type_has_resources(*inner, arena)
        }
        _ => false,
    }
}

/// Returns the canonical ABI alignment (in bytes) for a value type.
fn canonical_align(id: ValueTypeId, arena: &TypeArena) -> u32 {
    match arena.lookup_val(id) {
        ValueType::Bool | ValueType::U8 | ValueType::S8 => 1,
        ValueType::U16 | ValueType::S16 => 2,
        ValueType::U32
        | ValueType::S32
        | ValueType::F32
        | ValueType::Char
        | ValueType::String
        | ValueType::Resource(_)
        | ValueType::AsyncHandle => 4,
        ValueType::U64 | ValueType::S64 | ValueType::F64 => 8,
        ValueType::List(_) | ValueType::FixedSizeList(..) | ValueType::Map(..) => 4,
        ValueType::Record(fields) => fields
            .iter()
            .map(|(_, id)| canonical_align(*id, arena))
            .max()
            .unwrap_or(1),
        ValueType::Tuple(ids) => ids
            .iter()
            .map(|id| canonical_align(*id, arena))
            .max()
            .unwrap_or(1),
        ValueType::Variant(cases) => {
            let payload_align = cases
                .iter()
                .filter_map(|(_, opt_id)| opt_id.map(|id| canonical_align(id, arena)))
                .max()
                .unwrap_or(1);
            std::cmp::max(disc_align(cases.len()), payload_align)
        }
        ValueType::Option(inner) => std::cmp::max(1, canonical_align(*inner, arena)),
        ValueType::Result { ok, err } => {
            let ok_a = ok.map(|id| canonical_align(id, arena)).unwrap_or(1);
            let err_a = err.map(|id| canonical_align(id, arena)).unwrap_or(1);
            std::cmp::max(1, std::cmp::max(ok_a, err_a))
        }
        ValueType::Enum(tags) => disc_align(tags.len()),
        ValueType::Flags(names) => {
            if names.len() > 16 {
                4
            } else if names.len() > 8 {
                2
            } else {
                1
            }
        }
        ValueType::ErrorContext => 4,
    }
}

/// Discriminant alignment for a variant/enum with `n` cases.
fn disc_align(n: usize) -> u32 {
    if n <= 256 {
        1
    } else if n <= 65536 {
        2
    } else {
        4
    }
}

/// Round `offset` up to the nearest multiple of `align`.
fn align_to_val(offset: u32, align: u32) -> u32 {
    (offset + align - 1) & !(align - 1)
}

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

/// Collect all unique resource ValueTypeIds used (transitively) in function signatures.
fn collect_resource_ids(funcs: &[AdapterFunc], arena: &TypeArena) -> Vec<(ValueTypeId, String)> {
    let mut seen = HashMap::new();
    for func in funcs {
        for &id in &func.param_type_ids {
            collect_resources_rec(id, arena, &mut seen);
        }
        if let Some(id) = func.result_type_id {
            collect_resources_rec(id, arena, &mut seen);
        }
    }
    let mut out: Vec<(ValueTypeId, String)> = seen.into_iter().collect();
    out.sort_by(|a, b| a.1.cmp(&b.1)); // deterministic order
    out
}

fn collect_resources_rec(
    id: ValueTypeId,
    arena: &TypeArena,
    seen: &mut HashMap<ValueTypeId, String>,
) {
    match arena.lookup_val(id) {
        ValueType::Resource(name) => {
            if !seen.contains_key(&id) {
                let export_name = if name.is_empty() {
                    format!("res-{}", seen.len())
                } else {
                    name.clone()
                };
                seen.insert(id, export_name);
            }
        }
        ValueType::AsyncHandle => {
            if !seen.contains_key(&id) {
                let export_name = format!("res-{}", seen.len());
                seen.insert(id, export_name);
            }
        }
        ValueType::Record(fields) => {
            for (_, fid) in fields {
                collect_resources_rec(*fid, arena, seen);
            }
        }
        ValueType::Tuple(ids) => {
            for fid in ids {
                collect_resources_rec(*fid, arena, seen);
            }
        }
        ValueType::Variant(cases) => {
            for (_, opt_id) in cases {
                if let Some(fid) = opt_id {
                    collect_resources_rec(*fid, arena, seen);
                }
            }
        }
        ValueType::Option(inner) => collect_resources_rec(*inner, arena, seen),
        ValueType::Result { ok, err } => {
            if let Some(oid) = ok {
                collect_resources_rec(*oid, arena, seen);
            }
            if let Some(eid) = err {
                collect_resources_rec(*eid, arena, seen);
            }
        }
        ValueType::List(inner) | ValueType::FixedSizeList(inner, _) => {
            collect_resources_rec(*inner, arena, seen);
        }
        _ => {}
    }
}

/// Emit Wasm instructions that push the flat values for a multi-value task.return
/// by reading from the canonical ABI memory layout at `result_ptr`.
///
/// For `result<T, E>`: reads discriminant (u8 at offset 0) and first payload (at
/// computed payload offset), then pushes zeros for remaining flat values.
/// This handles the Ok case correctly; Err cases without payloads also work.
fn emit_task_return_loads(
    f: &mut Function,
    result_ptr: u32,
    result_type_id: ValueTypeId,
    core_results: &[ValType],
    arena: &TypeArena,
) {
    let vt = arena.lookup_val(result_type_id).clone();
    match vt {
        ValueType::Result { ok, err } => {
            // flat[0]: result discriminant (u8 at offset 0 → i32)
            f.instruction(&Instruction::I32Const(result_ptr as i32));
            f.instruction(&Instruction::I32Load8U(wasm_encoder::MemArg {
                offset: 0,
                align: 0,
                memory_index: 0,
            }));

            // Compute payload offset: align_to(1, max(ok_align, err_align))
            let ok_a = ok.map(|id| canonical_align(id, arena)).unwrap_or(1);
            let err_a = err.map(|id| canonical_align(id, arena)).unwrap_or(1);
            let payload_offset = align_to_val(1, std::cmp::max(ok_a, err_a));

            // flat[1]: first payload value
            if core_results.len() > 1 {
                f.instruction(&Instruction::I32Const(result_ptr as i32));
                match core_results[1] {
                    ValType::I64 => {
                        f.instruction(&Instruction::I64Load(wasm_encoder::MemArg {
                            offset: payload_offset as u64,
                            align: 3,
                            memory_index: 0,
                        }));
                    }
                    ValType::F32 => {
                        f.instruction(&Instruction::F32Load(wasm_encoder::MemArg {
                            offset: payload_offset as u64,
                            align: 2,
                            memory_index: 0,
                        }));
                    }
                    ValType::F64 => {
                        f.instruction(&Instruction::F64Load(wasm_encoder::MemArg {
                            offset: payload_offset as u64,
                            align: 3,
                            memory_index: 0,
                        }));
                    }
                    _ => {
                        f.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
                            offset: payload_offset as u64,
                            align: 2,
                            memory_index: 0,
                        }));
                    }
                }
            }

            // flat[2..]: zero remaining values (handles Ok and simple Err cases)
            for vt in core_results.iter().skip(2) {
                match vt {
                    ValType::I64 => {
                        f.instruction(&Instruction::I64Const(0));
                    }
                    ValType::F32 => {
                        f.instruction(&Instruction::F32Const(0.0f32.into()));
                    }
                    ValType::F64 => {
                        f.instruction(&Instruction::F64Const(0.0f64.into()));
                    }
                    _ => {
                        f.instruction(&Instruction::I32Const(0));
                    }
                }
            }
        }
        _ => {
            // For non-result types with multi-value: just push zeros.
            // This is a fallback; extend as needed for other compound types.
            for vt in core_results.iter() {
                match vt {
                    ValType::I64 => {
                        f.instruction(&Instruction::I64Const(0));
                    }
                    ValType::F32 => {
                        f.instruction(&Instruction::F32Const(0.0f32.into()));
                    }
                    ValType::F64 => {
                        f.instruction(&Instruction::F64Const(0.0f64.into()));
                    }
                    _ => {
                        f.instruction(&Instruction::I32Const(0));
                    }
                }
            }
        }
    }
}

// ─── Types-instance encoder (export-aware) ─────────────────────────────────
//
// In an import instance type, compound types (records, variants) that are
// referenced by other types MUST be exported via `(type (eq N))` and later
// types must use the EXPORT type index.  This encoder handles that by
// exporting named types immediately after defining them.

/// Build the types instance type with export-aware indexing.
///
/// `type_exports` maps export names (e.g. "request", "error-code") to ValueTypeIds.
/// Resources are exported as SubResource; compound types are defined, exported (eq),
/// and subsequent references use the export index.
fn build_types_instance_type(
    type_exports: &BTreeMap<String, ValueTypeId>,
    arena: &TypeArena,
) -> InstanceType {
    let mut inst = InstanceType::new();
    let mut cache: HashMap<ValueTypeId, ComponentValType> = HashMap::new();
    // Reverse map: vid → export name
    let name_map: HashMap<ValueTypeId, &str> = type_exports
        .iter()
        .map(|(name, &vid)| (vid, name.as_str()))
        .collect();

    // Encode each type export (dependencies are encoded recursively).
    for (_name, &vid) in type_exports {
        encode_types_inst_cv(vid, &mut inst, arena, &mut cache, &name_map);
    }

    inst
}

/// Recursively encode a value type into the types instance type.
///
/// Returns the `ComponentValType` to use when referencing this type.
/// Named types (those in `name_map`) are exported and the returned CVT
/// uses the export's type index; anonymous types use the raw definition index.
fn encode_types_inst_cv(
    id: ValueTypeId,
    inst: &mut InstanceType,
    arena: &TypeArena,
    cache: &mut HashMap<ValueTypeId, ComponentValType>,
    name_map: &HashMap<ValueTypeId, &str>,
) -> ComponentValType {
    // Already encoded?
    if let Some(&cv) = cache.get(&id) {
        return cv;
    }

    let vt = arena.lookup_val(id).clone();

    // Primitives — no local type needed.
    if let Some(cv) = prim_cv(&vt) {
        return cv;
    }

    // Resources → SubResource export.
    if matches!(vt, ValueType::Resource(_) | ValueType::AsyncHandle) {
        let name = name_map.get(&id).copied().unwrap_or("resource");
        let idx = inst.type_count();
        inst.export(name, ComponentTypeRef::Type(TypeBounds::SubResource));
        let cv = ComponentValType::Type(idx);
        cache.insert(id, cv);
        return cv;
    }

    // Encode the type definition (recursively encoding dependencies first).
    let raw_idx = match vt {
        ValueType::Record(ref fields) => {
            let encoded: Vec<(String, ComponentValType)> = fields
                .iter()
                .map(|(name, fid)| {
                    let cv = encode_types_inst_cv(*fid, inst, arena, cache, name_map);
                    (name.clone(), cv)
                })
                .collect();
            let idx = inst.type_count();
            inst.ty()
                .defined_type()
                .record(encoded.iter().map(|(n, cv)| (n.as_str(), *cv)));
            idx
        }
        ValueType::Variant(ref cases) => {
            let encoded: Vec<(String, Option<ComponentValType>)> = cases
                .iter()
                .map(|(name, opt_id)| {
                    let opt_cv =
                        opt_id.map(|fid| encode_types_inst_cv(fid, inst, arena, cache, name_map));
                    (name.clone(), opt_cv)
                })
                .collect();
            let idx = inst.type_count();
            inst.ty()
                .defined_type()
                .variant(encoded.iter().map(|(n, cv)| (n.as_str(), *cv)));
            idx
        }
        ValueType::Option(inner) => {
            let inner_cv = encode_types_inst_cv(inner, inst, arena, cache, name_map);
            let idx = inst.type_count();
            inst.ty().defined_type().option(inner_cv);
            idx
        }
        ValueType::Result { ok, err } => {
            let ok_cv = ok.map(|fid| encode_types_inst_cv(fid, inst, arena, cache, name_map));
            let err_cv = err.map(|fid| encode_types_inst_cv(fid, inst, arena, cache, name_map));
            let idx = inst.type_count();
            inst.ty().defined_type().result(ok_cv, err_cv);
            idx
        }
        ValueType::Tuple(ref ids) => {
            let encoded: Vec<ComponentValType> = ids
                .iter()
                .map(|fid| encode_types_inst_cv(*fid, inst, arena, cache, name_map))
                .collect();
            let idx = inst.type_count();
            inst.ty().defined_type().tuple(encoded.into_iter());
            idx
        }
        ValueType::List(inner) => {
            let inner_cv = encode_types_inst_cv(inner, inst, arena, cache, name_map);
            let idx = inst.type_count();
            inst.ty().defined_type().list(inner_cv);
            idx
        }
        ValueType::Enum(ref tags) => {
            let idx = inst.type_count();
            inst.ty()
                .defined_type()
                .enum_type(tags.iter().map(|s| s.as_str()));
            idx
        }
        ValueType::Flags(ref names) => {
            let idx = inst.type_count();
            inst.ty()
                .defined_type()
                .flags(names.iter().map(|s| s.as_str()));
            idx
        }
        _ => {
            // Fallback for unsupported types — should not happen for well-formed interfaces.
            let idx = inst.type_count();
            inst.ty()
                .defined_type()
                .record(std::iter::empty::<(&str, ComponentValType)>());
            idx
        }
    };

    // Records and variants MUST be exported in import instance types.
    // Use the name from name_map if available, otherwise a synthetic name.
    let needs_export = name_map.contains_key(&id)
        || matches!(vt, ValueType::Record(_) | ValueType::Variant(_));

    if needs_export {
        let name = name_map
            .get(&id)
            .copied()
            .unwrap_or_else(|| {
                // Leak a synthetic name — this is fine since adapter generation is short-lived.
                Box::leak(format!("type-{}", raw_idx).into_boxed_str())
            });
        let export_idx = inst.type_count();
        inst.export(name, ComponentTypeRef::Type(TypeBounds::Eq(raw_idx)));
        let cv = ComponentValType::Type(export_idx);
        cache.insert(id, cv);
        cv
    } else {
        let cv = ComponentValType::Type(raw_idx);
        cache.insert(id, cv);
        cv
    }
}

/// Returns the ComponentValType for a primitive value type, or None if it's a compound type.
fn prim_cv(vt: &ValueType) -> Option<ComponentValType> {
    let pv = match vt {
        ValueType::Bool => PrimitiveValType::Bool,
        ValueType::U8 => PrimitiveValType::U8,
        ValueType::S8 => PrimitiveValType::S8,
        ValueType::U16 => PrimitiveValType::U16,
        ValueType::S16 => PrimitiveValType::S16,
        ValueType::U32 => PrimitiveValType::U32,
        ValueType::S32 => PrimitiveValType::S32,
        ValueType::U64 => PrimitiveValType::U64,
        ValueType::S64 => PrimitiveValType::S64,
        ValueType::F32 => PrimitiveValType::F32,
        ValueType::F64 => PrimitiveValType::F64,
        ValueType::Char => PrimitiveValType::Char,
        ValueType::String => PrimitiveValType::String,
        _ => return None,
    };
    Some(ComponentValType::Primitive(pv))
}

// ─── Type extraction ────────────────────────────────────────────────────────

fn extract_adapter_funcs(
    iface_ty: &InterfaceType,
    arena: &TypeArena,
) -> anyhow::Result<Vec<AdapterFunc>> {
    let inst = match iface_ty {
        InterfaceType::Instance(i) => i,
        InterfaceType::Func(_) => anyhow::bail!(
            "Expected an instance-type interface for tier-1 adapter generation; \
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
        let mut param_type_ids = Vec::new();
        let mut core_params = Vec::new();
        for (i, &id) in sig.params.iter().enumerate() {
            let pname = if i < sig.param_names.len() {
                sig.param_names[i].clone()
            } else {
                format!("p{i}")
            };
            param_names.push(pname);
            param_type_ids.push(id);
            core_params.extend(flat_types_for(id, arena));
        }

        if sig.results.len() > 1 {
            anyhow::bail!(
                "Function '{}' has {} results; only 0 or 1 results are supported \
                 for tier-1 adapter generation in this version.",
                name,
                sig.results.len()
            );
        }

        let (result_type_id, result_is_complex, core_results) = if sig.results.is_empty() {
            (None, false, vec![])
        } else {
            let rid = sig.results[0];
            let flat = flat_types_for(rid, arena);
            let is_complex = flat.len() > 1;
            // Sync multi-value results would need retptr handling — not yet implemented.
            if !sig.is_async && is_complex {
                anyhow::bail!(
                    "Function '{}' has a multi-value result ({} flat values) which is not \
                     yet supported for sync tier-1 adapter generation.",
                    name,
                    flat.len()
                );
            }
            // Store full flat types; task.return uses these as params (up to MAX_FLAT_PARAMS=16).
            (Some(rid), is_complex, flat)
        };

        // For async functions with a result, reserve memory.
        let (async_result_mem_offset, async_result_mem_size) =
            if sig.is_async && result_type_id.is_some() {
                let size = if result_is_complex { 512u32 } else { 8u32 };
                let off = async_result_cursor;
                async_result_cursor += size;
                (Some(off), size)
            } else {
                (None, 0)
            };

        let name_len = name.len() as u32;
        funcs.push(AdapterFunc {
            name: name.clone(),
            is_async: sig.is_async,
            param_names,
            param_type_ids,
            result_type_id,
            result_is_complex,
            core_params,
            core_results,
            name_offset,
            name_len,
            async_result_mem_offset,
            async_result_mem_size,
        });
        name_offset += name_len;
    }
    Ok(funcs)
}

// ─── InstTypeCtx: recursive InstanceType encoder ────────────────────────────

/// Encodes component types into an InstanceType recursively.
///
/// Tracks:
/// - `cache`: ValueTypeId → local type index within the InstanceType
/// - `resource_exports`: (vid, export_name, resource_local_idx, own_local_idx)
/// - `outer_resources`: When non-empty, resources are resolved via `alias outer`
///   instead of being exported as SubResource.  Maps ValueTypeId → component-scope type index.
struct InstTypeCtx {
    cache: HashMap<ValueTypeId, u32>,
    resource_exports: Vec<(ValueTypeId, String, u32, u32)>,
    /// Maps resource ValueTypeId → component-scope type index.
    /// When populated, resources use `alias outer 1 <comp_idx>` + inline own<T>
    /// instead of SubResource exports.
    outer_resources: HashMap<ValueTypeId, u32>,
    /// Maps resource ValueTypeId → local alias index within the instance type.
    /// Populated by the caller after emitting `alias outer` declarations.
    alias_locals: HashMap<ValueTypeId, u32>,
}

impl InstTypeCtx {
    fn new() -> Self {
        Self {
            cache: HashMap::new(),
            resource_exports: Vec::new(),
            outer_resources: HashMap::new(),
            alias_locals: HashMap::new(),
        }
    }

    fn with_outer_resources(outer: HashMap<ValueTypeId, u32>) -> Self {
        Self {
            cache: HashMap::new(),
            resource_exports: Vec::new(),
            outer_resources: outer,
            alias_locals: HashMap::new(),
        }
    }

    /// Encodes a value type into the InstanceType, returning its `ComponentValType`.
    ///
    /// - Primitives: returns `ComponentValType::Primitive(...)` (no allocation).
    /// - Resources: declares sub-resource export + `own<>`, returns `Type(own_local_idx)`.
    ///   Named resources (`ValueType::Resource("request")`) use their interface name as the
    ///   export name; unnamed resources use synthetic `"res-N"` names.  Each distinct
    ///   `ValueTypeId` is encoded only once (cached).
    /// - Compounds (result, variant, option, record, etc.): recursively encodes,
    ///   returns `Type(compound_local_idx)`.
    fn encode_cv(
        &mut self,
        id: ValueTypeId,
        inst: &mut InstanceType,
        arena: &TypeArena,
    ) -> ComponentValType {
        // Primitives — no local type needed.
        if let Some(cv) = prim_cv(arena.lookup_val(id)) {
            return cv;
        }

        // Already encoded?
        if let Some(&local_idx) = self.cache.get(&id) {
            return ComponentValType::Type(local_idx);
        }

        // Clone to avoid borrow conflicts during recursion.
        let vt = arena.lookup_val(id).clone();

        let local_idx = match vt {
            ValueType::Resource(ref name) => {
                if self.outer_resources.contains_key(&id) {
                    // Resource comes from parent scope via alias outer.
                    // Look up the local alias index (set by caller after emitting alias outer).
                    let alias_local = self
                        .alias_locals
                        .get(&id)
                        .copied()
                        .unwrap_or_else(|| {
                            panic!(
                                "outer_resources entry for {:?} but no alias_locals entry; \
                                 alias outer should have been emitted before encode_cv",
                                id
                            );
                        });
                    // Create own<T> referencing the alias'd resource local index.
                    let own_local = inst.type_count();
                    inst.ty().defined_type().own(alias_local);
                    let export_name = if name.is_empty() {
                        format!("res-{}", self.resource_exports.len())
                    } else {
                        name.clone()
                    };
                    self.resource_exports
                        .push((id, export_name, alias_local, own_local));
                    own_local
                } else {
                    // Original SubResource export pattern.
                    let export_name = if name.is_empty() {
                        format!("res-{}", self.resource_exports.len())
                    } else {
                        name.clone()
                    };
                    let res_local = inst.type_count();
                    inst.export(&export_name, ComponentTypeRef::Type(TypeBounds::SubResource));
                    let own_local = inst.type_count();
                    inst.ty().defined_type().own(res_local);
                    self.resource_exports
                        .push((id, export_name, res_local, own_local));
                    own_local
                }
            }
            ValueType::AsyncHandle => {
                if self.outer_resources.contains_key(&id) {
                    let alias_local = self.alias_locals.get(&id).copied().unwrap_or_else(|| {
                        panic!("outer_resources entry for AsyncHandle but no alias_locals entry");
                    });
                    let own_local = inst.type_count();
                    inst.ty().defined_type().own(alias_local);
                    let export_name = format!("res-{}", self.resource_exports.len());
                    self.resource_exports
                        .push((id, export_name, alias_local, own_local));
                    own_local
                } else {
                    let export_name = format!("res-{}", self.resource_exports.len());
                    let res_local = inst.type_count();
                    inst.export(&export_name, ComponentTypeRef::Type(TypeBounds::SubResource));
                    let own_local = inst.type_count();
                    inst.ty().defined_type().own(res_local);
                    self.resource_exports
                        .push((id, export_name, res_local, own_local));
                    own_local
                }
            }

            ValueType::Option(inner_id) => {
                let inner_cv = self.encode_cv(inner_id, inst, arena);
                let idx = inst.type_count();
                inst.ty().defined_type().option(inner_cv);
                idx
            }

            ValueType::Result { ok, err } => {
                let ok_cv = ok.map(|id| self.encode_cv(id, inst, arena));
                let err_cv = err.map(|id| self.encode_cv(id, inst, arena));
                let idx = inst.type_count();
                inst.ty().defined_type().result(ok_cv, err_cv);
                idx
            }

            ValueType::Variant(cases) => {
                let encoded: Vec<(String, Option<ComponentValType>)> = cases
                    .iter()
                    .map(|(name, opt_id)| {
                        let opt_cv = opt_id.map(|id| self.encode_cv(id, inst, arena));
                        (name.clone(), opt_cv)
                    })
                    .collect();
                let idx = inst.type_count();
                inst.ty()
                    .defined_type()
                    .variant(encoded.iter().map(|(n, cv)| (n.as_str(), *cv)));
                idx
            }

            ValueType::Record(fields) => {
                let encoded: Vec<(String, ComponentValType)> = fields
                    .iter()
                    .map(|(name, id)| (name.clone(), self.encode_cv(*id, inst, arena)))
                    .collect();
                let idx = inst.type_count();
                inst.ty()
                    .defined_type()
                    .record(encoded.iter().map(|(n, cv)| (n.as_str(), *cv)));
                idx
            }

            ValueType::Tuple(ids) => {
                let encoded: Vec<ComponentValType> = ids
                    .iter()
                    .map(|id| self.encode_cv(*id, inst, arena))
                    .collect();
                let idx = inst.type_count();
                inst.ty().defined_type().tuple(encoded.into_iter());
                idx
            }

            ValueType::List(inner_id) => {
                let inner_cv = self.encode_cv(inner_id, inst, arena);
                let idx = inst.type_count();
                inst.ty().defined_type().list(inner_cv);
                idx
            }

            ValueType::FixedSizeList(inner_id, _n) => {
                // Treat fixed-size lists as regular lists (conservative).
                let inner_cv = self.encode_cv(inner_id, inst, arena);
                let idx = inst.type_count();
                inst.ty().defined_type().list(inner_cv);
                idx
            }

            ValueType::Enum(tags) => {
                let idx = inst.type_count();
                inst.ty()
                    .defined_type()
                    .enum_type(tags.iter().map(|s| s.as_str()));
                idx
            }

            ValueType::Flags(names) => {
                let idx = inst.type_count();
                inst.ty()
                    .defined_type()
                    .flags(names.iter().map(|s| s.as_str()));
                idx
            }

            // TODO: Map, ErrorContext — not yet supported as component-level types.
            other => panic!(
                "InstTypeCtx::encode_cv: unsupported type {:?}",
                other
            ),
        };

        self.cache.insert(id, local_idx);
        ComponentValType::Type(local_idx)
    }
}

// ─── Component-level type helpers ───────────────────────────────────────────

/// Encodes a value type at the component level, adding any needed compound type definitions
/// to `comp_types` and returning the `ComponentValType` that references them.
///
/// - Primitives → `ComponentValType::Primitive(...)` (no allocation)
/// - Resources / AsyncHandle → looks up the component-level `own<T>` index from `comp_own_by_vid`
/// - Compound types (Result, Variant, Option, Record, etc.) → recursively encodes inner types,
///   adds a new defined-type entry to `comp_types`, returns `ComponentValType::Type(idx)`
///
/// `comp_type_count` tracks the running component-level type index (incremented for each new entry).
/// `comp_cache` prevents redundant encoding of the same `ValueTypeId`.
#[allow(clippy::too_many_arguments)]
fn encode_comp_cv(
    id: ValueTypeId,
    arena: &TypeArena,
    comp_types: &mut ComponentTypeSection,
    comp_type_count: &mut u32,
    comp_own_by_vid: &HashMap<ValueTypeId, u32>,
    comp_cache: &mut HashMap<ValueTypeId, u32>,
) -> ComponentValType {
    // Primitives don't need a type declaration.
    if let Some(cv) = prim_cv(arena.lookup_val(id)) {
        return cv;
    }
    // Already encoded?
    if let Some(&idx) = comp_cache.get(&id) {
        return ComponentValType::Type(idx);
    }

    let vt = arena.lookup_val(id).clone();

    match vt {
        ValueType::Resource(_) | ValueType::AsyncHandle => {
            // Use the pre-built component-level own<T> index.
            if let Some(&own_idx) = comp_own_by_vid.get(&id) {
                ComponentValType::Type(own_idx)
            } else {
                // Fallback for anonymous/unmatched resources.
                ComponentValType::Primitive(PrimitiveValType::U32)
            }
        }
        ValueType::Result { ok, err } => {
            let ok_cv = ok.map(|id| encode_comp_cv(id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache));
            let err_cv = err.map(|id| encode_comp_cv(id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache));
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().result(ok_cv, err_cv);
            comp_cache.insert(id, idx);
            ComponentValType::Type(idx)
        }
        ValueType::Option(inner_id) => {
            let inner_cv = encode_comp_cv(inner_id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache);
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().option(inner_cv);
            comp_cache.insert(id, idx);
            ComponentValType::Type(idx)
        }
        ValueType::Variant(cases) => {
            let encoded: Vec<(String, Option<ComponentValType>)> = cases
                .iter()
                .map(|(name, opt_id)| {
                    let opt_cv = opt_id.map(|id| encode_comp_cv(id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache));
                    (name.clone(), opt_cv)
                })
                .collect();
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().variant(encoded.iter().map(|(n, cv)| (n.as_str(), *cv)));
            comp_cache.insert(id, idx);
            ComponentValType::Type(idx)
        }
        ValueType::Record(fields) => {
            let encoded: Vec<(String, ComponentValType)> = fields
                .iter()
                .map(|(name, id)| (name.clone(), encode_comp_cv(*id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache)))
                .collect();
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().record(encoded.iter().map(|(n, cv)| (n.as_str(), *cv)));
            comp_cache.insert(id, idx);
            ComponentValType::Type(idx)
        }
        ValueType::Tuple(ids) => {
            let encoded: Vec<ComponentValType> = ids
                .iter()
                .map(|id| encode_comp_cv(*id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache))
                .collect();
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().tuple(encoded.into_iter());
            comp_cache.insert(id, idx);
            ComponentValType::Type(idx)
        }
        ValueType::List(inner_id) => {
            let inner_cv = encode_comp_cv(inner_id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache);
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().list(inner_cv);
            comp_cache.insert(id, idx);
            ComponentValType::Type(idx)
        }
        ValueType::FixedSizeList(inner_id, _n) => {
            // Treat fixed-size lists as regular lists (conservative).
            let inner_cv = encode_comp_cv(inner_id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache);
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().list(inner_cv);
            comp_cache.insert(id, idx);
            ComponentValType::Type(idx)
        }
        ValueType::Enum(tags) => {
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().enum_type(tags.iter().map(|s| s.as_str()));
            comp_cache.insert(id, idx);
            ComponentValType::Type(idx)
        }
        ValueType::Flags(names) => {
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().flags(names.iter().map(|s| s.as_str()));
            comp_cache.insert(id, idx);
            ComponentValType::Type(idx)
        }
        // Primitives already handled above; ErrorContext/Map not supported as component-level types.
        other => panic!("encode_comp_cv: unsupported type {:?}", other),
    }
}

// ─── Component binary generation ────────────────────────────────────────────

/// Build the full Wasm component binary for the tier-1 adapter.
///
/// Uses `wasm_encoder::Component` with explicit section management so that
/// we can create a component instance from exported items (a capability not
/// exposed as a public method by `ComponentBuilder`).
fn build_adapter_bytes(
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

    let downstream_inst_ty: u32;
    let before_inst_ty: Option<u32>;
    let after_inst_ty: Option<u32>;
    let blocking_inst_ty: Option<u32>;
    let inst_ctx: InstTypeCtx;

    let downstream_inst: u32;
    let before_inst: Option<u32>;
    let after_inst: Option<u32>;
    let blocking_inst: Option<u32>;

    let downstream_func_base: u32;
    let before_comp_func: Option<u32>;
    let after_comp_func: Option<u32>;
    let blocking_comp_func: Option<u32>;
    let comp_resource_indices: Vec<u32>;
    // Maps vid → comp scope type index for ALL aliased types (resources + compounds).
    let mut comp_aliased_types: HashMap<ValueTypeId, u32> = HashMap::new();

    // Helper closure: emit hook func types into a ComponentTypeSection.
    // Returns after adding types 0 (void) and 1 (bool).
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

    // Helper: build the downstream instance type, encode function types & exports.
    // Caller may pre-populate `inst` with alias outer declarations for resources.
    fn build_downstream_inst_type(
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

    // Helper: emit hook instance types (before/after/blocking).
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

    // Helper: emit hook imports.
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

    // Helper: alias funcs from downstream + hook instances.
    fn emit_func_aliases(
        aliases: &mut ComponentAliasSection,
        func_count: &mut u32,
        funcs: &[AdapterFunc],
        downstream_inst: u32,
        before_inst: Option<u32>,
        after_inst: Option<u32>,
        blocking_inst: Option<u32>,
    ) -> (u32, Option<u32>, Option<u32>, Option<u32>) {
        let downstream_func_base = *func_count;
        for func in funcs {
            aliases.alias(Alias::InstanceExport {
                instance: downstream_inst,
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
            downstream_func_base,
            before_comp_func,
            after_comp_func,
            blocking_comp_func,
        )
    }

    // Path A requires named type exports (from wirm) so we can build the types
    // instance with proper export names.  When type_exports is empty (e.g. the
    // composed component's handler interface didn't resolve resource names),
    // fall back to path B which uses SubResource exports from the downstream.
    let has_type_exports = match iface_ty {
        InterfaceType::Instance(inst) => !inst.type_exports.is_empty(),
        _ => false,
    };

    if let Some(split) = split_imports {
        // ── Path (S): pass-through split imports ─────────────────────────────
        //
        // Copy the downstream split's type/import/alias sections verbatim.
        // This gives the adapter the same type definitions and pass-through
        // imports (types instance, WASI interfaces) as the downstream.
        //
        // If the split imports the handler, we use that import directly.
        // If the split exports the handler (it IS the downstream service),
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
            downstream_inst = handler_idx;

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
                    &mut aliases, &mut func_count, funcs, downstream_inst,
                    before_inst, after_inst, blocking_inst,
                );
                downstream_func_base = dfb;
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
                build_downstream_inst_type(&mut ctx, &mut dummy_inst, funcs, arena);
                inst_ctx = ctx;
            }

            // Alias resource types from the handler instance
            {
                let mut aliases = ComponentAliasSection::new();
                let mut res_vec: Vec<u32> = Vec::new();
                for (_vid, export_name, _res_local, _own_local) in &inst_ctx.resource_exports {
                    let comp_res_idx = type_count;
                    type_count += 1;
                    aliases.alias(Alias::InstanceExport {
                        instance: downstream_inst,
                        kind: ComponentExportKind::Type,
                        name: export_name,
                    });
                    res_vec.push(comp_res_idx);
                }
                comp_resource_indices = res_vec;
                if !inst_ctx.resource_exports.is_empty() {
                    component.section(&aliases);
                }
            }
            downstream_inst_ty = 0; // handler type came from raw sections
        } else {
            // The split exports the handler rather than importing it (this is the
            // downstream service itself). Build the handler import type from the
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
                build_downstream_inst_type(&mut ctx, &mut inst, funcs, arena);
                downstream_inst_ty = type_count;
                type_count += 1;
                inst_ctx = ctx;
                types.instance(&inst);
            } else {
                let mut ctx = InstTypeCtx::new();
                let mut inst = InstanceType::new();
                build_downstream_inst_type(&mut ctx, &mut inst, funcs, arena);
                downstream_inst_ty = type_count;
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
                downstream_inst = instance_count;
                instance_count += 1;
                imports.import(target_interface, ComponentTypeRef::Instance(downstream_inst_ty));
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
                    &mut aliases, &mut func_count, funcs, downstream_inst,
                    before_inst, after_inst, blocking_inst,
                );
                downstream_func_base = dfb;
                before_comp_func = bcf;
                after_comp_func = acf;
                blocking_comp_func = blcf;

                let mut res_vec: Vec<u32> = Vec::new();
                for (_vid, export_name, _res_local, _own_local) in &inst_ctx.resource_exports {
                    let comp_res_idx = type_count;
                    type_count += 1;
                    aliases.alias(Alias::InstanceExport {
                        instance: downstream_inst,
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
        // ── Path (A): types-instance import pattern ──────────────────────────
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
            downstream_inst_ty = type_count;
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

                build_downstream_inst_type(&mut ctx, &mut inst, funcs, arena);
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
            downstream_inst = instance_count;
            instance_count += 1;
            imports.import(
                target_interface,
                ComponentTypeRef::Instance(downstream_inst_ty),
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
                downstream_inst,
                before_inst,
                after_inst,
                blocking_inst,
            );
            downstream_func_base = dfb;
            before_comp_func = bcf;
            after_comp_func = acf;
            blocking_comp_func = blcf;
            component.section(&aliases);
        }
    } else {
        // ── Path (B): original single-instance pattern (no resources) ────────

        // Section 1: all types in one section
        {
            let mut types = ComponentTypeSection::new();
            emit_hook_func_types(&mut types);
            type_count += 2;

            // Downstream instance type with SubResource exports.
            downstream_inst_ty = type_count;
            type_count += 1;
            {
                let mut ctx = InstTypeCtx::new();
                let mut inst = InstanceType::new();
                build_downstream_inst_type(&mut ctx, &mut inst, funcs, arena);
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
            downstream_inst = instance_count;
            instance_count += 1;
            imports.import(
                target_interface,
                ComponentTypeRef::Instance(downstream_inst_ty),
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

        // Section 3: alias funcs + resource types from downstream
        {
            let mut aliases = ComponentAliasSection::new();
            let (dfb, bcf, acf, blcf) = emit_func_aliases(
                &mut aliases,
                &mut func_count,
                funcs,
                downstream_inst,
                before_inst,
                after_inst,
                blocking_inst,
            );
            downstream_func_base = dfb;
            before_comp_func = bcf;
            after_comp_func = acf;
            blocking_comp_func = blcf;

            // Alias resource types from the downstream instance.
            let mut res_vec: Vec<u32> = Vec::new();
            for (_vid, export_name, _res_local, _own_local) in &inst_ctx.resource_exports {
                let comp_res_idx = type_count;
                type_count += 1;
                aliases.alias(Alias::InstanceExport {
                    instance: downstream_inst,
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
        let mut comp_cv_cache: HashMap<ValueTypeId, u32> = HashMap::new();
        if any_has_resources && has_type_exports {
            if let InterfaceType::Instance(inst) = iface_ty {
                for (_name, &vid) in &inst.type_exports {
                    if !matches!(arena.lookup_val(vid), ValueType::Resource(_) | ValueType::AsyncHandle) {
                        if let Some(&comp_idx) = comp_aliased_types.get(&vid) {
                            comp_cv_cache.insert(vid, comp_idx);
                        }
                    }
                }
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
    // downstream functions with complex result types that contain strings/resources).
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
        // For functions with resources/strings: need Memory + UTF8 + Realloc.
        // For async: also need Async flag.
        core_downstream_func_base = core_func_count;
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
            canons.lower(downstream_func_base + i as u32, opts);
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
/// When `with_realloc` is true, also exports a bump-allocator "realloc" function.
/// This memory is shared with the dispatch module for string passing and result buffers.
fn build_mem_module(with_realloc: bool, bump_start: u32) -> Module {
    let mut module = Module::new();

    if with_realloc {
        // Type section (1): realloc signature (i32,i32,i32,i32)->i32
        let mut types = TypeSection::new();
        types
            .ty()
            .function([ValType::I32, ValType::I32, ValType::I32, ValType::I32], [ValType::I32]);
        module.section(&types);

        // Function section (3): declare one function (realloc) with type 0
        let mut fn_section = FunctionSection::new();
        fn_section.function(0);
        module.section(&fn_section);
    }

    // Memory section (5): one memory, 1 initial page, no maximum
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

    if with_realloc {
        // Global section (6): bump pointer initialized to bump_start
        let mut globals = wasm_encoder::GlobalSection::new();
        globals.global(
            wasm_encoder::GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &wasm_encoder::ConstExpr::i32_const(bump_start as i32),
        );
        module.section(&globals);
    }

    // Export section (7)
    {
        let mut exports = ExportSection::new();
        exports.export("mem", ExportKind::Memory, 0);
        if with_realloc {
            exports.export("realloc", ExportKind::Func, 0);
        }
        module.section(&exports);
    }

    if with_realloc {
        // Code section (10): bump allocator
        // aligned = (bump_ptr + align - 1) & ~(align - 1)
        // bump_ptr = aligned + new_size
        // return aligned
        let mut code_section = CodeSection::new();
        let mut rf = Function::new(vec![(1u32, ValType::I32)]); // local 4: aligned
        // mask = ~(align - 1) = (align - 1) ^ -1
        rf.instruction(&Instruction::LocalGet(2)); // align
        rf.instruction(&Instruction::I32Const(1));
        rf.instruction(&Instruction::I32Sub);
        rf.instruction(&Instruction::I32Const(-1));
        rf.instruction(&Instruction::I32Xor); // ~(align - 1)
        // bump_ptr + align - 1
        rf.instruction(&Instruction::GlobalGet(0));
        rf.instruction(&Instruction::LocalGet(2));
        rf.instruction(&Instruction::I32Const(1));
        rf.instruction(&Instruction::I32Sub);
        rf.instruction(&Instruction::I32Add);
        // aligned = and
        rf.instruction(&Instruction::I32And);
        rf.instruction(&Instruction::LocalTee(4));
        // bump_ptr = aligned + new_size
        rf.instruction(&Instruction::LocalGet(3));
        rf.instruction(&Instruction::I32Add);
        rf.instruction(&Instruction::GlobalSet(0));
        // return aligned
        rf.instruction(&Instruction::LocalGet(4));
        rf.instruction(&Instruction::End);
        code_section.function(&rf);
        module.section(&code_section);
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
/// `needs_realloc` is true when any async function has string params (canon lift async requires Realloc).
/// `bump_start` is the first free byte in linear memory for the bump allocator (after static data).
fn build_dispatch_module(
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    event_ptr: u32,
    block_result_ptr: Option<u32>,
    needs_realloc: bool,
    bump_start: u32,
    arena: &TypeArena,
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
    let void_void_ty: u32; // ()->()  for void async task.return
    let void_i64_ty: u32;
    let void_f32_ty: u32;
    let void_f64_ty: u32;
    // NOTE: realloc is now in the memory module; realloc_ty no longer needed here.

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
                    if func.result_type_id.is_some() {
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
            ty_idx += 1;
            types.ty().function([ValType::F64], []);

            void_void_ty = ty_idx;
            ty_idx += 1;
            types.ty().function([], []);

            // Per-function custom task.return types for multi-value results.
            // Only emitted for async funcs with result_is_complex (>1 flat value).
            for func in funcs.iter() {
                if func.is_async && func.result_is_complex {
                    ty_idx += 1;
                    types
                        .ty()
                        .function(func.core_results.iter().copied(), []);
                }
            }
        } else {
            // Placeholders — never used when has_async is false.
            waitable_new_ty = 0;
            waitable_join_ty = 0;
            waitable_wait_ty = 0;
            void_i32_ty = 0;
            void_i64_ty = 0;
            void_f32_ty = 0;
            void_f64_ty = 0;
            void_void_ty = 0;
        }

        // NOTE: realloc type is now in the memory module, not here.
        let _ = needs_realloc;
        let _ = bump_start;

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

            // task.return per async func (void or non-void).
            // Multi-value results use custom types emitted after void_void_ty.
            let mut trf: Vec<Option<u32>> = vec![None; funcs.len()];
            // The custom multi-value task.return types start after void_void_ty + 1.
            let mut custom_tr_ty_idx = void_void_ty + 1;
            for (i, func) in funcs.iter().enumerate() {
                if func.is_async {
                    let tr_ty = if func.result_type_id.is_none() {
                        void_void_ty
                    } else if func.result_is_complex {
                        // Multi-value: use per-function custom type.
                        let ty = custom_tr_ty_idx;
                        custom_tr_ty_idx += 1;
                        ty
                    } else {
                        // Single-value: use the matching primitive type.
                        match func.core_results.first() {
                            Some(ValType::I32) => void_i32_ty,
                            Some(ValType::I64) => void_i64_ty,
                            Some(ValType::F32) => void_f32_ty,
                            Some(ValType::F64) => void_f64_ty,
                            _ => void_i32_ty,
                        }
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
                // waitable_new, join, wait, drop, subtask_drop + one task.return per async func
                5 + funcs
                    .iter()
                    .filter(|f| f.is_async)
                    .count() as u32
            } else {
                0
            };

        for (i, _) in funcs.iter().enumerate() {
            fn_section.function(wrapper_ty_base + i as u32);
        }
        module.section(&fn_section);
    }

    // NOTE: Global section for bump allocator is now in the memory module.

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
            let has_result = func.result_type_id.is_some();

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
                // Async-lifted functions MUST call task.return before returning.
                // For void async: task.return with no args.
                // (Blocking with a result-returning async function is rejected earlier.)
                if let Some(tr_fn) = task_return_fns[fi] {
                    f.instruction(&Instruction::Call(tr_fn));
                }
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
                // TODO: multi-value results (e.g. string → ptr+len) with has_after need
                // multiple locals saved, one per core_results entry, not just the first.
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
                // 5. task.return — required for ALL async-lifted wrappers before End.
                if let Some(result_ptr) = func.async_result_mem_offset {
                    if let Some(tr_fn) = task_return_fns[fi] {
                        if func.result_is_complex {
                            // Multi-value result: read flat values from memory.
                            // For result<T, E>: read discriminant + first payload, zero rest.
                            emit_task_return_loads(
                                &mut f,
                                result_ptr,
                                func.result_type_id.unwrap(),
                                &func.core_results,
                                arena,
                            );
                        } else {
                            // Simple (single value): load from memory.
                            f.instruction(&Instruction::I32Const(result_ptr as i32));
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
                            f.instruction(&load_instr);
                        }
                        f.instruction(&Instruction::Call(tr_fn));
                    }
                } else if let Some(tr_fn) = task_return_fns[fi] {
                    // Void async: task.return with no args (still required before End).
                    f.instruction(&Instruction::Call(tr_fn));
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

        // NOTE: realloc is now in the memory module, not here.

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

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use cviz::model::{FuncSignature, InstanceInterface};

    /// Helper: validate that bytes form a valid component-model binary.
    fn validate_component(bytes: &[u8]) {
        let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
        validator.validate_all(bytes).expect("generated adapter should be a valid component");
    }

    /// Helper: generate an adapter and return the raw bytes.
    fn gen_adapter(
        target: &str,
        hooks: &[&str],
        iface: &InterfaceType,
        arena: &TypeArena,
    ) -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        let hook_strings: Vec<String> = hooks.iter().map(|s| s.to_string()).collect();
        let path = generate_tier1_adapter(
            "test-mdl",
            None,
            target,
            &hook_strings,
            Some(iface),
            tmp.path().to_str().unwrap(),
            None, // no downstream split in unit tests
            arena,
        )
        .expect("adapter generation should succeed");
        std::fs::read(&path).expect("should read generated adapter file")
    }

    fn make_iface(funcs: Vec<(&str, FuncSignature)>) -> InterfaceType {
        InterfaceType::Instance(InstanceInterface {
            functions: funcs
                .into_iter()
                .map(|(n, s)| (n.to_string(), s))
                .collect(),
            type_exports: BTreeMap::new(),
        })
    }

    fn sig(is_async: bool, names: &[&str], params: Vec<ValueTypeId>, results: Vec<ValueTypeId>) -> FuncSignature {
        FuncSignature {
            is_async,
            param_names: names.iter().map(|s| s.to_string()).collect(),
            params,
            results,
        }
    }

    // ── Tier 1: sync primitives ──────────────────────────────────────────

    #[test]
    fn test_adapter_sync_primitives() {
        let mut arena = TypeArena::default();
        let s32 = arena.intern_val(ValueType::S32);
        let iface = make_iface(vec![("add", sig(false, &["a", "b"], vec![s32, s32], vec![s32]))]);
        let bytes = gen_adapter(
            "test:pkg/adder@1.0.0",
            &["splicer:adapter/before", "splicer:adapter/after"],
            &iface,
            &arena,
        );
        validate_component(&bytes);
    }

    // ── Tier 1: async void with string param ─────────────────────────────

    #[test]
    fn test_adapter_async_void_string() {
        let mut arena = TypeArena::default();
        let string = arena.intern_val(ValueType::String);
        let iface = make_iface(vec![("print", sig(true, &["msg"], vec![string], vec![]))]);
        let bytes = gen_adapter(
            "test:pkg/printer@1.0.0",
            &["splicer:adapter/before", "splicer:adapter/after"],
            &iface,
            &arena,
        );
        validate_component(&bytes);
    }

    // ── Tier 1: async with resource types (HTTP handler pattern) ─────────

    #[test]
    fn test_adapter_resource_handler() {
        let mut arena = TypeArena::default();

        // Build the error-code variant (simplified)
        let string_id = arena.intern_val(ValueType::String);
        let opt_string = arena.intern_val(ValueType::Option(string_id));
        let u16_id = arena.intern_val(ValueType::U16);
        let opt_u16 = arena.intern_val(ValueType::Option(u16_id));
        let dns_error_payload = arena.intern_val(ValueType::Record(vec![
            ("rcode".into(), opt_string),
            ("info-code".into(), opt_u16),
        ]));
        let error_code = arena.intern_val(ValueType::Variant(vec![
            ("DNS-timeout".into(), None),
            ("DNS-error".into(), Some(dns_error_payload)),
            ("connection-refused".into(), None),
            ("internal-error".into(), Some(opt_string)),
        ]));

        let request = arena.intern_val(ValueType::Resource("request".into()));
        let response = arena.intern_val(ValueType::Resource("response".into()));
        let result_ty = arena.intern_val(ValueType::Result {
            ok: Some(response),
            err: Some(error_code),
        });

        let func = sig(true, &["request"], vec![request], vec![result_ty]);
        let iface = InterfaceType::Instance(InstanceInterface {
            functions: BTreeMap::from([("handle".to_string(), func)]),
            type_exports: BTreeMap::from([
                ("request".to_string(), request),
                ("response".to_string(), response),
                ("error-code".to_string(), error_code),
            ]),
        });

        let bytes = gen_adapter(
            "wasi:http/handler@0.3.0-rc-2026-01-06",
            &["splicer:adapter/before", "splicer:adapter/after"],
            &iface,
            &arena,
        );
        validate_component(&bytes);
    }

    // ── Tier 1: multiple functions ───────────────────────────────────────

    #[test]
    fn test_adapter_multi_func() {
        let mut arena = TypeArena::default();
        let s32 = arena.intern_val(ValueType::S32);
        let string = arena.intern_val(ValueType::String);
        let iface = make_iface(vec![
            ("add", sig(false, &["a", "b"], vec![s32, s32], vec![s32])),
            ("print", sig(true, &["msg"], vec![string], vec![])),
            ("get-value", sig(false, &[], vec![], vec![s32])),
        ]);
        let bytes = gen_adapter(
            "test:pkg/mixed@1.0.0",
            &["splicer:adapter/before", "splicer:adapter/after"],
            &iface,
            &arena,
        );
        validate_component(&bytes);
    }

    // ── Tier 1: before hook only ─────────────────────────────────────────

    #[test]
    fn test_adapter_before_only() {
        let mut arena = TypeArena::default();
        let s32 = arena.intern_val(ValueType::S32);
        let iface = make_iface(vec![("get", sig(false, &[], vec![], vec![s32]))]);
        let bytes = gen_adapter(
            "test:pkg/getter@1.0.0",
            &["splicer:adapter/before"],
            &iface,
            &arena,
        );
        validate_component(&bytes);
    }

    // ── Tier 1: after hook only ──────────────────────────────────────────

    #[test]
    fn test_adapter_after_only() {
        let mut arena = TypeArena::default();
        let s32 = arena.intern_val(ValueType::S32);
        let iface = make_iface(vec![("get", sig(true, &[], vec![], vec![s32]))]);
        let bytes = gen_adapter(
            "test:pkg/getter@1.0.0",
            &["splicer:adapter/after"],
            &iface,
            &arena,
        );
        validate_component(&bytes);
    }

    // ── Tier 1: blocking hook (void async only) ──────────────────────────

    #[test]
    fn test_adapter_blocking() {
        let mut arena = TypeArena::default();
        let string = arena.intern_val(ValueType::String);
        let iface = make_iface(vec![("fire", sig(true, &["msg"], vec![string], vec![]))]);
        let bytes = gen_adapter(
            "test:pkg/fire@1.0.0",
            &["splicer:adapter/before", "splicer:adapter/blocking", "splicer:adapter/after"],
            &iface,
            &arena,
        );
        validate_component(&bytes);
    }

    // ── Tier 1: no hooks at all ──────────────────────────────────────────

    #[test]
    fn test_adapter_no_hooks() {
        let mut arena = TypeArena::default();
        let s32 = arena.intern_val(ValueType::S32);
        let iface = make_iface(vec![("add", sig(false, &["a", "b"], vec![s32, s32], vec![s32]))]);
        let bytes = gen_adapter(
            "test:pkg/adder@1.0.0",
            &[],
            &iface,
            &arena,
        );
        validate_component(&bytes);
    }
}
