//! Per-function value object for the tier-1 adapter generator.
//!
//! [`AdapterFunc`] holds everything the component-level builder and
//! the dispatch-module builder need about a single function in the
//! target interface — both the component-level type info (parameter
//! `ValueTypeId`s, result type) and the core-Wasm canonical-ABI
//! flattening (core param/result `ValType`s, memory offsets for
//! async results, name offsets for the dispatch module's data
//! segment).
//!
//! [`extract_adapter_funcs`] turns a cviz `InterfaceType::Instance`
//! into a `Vec<AdapterFunc>` ready to feed to the builders.

use cviz::model::{InterfaceType, TypeArena, ValueTypeId};
use wasm_encoder::ValType;

use super::ty::{flat_types_for, type_has_strings};

/// A function in the target interface, fully resolved to both
/// component-level and core-Wasm types for adapter generation.
pub(super) struct AdapterFunc {
    /// The function's name in the interface.
    pub name: String,
    /// Whether this function is `async` in the component model.
    pub is_async: bool,
    /// Parameter names, parallel to `param_type_ids`. Falls back to
    /// `p{i}` when the cviz model did not carry names (e.g. from
    /// JSON input).
    pub param_names: Vec<String>,
    /// Original `ValueTypeId`s for each parameter (for
    /// component-level type encoding).
    pub param_type_ids: Vec<ValueTypeId>,
    /// Original `ValueTypeId` for the result (for component-level
    /// type encoding).
    pub result_type_id: Option<ValueTypeId>,
    /// True when the result type requires pointer-based passing
    /// (>MAX_FLAT_RESULTS flat values).
    pub result_is_complex: bool,
    /// Core Wasm parameter types after canonical ABI flattening.
    pub core_params: Vec<ValType>,
    /// Core Wasm result types after canonical ABI flattening. For
    /// async functions this reflects the sync canonical types
    /// (used for `task.return` type). For complex results this is
    /// `[I32]` (the pointer type for `task.return`).
    pub core_results: Vec<ValType>,
    /// Byte offset of `name` in the dispatch module's data segment.
    pub name_offset: u32,
    /// Byte length of `name` (UTF-8).
    pub name_len: u32,
    /// For async functions that have a result: the byte offset
    /// within the dispatch module's memory where the result will
    /// be written by the async-lowered handler call. `None` for
    /// sync functions or async void functions.
    pub async_result_mem_offset: Option<u32>,
    /// Number of bytes reserved for the async result in linear
    /// memory. 8 for simple (fits in one register), 512 for
    /// complex (pointer-based) results.
    pub async_result_mem_size: u32,
    /// For sync functions with complex results (`result_is_complex`):
    /// the byte offset within the dispatch module's memory where the
    /// wrapper stores the result buffer address that canon lift reads
    /// from. `None` for async functions or sync functions with simple
    /// (single-value) results.
    pub sync_result_mem_offset: Option<u32>,
}

impl AdapterFunc {
    /// Returns true if any parameter or the result contains a string
    /// type (deep check — traverses compound types).
    pub fn has_strings(&self, arena: &TypeArena) -> bool {
        self.param_type_ids
            .iter()
            .any(|&id| type_has_strings(id, arena))
            || self
                .result_type_id
                .is_some_and(|id| type_has_strings(id, arena))
    }
}

/// Resolve a cviz `InterfaceType::Instance` into a list of
/// [`AdapterFunc`]s with both component-level type ids and
/// canonical-ABI core-Wasm flattening pre-computed.
///
/// Errors when:
/// - The interface is not an instance type (bare function
///   interfaces aren't supported by the tier-1 adapter generator)
/// - A function has more than one result
/// - A sync function has a multi-value result (would need retptr
///   handling, not yet implemented)
pub(super) fn extract_adapter_funcs(
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
    // Async result storage lives right after the concatenated
    // function-name bytes, rounded up to 4-byte alignment.
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
            // Store full flat types. For async functions `task.return`
            // uses these as params (up to MAX_FLAT_PARAMS=16). For sync
            // functions with `is_complex`, the canonical ABI uses a
            // retptr pattern: an extra i32 param is appended and the
            // function returns void (results are written at the retptr
            // by the callee).
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

        // For sync functions with complex results (> MAX_FLAT_RESULTS),
        // reserve a result buffer in linear memory. The wrapper stores
        // handler output here and returns the buffer address to canon lift.
        // Size: 4 bytes per I32/F32, 8 bytes per I64/F64.
        let sync_result_mem_offset = if !sig.is_async && result_is_complex {
            let size: u32 = core_results
                .iter()
                .map(|vt| match vt {
                    ValType::I64 | ValType::F64 => 8u32,
                    _ => 4u32,
                })
                .sum();
            let aligned = (async_result_cursor + 3) & !3;
            async_result_cursor = aligned + size;
            Some(aligned)
        } else {
            None
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
            sync_result_mem_offset,
        });
        name_offset += name_len;
    }
    Ok(funcs)
}
