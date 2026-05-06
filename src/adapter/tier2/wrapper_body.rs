//! Wrapper-body emit: builds the wasm function body for one
//! exported wrapper. Drives the four-phase shape (build call-id
//! and on-call → call handler → on-return → tail / `task.return`)
//! and threads the schema-layout addresses + lift codegen helpers.
//!
//! Wrapper body shape:
//!
//! ```text
//! ;; build call-id flat: (iface_ptr, iface_len, fn_ptr, fn_len)
//! i32.const iface_offset
//! i32.const iface_len
//! i32.const fn_offset
//! i32.const fn_len
//! ;; empty list<field> args (ptr=0, len=0)
//! i32.const 0
//! i32.const 0
//! call $on_call               ;; canon-lower-async — returns packed (handle<<4)|status
//! local.set $st
//! ;; wait loop (only if subtask didn't return synchronously)
//! local.get $st
//! i32.const 4
//! i32.shr_u
//! local.set $st               ;; raw subtask handle now
//! local.get $st
//! if
//!     call $waitable_set_new
//!     local.set $ws
//!     local.get $st
//!     local.get $ws
//!     call $waitable_join
//!     local.get $ws
//!     i32.const event_ptr
//!     call $waitable_set_wait
//!     drop                     ;; event code (we don't inspect)
//!     local.get $st
//!     call $subtask_drop
//!     local.get $ws
//!     call $waitable_set_drop
//! end
//! ;; pass-through to handler
//! local.get $param_0 ; ... ; local.get $param_N
//! call $handler
//! ```

use wasm_encoder::{CodeSection, Function};
use wit_parser::Resolve;

use super::super::abi::canon_async;
use super::super::abi::emit::{
    emit_alloc_call_id, emit_borrow_drops, emit_bump_restore, emit_bump_save,
    emit_cabi_realloc_call, emit_handler_call, emit_populate_call_id, emit_store_i64_local,
    emit_store_slice, emit_store_slice_ptr_runtime, emit_wrapper_return, BlobSlice, BumpReset,
    RecordLayout,
};
use super::super::indices::LocalsBuilder;
use super::lift::{
    alloc_wrapper_locals, emit_lift_compound_prefix, emit_lift_plan, emit_lift_result,
    CellSideRefs, ResultEmitPlan, WrapperLocals,
};
use super::schema::{
    SchemaLayouts, FIELD_TREE, ON_CALL_ARGS, ON_CALL_CALL, ON_RET_CALL, ON_RET_RESULT, TREE_CELLS,
};
use super::section_emit::FuncIndices;
use super::{FuncDispatch, FuncShape};

/// Static context the wrapper-body emitter needs to read per-call
/// from the layout phase. Bundles the schema + memory-layout
/// addresses so the body emitter doesn't take a half-dozen positional args.
pub(super) struct WrapperCtx<'a> {
    pub(super) schema: &'a SchemaLayouts,
    pub(super) resolve: &'a Resolve,
    pub(super) iface_name: BlobSlice,
    /// `Some` iff the middleware exports `splicer:tier2/before` —
    /// holds every per-build value the on-call emit path needs.
    pub(super) before_hook: Option<BeforeHook<'a>>,
    /// `Some` iff the middleware exports `splicer:tier2/after` —
    /// holds every per-build value the on-return emit path needs.
    /// Per-fn after-hook offsets live on [`FuncDispatch::after`].
    pub(super) after_hook: Option<AfterHook<'a>>,
    /// i64 counter global; bumped once per call to publish `call-id.id`.
    pub(super) call_id_counter_global: u32,
    /// i32 bump-allocator global. Saved at wrapper entry, restored at
    /// exit — per-call `cabi_realloc` traffic frees atomically.
    pub(super) bump_global: u32,
}

/// Per-build static values for the before-hook emit path. Bundling
/// `idx` (import index), `layout` (on-call params record layout), and
/// `params_ptr` (indirect-params scratch buffer offset) into a single
/// `Option` lets the wrapper-body emitter take the "before-hook
/// wired" branch with a single `if let Some(...)` arm rather than a
/// trio of correlated `Option`s and `expect()`s.
pub(super) struct BeforeHook<'a> {
    pub(super) idx: u32,
    pub(super) layout: &'a RecordLayout,
    pub(super) params_ptr: i32,
}

/// Per-build static values for the after-hook emit path. The per-fn
/// params-buffer offset lives on [`FuncDispatch::after`]; the static
/// parts (import index + on-return params layout) are shared across
/// all wrappers. Result cells are `cabi_realloc`'d per call.
pub(super) struct AfterHook<'a> {
    pub(super) idx: u32,
    pub(super) layout: &'a RecordLayout,
}

/// Per-call values written into the on-call indirect-params buffer.
struct OnCallCallSite {
    iface_name: BlobSlice,
    fn_name: BlobSlice,
    args: BlobSlice,
    /// Local holding this invocation's id (bumped at body top).
    id_local: u32,
}

/// Allocate `n_cells` cells worth of memory via `cabi_realloc`, store
/// the result in `cells_base_local`, and patch the field-tree's
/// `cells.ptr` field (`base_ptr + cells_field_off`) so the hook sees
/// the freshly-allocated buffer. `cells.len` was baked at build time
/// from the static plan.
fn emit_alloc_cells_and_patch(
    f: &mut Function,
    cabi_realloc_idx: u32,
    schema: &SchemaLayouts,
    n_cells: u32,
    fields_base_ptr: i32,
    cells_field_off: u32,
    cells_base_local: u32,
) {
    emit_cabi_realloc_call(
        f,
        cabi_realloc_idx,
        schema.cell_layout.align,
        n_cells * schema.cell_layout.size,
        cells_base_local,
    );
    emit_store_slice_ptr_runtime(f, fields_base_ptr, cells_field_off, cells_base_local);
}

/// Byte offset of the `cells: list<cell>` slice within a `field`
/// record (relative to the field's base).
fn field_cells_slice_off(schema: &SchemaLayouts) -> u32 {
    schema.field_layout.offset_of(FIELD_TREE) + schema.tree_layout.offset_of(TREE_CELLS)
}

/// Byte offset of the `cells: list<cell>` slice within the on-return
/// params record (relative to the record's base).
fn after_result_cells_slice_off(schema: &SchemaLayouts, after_layout: &RecordLayout) -> u32 {
    after_layout.offset_of(ON_RET_RESULT)
        + schema.option_payload_off
        + schema.tree_layout.offset_of(TREE_CELLS)
}

/// Write the call-id record + per-call `list<field>` args pointer/len
/// into the indirect-params buffer at `base_ptr`.
fn emit_populate_hook_params(
    f: &mut Function,
    schema: &SchemaLayouts,
    before: &BeforeHook<'_>,
    site: &OnCallCallSite,
) {
    let call_off = before.layout.offset_of(ON_CALL_CALL);
    let args_off = before.layout.offset_of(ON_CALL_ARGS);
    emit_populate_call_id(
        f,
        before.params_ptr,
        call_off,
        &schema.callid_layout,
        site.iface_name,
        site.fn_name,
        site.id_local,
    );
    emit_store_slice(f, before.params_ptr, args_off, site.args);
}

pub(super) fn emit_wrapper_function(
    code: &mut CodeSection,
    func_idx: &FuncIndices,
    ctx: &WrapperCtx<'_>,
    i: usize,
    fd: &FuncDispatch,
) {
    let async_funcs = &func_idx.async_funcs;
    let schema = ctx.schema;
    let nparams = fd.export_sig.params.len() as u32;
    let builder = LocalsBuilder::new(nparams);
    // `alloc_wrapper_locals` consumes the builder: it allocates every
    // wrapper local (incl. compound-result synth locals + task.return
    // bindgen scratch + the call-id local), pre-builds the lift load
    // sequences, and returns a `FrozenLocals`. After this point there
    // is no `LocalsBuilder` in scope, so additional `alloc_local` calls
    // are a compile error.
    let (lcl, result_emit, frozen) =
        alloc_wrapper_locals(ctx.resolve, &schema.size_align, builder, fd);

    let mut f = Function::new_with_locals_types(frozen.locals);

    let bump_reset = BumpReset {
        global: ctx.bump_global,
        saved_local: lcl.saved_bump,
    };
    emit_bump_save(&mut f, bump_reset);

    emit_alloc_call_id(&mut f, ctx.call_id_counter_global, lcl.id_local);

    // ── Phase 1: on-call (only if before-hook wired) ──
    if let Some(before) = ctx.before_hook.as_ref() {
        // Plan cells reference plan-relative flat slots; thread the
        // cumulative cursor as the per-param `local_base` so cell N
        // resolves to absolute wasm-local `local_base + N`.
        let mut local_base: u32 = 0;
        let cells_slice_off = field_cells_slice_off(schema);
        for (i, p) in fd.params.iter().enumerate() {
            let field_off = i as u32 * schema.field_layout.size;
            emit_alloc_cells_and_patch(
                &mut f,
                func_idx.cabi_realloc_idx,
                schema,
                p.lift.plan.cell_count(),
                fd.fields_buf_offset as i32,
                field_off + cells_slice_off,
                lcl.cells_base,
            );
            emit_lift_plan(
                &mut f,
                &schema.cell_layout,
                lcl.cells_base,
                &p.lift.plan,
                CellSideRefs {
                    cell_side: &p.cell_side,
                },
                local_base,
                &lcl,
            );
            local_base += p.lift.plan.flat_slot_count;
        }
        let nargs = fd.params.len() as u32;
        let args_off = if nargs == 0 { 0 } else { fd.fields_buf_offset };
        emit_populate_hook_params(
            &mut f,
            schema,
            before,
            &OnCallCallSite {
                iface_name: ctx.iface_name,
                fn_name: BlobSlice {
                    off: fd.fn_name_offset as u32,
                    len: fd.fn_name_len as u32,
                },
                args: BlobSlice {
                    off: args_off,
                    len: nargs,
                },
                id_local: lcl.id_local,
            },
        );
        f.instructions().i32_const(before.params_ptr);
        canon_async::emit_call_and_wait(&mut f, before.idx, lcl.st, lcl.ws, async_funcs);
    }

    // ── Phase 2: forward to handler. Bridges callee-returns ↔
    // caller-allocates for compound results via the shared
    // abi/emit helpers. For async, the import returns a packed
    // canon-lower-async status that we wait on.
    emit_handler_call(
        &mut f,
        nparams,
        fd.import_sig.retptr,
        fd.retptr_offset,
        func_idx.handler_imp_base + i as u32,
    );
    match &fd.shape {
        FuncShape::Async(_) => {
            f.instructions().local_set(lcl.st);
            canon_async::emit_wait_loop(&mut f, lcl.st, lcl.ws, async_funcs);
        }
        FuncShape::Sync => {
            if let Some(local) = lcl.result {
                f.instructions().local_set(local);
            }
        }
    }

    // ── Phase 3: on-return (only if after-hook wired) ──
    // `ctx.after_hook` (per-build static) and `fd.after` (per-fn
    // offsets) are populated in lockstep at layout time; the
    // unreachable arm pins that contract.
    let after_zip = match (ctx.after_hook.as_ref(), fd.after.as_ref()) {
        (Some(s), Some(pf)) => Some((s, pf)),
        (None, None) => None,
        _ => unreachable!("after-hook ctx and per-fn data are wired in lockstep"),
    };
    if let Some((after_static, after_pf)) = after_zip {
        let n_cells = match &result_emit {
            ResultEmitPlan::Compound { plan, .. } => plan.cell_count(),
            ResultEmitPlan::Direct { .. } => 1,
            ResultEmitPlan::None => 0,
        };
        if n_cells > 0 {
            emit_alloc_cells_and_patch(
                &mut f,
                func_idx.cabi_realloc_idx,
                schema,
                n_cells,
                after_pf.params_offset,
                after_result_cells_slice_off(schema, after_static.layout),
                lcl.cells_base,
            );
            match &result_emit {
                ResultEmitPlan::Compound {
                    plan,
                    retptr_offset,
                    addr_local,
                    synth_locals,
                    loads,
                    side_refs,
                } => {
                    // Memory → flat-on-stack → synthetic locals → walk plan.
                    emit_lift_compound_prefix(
                        &mut f,
                        plan.flat_slot_count,
                        *retptr_offset,
                        loads,
                        *addr_local,
                        synth_locals,
                    );
                    // Synth locals are contiguous; `synth_locals[0]`
                    // is the plan's `local_base`.
                    emit_lift_plan(
                        &mut f,
                        &schema.cell_layout,
                        lcl.cells_base,
                        plan,
                        *side_refs,
                        synth_locals[0],
                        &lcl,
                    );
                }
                ResultEmitPlan::Direct { .. } => {
                    // Single-cell direct result: `lcl.addr = cells_base`.
                    f.instructions().local_get(lcl.cells_base);
                    f.instructions().local_set(lcl.addr);
                    emit_lift_result(&mut f, &schema.cell_layout, &result_emit, &lcl);
                }
                ResultEmitPlan::None => {}
            }
        }
        // iface/fn are prewritten by `build_after_params_blob`;
        // only `call.id` changes per call, so patch it at runtime.
        let id_field_off =
            after_static.layout.offset_of(ON_RET_CALL) + schema.callid_layout.id_off();
        emit_store_i64_local(&mut f, after_pf.params_offset, id_field_off, lcl.id_local);
        f.instructions().i32_const(after_pf.params_offset);
        canon_async::emit_call_and_wait(&mut f, after_static.idx, lcl.st, lcl.ws, async_funcs);
    }

    // Drop borrow handles before tail emit — runtime-required.
    emit_borrow_drops(&mut f, &fd.borrow_drops, &func_idx.resource_drop);

    emit_bump_restore(&mut f, bump_reset);

    // ── Phase 4: tail. Async fns publish the result via task.return;
    // sync fns return the direct value (or static retptr).
    match &fd.shape {
        FuncShape::Async(_) => {
            emit_task_return(&mut f, fd, func_idx, i, &lcl);
        }
        FuncShape::Sync => {
            emit_wrapper_return(&mut f, lcl.result, fd.export_sig.retptr, fd.retptr_offset);
        }
    }
    f.instructions().end();
    code.function(&f);
}

/// Emit the async tail: call `task.return` with the appropriate
/// args. Three shapes:
/// - void result → no args.
/// - `tr_sig.indirect_params` (compound result) → push retptr scratch.
/// - flat result → load each value from retptr via the pre-built
///   `lift_from_memory` instruction sequence.
fn emit_task_return(
    f: &mut Function,
    fd: &FuncDispatch,
    func_idx: &FuncIndices,
    i: usize,
    lcl: &WrapperLocals,
) {
    let imp_task_return =
        func_idx.task_return_idx[i].expect("async func must have task.return import");
    let FuncShape::Async(tr) = &fd.shape else {
        unreachable!("emit_task_return called only for async funcs")
    };
    if fd.result_ty.is_none() {
        f.instructions().call(imp_task_return);
    } else if tr.sig.indirect_params {
        f.instructions().i32_const(
            fd.retptr_offset
                .expect("async non-void result → retptr_offset"),
        );
        f.instructions().call(imp_task_return);
    } else {
        let addr_local = lcl.tr_addr.expect("flat loads → tr_addr local");
        let task_return_loads = lcl
            .task_return_loads
            .as_deref()
            .expect("flat loads → instruction sequence");
        f.instructions().i32_const(
            fd.retptr_offset
                .expect("async non-void result → retptr_offset"),
        );
        f.instructions().local_set(addr_local);
        for inst in task_return_loads {
            f.instruction(inst);
        }
        f.instructions().call(imp_task_return);
    }
}
