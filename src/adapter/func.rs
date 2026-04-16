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

use super::ty::{align_to_val, flat_types_for, type_has_strings, val_type_byte_size};

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
/// The returned `Vec` has **one entry per function in the target
/// interface**. A "target interface" is an instance type, and an
/// instance type can export any number of functions — e.g.
/// `wasi:http/handler` exports just `handle`, but a hypothetical
/// `my:service/math` could export `add`, `sub`, `mul`, `div`. The
/// tier-1 adapter interposes on *all* of them uniformly: it emits a
/// dispatch wrapper per function, each invoking the same
/// `before-call(name) / after-call(name) / should-block-call(name)`
/// hook imports with the function's own name as the string arg — so
/// the middleware can discriminate per-func via that `name`.
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
             bare function-type interfaces are not yet supported. If you need this, \
             please open an issue with a repro at https://github.com/ejrgilbert/splicer/issues"
        ),
    };

    let mut funcs = Vec::new();
    let mut name_offset: u32 = 0;
    // Result buffer storage lives right after the concatenated
    // function-name bytes, aligned up to 4 bytes so that i32/f32
    // loads and stores are naturally aligned. This cursor tracks
    // the next free byte for both async result buffers and
    // sync-complex retptr buffers.
    let total_name_bytes: u32 = inst.functions.keys().map(|n| n.len() as u32).sum();
    let result_buf_base: u32 = align_to_val(total_name_bytes, 4);
    let mut result_buf_cursor: u32 = result_buf_base;

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
                 for tier-1 adapter generation. If you need multi-result support, \
                 please open an issue with a repro at https://github.com/ejrgilbert/splicer/issues",
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

        // Compute the exact byte size needed to store the flat result
        // values in linear memory.
        let result_byte_size: u32 = core_results.iter().map(val_type_byte_size).sum();

        // For async functions with a result, reserve memory for the
        // async-lowered handler to write into.
        let (async_result_mem_offset, async_result_mem_size) =
            if sig.is_async && result_type_id.is_some() {
                let off = result_buf_cursor;
                result_buf_cursor += result_byte_size;
                (Some(off), result_byte_size)
            } else {
                (None, 0)
            };

        // For sync functions with complex results (> MAX_FLAT_RESULTS),
        // reserve a result buffer in linear memory. The wrapper stores
        // handler output here and returns the buffer address to canon lift.
        let sync_result_mem_offset = if !sig.is_async && result_is_complex {
            let aligned = align_to_val(result_buf_cursor, 4);
            result_buf_cursor = aligned + result_byte_size;
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
