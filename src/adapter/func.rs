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

use cviz::model::{FuncSignature, InterfaceType, TypeArena, ValueTypeId};
use wasm_encoder::ValType;

use super::mem_layout::MemoryLayoutBuilder;
use super::ty::{flat_types_for, type_has_lists, type_has_strings, FlatLayout};

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
    /// For sync functions with complex results (`result_is_complex`):
    /// the byte offset within the dispatch module's memory where the
    /// wrapper stores the result buffer address that canon lift reads
    /// from. `None` for async functions or sync functions with simple
    /// (single-value) results.
    pub sync_result_mem_offset: Option<u32>,
    /// True when any parameter or the result contains a string
    /// (deep check; traverses compound types). Drives the
    /// canon-lift/lower `realloc` option.
    pub has_strings: bool,
    /// True when any parameter or the result contains a list
    /// (`list<T>` or `list<T, N>`, deep check). Drives the
    /// needs-realloc decision — canon lower allocates memory via
    /// realloc to marshal list contents.
    pub has_lists: bool,
}

impl AdapterFunc {
    /// True when any canon operation on this function needs the
    /// `Memory(_)` option — i.e. when at least one param or result
    /// is marshaled through linear memory. Covers:
    /// - strings / lists (`(ptr, len)` body in memory)
    /// - sync-complex results (retptr pattern)
    /// - async functions with any result (written to the pre-reserved
    ///   async result buffer)
    ///
    /// Bare resource handles don't need memory on their own — they're
    /// `i32` values on the wire. A resource inside a compound that
    /// goes through retptr is caught by `result_is_complex`, and a
    /// resource in an async result is caught by the async-with-result
    /// clause.
    pub fn canon_needs_memory(&self) -> bool {
        self.has_strings
            || self.has_lists
            || self.result_is_complex
            || (self.is_async && self.result_type_id.is_some())
    }

    /// True when any canon operation on this function may need to
    /// allocate memory via `realloc` — strings and lists, which the
    /// canonical ABI marshals as `(ptr, len)` pairs written into
    /// memory by the lowering side. Bare resource handles (`own<T>`)
    /// don't need realloc — they're just `i32` values on the wire
    /// and never allocate.
    pub fn canon_needs_realloc(&self) -> bool {
        self.has_strings || self.has_lists
    }

    /// True when any canon operation on this function uses UTF-8 for
    /// string encoding. UTF-8 is only relevant when a string is
    /// actually present — resources and lists of non-string types
    /// don't need it.
    pub fn canon_needs_utf8(&self) -> bool {
        self.has_strings
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
) -> anyhow::Result<(Vec<AdapterFunc>, MemoryLayoutBuilder)> {
    let inst = match iface_ty {
        InterfaceType::Instance(i) => i,
        InterfaceType::Func(_) => anyhow::bail!(
            "Expected an instance-type interface for tier-1 adapter generation; \
             bare function-type interfaces are not yet supported. If you need this, \
             please open an issue with a repro at https://github.com/ejrgilbert/splicer/issues"
        ),
    };

    let total_name_bytes: u32 = inst.functions.keys().map(|n| n.len() as u32).sum();
    let mut layout = MemoryLayoutBuilder::new(total_name_bytes);
    let mut funcs = Vec::with_capacity(inst.functions.len());

    for (name, sig) in &inst.functions {
        let extracted = extract_func_sig(name, sig, arena)?;
        let name_len = name.len() as u32;
        let name_offset = layout.alloc_name(name_len);

        let has_result = extracted.result_type_id.is_some();
        let async_result_mem_offset = (extracted.is_async && has_result)
            .then(|| layout.alloc_async_result(extracted.result_byte_size));
        let sync_result_mem_offset = (!extracted.is_async && extracted.result_is_complex)
            .then(|| layout.alloc_sync_result(extracted.result_byte_size));

        let param_ids = extracted.param_type_ids.iter().copied();
        let result_id = extracted.result_type_id.into_iter();
        let all_ids: Vec<ValueTypeId> = param_ids.chain(result_id).collect();
        let has_strings = all_ids.iter().any(|&id| type_has_strings(id, arena));
        let has_lists = all_ids.iter().any(|&id| type_has_lists(id, arena));

        funcs.push(AdapterFunc {
            name: name.clone(),
            is_async: extracted.is_async,
            param_names: extracted.param_names,
            param_type_ids: extracted.param_type_ids,
            result_type_id: extracted.result_type_id,
            result_is_complex: extracted.result_is_complex,
            core_params: extracted.core_params,
            core_results: extracted.core_results,
            name_offset,
            name_len,
            async_result_mem_offset,
            sync_result_mem_offset,
            has_strings,
            has_lists,
        });
    }
    // The builder is returned (not dropped) so the adapter builder
    // can continue appending fixed slots — event record, block
    // result, bump_start — without re-deriving the post-func cursor
    // from the funcs table.
    Ok((funcs, layout))
}

/// Intermediate value: everything pulled out of a single cviz
/// [`FuncSignature`] that doesn't depend on where the func ends up in
/// the dispatch module's memory layout. Produced by
/// [`extract_func_sig`], consumed by [`extract_adapter_funcs`] as it
/// interleaves signature data with per-func memory-offset allocation.
struct ExtractedSig {
    is_async: bool,
    param_names: Vec<String>,
    param_type_ids: Vec<ValueTypeId>,
    result_type_id: Option<ValueTypeId>,
    /// `true` when the flat result won't fit in `MAX_FLAT_RESULTS`
    /// core values and canon lift/lower fall back to the retptr
    /// pattern.
    result_is_complex: bool,
    core_params: Vec<ValType>,
    core_results: Vec<ValType>,
    /// Pre-summed byte size of `core_results`, used by the memory
    /// layout builder to size the async/sync-complex result buffer.
    result_byte_size: u32,
}

/// Resolve a single cviz [`FuncSignature`] into the core-Wasm shape
/// the adapter builders consume: param-name/type parallel vectors
/// with a `p{i}` fallback for unnamed params, canonical-ABI flat
/// types for both params and result, and the
/// `MAX_FLAT_RESULTS`-based complexity flag.
///
/// Errors when a function declares more than one result — we only
/// support 0 or 1 results today.
fn extract_func_sig(
    name: &str,
    sig: &FuncSignature,
    arena: &TypeArena,
) -> anyhow::Result<ExtractedSig> {
    let mut param_names = Vec::with_capacity(sig.params.len());
    let mut param_type_ids = Vec::with_capacity(sig.params.len());
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

    let (result_type_id, result_is_complex, core_results, result_byte_size) =
        if sig.results.is_empty() {
            (None, false, vec![], 0)
        } else {
            let rid = sig.results[0];
            let flat = flat_types_for(rid, arena);
            let is_complex = flat.len() > 1;
            // `FlatLayout::total_bytes` accounts for the
            // discriminant-and-padding shape of `result<T, E>` and
            // inter-slot natural alignment (`[i32, i64]` is 16 bytes,
            // not 12), so every result-buffer allocation uses the
            // same byte size the dispatch module's loads assume.
            let total_bytes = FlatLayout::new(rid, &flat, arena).total_bytes;
            // Store full flat types. For async functions `task.return`
            // uses these as params (up to MAX_FLAT_PARAMS=16). For sync
            // functions with `is_complex`, the canonical ABI uses a
            // retptr pattern: an extra i32 param is appended and the
            // function returns void (results are written at the retptr
            // by the callee).
            (Some(rid), is_complex, flat, total_bytes)
        };

    Ok(ExtractedSig {
        is_async: sig.is_async,
        param_names,
        param_type_ids,
        result_type_id,
        result_is_complex,
        core_params,
        core_results,
        result_byte_size,
    })
}
