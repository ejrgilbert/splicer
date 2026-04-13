//! Core-Wasm modules nested inside the tier-1 adapter component:
//! the **memory module** and the **dispatch module**.
//!
//! - [`build_mem_module`] emits a tiny core module that exports a
//!   1-page linear memory (and optionally a bump-allocator `realloc`)
//!   under the names `mem` / `realloc`. The dispatch module imports
//!   this memory so the canonical-ABI lift/lower options
//!   (`memory $0` / `realloc $f`) have something to point at.
//!
//! - [`build_dispatch_module`] emits the core module that holds the
//!   adapter's per-function wrapper bodies. Each wrapper:
//!     1. (optional) calls the `before_call(name)` hook and waits on
//!        the returned subtask handle if the hook is async-lowered;
//!     2. (optional) calls `should_block_call(name, result_ptr)` and,
//!        if the hook signals true, calls `task.return` and returns
//!        without invoking the handler;
//!     3. calls `handler_f{i}` — for sync funcs this is a regular
//!        call, for async funcs it's the `canon lower async` shape
//!        `(flat_params..., result_ptr?) -> packed_handle`;
//!     4. (optional) calls `after_call(name)` and waits;
//!     5. for async funcs, calls `task.return` (loading multi-value
//!        results from `async_result_mem_offset` via
//!        [`emit_task_return_loads`] when needed); for sync funcs,
//!        leaves the saved result on the value stack.
//!
//! [`emit_task_return_loads`] is the helper that pushes the flat
//! values for `task.return` by reading them out of the canonical-ABI
//! memory layout the async lowering wrote at `result_ptr`. It's
//! private to dispatch because no other phase needs to peek inside an
//! async result frame.
//!
//! Both builders return raw core-Wasm bytes; the component-builder
//! wraps them in a `ModuleSection` of the outer Component.

use cviz::model::{TypeArena, ValueType, ValueTypeId};
use wasm_encoder::{
    BlockType, CodeSection, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, Instruction, MemoryType, Module, TypeSection, ValType,
};

use super::func::AdapterFunc;
use super::ty::{align_to_val, canonical_align};

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
            // For non-result types with multi-value (e.g. bare string,
            // tuple, record): load each flat value from the result
            // buffer at sequential offsets.
            let mut byte_offset: u32 = 0;
            for vt in core_results.iter() {
                f.instruction(&Instruction::I32Const(result_ptr as i32));
                match vt {
                    ValType::I64 => {
                        f.instruction(&Instruction::I64Load(wasm_encoder::MemArg {
                            offset: byte_offset as u64,
                            align: 3,
                            memory_index: 0,
                        }));
                        byte_offset += 8;
                    }
                    ValType::F32 => {
                        f.instruction(&Instruction::F32Load(wasm_encoder::MemArg {
                            offset: byte_offset as u64,
                            align: 2,
                            memory_index: 0,
                        }));
                        byte_offset += 4;
                    }
                    ValType::F64 => {
                        f.instruction(&Instruction::F64Load(wasm_encoder::MemArg {
                            offset: byte_offset as u64,
                            align: 3,
                            memory_index: 0,
                        }));
                        byte_offset += 8;
                    }
                    _ => {
                        f.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
                            offset: byte_offset as u64,
                            align: 2,
                            memory_index: 0,
                        }));
                        byte_offset += 4;
                    }
                }
            }
        }
    }
}

/// Build a tiny core Wasm module that exports one 1-page memory as "mem".
/// When `with_realloc` is true, also exports a bump-allocator "realloc" function.
/// This memory is shared with the dispatch module for string passing and result buffers.
pub(super) fn build_mem_module(with_realloc: bool, bump_start: u32) -> Module {
    let mut module = Module::new();

    if with_realloc {
        // Type section (1): realloc signature (i32,i32,i32,i32)->i32
        let mut types = TypeSection::new();
        types.ty().function(
            [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            [ValType::I32],
        );
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
///   - `env/handler_f{i}`            (func, for each target function)
///   - async builtins from `env`     (if any async funcs present)
///
/// `event_ptr` is the memory offset reserved for `waitable-set.wait` event output.
/// `needs_realloc` is true when any async function has string params (canon lift async requires Realloc).
/// `bump_start` is the first free byte in linear memory for the bump allocator (after static data).
#[allow(clippy::too_many_arguments)]
pub(super) fn build_dispatch_module(
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
    //   slot 2+N..2+N+A-1: handler async call types (flat_params[,result_ptr]→i32)
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

    // Async handler call type indices, parallel to funcs (None for sync funcs).
    let mut async_ds_tys: Vec<Option<u32>> = vec![None; funcs.len()];
    // Sync-complex handler import type indices (None for simple/async).
    let mut sync_complex_handler_tys: Vec<Option<u32>> = vec![None; funcs.len()];
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

        // wrapper types (what canon lift expects from our exported wrapper):
        //   - async: (flat_params...) -> void
        //   - sync simple: (flat_params...) -> (flat_results...)
        //   - sync complex: (flat_params...) -> (i32)  [returns result ptr]
        for func in funcs {
            ty_idx += 1;
            if func.is_async {
                types.ty().function(func.core_params.iter().copied(), []);
            } else if func.result_is_complex {
                // canon lift for multi-value results: the core function
                // returns a single i32 — a pointer to the flat results
                // in linear memory. The runtime reads from there.
                types
                    .ty()
                    .function(func.core_params.iter().copied(), [ValType::I32]);
            } else {
                types.ty().function(
                    func.core_params.iter().copied(),
                    func.core_results.iter().copied(),
                );
            }
        }

        // Sync-complex handler import types: canon lower uses the
        // retptr pattern (extra i32 param, void return) — different
        // from the wrapper type which returns an i32 pointer.
        for (i, func) in funcs.iter().enumerate() {
            if !func.is_async && func.result_is_complex {
                sync_complex_handler_tys[i] = Some(ty_idx);
                ty_idx += 1;
                let mut params: Vec<ValType> = func.core_params.clone();
                params.push(ValType::I32); // retptr
                types.ty().function(params.iter().copied(), []);
            }
        }

        if has_async_machinery {
            // Async handler call types: (flat_params..., result_ptr?) -> i32
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
            // No further reads of ty_idx after this point — the per-function
            // custom task.return types live at indices `void_void_ty + 1 + N`
            // and are tracked separately in the import phase via
            // `custom_tr_ty_idx` (see line ~515).
            types.ty().function([], []);

            // Per-function custom task.return types for multi-value results.
            // Only emitted for async funcs with result_is_complex (>1 flat value).
            for func in funcs.iter() {
                if func.is_async && func.result_is_complex {
                    types.ty().function(func.core_results.iter().copied(), []);
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
    let handler_import_fn_base: u32;
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

        // Handler funcs — type depends on calling convention:
        //   - sync simple: same as wrapper type (single-value or void)
        //   - sync complex: separate type (canon lower uses retptr
        //     pattern: params + retptr → void; wrapper returns ptr)
        //   - async: separate async_ds_ty (canon lower async)
        handler_import_fn_base = fn_idx;
        for (i, func) in funcs.iter().enumerate() {
            let ty = if func.is_async {
                async_ds_tys[i].expect("async_ds_ty must be set for async func")
            } else if let Some(handler_ty) = sync_complex_handler_tys[i] {
                handler_ty
            } else {
                wrapper_ty_base + i as u32
            };
            imports.import("env", &format!("handler_f{i}"), EntityType::Function(ty));
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
        wrapper_fn_base = handler_import_fn_base
            + funcs.len() as u32
            + if has_async_machinery {
                // waitable_new, join, wait, drop, subtask_drop + one task.return per async func
                5 + funcs.iter().filter(|f| f.is_async).count() as u32
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
                     `should-block-call`. Tier-1 blocking is only supported for \
                     void-returning functions because the adapter cannot synthesize \
                     a return value when the call is blocked. Tier-3 (read-write \
                     interception) will support this in the future.",
                    func.name
                );
            }
            // Locals beyond params: [subtask: i32, ws: i32] for
            // async/hook machinery. Sync-complex wrappers don't need
            // extra locals — the result buffer is at a fixed address.
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
                             exports `should-block-call`. Tier-1 blocking is not supported \
                             for async functions with results. Tier-3 (read-write \
                             interception) will support this in the future.",
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
                // 3. Call async-lowered handler: (flat_params...[, result_ptr]) -> subtask
                let handler_fn = handler_import_fn_base + fi as u32;
                for (pi, _) in func.core_params.iter().enumerate() {
                    f.instruction(&Instruction::LocalGet(pi as u32));
                }
                if let Some(result_ptr) = func.async_result_mem_offset {
                    f.instruction(&Instruction::I32Const(result_ptr as i32));
                }
                f.instruction(&Instruction::Call(handler_fn));
                f.instruction(&Instruction::LocalSet(subtask_local));
                emit_wait_loop!(f, subtask_local, ws_local);
            } else if func.result_is_complex {
                // ── Sync wrapper with complex result ──────────────────────────
                //
                // canon lift expects: `(core_params) -> (i32)` — the
                // wrapper returns a pointer to the flat results in
                // linear memory.
                //
                // canon lower produces: `(core_params, retptr: i32) -> ()`
                // — the handler takes an extra retptr and writes the
                // flat results there.
                //
                // The wrapper calls the handler with a known result-
                // buffer address and returns that address.
                let handler_fn = handler_import_fn_base + fi as u32;
                let result_buf = func
                    .sync_result_mem_offset
                    .expect("sync_result_mem_offset must be set for sync complex");

                // 3. Call handler with params + result buffer address
                for (pi, _) in func.core_params.iter().enumerate() {
                    f.instruction(&Instruction::LocalGet(pi as u32));
                }
                f.instruction(&Instruction::I32Const(result_buf as i32));
                f.instruction(&Instruction::Call(handler_fn));
                // Handler wrote flat results at result_buf.
            } else {
                // ── Sync wrapper (single-value or void result) ────────────────
                if has_after && has_result {
                    result_local_idx = Some(func.core_params.len() as u32);
                }

                // 3. Call handler
                let handler_fn = handler_import_fn_base + fi as u32;
                for (pi, _) in func.core_params.iter().enumerate() {
                    f.instruction(&Instruction::LocalGet(pi as u32));
                }
                f.instruction(&Instruction::Call(handler_fn));

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
            } else if func.result_is_complex {
                // 5. Sync complex: return the result buffer pointer.
                let result_buf = func
                    .sync_result_mem_offset
                    .expect("sync_result_mem_offset must be set for sync complex");
                f.instruction(&Instruction::I32Const(result_buf as i32));
            } else {
                // 5. Sync single-value: return the result (if any).
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
