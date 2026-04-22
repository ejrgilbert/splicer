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

use cviz::model::{FuncSignature, InterfaceType, ValueTypeId};
use wasm_encoder::ValType;
use wit_parser::abi::{AbiVariant, WasmSignature};
use wit_parser::{Docs, Function, FunctionKind, Stability};

use super::abi::{wasm_to_val, WitBridge};
use super::build::MemoryLayoutBuilder;

/// A function in the target interface, fully resolved to both
/// component-level and core-Wasm types for adapter generation.
pub(crate) struct AdapterFunc {
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

    /// True when canon-lower-async lowers this func's params via a
    /// single pointer instead of flat values. Centralized so every
    /// flat-form-assuming dispatch site can guard with
    /// `debug_assert!(!func.uses_async_pointer_params(bridge))`.
    pub fn uses_async_pointer_params(&self, bridge: &WitBridge) -> bool {
        self.is_async
            && self
                .wasm_signature(bridge, AbiVariant::GuestImportAsync)
                .indirect_params
    }

    /// Core-Wasm `(params, results)` for the imported handler call
    /// (canon-lower side), including any retptr/result_ptr appended
    /// per the canonical ABI.
    pub fn handler_import_sig(&self, bridge: &WitBridge) -> (Vec<ValType>, Vec<ValType>) {
        let variant = if self.is_async {
            AbiVariant::GuestImportAsync
        } else {
            AbiVariant::GuestImport
        };
        self.core_sig(bridge, variant)
    }

    /// Core-Wasm `(params, results)` for the adapter's exported
    /// wrapper (canon-lift side). Splicer uses stackful async
    /// exports, so async funcs get `GuestExportAsyncStackful`.
    pub fn wrapper_export_sig(&self, bridge: &WitBridge) -> (Vec<ValType>, Vec<ValType>) {
        let variant = if self.is_async {
            AbiVariant::GuestExportAsyncStackful
        } else {
            AbiVariant::GuestExport
        };
        self.core_sig(bridge, variant)
    }

    fn core_sig(&self, bridge: &WitBridge, variant: AbiVariant) -> (Vec<ValType>, Vec<ValType>) {
        let ws = self.wasm_signature(bridge, variant);
        let params = ws.params.into_iter().map(wasm_to_val).collect();
        let results = ws.results.into_iter().map(wasm_to_val).collect();
        (params, results)
    }

    /// wit-parser's authoritative core-Wasm signature for this func
    /// under `variant`. Encodes `MAX_FLAT_PARAMS`,
    /// `MAX_FLAT_ASYNC_PARAMS`, retptr placement, etc., so splicer
    /// doesn't re-derive those rules.
    pub fn wasm_signature(&self, bridge: &WitBridge, variant: AbiVariant) -> WasmSignature {
        let name = &self.name;
        let is_async = self.is_async;
        let param_names = self
            .param_names
            .iter()
            .cloned()
            .chain(std::iter::repeat(String::new()));
        let params_iter: Vec<(String, ValueTypeId)> = self
            .param_type_ids
            .iter()
            .copied()
            .zip(param_names)
            .map(|(id, n)| (n, id))
            .collect();
        let func = build_wit_function_from_parts(
            name,
            is_async,
            &params_iter,
            self.result_type_id,
            bridge,
        );
        bridge.resolve.wasm_signature(variant, &func)
    }
}

fn build_wit_function(name: &str, sig: &FuncSignature, bridge: &WitBridge) -> Function {
    let params: Vec<(String, ValueTypeId)> = sig
        .params
        .iter()
        .enumerate()
        .map(|(i, &id)| {
            let n = sig
                .param_names
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("p{i}"));
            (n, id)
        })
        .collect();
    build_wit_function_from_parts(
        name,
        sig.is_async,
        &params,
        sig.results.first().copied(),
        bridge,
    )
}

fn build_wit_function_from_parts(
    name: &str,
    is_async: bool,
    params: &[(String, ValueTypeId)],
    result_type_id: Option<ValueTypeId>,
    bridge: &WitBridge,
) -> Function {
    let kind = if is_async {
        FunctionKind::AsyncFreestanding
    } else {
        FunctionKind::Freestanding
    };
    let wit_params: Vec<(String, wit_parser::Type)> = params
        .iter()
        .map(|(pname, id)| (pname.clone(), bridge.get(*id)))
        .collect();
    Function {
        name: name.to_string(),
        kind,
        params: wit_params,
        result: result_type_id.map(|id| bridge.get(id)),
        docs: Docs::default(),
        stability: Stability::Unknown,
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
pub(crate) fn extract_adapter_funcs(
    iface_ty: &InterfaceType,
    bridge: &WitBridge,
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
        let extracted = extract_func_sig(name, sig, bridge)?;
        let name_len = name.len() as u32;
        let name_offset = layout.alloc_name(name_len);

        let has_result = extracted.result_type_id.is_some();
        let result_align = extracted
            .result_type_id
            .map(|id| bridge.align_bytes(id))
            .unwrap_or(1);
        let async_result_mem_offset = (extracted.is_async && has_result)
            .then(|| layout.alloc_async_result(extracted.result_byte_size, result_align));
        let sync_result_mem_offset = (!extracted.is_async && extracted.result_is_complex)
            .then(|| layout.alloc_sync_result(extracted.result_byte_size, result_align));

        let param_ids = extracted.param_type_ids.iter().copied();
        let result_id = extracted.result_type_id.into_iter();
        let all_ids: Vec<ValueTypeId> = param_ids.chain(result_id).collect();
        let has_strings = all_ids.iter().any(|&id| bridge.has_strings(id));
        let has_lists = all_ids.iter().any(|&id| bridge.has_lists(id));

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
    bridge: &WitBridge,
) -> anyhow::Result<ExtractedSig> {
    const MAX_FLAT: usize = 16;

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
        core_params.extend(bridge.flat_types(id));
    }
    if core_params.len() > MAX_FLAT {
        anyhow::bail!(
            "Function '{name}' has {} flat parameter values (exceeds the \
             canonical-ABI limit of {MAX_FLAT}). The pointer-form lowering \
             required for >{MAX_FLAT} flat params is not yet implemented.",
            core_params.len()
        );
    }
    // Grep for `uses_async_pointer_params` to find every dispatch
    // site that assumes flat form — removing the bail below while
    // leaving those sites unchanged fires their `debug_assert!`s.
    let wit_func = build_wit_function(name, sig, bridge);
    let import_variant = if sig.is_async {
        AbiVariant::GuestImportAsync
    } else {
        AbiVariant::GuestImport
    };
    let import_sig = bridge.resolve.wasm_signature(import_variant, &wit_func);

    if sig.is_async && import_sig.indirect_params {
        anyhow::bail!(
            "Function '{name}' is async with a param shape that wit-parser \
             lowers via pointer form ({} flat values). Pointer-form async \
             param dispatch is not yet implemented in the adapter body.",
            core_params.len()
        );
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
            let flat = bridge.flat_types(rid);
            if flat.len() > MAX_FLAT {
                anyhow::bail!(
                    "Function '{name}' has a result that flattens to {} core \
                     values (exceeds {MAX_FLAT}). The pointer-form lowering \
                     required for >{MAX_FLAT} flat results is not yet \
                     implemented.",
                    flat.len()
                );
            }
            // For async, `import_sig.retptr` flips on whenever the
            // func has *any* result (the result_ptr param) — but
            // `result_is_complex` downstream means "flattens to >1
            // values" (custom task.return type vs shared void_i32 /
            // void_i64 slot), so async has to test flatness directly.
            let is_complex = if sig.is_async {
                flat.len() > 1
            } else {
                import_sig.retptr
            };
            // Canonical-ABI memory size for the result — accounts for
            // the discriminant-and-padding shape of `result<T, E>`
            // and inter-field natural alignment (`record { i32, i64 }`
            // is 16 bytes, not 12). The dispatch module's loads use
            // this exact size when sizing the result buffer.
            let total_bytes = bridge.size_bytes(rid);
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
