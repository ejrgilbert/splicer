//! Codegen: walk a [`LiftPlan`] and emit the wasm that writes one
//! cell per (param | result) into the cells slab, plus the result-
//! lift emission for Direct (sync flat) and Compound result kinds.

use wasm_encoder::{BlockType, Function, Instruction, MemArg, ValType};
use wit_bindgen_core::abi::lift_from_memory;
use wit_parser::{Resolve, SizeAlign};

use super::super::super::abi::cast;
use super::super::super::abi::emit::{
    direct_return_type, emit_bitcast, emit_cabi_realloc_call_runtime, wasm_type_to_val, BlobSlice,
    RecordLayout, I32_STORE_LOG2_ALIGN, I64_STORE_LOG2_ALIGN, I8_STORE_LOG2_ALIGN, MAX_UTF8_LEN,
    OPTION_NONE, OPTION_SOME, SLICE_LEN_OFFSET, SLICE_PTR_OFFSET, STRING_FLAT_BYTES,
};
use super::super::super::abi::flat_types;
use super::super::super::abi::WasmEncoderBindgen;
use super::super::super::indices::{FrozenLocals, LocalsBuilder};
use super::super::cells::{CellLayout, PayloadSource};
use super::super::FuncDispatch;
use super::classify::{InfoCounts, ResultSourceLayout};
use super::plan::{ArmGuard, Cell, LiftPlan, ListElementClass, ListSpec};
use super::sidetable::flags_info::FlagsRuntimeFill;
use super::sidetable::handle_info::HandleRuntimeFill;
use super::sidetable::record_info::RecordRuntimeFill;
use super::sidetable::variant_info::VariantRuntimeFill;
use super::sidetable::{CellSideData, CharScratch, TupleIdxSource};
use wit_parser::abi::WasmType;

/// Wrapper-body locals, allocated up front so all downstream emit
/// phases share the same indices. Result-lift-only locals live on
/// [`ResultEmitPlan`].
///
/// **Stage-then-consume invariant**: the `Option<u32>` staging slots
/// (`tuple_slot_ptr`, `list_elem_*_base`, `*_info_base`, etc.) are
/// each a single shared local set immediately before their consumer
/// emits. Interleaving any emit between stage and consume silently
/// clobbers them — keep stage→consume contiguous.
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
    /// second slot of `Cell::Text` / `Cell::Bytes`. Liveness analysis
    /// drops them when unused.
    pub(super) widen_i32_a: u32,
    pub(super) widen_i32_b: u32,
    /// Scratch f32 local for joined-flat F32 widening — rare (only
    /// when an F32 leaf shares a joined slot with a wider arm).
    pub(super) widen_f32: u32,
    /// `Cell::Flags` bit-walk cursor + count.
    pub(super) flags_addr: u32,
    pub(super) flags_count: u32,
    /// `Cell::Char` utf-8 encoder locals. Both `Some` iff any
    /// `Cell::Char` (top-level or list-element).
    pub(super) char_len: Option<u32>,
    pub(super) char_scratch_addr: Option<u32>,
    /// Staging slot for list-element child cell-array indices
    /// (Option/Result payloads in `PrestagedChildIdx` element class).
    pub(super) list_elem_child_idx: Option<u32>,
    /// Staging slot for list-element TupleOf cells' per-call
    /// indices-buffer slot ptr. Stage-then-consume.
    pub(super) tuple_slot_ptr: Option<u32>,
    /// Direct-return value when the export sig has a single flat result.
    pub result: Option<u32>,
    /// Address local for async `task.return` flat loads. `None` for
    /// sync, void async, and async with retptr-passthrough.
    pub tr_addr: Option<u32>,
    /// i64 call-id local. Tier-2 always wires at least one hook.
    pub id_local: u32,
    /// Pre-built bindgen load sequence for async `task.return`.
    /// Stored here so every local the bindgen needed is in `FrozenLocals`.
    pub task_return_loads: Option<Vec<Instruction<'static>>>,
    /// Pre-built bindgen lower for async indirect_params. Same
    /// FrozenLocals rationale as `task_return_loads`.
    pub params_lower_seq: Option<Vec<Instruction<'static>>>,
    /// Bump snapshot at wrapper entry; restored at exit.
    pub saved_bump: u32,
    /// Active plan's cells slab base; rewritten per plan.
    pub cells_base: u32,
    /// Running cell-index counter; holds `total_cells` after pre-pass.
    pub next_cell_idx: u32,
    /// Per-iter `ll.handle.slot_base + j * count_per_elem`. Stage-
    /// then-consume; `Some` iff [`fn_has_list_elem_handle`].
    pub list_elem_handle_base: Option<u32>,
    /// Per-iter `ll.flags.slot_base + j * count_per_elem` + scratch
    /// base. Stage-then-consume; `Some` iff [`fn_has_list_elem_flags`].
    pub list_elem_flags_base: Option<u32>,
    pub list_elem_flags_scratch_base: Option<u32>,
    /// Runtime entry-address scratch (list-element flags only).
    pub flags_slot_addr: Option<u32>,
    /// Runtime cell-payload idx scratch.
    pub flags_payload_idx: Option<u32>,
    /// Runtime slot byte-address scratch (list-element handles only).
    pub handle_slot_addr: Option<u32>,
    /// Runtime cell-payload idx (list-element handles, non-zero offset).
    pub handle_payload_idx: Option<u32>,
    /// Running handle-info entry count: starts at static count, bumps
    /// by `len * ll.handle.count_per_elem` per list. Sizes `cabi_realloc`
    /// + patches `handle_infos.len`. `Some` iff list-elem-handle present.
    pub next_handle_idx: Option<u32>,
    pub next_flags_idx: Option<u32>,
    pub next_record_idx: Option<u32>,
    /// Per-iter `ll.record.slot_base + j * count_per_elem` + tuples
    /// sub-region base. Stage-then-consume.
    pub list_elem_record_base: Option<u32>,
    pub list_elem_record_tuples_base: Option<u32>,
    pub record_slot_addr: Option<u32>,
    pub record_payload_idx: Option<u32>,
    /// Runtime per-record field-tuples sub-slice base; reused across
    /// per-tuple field writes.
    pub record_tuples_slice_addr: Option<u32>,
    /// Active plan's record-info buffer base. Stage-then-consume.
    pub record_info_base: Option<u32>,
    /// Active plan's variant-info buffer base. Stage-then-consume.
    pub variant_info_base: Option<u32>,
    pub next_variant_idx: Option<u32>,
    pub list_elem_variant_base: Option<u32>,
    pub variant_slot_addr: Option<u32>,
    pub variant_payload_idx: Option<u32>,
    /// Active plan's flags-info buffer base. Stage-then-consume.
    pub flags_info_base: Option<u32>,
    /// Active plan's handle-info buffer base. Stage-then-consume —
    /// load-bearing on (1) only `Cell::Handle` reads it and (2) per-
    /// plan alloc immediately precedes the plan's lift. A future
    /// reader or interleaved fill must restage or split per plan.
    pub handle_info_base: Option<u32>,
    /// Per-param list emit locals; `param_list_locals[i]` parallels
    /// `params[i].lift.plan.list_specs()`.
    pub param_list_locals: Vec<Vec<ListEmitLocals>>,
}

/// Result-side lift bundle. Direct carries side-data inline;
/// Compound borrows it from the layout phase.
pub(crate) enum ResultEmitPlan<'a> {
    /// Void or unsupported result: no lift fires.
    None,
    /// Sync flat return — source already in `source_local`.
    Direct {
        cell: Cell,
        source_local: u32,
        side_data: CellSideData,
    },
    /// Retptr-loaded result. `addr_local` drives the
    /// `lift_from_memory`-built `loads` sequence; the wrapper
    /// `local.set`s values into `synth_locals` (LIFO), with
    /// `local_base = synth_locals[0]`.
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
/// `ResultSourceLayout::Compound`. One entry per cell.
#[derive(Clone, Copy)]
pub(crate) struct CellSideRefs<'a> {
    pub cell_side: &'a [CellSideData],
}

/// Per-build context shared across every lift emit. Bundles
/// `cell_layout` + `cabi_realloc_idx` + per-call buffer geometries
/// so per-cell helpers don't pay `offset_of` per emit.
#[derive(Clone, Copy)]
pub(crate) struct LiftEmitCtx<'a> {
    pub cell_layout: &'a CellLayout,
    pub cabi_realloc_idx: u32,
    pub handle_info: HandleInfoOffsets,
    pub flags_info: FlagsInfoOffsets,
    pub record_info: RecordInfoOffsets,
    pub variant_info: VariantInfoOffsets,
}

/// Build-time-resolved geometry of one `record handle-info` entry.
#[derive(Clone, Copy)]
pub(crate) struct HandleInfoOffsets {
    pub entry_size: u32,
    pub align: u32,
    pub type_name_off: u32,
    pub id_off: u32,
}

impl HandleInfoOffsets {
    pub(crate) fn from_layout(layout: &RecordLayout) -> Self {
        use super::super::schema::{HANDLE_INFO_ID, HANDLE_INFO_TYPE_NAME};
        Self {
            entry_size: layout.size,
            align: layout.align,
            type_name_off: layout.offset_of(HANDLE_INFO_TYPE_NAME),
            id_off: layout.offset_of(HANDLE_INFO_ID),
        }
    }
}

/// Build-time-resolved geometry of one `record flags-info` entry.
/// Same shape as [`HandleInfoOffsets`] for the flags-info side.
#[derive(Clone, Copy)]
pub(crate) struct FlagsInfoOffsets {
    pub entry_size: u32,
    pub align: u32,
    pub type_name_off: u32,
    pub set_flags_off: u32,
}

impl FlagsInfoOffsets {
    pub(crate) fn from_layout(layout: &RecordLayout) -> Self {
        use super::super::schema::FLAGS_INFO_SET_FLAGS;
        use super::sidetable::INFO_TYPE_NAME;
        Self {
            entry_size: layout.size,
            align: layout.align,
            type_name_off: layout.offset_of(INFO_TYPE_NAME),
            set_flags_off: layout.offset_of(FLAGS_INFO_SET_FLAGS),
        }
    }
}

/// Build-time-resolved geometry of one `record variant-info` entry,
/// including the `option<u32>` payload's value-byte sub-offset.
/// `payload_off` lands the option-disc byte; `payload_off +
/// payload_value_off` lands the u32 value slot.
#[derive(Clone, Copy)]
pub(crate) struct VariantInfoOffsets {
    pub entry_size: u32,
    pub align: u32,
    pub type_name_off: u32,
    pub case_name_off: u32,
    pub payload_off: u32,
    pub payload_value_off: u32,
}

impl VariantInfoOffsets {
    /// `payload_value_off` is a separate arg because `payload` is
    /// `option<u32>`, not a record — `RecordLayout::offset_of` can't
    /// reach into it. Sourced from `option_payload_offset`.
    pub(crate) fn from_layout(layout: &RecordLayout, payload_value_off: u32) -> Self {
        use super::super::schema::{VARIANT_INFO_CASE_NAME, VARIANT_INFO_PAYLOAD};
        use super::sidetable::INFO_TYPE_NAME;
        Self {
            entry_size: layout.size,
            align: layout.align,
            type_name_off: layout.offset_of(INFO_TYPE_NAME),
            case_name_off: layout.offset_of(VARIANT_INFO_CASE_NAME),
            payload_off: layout.offset_of(VARIANT_INFO_PAYLOAD),
            payload_value_off,
        }
    }
}

/// Build-time-resolved geometry of one `record record-info` entry +
/// the inner `tuple<string, u32>` field-tuple shape that
/// `record-info.fields` points at. Bundled so list-element
/// `emit_record_runtime_fill` can write per-iteration tuples without
/// re-resolving offsets. The tuple geometry is also constant across
/// every call site, so the layout pays a single `offset_of` walk in
/// `from_layout`.
#[derive(Clone, Copy)]
pub(crate) struct RecordInfoOffsets {
    pub entry_size: u32,
    pub align: u32,
    pub type_name_off: u32,
    pub fields_off: u32,
    pub tuple_size: u32,
    pub tuple_align: u32,
    pub tuple_name_off: u32,
    pub tuple_idx_off: u32,
}

impl RecordInfoOffsets {
    pub(crate) fn from_layout(layout: &RecordLayout, tuple_layout: &RecordLayout) -> Self {
        use super::super::schema::{
            RECORD_FIELD_TUPLE_IDX, RECORD_FIELD_TUPLE_NAME, RECORD_INFO_FIELDS,
        };
        use super::sidetable::INFO_TYPE_NAME;
        Self {
            entry_size: layout.size,
            align: layout.align,
            type_name_off: layout.offset_of(INFO_TYPE_NAME),
            fields_off: layout.offset_of(RECORD_INFO_FIELDS),
            tuple_size: tuple_layout.size,
            tuple_align: tuple_layout.align,
            tuple_name_off: tuple_layout.offset_of(RECORD_FIELD_TUPLE_NAME),
            tuple_idx_off: tuple_layout.offset_of(RECORD_FIELD_TUPLE_IDX),
        }
    }
}

/// Per-plan-walk cursor: the plan + the wrapper-local offset added to
/// each cell's plan-relative flat slot. `elem_cell_base` is `Some`
/// inside list-element bodies — Option/Result resolve runtime idx as
/// `elem_cell_base + relative_idx`; top-level walks use build-time-
/// known absolute idx.
#[derive(Clone, Copy)]
pub(crate) struct PlanCursor<'a> {
    pub plan: &'a LiftPlan,
    pub local_base: u32,
    pub elem_cell_base: Option<u32>,
}

/// Per-info-kind buffer locals for one list. Handle/variant leave
/// `buf_base`/`bytes_per_elem` unused so all four kinds share shape.
pub(crate) struct KindBuffers {
    /// Slice base in the wrapper-level info buffer; `None` iff zero entries.
    pub slot_base: Option<u32>,
    pub count_per_elem: u32,
    /// Per-call cabi_realloc'd scratch base; `None` for handle/variant.
    pub buf_base: Option<u32>,
    pub bytes_per_elem: u32,
}

/// Per-`Cell::ListOf` emit-time bundle. One entry per list-of cell;
/// parallel to [`LiftPlan::list_specs`].
pub(crate) struct ListEmitLocals {
    /// Cell-idx where this list's element cells begin.
    pub start_i: u32,
    /// Captured source `len` flat slot value.
    pub len: u32,
    /// Per-call indices buffer base (`len * 4` bytes).
    pub indices_ptr: u32,
    /// Element-loop counter (0..len).
    pub j: u32,
    /// Per-iter source element address; drives `elem_loads`.
    pub elem_addr: u32,
    /// One local per element-plan flat slot, contiguous so plan slot
    /// N maps to `elem_flat_locals[0] + N`.
    pub elem_flat_locals: Vec<u32>,
    /// Pre-built `lift_from_memory` loads — pushes element flat values
    /// for capture into `elem_flat_locals` (LIFO).
    pub elem_loads: Vec<Instruction<'static>>,
    /// Canonical-ABI byte size of one element.
    pub elem_byte_size: u32,
    /// Side-data parallel to `element_plan.cells`.
    pub elem_cell_side: Vec<CellSideData>,
    /// Per-call utf-8 scratch for `Cell::Char` element cells. The k-th
    /// char in iteration j lives at `(j * chars_per_elem + k) * MAX_UTF8_LEN`.
    pub char_scratch_base: Option<u32>,
    pub chars_per_elem: u32,
    /// Per-call buffer for `Cell::TupleOf` element cells —
    /// `len * tuple_idx_count_per_elem * 4` bytes.
    pub tuple_idx_buf_base: Option<u32>,
    pub tuple_idx_count_per_elem: u32,
    /// Per-iteration cell-array base (`start_i + j*elem_count`).
    /// Always allocated — saves 1–2 insns per cell per iter vs recomputing.
    pub elem_cell_base: u32,
    /// Per-info-kind buffer locals. Flags/record use the scratch pair;
    /// handle/variant leave it `None`/`0`.
    pub handle: KindBuffers,
    pub flags: KindBuffers,
    pub record: KindBuffers,
    pub variant: KindBuffers,
    /// Nested-list state: `Some` exactly when this list's element plan
    /// is `[Cell::ListOf]` (enforced by `push_list_of`'s depth-2 cap).
    /// Bundles inner locals + per-kind cursors + outer-ptr scratch so
    /// the all-or-none invariant is structural (one `Option` instead
    /// of five gated on each other).
    pub nested: Option<NestedListLocals>,
}

/// Per-outer-list bundle for `list<list<T>>` emit. Cursors track
/// running positions in wrapper-level info buffers — pre-pass seeds
/// them, `emit_list_of_arm` snaps `inner.<kind>_slot_base = cursor`
/// per outer iter, iter-end advances by `inner.len * per_elem`. One
/// cursor per per-call info kind contributed by the inner element
/// plan; `None` for kinds the inner doesn't carry.
pub(crate) struct NestedListLocals {
    /// Inner list's emit locals; inner's `start_i` is *written once per
    /// outer iter* from [`Self::cursor`] before inner emit runs (same
    /// "base, not cursor" role as a top-level list's `start_i`).
    /// `inner.len` is set from outer's loaded elem-flats per outer iter.
    pub inner: Box<ListEmitLocals>,
    /// Running cell-array cursor: pre-pass seeds `outer.start_i +
    /// outer.len`; iter-end bumps by `inner.len * inner_elem_count`.
    /// Kept separate from `inner.start_i` so the inner's `start_i`
    /// keeps its single base-pointer meaning.
    pub cursor: u32,
    /// Per-kind cursors paired with the inner list's [`KindBuffers`].
    /// Seeded from `lcl.next_<kind>_idx`; advances per outer iter.
    pub kinds: NestedKindCursors,
    /// Outer-ptr scratch for the nested-list pre-pass memory walk.
    pub outer_ptr_scratch: u32,
}

/// One cursor per [`KindBuffers`] entry; `None` iff inner contributes
/// zero entries of that kind.
pub(crate) struct NestedKindCursors {
    pub handle: Option<u32>,
    pub flags: Option<u32>,
    pub record: Option<u32>,
    pub variant: Option<u32>,
}

#[derive(Clone, Copy)]
struct NestedKindRow<'a> {
    cursor: Option<u32>,
    kb: &'a KindBuffers,
    next_idx: Option<u32>,
    label: &'static str,
}

fn nested_kind_rows<'a>(
    nested: &NestedListLocals,
    inner_ll: &'a ListEmitLocals,
    lcl: &WrapperLocals,
) -> [NestedKindRow<'a>; 4] {
    [
        NestedKindRow {
            cursor: nested.kinds.handle,
            kb: &inner_ll.handle,
            next_idx: lcl.next_handle_idx,
            label: "handle",
        },
        NestedKindRow {
            cursor: nested.kinds.flags,
            kb: &inner_ll.flags,
            next_idx: lcl.next_flags_idx,
            label: "flags",
        },
        NestedKindRow {
            cursor: nested.kinds.record,
            kb: &inner_ll.record,
            next_idx: lcl.next_record_idx,
            label: "record",
        },
        NestedKindRow {
            cursor: nested.kinds.variant,
            kb: &inner_ll.variant,
            next_idx: lcl.next_variant_idx,
            label: "variant",
        },
    ]
}

/// Allocate per-list emit locals + pre-build the `lift_from_memory`
/// loads for every `Cell::ListOf` in `plan`. Runs while the builder
/// is live (bindgen may allocate scratch locals).
pub(super) fn alloc_list_emit_locals(
    plan: &LiftPlan,
    resolve: &Resolve,
    size_align: &SizeAlign,
    record_tuple_size: u32,
    builder: &mut LocalsBuilder,
) -> Vec<ListEmitLocals> {
    plan.list_specs()
        .map(|spec: ListSpec<'_>| {
            build_list_emit_locals_for_plan(
                spec.element_plan,
                resolve,
                size_align,
                record_tuple_size,
                builder,
            )
        })
        .collect()
}

/// Build locals for one list with `element_plan` as the per-element
/// lift plan. Reentrant: the nested-list arm recurses on the inner
/// list's own element plan to allocate its `ListEmitLocals`.
fn build_list_emit_locals_for_plan(
    element_plan: &LiftPlan,
    resolve: &Resolve,
    size_align: &SizeAlign,
    record_tuple_size: u32,
    builder: &mut LocalsBuilder,
) -> ListEmitLocals {
    let start_i = builder.alloc_local(ValType::I32);
    let len = builder.alloc_local(ValType::I32);
    let indices_ptr = builder.alloc_local(ValType::I32);
    let j = builder.alloc_local(ValType::I32);
    let elem_addr = builder.alloc_local(ValType::I32);
    // Contiguous flat-slot locals: plan slot N → elem_flat_locals[0] + N.
    let elem_ty = element_plan.source_ty;
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
    let (elem_cell_side, counts) = walk_element_plan(element_plan, record_tuple_size);
    let char_scratch_base = (counts.chars > 0).then(|| builder.alloc_local(ValType::I32));
    let tuple_idx_buf_base =
        (counts.tuple_idx_slots > 0).then(|| builder.alloc_local(ValType::I32));
    let chars_per_elem = counts.chars;
    let tuple_idx_count_per_elem = counts.tuple_idx_slots;
    let handle = KindBuffers {
        slot_base: (counts.handles > 0).then(|| builder.alloc_local(ValType::I32)),
        count_per_elem: counts.handles,
        buf_base: None,
        bytes_per_elem: 0,
    };
    let flags = KindBuffers {
        slot_base: (counts.flags > 0).then(|| builder.alloc_local(ValType::I32)),
        count_per_elem: counts.flags,
        buf_base: (counts.flags_scratch_bytes > 0).then(|| builder.alloc_local(ValType::I32)),
        bytes_per_elem: counts.flags_scratch_bytes,
    };
    let record = KindBuffers {
        slot_base: (counts.records > 0).then(|| builder.alloc_local(ValType::I32)),
        count_per_elem: counts.records,
        buf_base: (counts.record_tuples_bytes > 0).then(|| builder.alloc_local(ValType::I32)),
        bytes_per_elem: counts.record_tuples_bytes,
    };
    let variant = KindBuffers {
        slot_base: (counts.variants > 0).then(|| builder.alloc_local(ValType::I32)),
        count_per_elem: counts.variants,
        buf_base: None,
        bytes_per_elem: 0,
    };
    let elem_cell_base = builder.alloc_local(ValType::I32);
    // Nested-list recursion: element plan is exactly one `Cell::ListOf`.
    // Enforced by `push_list_of`'s depth-2 cap, so any other shape
    // means a plan-build bug.
    let nested = match element_plan.cells.as_slice() {
        [Cell::ListOf {
            element_plan: inner_plan,
            ..
        }] => {
            let inner = build_list_emit_locals_for_plan(
                inner_plan,
                resolve,
                size_align,
                record_tuple_size,
                builder,
            );
            let cursor = builder.alloc_local(ValType::I32);
            // Per-kind cursor allocated only when inner contributes that kind.
            let mut alloc_if =
                |kb: &KindBuffers| kb.slot_base.map(|_| builder.alloc_local(ValType::I32));
            let kinds = NestedKindCursors {
                handle: alloc_if(&inner.handle),
                flags: alloc_if(&inner.flags),
                record: alloc_if(&inner.record),
                variant: alloc_if(&inner.variant),
            };
            let outer_ptr_scratch = builder.alloc_local(ValType::I32);
            Some(NestedListLocals {
                inner: Box::new(inner),
                cursor,
                kinds,
                outer_ptr_scratch,
            })
        }
        _ => None,
    };
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
        char_scratch_base,
        chars_per_elem,
        tuple_idx_buf_base,
        tuple_idx_count_per_elem,
        elem_cell_base,
        handle,
        flags,
        record,
        variant,
        nested,
    }
}

/// Whether any list-element cell across the wrapper's plans has the
/// given [`ListElementClass`]. Recurses through nested lists' inner
/// element plans so their per-class wrapper locals also get gated on.
fn fn_has_list_elem_class(fd: &FuncDispatch, want: ListElementClass) -> bool {
    let plan_has = |plan: &LiftPlan| plan.any_list_element_has_class(want);
    if fd.params.iter().any(|p| plan_has(&p.lift.plan)) {
        return true;
    }
    match fd.result_lift.as_ref().map(|rl| &rl.source) {
        Some(ResultSourceLayout::Compound { compound, .. }) => plan_has(&compound.plan),
        _ => false,
    }
}

fn fn_has_list_elem_child_idx(fd: &FuncDispatch) -> bool {
    fn_has_list_elem_class(fd, ListElementClass::PrestagedChildIdx)
}

fn fn_has_list_elem_tuple(fd: &FuncDispatch) -> bool {
    fn_has_list_elem_class(fd, ListElementClass::PrestagedTupleIndices)
}

fn fn_has_list_elem_handle(fd: &FuncDispatch) -> bool {
    fn_has_list_elem_class(fd, ListElementClass::PrestagedHandle)
}

fn fn_has_list_elem_flags(fd: &FuncDispatch) -> bool {
    fn_has_list_elem_class(fd, ListElementClass::PrestagedFlags)
}

fn fn_has_list_elem_record(fd: &FuncDispatch) -> bool {
    fn_has_list_elem_class(fd, ListElementClass::PrestagedRecord)
}

fn fn_has_list_elem_variant(fd: &FuncDispatch) -> bool {
    fn_has_list_elem_class(fd, ListElementClass::PrestagedVariant)
}

/// Any `Cell::Char` in the wrapper. Gates `char_len` + `char_scratch_addr`.
fn fn_contains_char(fd: &FuncDispatch) -> bool {
    if fd.params.iter().any(|p| p.lift.plan.contains_char()) {
        return true;
    }
    if let Some(rl) = fd.result_lift.as_ref() {
        match &rl.source {
            ResultSourceLayout::Direct { cell, .. } => matches!(cell, Cell::Char { .. }),
            ResultSourceLayout::Compound { compound, .. } => compound.plan.contains_char(),
        }
    } else {
        false
    }
}

/// Any cell of `kind` in the wrapper. Gates the matching
/// `WrapperLocals.*_info_base`.
fn fn_has_info_cells(
    fd: &FuncDispatch,
    kind: ListElementClass,
    count: fn(&InfoCounts) -> u32,
) -> bool {
    fn_has_list_elem_class(fd, kind)
        || fd.params.iter().any(|p| count(&p.info_counts) > 0)
        || fd
            .result_lift
            .as_ref()
            .is_some_and(|rl| count(&rl.info_counts) > 0)
}

fn fn_has_handle_cells(fd: &FuncDispatch) -> bool {
    fn_has_info_cells(fd, ListElementClass::PrestagedHandle, |c| c.handle)
}
fn fn_has_flags_cells(fd: &FuncDispatch) -> bool {
    fn_has_info_cells(fd, ListElementClass::PrestagedFlags, |c| c.flags)
}
fn fn_has_record_cells(fd: &FuncDispatch) -> bool {
    fn_has_info_cells(fd, ListElementClass::PrestagedRecord, |c| c.record)
}
fn fn_has_variant_cells(fd: &FuncDispatch) -> bool {
    fn_has_info_cells(fd, ListElementClass::PrestagedVariant, |c| c.variant)
}

/// Per-class counts driving per-list allocation decisions.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ElementCounts {
    pub chars: u32,
    /// Sum of `children.len()` across `Cell::TupleOf` cells.
    pub tuple_idx_slots: u32,
    pub handles: u32,
    pub flags: u32,
    /// Total set-flags scratch bytes across `Cell::Flags` cells —
    /// variable-stride per cell.
    pub flags_scratch_bytes: u32,
    pub records: u32,
    /// Total field-tuples bytes across `Cell::RecordOf` cells —
    /// variable-stride per cell.
    pub record_tuples_bytes: u32,
    pub variants: u32,
}

/// Single walk over `element_plan.cells` producing per-cell side
/// data + per-class counts. Driven off [`Cell::list_element_class`] so
/// adding a class forces a fold arm at compile time.
pub(super) fn walk_element_plan(
    element_plan: &LiftPlan,
    record_tuple_size: u32,
) -> (Vec<CellSideData>, ElementCounts) {
    let mut counts = ElementCounts::default();
    let side: Vec<CellSideData> = element_plan
        .cells
        .iter()
        .map(|cell| {
            let class = cell.list_element_class().unwrap_or_else(|| {
                unreachable!(
                    "Cell {cell:?} reached walk_element_plan despite \
                     Cell::list_element_class() rejecting it"
                )
            });
            match class {
                ListElementClass::Scalar => CellSideData::None,
                ListElementClass::PrestagedChar => {
                    counts.chars += 1;
                    CellSideData::Char {
                        scratch: CharScratch::Prestaged,
                    }
                }
                // Option/Result resolve child idx via PlanCursor.elem_cell_base.
                ListElementClass::PrestagedChildIdx => CellSideData::None,
                ListElementClass::PrestagedTupleIndices => {
                    let Cell::TupleOf { children } = cell else {
                        unreachable!("PrestagedTupleIndices class on non-TupleOf {cell:?}")
                    };
                    let off = counts.tuple_idx_slots * 4;
                    counts.tuple_idx_slots += children.len() as u32;
                    CellSideData::Tuple {
                        source: TupleIdxSource::PerIteration {
                            offset_in_elem: off,
                        },
                    }
                }
                ListElementClass::PrestagedHandle => {
                    let Cell::Handle { type_name, .. } = cell else {
                        unreachable!("PrestagedHandle class on non-Handle {cell:?}")
                    };
                    let offset_in_elem = counts.handles;
                    counts.handles += 1;
                    CellSideData::Handle(Box::new(
                        super::sidetable::handle_info::HandleRuntimeFill {
                            slot_source:
                                super::sidetable::handle_info::HandleSlotSource::PerIteration {
                                    offset_in_elem,
                                },
                            type_name: *type_name,
                        },
                    ))
                }
                ListElementClass::PrestagedFlags => {
                    let Cell::Flags {
                        type_name,
                        flag_names,
                        ..
                    } = cell
                    else {
                        unreachable!("PrestagedFlags class on non-Flags {cell:?}")
                    };
                    let entry_offset_in_elem = counts.flags;
                    let scratch_offset_in_elem = counts.flags_scratch_bytes;
                    counts.flags += 1;
                    counts.flags_scratch_bytes += flag_names.len() as u32 * STRING_FLAT_BYTES;
                    CellSideData::Flags(Box::new(super::sidetable::flags_info::FlagsRuntimeFill {
                        slot_source: super::sidetable::flags_info::FlagsSlotSource::PerIteration {
                            entry_offset_in_elem,
                            scratch_offset_in_elem,
                        },
                        type_name: *type_name,
                        flag_names: flag_names.clone(),
                    }))
                }
                ListElementClass::PrestagedRecord => {
                    let Cell::RecordOf { type_name, fields } = cell else {
                        unreachable!("PrestagedRecord class on non-RecordOf {cell:?}")
                    };
                    let entry_offset_in_elem = counts.records;
                    let tuples_offset_in_elem = counts.record_tuples_bytes;
                    counts.records += 1;
                    counts.record_tuples_bytes += fields.len() as u32 * record_tuple_size;
                    CellSideData::Record(Box::new(
                        super::sidetable::record_info::RecordRuntimeFill {
                            slot_source:
                                super::sidetable::record_info::RecordSlotSource::PerIteration {
                                    entry_offset_in_elem,
                                    tuples_offset_in_elem,
                                },
                            type_name: *type_name,
                            fields_len: fields.len() as u32,
                        },
                    ))
                }
                ListElementClass::PrestagedVariant => {
                    let Cell::Variant {
                        type_name,
                        case_names,
                        per_case_payload,
                        ..
                    } = cell
                    else {
                        unreachable!("PrestagedVariant class on non-Variant {cell:?}")
                    };
                    let entry_offset_in_elem = counts.variants;
                    counts.variants += 1;
                    CellSideData::Variant(Box::new(
                        super::sidetable::variant_info::VariantRuntimeFill {
                            slot_source:
                                super::sidetable::variant_info::VariantSlotSource::PerIteration {
                                    entry_offset_in_elem,
                                },
                            type_name: *type_name,
                            case_names: case_names.clone(),
                            per_case_payload: per_case_payload.clone(),
                        },
                    ))
                }
                // Nested list: per-iter state lives on outer's
                // `ListEmitLocals.nested.inner`; no per-elem counter
                // contribution (inner kinds gated to non-per-call-buf).
                ListElementClass::PrestagedNestedList => CellSideData::None,
            }
        })
        .collect();
    (side, counts)
}

/// Allocate every wrapper-body local + build compound-result and
/// task-return load sequences, then freeze the locals list. Taking
/// `builder` by value is the typestate hinge: post-freeze allocation
/// is a compile error rather than a runtime trap.
pub(crate) fn alloc_wrapper_locals<'a>(
    resolve: &Resolve,
    size_align: &SizeAlign,
    record_tuple_size: u32,
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
    let widen_f32 = builder.alloc_local(ValType::F32);
    let flags_addr = builder.alloc_local(ValType::I32);
    let flags_count = builder.alloc_local(ValType::I32);
    let needs_char_locals = fn_contains_char(fd);
    let char_len = needs_char_locals.then(|| builder.alloc_local(ValType::I32));
    let char_scratch_addr = needs_char_locals.then(|| builder.alloc_local(ValType::I32));
    let list_elem_child_idx =
        fn_has_list_elem_child_idx(fd).then(|| builder.alloc_local(ValType::I32));
    let tuple_slot_ptr = fn_has_list_elem_tuple(fd).then(|| builder.alloc_local(ValType::I32));
    let needs_list_handle_locals = fn_has_list_elem_handle(fd);
    let list_elem_handle_base = needs_list_handle_locals.then(|| builder.alloc_local(ValType::I32));
    let handle_slot_addr = needs_list_handle_locals.then(|| builder.alloc_local(ValType::I32));
    let handle_payload_idx = needs_list_handle_locals.then(|| builder.alloc_local(ValType::I32));
    let needs_list_flags_locals = fn_has_list_elem_flags(fd);
    let list_elem_flags_base = needs_list_flags_locals.then(|| builder.alloc_local(ValType::I32));
    let list_elem_flags_scratch_base =
        needs_list_flags_locals.then(|| builder.alloc_local(ValType::I32));
    let flags_slot_addr = needs_list_flags_locals.then(|| builder.alloc_local(ValType::I32));
    let flags_payload_idx = needs_list_flags_locals.then(|| builder.alloc_local(ValType::I32));
    let needs_list_record_locals = fn_has_list_elem_record(fd);
    let list_elem_record_base = needs_list_record_locals.then(|| builder.alloc_local(ValType::I32));
    let list_elem_record_tuples_base =
        needs_list_record_locals.then(|| builder.alloc_local(ValType::I32));
    let record_slot_addr = needs_list_record_locals.then(|| builder.alloc_local(ValType::I32));
    let record_payload_idx = needs_list_record_locals.then(|| builder.alloc_local(ValType::I32));
    let record_tuples_slice_addr =
        needs_list_record_locals.then(|| builder.alloc_local(ValType::I32));
    let needs_list_variant_locals = fn_has_list_elem_variant(fd);
    let list_elem_variant_base =
        needs_list_variant_locals.then(|| builder.alloc_local(ValType::I32));
    let variant_slot_addr = needs_list_variant_locals.then(|| builder.alloc_local(ValType::I32));
    let variant_payload_idx = needs_list_variant_locals.then(|| builder.alloc_local(ValType::I32));
    let result = direct_return_type(&fd.export_sig).map(|t| builder.alloc_local(t));
    // Non-retptr-passthrough async task.return: i32 addr drives
    // `lift_from_memory` flat-load out of the retptr scratch.
    let tr_uses_flat_loads = fd
        .shape
        .task_return()
        .is_some_and(|tr| !tr.sig.indirect_params && fd.result_ty.is_some());
    let tr_addr = tr_uses_flat_loads.then(|| builder.alloc_local(ValType::I32));

    // Compound: extra locals + bindgen-driven `lift_from_memory` may
    // allocate scratch locals — must run before freeze.
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
                // Contiguous synth locals: cell N's flat slot
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
                let list_locals = alloc_list_emit_locals(
                    &compound.plan,
                    resolve,
                    size_align,
                    record_tuple_size,
                    &mut builder,
                );
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

    // Must allocate before freeze; empty inner Vec for list-free params.
    let param_list_locals: Vec<Vec<ListEmitLocals>> = fd
        .params
        .iter()
        .map(|p| {
            alloc_list_emit_locals(
                &p.lift.plan,
                resolve,
                size_align,
                record_tuple_size,
                &mut builder,
            )
        })
        .collect();

    // Second `lift_from_memory` pass; must run before freeze.
    let task_return_loads: Option<Vec<Instruction<'static>>> = tr_addr.map(|addr_local| {
        let result_ty = fd
            .result_ty
            .as_ref()
            .expect("flat task.return loads → result_ty");
        let mut bindgen = WasmEncoderBindgen::new(size_align, addr_local, &mut builder);
        lift_from_memory(resolve, &mut bindgen, (), result_ty);
        bindgen.into_instructions()
    });

    // Indirect-params lower (async overflowed MAX_FLAT_ASYNC_PARAMS);
    // driven through the same builder so scratch lands in `frozen`.
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

    let id_local = builder.alloc_local(ValType::I64);
    let saved_bump = builder.alloc_local(ValType::I32);
    let cells_base = builder.alloc_local(ValType::I32);
    let next_cell_idx = builder.alloc_local(ValType::I32);
    let handle_info_base = fn_has_handle_cells(fd).then(|| builder.alloc_local(ValType::I32));
    let flags_info_base = fn_has_flags_cells(fd).then(|| builder.alloc_local(ValType::I32));
    let record_info_base = fn_has_record_cells(fd).then(|| builder.alloc_local(ValType::I32));
    let variant_info_base = fn_has_variant_cells(fd).then(|| builder.alloc_local(ValType::I32));
    let next_handle_idx = needs_list_handle_locals.then(|| builder.alloc_local(ValType::I32));
    let next_flags_idx = needs_list_flags_locals.then(|| builder.alloc_local(ValType::I32));
    let next_record_idx = needs_list_record_locals.then(|| builder.alloc_local(ValType::I32));
    let next_variant_idx = needs_list_variant_locals.then(|| builder.alloc_local(ValType::I32));

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
            widen_f32,
            flags_addr,
            flags_count,
            char_len,
            char_scratch_addr,
            list_elem_child_idx,
            tuple_slot_ptr,
            list_elem_handle_base,
            handle_slot_addr,
            handle_payload_idx,
            list_elem_flags_base,
            list_elem_flags_scratch_base,
            flags_slot_addr,
            flags_payload_idx,
            list_elem_record_base,
            list_elem_record_tuples_base,
            record_slot_addr,
            record_payload_idx,
            record_tuples_slice_addr,
            list_elem_variant_base,
            variant_slot_addr,
            variant_payload_idx,
            result,
            tr_addr,
            id_local,
            task_return_loads,
            params_lower_seq,
            saved_bump,
            cells_base,
            next_cell_idx,
            handle_info_base,
            flags_info_base,
            record_info_base,
            variant_info_base,
            next_variant_idx,
            next_handle_idx,
            next_flags_idx,
            next_record_idx,
            param_list_locals,
        },
        result_emit,
        frozen,
    )
}

/// Emit the wasm that lifts one plan into its cells slab. Walks
/// `plan.cells` in allocation order, setting `lcl.addr` per cell and
/// dispatching on the cell variant. `local_base` resolves
/// plan-relative flat slots to absolute wrapper-local indices.
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
            PlanCursor {
                plan,
                local_base,
                elem_cell_base: None,
            },
            op,
            &side_refs.cell_side[cell_idx],
            lcl,
            list_slot,
        );
    }
}

/// Resolve a leaf-level flat-slot read, applying joined-flat widening
/// bitcast when needed. Returns the absolute wrapper-local — either
/// `local_base + flat_slot` (no widening) or a typed scratch.
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

/// Form of [`pin_leaf_flat`] taking a caller-supplied i32 scratch —
/// only Text / Bytes need this (two i32 slots can both widen).
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
        // Another arm widened the slot, but this arm doesn't need it.
        return local_base + flat_slot;
    }
    f.instructions().local_get(local_base + flat_slot);
    emit_bitcast(f, &bc);
    let scratch = match arm {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => scratch_i32,
        WasmType::I64 | WasmType::PointerOrI64 => lcl.ext64,
        WasmType::F32 => lcl.widen_f32,
        WasmType::F64 => lcl.ext_f64,
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

/// `cursor += len_local * per_elem` — folds the `* 1` case so the
/// emitted wasm stays free of dead `i32.const 1; i32.mul` pairs.
/// Drives every running-counter bump in the per-list pre-pass and
/// the nested-list inner-cursor advances at iter-end.
fn emit_cursor_advance_by_len(f: &mut Function, cursor: u32, len_local: u32, per_elem: u32) {
    f.instructions().local_get(cursor);
    f.instructions().local_get(len_local);
    if per_elem != 1 {
        f.instructions().i32_const(per_elem as i32);
        f.instructions().i32_mul();
    }
    f.instructions().i32_add();
    f.instructions().local_set(cursor);
}

/// Per-iter `dest = kb.slot_base + j * kb.count_per_elem`. No-op
/// when `kb.slot_base` is `None`. Uniform across all info kinds.
fn emit_set_per_iter_slot_base(
    f: &mut Function,
    kb: &KindBuffers,
    dest_opt: Option<u32>,
    j: u32,
    kind_label: &str,
) {
    let Some(slot_base) = kb.slot_base else {
        return;
    };
    let dest = dest_opt.unwrap_or_else(|| {
        panic!("fn_has_list_elem_{kind_label} disagrees with {kind_label}.slot_base")
    });
    f.instructions().local_get(slot_base);
    f.instructions().local_get(j);
    if kb.count_per_elem != 1 {
        f.instructions().i32_const(kb.count_per_elem as i32);
        f.instructions().i32_mul();
    }
    f.instructions().i32_add();
    f.instructions().local_set(dest);
}

/// Per-iter `dest = kb.buf_base + j * kb.bytes_per_elem`. No-op
/// when `kb.buf_base` is `None`. Stride is always > 1 byte so the
/// `* 1` fold from [`emit_set_per_iter_slot_base`] never applies.
fn emit_set_per_iter_scratch_base(
    f: &mut Function,
    kb: &KindBuffers,
    dest_opt: Option<u32>,
    j: u32,
    kind_label: &str,
) {
    let Some(buf_base) = kb.buf_base else {
        return;
    };
    let dest = dest_opt.unwrap_or_else(|| {
        panic!("fn_has_list_elem_{kind_label} disagrees with {kind_label}.buf_base")
    });
    f.instructions().local_get(buf_base);
    f.instructions().local_get(j);
    f.instructions().i32_const(kb.bytes_per_elem as i32);
    f.instructions().i32_mul();
    f.instructions().i32_add();
    f.instructions().local_set(dest);
}

/// Snap `slot_base = cursor`, then advance the cursor by
/// `len_local * per_elem`. This is the per-list "give this list its
/// slice of the wrapper-level info buffer, then walk the cursor past
/// it" sequence used for handle / flags / record / variant counters
/// in [`emit_list_pre_pass`]. Uses `local.tee` to leave the cursor
/// value on the stack for the advance, saving one `local.get` per
/// snap site (× 4 kinds × every wrapper body).
fn emit_snap_and_advance_cursor(
    f: &mut Function,
    cursor: u32,
    slot_base: u32,
    len_local: u32,
    per_elem: u32,
) {
    f.instructions().local_get(cursor);
    f.instructions().local_tee(slot_base);
    f.instructions().local_get(len_local);
    if per_elem != 1 {
        f.instructions().i32_const(per_elem as i32);
        f.instructions().i32_mul();
    }
    f.instructions().i32_add();
    f.instructions().local_set(cursor);
}

/// Pre-pass: init `lcl.next_cell_idx` to the plan's static cell count,
/// then bump by `len · elem_count` per list (capturing `start_i` and
/// `len`). Joined-arm lists disc-gate the bump so an inactive arm's
/// bytes can't bloat the slab — zero-init keeps `ll.len`/`ll.start_i`
/// defined on the inactive path. Parallel running counters for
/// handle/flags/record/variant-info entries follow the same shape.
pub(crate) fn emit_list_pre_pass(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    plan: &LiftPlan,
    static_counts: &InfoCounts,
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
    for (next_idx, count) in [
        (lcl.next_handle_idx, static_counts.handle),
        (lcl.next_flags_idx, static_counts.flags),
        (lcl.next_record_idx, static_counts.record),
        (lcl.next_variant_idx, static_counts.variant),
    ] {
        if let Some(next_idx) = next_idx {
            f.instructions().i32_const(count as i32);
            f.instructions().local_set(next_idx);
        }
    }
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
        emit_cursor_advance_by_len(f, lcl.next_cell_idx, ll.len, elem_count);
        // Mirror the per-list bump on each kind's running counter.
        for (next_idx, kb) in [
            (lcl.next_handle_idx, &ll.handle),
            (lcl.next_flags_idx, &ll.flags),
            (lcl.next_record_idx, &ll.record),
            (lcl.next_variant_idx, &ll.variant),
        ] {
            if let (Some(next_idx), Some(slot_base)) = (next_idx, kb.slot_base) {
                emit_snap_and_advance_cursor(f, next_idx, slot_base, ll.len, kb.count_per_elem);
            }
        }
        if ll.nested.is_some() {
            emit_nested_list_pre_pass(f, ctx, plan, &spec, ll, local_base, lcl);
        }
        emit_close_arm_guards(f, spec.arm_guards.len());
    }
}

/// Outer's [`NestedListLocals`]. Panics on stale callers that reach
/// a non-nested list with a `Cell::ListOf` element — guarded by
/// `push_list_of`'s depth-2 cap (`lift/plan.rs`).
fn nested_of(outer_ll: &ListEmitLocals) -> &NestedListLocals {
    outer_ll
        .nested
        .as_ref()
        .expect("Cell::ListOf element ⇒ nested locals allocated")
}

/// Nested-list pre-pass: walk outer's data and bump `next_cell_idx`
/// by `inner_len_j * inner_elem_count` per element. Seeds the cursor
/// to `outer.start_i + outer.len`. Runs inside the outer's arm-guard
/// block. Guarded by `outer.len > 0` so the empty-outer path skips
/// the loop preamble. If the inner element plan contributes records
/// or variants, the same walk also bumps `lcl.next_record_idx` /
/// `lcl.next_variant_idx` so the wrapper-level info slabs are sized
/// correctly.
fn emit_nested_list_pre_pass(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    plan: &LiftPlan,
    outer_spec: &ListSpec<'_>,
    outer_ll: &ListEmitLocals,
    local_base: u32,
    lcl: &WrapperLocals,
) {
    let nested = nested_of(outer_ll);
    let inner_ll = nested.inner.as_ref();
    let cursor = nested.cursor;
    let outer_ptr_scratch = nested.outer_ptr_scratch;
    // `walk_element_plan` produced one side-data entry per inner-plan
    // cell, so the count matches `inner_element_plan.cell_count()`.
    let inner_elem_count = inner_ll.elem_cell_side.len() as u32;
    // Outer element type is `list<T>` (depth-2 cap) → flat layout is
    // (i32 ptr, i32 len) = 8 bytes, len at offset 4. The pre-pass
    // load math below hard-codes that; pin it here so a layout drift
    // in the canon-ABI list lower would surface as a debug-build
    // panic rather than silent misread.
    debug_assert_eq!(
        outer_ll.elem_byte_size, 8,
        "nested-list outer element must be canon-ABI list = (ptr, len) 8 bytes",
    );
    let rows = nested_kind_rows(nested, inner_ll, lcl);
    // Skip cursor seeds + data walk on empty outer.
    f.instructions().local_get(outer_ll.len);
    f.instructions().if_(BlockType::Empty);
    // cursor = outer.start_i + outer.len.
    f.instructions().local_get(outer_ll.start_i);
    f.instructions().local_get(outer_ll.len);
    f.instructions().i32_add();
    f.instructions().local_set(cursor);
    for row in rows {
        let Some(c) = row.cursor else { continue };
        debug_assert!(
            row.kb.count_per_elem > 0,
            "nested cursor allocated only when inner contributes {}",
            row.label,
        );
        // count_per_elem ≤ inner_elem_count by construction → the
        // cell-slab overflow trap below guards this kind's slab too.
        debug_assert!(row.kb.count_per_elem <= inner_elem_count);
        let next_idx = row.next_idx.unwrap_or_else(|| {
            panic!(
                "fn_has_list_elem_{} gate disagrees with nested.kinds.{}",
                row.label, row.label
            )
        });
        f.instructions().local_get(next_idx);
        f.instructions().local_set(c);
    }
    push_widened_get(f, plan, local_base, outer_spec.ptr_slot, WasmType::I32);
    f.instructions().local_set(outer_ptr_scratch);
    // for j in 0..outer.len: inner.len = mem[outer_ptr + j*8 + 4];
    //   trap_check; bump next_cell_idx + each contributed next_<kind>_idx.
    f.instructions().i32_const(0);
    f.instructions().local_set(outer_ll.j);
    f.instructions().block(BlockType::Empty);
    f.instructions().loop_(BlockType::Empty);
    f.instructions().local_get(outer_ll.j);
    f.instructions().local_get(outer_ll.len);
    f.instructions().i32_ge_u();
    f.instructions().br_if(1);
    // inner.len = i32_load (outer_ptr + j*8 + 4)
    f.instructions().local_get(outer_ptr_scratch);
    f.instructions().local_get(outer_ll.j);
    f.instructions().i32_const(8);
    f.instructions().i32_mul();
    f.instructions().i32_add();
    f.instructions().i32_load(MemArg {
        offset: 4,
        align: I32_STORE_LOG2_ALIGN,
        memory_index: 0,
    });
    f.instructions().local_set(inner_ll.len);
    super::super::super::abi::emit::emit_trap_if_list_overflows_cell_slab(
        f,
        inner_ll.len,
        inner_elem_count,
        lcl.next_cell_idx,
        ctx.cell_layout.size,
    );
    emit_cursor_advance_by_len(f, lcl.next_cell_idx, inner_ll.len, inner_elem_count);
    for row in rows {
        if row.cursor.is_none() {
            continue;
        }
        let next_idx = row.next_idx.unwrap_or_else(|| {
            panic!(
                "fn_has_list_elem_{} gate disagrees with nested.kinds.{}",
                row.label, row.label
            )
        });
        emit_cursor_advance_by_len(f, next_idx, inner_ll.len, row.kb.count_per_elem);
    }
    f.instructions().local_get(outer_ll.j);
    f.instructions().i32_const(1);
    f.instructions().i32_add();
    f.instructions().local_set(outer_ll.j);
    f.instructions().br(0);
    f.instructions().end(); // loop
    f.instructions().end(); // block
    f.instructions().end(); // if outer.len > 0
}

/// Emit one cell at `lcl.addr`. `list_slot` is `Some` exactly for
/// `Cell::ListOf`. New `Cell` variants add an arm (no `_` catchall).
fn emit_cell_op(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    cur: PlanCursor<'_>,
    op: &Cell,
    side_data: &CellSideData,
    lcl: &WrapperLocals,
    list_slot: Option<&ListEmitLocals>,
) {
    let PlanCursor {
        plan,
        local_base,
        elem_cell_base,
    } = cur;
    let addr = lcl.addr;
    let cell_layout = ctx.cell_layout;
    match op {
        Cell::Bool { flat_slot }
        | Cell::IntegerSignExt { flat_slot }
        | Cell::IntegerZeroExt { flat_slot }
        | Cell::EnumCase { flat_slot, .. }
        | Cell::Flags { flat_slot, .. }
        | Cell::Char { flat_slot }
        | Cell::Handle { flat_slot, .. } => {
            let src = pin_leaf_flat(f, plan, local_base, *flat_slot, WasmType::I32, lcl);
            emit_single_slot_cell(f, ctx, op, side_data, src, lcl, elem_cell_base);
        }
        Cell::Integer64 { flat_slot } => {
            let src = pin_leaf_flat(f, plan, local_base, *flat_slot, WasmType::I64, lcl);
            emit_single_slot_cell(f, ctx, op, side_data, src, lcl, elem_cell_base);
        }
        Cell::FloatingF32 { flat_slot } => {
            let src = pin_leaf_flat(f, plan, local_base, *flat_slot, WasmType::F32, lcl);
            emit_single_slot_cell(f, ctx, op, side_data, src, lcl, elem_cell_base);
        }
        Cell::FloatingF64 { flat_slot } => {
            let src = pin_leaf_flat(f, plan, local_base, *flat_slot, WasmType::F64, lcl);
            emit_single_slot_cell(f, ctx, op, side_data, src, lcl, elem_cell_base);
        }
        Cell::Option { disc_slot, .. }
        | Cell::Result { disc_slot, .. }
        | Cell::Variant { disc_slot, .. } => {
            let src = pin_leaf_flat(f, plan, local_base, *disc_slot, WasmType::I32, lcl);
            emit_single_slot_cell(f, ctx, op, side_data, src, lcl, elem_cell_base);
        }
        Cell::Text { ptr_slot, len_slot } => {
            let (ptr, len) = pin_text_bytes_slots(f, plan, local_base, *ptr_slot, *len_slot, lcl);
            cell_layout.emit_text(f, addr, ptr, len);
        }
        Cell::Bytes { ptr_slot, len_slot } => {
            let (ptr, len) = pin_text_bytes_slots(f, plan, local_base, *ptr_slot, *len_slot, lcl);
            cell_layout.emit_bytes(f, addr, ptr, len);
        }
        Cell::RecordOf { fields, .. } => {
            let CellSideData::Record(fill) = side_data else {
                panic!("RecordOf cell paired with non-Record side data {side_data:?}");
            };
            emit_record_runtime_fill(f, fill, fields, elem_cell_base, lcl, ctx.record_info);
            let payload = stage_record_cell_payload(f, lcl, fill);
            cell_layout.emit_record_of(f, addr, payload);
        }
        Cell::TupleOf { children } => {
            let CellSideData::Tuple { source: src_kind } = side_data else {
                panic!("TupleOf cell paired with non-Tuple side data {side_data:?}");
            };
            emit_tuple_of_cell(f, cell_layout, addr, children, src_kind, lcl);
        }
        Cell::ListOf {
            ptr_slot,
            element_plan,
            arm_guards,
            ..
        } => {
            let ll =
                list_slot.expect("ListOf cell must arrive with a matching ListEmitLocals slot");
            // Disc-gate cabi_realloc + element loop so an inactive
            // sibling arm's bytes can't surface as `len`.
            emit_open_arm_guards(f, plan, local_base, arm_guards);
            let ptr = pin_leaf_flat(f, plan, local_base, *ptr_slot, WasmType::I32, lcl);
            emit_list_of_arm(f, ctx, ll, ptr, element_plan, lcl);
            emit_close_arm_guards(f, arm_guards.len());
        }
    }
}

/// Emit one single-source cell at `lcl.addr`, reading from `source`.
/// Shared by `emit_cell_op` and `emit_lift_result`'s Direct branch.
/// `elem_cell_base = Some` inside list-element bodies.
fn emit_single_slot_cell(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    cell: &Cell,
    side_data: &CellSideData,
    source: u32,
    lcl: &WrapperLocals,
    elem_cell_base: Option<u32>,
) {
    let addr = lcl.addr;
    let cell_layout = ctx.cell_layout;
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
            emit_flags_runtime_fill(f, source, fill, lcl, ctx.flags_info);
            let payload = stage_flags_cell_payload(f, lcl, fill);
            cell_layout.emit_flags_set(f, addr, payload);
        }
        Cell::Char { .. } => {
            let CellSideData::Char { scratch } = side_data else {
                panic!("Char cell paired with non-Char side data {side_data:?}");
            };
            let scratch_addr_local = lcl
                .char_scratch_addr
                .expect("fn_contains_char must agree with cells reaching here");
            let len_local = lcl.char_len.expect("same gate as char_scratch_addr");
            match scratch {
                CharScratch::Static { scratch_addr } => {
                    f.instructions().i32_const(*scratch_addr);
                    f.instructions().local_set(scratch_addr_local);
                }
                // Caller (emit_list_of_arm) wrote scratch_addr_local.
                CharScratch::Prestaged => {}
            }
            cell_layout.emit_char(f, addr, source, scratch_addr_local, len_local);
        }
        Cell::Handle { kind, .. } => {
            let CellSideData::Handle(fill) = side_data else {
                panic!("Handle cell paired with non-Handle side data {side_data:?}");
            };
            emit_handle_runtime_fill(f, source, fill, lcl, ctx.handle_info);
            let payload = stage_handle_cell_payload(f, lcl, fill);
            cell_layout.emit_handle_cell(f, addr, kind.cell_disc_case(), payload);
        }
        Cell::Option { child_idx, .. } => {
            // Stage inside the `some` arm; none skips it entirely.
            f.instructions().local_get(source);
            f.instructions().if_(BlockType::Empty);
            let child_idx_source = stage_child_idx_source(f, lcl, elem_cell_base, *child_idx);
            cell_layout.emit_option_some(f, addr, child_idx_source);
            f.instructions().else_();
            cell_layout.emit_option_none(f, addr);
            f.instructions().end();
        }
        Cell::Result {
            ok_idx, err_idx, ..
        } => {
            // Stage per arm; unit arms skip (has_payload == false).
            f.instructions().local_get(source);
            f.instructions().if_(BlockType::Empty);
            // wasm `if` fires on non-zero, so err goes in the if block.
            let err_source = match err_idx {
                Some(rel) => stage_child_idx_source(f, lcl, elem_cell_base, *rel),
                None => PayloadSource::ConstI32(0),
            };
            cell_layout.emit_result_err(f, addr, err_idx.is_some(), err_source);
            f.instructions().else_();
            let ok_source = match ok_idx {
                Some(rel) => stage_child_idx_source(f, lcl, elem_cell_base, *rel),
                None => PayloadSource::ConstI32(0),
            };
            cell_layout.emit_result_ok(f, addr, ok_idx.is_some(), ok_source);
            f.instructions().end();
        }
        Cell::Variant { .. } => {
            let CellSideData::Variant(fill) = side_data else {
                panic!("Variant cell paired with non-Variant side data {side_data:?}");
            };
            emit_variant_runtime_fill(f, source, fill, elem_cell_base, lcl, ctx.variant_info);
            let payload = stage_variant_cell_payload(f, lcl, fill);
            cell_layout.emit_variant_case(f, addr, payload);
        }
        Cell::Text { .. }
        | Cell::Bytes { .. }
        | Cell::RecordOf { .. }
        | Cell::TupleOf { .. }
        | Cell::ListOf { .. } => {
            unreachable!("emit_single_slot_cell reached non-single-source Cell {cell:?}")
        }
    }
}

/// Emit one `Cell::ListOf` arm: write the list-of payload, allocate
/// per-call buffers, loop `j ∈ 0..len` lifting each element. Each
/// element cell `k` lands at `start_i + j*elem_count + k`.
fn emit_list_of_arm(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    ll: &ListEmitLocals,
    list_ptr_local: u32,
    element_plan: &LiftPlan,
    lcl: &WrapperLocals,
) {
    let cell_layout = ctx.cell_layout;
    let elem_count = element_plan.cell_count();
    emit_cabi_realloc_call_runtime(f, ctx.cabi_realloc_idx, 4, ll.len, 4, ll.indices_ptr);
    // Per-element utf-8 scratch: chars_per_elem * MAX_UTF8_LEN bytes.
    if let Some(scratch_base) = ll.char_scratch_base {
        emit_cabi_realloc_call_runtime(
            f,
            ctx.cabi_realloc_idx,
            1,
            ll.len,
            ll.chars_per_elem * MAX_UTF8_LEN,
            scratch_base,
        );
    }
    // Per-element tuple-indices buffer: tuple_idx_count_per_elem u32 slots.
    if let Some(buf_base) = ll.tuple_idx_buf_base {
        let elem_bytes = ll
            .tuple_idx_count_per_elem
            .checked_mul(4)
            .expect("tuple_idx_count_per_elem * 4 overflowed u32");
        emit_cabi_realloc_call_runtime(f, ctx.cabi_realloc_idx, 4, ll.len, elem_bytes, buf_base);
    }
    // Per-element flags set-flags scratch.
    if let Some(buf_base) = ll.flags.buf_base {
        emit_cabi_realloc_call_runtime(
            f,
            ctx.cabi_realloc_idx,
            4,
            ll.len,
            ll.flags.bytes_per_elem,
            buf_base,
        );
    }
    // Per-element record field-tuples scratch.
    if let Some(buf_base) = ll.record.buf_base {
        emit_cabi_realloc_call_runtime(
            f,
            ctx.cabi_realloc_idx,
            ctx.record_info.tuple_align,
            ll.len,
            ll.record.bytes_per_elem,
            buf_base,
        );
    }
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

    // elem_cell_base = start_i + j*elem_count — staged once per iter.
    f.instructions().local_get(ll.start_i);
    f.instructions().local_get(ll.j);
    if elem_count != 1 {
        f.instructions().i32_const(elem_count as i32);
        f.instructions().i32_mul();
    }
    f.instructions().i32_add();
    f.instructions().local_set(ll.elem_cell_base);

    // Per-iter `dest = slot_base + j * count_per_elem` across all four
    // info kinds; gated to the kinds this list contributes.
    for (kb, dest, label) in [
        (&ll.handle, lcl.list_elem_handle_base, "handle"),
        (&ll.flags, lcl.list_elem_flags_base, "flags"),
        (&ll.record, lcl.list_elem_record_base, "record"),
        (&ll.variant, lcl.list_elem_variant_base, "variant"),
    ] {
        emit_set_per_iter_slot_base(f, kb, dest, ll.j, label);
    }

    // Per-iter scratch base; only flags/record carry one. Floor stride
    // is 1 cell's worth (gated by Some); pinned as a debug_assert.
    debug_assert!(
        ll.flags.buf_base.is_none() || ll.flags.bytes_per_elem >= STRING_FLAT_BYTES,
        "flags.bytes_per_elem ({}) below 1 cell's pair-bytes ({STRING_FLAT_BYTES})",
        ll.flags.bytes_per_elem,
    );
    emit_set_per_iter_scratch_base(
        f,
        &ll.flags,
        lcl.list_elem_flags_scratch_base,
        ll.j,
        "flags",
    );
    emit_set_per_iter_scratch_base(
        f,
        &ll.record,
        lcl.list_elem_record_tuples_base,
        ll.j,
        "record",
    );

    // Per element-plan cell: stage addr (and any per-iter scratch),
    // then dispatch. `char_idx` walks char-cells so multi-char
    // elements get distinct slots in the per-call buffer.
    let mut char_idx: u32 = 0;
    for (cell_pos, elem_cell) in element_plan.cells.iter().enumerate() {
        emit_set_elem_cell_addr(f, lcl, ll, cell_pos as u32, cell_layout.size);
        match elem_cell {
            Cell::Char { .. } => {
                emit_stage_char_scratch_addr(f, lcl, ll, char_idx);
                char_idx += 1;
            }
            Cell::TupleOf { children } => {
                let CellSideData::Tuple {
                    source: TupleIdxSource::PerIteration { offset_in_elem },
                } = ll.elem_cell_side[cell_pos]
                else {
                    unreachable!(
                        "list-element TupleOf at {cell_pos} must carry PerIteration side data, \
                         got {:?}",
                        ll.elem_cell_side[cell_pos]
                    );
                };
                emit_stage_tuple_slot(f, lcl, ll, offset_in_elem, children);
            }
            // Nested list: copy inner.len from outer's elem-flats; snap
            // inner.start_i + inner.<kind>.slot_base to their running
            // cursors. Iter-end advances them.
            Cell::ListOf { len_slot, .. } => {
                let nested = nested_of(ll);
                let inner_ll = nested.inner.as_ref();
                f.instructions()
                    .local_get(ll.elem_flat_locals[0] + *len_slot);
                f.instructions().local_set(inner_ll.len);
                f.instructions().local_get(nested.cursor);
                f.instructions().local_set(inner_ll.start_i);
                for row in nested_kind_rows(nested, inner_ll, lcl) {
                    let Some(cursor) = row.cursor else { continue };
                    let dest = row.kb.slot_base.unwrap_or_else(|| {
                        panic!("nested.kinds.{} ⇔ inner.{}.slot_base", row.label, row.label)
                    });
                    f.instructions().local_get(cursor);
                    f.instructions().local_set(dest);
                }
            }
            _ => {}
        }
        let list_slot_for_cell = match elem_cell {
            Cell::ListOf { .. } => Some(nested_of(ll).inner.as_ref()),
            _ => None,
        };
        emit_cell_op(
            f,
            ctx,
            PlanCursor {
                plan: element_plan,
                local_base: ll.elem_flat_locals[0],
                elem_cell_base: Some(ll.elem_cell_base),
            },
            elem_cell,
            &ll.elem_cell_side[cell_pos],
            lcl,
            list_slot_for_cell,
        );
        // Advance running cursor(s): cursor += inner.len * inner_elem_count
        // (and per-kind cursors += inner.len * inner.<kind>.count_per_elem).
        // Inner's bases were snapped at iter-start and stay fixed.
        if let Cell::ListOf {
            element_plan: inner_plan,
            ..
        } = elem_cell
        {
            let nested = nested_of(ll);
            let inner_ll = nested.inner.as_ref();
            emit_cursor_advance_by_len(f, nested.cursor, inner_ll.len, inner_plan.cell_count());
            for row in nested_kind_rows(nested, inner_ll, lcl) {
                if let Some(cursor) = row.cursor {
                    emit_cursor_advance_by_len(f, cursor, inner_ll.len, row.kb.count_per_elem);
                }
            }
        }
    }
    debug_assert_eq!(
        char_idx, ll.chars_per_elem,
        "emit walk visited {char_idx} Cell::Char element cells; \
         build_one_list_emit_locals counted {}",
        ll.chars_per_elem,
    );

    // indices_ptr[j*4] = elem_cell_base + root.
    f.instructions().local_get(ll.indices_ptr);
    f.instructions().local_get(ll.j);
    f.instructions().i32_const(4);
    f.instructions().i32_mul();
    f.instructions().i32_add();
    f.instructions().local_get(ll.elem_cell_base);
    if element_plan.root() != 0 {
        f.instructions().i32_const(element_plan.root() as i32);
        f.instructions().i32_add();
    }
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

/// Stage the absolute cell-array address of element-plan position
/// `cell_pos` into `lcl.addr`:
/// `cells_base + (elem_cell_base + cell_pos) * cell_size`.
fn emit_set_elem_cell_addr(
    f: &mut Function,
    lcl: &WrapperLocals,
    ll: &ListEmitLocals,
    cell_pos: u32,
    cell_size: u32,
) {
    f.instructions().local_get(lcl.cells_base);
    f.instructions().local_get(ll.elem_cell_base);
    if cell_pos != 0 {
        f.instructions().i32_const(cell_pos as i32);
        f.instructions().i32_add();
    }
    f.instructions().i32_const(cell_size as i32);
    f.instructions().i32_mul();
    f.instructions().i32_add();
    f.instructions().local_set(lcl.addr);
}

/// Resolve a cell-payload child cell-array index. Static → ConstI32;
/// list-element → stage `elem_cell_base + relative_idx` into
/// `lcl.list_elem_child_idx`. `relative_idx == 0` reuses `base` directly.
fn stage_child_idx_source(
    f: &mut Function,
    lcl: &WrapperLocals,
    elem_cell_base: Option<u32>,
    relative_idx: u32,
) -> PayloadSource {
    let Some(base) = elem_cell_base else {
        return PayloadSource::ConstI32(relative_idx as i32);
    };
    if relative_idx == 0 {
        return PayloadSource::Local(base);
    }
    let dest = lcl
        .list_elem_child_idx
        .expect("fn_has_list_elem_child_idx disagrees with cells reaching here");
    f.instructions().local_get(base);
    f.instructions().i32_const(relative_idx as i32);
    f.instructions().i32_add();
    f.instructions().local_set(dest);
    PayloadSource::Local(dest)
}

/// Emit one `Cell::TupleOf`. Static cells point at the build-time
/// blob; list-element cells consume `lcl.tuple_slot_ptr`.
fn emit_tuple_of_cell(
    f: &mut Function,
    cell_layout: &CellLayout,
    addr: u32,
    children: &[u32],
    src_kind: &TupleIdxSource,
    lcl: &WrapperLocals,
) {
    let len = children.len() as u32;
    match src_kind {
        TupleIdxSource::Static(slice) => {
            debug_assert_eq!(slice.len, len);
            cell_layout.emit_tuple_of(f, addr, PayloadSource::ConstI32(slice.off as i32), len);
        }
        TupleIdxSource::PerIteration { .. } => {
            let slot_ptr_local = lcl.tuple_slot_ptr.expect(
                "tuple_slot_ptr unset — fn_has_list_elem_tuple must agree with \
                 PerIteration cells reaching emit_tuple_of_cell",
            );
            cell_layout.emit_tuple_of(f, addr, PayloadSource::Local(slot_ptr_local), len);
        }
    }
}

/// Stage `lcl.tuple_slot_ptr = ll.tuple_idx_buf_base + j *
/// tuple_idx_count_per_elem * 4 + offset_in_elem`, then write each
/// child's runtime cell-array index (`elem_cell_base + relative`)
/// into `mem[slot_ptr + i*4]`. Called once per iteration before the
/// matching `Cell::TupleOf` element-plan cell's emit fires.
fn emit_stage_tuple_slot(
    f: &mut Function,
    lcl: &WrapperLocals,
    ll: &ListEmitLocals,
    offset_in_elem: u32,
    children: &[u32],
) {
    let buf_base = ll.tuple_idx_buf_base.expect(
        "Cell::TupleOf element requires tuple_idx_buf_base — \
         build_one_list_emit_locals must have allocated it",
    );
    let dest = lcl.tuple_slot_ptr.expect(
        "tuple_slot_ptr unset for list with TupleOf elements — \
         fn_has_list_elem_tuple must include element-plan TupleOf cells",
    );
    // slot_ptr = buf_base + j*stride + offset_in_elem. Stride is
    // always ≥4 (one u32 per child × ≥1 child per TupleOf).
    let stride_bytes = ll
        .tuple_idx_count_per_elem
        .checked_mul(4)
        .expect("tuple_idx_count_per_elem * 4 overflowed u32");
    f.instructions().local_get(buf_base);
    f.instructions().local_get(ll.j);
    f.instructions().i32_const(stride_bytes as i32);
    f.instructions().i32_mul();
    f.instructions().i32_add();
    if offset_in_elem != 0 {
        f.instructions().i32_const(offset_in_elem as i32);
        f.instructions().i32_add();
    }
    f.instructions().local_set(dest);
    // mem[slot_ptr + i*4] = elem_cell_base + child[i]
    for (i, child) in children.iter().enumerate() {
        f.instructions().local_get(dest);
        f.instructions().local_get(ll.elem_cell_base);
        if *child != 0 {
            f.instructions().i32_const(*child as i32);
            f.instructions().i32_add();
        }
        f.instructions().i32_store(MemArg {
            offset: ((i as u32) * 4) as u64,
            align: I32_STORE_LOG2_ALIGN,
            memory_index: 0,
        });
    }
}

/// Stage utf-8 scratch addr for the `char_idx`-th `Cell::Char` of
/// element_plan: `base + (j * chars_per_elem + char_idx) * MAX_UTF8_LEN`.
/// Pairs with `CharScratch::Prestaged`.
fn emit_stage_char_scratch_addr(
    f: &mut Function,
    lcl: &WrapperLocals,
    ll: &ListEmitLocals,
    char_idx: u32,
) {
    let scratch_base = ll
        .char_scratch_base
        .expect("Cell::Char element requires char_scratch_base");
    let scratch_addr_local = lcl
        .char_scratch_addr
        .expect("fn_contains_char must include element-plan chars");
    debug_assert!(char_idx < ll.chars_per_elem);
    // base + (j * chars_per_elem + char_idx) * MAX_UTF8_LEN
    f.instructions().local_get(scratch_base);
    f.instructions().local_get(ll.j);
    if ll.chars_per_elem != 1 {
        f.instructions().i32_const(ll.chars_per_elem as i32);
        f.instructions().i32_mul();
    }
    if char_idx != 0 {
        f.instructions().i32_const(char_idx as i32);
        f.instructions().i32_add();
    }
    if MAX_UTF8_LEN != 1 {
        f.instructions().i32_const(MAX_UTF8_LEN as i32);
        f.instructions().i32_mul();
    }
    f.instructions().i32_add();
    f.instructions().local_set(scratch_addr_local);
}

/// Fill one `Cell::Handle`'s slot: const `type-name` + runtime
/// zero-extended `id`. Static folds the offset into memargs;
/// PerIteration stages slot_addr once and reuses it.
fn emit_handle_runtime_fill(
    f: &mut Function,
    handle_local: u32,
    fill: &HandleRuntimeFill,
    lcl: &WrapperLocals,
    info: HandleInfoOffsets,
) {
    use super::sidetable::handle_info::HandleSlotSource;
    let base_local = lcl
        .handle_info_base
        .expect("fn_has_handle_cells disagrees with cells reaching here");
    let (slot_local, type_name_off, id_off) = match fill.slot_source {
        HandleSlotSource::Static(idx) => {
            let entry_off = idx * info.entry_size;
            (
                base_local,
                entry_off + info.type_name_off,
                entry_off + info.id_off,
            )
        }
        HandleSlotSource::PerIteration { offset_in_elem } => {
            // slot_addr = base + (iter_base + offset_in_elem) * entry_size
            let iter_base = lcl
                .list_elem_handle_base
                .expect("fn_has_list_elem_handle disagrees with walk_element_plan");
            let scratch = lcl
                .handle_slot_addr
                .expect("fn_has_list_elem_handle disagrees with cells reaching here");
            f.instructions().local_get(iter_base);
            if offset_in_elem != 0 {
                f.instructions().i32_const(offset_in_elem as i32);
                f.instructions().i32_add();
            }
            f.instructions().i32_const(info.entry_size as i32);
            f.instructions().i32_mul();
            f.instructions().local_get(base_local);
            f.instructions().i32_add();
            f.instructions().local_set(scratch);
            (scratch, info.type_name_off, info.id_off)
        }
    };

    let store_i32 = |f: &mut Function, off: u32, value: i32| {
        f.instructions().local_get(slot_local);
        f.instructions().i32_const(value);
        f.instructions().i32_store(MemArg {
            offset: off as u64,
            align: I32_STORE_LOG2_ALIGN,
            memory_index: 0,
        });
    };
    store_i32(
        f,
        type_name_off + SLICE_PTR_OFFSET,
        fill.type_name.off as i32,
    );
    store_i32(
        f,
        type_name_off + SLICE_LEN_OFFSET,
        fill.type_name.len as i32,
    );

    f.instructions().local_get(slot_local);
    f.instructions().local_get(handle_local);
    f.instructions().i64_extend_i32_u();
    f.instructions().i64_store(MemArg {
        offset: id_off as u64,
        align: I64_STORE_LOG2_ALIGN,
        memory_index: 0,
    });
}

/// Fill one `Cell::RecordOf`'s slot: write `type-name` + `fields`
/// slice. Static folds offsets into memargs; PerIteration stages
/// entry + tuples addrs from the iter bases and writes each field's
/// `(name, child-cell-idx)` tuple with idx = elem_cell_base + child_pos.
fn emit_record_runtime_fill(
    f: &mut Function,
    fill: &RecordRuntimeFill,
    fields: &[(BlobSlice, u32)],
    elem_cell_base: Option<u32>,
    lcl: &WrapperLocals,
    info: RecordInfoOffsets,
) {
    use super::sidetable::record_info::RecordSlotSource;
    debug_assert_eq!(
        matches!(fill.slot_source, RecordSlotSource::PerIteration { .. }),
        elem_cell_base.is_some(),
        "slot_source / elem_cell_base must agree (PerIteration ↔ Some)",
    );
    let base_local = lcl
        .record_info_base
        .expect("fn_has_record_cells disagrees with cells reaching here");
    let store_i32 = |off: u32| MemArg {
        offset: off as u64,
        align: I32_STORE_LOG2_ALIGN,
        memory_index: 0,
    };

    match fill.slot_source {
        RecordSlotSource::Static {
            entry_idx,
            fields_ptr,
        } => {
            let entry_off = entry_idx * info.entry_size;
            let type_name_off = entry_off + info.type_name_off;
            let fields_off = entry_off + info.fields_off;
            let store_const = |f: &mut Function, off: u32, value: i32| {
                f.instructions().local_get(base_local);
                f.instructions().i32_const(value);
                f.instructions().i32_store(store_i32(off));
            };
            store_const(
                f,
                type_name_off + SLICE_PTR_OFFSET,
                fill.type_name.off as i32,
            );
            store_const(
                f,
                type_name_off + SLICE_LEN_OFFSET,
                fill.type_name.len as i32,
            );
            store_const(f, fields_off + SLICE_PTR_OFFSET, fields_ptr);
            store_const(f, fields_off + SLICE_LEN_OFFSET, fill.fields_len as i32);
        }
        RecordSlotSource::PerIteration {
            entry_offset_in_elem,
            tuples_offset_in_elem,
        } => {
            let iter_entry_base = lcl
                .list_elem_record_base
                .expect("fn_has_list_elem_record disagrees");
            let iter_tuples_base = lcl
                .list_elem_record_tuples_base
                .expect("fn_has_list_elem_record disagrees");
            let entry_dest = lcl
                .record_slot_addr
                .expect("fn_has_list_elem_record disagrees");
            let tuples_dest = lcl
                .record_tuples_slice_addr
                .expect("fn_has_list_elem_record disagrees");
            // entry_addr = base + (iter_entry_base + entry_offset_in_elem) * entry_size
            f.instructions().local_get(iter_entry_base);
            if entry_offset_in_elem != 0 {
                f.instructions().i32_const(entry_offset_in_elem as i32);
                f.instructions().i32_add();
            }
            f.instructions().i32_const(info.entry_size as i32);
            f.instructions().i32_mul();
            f.instructions().local_get(base_local);
            f.instructions().i32_add();
            f.instructions().local_set(entry_dest);
            // tuples_slice_addr = iter_tuples_base + tuples_offset_in_elem
            f.instructions().local_get(iter_tuples_base);
            if tuples_offset_in_elem != 0 {
                f.instructions().i32_const(tuples_offset_in_elem as i32);
                f.instructions().i32_add();
            }
            f.instructions().local_set(tuples_dest);

            // type-name + fields.len const; fields.ptr = tuples_slice_addr.
            let store_const = |f: &mut Function, off: u32, value: i32| {
                f.instructions().local_get(entry_dest);
                f.instructions().i32_const(value);
                f.instructions().i32_store(store_i32(off));
            };
            store_const(
                f,
                info.type_name_off + SLICE_PTR_OFFSET,
                fill.type_name.off as i32,
            );
            store_const(
                f,
                info.type_name_off + SLICE_LEN_OFFSET,
                fill.type_name.len as i32,
            );
            f.instructions().local_get(entry_dest);
            f.instructions().local_get(tuples_dest);
            f.instructions()
                .i32_store(store_i32(info.fields_off + SLICE_PTR_OFFSET));
            store_const(
                f,
                info.fields_off + SLICE_LEN_OFFSET,
                fill.fields_len as i32,
            );

            // tuple at tuples_slice_addr + i*tuple_size:
            // name = const, idx = elem_cell_base + child_pos.
            let elem_base = elem_cell_base.expect(
                "PerIteration record fill needs elem_cell_base — emit_cell_op must thread it",
            );
            debug_assert_eq!(
                fields.len() as u32,
                fill.fields_len,
                "fill.fields_len must match the cell's fields slice",
            );
            for (i, (name, child_pos_in_elem)) in fields.iter().enumerate() {
                let i = i as u32;
                let tuple_off = i * info.tuple_size;
                let store_name = |f: &mut Function, off: u32, value: i32| {
                    f.instructions().local_get(tuples_dest);
                    f.instructions().i32_const(value);
                    f.instructions().i32_store(store_i32(off));
                };
                store_name(
                    f,
                    tuple_off + info.tuple_name_off + SLICE_PTR_OFFSET,
                    name.off as i32,
                );
                store_name(
                    f,
                    tuple_off + info.tuple_name_off + SLICE_LEN_OFFSET,
                    name.len as i32,
                );
                f.instructions().local_get(tuples_dest);
                f.instructions().local_get(elem_base);
                if *child_pos_in_elem != 0 {
                    f.instructions().i32_const(*child_pos_in_elem as i32);
                    f.instructions().i32_add();
                }
                f.instructions()
                    .i32_store(store_i32(tuple_off + info.tuple_idx_off));
            }
        }
    }
}

/// Resolve a `Cell::RecordOf`'s `cell::record-of(idx)` payload.
fn stage_record_cell_payload(
    f: &mut Function,
    lcl: &WrapperLocals,
    fill: &RecordRuntimeFill,
) -> PayloadSource {
    use super::sidetable::record_info::RecordSlotSource;
    match fill.slot_source {
        RecordSlotSource::Static { entry_idx, .. } => PayloadSource::ConstI32(entry_idx as i32),
        RecordSlotSource::PerIteration {
            entry_offset_in_elem,
            ..
        } => {
            let iter_base = lcl
                .list_elem_record_base
                .expect("fn_has_list_elem_record disagrees");
            if entry_offset_in_elem == 0 {
                return PayloadSource::Local(iter_base);
            }
            let dest = lcl
                .record_payload_idx
                .expect("fn_has_list_elem_record disagrees");
            f.instructions().local_get(iter_base);
            f.instructions().i32_const(entry_offset_in_elem as i32);
            f.instructions().i32_add();
            f.instructions().local_set(dest);
            PayloadSource::Local(dest)
        }
    }
}

/// Resolve a `Cell::Flags`'s `cell::flags-set(idx)` payload.
fn stage_flags_cell_payload(
    f: &mut Function,
    lcl: &WrapperLocals,
    fill: &FlagsRuntimeFill,
) -> PayloadSource {
    use super::sidetable::flags_info::FlagsSlotSource;
    match fill.slot_source {
        FlagsSlotSource::Static { entry_idx, .. } => PayloadSource::ConstI32(entry_idx as i32),
        FlagsSlotSource::PerIteration {
            entry_offset_in_elem,
            ..
        } => {
            let iter_base = lcl
                .list_elem_flags_base
                .expect("fn_has_list_elem_flags disagrees");
            if entry_offset_in_elem == 0 {
                return PayloadSource::Local(iter_base);
            }
            let dest = lcl
                .flags_payload_idx
                .expect("fn_has_list_elem_flags disagrees");
            f.instructions().local_get(iter_base);
            f.instructions().i32_const(entry_offset_in_elem as i32);
            f.instructions().i32_add();
            f.instructions().local_set(dest);
            PayloadSource::Local(dest)
        }
    }
}

/// Resolve a `Cell::Handle`'s `cell::*-handle(idx)` payload.
fn stage_handle_cell_payload(
    f: &mut Function,
    lcl: &WrapperLocals,
    fill: &HandleRuntimeFill,
) -> PayloadSource {
    use super::sidetable::handle_info::HandleSlotSource;
    match fill.slot_source {
        HandleSlotSource::Static(idx) => PayloadSource::ConstI32(idx as i32),
        HandleSlotSource::PerIteration { offset_in_elem } => {
            let iter_base = lcl.list_elem_handle_base.expect(
                "list_elem_handle_base unset — fn_has_list_elem_handle gate \
                 disagrees with the cells reaching stage_handle_cell_payload",
            );
            if offset_in_elem == 0 {
                return PayloadSource::Local(iter_base);
            }
            let dest = lcl.handle_payload_idx.expect(
                "handle_payload_idx unset — fn_has_list_elem_handle gate \
                 disagrees with the cells reaching stage_handle_cell_payload",
            );
            f.instructions().local_get(iter_base);
            f.instructions().i32_const(offset_in_elem as i32);
            f.instructions().i32_add();
            f.instructions().local_set(dest);
            PayloadSource::Local(dest)
        }
    }
}

/// Fill one `Cell::Flags`'s slot + scratch: type-name + set-flags.ptr
/// const, bit-walk writes `(name_ptr, name_len)` pairs + count.
/// Unrolled bit-walk — at ≤ 8 bits a loop's overhead dominates.
fn emit_flags_runtime_fill(
    f: &mut Function,
    bitmask_local: u32,
    fill: &FlagsRuntimeFill,
    lcl: &WrapperLocals,
    info: FlagsInfoOffsets,
) {
    use super::sidetable::flags_info::FlagsSlotSource;
    let store_i32 = |off: u32| MemArg {
        offset: off as u64,
        align: I32_STORE_LOG2_ALIGN,
        memory_index: 0,
    };

    let base_local = lcl
        .flags_info_base
        .expect("fn_has_flags_cells disagrees with cells reaching here");
    // Stage entry_addr + flags_addr cursor for the bit-walker.
    let (entry_addr_local, entry_off) = match fill.slot_source {
        FlagsSlotSource::Static {
            entry_idx,
            scratch_addr,
        } => {
            f.instructions().i32_const(scratch_addr);
            f.instructions().local_set(lcl.flags_addr);
            (base_local, entry_idx * info.entry_size)
        }
        FlagsSlotSource::PerIteration {
            entry_offset_in_elem,
            scratch_offset_in_elem,
        } => {
            let entry_dest = lcl
                .flags_slot_addr
                .expect("fn_has_list_elem_flags disagrees");
            let iter_entry_base = lcl
                .list_elem_flags_base
                .expect("fn_has_list_elem_flags disagrees");
            let iter_scratch_base = lcl
                .list_elem_flags_scratch_base
                .expect("fn_has_list_elem_flags disagrees");
            // entry_addr = base + (iter_entry_base + entry_offset) * entry_size
            f.instructions().local_get(iter_entry_base);
            if entry_offset_in_elem != 0 {
                f.instructions().i32_const(entry_offset_in_elem as i32);
                f.instructions().i32_add();
            }
            f.instructions().i32_const(info.entry_size as i32);
            f.instructions().i32_mul();
            f.instructions().local_get(base_local);
            f.instructions().i32_add();
            f.instructions().local_set(entry_dest);
            // flags_addr = iter_scratch_base + scratch_offset_in_elem
            f.instructions().local_get(iter_scratch_base);
            if scratch_offset_in_elem != 0 {
                f.instructions().i32_const(scratch_offset_in_elem as i32);
                f.instructions().i32_add();
            }
            f.instructions().local_set(lcl.flags_addr);
            (entry_dest, 0u32)
        }
    };

    let type_name_off = entry_off + info.type_name_off;
    let set_flags_off = entry_off + info.set_flags_off;

    let store_const = |f: &mut Function, off: u32, value: i32| {
        f.instructions().local_get(entry_addr_local);
        f.instructions().i32_const(value);
        f.instructions().i32_store(store_i32(off));
    };
    store_const(
        f,
        type_name_off + SLICE_PTR_OFFSET,
        fill.type_name.off as i32,
    );
    store_const(
        f,
        type_name_off + SLICE_LEN_OFFSET,
        fill.type_name.len as i32,
    );
    // set-flags.ptr captures flags_addr before the bit-walk advances it.
    f.instructions().local_get(entry_addr_local);
    f.instructions().local_get(lcl.flags_addr);
    f.instructions()
        .i32_store(store_i32(set_flags_off + SLICE_PTR_OFFSET));

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

    // Write set-flags.len = flags_count (runtime).
    f.instructions().local_get(entry_addr_local);
    f.instructions().local_get(lcl.flags_count);
    f.instructions()
        .i32_store(store_i32(set_flags_off + SLICE_LEN_OFFSET));
}

/// Fill one `Cell::Variant`'s slot: const `type-name` + disc-dispatched
/// `case-name` + `payload` (option<u32>). N-way disc dispatch is nested
/// if/else; `br_table` is a future optimization.
fn emit_variant_runtime_fill(
    f: &mut Function,
    disc_local: u32,
    fill: &VariantRuntimeFill,
    elem_cell_base: Option<u32>,
    lcl: &WrapperLocals,
    info: VariantInfoOffsets,
) {
    use super::sidetable::variant_info::VariantSlotSource;
    // PerIteration ↔ Some couple: walk_element_plan only assigns
    // PerIteration to list-element cells; drift would silently emit
    // child_idx as a const ignoring `elem_cell_base`.
    debug_assert_eq!(
        matches!(fill.slot_source, VariantSlotSource::PerIteration { .. }),
        elem_cell_base.is_some(),
        "slot_source / elem_cell_base must agree (PerIteration ↔ Some)",
    );
    let base_local = lcl
        .variant_info_base
        .expect("fn_has_variant_cells disagrees with cells reaching here");
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

    // Static folds offsets into memargs; PerIteration stages slot_addr.
    let (slot_local, slot_off) = match fill.slot_source {
        VariantSlotSource::Static { entry_idx } => (base_local, entry_idx * info.entry_size),
        VariantSlotSource::PerIteration {
            entry_offset_in_elem,
        } => {
            let iter_base = lcl
                .list_elem_variant_base
                .expect("fn_has_list_elem_variant disagrees");
            let dest = lcl
                .variant_slot_addr
                .expect("fn_has_list_elem_variant disagrees");
            // slot_addr = base + (iter_base + entry_offset_in_elem) * entry_size
            f.instructions().local_get(iter_base);
            if entry_offset_in_elem != 0 {
                f.instructions().i32_const(entry_offset_in_elem as i32);
                f.instructions().i32_add();
            }
            f.instructions().i32_const(info.entry_size as i32);
            f.instructions().i32_mul();
            f.instructions().local_get(base_local);
            f.instructions().i32_add();
            f.instructions().local_set(dest);
            (dest, 0u32)
        }
    };

    let type_name_off = slot_off + info.type_name_off;
    let store_const = |f: &mut Function, off: u32, value: i32| {
        f.instructions().local_get(slot_local);
        f.instructions().i32_const(value);
        f.instructions().i32_store(store_i32(off));
    };
    store_const(
        f,
        type_name_off + SLICE_PTR_OFFSET,
        fill.type_name.off as i32,
    );
    store_const(
        f,
        type_name_off + SLICE_LEN_OFFSET,
        fill.type_name.len as i32,
    );

    let case_name_off = slot_off + info.case_name_off;
    let payload_off = slot_off + info.payload_off;
    let payload_value_off = payload_off + info.payload_value_off;

    debug_assert_eq!(fill.case_names.len(), fill.per_case_payload.len());

    // Nested if/else per disc; last arm has no else (canonical-ABI
    // disc out of range is unreachable).
    for (i, name) in fill.case_names.iter().enumerate() {
        let is_last = i + 1 == fill.case_names.len();
        if !is_last {
            f.instructions().local_get(disc_local);
            f.instructions().i32_const(i as i32);
            f.instructions().i32_eq();
            f.instructions().if_(BlockType::Empty);
        }
        store_const(f, case_name_off + SLICE_PTR_OFFSET, name.off as i32);
        store_const(f, case_name_off + SLICE_LEN_OFFSET, name.len as i32);
        // child_idx is plan-relative; list-element resolves to
        // elem_cell_base + child_pos at runtime.
        match fill.per_case_payload[i] {
            Some(child_idx) => {
                f.instructions().local_get(slot_local);
                f.instructions().i32_const(OPTION_SOME as i32);
                f.instructions().i32_store8(store_i8(payload_off));
                f.instructions().local_get(slot_local);
                match elem_cell_base {
                    None => {
                        f.instructions().i32_const(child_idx as i32);
                    }
                    Some(base) => {
                        f.instructions().local_get(base);
                        if child_idx != 0 {
                            f.instructions().i32_const(child_idx as i32);
                            f.instructions().i32_add();
                        }
                    }
                }
                f.instructions().i32_store(store_i32(payload_value_off));
            }
            None => {
                // value slot untouched (irrelevant when disc=0)
                f.instructions().local_get(slot_local);
                f.instructions().i32_const(OPTION_NONE as i32);
                f.instructions().i32_store8(store_i8(payload_off));
            }
        }
        if !is_last {
            f.instructions().else_();
        }
    }
    for _ in 0..fill.case_names.len().saturating_sub(1) {
        f.instructions().end();
    }
}

/// Resolve a `Cell::Variant`'s `cell::variant-case(idx)` payload.
fn stage_variant_cell_payload(
    f: &mut Function,
    lcl: &WrapperLocals,
    fill: &VariantRuntimeFill,
) -> PayloadSource {
    use super::sidetable::variant_info::VariantSlotSource;
    match fill.slot_source {
        VariantSlotSource::Static { entry_idx } => PayloadSource::ConstI32(entry_idx as i32),
        VariantSlotSource::PerIteration {
            entry_offset_in_elem,
        } => {
            let iter_base = lcl
                .list_elem_variant_base
                .expect("fn_has_list_elem_variant disagrees");
            if entry_offset_in_elem == 0 {
                return PayloadSource::Local(iter_base);
            }
            let dest = lcl
                .variant_payload_idx
                .expect("fn_has_list_elem_variant disagrees");
            f.instructions().local_get(iter_base);
            f.instructions().i32_const(entry_offset_in_elem as i32);
            f.instructions().i32_add();
            f.instructions().local_set(dest);
            PayloadSource::Local(dest)
        }
    }
}

/// Emit lift for a single-cell Direct result at `lcl.addr`. Compound
/// results go through `emit_lift_compound_prefix` + `emit_lift_plan`.
pub(crate) fn emit_lift_result(
    f: &mut Function,
    ctx: &LiftEmitCtx<'_>,
    plan: &ResultEmitPlan<'_>,
    lcl: &WrapperLocals,
) {
    match plan {
        ResultEmitPlan::Direct {
            cell,
            source_local,
            side_data,
        } => {
            // classify_result_lift routes Compound-only kinds away
            // from Direct.
            debug_assert!(
                !matches!(
                    cell,
                    Cell::Option { .. } | Cell::Result { .. } | Cell::Variant { .. }
                ),
                "Direct result reached emit_lift_result with Compound-only cell {cell:?}",
            );
            emit_single_slot_cell(f, ctx, cell, side_data, *source_local, lcl, None);
        }
        ResultEmitPlan::Compound { .. } | ResultEmitPlan::None => unreachable!(
            "Compound is emitted via emit_lift_compound_prefix + emit_lift_plan; \
             emit_lift_result handles only Direct sources"
        ),
    }
}

/// Emit compound-result prefix: load bytes from `retptr_offset` via
/// `loads`, then capture each flat value into `synth_locals` in
/// REVERSE order (wasm stack is LIFO).
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
    f.instructions().i32_const(retptr_offset);
    f.instructions().local_set(addr_local);
    for inst in loads {
        f.instruction(inst);
    }
    // Reverse order: stack top is the last pushed (highest slot).
    for &local in synth_locals.iter().rev() {
        f.instructions().local_set(local);
    }
}
