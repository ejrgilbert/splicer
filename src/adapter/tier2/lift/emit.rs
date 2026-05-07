//! Codegen: walk a [`LiftPlan`] and emit the wasm that writes one
//! cell per (param | result) into the cells slab, plus the result-
//! lift emission for Direct (sync flat) and Compound result kinds.

use wasm_encoder::{BlockType, Function, Instruction, MemArg, ValType};
use wit_bindgen_core::abi::lift_from_memory;
use wit_parser::{Resolve, SizeAlign};

use super::super::super::abi::cast;
use super::super::super::abi::emit::{
    direct_return_type, emit_bitcast, emit_cabi_realloc_call_runtime, wasm_type_to_val,
    I32_STORE_LOG2_ALIGN, I64_STORE_LOG2_ALIGN, I8_STORE_LOG2_ALIGN, OPTION_NONE, OPTION_SOME,
    SLICE_LEN_OFFSET, SLICE_PTR_OFFSET, STRING_FLAT_BYTES,
};
use super::super::super::abi::flat_types;
use super::super::super::abi::WasmEncoderBindgen;
use super::super::super::indices::{FrozenLocals, LocalsBuilder};
use super::super::cells::CellLayout;
use super::super::FuncDispatch;
use super::classify::ResultSourceLayout;
use super::plan::{ArmGuard, Cell, LiftPlan, ListSpec};
use super::sidetable::flags_info::FlagsRuntimeFill;
use super::sidetable::handle_info::HandleRuntimeFill;
use super::sidetable::variant_info::VariantRuntimeFill;
use super::sidetable::CellSideData;
use wit_parser::abi::WasmType;

/// Locals + pre-built load sequences used by the wrapper body.
/// Allocated once up front so all downstream emit phases (param lift,
/// hook calls, result lift, async task.return load) reference the same
/// indices. Result-lift-only locals (Compound addr + synth slot locals,
/// plus the pre-built `lift_from_memory` instruction sequence) live on
/// [`ResultEmitPlan`] instead — that type bundles the result emit's
/// per-variant data so the four-fields-must-agree invariant disappears
/// into the sum-type discriminant.
pub(crate) struct WrapperLocals {
    /// Scratch for the cell write address.
    pub addr: u32,
    /// Packed status from canon-async hook calls.
    pub st: u32,
    /// Waitable-set handle for the wait loop.
    pub ws: u32,
    /// i64 widening source for IntegerSignExt/ZeroExt.
    pub(super) ext64: u32,
    /// f64 promoted source for FloatingF32.
    pub(super) ext_f64: u32,
    /// Scratch i32 locals for joined-flat widening reads. `_a` lands
    /// the bitcast for any i32-arm leaf; `_b` is reserved for the
    /// second slot of a `Cell::Text` / `Cell::Bytes` so the ptr
    /// scratch survives the len read. Both unconditional — wasm
    /// liveness analysis drops them when unused.
    pub(super) widen_i32_a: u32,
    pub(super) widen_i32_b: u32,
    /// Cursor + count locals for the `Cell::Flags` bit-walk
    /// (re-used across every flags cell in a sequential wrapper body).
    pub(super) flags_addr: u32,
    pub(super) flags_count: u32,
    /// Length local for the `Cell::Char` utf-8 encoder; staged into
    /// `cell::text(ptr, len)`. Re-used across every char cell.
    pub(super) char_len: u32,
    /// Direct-return value when the export sig has a single flat
    /// result; `None` otherwise.
    pub result: Option<u32>,
    /// Address local that drives `lift_from_memory` for async
    /// `task.return` flat loads. `None` for sync, void async, and
    /// async with retptr-passthrough task.return.
    pub tr_addr: Option<u32>,
    /// i64 call-id local. Tier-2 always wires at least one hook
    /// (`build_tier2_adapter` bails otherwise), so this is always live.
    pub id_local: u32,
    /// Pre-built bindgen load sequence for async `task.return` flat
    /// args. `Some` exactly when [`Self::tr_addr`] is `Some`, sourced
    /// from `lift_from_memory` driven by the same builder that allocated
    /// every other local. Stored here (not synthesized at emit time) so
    /// every local the bindgen needed is already in [`FrozenLocals`].
    pub task_return_loads: Option<Vec<Instruction<'static>>>,
    /// Pre-built bindgen lower sequence that writes the wrapper's
    /// flat function params into the canonical params record. `Some`
    /// iff the async target uses `indirect_params` (canon-lower-async
    /// switched to pass-by-record); replayed at the handler-call site
    /// before the record's pointer is pushed. Sourced from
    /// `wit_bindgen_core::abi::lower_to_memory` driven by the same
    /// builder that allocated every other local — same rationale as
    /// [`Self::task_return_loads`].
    pub params_lower_seq: Option<Vec<Instruction<'static>>>,
    /// Bump-pointer snapshot at wrapper entry; restored at exit
    /// for stack-reset semantics on per-call `cabi_realloc`.
    pub saved_bump: u32,
    /// Base address of the active plan's cells slab. Set by the
    /// wrapper-body emitter before each [`emit_lift_plan`] call;
    /// reused across plans (each set overwrites the previous).
    pub cells_base: u32,
    /// Running cell-index counter driving the per-plan pre-pass;
    /// holds `total_cells` after the pass. Reused across plans.
    pub next_cell_idx: u32,
    /// Per-param list emit locals. `param_list_locals[i]` is parallel
    /// to `params[i].lift.plan.list_specs()`; empty for list-free params.
    pub param_list_locals: Vec<Vec<ListEmitLocals>>,
}

/// Per-function emit-time bundle for the result-side lift. Built once
/// in [`alloc_wrapper_locals`] from the layout-phase
/// [`super::classify::ResultLayout`] + the locals just allocated, then
/// consumed by phase-3 of the wrapper-body emitter via a pattern
/// match. Direct carries side-data inline; Compound borrows it.
pub(crate) enum ResultEmitPlan<'a> {
    /// Void or unsupported result: no lift fires.
    None,
    /// Sync flat return — source already in `source_local`.
    /// `side_data` carries any per-kind bookkeeping (Flags / Char /
    /// Handle); `None` for primitives that need none.
    Direct {
        cell: Cell,
        source_local: u32,
        side_data: CellSideData,
    },
    /// Retptr-loaded result. `addr_local` drives the
    /// `lift_from_memory`-built `loads` sequence; the wrapper
    /// `local.set`s values into `synth_locals` (LIFO) for the plan
    /// walker, with `local_base = synth_locals[0]`.
    Compound {
        plan: &'a LiftPlan,
        retptr_offset: i32,
        addr_local: u32,
        synth_locals: Vec<u32>,
        loads: Vec<Instruction<'static>>,
        side_refs: CellSideRefs<'a>,
        /// Per-list emit locals, parallel to `plan.list_specs()`.
        list_locals: Vec<ListEmitLocals>,
    },
}

/// Per-plan-cell side-table data borrowed off `ParamLayout` /
/// `ResultSourceLayout::Compound` for [`emit_lift_plan`]. One entry
/// per cell — adding a new side-table kind is a [`CellSideData`]
/// variant + a [`emit_cell_op`] arm, no field-shape changes here.
#[derive(Clone, Copy)]
pub(crate) struct CellSideRefs<'a> {
    pub cell_side: &'a [CellSideData],
}

/// Per-build context shared across every lift emit in a wrapper.
/// Bundles `cell_layout` + `cabi_realloc_idx` so per-call helpers
/// don't repeat them in every signature.
#[derive(Clone, Copy)]
pub(crate) struct LiftEmitCtx<'a> {
    pub cell_layout: &'a CellLayout,
    pub cabi_realloc_idx: u32,
}

/// Per-plan-walk cursor: the plan being emitted and the wrapper-local
/// offset added to every cell's plan-relative flat slot. Both travel
/// together — the plan is consulted for `widening_for(slot)` while
/// `local_base` resolves slots to absolute wrapper-local indices —
/// so they're bundled at every plan-walk boundary.
#[derive(Clone, Copy)]
pub(crate) struct PlanCursor<'a> {
    pub plan: &'a LiftPlan,
    pub local_base: u32,
}

/// Per-`Cell::ListOf` emit-time bundle. One entry per list-of cell
/// in plan order — parallel to [`LiftPlan::list_specs`].
pub(crate) struct ListEmitLocals {
    /// Cell-idx where this list's element cells begin (captured
    /// from the pre-pass running counter).
    pub start_i: u32,
    /// Captured source `len` flat slot value.
    pub len: u32,
    /// Per-call indices buffer base (`len * 4` bytes).
    pub indices_ptr: u32,
    /// Element-loop counter (0..len).
    pub j: u32,
    /// Per-iter source element address; drives `elem_loads`.
    pub elem_addr: u32,
    /// One local per element-plan flat slot, contiguous so plan
    /// slot N maps to `elem_flat_locals[0] + N`.
    pub elem_flat_locals: Vec<u32>,
    /// Pre-built `lift_from_memory` loads — pushes element flat
    /// values for capture into `elem_flat_locals` (LIFO).
    pub elem_loads: Vec<Instruction<'static>>,
    /// Canonical-ABI byte size of one element.
    pub elem_byte_size: u32,
    /// Side-data parallel to `element_plan.cells`; all-`None` while
    /// only scalar elements are supported.
    pub elem_cell_side: Vec<CellSideData>,
}

/// Allocate per-list emit locals + pre-build the `lift_from_memory`
/// loads for every `Cell::ListOf` in `plan`. Runs while the
/// [`LocalsBuilder`] is still live since the bindgen may allocate
/// scratch locals.
pub(super) fn alloc_list_emit_locals(
    plan: &LiftPlan,
    resolve: &Resolve,
    size_align: &SizeAlign,
    builder: &mut LocalsBuilder,
) -> Vec<ListEmitLocals> {
    plan.list_specs()
        .map(|spec: ListSpec<'_>| build_one_list_emit_locals(spec, resolve, size_align, builder))
        .collect()
}

fn build_one_list_emit_locals(
    spec: ListSpec<'_>,
    resolve: &Resolve,
    size_align: &SizeAlign,
    builder: &mut LocalsBuilder,
) -> ListEmitLocals {
    let start_i = builder.alloc_local(ValType::I32);
    let len = builder.alloc_local(ValType::I32);
    let indices_ptr = builder.alloc_local(ValType::I32);
    let j = builder.alloc_local(ValType::I32);
    let elem_addr = builder.alloc_local(ValType::I32);
    // Contiguous element flat-slot locals: plan slot N maps to
    // `elem_flat_locals[0] + N`.
    let elem_ty = spec.element_plan.source_ty;
    let flat = flat_types(resolve, &elem_ty, None)
        .expect("list element type must flatten within MAX_FLAT_PARAMS");
    let elem_flat_locals: Vec<u32> = flat
        .iter()
        .map(|wt| builder.alloc_local(wasm_type_to_val(*wt)))
        .collect();
    debug_assert!(
        elem_flat_locals.windows(2).all(|w| w[1] == w[0] + 1),
        "elem_flat_locals must be contiguous (plan slot N = elem_flat_locals[0] + N)",
    );
    let mut bindgen = WasmEncoderBindgen::new(size_align, elem_addr, builder);
    lift_from_memory(resolve, &mut bindgen, (), &elem_ty);
    let elem_loads = bindgen.into_instructions();
    let elem_byte_size = size_align.size(&elem_ty).size_wasm32() as u32;
    // Stub assumes every element cell folds to CellSideData::None.
    // When compound elements wire up, this fires so the stub gets
    // replaced with a real walk over `element_plan.cells`.
    debug_assert!(
        spec.element_plan
            .cells
            .iter()
            .all(|c| c.allowed_as_list_element()),
        "elem_cell_side stub assumes no side-data-bearing element cells",
    );
    let elem_cell_side = vec![CellSideData::None; spec.element_plan.cell_count() as usize];
    ListEmitLocals {
        start_i,
        len,
        indices_ptr,
        j,
        elem_addr,
        elem_flat_locals,
        elem_loads,
        elem_byte_size,
        elem_cell_side,
    }
}

/// Allocate every local the wrapper body will reference, build the
/// (data-driven) compound-result and async-task-return load sequences,
/// then [`LocalsBuilder::freeze`] the result and hand back the frozen
/// locals list. Taking `builder` by value is the typestate hinge: the
/// caller surrenders its ability to allocate further locals before
/// receiving the [`FrozenLocals`] that feeds
/// `Function::new_with_locals_types`, so "allocate after freeze" is a
/// compile error rather than a runtime panic when wasm validation
/// trips on out-of-range locals.
pub(crate) fn alloc_wrapper_locals<'a>(
    resolve: &Resolve,
    size_align: &SizeAlign,
    mut builder: LocalsBuilder,
    fd: &'a FuncDispatch,
    func: &wit_parser::Function,
) -> (WrapperLocals, ResultEmitPlan<'a>, FrozenLocals) {
    let addr = builder.alloc_local(ValType::I32);
    let st = builder.alloc_local(ValType::I32);
    let ws = builder.alloc_local(ValType::I32);
    let ext64 = builder.alloc_local(ValType::I64);
    let ext_f64 = builder.alloc_local(ValType::F64);
    let widen_i32_a = builder.alloc_local(ValType::I32);
    let widen_i32_b = builder.alloc_local(ValType::I32);
    let flags_addr = builder.alloc_local(ValType::I32);
    let flags_count = builder.alloc_local(ValType::I32);
    let char_len = builder.alloc_local(ValType::I32);
    let result = direct_return_type(&fd.export_sig).map(|t| builder.alloc_local(t));
    // Async with a non-retptr-passthrough task.return needs an
    // i32 addr local so `lift_from_memory` can flat-load result
    // values out of the retptr scratch.
    let tr_uses_flat_loads = fd
        .shape
        .task_return()
        .is_some_and(|tr| !tr.sig.indirect_params && fd.result_ty.is_some());
    let tr_addr = tr_uses_flat_loads.then(|| builder.alloc_local(ValType::I32));

    // Result-emit plan: discriminate on the layout-phase `ResultLayout`
    // and pull together the variant-specific locals/offsets/loads.
    // Compound allocates extra locals (one i32 addr + one synth per
    // flat slot) AND drives the bindgen for `lift_from_memory` —
    // bindgen may allocate further scratch locals, so this must run
    // before the locals list freezes.
    let result_emit = match fd.result_lift.as_ref() {
        None => ResultEmitPlan::None,
        Some(rl) => match &rl.source {
            ResultSourceLayout::Direct { cell, side_data } => ResultEmitPlan::Direct {
                cell: cell.clone(),
                source_local: result
                    .expect("ResultSourceLayout::Direct → direct-return local allocated"),
                side_data: side_data.clone(),
            },
            ResultSourceLayout::Compound {
                compound,
                retptr_offset,
                cell_side,
            } => {
                let side_refs = CellSideRefs { cell_side };
                let addr_local = builder.alloc_local(ValType::I32);
                let flat = flat_types(resolve, &compound.ty, None).unwrap_or_else(|| {
                    panic!(
                        "Compound result must flatten within MAX_FLAT_PARAMS ({}) — \
                             classify_result_lift only returns Compound for kinds that do",
                        Resolve::MAX_FLAT_PARAMS
                    )
                });
                assert_eq!(
                    flat.len(),
                    compound.plan.flat_slot_count as usize,
                    "canonical-ABI flat count (emit) must match classify-time plan"
                );
                // Synth locals are allocated contiguously; the emit
                // phase passes `synth_locals[0]` to `emit_lift_plan`
                // as the plan's `local_base`, so cell N's flat slot
                // resolves to `synth_locals[0] + N = synth_locals[N]`.
                let synth_locals: Vec<u32> = flat
                    .into_iter()
                    .map(|wt| builder.alloc_local(wasm_type_to_val(wt)))
                    .collect();
                debug_assert!(
                    synth_locals.windows(2).all(|w| w[1] == w[0] + 1),
                    "synth_locals must be contiguous (plan slot N = synth_locals[0] + N)",
                );
                let mut bindgen = WasmEncoderBindgen::new(size_align, addr_local, &mut builder);
                lift_from_memory(resolve, &mut bindgen, (), &compound.ty);
                let loads = bindgen.into_instructions();
                let list_locals =
                    alloc_list_emit_locals(&compound.plan, resolve, size_align, &mut builder);
                ResultEmitPlan::Compound {
                    plan: &compound.plan,
                    retptr_offset: *retptr_offset,
                    addr_local,
                    synth_locals,
                    loads,
                    side_refs,
                    list_locals,
                }
            }
        },
    };

    // Per-param list-of locals — must allocate before freeze. Empty
    // inner Vec for params with no list cells.
    let param_list_locals: Vec<Vec<ListEmitLocals>> = fd
        .params
        .iter()
        .map(|p| alloc_list_emit_locals(&p.lift.plan, resolve, size_align, &mut builder))
        .collect();

    // Async task.return flat-loads run a second `lift_from_memory`
    // pass over `result_ty`; that bindgen may allocate scratch locals
    // too, so it has to happen before we freeze.
    let task_return_loads: Option<Vec<Instruction<'static>>> = tr_addr.map(|addr_local| {
        let result_ty = fd
            .result_ty
            .as_ref()
            .expect("flat task.return loads → result_ty");
        let mut bindgen = WasmEncoderBindgen::new(size_align, addr_local, &mut builder);
        lift_from_memory(resolve, &mut bindgen, (), result_ty);
        bindgen.into_instructions()
    });

    // Indirect-params lower-to-memory sequence for async fns whose
    // flat params overflowed `MAX_FLAT_ASYNC_PARAMS`. Driven by
    // `wit_bindgen_core::abi::lower_to_memory` through the same
    // builder, so any addr / store-tmp scratch lands in `frozen`.
    let params_lower_seq: Option<Vec<Instruction<'static>>> =
        fd.import_sig.indirect_params.then(|| {
            let base = fd
                .params_record_offset
                .expect("indirect_params → params_record_offset reserved");
            super::super::super::abi::emit::build_lower_params_to_memory(
                resolve,
                size_align,
                &mut builder,
                func,
                base,
            )
        });

    // i64 call-id local. Tier-2 generation requires at least one hook
    // (`build_tier2_adapter` bails otherwise), so this is always live.
    let id_local = builder.alloc_local(ValType::I64);
    let saved_bump = builder.alloc_local(ValType::I32);
    let cells_base = builder.alloc_local(ValType::I32);
    let next_cell_idx = builder.alloc_local(ValType::I32);

    let frozen = builder.freeze();
    (
        WrapperLocals {
            addr,
            st,
            ws,
            ext64,
            ext_f64,
            widen_i32_a,
            widen_i32_b,
            flags_addr,
            flags_count,
            char_len,
            result,
            tr_addr,
            id_local,
            task_return_loads,
            params_lower_seq,
            saved_bump,
            cells_base,
            next_cell_idx,
            param_list_locals,
        },
        result_emit,
        frozen,
    )
}

/// Emit the wasm that lifts one plan into its cells slab. Walks
/// `plan.cells` in allocation order and, for each cell, sets
/// `lcl.addr` to that cell's absolute address (`cells_base + i *
/// cell_size`, computed at runtime from the `cells_base` local set
/// by the caller) and dispatches on the cell's variant. Cells
/// reference plan-relative flat slots; `local_base` is added per-cell
/// to recover the absolute wasm-local index — params pass the
/// cumulative slot cursor, compound results pass `synth_locals[0]`.
///
/// `list_locals` is parallel to [`LiftPlan::list_specs`] order; for
/// each `Cell::ListOf` encountered the matching slot drives the
/// per-list element loop. Empty for plans without lists.
/// `cabi_realloc_idx` lets the list-of arm allocate the per-list
/// indices buffer at runtime.
pub(crate) fn emit_lift_plan(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    plan: &LiftPlan,
    side_refs: CellSideRefs<'_>,
    local_base: u32,
    lcl: &WrapperLocals,
    list_locals: &[ListEmitLocals],
) {
    assert_eq!(
        side_refs.cell_side.len(),
        plan.cells.len(),
        "side-table data (emit input) must have one entry per classify-time plan cell"
    );
    debug_assert_eq!(
        list_locals.len(),
        plan.list_specs().count(),
        "per-plan list_locals must be parallel to plan.list_specs()",
    );
    for (cell_idx, op) in plan.cells.iter().enumerate() {
        f.instructions().local_get(lcl.cells_base);
        if cell_idx > 0 {
            f.instructions()
                .i32_const((cell_idx as u32 * ctx.cell_layout.size) as i32);
            f.instructions().i32_add();
        }
        f.instructions().local_set(lcl.addr);
        let list_slot = match op {
            Cell::ListOf { list_idx, .. } => Some(&list_locals[*list_idx as usize]),
            _ => None,
        };
        emit_cell_op(
            f,
            ctx,
            PlanCursor { plan, local_base },
            op,
            &side_refs.cell_side[cell_idx],
            lcl,
            list_slot,
        );
    }
}

/// Resolve a leaf-level flat-slot read, applying any joined-flat
/// widening bitcast recorded in `plan`. The returned `u32` is always
/// an absolute wrapper-local index — either the wrapper's flat-param
/// slot (`local_base + flat_slot`) when no bitcast is needed, or one
/// of the typed scratches (`lcl.widen_i32_a` / `lcl.ext64` /
/// `lcl.ext_f64`) materialized via `local.get + bitcast + local.set`.
/// F32-arm leaves use `push_widened_get` inline (no helper takes an
/// f32 local idx).
fn pin_leaf_flat(
    f: &mut Function,
    plan: &LiftPlan,
    local_base: u32,
    flat_slot: u32,
    arm: WasmType,
    lcl: &WrapperLocals,
) -> u32 {
    pin_leaf_flat_with_i32_scratch(f, plan, local_base, flat_slot, arm, lcl.widen_i32_a, lcl)
}

/// Inner form of [`pin_leaf_flat`] that lets the caller name the i32
/// scratch — only `Cell::Text` / `Cell::Bytes` need this (their two
/// i32 slots can both widen in the same cell). Other arm types ignore
/// `scratch_i32` and pick `lcl.ext64` / `lcl.ext_f64` by `arm`.
fn pin_leaf_flat_with_i32_scratch(
    f: &mut Function,
    plan: &LiftPlan,
    local_base: u32,
    flat_slot: u32,
    arm: WasmType,
    scratch_i32: u32,
    lcl: &WrapperLocals,
) -> u32 {
    let Some(joined) = plan.widening_for(flat_slot) else {
        return local_base + flat_slot;
    };
    let bc = cast(joined, arm);
    if matches!(bc, wit_bindgen_core::abi::Bitcast::None) {
        // Joined matches this arm's per-position type — another arm
        // widened the slot, but we don't need to.
        return local_base + flat_slot;
    }
    f.instructions().local_get(local_base + flat_slot);
    emit_bitcast(f, &bc);
    let scratch = match arm {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => scratch_i32,
        WasmType::I64 | WasmType::PointerOrI64 => lcl.ext64,
        WasmType::F64 => lcl.ext_f64,
        WasmType::F32 => panic!("F32 widening must use push_widened_get inline"),
    };
    f.instructions().local_set(scratch);
    scratch
}

/// Pin both i32 slots of a `Text` / `Bytes` cell into distinct
/// scratches (`widen_i32_a` for ptr, `widen_i32_b` for len) so the
/// ptr value survives the len read. Returns `(ptr_local, len_local)`
/// for the cell-layout helper.
fn pin_text_bytes_slots(
    f: &mut Function,
    plan: &LiftPlan,
    local_base: u32,
    ptr_slot: u32,
    len_slot: u32,
    lcl: &WrapperLocals,
) -> (u32, u32) {
    let ptr = pin_leaf_flat(f, plan, local_base, ptr_slot, WasmType::I32, lcl);
    let len = pin_leaf_flat_with_i32_scratch(
        f,
        plan,
        local_base,
        len_slot,
        WasmType::I32,
        lcl.widen_i32_b,
        lcl,
    );
    (ptr, len)
}

/// `local.get` then (when widening is recorded for `flat_slot`) the
/// joined→arm bitcast — leaves the arm-typed value on the wasm stack.
/// Used for cells that do their own follow-up (extend / promote /
/// `if_`) rather than handing a local index to a helper.
fn push_widened_get(
    f: &mut Function,
    plan: &LiftPlan,
    local_base: u32,
    flat_slot: u32,
    arm: WasmType,
) {
    f.instructions().local_get(local_base + flat_slot);
    if let Some(joined) = plan.widening_for(flat_slot) {
        emit_bitcast(f, &cast(joined, arm));
    }
}

/// Open one `if disc == expected` per guard. Body lands inside the
/// innermost block; pair with [`emit_close_arm_guards`].
fn emit_open_arm_guards(f: &mut Function, plan: &LiftPlan, local_base: u32, guards: &[ArmGuard]) {
    for guard in guards {
        push_widened_get(f, plan, local_base, guard.disc_slot, WasmType::I32);
        f.instructions().i32_const(guard.expected_disc as i32);
        f.instructions().i32_eq();
        f.instructions().if_(BlockType::Empty);
    }
}

/// Close `n` `if` blocks opened by [`emit_open_arm_guards`]. `n`
/// must equal the guard count passed at open or wasm validation
/// will reject the function.
fn emit_close_arm_guards(f: &mut Function, n: usize) {
    for _ in 0..n {
        f.instructions().end();
    }
}

/// Pre-pass that initializes `lcl.next_cell_idx` to the plan's
/// static cell count, then bumps it by `len · elem_count` per list
/// (capturing each list's `start_i` and `len` along the way).
/// Lists nested in joined arms disc-gate the bump so the inactive
/// arm's bytes can't bloat the slab — locals zero-init keeps
/// `ll.len` / `ll.start_i` defined on the inactive path. Per-list
/// trap guards the `i32` mul + add against silent wrap; see
/// `emit_trap_if_list_overflows_cell_slab`.
pub(crate) fn emit_list_pre_pass(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    plan: &LiftPlan,
    list_locals: &[ListEmitLocals],
    local_base: u32,
    lcl: &WrapperLocals,
) {
    debug_assert_eq!(
        list_locals.len(),
        plan.list_specs().count(),
        "per-plan list_locals must be parallel to plan.list_specs()",
    );
    f.instructions().i32_const(plan.cell_count() as i32);
    f.instructions().local_set(lcl.next_cell_idx);
    for spec in plan.list_specs() {
        let ll = &list_locals[spec.list_idx as usize];
        emit_open_arm_guards(f, plan, local_base, spec.arm_guards);
        f.instructions().local_get(lcl.next_cell_idx);
        f.instructions().local_set(ll.start_i);
        push_widened_get(f, plan, local_base, spec.len_slot, WasmType::I32);
        f.instructions().local_set(ll.len);
        let elem_count = spec.element_plan.cell_count();
        super::super::super::abi::emit::emit_trap_if_list_overflows_cell_slab(
            f,
            ll.len,
            elem_count,
            lcl.next_cell_idx,
            ctx.cell_layout.size,
        );
        f.instructions().local_get(lcl.next_cell_idx);
        f.instructions().local_get(ll.len);
        if elem_count != 1 {
            f.instructions().i32_const(elem_count as i32);
            f.instructions().i32_mul();
        }
        f.instructions().i32_add();
        f.instructions().local_set(lcl.next_cell_idx);
        emit_close_arm_guards(f, spec.arm_guards.len());
    }
}

/// Emit one cell's worth of wasm at the address held in `lcl.addr`.
/// `local_base` is added to each plan-relative flat-slot. `list_slot`
/// is `Some` exactly for `Cell::ListOf`. New [`Cell`] variants add an
/// arm here (no `_` catchall). `plan` carries the joined-flat widening
/// table consulted at every leaf flat-slot read.
fn emit_cell_op(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    cur: PlanCursor<'_>,
    op: &Cell,
    side_data: &CellSideData,
    lcl: &WrapperLocals,
    list_slot: Option<&ListEmitLocals>,
) {
    let PlanCursor { plan, local_base } = cur;
    let addr = lcl.addr;
    let cell_layout = ctx.cell_layout;
    match op {
        Cell::Bool { flat_slot } => {
            let src = pin_leaf_flat(f, plan, local_base, *flat_slot, WasmType::I32, lcl);
            cell_layout.emit_bool(f, addr, src);
        }
        Cell::IntegerSignExt { flat_slot } => {
            push_widened_get(f, plan, local_base, *flat_slot, WasmType::I32);
            f.instructions().i64_extend_i32_s();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        Cell::IntegerZeroExt { flat_slot } => {
            push_widened_get(f, plan, local_base, *flat_slot, WasmType::I32);
            f.instructions().i64_extend_i32_u();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        Cell::Integer64 { flat_slot } => {
            let src = pin_leaf_flat(f, plan, local_base, *flat_slot, WasmType::I64, lcl);
            cell_layout.emit_integer(f, addr, src);
        }
        Cell::FloatingF32 { flat_slot } => {
            push_widened_get(f, plan, local_base, *flat_slot, WasmType::F32);
            f.instructions().f64_promote_f32();
            f.instructions().local_set(lcl.ext_f64);
            cell_layout.emit_floating(f, addr, lcl.ext_f64);
        }
        Cell::FloatingF64 { flat_slot } => {
            let src = pin_leaf_flat(f, plan, local_base, *flat_slot, WasmType::F64, lcl);
            cell_layout.emit_floating(f, addr, src);
        }
        Cell::Text { ptr_slot, len_slot } => {
            let (ptr, len) = pin_text_bytes_slots(f, plan, local_base, *ptr_slot, *len_slot, lcl);
            cell_layout.emit_text(f, addr, ptr, len);
        }
        Cell::Bytes { ptr_slot, len_slot } => {
            let (ptr, len) = pin_text_bytes_slots(f, plan, local_base, *ptr_slot, *len_slot, lcl);
            cell_layout.emit_bytes(f, addr, ptr, len);
        }
        Cell::EnumCase { flat_slot, .. } => {
            let src = pin_leaf_flat(f, plan, local_base, *flat_slot, WasmType::I32, lcl);
            cell_layout.emit_enum_case(f, addr, src);
        }
        Cell::RecordOf { .. } => {
            let CellSideData::Record { idx } = side_data else {
                panic!("RecordOf cell paired with non-Record side data {side_data:?}");
            };
            cell_layout.emit_record_of(f, addr, *idx);
        }
        Cell::TupleOf { .. } => {
            let CellSideData::Tuple { slice } = side_data else {
                panic!("TupleOf cell paired with non-Tuple side data {side_data:?}");
            };
            cell_layout.emit_tuple_of(f, addr, slice.off, slice.len);
        }
        Cell::Option {
            disc_slot,
            child_idx,
        } => {
            push_widened_get(f, plan, local_base, *disc_slot, WasmType::I32);
            f.instructions().if_(BlockType::Empty);
            cell_layout.emit_option_some(f, addr, *child_idx);
            f.instructions().else_();
            cell_layout.emit_option_none(f, addr);
            f.instructions().end();
        }
        Cell::Result {
            disc_slot,
            ok_idx,
            err_idx,
        } => {
            // wasm `if` fires on non-zero, so err goes in the if block.
            push_widened_get(f, plan, local_base, *disc_slot, WasmType::I32);
            f.instructions().if_(BlockType::Empty);
            cell_layout.emit_result_err(f, addr, err_idx.is_some(), err_idx.unwrap_or(0));
            f.instructions().else_();
            cell_layout.emit_result_ok(f, addr, ok_idx.is_some(), ok_idx.unwrap_or(0));
            f.instructions().end();
        }
        Cell::Flags { flat_slot, .. } => {
            let CellSideData::Flags(fill) = side_data else {
                panic!("Flags cell paired with non-Flags side data {side_data:?}");
            };
            let src = pin_leaf_flat(f, plan, local_base, *flat_slot, WasmType::I32, lcl);
            emit_flags_runtime_fill(f, src, fill, lcl);
            cell_layout.emit_flags_set(f, lcl.addr, fill.side_table_idx);
        }
        Cell::Variant { disc_slot, .. } => {
            let CellSideData::Variant(fill) = side_data else {
                panic!("Variant cell paired with non-Variant side data {side_data:?}");
            };
            let src = pin_leaf_flat(f, plan, local_base, *disc_slot, WasmType::I32, lcl);
            emit_variant_runtime_fill(f, src, fill);
            cell_layout.emit_variant_case(f, lcl.addr, fill.side_table_idx);
        }
        Cell::Char { flat_slot } => {
            let CellSideData::Char { scratch_addr } = side_data else {
                panic!("Char cell paired with non-Char side data {side_data:?}");
            };
            let src = pin_leaf_flat(f, plan, local_base, *flat_slot, WasmType::I32, lcl);
            cell_layout.emit_char(f, lcl.addr, src, *scratch_addr, lcl.char_len);
        }
        Cell::Handle {
            flat_slot, kind, ..
        } => {
            let CellSideData::Handle(fill) = side_data else {
                panic!("Handle cell paired with non-Handle side data {side_data:?}");
            };
            let src = pin_leaf_flat(f, plan, local_base, *flat_slot, WasmType::I32, lcl);
            emit_handle_runtime_fill(f, src, fill);
            cell_layout.emit_handle_cell(f, lcl.addr, kind.cell_disc_case(), fill.side_table_idx);
        }
        Cell::ListOf {
            ptr_slot,
            element_plan,
            arm_guards,
            ..
        } => {
            let ll =
                list_slot.expect("ListOf cell must arrive with a matching ListEmitLocals slot");
            // Disc-gate cabi_realloc + the element loop so an inactive
            // sibling arm's bytes don't surface as `len`.
            emit_open_arm_guards(f, plan, local_base, arm_guards);
            let ptr = pin_leaf_flat(f, plan, local_base, *ptr_slot, WasmType::I32, lcl);
            emit_list_of_arm(f, ctx, ll, ptr, element_plan, lcl);
            emit_close_arm_guards(f, arm_guards.len());
        }
    }
}

/// Emit one `Cell::ListOf` arm: write the list-of cell payload at
/// `lcl.addr`, allocate the per-call indices buffer, then loop
/// `j ∈ 0..len` lifting each element.
fn emit_list_of_arm(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    ll: &ListEmitLocals,
    list_ptr_local: u32,
    element_plan: &LiftPlan,
    lcl: &WrapperLocals,
) {
    let elem_cell = &element_plan.cells[0];
    let cell_layout = ctx.cell_layout;
    emit_cabi_realloc_call_runtime(f, ctx.cabi_realloc_idx, 4, ll.len, 4, ll.indices_ptr);
    cell_layout.emit_list_of(f, lcl.addr, ll.indices_ptr, ll.len);

    // for (j = 0; j < len; j++) { ... }
    f.instructions().i32_const(0);
    f.instructions().local_set(ll.j);
    f.instructions().block(BlockType::Empty);
    f.instructions().loop_(BlockType::Empty);
    f.instructions().local_get(ll.j);
    f.instructions().local_get(ll.len);
    f.instructions().i32_ge_u();
    f.instructions().br_if(1);

    // elem_addr = list_ptr + j * elem_byte_size
    f.instructions().local_get(list_ptr_local);
    f.instructions().local_get(ll.j);
    if ll.elem_byte_size != 1 {
        f.instructions().i32_const(ll.elem_byte_size as i32);
        f.instructions().i32_mul();
    }
    f.instructions().i32_add();
    f.instructions().local_set(ll.elem_addr);

    // Lift element flat values from memory into elem_flat_locals (LIFO capture).
    for inst in &ll.elem_loads {
        f.instruction(inst);
    }
    for &local in ll.elem_flat_locals.iter().rev() {
        f.instructions().local_set(local);
    }

    // lcl.addr = cells_base + (start_i + j) * cell_size
    f.instructions().local_get(lcl.cells_base);
    f.instructions().local_get(ll.start_i);
    f.instructions().local_get(ll.j);
    f.instructions().i32_add();
    f.instructions().i32_const(cell_layout.size as i32);
    f.instructions().i32_mul();
    f.instructions().i32_add();
    f.instructions().local_set(lcl.addr);

    emit_cell_op(
        f,
        ctx,
        PlanCursor {
            plan: element_plan,
            local_base: ll.elem_flat_locals[0],
        },
        elem_cell,
        &ll.elem_cell_side[0],
        lcl,
        None,
    );

    // indices_ptr[j*4] = start_i + j
    f.instructions().local_get(ll.indices_ptr);
    f.instructions().local_get(ll.j);
    f.instructions().i32_const(4);
    f.instructions().i32_mul();
    f.instructions().i32_add();
    f.instructions().local_get(ll.start_i);
    f.instructions().local_get(ll.j);
    f.instructions().i32_add();
    f.instructions().i32_store(MemArg {
        offset: 0,
        align: I32_STORE_LOG2_ALIGN,
        memory_index: 0,
    });

    f.instructions().local_get(ll.j);
    f.instructions().i32_const(1);
    f.instructions().i32_add();
    f.instructions().local_set(ll.j);
    f.instructions().br(0);
    f.instructions().end(); // loop
    f.instructions().end(); // block
}

/// Patch one `Cell::Handle`'s `id: u64` slot per call: zero-extend
/// the i32 handle bits. Per-instance correlation (same bits → same
/// id) — see `handle-info` in `wit/common/world.wit`.
fn emit_handle_runtime_fill(f: &mut Function, handle_local: u32, fill: &HandleRuntimeFill) {
    let id_addr = fill
        .id_addr
        .expect("id_addr unset — layout must run back_fill_handle_id_addrs");
    f.instructions().i32_const(id_addr);
    f.instructions().local_get(handle_local);
    f.instructions().i64_extend_i32_u();
    f.instructions().i64_store(MemArg {
        offset: 0,
        align: I64_STORE_LOG2_ALIGN,
        memory_index: 0,
    });
}

/// Per-bit unrolled bit-walk filling the cell's scratch buffer with
/// `(name_ptr, name_len)` pairs and patching `set-flags.len`. Unrolled
/// rather than looped — at ≤ 8 bits per typical flag type the
/// overhead of a counter + `br_if` outweighs the static instructions.
/// Single-threaded today; the static buffer is unsafe under concurrent
/// calls (revisit when tier-2 grows concurrency).
fn emit_flags_runtime_fill(
    f: &mut Function,
    bitmask_local: u32,
    fill: &FlagsRuntimeFill,
    lcl: &WrapperLocals,
) {
    let store_i32 = |off: u32| MemArg {
        offset: off as u64,
        align: I32_STORE_LOG2_ALIGN,
        memory_index: 0,
    };

    f.instructions().i32_const(fill.scratch_addr);
    f.instructions().local_set(lcl.flags_addr);
    f.instructions().i32_const(0);
    f.instructions().local_set(lcl.flags_count);

    for (i, name) in fill.flag_names.iter().enumerate() {
        // (bitmask >> i) & 1
        f.instructions().local_get(bitmask_local);
        f.instructions().i32_const(i as i32);
        f.instructions().i32_shr_u();
        f.instructions().i32_const(1);
        f.instructions().i32_and();
        f.instructions().if_(BlockType::Empty);
        // *flags_addr = name.off; *(flags_addr + SLICE_LEN_OFFSET) = name.len
        f.instructions().local_get(lcl.flags_addr);
        f.instructions().i32_const(name.off as i32);
        f.instructions().i32_store(store_i32(SLICE_PTR_OFFSET));
        f.instructions().local_get(lcl.flags_addr);
        f.instructions().i32_const(name.len as i32);
        f.instructions().i32_store(store_i32(SLICE_LEN_OFFSET));
        // flags_addr += sizeof(string); flags_count += 1
        f.instructions().local_get(lcl.flags_addr);
        f.instructions().i32_const(STRING_FLAT_BYTES as i32);
        f.instructions().i32_add();
        f.instructions().local_set(lcl.flags_addr);
        f.instructions().local_get(lcl.flags_count);
        f.instructions().i32_const(1);
        f.instructions().i32_add();
        f.instructions().local_set(lcl.flags_count);
        f.instructions().end();
    }

    let len_addr = fill
        .set_flags_len_addr
        .expect("set_flags_len_addr unset — layout must run back_fill_flags_len_addrs");
    f.instructions().i32_const(len_addr);
    f.instructions().local_get(lcl.flags_count);
    f.instructions().i32_store(store_i32(0));
}

/// N-way disc dispatch for one `Cell::Variant` cell. For each case
/// `i ∈ 0..N` the wrapper writes:
///   - `case_names[i]` `(ptr, len)` into `case_name_addr`
///   - option<u32> at `payload_disc_addr` / `payload_value_addr`:
///     `some(child_idx)` for payload-bearing cases, `none` for unit
///
/// Encoded as nested if/else (compares disc to each case_idx). For
/// typical variants (≤ 8 cases) the nested depth is manageable;
/// `br_table` is a future optimization. Same single-threaded
/// constraint as flags's bit-walk — the static segment is unsafe
/// under concurrent calls.
fn emit_variant_runtime_fill(f: &mut Function, disc_local: u32, fill: &VariantRuntimeFill) {
    let store_i32 = |off: u32| MemArg {
        offset: off as u64,
        align: I32_STORE_LOG2_ALIGN,
        memory_index: 0,
    };
    let store_i8 = |off: u32| MemArg {
        offset: off as u64,
        align: I8_STORE_LOG2_ALIGN,
        memory_index: 0,
    };

    let case_name_addr = fill
        .case_name_addr
        .expect("case_name_addr unset — layout must run back_fill_variant_entry_addrs");
    let payload_disc_addr = fill
        .payload_disc_addr
        .expect("payload_disc_addr unset — layout must run back_fill_variant_entry_addrs");
    let payload_value_addr = fill
        .payload_value_addr
        .expect("payload_value_addr unset — layout must run back_fill_variant_entry_addrs");

    debug_assert_eq!(fill.case_names.len(), fill.per_case_payload.len());

    // Nested if/else: for each case `i`, `if disc == i { write case
    // i's data }` else recurse to the next case. The last arm has
    // no else (unreachable canonical-ABI disc out of range — wasm
    // validators don't require unreachable for completeness here
    // since the ABI guarantees disc < N).
    for (i, name) in fill.case_names.iter().enumerate() {
        let is_last = i + 1 == fill.case_names.len();
        if !is_last {
            f.instructions().local_get(disc_local);
            f.instructions().i32_const(i as i32);
            f.instructions().i32_eq();
            f.instructions().if_(BlockType::Empty);
        }
        // case-name = case_names[i]
        f.instructions().i32_const(case_name_addr);
        f.instructions().i32_const(name.off as i32);
        f.instructions().i32_store(store_i32(SLICE_PTR_OFFSET));
        f.instructions().i32_const(case_name_addr);
        f.instructions().i32_const(name.len as i32);
        f.instructions().i32_store(store_i32(SLICE_LEN_OFFSET));
        // payload = some(child_idx) or none
        match fill.per_case_payload[i] {
            Some(child_idx) => {
                f.instructions().i32_const(payload_disc_addr);
                f.instructions().i32_const(OPTION_SOME as i32);
                f.instructions().i32_store8(store_i8(0));
                f.instructions().i32_const(payload_value_addr);
                f.instructions().i32_const(child_idx as i32);
                f.instructions().i32_store(store_i32(0));
            }
            None => {
                f.instructions().i32_const(payload_disc_addr);
                f.instructions().i32_const(OPTION_NONE as i32);
                f.instructions().i32_store8(store_i8(0));
                // value slot left untouched (irrelevant when disc=0)
            }
        }
        if !is_last {
            f.instructions().else_();
        }
    }
    // Close all the nested `if`s — N-1 ends.
    for _ in 0..fill.case_names.len().saturating_sub(1) {
        f.instructions().end();
    }
}

/// Lift a Direct (sync flat return) result. Only single-flat-slot
/// kinds reach here — multi-slot kinds (Text/Bytes) and compound
/// shapes always retptr and route through Compound. The cell's
/// `flat_slot` field is ignored — `source` is `lcl.result` directly.
fn emit_lift_kind(
    f: &mut Function,
    cell_layout: &CellLayout,
    cell: &Cell,
    side_data: &CellSideData,
    source: u32,
    lcl: &WrapperLocals,
) {
    let addr = lcl.addr;
    match cell {
        Cell::Bool { .. } => cell_layout.emit_bool(f, addr, source),
        Cell::IntegerSignExt { .. } => {
            f.instructions().local_get(source);
            f.instructions().i64_extend_i32_s();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        Cell::IntegerZeroExt { .. } => {
            f.instructions().local_get(source);
            f.instructions().i64_extend_i32_u();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        Cell::Integer64 { .. } => cell_layout.emit_integer(f, addr, source),
        Cell::FloatingF32 { .. } => {
            f.instructions().local_get(source);
            f.instructions().f64_promote_f32();
            f.instructions().local_set(lcl.ext_f64);
            cell_layout.emit_floating(f, addr, lcl.ext_f64);
        }
        Cell::FloatingF64 { .. } => cell_layout.emit_floating(f, addr, source),
        Cell::EnumCase { .. } => cell_layout.emit_enum_case(f, addr, source),
        Cell::Flags { .. } => {
            let CellSideData::Flags(fill) = side_data else {
                panic!("Flags cell paired with non-Flags side data {side_data:?}");
            };
            emit_flags_runtime_fill(f, source, fill, lcl);
            cell_layout.emit_flags_set(f, addr, fill.side_table_idx);
        }
        Cell::Char { .. } => {
            let CellSideData::Char { scratch_addr } = side_data else {
                panic!("Char cell paired with non-Char side data {side_data:?}");
            };
            cell_layout.emit_char(f, addr, source, *scratch_addr, lcl.char_len);
        }
        Cell::Handle { kind, .. } => {
            let CellSideData::Handle(fill) = side_data else {
                panic!("Handle cell paired with non-Handle side data {side_data:?}");
            };
            emit_handle_runtime_fill(f, source, fill);
            cell_layout.emit_handle_cell(f, addr, kind.cell_disc_case(), fill.side_table_idx);
        }
        // Multi-slot + compound + un-wired kinds always retptr;
        // classify_result_lift routes them through Compound.
        Cell::Text { .. }
        | Cell::Bytes { .. }
        | Cell::RecordOf { .. }
        | Cell::TupleOf { .. }
        | Cell::ListOf { .. }
        | Cell::Option { .. }
        | Cell::Result { .. }
        | Cell::Variant { .. } => unreachable!(
            "emit_lift_kind reached non-Direct Cell {cell:?} — \
             classify_result_lift should have routed it through Compound"
        ),
    }
}

/// Emit the wasm to lift a single-cell result value into the cell at
/// `lcl.addr`. Direct primitive returns read from `lcl.result`;
/// Text/Bytes returns load `(ptr, len)` from retptr scratch into
/// `ptr_scratch` / `len_scratch` and lift those.
///
/// Compound results don't go through here — their cells aren't a
/// single one-shot write, so the wrapper-body emitter walks them via
/// [`emit_lift_plan`] after capturing the retptr-loaded flat slots
/// into synthetic locals.
pub(crate) fn emit_lift_result(
    f: &mut Function,
    cell_layout: &CellLayout,
    plan: &ResultEmitPlan<'_>,
    lcl: &WrapperLocals,
) {
    match plan {
        ResultEmitPlan::Direct {
            cell,
            source_local,
            side_data,
        } => {
            emit_lift_kind(f, cell_layout, cell, side_data, *source_local, lcl);
        }
        ResultEmitPlan::Compound { .. } | ResultEmitPlan::None => unreachable!(
            "Compound is emitted via emit_lift_compound_prefix + emit_lift_plan; \
             emit_lift_result handles only Direct sources"
        ),
    }
}

/// Emit the wasm prefix for a compound result: load the result's
/// canonical-ABI bytes from `retptr_offset` via the pre-built
/// `lift_from_memory` instruction sequence, then capture each flat
/// value into a synthetic local in REVERSE order (the wasm stack is
/// LIFO — the last-pushed value is the highest-indexed flat slot).
///
/// After this returns, the synthetic locals at `synth_locals[0]..
/// synth_locals[N]` hold the result's flat values in canonical-ABI
/// order, ready for [`emit_lift_plan`] (called with `local_base =
/// synth_locals[0]`) to walk the cell plan and recover the absolute
/// synth-local indices.
pub(crate) fn emit_lift_compound_prefix(
    f: &mut Function,
    plan_flat_slot_count: u32,
    retptr_offset: i32,
    loads: &[Instruction<'static>],
    addr_local: u32,
    synth_locals: &[u32],
) {
    assert_eq!(
        synth_locals.len(),
        plan_flat_slot_count as usize,
        "synthetic-local count (emit) must match classify-time plan flat slot count"
    );
    // Stage retptr_offset into the addr local that the pre-built
    // bindgen loads read from.
    f.instructions().i32_const(retptr_offset);
    f.instructions().local_set(addr_local);
    // Push canonical-ABI flat values onto the wasm value stack.
    for inst in loads {
        f.instruction(inst);
    }
    // local.set in reverse order: top-of-stack is the LAST pushed (=
    // highest flat-slot index). Working back to slot 0.
    for &local in synth_locals.iter().rev() {
        f.instructions().local_set(local);
    }
}
