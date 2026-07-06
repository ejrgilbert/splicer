//! Wrapper-body emit: builds the wasm function body for one wrapper.
//!
//! **Concurrency**: wrappers assume one in-flight call per instance.
//! Static side-table scratch (`set-flags`, `case-name`, char utf-8),
//! per-call `handle-info` buffer, field-tree `cells` slice, and the
//! per-list indices buffer all mutate in place. A concurrent second
//! call would see the first mid-update. The canon-async runtime
//! serializes per instance; revisit if that changes.

use wasm_encoder::{CodeSection, Function};
use wit_parser::{Function as WitFunction, Resolve};

use super::super::abi::canon_async;
use super::super::abi::emit::{
    emit_alloc_call_id, emit_borrow_drops, emit_bump_restore, emit_bump_save,
    emit_cabi_realloc_call, emit_cabi_realloc_call_runtime, emit_handler_call,
    emit_store_i64_local, emit_store_slice_len_runtime, emit_store_slice_ptr_runtime,
    emit_wrapper_return, BlobSlice, BumpReset, RecordLayout,
};
use super::super::indices::LocalsBuilder;
use super::lift::plan::LiftPlan;
use super::lift::{
    alloc_wrapper_locals, emit_lift_compound_prefix, emit_lift_plan, emit_lift_result,
    emit_list_pre_pass, CellSideRefs, FlagsInfoOffsets, HandleInfoOffsets, InfoCounts, LiftEmitCtx,
    ListEmitLocals, RecordInfoOffsets, ResultEmitPlan, VariantInfoOffsets, WrapperLocals,
};
use super::schema::{
    SchemaLayouts, FIELD_TREE, ON_RET_CALL, ON_RET_RESULT, TREE_CELLS, TREE_FLAGS_INFOS,
    TREE_HANDLE_INFOS, TREE_RECORD_INFOS, TREE_VARIANT_INFOS,
};
use super::section_emit::FuncIndices;
use super::{FuncDispatch, FuncShape};

/// Static context the wrapper-body emitter reads from the layout phase.
pub(super) struct WrapperCtx<'a> {
    pub(super) schema: &'a SchemaLayouts,
    pub(super) resolve: &'a Resolve,
    pub(super) iface_name: BlobSlice,
    pub(super) before_hook: Option<BeforeHook>,
    pub(super) after_hook: Option<AfterHook<'a>>,
    pub(super) gate_hook: Option<GateHook>,
    /// i64 counter; bumped once per call to publish `call-id.id`.
    pub(super) call_id_counter_global: u32,
    /// Bump-allocator i32 global; save/restore frees per-call `cabi_realloc`.
    pub(super) bump_global: u32,
}

/// Per-build values for the before-hook emit path.
pub(super) struct BeforeHook {
    pub(super) idx: u32,
}

/// Per-build values for the after-hook emit path. Per-fn params-buffer
/// offset lives on `FuncDispatch::after`.
pub(super) struct AfterHook<'a> {
    pub(super) idx: u32,
    pub(super) layout: &'a RecordLayout,
}

/// Per-build values for the gate-hook emit path. Bool result returned
/// directly as i32 (no retptr needed).
pub(super) struct GateHook {
    pub(super) idx: u32,
}

/// Where the patched `cells: list<cell>` slice lives + per-plan
/// pre-pass seeds (outer Handle/Flags counts; list elements layer on top).
struct CellsTarget {
    fields_base_ptr: i32,
    cells_field_off: u32,
    static_info_counts: InfoCounts,
}

/// Cells-slab allocation for one plan: pre-pass (disc-gated for
/// joined-arm lists), `cabi_realloc` the slab, patch `cells.ptr`/`.len`.
fn emit_alloc_cells_for_plan(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    plan: &LiftPlan,
    list_locals: &[ListEmitLocals],
    local_base: u32,
    lcl: &WrapperLocals,
    target: CellsTarget,
) {
    emit_list_pre_pass(
        f,
        ctx,
        plan,
        &target.static_info_counts,
        list_locals,
        local_base,
        lcl,
    );
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

/// Byte offset of `field.tree.<field_name>` within a `field` record.
fn field_tree_slice_off(schema: &SchemaLayouts, tree_field: &str) -> u32 {
    schema.field_layout.offset_of(FIELD_TREE) + schema.tree_layout.offset_of(tree_field)
}

/// After-hook twin of `field_tree_slice_off`.
fn after_result_tree_slice_off(
    schema: &SchemaLayouts,
    after_layout: &RecordLayout,
    tree_field: &str,
) -> u32 {
    after_layout.offset_of(ON_RET_RESULT)
        + schema.option_payload_off
        + schema.tree_layout.offset_of(tree_field)
}

/// Per-cell side-table kinds with a per-call info buffer.
#[derive(Clone, Copy)]
enum InfoKind {
    Handle,
    Flags,
    Record,
    Variant,
}

impl InfoKind {
    /// Suffix used in panic messages; must match the gate-predicate
    /// names (`fn_has_*_cells`, `next_*_idx`, `*_info_base`, etc.).
    fn name(self) -> &'static str {
        match self {
            InfoKind::Handle => "handle",
            InfoKind::Flags => "flags",
            InfoKind::Record => "record",
            InfoKind::Variant => "variant",
        }
    }

    /// `(align, entry_size)` from the schema-derived offsets bundle.
    fn entry_geometry(self, ctx: &LiftEmitCtx<'_>) -> (u32, u32) {
        match self {
            InfoKind::Handle => (ctx.handle_info.align, ctx.handle_info.entry_size),
            InfoKind::Flags => (ctx.flags_info.align, ctx.flags_info.entry_size),
            InfoKind::Record => (ctx.record_info.align, ctx.record_info.entry_size),
            InfoKind::Variant => (ctx.variant_info.align, ctx.variant_info.entry_size),
        }
    }

    /// `(info_base_local, runtime_count_local)` from the wrapper
    /// locals. Both `Option` — `info_base` is `Some` iff the wrapper
    /// has any cells of this kind; `runtime_count` is `Some` iff the
    /// wrapper has list-element cells of this kind.
    fn locals(self, lcl: &WrapperLocals) -> (Option<u32>, Option<u32>) {
        match self {
            InfoKind::Handle => (lcl.handle_info_base, lcl.next_handle_idx),
            InfoKind::Flags => (lcl.flags_info_base, lcl.next_flags_idx),
            InfoKind::Record => (lcl.record_info_base, lcl.next_record_idx),
            InfoKind::Variant => (lcl.variant_info_base, lcl.next_variant_idx),
        }
    }
}

/// Per-kind dispatch + slice-target inputs for one
/// [`emit_alloc_info_buffer_for_plan`] call. Bundled so the function
/// signature stays under clippy's `too_many_arguments` cap; the call
/// sites already iterate over `[InfoBufferTarget; 4]` arrays.
struct InfoBufferTarget {
    kind: InfoKind,
    /// Outer-cell count of this kind (drives static-count `cabi_realloc`).
    static_count: u32,
    /// `true` iff this plan has list-element cells of this kind —
    /// the alloc reads `next_*_idx` and patches both `ptr` + `len`.
    runtime_sized: bool,
    /// Field-tree slice offset within the `field` (or after-result)
    /// record where this kind's `*-infos` slice lives.
    slice_field_off: u32,
}

/// Per-call info-buffer alloc + slice-ptr patch for one
/// (param | result) plan.
///
/// Runtime path (list-element cells present): pre-pass-accumulated
/// `next_*_idx` sizes the slab; ptr + len both patched. Build-time
/// path: `static_count * entry_size` slab; only ptr patched (len was
/// baked by `build_fields_blob`). Skip when both are zero.
fn emit_alloc_info_buffer_for_plan(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    target: InfoBufferTarget,
    base_ptr: i32,
    lcl: &WrapperLocals,
) {
    let InfoBufferTarget {
        kind,
        static_count,
        runtime_sized,
        slice_field_off,
    } = target;
    let (align, entry_size) = kind.entry_geometry(ctx);
    let (base_local_opt, count_local_opt) = kind.locals(lcl);
    if runtime_sized {
        let count_local = count_local_opt.unwrap_or_else(|| {
            panic!(
                "fn_has_list_elem_{kind} gate disagrees with plan",
                kind = kind.name()
            )
        });
        let base_local = base_local_opt.unwrap_or_else(|| {
            panic!(
                "fn_has_{kind}_cells gate disagrees with fn_has_list_elem_{kind}",
                kind = kind.name()
            )
        });
        emit_cabi_realloc_call_runtime(
            f,
            ctx.cabi_realloc_idx,
            align,
            count_local,
            entry_size,
            base_local,
        );
        emit_store_slice_ptr_runtime(f, base_ptr, slice_field_off, base_local);
        emit_store_slice_len_runtime(f, base_ptr, slice_field_off, count_local);
        return;
    }
    if static_count == 0 {
        return;
    }
    let base_local = base_local_opt.unwrap_or_else(|| {
        panic!(
            "fn_has_{kind}_cells gate disagrees with {kind}_count",
            kind = kind.name()
        )
    });
    let buf_size = static_count
        .checked_mul(entry_size)
        .unwrap_or_else(|| panic!("{kind}_info buffer size overflowed u32", kind = kind.name()));
    emit_cabi_realloc_call(f, ctx.cabi_realloc_idx, align, buf_size, base_local);
    emit_store_slice_ptr_runtime(f, base_ptr, slice_field_off, base_local);
}

/// Push the 7 flat values for a `(call: call-id, args: list<field>)`
/// call onto the wasm stack. Used by the sync before- and gate-hook
/// call sites; the hook's canonical-ABI sig is
/// `(i32, i32, i32, i32, i64, i32, i32) -> ()`.
fn emit_flat_before_params(
    f: &mut Function,
    ctx: &WrapperCtx<'_>,
    fd: &FuncDispatch,
    nargs: u32,
    args_off: u32,
    id_local: u32,
) {
    f.instructions().i32_const(ctx.iface_name.off as i32);
    f.instructions().i32_const(ctx.iface_name.len as i32);
    f.instructions().i32_const(fd.fn_name_offset);
    f.instructions().i32_const(fd.fn_name_len);
    f.instructions().local_get(id_local);
    f.instructions().i32_const(args_off as i32);
    f.instructions().i32_const(nargs as i32);
}

pub(super) fn emit_wrapper_function(
    code: &mut CodeSection,
    func_idx: &FuncIndices,
    ctx: &WrapperCtx<'_>,
    i: usize,
    fd: &FuncDispatch,
    func: &WitFunction,
) -> anyhow::Result<()> {
    let async_funcs = &func_idx.async_funcs;
    let schema = ctx.schema;
    let nparams = fd.export_sig.params.len() as u32;
    let builder = LocalsBuilder::new(nparams);
    // `alloc_wrapper_locals` consumes the builder, returns `FrozenLocals`;
    // additional `alloc_local` after this point is a compile error.
    let (lcl, result_emit, frozen) = alloc_wrapper_locals(
        ctx.resolve,
        &schema.size_align,
        schema.record_field_tuple_layout.size,
        builder,
        fd,
        func,
        ctx.before_hook.is_some(),
    )?;

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
        handle_info: HandleInfoOffsets::from_layout(&schema.handle_info_layout),
        flags_info: FlagsInfoOffsets::from_layout(&schema.flags_info_layout),
        record_info: RecordInfoOffsets::from_layout(
            &schema.record_info_layout,
            &schema.record_field_tuple_layout,
        ),
        variant_info: VariantInfoOffsets::from_layout(
            &schema.variant_info_layout,
            schema.variant_info_payload_value_off,
        ),
    };

    // ── Phase 1: on-call + gate (lift runs if either is wired) ──
    //
    // Both hooks take flat params `(call-id: 5 flat, args: list 2 flat)`.
    // The lift work — cells alloc, info-buffer alloc, plan emit — runs
    // once; args.ptr points at the fields buffer built here.
    let has_before_or_gate = ctx.before_hook.is_some() || ctx.gate_hook.is_some();
    if has_before_or_gate {
        // Symmetric-indirect (sync >16 flat): wrapper has only `local
        // 0` (the host's params pointer), so we materialize each
        // param's flat representation into synth locals up front by
        // playing the per-param lift-from-memory sequences. Phase 1
        // then reads from `synth_base[i]` instead of cumulative-flat.
        if let Some(seqs) = lcl.params_lift_seqs.as_ref() {
            debug_assert_eq!(
                seqs.len(),
                fd.params.len(),
                "params_lift_seqs length must match fd.params",
            );
            for seq in seqs {
                for inst in &seq.instructions {
                    f.instruction(inst);
                }
            }
        }
        // Cumulative `local_base` threads plan-relative slots into
        // absolute wasm-locals; only used in the flat-param path.
        let mut flat_local_base: u32 = 0;
        let cells_slice_off = field_tree_slice_off(schema, TREE_CELLS);
        let handle_infos_slice_off = field_tree_slice_off(schema, TREE_HANDLE_INFOS);
        let flags_infos_slice_off = field_tree_slice_off(schema, TREE_FLAGS_INFOS);
        let record_infos_slice_off = field_tree_slice_off(schema, TREE_RECORD_INFOS);
        let variant_infos_slice_off = field_tree_slice_off(schema, TREE_VARIANT_INFOS);
        for (i, p) in fd.params.iter().enumerate() {
            let local_base = match lcl.params_lift_seqs.as_ref() {
                Some(seqs) => seqs[i].synth_base,
                None => {
                    let base = flat_local_base;
                    flat_local_base += p.lift.plan.flat_slot_count;
                    base
                }
            };
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
                    static_info_counts: p.info_counts,
                },
            );
            for target in [
                InfoBufferTarget {
                    kind: InfoKind::Handle,
                    static_count: p.info_counts.handle,
                    runtime_sized: p.lift.plan.has_list_elem_handle(),
                    slice_field_off: field_off + handle_infos_slice_off,
                },
                InfoBufferTarget {
                    kind: InfoKind::Flags,
                    static_count: p.info_counts.flags,
                    runtime_sized: p.lift.plan.has_list_elem_flags(),
                    slice_field_off: field_off + flags_infos_slice_off,
                },
                InfoBufferTarget {
                    kind: InfoKind::Record,
                    static_count: p.info_counts.record,
                    runtime_sized: p.lift.plan.has_list_elem_record(),
                    slice_field_off: field_off + record_infos_slice_off,
                },
                InfoBufferTarget {
                    kind: InfoKind::Variant,
                    static_count: p.info_counts.variant,
                    runtime_sized: p.lift.plan.has_list_elem_variant(),
                    slice_field_off: field_off + variant_infos_slice_off,
                },
            ] {
                emit_alloc_info_buffer_for_plan(
                    &mut f,
                    &lift_ctx,
                    target,
                    fd.fields_buf_offset as i32,
                    &lcl,
                );
            }
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
        }
        let nargs = fd.params.len() as u32;
        let args_off = if nargs == 0 { 0 } else { fd.fields_buf_offset };
        if let Some(before) = ctx.before_hook.as_ref() {
            emit_flat_before_params(&mut f, ctx, fd, nargs, args_off, lcl.id_local);
            f.instructions().call(before.idx);
        }
        if let Some(gate) = ctx.gate_hook.as_ref() {
            emit_flat_before_params(&mut f, ctx, fd, nargs, args_off, lcl.id_local);
            f.instructions().call(gate.idx);
            // Bool result on the stack directly as i32. `true` → fall
            // through; `false` → skip. `i32.eqz` fires the if-arm on skip.
            f.instructions().i32_eqz();
            f.instructions().if_(wasm_encoder::BlockType::Empty);
            if let FuncShape::Async(_) = &fd.shape {
                let imp_task_return = func_idx.task_return_idx[i]
                    .expect("async target with gate hook must have task.return import");
                f.instructions().call(imp_task_return);
            }
            emit_bump_restore(&mut f, bump_reset);
            f.instructions().return_();
            f.instructions().end();
        }
    }

    // ── Phase 2: forward to handler. Two arg shapes per
    // canon-lower-async: direct pushes each flat param; indirect
    // replays the lower-to-memory sequence + pushes the params-record
    // ptr (capped at retptr if the import also caller-allocates).
    let handler_imp_idx = func_idx.handler_imp_base + i as u32;
    if let Some(seq) = lcl.params_lower_seq.as_ref() {
        for inst in seq {
            f.instruction(inst);
        }
        f.instructions().i32_const(
            fd.params_record_offset
                .expect("indirect_params → params_record_offset"),
        );
        if fd.import_sig.retptr {
            f.instructions()
                .i32_const(fd.retptr_offset.expect("import_retptr → retptr_offset"));
        }
        f.instructions().call(handler_imp_idx);
    } else {
        emit_handler_call(
            &mut f,
            nparams,
            fd.import_sig.retptr,
            fd.retptr_offset,
            handler_imp_idx,
        );
    }
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
    // `ctx.after_hook` + `fd.after` are wired in lockstep.
    let after_zip = match (ctx.after_hook.as_ref(), fd.after.as_ref()) {
        (Some(s), Some(pf)) => Some((s, pf)),
        (None, None) => None,
        _ => unreachable!("after-hook ctx and per-fn data are wired in lockstep"),
    };
    if let Some((after_static, after_pf)) = after_zip {
        let cells_field_off = after_result_tree_slice_off(schema, after_static.layout, TREE_CELLS);
        let handle_infos_field_off =
            after_result_tree_slice_off(schema, after_static.layout, TREE_HANDLE_INFOS);
        let flags_infos_field_off =
            after_result_tree_slice_off(schema, after_static.layout, TREE_FLAGS_INFOS);
        let record_infos_field_off =
            after_result_tree_slice_off(schema, after_static.layout, TREE_RECORD_INFOS);
        let variant_infos_field_off =
            after_result_tree_slice_off(schema, after_static.layout, TREE_VARIANT_INFOS);
        let result_info_counts = fd
            .result_lift
            .as_ref()
            .map(|rl| rl.info_counts)
            .unwrap_or_default();
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
                        static_info_counts: result_info_counts,
                    },
                );
                for target in [
                    InfoBufferTarget {
                        kind: InfoKind::Handle,
                        static_count: result_info_counts.handle,
                        runtime_sized: plan.has_list_elem_handle(),
                        slice_field_off: handle_infos_field_off,
                    },
                    InfoBufferTarget {
                        kind: InfoKind::Flags,
                        static_count: result_info_counts.flags,
                        runtime_sized: plan.has_list_elem_flags(),
                        slice_field_off: flags_infos_field_off,
                    },
                    InfoBufferTarget {
                        kind: InfoKind::Record,
                        static_count: result_info_counts.record,
                        runtime_sized: plan.has_list_elem_record(),
                        slice_field_off: record_infos_field_off,
                    },
                    InfoBufferTarget {
                        kind: InfoKind::Variant,
                        static_count: result_info_counts.variant,
                        runtime_sized: plan.has_list_elem_variant(),
                        slice_field_off: variant_infos_field_off,
                    },
                ] {
                    emit_alloc_info_buffer_for_plan(
                        &mut f,
                        &lift_ctx,
                        target,
                        after_pf.params_offset,
                        &lcl,
                    );
                }
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
                // Single-cell: build-time-sized slab, patch ptr (len is static).
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
                // Direct is single-cell flat; lists can't reach here.
                for target in [
                    InfoBufferTarget {
                        kind: InfoKind::Handle,
                        static_count: result_info_counts.handle,
                        runtime_sized: false,
                        slice_field_off: handle_infos_field_off,
                    },
                    InfoBufferTarget {
                        kind: InfoKind::Flags,
                        static_count: result_info_counts.flags,
                        runtime_sized: false,
                        slice_field_off: flags_infos_field_off,
                    },
                    InfoBufferTarget {
                        kind: InfoKind::Record,
                        static_count: result_info_counts.record,
                        runtime_sized: false,
                        slice_field_off: record_infos_field_off,
                    },
                    InfoBufferTarget {
                        kind: InfoKind::Variant,
                        static_count: result_info_counts.variant,
                        runtime_sized: false,
                        slice_field_off: variant_infos_field_off,
                    },
                ] {
                    emit_alloc_info_buffer_for_plan(
                        &mut f,
                        &lift_ctx,
                        target,
                        after_pf.params_offset,
                        &lcl,
                    );
                }
                f.instructions().local_get(lcl.cells_base);
                f.instructions().local_set(lcl.addr);
                emit_lift_result(&mut f, &lift_ctx, &result_emit, &lcl);
            }
            ResultEmitPlan::None => {}
        }
        // iface/fn prewritten by `build_after_params_blob`; patch id.
        let id_field_off =
            after_static.layout.offset_of(ON_RET_CALL) + schema.callid_layout.id_off();
        emit_store_i64_local(&mut f, after_pf.params_offset, id_field_off, lcl.id_local);
        f.instructions().i32_const(after_pf.params_offset);
        f.instructions().call(after_static.idx);
    }

    // Drop borrow handles before tail (runtime-required).
    emit_borrow_drops(&mut f, &fd.borrow_drops, &func_idx.resource_drop);

    emit_bump_restore(&mut f, bump_reset);

    // ── Phase 4: tail (async: task.return; sync: direct return).
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
    Ok(())
}

/// Async tail. Three shapes: void (no args); indirect_params (push
/// retptr scratch); flat (replay `lift_from_memory` loads).
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
