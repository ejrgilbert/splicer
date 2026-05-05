//! Codegen: walk a [`LiftPlan`] and emit the wasm that writes one
//! cell per (param | result) into the cells slab, plus the result-
//! lift emission for direct / retptr-pair / compound result kinds.

use wasm_encoder::{BlockType, Function, Instruction, MemArg, ValType};
use wit_bindgen_core::abi::lift_from_memory;
use wit_parser::{Resolve, SizeAlign};

use super::super::super::abi::emit::{
    direct_return_type, wasm_type_to_val, BlobSlice, SLICE_LEN_OFFSET, SLICE_PTR_OFFSET,
};
use super::super::super::abi::WasmEncoderBindgen;
use super::super::super::indices::{FrozenLocals, LocalsBuilder};
use super::super::cells::CellLayout;
use super::super::FuncDispatch;
use super::classify::ResultSourceLayout;
use super::plan::{Cell, LiftPlan};

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
}

/// Per-function emit-time bundle for the result-side lift. Built once
/// in [`alloc_wrapper_locals`] from the layout-phase
/// [`super::classify::ResultLayout`] + the locals just allocated, then
/// consumed by the wrapper-body emitter's phase-3 result-lift block
/// via a single pattern match. Replaces the prior pile of parallel
/// `Option`-shaped fields whose Some-ness had to agree by hand.
/// Borrows the layout-time per-cell record-info index map from the
/// owning [`FuncDispatch`].
pub(crate) enum ResultEmitPlan<'a> {
    /// Void function or unsupported result kind: no lift fires.
    None,
    /// Direct primitive return — source value already in
    /// `source_local` (captured from the handler's flat return after
    /// the call). [`Cell`] carries the variant tag for emit dispatch.
    Direct { cell: Cell, source_local: u32 },
    /// `(ptr, len)` pair lives at `retptr_offset` in static scratch.
    /// The wrapper loads the pair into `ptr_local` / `len_local`
    /// before lifting (today these are always `lcl.ptr_scratch` /
    /// `lcl.len_scratch` — the variant carries them so the consumer
    /// doesn't re-thread `&WrapperLocals` for that lookup).
    RetptrPair {
        cell: Cell,
        retptr_offset: i32,
        ptr_local: u32,
        len_local: u32,
    },
    /// Compound result: classify-time cell plan + layout offsets +
    /// emit-time locals/loads. `addr_local` drives the
    /// `lift_from_memory`-built `loads` sequence (which pushes
    /// canonical-ABI flat values onto the wasm stack); the wrapper
    /// then `local.set`s those into `synth_locals` (in reverse) for
    /// the plan walker. The plan is borrowed straight from
    /// [`super::classify::CompoundResult::plan`] — its cells hold
    /// plan-relative flat slots, and the emit phase pairs them with
    /// `local_base = synth_locals[0]` to recover absolute wasm-local
    /// indices. `record_info_cell_idx` is the layout-phase per-cell
    /// `Cell::RecordOf` side-table index map (borrowed off
    /// [`ResultSourceLayout::Compound`]).
    Compound {
        plan: &'a LiftPlan,
        retptr_offset: i32,
        addr_local: u32,
        synth_locals: Vec<u32>,
        loads: Vec<Instruction<'static>>,
        side_refs: CellSideRefs<'a>,
    },
}

/// Per-plan-cell side-table lookups bundled for [`emit_lift_plan`].
/// One slice per kind, parallel to `plan.cells`. Adding a kind =
/// adding a slice here + a [`emit_cell_op`] arm.
#[derive(Clone, Copy)]
pub(crate) struct CellSideRefs<'a> {
    pub record_info_cell_idx: &'a [Option<u32>],
    pub tuple_indices_cell_idx: &'a [Option<BlobSlice>],
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
) -> (WrapperLocals, ResultEmitPlan<'a>, FrozenLocals) {
    let addr = builder.alloc_local(ValType::I32);
    let st = builder.alloc_local(ValType::I32);
    let ws = builder.alloc_local(ValType::I32);
    let ptr_scratch = builder.alloc_local(ValType::I32);
    let len_scratch = builder.alloc_local(ValType::I32);
    let ext64 = builder.alloc_local(ValType::I64);
    let ext_f64 = builder.alloc_local(ValType::F64);
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
            ResultSourceLayout::Direct(cell) => ResultEmitPlan::Direct {
                cell: cell.clone(),
                source_local: result
                    .expect("ResultSourceLayout::Direct → direct-return local allocated"),
            },
            ResultSourceLayout::RetptrPair {
                cell,
                retptr_offset,
            } => ResultEmitPlan::RetptrPair {
                cell: cell.clone(),
                retptr_offset: *retptr_offset,
                ptr_local: ptr_scratch,
                len_local: len_scratch,
            },
            ResultSourceLayout::Compound {
                compound,
                retptr_offset,
                record_info_cell_idx,
                tuple_indices_cell_idx,
            } => {
                let side_refs = CellSideRefs {
                    record_info_cell_idx,
                    tuple_indices_cell_idx,
                };
                let addr_local = builder.alloc_local(ValType::I32);
                let flat = super::super::super::abi::flat_types(resolve, &compound.ty, None)
                    .unwrap_or_else(|| {
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
                let mut bindgen = WasmEncoderBindgen::new(size_align, addr_local, &mut builder);
                lift_from_memory(resolve, &mut bindgen, (), &compound.ty);
                let loads = bindgen.into_instructions();
                ResultEmitPlan::Compound {
                    plan: &compound.plan,
                    retptr_offset: *retptr_offset,
                    addr_local,
                    synth_locals,
                    loads,
                    side_refs,
                }
            }
        },
    };

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

    // i64 call-id local. Tier-2 generation requires at least one hook
    // (`build_tier2_adapter` bails otherwise), so this is always live.
    let id_local = builder.alloc_local(ValType::I64);

    let frozen = builder.freeze();
    (
        WrapperLocals {
            addr,
            st,
            ws,
            ext64,
            ext_f64,
            result,
            tr_addr,
            id_local,
            task_return_loads,
        },
        result_emit,
        frozen,
    )
}

/// Emit the wasm that lifts one plan into its cells slab. Walks
/// `plan.cells` in allocation order and, for each cell, sets
/// `lcl.addr` to that cell's absolute address (`cells_offset + i *
/// cell_size`) and dispatches on the cell's variant. Cells reference
/// plan-relative flat slots; `local_base` is added per-cell to
/// recover the absolute wasm-local index — params pass the cumulative
/// slot cursor, compound results pass `synth_locals[0]`.
pub(crate) fn emit_lift_plan(
    f: &mut Function,
    cell_layout: &CellLayout,
    cells_offset: u32,
    plan: &LiftPlan,
    side_refs: CellSideRefs<'_>,
    local_base: u32,
    lcl: &WrapperLocals,
) {
    assert_eq!(
        side_refs.record_info_cell_idx.len(),
        plan.cells.len(),
        "side-table record-info indices (emit input) must have one entry per classify-time plan cell"
    );
    assert_eq!(
        side_refs.tuple_indices_cell_idx.len(),
        plan.cells.len(),
        "side-table tuple-indices (emit input) must have one entry per classify-time plan cell"
    );
    for (cell_idx, op) in plan.cells.iter().enumerate() {
        let cell_addr = cells_offset + cell_idx as u32 * cell_layout.size;
        f.instructions().i32_const(cell_addr as i32);
        f.instructions().local_set(lcl.addr);
        emit_cell_op(
            f,
            cell_layout,
            op,
            side_refs.record_info_cell_idx[cell_idx],
            side_refs.tuple_indices_cell_idx[cell_idx],
            local_base,
            lcl,
        );
    }
}

/// Emit one cell's worth of wasm at the address held in `lcl.addr`.
///
/// `local_base` is added to each cell's plan-relative flat-slot
/// position to recover its absolute wasm-local index. `record_info_idx`
/// is the side-table index for `Cell::RecordOf` cells (set by the
/// layout phase via the static record-info builder — adapter-build-
/// time-known, emitted as `i32.const`). Other cells don't read it.
///
/// The match is exhaustive without a `_` catchall: adding a new
/// [`Cell`] variant must add an arm here. Un-wired variants `todo!()`
/// — they're never produced by [`super::plan::LiftPlanBuilder::push`]
/// today, but keeping the arms structural rather than a wildcard
/// means the compiler flags any new variant that's missing codegen.
fn emit_cell_op(
    f: &mut Function,
    cell_layout: &CellLayout,
    op: &Cell,
    record_info_idx: Option<u32>,
    tuple_indices_slice: Option<BlobSlice>,
    local_base: u32,
    lcl: &WrapperLocals,
) {
    let addr = lcl.addr;
    match op {
        Cell::Bool { flat_slot } => cell_layout.emit_bool(f, addr, local_base + *flat_slot),
        Cell::IntegerSignExt { flat_slot } => {
            f.instructions().local_get(local_base + *flat_slot);
            f.instructions().i64_extend_i32_s();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        Cell::IntegerZeroExt { flat_slot } => {
            f.instructions().local_get(local_base + *flat_slot);
            f.instructions().i64_extend_i32_u();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        Cell::Integer64 { flat_slot } => cell_layout.emit_integer(f, addr, local_base + *flat_slot),
        Cell::FloatingF32 { flat_slot } => {
            f.instructions().local_get(local_base + *flat_slot);
            f.instructions().f64_promote_f32();
            f.instructions().local_set(lcl.ext_f64);
            cell_layout.emit_floating(f, addr, lcl.ext_f64);
        }
        Cell::FloatingF64 { flat_slot } => {
            cell_layout.emit_floating(f, addr, local_base + *flat_slot)
        }
        Cell::Text { ptr_slot, len_slot } => {
            cell_layout.emit_text(f, addr, local_base + *ptr_slot, local_base + *len_slot);
        }
        Cell::Bytes { ptr_slot, len_slot } => {
            cell_layout.emit_bytes(f, addr, local_base + *ptr_slot, local_base + *len_slot);
        }
        Cell::EnumCase { flat_slot, .. } => {
            cell_layout.emit_enum_case(f, addr, local_base + *flat_slot);
        }
        Cell::RecordOf { .. } => {
            let idx = record_info_idx
                .expect("record-info index missing — layout phase didn't backfill RecordOf cell");
            cell_layout.emit_record_of(f, addr, idx);
        }
        Cell::TupleOf { .. } => {
            let slice = tuple_indices_slice
                .expect("tuple-indices slice missing — layout phase didn't backfill TupleOf cell");
            cell_layout.emit_tuple_of(f, addr, slice.off, slice.len);
        }
        Cell::Option {
            disc_slot,
            child_idx,
        } => {
            // disc=1 (some) → option-some(child_idx); disc=0 (none) → option-none.
            f.instructions().local_get(local_base + *disc_slot);
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
            // disc=0 → result-ok; disc=1 → result-err. `wasm if` fires
            // on non-zero, so the err arm goes in the `if` block.
            f.instructions().local_get(local_base + *disc_slot);
            f.instructions().if_(BlockType::Empty);
            cell_layout.emit_result_err(f, addr, err_idx.is_some(), err_idx.unwrap_or(0));
            f.instructions().else_();
            cell_layout.emit_result_ok(f, addr, ok_idx.is_some(), ok_idx.unwrap_or(0));
            f.instructions().end();
        }
        Cell::Char
        | Cell::ListOf
        | Cell::Flags
        | Cell::Variant
        | Cell::Handle
        | Cell::Future
        | Cell::Stream
        | Cell::ErrorContext => todo!("emit_cell_op for un-wired Cell variant {op:?}"),
    }
}

/// Shared lift body for direct-return result values. `slot0` /
/// `slot1` are wasm locals carrying the source value(s); for single-
/// slot kinds only `slot0` is used. Multi-slot kinds (Text/Bytes)
/// expect `(ptr, len)` in (slot0, slot1).
///
/// The cell's `flat_slot` / `ptr_slot` / `len_slot` fields are
/// ignored — direct/retptr-pair sources don't go through the
/// plan-relative flat-slot lookup; the caller passes the source
/// locals directly. The cell variant tag drives dispatch.
fn emit_lift_kind(
    f: &mut Function,
    cell_layout: &CellLayout,
    cell: &Cell,
    slot0: u32,
    slot1: u32,
    lcl: &WrapperLocals,
) {
    let addr = lcl.addr;
    match cell {
        Cell::Bool { .. } => cell_layout.emit_bool(f, addr, slot0),
        Cell::IntegerSignExt { .. } => {
            f.instructions().local_get(slot0);
            f.instructions().i64_extend_i32_s();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        Cell::IntegerZeroExt { .. } => {
            f.instructions().local_get(slot0);
            f.instructions().i64_extend_i32_u();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        Cell::Integer64 { .. } => cell_layout.emit_integer(f, addr, slot0),
        Cell::FloatingF32 { .. } => {
            f.instructions().local_get(slot0);
            f.instructions().f64_promote_f32();
            f.instructions().local_set(lcl.ext_f64);
            cell_layout.emit_floating(f, addr, lcl.ext_f64);
        }
        Cell::FloatingF64 { .. } => cell_layout.emit_floating(f, addr, slot0),
        Cell::Text { .. } => cell_layout.emit_text(f, addr, slot0, slot1),
        Cell::Bytes { .. } => cell_layout.emit_bytes(f, addr, slot0, slot1),
        Cell::EnumCase { .. } => cell_layout.emit_enum_case(f, addr, slot0),
        // Compound + un-wired variants aren't valid direct/retptr-pair
        // sources; classify_result_lift's whitelist filters them out.
        Cell::RecordOf { .. }
        | Cell::TupleOf { .. }
        | Cell::Char
        | Cell::ListOf
        | Cell::Option { .. }
        | Cell::Result { .. }
        | Cell::Flags
        | Cell::Variant
        | Cell::Handle
        | Cell::Future
        | Cell::Stream
        | Cell::ErrorContext => unreachable!(
            "emit_lift_kind reached unsupported result Cell {cell:?} — \
             classify_result_lift should have filtered it"
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
        ResultEmitPlan::Direct { cell, source_local } => {
            emit_lift_kind(f, cell_layout, cell, *source_local, *source_local, lcl);
        }
        ResultEmitPlan::RetptrPair {
            cell,
            retptr_offset,
            ptr_local,
            len_local,
        } => {
            f.instructions().i32_const(*retptr_offset);
            f.instructions().i32_load(MemArg {
                offset: SLICE_PTR_OFFSET as u64,
                align: 2,
                memory_index: 0,
            });
            f.instructions().local_set(*ptr_local);
            f.instructions().i32_const(*retptr_offset);
            f.instructions().i32_load(MemArg {
                offset: SLICE_LEN_OFFSET as u64,
                align: 2,
                memory_index: 0,
            });
            f.instructions().local_set(*len_local);
            emit_lift_kind(f, cell_layout, cell, *ptr_local, *len_local, lcl);
        }
        ResultEmitPlan::Compound { .. } | ResultEmitPlan::None => unreachable!(
            "compound results are emitted directly via emit_lift_compound_prefix + \
             emit_lift_plan; emit_lift_result handles only single-cell sources"
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
