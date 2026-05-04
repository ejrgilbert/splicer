//! Wrapper-body emit: builds the wasm function body for one
//! exported wrapper. Drives the four-phase shape (build call-id
//! + on-call → call handler → on-return → tail / `task.return`)
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

use wasm_encoder::{CodeSection, Function, MemArg};
use wit_bindgen_core::abi::lift_from_memory;
use wit_parser::Resolve;

use super::super::abi::canon_async;
use super::super::abi::emit::{emit_handler_call, emit_wrapper_return};
use super::super::abi::WasmEncoderBindgen;
use super::super::indices::FunctionIndices;
use super::blob::BlobSlice;
use super::lift::{
    alloc_wrapper_locals, emit_lift_compound_prefix, emit_lift_plan, emit_lift_result,
    ResultEmitPlan, WrapperLocals,
};
use super::schema::{
    SchemaLayouts, CALLID_FN, CALLID_IFACE, ON_CALL_ARGS, ON_CALL_CALL,
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
    pub(super) hook_params_ptr: i32,
}

/// Per-call values written into the on-call indirect-params buffer.
/// Slice-typed (vs. raw i32 pairs) so callers can't swap ptr/len.
struct OnCallCallSite {
    iface_name: BlobSlice,
    fn_name: BlobSlice,
    args: BlobSlice,
}

/// Emit wasm that writes the call-id (interface + function name
/// pointers/lengths) and the per-call `list<field>` args pointer/
/// length into the indirect-params buffer at `base_ptr`. Field
/// offsets are looked up from the schema at use site so the
/// canonical-ABI numbers stay schema-driven.
fn emit_populate_hook_params(
    f: &mut Function,
    base_ptr: i32,
    schema: &SchemaLayouts,
    site: &OnCallCallSite,
) {
    let on_call = schema
        .on_call_params_layout
        .as_ref()
        .expect("emit_populate_hook_params called only when before-hook wired");
    let call_off = on_call.offset_of(ON_CALL_CALL);
    let args_off = on_call.offset_of(ON_CALL_ARGS);
    let iface_off = call_off + schema.callid_layout.offset_of(CALLID_IFACE);
    let fn_off = call_off + schema.callid_layout.offset_of(CALLID_FN);
    emit_store_slice(f, base_ptr, iface_off, site.iface_name);
    emit_store_slice(f, base_ptr, fn_off, site.fn_name);
    emit_store_slice(f, base_ptr, args_off, site.args);
}

/// Emit two `i32.store`s writing `slice.off` then `slice.len` into
/// the `(ptr, len)` pair starting at `base_ptr + field_off`.
fn emit_store_slice(f: &mut Function, base_ptr: i32, field_off: u32, slice: BlobSlice) {
    use super::super::abi::emit::{SLICE_LEN_OFFSET, SLICE_PTR_OFFSET};
    let store = |f: &mut Function, sub_off: u32, value: i32| {
        f.instructions().i32_const(base_ptr);
        f.instructions().i32_const(value);
        f.instructions().i32_store(MemArg {
            offset: (field_off + sub_off) as u64,
            align: 2,
            memory_index: 0,
        });
    };
    store(f, SLICE_PTR_OFFSET, slice.off as i32);
    store(f, SLICE_LEN_OFFSET, slice.len as i32);
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
    let mut locals = FunctionIndices::new(nparams);
    // `alloc_wrapper_locals` also drives the `lift_from_memory`
    // bindgen for compound result lifts (it may allocate further
    // scratch locals — must happen before the locals list freezes).
    let (lcl, result_emit) = alloc_wrapper_locals(ctx.resolve, &schema.size_align, &mut locals, fd);

    // For async funcs whose `task.return` takes flat-form params,
    // pre-build the load sequence — `lift_from_memory` may allocate
    // additional bindgen scratch locals, which must happen before
    // the locals list is frozen below.
    let task_return_loads: Option<Vec<wasm_encoder::Instruction<'static>>> =
        lcl.tr_addr.map(|addr_local| {
            let result_ty = fd.result_ty.as_ref().expect("flat loads → result_ty");
            let mut bindgen = WasmEncoderBindgen::new(&schema.size_align, addr_local, &mut locals);
            lift_from_memory(ctx.resolve, &mut bindgen, (), result_ty);
            bindgen.into_instructions()
        });

    let mut f = Function::new_with_locals_types(locals.into_locals());

    // ── Phase 1: on-call (only if before-hook wired) ──
    if let Some(before_idx) = func_idx.before_hook_idx {
        for p in fd.params.iter() {
            emit_lift_plan(
                &mut f,
                &schema.cell_layout,
                p.cells_offset,
                &p.lift.plan,
                &p.record_info_cell_idx,
                &lcl,
            );
        }
        let nargs = fd.params.len() as u32;
        let args_off = if nargs == 0 { 0 } else { fd.fields_buf_offset };
        emit_populate_hook_params(
            &mut f,
            ctx.hook_params_ptr,
            schema,
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
            },
        );
        f.instructions().i32_const(ctx.hook_params_ptr);
        canon_async::emit_call_and_wait(&mut f, before_idx, lcl.st, lcl.ws, async_funcs);
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
    if let (Some(after_idx), Some(after)) = (func_idx.after_hook_idx, fd.after.as_ref()) {
        if let Some(cells_off) = after.result_cells_offset {
            match &result_emit {
                ResultEmitPlan::Compound {
                    plan,
                    retptr_offset,
                    addr_local,
                    synth_locals,
                    loads,
                    record_info_cell_idx,
                    ..
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
                    emit_lift_plan(
                        &mut f,
                        &schema.cell_layout,
                        cells_off,
                        plan,
                        record_info_cell_idx,
                        &lcl,
                    );
                }
                ResultEmitPlan::Direct { .. } | ResultEmitPlan::RetptrPair { .. } => {
                    f.instructions().i32_const(cells_off as i32);
                    f.instructions().local_set(lcl.addr);
                    emit_lift_result(&mut f, &schema.cell_layout, &result_emit, &lcl);
                }
                ResultEmitPlan::None => {}
            }
        }
        f.instructions().i32_const(after.params_offset);
        canon_async::emit_call_and_wait(&mut f, after_idx, lcl.st, lcl.ws, async_funcs);
    }

    // ── Phase 4: tail. Async fns publish the result via task.return;
    // sync fns return the direct value (or static retptr).
    match &fd.shape {
        FuncShape::Async(_) => {
            emit_task_return(&mut f, fd, func_idx, i, &lcl, task_return_loads.as_deref());
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
    task_return_loads: Option<&[wasm_encoder::Instruction<'static>]>,
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
        f.instructions().i32_const(
            fd.retptr_offset
                .expect("async non-void result → retptr_offset"),
        );
        f.instructions().local_set(addr_local);
        for inst in task_return_loads.expect("flat loads → instruction sequence") {
            f.instruction(inst);
        }
        f.instructions().call(imp_task_return);
    }
}
