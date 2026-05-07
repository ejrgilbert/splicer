//! Wrapper-body emit: builds the wasm function body for one
//! exported wrapper.
//!
//! ## Concurrency
//!
//! Wrappers assume one in-flight call per instance. State mutated
//! in place over a call's lifetime that would corrupt under
//! reentrancy:
//!
//! - Static side-table scratch (`flags-info.set-flags`,
//!   `variant-info.case-name` + `payload`, `handle-info.id`,
//!   per-cell char utf-8 scratch) — written per call; the cell
//!   tree points into them.
//! - Static field-tree `cells` slice — `ptr` and `len` patched
//!   per call to point at the freshly-`cabi_realloc`'d slab.
//! - Per-list indices buffer — `cabi_realloc`'d per call from the
//!   wrapper's bump allocator, freed at exit via bump save/restore.
//!
//! A concurrent second call would see the first's scratch
//! addresses and slab pointer mid-update. The canon-async runtime
//! is expected to serialize per instance; revisit if that changes.

use wasm_encoder::{CodeSection, Function};
use wit_parser::Resolve;

use super::super::abi::canon_async;
use super::super::abi::emit::{
    emit_alloc_call_id, emit_borrow_drops, emit_bump_restore, emit_bump_save,
    emit_cabi_realloc_call, emit_cabi_realloc_call_runtime, emit_handler_call,
    emit_populate_call_id, emit_store_i64_local, emit_store_slice, emit_store_slice_len_runtime,
    emit_store_slice_ptr_runtime, emit_wrapper_return, BlobSlice, BumpReset, RecordLayout,
};
use super::super::indices::LocalsBuilder;
use super::lift::plan::LiftPlan;
use super::lift::{
    alloc_wrapper_locals, emit_lift_compound_prefix, emit_lift_plan, emit_lift_result,
    CellSideRefs, LiftEmitCtx, ListEmitLocals, ResultEmitPlan, WrapperLocals,
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

/// Where the patched `cells: list<cell>` slice lives in linear memory.
struct CellsTarget {
    fields_base_ptr: i32,
    cells_field_off: u32,
}

/// Cells-slab allocation for one (param | result) plan. Pre-pass
/// captures each list's `start_i` + `len`, accumulates
/// `total_cells = static + Σ(len_i · elem_count_i)` into
/// `lcl.next_cell_idx`, then `cabi_realloc`s the slab and patches
/// both `cells.ptr` and `cells.len`.
fn emit_alloc_cells_for_plan(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    plan: &LiftPlan,
    list_locals: &[ListEmitLocals],
    local_base: u32,
    lcl: &WrapperLocals,
    target: CellsTarget,
) {
    debug_assert_eq!(
        list_locals.len(),
        plan.list_specs().count(),
        "per-plan list_locals must be parallel to plan.list_specs()",
    );
    // Pre-pass: next_cell_idx = static_count, then for each list,
    // capture start_i + len, bump by len * elem_count. `list_idx`
    // on the spec keys directly into `list_locals` so this is
    // structural, not positional.
    f.instructions().i32_const(plan.cell_count() as i32);
    f.instructions().local_set(lcl.next_cell_idx);
    for spec in plan.list_specs() {
        let ll = &list_locals[spec.list_idx as usize];
        f.instructions().local_get(lcl.next_cell_idx);
        f.instructions().local_set(ll.start_i);
        f.instructions().local_get(local_base + spec.len_slot);
        f.instructions().local_set(ll.len);
        let elem_count = spec.element_plan.cell_count();
        f.instructions().local_get(lcl.next_cell_idx);
        f.instructions().local_get(ll.len);
        if elem_count != 1 {
            f.instructions().i32_const(elem_count as i32);
            f.instructions().i32_mul();
        }
        f.instructions().i32_add();
        f.instructions().local_set(lcl.next_cell_idx);
    }
    // Single cabi_realloc(next_cell_idx * cell_size).
    emit_cabi_realloc_call_runtime(
        f,
        ctx.cabi_realloc_idx,
        ctx.cell_layout.align,
        lcl.next_cell_idx,
        ctx.cell_layout.size,
        lcl.cells_base,
    );
    emit_store_slice_ptr_runtime(
        f,
        target.fields_base_ptr,
        target.cells_field_off,
        lcl.cells_base,
    );
    emit_store_slice_len_runtime(
        f,
        target.fields_base_ptr,
        target.cells_field_off,
        lcl.next_cell_idx,
    );
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

    let lift_ctx = LiftEmitCtx {
        cell_layout: &schema.cell_layout,
        cabi_realloc_idx: func_idx.cabi_realloc_idx,
    };

    // ── Phase 1: on-call (only if before-hook wired) ──
    if let Some(before) = ctx.before_hook.as_ref() {
        // Plan cells reference plan-relative flat slots; thread the
        // cumulative cursor as the per-param `local_base` so cell N
        // resolves to absolute wasm-local `local_base + N`.
        let mut local_base: u32 = 0;
        let cells_slice_off = field_cells_slice_off(schema);
        for (i, p) in fd.params.iter().enumerate() {
            let field_off = i as u32 * schema.field_layout.size;
            let list_locals = &lcl.param_list_locals[i];
            emit_alloc_cells_for_plan(
                &mut f,
                &lift_ctx,
                &p.lift.plan,
                list_locals,
                local_base,
                &lcl,
                CellsTarget {
                    fields_base_ptr: fd.fields_buf_offset as i32,
                    cells_field_off: field_off + cells_slice_off,
                },
            );
            emit_lift_plan(
                &mut f,
                &lift_ctx,
                &p.lift.plan,
                CellSideRefs {
                    cell_side: &p.cell_side,
                },
                local_base,
                &lcl,
                list_locals,
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
        let cells_field_off = after_result_cells_slice_off(schema, after_static.layout);
        match &result_emit {
            ResultEmitPlan::Compound {
                plan,
                retptr_offset,
                addr_local,
                synth_locals,
                loads,
                side_refs,
                list_locals,
            } => {
                // Memory → flat-on-stack → synthetic locals first,
                // so the alloc pre-pass can read each list's
                // `len_slot` from synth_locals.
                emit_lift_compound_prefix(
                    &mut f,
                    plan.flat_slot_count,
                    *retptr_offset,
                    loads,
                    *addr_local,
                    synth_locals,
                );
                emit_alloc_cells_for_plan(
                    &mut f,
                    &lift_ctx,
                    plan,
                    list_locals,
                    synth_locals[0],
                    &lcl,
                    CellsTarget {
                        fields_base_ptr: after_pf.params_offset,
                        cells_field_off,
                    },
                );
                // Synth locals are contiguous; `synth_locals[0]`
                // is the plan's `local_base`.
                emit_lift_plan(
                    &mut f,
                    &lift_ctx,
                    plan,
                    *side_refs,
                    synth_locals[0],
                    &lcl,
                    list_locals,
                );
            }
            ResultEmitPlan::Direct { .. } => {
                // Single-cell direct result: build-time-sized cells slab,
                // patch ptr (len is static-filled), `lcl.addr = cells_base`.
                emit_cabi_realloc_call(
                    &mut f,
                    func_idx.cabi_realloc_idx,
                    schema.cell_layout.align,
                    schema.cell_layout.size,
                    lcl.cells_base,
                );
                emit_store_slice_ptr_runtime(
                    &mut f,
                    after_pf.params_offset,
                    cells_field_off,
                    lcl.cells_base,
                );
                f.instructions().local_get(lcl.cells_base);
                f.instructions().local_set(lcl.addr);
                emit_lift_result(&mut f, &schema.cell_layout, &result_emit, &lcl);
            }
            ResultEmitPlan::None => {}
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
