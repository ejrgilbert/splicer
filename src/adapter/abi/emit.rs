//! Wasm-encoder emit helpers shared between tier-1 and tier-2's
//! dispatch core modules. Everything here is canonical-ABI plumbing
//! that wit-component's `ComponentEncoder` requires the core module
//! to provide regardless of which tier of adapter it backs.

use wasm_encoder::{
    CodeSection, ConstExpr, Function, GlobalSection, GlobalType, MemorySection, MemoryType, Module,
    ValType,
};
use wit_parser::abi::{WasmSignature, WasmType};
use wit_parser::{Int, Resolve, SizeAlign, Type, TypeDefKind, TypeId};

use super::super::indices::FunctionIndices;

// ─── Standard wasm-component-model exports ────────────────────────
//
// wit-component's `ComponentEncoder` requires the core module to
// export memory, cabi_realloc, and `_initialize` under these exact
// names — they're part of the canonical-ABI contract, not anything
// we get to pick.

pub(crate) const EXPORT_MEMORY: &str = "memory";
pub(crate) const EXPORT_CABI_REALLOC: &str = "cabi_realloc";
pub(crate) const EXPORT_INITIALIZE: &str = "_initialize";

/// Index of the bump-pointer global emitted by [`emit_memory_and_globals`].
/// Both `cabi_realloc` and any per-tier scratch allocator reference it.
pub(crate) const BUMP_POINTER_GLOBAL: u32 = 0;

/// Memory section + bump-pointer global. `bump_start` is the byte
/// offset where the bump allocator begins serving allocations; both
/// tiers compute it from their pre-allocated scratch / name regions.
pub(crate) fn emit_memory_and_globals(module: &mut Module, bump_start: u32) {
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memory);

    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(bump_start as i32),
    );
    module.section(&globals);
}

/// Standard `cabi_realloc(old_ptr, old_size, align, new_size) -> new_ptr`
/// implementation: bump-allocator that ignores `old_*`, aligns the
/// current bump pointer up to `align`, returns the aligned address,
/// and advances the bump global by `new_size`.
///
/// `align` is assumed to be a power of two (canonical-ABI guarantee).
/// Pushes the function into `code`; caller decides where it lands in
/// the function index space.
pub(crate) fn emit_cabi_realloc(code: &mut CodeSection) {
    const PARAM_COUNT: u32 = 4;
    const ALIGN_LOCAL: u32 = 2;
    const NEW_SIZE_LOCAL: u32 = 3;
    let mut locals = FunctionIndices::new(PARAM_COUNT);
    let scratch = locals.alloc_local(ValType::I32);
    let mut f = Function::new_with_locals_types(locals.into_locals());

    // scratch = (global.bump + (align - 1)) & ~(align - 1)
    f.instructions().global_get(BUMP_POINTER_GLOBAL);
    f.instructions().local_get(ALIGN_LOCAL);
    f.instructions().i32_const(1);
    f.instructions().i32_sub();
    f.instructions().i32_add();
    f.instructions().local_get(ALIGN_LOCAL);
    f.instructions().i32_const(1);
    f.instructions().i32_sub();
    f.instructions().i32_const(-1);
    f.instructions().i32_xor();
    f.instructions().i32_and();
    f.instructions().local_set(scratch);

    // global.bump = scratch + new_size
    f.instructions().local_get(scratch);
    f.instructions().local_get(NEW_SIZE_LOCAL);
    f.instructions().i32_add();
    f.instructions().global_set(BUMP_POINTER_GLOBAL);

    // return scratch
    f.instructions().local_get(scratch);
    f.instructions().end();
    code.function(&f);
}

/// `() -> ()` — empty body. Used for `_initialize` and per-func
/// `cabi_post_<name>` shims.
pub(crate) fn empty_function() -> Function {
    let mut f = Function::new_with_locals_types([]);
    f.instructions().end();
    f
}

/// wit-parser [`WasmType`]s → wasm-encoder [`ValType`]s. The
/// canonical-ABI flat-type representation of `Pointer` / `Length` is
/// always 32-bit on wasm32; `PointerOrI64` widens to i64.
pub(crate) fn val_types(types: &[WasmType]) -> Vec<ValType> {
    types.iter().copied().map(wasm_type_to_val).collect()
}

pub(crate) fn wasm_type_to_val(wt: WasmType) -> ValType {
    match wt {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => ValType::I32,
        WasmType::I64 | WasmType::PointerOrI64 => ValType::I64,
        WasmType::F32 => ValType::F32,
        WasmType::F64 => ValType::F64,
    }
}

// ─── Wrapper-body passthrough helpers ─────────────────────────────
// Bridge between callee-returns export sigs (`[] -> [I32]`) and
// caller-allocates import sigs (`[I32] -> []`) for sync compound
// returns. Shared across tiers.

/// `ValType` of a single-value direct (non-retptr) return, or `None`
/// for void / retptr-bound results.
pub(crate) fn direct_return_type(export_sig: &WasmSignature) -> Option<ValType> {
    if !export_sig.retptr && export_sig.results.len() == 1 {
        Some(wasm_type_to_val(export_sig.results[0]))
    } else {
        None
    }
}

/// Push the wrapper's `nparams` flat params (and an extra static
/// `retptr_offset` if the import wants caller-allocates), then call.
pub(crate) fn emit_handler_call(
    f: &mut Function,
    nparams: u32,
    import_retptr: bool,
    retptr_offset: Option<i32>,
    handler_idx: u32,
) {
    for p in 0..nparams {
        f.instructions().local_get(p);
    }
    if import_retptr {
        let off = retptr_offset.expect("import_retptr → retptr_offset must be Some");
        f.instructions().i32_const(off);
    }
    f.instructions().call(handler_idx);
}

/// Push either the direct-return local, or the static retptr (when
/// the export sig is callee-returns). No-op for void.
pub(crate) fn emit_wrapper_return(
    f: &mut Function,
    result_local: Option<u32>,
    export_retptr: bool,
    retptr_offset: Option<i32>,
) {
    if let Some(local) = result_local {
        f.instructions().local_get(local);
    } else if export_retptr {
        let off = retptr_offset.expect("export_retptr → retptr_offset must be Some");
        f.instructions().i32_const(off);
    }
}

// ─── Schema-driven layout helpers ─────────────────────────────────
// Tier-2 codegen builds static data segments whose shapes mirror
// canonical-ABI lowerings of WIT records (e.g. `field`, the on-call
// indirect-params record). Rather than hand-roll the byte offsets,
// these helpers ask `wit-parser::SizeAlign` to compute them — so a
// later change to `wit/common/world.wit` flows through without code
// edits.

/// Size + alignment + per-field byte offset (keyed by WIT field
/// name) of a record-shaped canonical-ABI value, on wasm32.
pub(crate) struct RecordLayout {
    pub size: u32,
    pub align: u32,
    pub field_offsets: std::collections::HashMap<String, u32>,
}

impl RecordLayout {
    /// Layout for an arbitrary list of named typed fields. Used for
    /// function-param records (e.g. on-call's
    /// `record { call: call-id, args: list<field> }`).
    pub(crate) fn for_named_fields(sizes: &SizeAlign, fields: &[(String, Type)]) -> Self {
        let types: Vec<Type> = fields.iter().map(|(_, t)| *t).collect();
        let info = sizes.record(&types);
        let offs = sizes.field_offsets(&types);
        Self {
            size: info.size.size_wasm32() as u32,
            align: info.align.align_wasm32() as u32,
            field_offsets: fields
                .iter()
                .zip(offs)
                .map(|((name, _), (off, _))| (name.clone(), off.size_wasm32() as u32))
                .collect(),
        }
    }

    /// Layout for a `record { … }` typedef (e.g. `field`,
    /// `field-tree`). Panics if `id` doesn't refer to a record.
    pub(crate) fn for_record_typedef(sizes: &SizeAlign, resolve: &Resolve, id: TypeId) -> Self {
        let typedef = &resolve.types[id];
        let TypeDefKind::Record(r) = &typedef.kind else {
            panic!(
                "RecordLayout::for_record_typedef called with non-record typedef `{:?}`",
                typedef.name
            );
        };
        let fields: Vec<(String, Type)> = r.fields.iter().map(|f| (f.name.clone(), f.ty)).collect();
        Self::for_named_fields(sizes, &fields)
    }

    /// Byte offset of the named field. Panics with a descriptive
    /// message if the field doesn't exist — i.e. the WIT was renamed
    /// without updating codegen.
    pub(crate) fn offset_of(&self, name: &str) -> u32 {
        *self.field_offsets.get(name).unwrap_or_else(|| {
            let mut keys: Vec<&str> = self.field_offsets.keys().map(|s| s.as_str()).collect();
            keys.sort();
            panic!("RecordLayout: no field named `{name}` (record has: {keys:?})")
        })
    }
}

/// Byte offset of an `option<T>`'s payload area (i.e. the byte right
/// after the 1-byte disc, padded up to `align(T)`).
pub(crate) fn option_payload_offset(sizes: &SizeAlign, payload_ty: &Type) -> u32 {
    sizes
        .payload_offset(Int::U8, [Some(payload_ty)])
        .size_wasm32() as u32
}

/// Canonical-ABI byte offsets of `string`/`list<T>`'s flat (ptr, len)
/// pair. Both are i32-aligned — true regardless of which `T` the
/// list carries — so these are kept as named constants instead of
/// being looked up via `SizeAlign`.
pub(crate) const SLICE_PTR_OFFSET: u32 = 0;
pub(crate) const SLICE_LEN_OFFSET: u32 = 4;
