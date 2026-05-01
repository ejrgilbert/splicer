//! Wasm-encoder emit helpers shared between tier-1 and tier-2's
//! dispatch core modules. Everything here is canonical-ABI plumbing
//! that wit-component's `ComponentEncoder` requires the core module
//! to provide regardless of which tier of adapter it backs.

use wasm_encoder::{
    CodeSection, ConstExpr, Function, GlobalSection, GlobalType, MemorySection, MemoryType,
    Module, ValType,
};
use wit_parser::abi::WasmType;

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
