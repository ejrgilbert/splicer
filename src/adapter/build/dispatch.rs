//! Core-Wasm modules nested inside the tier-1 adapter component:
//! the **memory module** and the **dispatch module**.
//!
//! ## Why a tier-1 adapter needs core Wasm at all
//!
//! The adapter sits between a caller and a handler, both of which
//! speak component-level values (strings, records, resources, …).
//! Those values can't be passed directly between core-Wasm functions
//! — core Wasm only knows i32/i64/f32/f64. The component model's
//! **canonical ABI** bridges the two by copying the bytes of a
//! component value through a linear memory and representing them at
//! the core level as `(ptr, len, …)` tuples.
//!
//! So even though the adapter's *user-visible* surface is entirely
//! component-level (it imports the handler + hook interfaces and
//! exports the target interface), the wiring underneath is two core
//! Wasm modules:
//!
//! - [`build_mem_module`] — a tiny core module that exports a 1-page
//!   linear memory (and, when any func has a string/list/record/
//!   variant/resource, a bump-allocator `realloc`) under the names
//!   `mem` / `realloc`. This is the scratch buffer the canonical-ABI
//!   lift/lower options (`memory $0` / `realloc $f`) write into and
//!   read out of — the *same* memory on both sides of the dispatch
//!   module, so a lowered param arg and a later lifted result both
//!   live in the same address space. Bump allocation is enough
//!   because the adapter's memory lives for one invocation and then
//!   the instance tears down.
//!
//! - [`build_dispatch_module`] — the per-function wrapper bodies.
//!   When the outside world calls the adapter's exported `handle`,
//!   the outer Component's `canon lift` copies the caller's args into
//!   `mem` and invokes the wrapper. The wrapper:
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
//! ## "Async" is just an async-shaped canon lift/lower
//!
//! Nothing in the dispatch module cares about async vs sync at the
//! memory level — both variants pass args/results through `mem` the
//! same way. The async shape differs only in the *control flow*: the
//! canon-lowered handler import returns a packed subtask handle that
//! the wrapper has to await (via `waitable-set.wait` or similar), and
//! the wrapper produces its final result via `task.return` instead of
//! a plain function return. The memory/realloc machinery is
//! identical either way.
//!
//! Both builders return raw core-Wasm bytes; the component-builder
//! wraps them in a `ModuleSection` of the outer Component.

use wasm_encoder::{
    BlockType, CodeSection, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, Instruction, MemoryType, Module, TypeSection, ValType,
};

use crate::adapter::abi::{WasmEncoderBindgen, WitBridge};
use crate::adapter::func::AdapterFunc;
use crate::adapter::indices::{DispatchIndices, FunctionIndices};
use crate::adapter::names;
use wit_bindgen_core::abi::lift_from_memory;

/// Pre-compute the wasm instructions that load a single async-result
/// value from `result_ptr` into the joined-flat representation on the
/// wasm stack, ready for `task.return`. Allocates any locals the
/// generator needs via the shared [`FunctionIndices`] so they can be
/// declared on the target `Function` before emission.
///
/// Returns the accumulated instruction sequence — a prefix that stashes
/// `result_ptr` into a freshly-allocated local, followed by the loads
/// that `wit_bindgen_core::abi::lift_from_memory` emits against that
/// local. The caller flushes this at the task-return point in
/// [`emit_return_phase`].
fn build_task_return_loads(
    result_ptr: u32,
    result_type_id: cviz::model::ValueTypeId,
    bridge: &WitBridge,
    indices: &mut FunctionIndices,
) -> Vec<Instruction<'static>> {
    let addr_local = indices.alloc_local(ValType::I32);
    let result_type = bridge.get(result_type_id);

    let mut out: Vec<Instruction<'static>> = vec![
        Instruction::I32Const(result_ptr as i32),
        Instruction::LocalSet(addr_local),
    ];

    let mut bindgen = WasmEncoderBindgen::new(&bridge.sizes, addr_local, indices);
    lift_from_memory(&bridge.resolve, &mut bindgen, (), &result_type);
    out.extend(bindgen.into_instructions());
    out
}

/// Build the **memory-provider** core module: one 1-page linear memory
/// exported as `mem`, plus — when `with_realloc` — a bump-allocator
/// `realloc` with the canonical-ABI signature
/// `(old_ptr, old_size, align, new_size) -> new_ptr`.
///
/// ## When realloc is needed
///
/// The canonical ABI calls `realloc` whenever it needs scratch space
/// for a value that can't fit in core args directly — strings, lists,
/// records, variants, resource handles. Primitive-only signatures
/// (`s32 + s32 -> s32`, `bool`, `f64`, …) pass entirely on the core
/// value stack and don't need a realloc at all, so the adapter emits
/// a memory-only module in that case (`with_realloc = false`).
///
/// ## When realloc fires during an invocation
///
/// - **On call-in** (`canon lift` of the adapter's exports): the
///   caller hands in component-level args, the lift copies string/
///   list bodies into `mem` via `realloc`, then invokes the
///   dispatch-module wrapper with core (ptr, len, …) tuples.
/// - **On call-out** (`canon lower` of the adapter's handler/hook
///   imports): the wrapper hands core (ptr, len, …) to the import,
///   the lower reads the bytes out of `mem`, calls the
///   component-level import, then copies any component-level result
///   back into `mem` via `realloc` for the wrapper to read.
///
/// ## Why bump allocation suffices
///
/// Every adapter instance lives for a single top-level call (the
/// inbound `handle`), and then the instance is torn down along with
/// its memory. So the allocator never needs to free — it just hands
/// out fresh aligned chunks and advances a pointer. `old_ptr` and
/// `old_size` are accepted (the canonical ABI insists on the full
/// signature) but ignored.
pub(crate) fn build_mem_module(with_realloc: bool, bump_start: u32) -> Module {
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
        exports.export(names::ENV_MEMORY, ExportKind::Memory, 0);
        if with_realloc {
            exports.export(names::ENV_REALLOC, ExportKind::Func, 0);
        }
        module.section(&exports);
    }

    if with_realloc {
        // ── Code section (10): bump allocator body ─────────────────
        //
        // Locals / params:
        //   local 0 = old_ptr    (ignored — we never "realloc")
        //   local 1 = old_size   (ignored)
        //   local 2 = align      (power of 2)
        //   local 3 = new_size
        //   local 4 = scratch for the aligned pointer
        //   global 0 = bump pointer
        //
        // Pseudocode:
        //   aligned  = (bump_ptr + (align - 1)) & ~(align - 1)
        //   bump_ptr = aligned + new_size
        //   return aligned
        //
        // The `+ (align - 1)` before the mask is the round-UP trick
        // — without it, ANDing with the mask would round *down* for
        // values that aren't already aligned.
        let mut code_section = CodeSection::new();
        let mut rf = Function::new(vec![(1u32, ValType::I32)]); // local 4: aligned

        // mask = ~(align - 1) = (align - 1) ^ -1
        rf.instruction(&Instruction::LocalGet(2)); // align
        rf.instruction(&Instruction::I32Const(1));
        rf.instruction(&Instruction::I32Sub);
        rf.instruction(&Instruction::I32Const(-1));
        rf.instruction(&Instruction::I32Xor); // ~(align - 1)

        // bump_ptr + (align - 1)
        rf.instruction(&Instruction::GlobalGet(0));
        rf.instruction(&Instruction::LocalGet(2));
        rf.instruction(&Instruction::I32Const(1));
        rf.instruction(&Instruction::I32Sub);
        rf.instruction(&Instruction::I32Add);

        // aligned = (bump_ptr + align - 1) & mask   ;; round up to align
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

// ─── Type-section emitters ─────────────────────────────────────────────────
//
// Each helper appends one logical group of types to the dispatch
// module's TypeSection and bumps `indices.ty` accordingly. They must
// be called in the order below — wasm type indices are positional
// and downstream imports/wrappers reference exact slots
// (`wrapper_ty_base + i`, etc.).

/// Emit one wrapper function type per target func, in contiguous
/// index order. Wrapper type shape depends on the calling convention:
///
/// - **async**: `(flat_params…) -> ()` — async canon lift returns
///   nothing, the wrapper produces its result via `task.return`.
/// - **sync complex** (multi-value result): `(flat_params…) -> (i32)`
///   — the wrapper returns a pointer to the flat results in linear
///   memory for canon lift to read from.
/// - **sync simple**: `(flat_params…) -> (flat_results…)` — the
///   straight-through single-value / void case.
fn emit_wrapper_types(
    types: &mut TypeSection,
    indices: &mut DispatchIndices,
    funcs: &[AdapterFunc],
) {
    for func in funcs {
        if func.is_async {
            types.ty().function(func.core_params.iter().copied(), []);
        } else if func.result_is_complex {
            types
                .ty()
                .function(func.core_params.iter().copied(), [ValType::I32]);
        } else {
            types.ty().function(
                func.core_params.iter().copied(),
                func.core_results.iter().copied(),
            );
        }
        indices.alloc_ty();
    }
}

/// Emit a retptr-pattern handler import type for each sync func with
/// a complex (multi-value) result. Canon lower uses
/// `(core_params…, retptr) -> ()` here, distinct from the wrapper's
/// `(core_params…) -> (i32)`. Records the assigned type index in
/// `out[i]` for sync-complex funcs; other entries are left untouched.
fn emit_sync_complex_handler_types(
    types: &mut TypeSection,
    indices: &mut DispatchIndices,
    funcs: &[AdapterFunc],
    out: &mut [Option<u32>],
) {
    for (i, func) in funcs.iter().enumerate() {
        if !func.is_async && func.result_is_complex {
            out[i] = Some(indices.alloc_ty());
            let mut params: Vec<ValType> = func.core_params.clone();
            params.push(ValType::I32); // retptr
            types.ty().function(params, []);
        }
    }
}

/// Emit an async-lowered handler call type for each async func:
/// `(core_params…, result_ptr?) -> (i32)` where `result_ptr` is
/// appended only when the func has a non-void result and the return
/// value is a packed subtask handle the wrapper awaits via the
/// waitable-set machinery. Records the assigned type index in
/// `out[i]` for async funcs.
fn emit_async_handler_types(
    types: &mut TypeSection,
    indices: &mut DispatchIndices,
    funcs: &[AdapterFunc],
    out: &mut [Option<u32>],
) {
    for (i, func) in funcs.iter().enumerate() {
        if func.is_async {
            out[i] = Some(indices.alloc_ty());
            let mut params: Vec<ValType> = func.core_params.clone();
            if func.result_type_id.is_some() {
                params.push(ValType::I32); // result_ptr
            }
            types.ty().function(params, [ValType::I32]);
        }
    }
}

/// Emit one `(core_results…) -> ()` task.return type per async func
/// whose result has >1 flat value. Single-value and void async funcs
/// use the shared `void_i32_ty` / `void_void_ty` slots emitted
/// earlier, so they don't appear here.
fn emit_custom_task_return_types(types: &mut TypeSection, funcs: &[AdapterFunc]) {
    for func in funcs {
        if func.is_async && func.result_is_complex {
            types.ty().function(func.core_results.iter().copied(), []);
        }
    }
}

// ─── Import-section emitters ───────────────────────────────────────────────

/// Import `env/handler_f{i}` for each target func, picking the type
/// slot that matches its calling convention (sync-simple reuses the
/// wrapper type; sync-complex uses its retptr type; async uses its
/// async-lowered type). Allocates `funcs.len()` function indices via
/// `indices.alloc_func()` and returns the base so the caller can
/// reference individual handler imports as `base + i`.
#[allow(clippy::too_many_arguments)]
fn emit_handler_imports(
    imports: &mut ImportSection,
    indices: &mut DispatchIndices,
    funcs: &[AdapterFunc],
    wrapper_ty_base: u32,
    async_ds_tys: &[Option<u32>],
    sync_complex_handler_tys: &[Option<u32>],
) -> u32 {
    let base = indices.func;
    for (i, func) in funcs.iter().enumerate() {
        let ty = if func.is_async {
            async_ds_tys[i].expect("async_ds_ty must be set for async func")
        } else if let Some(handler_ty) = sync_complex_handler_tys[i] {
            handler_ty
        } else {
            wrapper_ty_base + i as u32
        };
        indices.alloc_func();
        imports.import(
            names::ENV_INSTANCE,
            &names::env_handler_fn(i),
            EntityType::Function(ty),
        );
    }
    base
}

/// Import `env/task_return_f{i}` for each async func. Returns a vec
/// paralleling `funcs` where index `i` is `Some(fn_idx)` for async
/// funcs and `None` otherwise. Type selection:
///
/// - Void async:        `void_void_ty` — `() -> ()`
/// - Single-value async: matching `void_{i32,i64,f32,f64}_ty`
/// - Multi-value async: per-func custom type living right after
///   `void_void_ty` (see [`emit_custom_task_return_types`]), assigned
///   in the same order async-complex funcs appear in `funcs`.
#[allow(clippy::too_many_arguments)]
fn emit_task_return_imports(
    imports: &mut ImportSection,
    indices: &mut DispatchIndices,
    funcs: &[AdapterFunc],
    void_void_ty: u32,
    void_i32_ty: u32,
    void_i64_ty: u32,
    void_f32_ty: u32,
    void_f64_ty: u32,
) -> Vec<Option<u32>> {
    let mut trf: Vec<Option<u32>> = vec![None; funcs.len()];
    let mut custom_tr_ty_idx = void_void_ty + 1;
    for (i, func) in funcs.iter().enumerate() {
        if !func.is_async {
            continue;
        }
        let tr_ty = if func.result_type_id.is_none() {
            void_void_ty
        } else if func.result_is_complex {
            let ty = custom_tr_ty_idx;
            custom_tr_ty_idx += 1;
            ty
        } else {
            match func.core_results.first() {
                Some(ValType::I32) => void_i32_ty,
                Some(ValType::I64) => void_i64_ty,
                Some(ValType::F32) => void_f32_ty,
                Some(ValType::F64) => void_f64_ty,
                _ => void_i32_ty,
            }
        };
        trf[i] = Some(indices.alloc_func());
        imports.import(
            names::ENV_INSTANCE,
            &names::env_task_return_fn(i),
            EntityType::Function(tr_ty),
        );
    }
    trf
}

// ─── Code-section emitters ─────────────────────────────────────────────────

/// Function indices + scratch offset needed by [`emit_wait_loop`] and
/// [`emit_wrapper_body`]. Bundling them keeps the phase helpers from
/// each growing a list of near-identical handle-dropper / event-ptr
/// parameters.
struct WaitLoopCtx {
    waitable_new_fn: u32,
    waitable_join_fn: u32,
    waitable_wait_fn: u32,
    subtask_drop_fn: u32,
    waitable_drop_fn: u32,
    /// Memory offset that `waitable-set.wait` writes its completion
    /// event into. The wrapper allocates this once in `extract_adapter_funcs`
    /// and reuses it across every awaited subtask.
    event_ptr: u32,
}

/// Await a packed return value from `canon lower async` currently
/// stored in local `st` (subtask). Uses local `ws` (waitable-set) as
/// scratch. After this helper returns, the instruction stream has:
///
/// - Extracted the raw subtask handle (`packed >> 4`) into `st`.
/// - If the handle is nonzero (task still pending): created a new
///   waitable-set, joined the subtask to it, waited until the
///   `event_ptr` buffer is populated, then dropped both the subtask
///   and the waitable-set.
///
/// `canon lower async` returns a packed i32: low 4 bits are the
/// Status tag (`Returned=2` means sync-done; `Started=1` means
/// pending) and the upper 28 bits hold the raw subtask handle
/// (`0` when sync-done). Shifting right by 4 discards the tag.
fn emit_wait_loop(f: &mut Function, st: u32, ws: u32, ctx: &WaitLoopCtx) {
    // Extract raw handle from packed value: handle = packed >> 4
    f.instruction(&Instruction::LocalGet(st));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32ShrU);
    f.instruction(&Instruction::LocalSet(st));
    // If handle != 0 (task is still pending), wait for it.
    f.instruction(&Instruction::LocalGet(st));
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::Call(ctx.waitable_new_fn));
    f.instruction(&Instruction::LocalSet(ws));
    // waitable.join(waitable_handle, set_handle)
    f.instruction(&Instruction::LocalGet(st));
    f.instruction(&Instruction::LocalGet(ws));
    f.instruction(&Instruction::Call(ctx.waitable_join_fn));
    f.instruction(&Instruction::LocalGet(ws));
    f.instruction(&Instruction::I32Const(ctx.event_ptr as i32));
    f.instruction(&Instruction::Call(ctx.waitable_wait_fn));
    f.instruction(&Instruction::Drop);
    // Drop subtask first (it is a child of ws in the resource table).
    f.instruction(&Instruction::LocalGet(st));
    f.instruction(&Instruction::Call(ctx.subtask_drop_fn));
    f.instruction(&Instruction::LocalGet(ws));
    f.instruction(&Instruction::Call(ctx.waitable_drop_fn));
    f.instruction(&Instruction::End);
}

/// Everything the code-section phase helpers thread through on every
/// wrapper body they build. Populated once in [`build_dispatch_module`]
/// after the types/imports sections finish, then passed by reference
/// into [`emit_wrapper_body`].
struct DispatchCodeCtx<'a> {
    before_import_fn: Option<u32>,
    after_import_fn: Option<u32>,
    blocking: Option<BlockingConfig>,
    handler_import_fn_base: u32,
    /// `task_return_fns[i]` is the `task_return_f{i}` import index for
    /// async func `i`, or `None` for sync funcs.
    task_return_fns: &'a [Option<u32>],
    wait: WaitLoopCtx,
}

/// Present only when the middleware exports `splicer:tier1/blocking`.
struct BlockingConfig {
    import_fn: u32,
    result_ptr: u32,
}

/// Emit the full wrapper body for func `fi`, including all five
/// phases (before, blocking, handler call, after, return) and the
/// final `End` opcode.
fn emit_wrapper_body(
    f: &mut Function,
    fi: usize,
    func: &AdapterFunc,
    ctx: &DispatchCodeCtx<'_>,
    subtask_local: u32,
    ws_local: u32,
    task_return_loads: Option<&[Instruction<'static>]>,
) -> anyhow::Result<()> {
    let has_result = func.result_type_id.is_some();

    // Blocking + non-void sync is rejected — the adapter can't
    // fabricate a replacement return value when the call is skipped.
    // Blocking + async-with-result hits the same wall inside
    // emit_blocking_phase below.
    if ctx.blocking.is_some() && has_result && !func.is_async {
        anyhow::bail!(
            "Function '{}' returns a value but the middleware exports \
             `should-block-call`. Tier-1 blocking is only supported for \
             void-returning functions because the adapter cannot synthesize \
             a return value when the call is blocked. Tier-3 (read-write \
             interception) will support this in the future.",
            func.name
        );
    }

    emit_before_phase(f, func, ctx, subtask_local, ws_local);
    emit_blocking_phase(f, fi, func, ctx, subtask_local, ws_local)?;
    let result_local_idx = emit_handler_call_phase(f, fi, func, ctx, subtask_local, ws_local);
    emit_after_phase(f, func, ctx, subtask_local, ws_local);
    emit_return_phase(f, fi, func, ctx, result_local_idx, task_return_loads);

    f.instruction(&Instruction::End);
    Ok(())
}

/// Phase 1: call `before_call(name_ptr, name_len)` and await the
/// returned subtask handle. No-op when the middleware doesn't export
/// `splicer:tier1/before`.
fn emit_before_phase(
    f: &mut Function,
    func: &AdapterFunc,
    ctx: &DispatchCodeCtx<'_>,
    subtask_local: u32,
    ws_local: u32,
) {
    if let Some(before_fn) = ctx.before_import_fn {
        f.instruction(&Instruction::I32Const(func.name_offset as i32));
        f.instruction(&Instruction::I32Const(func.name_len as i32));
        f.instruction(&Instruction::Call(before_fn));
        f.instruction(&Instruction::LocalSet(subtask_local));
        emit_wait_loop(f, subtask_local, ws_local, &ctx.wait);
    }
}

/// Phase 2: call `should_block_call(name_ptr, name_len, result_ptr)`,
/// await it, read the bool at `result_ptr`, and `return` early (after
/// calling `task.return` for async-void) if the middleware said to
/// block. No-op when the middleware doesn't export
/// `splicer:tier1/blocking`.
///
/// Errors if the target func is async with a non-void result — tier-1
/// blocking can only synthesize a return for void funcs. Tier-3 is
/// the path for "block + synthesize a result".
fn emit_blocking_phase(
    f: &mut Function,
    fi: usize,
    func: &AdapterFunc,
    ctx: &DispatchCodeCtx<'_>,
    subtask_local: u32,
    ws_local: u32,
) -> anyhow::Result<()> {
    let Some(BlockingConfig {
        import_fn: block_fn,
        result_ptr: blk_ptr,
    }) = ctx.blocking
    else {
        return Ok(());
    };
    if func.result_type_id.is_some() {
        anyhow::bail!(
            "Function '{}' is async with a return value and the middleware \
             exports `should-block-call`. Tier-1 blocking is not supported \
             for async functions with results. Tier-3 (read-write \
             interception) will support this in the future.",
            func.name
        );
    }
    f.instruction(&Instruction::I32Const(func.name_offset as i32));
    f.instruction(&Instruction::I32Const(func.name_len as i32));
    f.instruction(&Instruction::I32Const(blk_ptr as i32));
    f.instruction(&Instruction::Call(block_fn));
    f.instruction(&Instruction::LocalSet(subtask_local));
    emit_wait_loop(f, subtask_local, ws_local, &ctx.wait);
    // Load the bool result and conditionally return (block the call).
    // Reads slot 0 of the canonical-ABI block-result record reserved
    // in memory by `MemoryLayoutBuilder::alloc_block_result` — see
    // `mem_layout::BLOCK_RESULT_SHAPE` for the record layout.
    f.instruction(&Instruction::I32Const(blk_ptr as i32));
    f.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::If(BlockType::Empty));
    // Async-lifted functions MUST call task.return before returning.
    // For void async: task.return with no args.
    // (Blocking with a result-returning async function is rejected above.)
    if let Some(tr_fn) = ctx.task_return_fns[fi] {
        f.instruction(&Instruction::Call(tr_fn));
    }
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    Ok(())
}

/// Phase 3: call the target func's handler import. Three shapes:
///
/// - **async**: params (+ optional `result_ptr`) → subtask handle,
///   awaited via [`emit_wait_loop`].
/// - **sync complex** (multi-value result): params + `retptr` → `()`;
///   the handler writes flat results at the adapter's pre-reserved
///   result buffer, which phase 5 returns.
/// - **sync simple**: params → flat results; when an after-call hook
///   will fire before the return, stash the result into a local.
///
/// Returns `Some(local_idx)` when phase 5 needs to re-push the stashed
/// sync-simple result (only when `after_call` is present and the func
/// has a result), `None` otherwise.
fn emit_handler_call_phase(
    f: &mut Function,
    fi: usize,
    func: &AdapterFunc,
    ctx: &DispatchCodeCtx<'_>,
    subtask_local: u32,
    ws_local: u32,
) -> Option<u32> {
    let handler_fn = ctx.handler_import_fn_base + fi as u32;

    if func.is_async {
        // async: (flat_params...[, result_ptr]) -> subtask
        for (pi, _) in func.core_params.iter().enumerate() {
            f.instruction(&Instruction::LocalGet(pi as u32));
        }
        if let Some(result_ptr) = func.async_result_mem_offset {
            f.instruction(&Instruction::I32Const(result_ptr as i32));
        }
        f.instruction(&Instruction::Call(handler_fn));
        f.instruction(&Instruction::LocalSet(subtask_local));
        emit_wait_loop(f, subtask_local, ws_local, &ctx.wait);
        None
    } else if func.result_is_complex {
        // sync complex: canon lift expects `(core_params) -> (i32)`
        // and canon lower produces `(core_params, retptr: i32) -> ()`.
        // We call the handler with a pre-reserved result-buffer
        // address; phase 5 returns that address.
        let result_buf = func
            .sync_result_mem_offset
            .expect("sync_result_mem_offset must be set for sync complex");
        for (pi, _) in func.core_params.iter().enumerate() {
            f.instruction(&Instruction::LocalGet(pi as u32));
        }
        f.instruction(&Instruction::I32Const(result_buf as i32));
        f.instruction(&Instruction::Call(handler_fn));
        None
    } else {
        // sync simple: call handler, stash result if after-call will
        // fire before the return (otherwise leave it on the stack).
        let result_local = if ctx.after_import_fn.is_some() && func.result_type_id.is_some() {
            Some(func.core_params.len() as u32)
        } else {
            None
        };

        for (pi, _) in func.core_params.iter().enumerate() {
            f.instruction(&Instruction::LocalGet(pi as u32));
        }
        f.instruction(&Instruction::Call(handler_fn));

        if let Some(local_idx) = result_local {
            f.instruction(&Instruction::LocalSet(local_idx));
        }
        result_local
    }
}

/// Phase 4: call `after_call(name_ptr, name_len)` and await the
/// returned subtask handle. No-op when the middleware doesn't export
/// `splicer:tier1/after`.
fn emit_after_phase(
    f: &mut Function,
    func: &AdapterFunc,
    ctx: &DispatchCodeCtx<'_>,
    subtask_local: u32,
    ws_local: u32,
) {
    if let Some(after_fn) = ctx.after_import_fn {
        f.instruction(&Instruction::I32Const(func.name_offset as i32));
        f.instruction(&Instruction::I32Const(func.name_len as i32));
        f.instruction(&Instruction::Call(after_fn));
        f.instruction(&Instruction::LocalSet(subtask_local));
        emit_wait_loop(f, subtask_local, ws_local, &ctx.wait);
    }
}

/// Phase 5: produce the wrapper's return value. Three shapes,
/// mirroring phase 3:
///
/// - **async**: call `task.return`, loading flat result values from
///   `async_result_mem_offset` first for non-void results (via
///   [`emit_task_return_loads`] for multi-value results). Async lifts
///   MUST call `task.return` before the function's `End`, even for
///   void results.
/// - **sync complex**: push the result-buffer pointer as the wrapper's
///   single i32 return.
/// - **sync simple**: re-push the stashed result local (if any) so
///   canon lift sees the original handler return.
fn emit_return_phase(
    f: &mut Function,
    fi: usize,
    func: &AdapterFunc,
    ctx: &DispatchCodeCtx<'_>,
    result_local_idx: Option<u32>,
    task_return_loads: Option<&[Instruction<'static>]>,
) {
    if func.is_async {
        if func.async_result_mem_offset.is_some() {
            if let Some(tr_fn) = ctx.task_return_fns[fi] {
                // Flush the pre-built load sequence produced by
                // [`build_task_return_loads`] — it stashes result_ptr
                // into a local and drives `lift_from_memory` to push
                // the joined flat values on the stack. Same sequence
                // handles simple single-flat-value and complex
                // multi-value results uniformly.
                let loads = task_return_loads
                    .expect("async-with-result wrapper must have pre-built task_return_loads");
                for inst in loads {
                    f.instruction(inst);
                }
                f.instruction(&Instruction::Call(tr_fn));
            }
        } else if let Some(tr_fn) = ctx.task_return_fns[fi] {
            // Void async: task.return with no args (still required before End).
            f.instruction(&Instruction::Call(tr_fn));
        }
    } else if func.result_is_complex {
        let result_buf = func
            .sync_result_mem_offset
            .expect("sync_result_mem_offset must be set for sync complex");
        f.instruction(&Instruction::I32Const(result_buf as i32));
    } else if let Some(local_idx) = result_local_idx {
        f.instruction(&Instruction::LocalGet(local_idx));
    }
}

/// Emit the function-name blob as an active data segment starting at
/// memory offset 0. Each [`AdapterFunc`] has `name_offset` / `name_len`
/// pre-computed against this layout, so the wrappers can pass
/// `(name_ptr, name_len)` to the hooks as raw memory slices.
fn emit_function_name_data(module: &mut Module, funcs: &[AdapterFunc]) {
    let mut data_section = DataSection::new();
    let all_names: Vec<u8> = funcs
        .iter()
        .flat_map(|f| f.name.as_bytes().iter().copied())
        .collect();
    data_section.active(0, &wasm_encoder::ConstExpr::i32_const(0), all_names);
    module.section(&data_section);
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
pub(crate) fn build_dispatch_module(
    funcs: &[AdapterFunc],
    has_before: bool,
    has_after: bool,
    has_blocking: bool,
    event_ptr: u32,
    block_result_ptr: Option<u32>,
    bridge: &WitBridge,
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

    // Single index allocator for both types and functions. Threaded
    // through every type/import emitter so no helper owns its own
    // counter.
    let mut indices = DispatchIndices::new();

    let hook_ty: u32;
    let block_ty: u32;
    let wrapper_ty_base: u32;

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

    {
        let mut types = TypeSection::new();

        // slot 0: async-lowered hook (before/after): (ptr, len) -> subtask_handle.
        // This is canon lower of `async func(name: string)` from
        // `wit/tier1/world.wit` — string flattens to (ptr, len) and
        // the async lowering adds the subtask handle return. If the
        // tier-1 hook signatures change in the WIT, update both this
        // type and the matching component-level InstanceType in
        // `component::emit_hook_inst_types`.
        hook_ty = indices.alloc_ty();
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);

        // slot 1: async-lowered block (should-block-call):
        // (ptr, len, result_ptr) -> subtask_handle. Canon lower of
        // `async func(name: string) -> bool` — bool result gets the
        // async retptr form (the extra i32). Same WIT-source caveat
        // as the slot-0 hook type above.
        block_ty = indices.alloc_ty();
        types
            .ty()
            .function([ValType::I32, ValType::I32, ValType::I32], [ValType::I32]);

        // Wrapper types (slots 2..2+N), then sync-complex handler
        // import types, then — only when any async/hook machinery is
        // in play — async handler types + the shared async builtin
        // types + per-func custom task.return types.
        wrapper_ty_base = indices.ty;
        emit_wrapper_types(&mut types, &mut indices, funcs);
        emit_sync_complex_handler_types(
            &mut types,
            &mut indices,
            funcs,
            &mut sync_complex_handler_tys,
        );

        if has_async_machinery {
            emit_async_handler_types(&mut types, &mut indices, funcs, &mut async_ds_tys);

            waitable_new_ty = indices.alloc_ty();
            types.ty().function([], [ValType::I32]);

            waitable_join_ty = indices.alloc_ty();
            types.ty().function([ValType::I32, ValType::I32], []);

            waitable_wait_ty = indices.alloc_ty();
            types
                .ty()
                .function([ValType::I32, ValType::I32], [ValType::I32]);

            void_i32_ty = indices.alloc_ty();
            types.ty().function([ValType::I32], []);

            void_i64_ty = indices.alloc_ty();
            types.ty().function([ValType::I64], []);

            void_f32_ty = indices.alloc_ty();
            types.ty().function([ValType::F32], []);

            void_f64_ty = indices.alloc_ty();
            types.ty().function([ValType::F64], []);

            void_void_ty = indices.alloc_ty();
            // The per-function custom task.return types live at
            // indices `void_void_ty + 1 + N` and are tracked
            // separately in the import phase via `custom_tr_ty_idx`
            // inside emit_task_return_imports.
            types.ty().function([], []);

            emit_custom_task_return_types(&mut types, funcs);
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
        // Names come from `super::names` (splicer-internal slots) and
        // `crate::contract` (WIT-derived hook env-slot names). Each
        // string matches an export declared on the outer component's
        // env core instance — see `component::emit_dispatch_phase`.
        use crate::contract::{
            TIER1_AFTER_ENV_SLOTS, TIER1_BEFORE_ENV_SLOTS, TIER1_BLOCKING_ENV_SLOTS,
        };

        let mut imports = ImportSection::new();

        // Memory import doesn't consume a function-index slot.
        imports.import(
            names::ENV_INSTANCE,
            names::ENV_MEMORY,
            EntityType::Memory(MemoryType {
                minimum: 1,
                maximum: None,
                memory64: false,
                shared: false,
                page_size_log2: None,
            }),
        );

        before_import_fn = if has_before {
            let idx = indices.alloc_func();
            imports.import(
                names::ENV_INSTANCE,
                TIER1_BEFORE_ENV_SLOTS[0],
                EntityType::Function(hook_ty),
            );
            Some(idx)
        } else {
            None
        };

        after_import_fn = if has_after {
            let idx = indices.alloc_func();
            imports.import(
                names::ENV_INSTANCE,
                TIER1_AFTER_ENV_SLOTS[0],
                EntityType::Function(hook_ty),
            );
            Some(idx)
        } else {
            None
        };

        blocking_import_fn = if has_blocking {
            let idx = indices.alloc_func();
            imports.import(
                names::ENV_INSTANCE,
                TIER1_BLOCKING_ENV_SLOTS[0],
                EntityType::Function(block_ty),
            );
            Some(idx)
        } else {
            None
        };

        handler_import_fn_base = emit_handler_imports(
            &mut imports,
            &mut indices,
            funcs,
            wrapper_ty_base,
            &async_ds_tys,
            &sync_complex_handler_tys,
        );

        // Async builtins — only imported if needed.
        if has_async_machinery {
            waitable_new_fn = indices.alloc_func();
            imports.import(
                names::ENV_INSTANCE,
                names::ENV_WAITABLE_NEW,
                EntityType::Function(waitable_new_ty),
            );

            waitable_join_fn = indices.alloc_func();
            imports.import(
                names::ENV_INSTANCE,
                names::ENV_WAITABLE_JOIN,
                EntityType::Function(waitable_join_ty),
            );

            waitable_wait_fn = indices.alloc_func();
            imports.import(
                names::ENV_INSTANCE,
                names::ENV_WAITABLE_WAIT,
                EntityType::Function(waitable_wait_ty),
            );

            waitable_drop_fn = indices.alloc_func();
            imports.import(
                names::ENV_INSTANCE,
                names::ENV_WAITABLE_DROP,
                EntityType::Function(void_i32_ty),
            );

            subtask_drop_fn = indices.alloc_func();
            imports.import(
                names::ENV_INSTANCE,
                names::ENV_SUBTASK_DROP,
                EntityType::Function(void_i32_ty),
            );

            let trf = emit_task_return_imports(
                &mut imports,
                &mut indices,
                funcs,
                void_void_ty,
                void_i32_ty,
                void_i64_ty,
                void_f32_ty,
                void_f64_ty,
            );
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

    // ── Export section ────────────────────────────────────────────────────

    {
        let mut exports = ExportSection::new();
        for (i, func) in funcs.iter().enumerate() {
            exports.export(&func.name, ExportKind::Func, wrapper_fn_base + i as u32);
        }
        module.section(&exports);
    }

    // ── Code section ──────────────────────────────────────────────────────

    let blocking = blocking_import_fn
        .zip(block_result_ptr)
        .map(|(import_fn, result_ptr)| BlockingConfig {
            import_fn,
            result_ptr,
        });
    let code_ctx = DispatchCodeCtx {
        before_import_fn,
        after_import_fn,
        blocking,
        handler_import_fn_base,
        task_return_fns: &task_return_fns,
        wait: WaitLoopCtx {
            waitable_new_fn,
            waitable_join_fn,
            waitable_wait_fn,
            subtask_drop_fn,
            waitable_drop_fn,
            event_ptr,
        },
    };
    {
        let mut code_section = CodeSection::new();
        for (fi, func) in funcs.iter().enumerate() {
            // Every wrapper reserves two scratch i32 locals after its
            // params: subtask handle + waitable-set handle. Used by
            // the hook-await path; harmless (but unused) for
            // sync-simple funcs without any hooks.
            let mut indices = FunctionIndices::new(func.core_params.len() as u32);
            let subtask_local = indices.alloc_local(ValType::I32);
            let ws_local = indices.alloc_local(ValType::I32);

            // Pre-build the task.return load sequence for async funcs
            // with a non-void result. `lift_from_memory` allocates
            // additional locals into `indices` as needed (a disc local
            // per variant plus a local per joined-payload slot), so
            // this must run before Function construction so every
            // local is declared.
            let task_return_loads: Option<Vec<Instruction<'static>>> = match (
                func.is_async,
                func.async_result_mem_offset,
                func.result_type_id,
            ) {
                (true, Some(result_ptr), Some(rid)) => Some(build_task_return_loads(
                    result_ptr,
                    rid,
                    bridge,
                    &mut indices,
                )),
                _ => None,
            };

            let mut f = Function::new_with_locals_types(indices.into_locals());
            emit_wrapper_body(
                &mut f,
                fi,
                func,
                &code_ctx,
                subtask_local,
                ws_local,
                task_return_loads.as_deref(),
            )?;
            code_section.function(&f);
        }
        module.section(&code_section);
    }

    // ── Data section ──────────────────────────────────────────────────────

    emit_function_name_data(&mut module, funcs);

    Ok(module.finish().to_vec())
}
