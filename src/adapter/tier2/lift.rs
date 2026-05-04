//! Tier-2 lift codegen: classifying WIT types into cell variants,
//! emitting the wasm that writes one cell per (param | result),
//! and laying out the per-field-tree side tables (`enum-infos`,
//! `record-infos`; `flags-infos` / `variant-infos` / `handle-infos`
//! join as their lift codegen lands).
//!
//! Split from `emit.rs` so the dispatch-orchestration code there
//! doesn't pull in every cell-variant detail.
//!
//! See [`docs/tiers/lift-codegen.md`](../../../docs/tiers/lift-codegen.md)
//! for the cross-tier design (data flow, invariants, why the plan
//! data structure exists).
//!
//! Three layers:
//! - **Classify.** [`LiftPlanBuilder`] walks a WIT type and emits a
//!   flat [`LiftPlan`] of [`CellOp`]s in allocation order — `cells[0]`
//!   is the root, child cells follow their parents. The plan owns
//!   the cell-index space; side-table contributions reference cells
//!   by `Vec`-position into the same plan.
//! - **Side-table population.** [`register_side_table_strings`] +
//!   [`build_side_table_blob`] (and per-kind facades) precompute the
//!   per-field-tree side tables at adapter-build time. Walks the
//!   per-param plans for nominal `CellOp` cases.
//! - **Codegen.** [`emit_lift_param`] walks `plan.cells` and emits
//!   one wasm cell per `CellOp`. [`emit_lift_result`] handles result
//!   lifts (kept on the legacy `LiftKind`+`ResultSource` path until
//!   compound result lifts land).

use std::collections::HashMap;

use wasm_encoder::{Function, Instruction, MemArg, ValType};
use wit_bindgen_core::abi::lift_from_memory;
use wit_parser::abi::WasmSignature;
use wit_parser::{Function as WitFunction, Resolve, SizeAlign, Type};

use super::super::abi::emit::{
    direct_return_type, wasm_type_to_val, RecordLayout, SLICE_LEN_OFFSET, SLICE_PTR_OFFSET,
};
use super::super::abi::WasmEncoderBindgen;
use super::super::indices::FunctionIndices;
use super::blob::{BlobSlice, RecordWriter, Reloc, Segment, SymRef, SymbolId};
use super::cells::CellLayout;
use super::emit::{
    FuncClassified, FuncDispatch, SchemaLayouts, RECORD_FIELD_TUPLE_IDX, RECORD_FIELD_TUPLE_NAME,
    RECORD_INFO_FIELDS,
};

// ─── WIT names referenced by lift codegen ─────────────────────────
//
// Side-table-info records in `splicer:common/types` share the same
// shape: `record { type-name: string, <item>-name: string }`. Field
// names below are passed to [`SideTableSpec`] per kind.
const INFO_TYPE_NAME: &str = "type-name";
const ENUM_INFO_CASE_NAME: &str = "case-name";

// ─── Classification + lift descriptors ────────────────────────────

/// How a WIT type maps to a `cell` variant. Wired variants are
/// implemented end-to-end (lift codegen produces real cells);
/// un-wired variants (Phase 2-2b / 2-4) classify here without panic
/// but `todo!()` at the codegen layer (`cells.rs`) when actually
/// reached at adapter-build time.
#[derive(Clone, Copy, Debug)]
pub(super) enum LiftKind {
    // ── Phase 2-2a (wired) ────────────────────────────────────────
    /// `bool` — 1 i32 slot (0/1) → `cell::bool`.
    Bool,
    /// `s8`/`s16`/`s32` — 1 i32 slot, sign-extend → `cell::integer`.
    IntegerSignExt,
    /// `u8`/`u16`/`u32` — 1 i32 slot, zero-extend → `cell::integer`.
    IntegerZeroExt,
    /// `s64`/`u64` — 1 i64 slot, no widen → `cell::integer`.
    Integer64,
    /// `f32` — 1 f32 slot, `f64.promote_f32` → `cell::floating`.
    FloatingF32,
    /// `f64` — 1 f64 slot, no widen → `cell::floating`.
    FloatingF64,
    /// `string` — 2 i32 slots (ptr, len) → `cell::text`.
    Text,
    /// `list<u8>` — 2 i32 slots (ptr, len) → `cell::bytes`.
    Bytes,

    // ── Phase 2-2b (todo!() in cells.rs) ─────────────────────────
    /// `char` → `cell::text` (utf-8 encode the i32 code point).
    Char,
    /// `list<T>` (non-u8 element) → `cell::list-of`.
    ListOf,
    /// `tuple<...>` → `cell::tuple-of`.
    TupleOf,
    /// `option<T>` → `cell::option-some(u32)` or `cell::option-none`.
    Option,
    /// `result<T, E>` → `cell::result-ok(option<u32>)` or `cell::result-err(option<u32>)`.
    Result,
    /// `record { ... }` → `cell::record-of(u32)` (side-table index).
    Record,
    /// `flags { ... }` → `cell::flags-set(u32)`.
    Flags,
    /// `enum { ... }` → `cell::enum-case(u32)`.
    Enum,
    /// `variant { ... }` → `cell::variant-case(u32)`.
    Variant,

    // ── Phase 2-4 (todo!() in cells.rs) ──────────────────────────
    /// `own<R>` / `borrow<R>` → `cell::resource-handle(u32)`.
    Handle,
    /// `future<T>` → `cell::future-handle(u32)`.
    Future,
    /// `stream<T>` → `cell::stream-handle(u32)`.
    Stream,

    // ── Future work ──────────────────────────────────────────────
    /// `error-context` — no cell variant yet; design TBD.
    ErrorContext,
}

impl LiftKind {
    /// Classify a WIT param type. Infallible: every `Type` maps to a
    /// `LiftKind`. Codegen for un-wired variants `todo!()`s in
    /// `cells.rs` / `slot_count` when actually reached.
    pub(super) fn classify(ty: &Type, resolve: &Resolve) -> LiftKind {
        match ty {
            Type::Bool => LiftKind::Bool,
            Type::S8 | Type::S16 | Type::S32 => LiftKind::IntegerSignExt,
            Type::U8 | Type::U16 | Type::U32 => LiftKind::IntegerZeroExt,
            Type::S64 | Type::U64 => LiftKind::Integer64,
            Type::F32 => LiftKind::FloatingF32,
            Type::F64 => LiftKind::FloatingF64,
            Type::String => LiftKind::Text,
            Type::Char => LiftKind::Char,
            Type::ErrorContext => LiftKind::ErrorContext,
            Type::Id(id) => match &resolve.types[*id].kind {
                wit_parser::TypeDefKind::List(Type::U8) => LiftKind::Bytes,
                wit_parser::TypeDefKind::List(_) => LiftKind::ListOf,
                wit_parser::TypeDefKind::Tuple(_) => LiftKind::TupleOf,
                wit_parser::TypeDefKind::Record(_) => LiftKind::Record,
                wit_parser::TypeDefKind::Variant(_) => LiftKind::Variant,
                wit_parser::TypeDefKind::Enum(_) => LiftKind::Enum,
                wit_parser::TypeDefKind::Flags(_) => LiftKind::Flags,
                wit_parser::TypeDefKind::Option(_) => LiftKind::Option,
                wit_parser::TypeDefKind::Result(_) => LiftKind::Result,
                wit_parser::TypeDefKind::Handle(_) => LiftKind::Handle,
                wit_parser::TypeDefKind::Future(_) => LiftKind::Future,
                wit_parser::TypeDefKind::Stream(_) => LiftKind::Stream,
                // Type aliases peel through and reclassify the
                // underlying type.
                wit_parser::TypeDefKind::Type(t) => LiftKind::classify(t, resolve),
                wit_parser::TypeDefKind::FixedLengthList(_, _)
                | wit_parser::TypeDefKind::Map(_, _)
                | wit_parser::TypeDefKind::Resource
                | wit_parser::TypeDefKind::Unknown => {
                    todo!(
                        "tier-2 lift: unsupported TypeDefKind {:?}",
                        &resolve.types[*id].kind
                    )
                }
            },
        }
    }
}

// ─── Lift plan: structural single-source-of-truth ─────────────────
//
// A LiftPlan describes one (param | result) lift end-to-end:
// every cell that needs to be written, in allocation order, with
// each cell bound to its source wasm locals (concrete absolute
// indices) and any nominal-cell side-table info it carries.
// `cells[0]` is the root that the field-tree's `root` field points at.
//
// Why a flat Vec instead of a nested IR: cell indices in
// nominal-cell side-table entries (e.g., a record's `fields`
// list) are just `Vec`-positions in `cells`. The same vector that
// drives codegen also drives side-table emission; child indices
// can't desync because they're a property of allocation order.
// `cells.len()` is the slab size; total flat-slot consumption is
// recorded explicitly on [`LiftPlan::flat_slot_count`]. See
// [`docs/tiers/lift-codegen.md`](../../../docs/tiers/lift-codegen.md).

/// One cell to write at a known cell-array index. Each variant
/// captures the cell's runtime-disc semantics, its source wasm
/// locals (absolute wasm-local indices), and any side-table info
/// this cell contributes (e.g., enum-info / record-info entries).
pub(super) enum CellOp {
    Bool {
        local: u32,
    },
    IntegerSignExt {
        local: u32,
    },
    IntegerZeroExt {
        local: u32,
    },
    Integer64 {
        local: u32,
    },
    FloatingF32 {
        local: u32,
    },
    FloatingF64 {
        local: u32,
    },
    Text {
        ptr: u32,
        len: u32,
    },
    Bytes {
        ptr: u32,
        len: u32,
    },
    EnumCase {
        local: u32,
        info: NamedListInfo,
    },
    RecordOf {
        type_name: String,
        /// `(field-name, child-cell-idx)` per field, in WIT order.
        /// `child-cell-idx` indexes into the same `LiftPlan::cells`.
        fields: Vec<(String, u32)>,
    },
}

/// Plan for lifting one (param | result) into a cell tree. Cells
/// are listed in allocation order; `cells[0]` is the root that the
/// field-tree's `root` field points at. Walked top-to-bottom by the
/// emit-code phase; the side-table builder also walks `cells` to
/// pull out per-kind side-table contributions.
pub(super) struct LiftPlan {
    pub cells: Vec<CellOp>,
    /// Total flat-slot locals consumed by the plan. Owners use this
    /// to assert that the absolute wasm-local indices baked into
    /// `cells` match the locals they intended to feed in.
    pub flat_slot_count: u32,
}

impl LiftPlan {
    pub(super) fn cell_count(&self) -> u32 {
        self.cells.len() as u32
    }

    /// Iterator over every `CellOp::EnumCase` in the plan. Used by
    /// the side-table builder to register enum strings.
    pub(super) fn enum_infos(&self) -> impl Iterator<Item = &NamedListInfo> {
        self.cells.iter().filter_map(|op| match op {
            CellOp::EnumCase { info, .. } => Some(info),
            _ => None,
        })
    }

    /// Iterator over every `CellOp::RecordOf` in the plan. Used by
    /// the record-info side-table builder.
    pub(super) fn record_ofs(&self) -> impl Iterator<Item = (&str, &[(String, u32)])> {
        self.cells.iter().filter_map(|op| match op {
            CellOp::RecordOf { type_name, fields } => Some((type_name.as_str(), fields.as_slice())),
            _ => None,
        })
    }
}

// ─── Lift plan builder ────────────────────────────────────────────

/// Allocates cells + flat-slot locals while walking a WIT type.
/// The "parent before children" recursion in [`Self::push`] is what
/// makes child cell indices observable from the parent's side-table
/// info (a child's index is just `cells.len()` after its sub-call
/// has appended).
///
/// Caller provides `local_base` — the absolute wasm-local index that
/// the plan's first flat slot occupies. Cells receive absolute indices
/// `local_base..local_base + flat_slot_count` baked in at build time;
/// the emit phase walks the plan with no further base resolution.
struct LiftPlanBuilder {
    cells: Vec<CellOp>,
    /// Absolute wasm-local index the builder started from. Used only
    /// to compute `flat_slot_count` at `into_plan` time.
    local_base: u32,
    /// Next available absolute wasm-local index. Incremented by
    /// `bump_local` as cells consume flat slots.
    next_local: u32,
}

impl LiftPlanBuilder {
    fn new(local_base: u32) -> Self {
        Self {
            cells: Vec::new(),
            local_base,
            next_local: local_base,
        }
    }

    /// Push cells for one lift; returns the root cell's index.
    fn push(&mut self, ty: &Type, resolve: &Resolve) -> u32 {
        let root_idx = self.cells.len() as u32;
        match LiftKind::classify(ty, resolve) {
            LiftKind::Bool => {
                let local = self.bump_local();
                self.cells.push(CellOp::Bool { local });
            }
            LiftKind::IntegerSignExt => {
                let local = self.bump_local();
                self.cells.push(CellOp::IntegerSignExt { local });
            }
            LiftKind::IntegerZeroExt => {
                let local = self.bump_local();
                self.cells.push(CellOp::IntegerZeroExt { local });
            }
            LiftKind::Integer64 => {
                let local = self.bump_local();
                self.cells.push(CellOp::Integer64 { local });
            }
            LiftKind::FloatingF32 => {
                let local = self.bump_local();
                self.cells.push(CellOp::FloatingF32 { local });
            }
            LiftKind::FloatingF64 => {
                let local = self.bump_local();
                self.cells.push(CellOp::FloatingF64 { local });
            }
            LiftKind::Text => {
                let ptr = self.bump_local();
                let len = self.bump_local();
                self.cells.push(CellOp::Text { ptr, len });
            }
            LiftKind::Bytes => {
                let ptr = self.bump_local();
                let len = self.bump_local();
                self.cells.push(CellOp::Bytes { ptr, len });
            }
            LiftKind::Enum => {
                let info = enum_lift_info_for_type(ty, resolve)
                    .expect("LiftKind::Enum classify implies enum-info available");
                let local = self.bump_local();
                self.cells.push(CellOp::EnumCase { local, info });
            }
            LiftKind::Record => self.push_record(ty, resolve, root_idx),
            other => todo!("Phase 2-2b/2-4 plan-builder for {other:?}"),
        }
        root_idx
    }

    fn bump_local(&mut self) -> u32 {
        let r = self.next_local;
        self.next_local = self
            .next_local
            .checked_add(1)
            // Tripwire; realistic blow-ups are caught by `check_layout_budget`.
            .expect("LiftPlanBuilder flat-slot counter overflowed u32");
        r
    }

    /// Records: push the parent first, recurse on each field
    /// (children get appended to `cells` AFTER the parent, so their
    /// returned root indices are the indices the parent's `fields`
    /// list needs), then backfill the parent's `fields`.
    fn push_record(&mut self, ty: &Type, resolve: &Resolve, root_idx: u32) {
        let Type::Id(id) = ty else {
            unreachable!("LiftKind::Record came from non-Id type")
        };
        let typedef = &resolve.types[*id];
        let wit_parser::TypeDefKind::Record(r) = &typedef.kind else {
            unreachable!("LiftKind::Record came from non-Record kind")
        };
        let type_name = typedef.name.clone().unwrap_or_default();
        // Reserve the parent slot at root_idx.
        self.cells.push(CellOp::RecordOf {
            type_name,
            fields: Vec::new(),
        });
        // Recurse on each field; children get appended after parent.
        let mut fields = Vec::with_capacity(r.fields.len());
        for field in &r.fields {
            let child_idx = self.push(&field.ty, resolve);
            fields.push((field.name.clone(), child_idx));
        }
        // Backfill the parent's `fields` with the now-known child indices.
        match &mut self.cells[root_idx as usize] {
            CellOp::RecordOf { fields: f, .. } => *f = fields,
            _ => unreachable!("just pushed RecordOf at root_idx"),
        }
    }

    fn into_plan(self) -> LiftPlan {
        LiftPlan {
            cells: self.cells,
            flat_slot_count: self.next_local - self.local_base,
        }
    }
}

// ─── Result-lift descriptors (classify-time, immutable) ───────────
//
// Three shapes:
//
// - `Direct(kind)` — primitive that fits in one flat slot, captured
//   into `lcl.result` after the handler call.
// - `RetptrPair(kind)` — `(ptr, len)` for string / `list<u8>` returns;
//   the wrapper loads the pair from retptr scratch into
//   `lcl.ptr_scratch` / `lcl.len_scratch` before lifting.
// - `Compound(CompoundResult)` — record / tuple / etc. that lives in
//   memory at retptr scratch. Driven by a [`LiftPlan`] symmetric with
//   the per-param plans; `wit_bindgen_core::abi::lift_from_memory`
//   pushes flat values onto the wasm stack from retptr_offset, and
//   the wrapper `local.set`s those into per-result synthetic locals
//   (in reverse, since the stack is LIFO) for the plan walker.
//
// All offsets (retptr_offset for RetptrPair / Compound, cells_offset
// + record_info_cell_idx for Compound) live on the post-layout
// [`ResultLayout`] / [`ResultSourceLayout`]; this classify-time type
// has no offsets and never gets mutated.

/// Classify-time descriptor for the function's return value. The
/// layout phase wraps it into a [`ResultLayout`] with the offsets
/// once those are known. `side_table` populates the result tree's
/// side tables at adapter-build time. Compound results carry their
/// side-table contributions inline on the plan's `CellOp`s instead.
pub(super) struct ResultLift {
    pub source: ResultSource,
    pub side_table: SideTableInfo,
}

pub(super) enum ResultSource {
    /// Direct primitive (no retptr): source is `lcl.result`.
    Direct(LiftKind),
    /// `(ptr, len)` pair in retptr scratch (string / `list<u8>`).
    RetptrPair(LiftKind),
    /// Compound result: walk a [`LiftPlan`] over flat slots loaded
    /// from retptr scratch via `lift_from_memory`.
    Compound(CompoundResult),
}

/// Per-fn compound-result classify output: which WIT type to lift
/// plus a structural cell-tree plan. The retptr scratch byte offset,
/// the cells-slab byte offset, and the per-cell record-info side-
/// table indices are all layout-phase outputs — they live on
/// [`ResultSourceLayout::Compound`], not here.
///
/// `plan`'s cells carry placeholder local indices (built with
/// `local_base = 0`) — only the structural fields (cell variants,
/// record-of `fields`, enum infos) are read by the side-table
/// builders. The emit phase rebuilds a fresh plan with the actual
/// synth-local base baked into the cells; the rebuilt plan lives on
/// [`ResultEmitPlan::Compound::plan`].
pub(super) struct CompoundResult {
    /// WIT type of the result value — kept around so the wrapper
    /// body can drive `lift_from_memory` through `WasmEncoderBindgen`,
    /// and so the emit phase can rebuild the cell plan with the
    /// correct synth-local base.
    pub ty: Type,
    /// Structural cell plan. Local indices are placeholder
    /// (`local_base = 0`); only cell structure is consumed.
    pub plan: LiftPlan,
}

impl ResultLift {
    /// Returns `Some(&CompoundResult)` for compound result lifts;
    /// `None` otherwise.
    pub(super) fn compound(&self) -> Option<&CompoundResult> {
        match &self.source {
            ResultSource::Compound(c) => Some(c),
            _ => None,
        }
    }
}

/// Classify-time per-parameter lift recipe. The plan's cells already
/// carry absolute wasm-local indices (the plan-builder bakes them in
/// from each param's cumulative flat-slot cursor). Cells-slab offset
/// + per-cell record-info indices live on the post-layout
/// [`ParamLayout`].
pub(super) struct ParamLift {
    pub name: BlobSlice,
    pub plan: LiftPlan,
}

// ─── Layout-phase outputs (immutable, includes offsets) ───────────
//
// The layout phase wraps each classify-time `ParamLift` /
// `ResultLift` with the offsets it computes. These types are what
// the emit phase reads — they're constructed once at the end of
// layout and never mutated. The "all `: 0  // back-filled later`
// placeholders" failure mode in the audit follow-up doc is
// structurally impossible with this split.

/// Post-layout per-parameter lift descriptor: the classify-time
/// data plus its cells-slab offset + per-cell record-info indices.
pub(super) struct ParamLayout {
    pub lift: ParamLift,
    /// Byte offset of this param's contiguous cells slab within
    /// the static data segment; the slab holds `lift.plan.cell_count()`
    /// cells, each `cell_layout.size` bytes.
    pub cells_offset: u32,
    /// Per cell of `lift.plan.cells`: the side-table index for
    /// `CellOp::RecordOf` cells, `None` for other cell kinds.
    pub record_info_cell_idx: Vec<Option<u32>>,
}

/// Post-layout per-result lift descriptor: a sum-type `source`
/// carrying both the lift kind and any layout-derived offsets
/// per-variant. The classify-time `side_table` info isn't carried
/// here — it's consumed by the side-table builders during the
/// layout phase (which see [`FuncClassified::result_lift`]'s
/// pre-layout `side_table` directly).
pub(super) struct ResultLayout {
    pub source: ResultSourceLayout,
}

pub(super) enum ResultSourceLayout {
    /// Direct primitive (no retptr): source is `lcl.result`.
    Direct(LiftKind),
    /// `(ptr, len)` pair at the function's retptr scratch.
    RetptrPair { kind: LiftKind, retptr_offset: i32 },
    /// Compound result: classify-time recipe plus layout offsets
    /// (retptr scratch + per-cell record-info side-table indices for
    /// `CellOp::RecordOf` cells). The cells-slab base is _not_
    /// duplicated here; the wrapper body reads it off
    /// [`AfterSetup::result_cells_offset`] (the canonical source —
    /// today's compound lifts only fire from the after-hook path).
    Compound {
        compound: CompoundResult,
        retptr_offset: i32,
        /// One entry per cell of `compound.plan.cells`, in plan
        /// order. `Some(idx)` for `CellOp::RecordOf` cells, `None`
        /// for other kinds. Built unconditionally (it's a property
        /// of the cell plan, not of the after-hook wiring).
        record_info_cell_idx: Vec<Option<u32>>,
    },
}

/// Result-side-table info, populated when a result is lift-able.
/// Mirrors the per-kind options on `SideTableInfo` but only for
/// results (params now carry their info inline in their LiftPlan
/// `CellOp`s).
#[derive(Default, Clone)]
pub(super) struct SideTableInfo {
    /// `Some` for enum-typed result lifts: carries the enum's type-name
    /// plus its case names in disc order.
    pub enum_info: Option<NamedListInfo>,
}

/// A type-name plus an ordered list of item names. Carries
/// enough info to populate any of the `*-info` side-table records
/// in `splicer:common/types` that share the
/// `{ type-name, <item>-name }` shape (enum-info, eventually flags-info
/// + variant-info).
#[derive(Clone)]
pub(super) struct NamedListInfo {
    pub type_name: String,
    /// Item names in WIT declaration order — the i'th entry's WIT
    /// declaration index equals `i` (matching the disc / bit-position
    /// / field-index used at runtime).
    pub item_names: Vec<String>,
}

// ─── Classifiers ──────────────────────────────────────────────────

/// Build a [`LiftPlan`] for every WIT param of `func`. `slot_cursor`
/// threads across params, seeding each param's plan with the absolute
/// wasm-local index of its first flat slot — the cumulative flat-slot
/// count of preceding params. Cells in the resulting plan carry their
/// final wasm-local indices baked in.
pub(super) fn classify_func_params(
    resolve: &Resolve,
    func: &WitFunction,
    name_blob: &mut Vec<u8>,
) -> Vec<ParamLift> {
    let mut params_lift: Vec<ParamLift> = Vec::with_capacity(func.params.len());
    let mut slot_cursor: u32 = 0;
    for param in &func.params {
        let pname = &param.name;
        let name = append_param_name(name_blob, pname);
        let mut builder = LiftPlanBuilder::new(slot_cursor);
        builder.push(&param.ty, resolve);
        let plan = builder.into_plan();
        let consumed = plan.flat_slot_count;
        params_lift.push(ParamLift { name, plan });
        slot_cursor += consumed;
    }
    params_lift
}

fn append_param_name(name_blob: &mut Vec<u8>, name: &str) -> BlobSlice {
    let off = name_blob.len() as u32;
    name_blob.extend_from_slice(name.as_bytes());
    BlobSlice {
        off,
        len: name.len() as u32,
    }
}

/// Classify the function's return value for on-return lift. Direct
/// primitive returns capture into `lcl.result`; string / `list<u8>`
/// returns ride retptr. Compound returns we don't yet lift bail out
/// with `None` (the wrapper still calls the after-hook with
/// `result: option::none`).
///
/// For async funcs canon-lower-async always retptr's a non-void
/// result, so even primitive results live at the retptr scratch.
/// Returns `None` only for void functions or unsupported result
/// kinds.
pub(super) fn classify_result_lift(
    resolve: &Resolve,
    func: &WitFunction,
    export_sig: &WasmSignature,
    import_sig: &WasmSignature,
    is_async: bool,
) -> Option<ResultLift> {
    let ty = func.result.as_ref()?;
    let kind = LiftKind::classify(ty, resolve);

    // Compound kinds (currently just Record) drive a LiftPlan over
    // retptr-loaded flat slots. Classify-time we build a structural
    // plan with `local_base = 0` (placeholder); only the cell-tree
    // structure is read by the side-table builders. The emit phase
    // rebuilds a fresh plan with the actual synth-local base baked
    // into the cells (see `alloc_wrapper_locals`).
    if matches!(kind, LiftKind::Record) {
        let mut builder = LiftPlanBuilder::new(0);
        builder.push(ty, resolve);
        let plan = builder.into_plan();
        return Some(ResultLift {
            source: ResultSource::Compound(CompoundResult { ty: *ty, plan }),
            side_table: SideTableInfo::default(),
        });
    }

    if !result_kind_supported(kind) {
        // Variants / option / list / etc. as result types: defer
        // until those land. The wrapper still calls after-hook with
        // option::none for `result`.
        return None;
    }
    let side_table = side_table_info_for(ty, kind, resolve);
    let result_at_retptr = if is_async {
        import_sig.retptr
    } else {
        export_sig.retptr
    };
    let source = if result_at_retptr {
        ResultSource::RetptrPair(kind)
    } else {
        ResultSource::Direct(kind)
    };
    Some(ResultLift { source, side_table })
}

/// Whether the (non-compound) result lift codegen can handle this
/// kind today. Compound kinds use the `Compound` arm and don't go
/// through this check.
fn result_kind_supported(kind: LiftKind) -> bool {
    matches!(
        kind,
        LiftKind::Bool
            | LiftKind::IntegerSignExt
            | LiftKind::IntegerZeroExt
            | LiftKind::Integer64
            | LiftKind::FloatingF32
            | LiftKind::FloatingF64
            | LiftKind::Text
            | LiftKind::Bytes
            | LiftKind::Enum
    )
}

/// Build the `SideTableInfo` for a (type, kind) pair. Empty for
/// primitive lifts; populated for compound lifts that need
/// per-tree side-table entries (currently only enum).
fn side_table_info_for(ty: &Type, kind: LiftKind, resolve: &Resolve) -> SideTableInfo {
    let mut info = SideTableInfo::default();
    if matches!(kind, LiftKind::Enum) {
        info.enum_info = enum_lift_info_for_type(ty, resolve);
    }
    info
}

/// Extract `(type-name, case-names)` from an enum-typed `Type::Id`.
/// Returns `None` if the type isn't an enum or lacks a name (the
/// canonical-ABI lower has the disc but the cell can't render
/// without case-names).
fn enum_lift_info_for_type(ty: &Type, resolve: &Resolve) -> Option<NamedListInfo> {
    let Type::Id(id) = ty else {
        return None;
    };
    let typedef = &resolve.types[*id];
    let wit_parser::TypeDefKind::Enum(e) = &typedef.kind else {
        return None;
    };
    let type_name = typedef.name.as_ref()?.clone();
    let item_names: Vec<String> = e.cases.iter().map(|c| c.name.clone()).collect();
    Some(NamedListInfo {
        type_name,
        item_names,
    })
}

// ─── Side-table population ────────────────────────────────────────
//
// All side-table kinds (enum / flags / variant / record) share the
// same shape and lifecycle:
//   1. Walk every (fn, param | result); for each lift carrying an
//      info of this kind, dedup-register the strings (type-name +
//      item-names) into the shared name_blob.
//   2. Lay out one entry record per item in declaration order, into
//      one contiguous side-table data segment.
//   3. Hand back per-(fn, param) and per-(fn, result) [`SymRef`]
//      pointers tagged with the segment's [`SymbolId`]; the layout
//      phase resolves them to absolute [`BlobSlice`]s after every
//      segment has a base.
//
// The kind-specific bits (where to find the info on `SideTableInfo`,
// which `RecordLayout` to use, what the item-name field is called)
// are passed in via [`SideTableSpec`] + an extractor closure.

/// Per-side-table-kind configuration. Plug-in points for adding a
/// new kind: provide the `RecordLayout` for one entry record + the
/// item-name field name, and pass an extractor closure that pulls
/// this kind's info off `SideTableInfo`.
pub(super) struct SideTableSpec<'a> {
    /// Layout of one entry record (e.g. `splicer:common/types.enum-info`).
    pub entry_layout: &'a RecordLayout,
    /// Field name on the entry record for the per-item identifier
    /// (e.g. `"case-name"` for enum-info, `"flag-name"` for flags-info).
    pub item_name_field: &'static str,
}

/// Where each registered type's strings live in the name blob.
/// Keyed by type-name to dedupe across multiple uses of the same
/// type across params / results / functions.
pub(super) type StringTable = HashMap<String, NamedListStrings>;

pub(super) struct NamedListStrings {
    type_name: BlobSlice,
    items: Vec<BlobSlice>, // per item, in declaration order
}

/// Output of [`build_side_table_blob`]: the entry-record [`Segment`]
/// plus per-(fn, param) and per-(fn, result) [`SymRef`]s into it.
/// Resolution to absolute [`BlobSlice`]s happens once the segment's
/// base is known.
pub(super) struct SideTableBlob {
    pub segment: Segment,
    pub per_param: Vec<Vec<SymRef>>,
    pub per_result: Vec<SymRef>,
}

/// Walk every param / result; for each lift that surfaces a
/// [`NamedListInfo`] of this kind, append its strings to `name_blob`
/// (deduped per type-name). Returns the per-type string offsets so
/// the side-table builder can stitch entries together without
/// re-scanning `name_blob`.
///
/// `from_plan` extracts the kind's infos from a per-param
/// [`LiftPlan`] (multiple infos possible if the plan has multiple
/// nominal cells of this kind). `from_result` reads the kind's
/// info off the result's [`SideTableInfo`] (single info, since
/// results today are single-cell).
pub(super) fn register_side_table_strings(
    per_func: &[FuncClassified],
    name_blob: &mut Vec<u8>,
    from_plan: impl Fn(&LiftPlan) -> Vec<&NamedListInfo>,
    from_result: impl Fn(&SideTableInfo) -> Option<&NamedListInfo>,
) -> StringTable {
    let mut table = StringTable::new();
    for fd in per_func {
        for p in &fd.params {
            for info in from_plan(&p.plan) {
                ensure_registered(&mut table, name_blob, info);
            }
        }
        if let Some(rl) = &fd.result_lift {
            if let Some(info) = from_result(&rl.side_table) {
                ensure_registered(&mut table, name_blob, info);
            }
        }
    }
    table
}

fn ensure_registered(table: &mut StringTable, name_blob: &mut Vec<u8>, info: &NamedListInfo) {
    if table.contains_key(&info.type_name) {
        return;
    }
    let type_name = append_string(name_blob, &info.type_name);
    let items = info
        .item_names
        .iter()
        .map(|n| append_string(name_blob, n))
        .collect();
    table.insert(
        info.type_name.clone(),
        NamedListStrings { type_name, items },
    );
}

fn append_string(name_blob: &mut Vec<u8>, s: &str) -> BlobSlice {
    let off = name_blob.len() as u32;
    name_blob.extend_from_slice(s.as_bytes());
    BlobSlice {
        off,
        len: s.len() as u32,
    }
}

/// Lay out one per-case-kind side table. For per-case kinds (enum,
/// variant) the side-table index is the runtime disc, so entries
/// are laid out one-per-case in WIT declaration order. The cell at
/// runtime points at the contiguous per-(param|result) range via
/// `(blob_off, len)`.
///
/// `from_plan` returns the (at most one for enum) info for a param's
/// plan that contributes to this side table. (When records of enums
/// land, this may yield multiple infos per plan — the builder
/// handles that by appending one contiguous range per plan-cell.)
pub(super) fn build_side_table_blob(
    per_func: &[FuncClassified],
    strings: &StringTable,
    spec: &SideTableSpec<'_>,
    segment_id: SymbolId,
    from_plan: impl Fn(&LiftPlan) -> Option<&NamedListInfo>,
    from_result: impl Fn(&SideTableInfo) -> Option<&NamedListInfo>,
) -> SideTableBlob {
    let mut bytes: Vec<u8> = Vec::new();
    let mut per_param: Vec<Vec<SymRef>> = Vec::with_capacity(per_func.len());
    let mut per_result: Vec<SymRef> = Vec::with_capacity(per_func.len());
    for fd in per_func {
        let mut params = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            params.push(append_entries(
                &mut bytes,
                strings,
                spec,
                segment_id,
                from_plan(&p.plan),
            ));
        }
        per_param.push(params);
        let result_info = fd
            .result_lift
            .as_ref()
            .and_then(|r| from_result(&r.side_table));
        per_result.push(append_entries(
            &mut bytes,
            strings,
            spec,
            segment_id,
            result_info,
        ));
    }
    SideTableBlob {
        segment: Segment {
            id: segment_id,
            align: spec.entry_layout.align,
            bytes,
            relocs: Vec::new(),
        },
        per_param,
        per_result,
    }
}

fn append_entries(
    blob: &mut Vec<u8>,
    strings: &StringTable,
    spec: &SideTableSpec<'_>,
    segment_id: SymbolId,
    info: Option<&NamedListInfo>,
) -> SymRef {
    let Some(info) = info else {
        return SymRef::EMPTY;
    };
    let s = strings
        .get(&info.type_name)
        .expect("register_side_table_strings ran for every info");
    let blob_off = blob.len() as u32;
    let len = info.item_names.len() as u32;
    for item_idx in 0..info.item_names.len() {
        let entry = RecordWriter::extend_zero(blob, spec.entry_layout);
        entry.write_slice(blob, INFO_TYPE_NAME, s.type_name);
        entry.write_slice(blob, spec.item_name_field, s.items[item_idx]);
    }
    SymRef {
        target: segment_id,
        off: blob_off,
        len,
    }
}

// ─── Convenience facades for emit.rs (one per side-table kind) ────

/// Register enum-info strings for every enum-typed lift across all
/// funcs. Thin wrapper over [`register_side_table_strings`] tied to
/// `CellOp::EnumCase`.
pub(super) fn register_enum_strings(
    per_func: &[FuncClassified],
    name_blob: &mut Vec<u8>,
) -> StringTable {
    register_side_table_strings(
        per_func,
        name_blob,
        |plan| plan.enum_infos().collect(),
        |st| st.enum_info.as_ref(),
    )
}

/// Build the enum-info side-table blob. Thin wrapper over
/// [`build_side_table_blob`] tied to the `enum-info` record + the
/// `enum_info` field on [`SideTableInfo`].
///
/// Today's plans have at most one `EnumCase` per param (enum is a
/// primitive at the param level — only nested-in-record enums could
/// produce multiple, and that's not yet supported). When that lands,
/// the per-plan extractor will need to surface multiple infos per
/// plan, with the side-table builder appending contiguous ranges
/// per plan-cell.
pub(super) fn build_enum_info_blob(
    per_func: &[FuncClassified],
    strings: &StringTable,
    schema: &SchemaLayouts,
    segment_id: SymbolId,
) -> SideTableBlob {
    build_side_table_blob(
        per_func,
        strings,
        &SideTableSpec {
            entry_layout: &schema.enum_info_layout,
            item_name_field: ENUM_INFO_CASE_NAME,
        },
        segment_id,
        |plan| plan.enum_infos().next(),
        |st| st.enum_info.as_ref(),
    )
}

// ─── Record-info side table ───────────────────────────────────────
//
// Different shape from enum-info: enum-info's side table has one
// entry per case (laid out per-type, indexed by runtime disc).
// record-info's side table has one entry per *record cell instance*
// (laid out per-(fn, param), indexed by an adapter-build-time-known
// constant). Each entry's `fields: list<tuple<string, u32>>` lives
// in a separate tuples blob; the record-info entry stores a slice
// pointer into it. Two segments to place, two layers of pointer
// patching.

/// Per-record-type strings registered in the shared `name_blob`.
/// Field-name strings dedupe per record type — two params of the
/// same record type reuse the strings. Cross-type collisions (e.g.
/// `"name"` appearing in `person` and `pet`) currently get
/// registered twice; promote to global string-dedup if it shows up
/// in profiling.
pub(super) struct RecordTypeStrings {
    pub type_name: BlobSlice,
    /// Per field, in WIT declaration order.
    pub field_names: Vec<BlobSlice>,
}

pub(super) type RecordStringTable = HashMap<String, RecordTypeStrings>;

/// Walk every plan's [`CellOp::RecordOf`] (params + compound results);
/// for each record type seen, register its `type-name` + each
/// `field-name` into `name_blob` (deduped per type-name). Result keyed
/// by record type-name.
pub(super) fn register_record_strings(
    per_func: &[FuncClassified],
    name_blob: &mut Vec<u8>,
) -> RecordStringTable {
    let mut table = RecordStringTable::new();
    let register_plan =
        |plan: &LiftPlan, name_blob: &mut Vec<u8>, table: &mut RecordStringTable| {
            for (type_name, fields) in plan.record_ofs() {
                if !table.contains_key(type_name) {
                    let tn = append_string(name_blob, type_name);
                    let fns = fields
                        .iter()
                        .map(|(name, _)| append_string(name_blob, name))
                        .collect();
                    table.insert(
                        type_name.to_string(),
                        RecordTypeStrings {
                            type_name: tn,
                            field_names: fns,
                        },
                    );
                }
            }
        };
    for fd in per_func {
        for p in &fd.params {
            register_plan(&p.plan, name_blob, &mut table);
        }
        if let Some(c) = fd.result_lift.as_ref().and_then(|rl| rl.compound()) {
            register_plan(&c.plan, name_blob, &mut table);
        }
    }
    table
}

/// Output of [`build_record_info_blob`]. Two [`Segment`]s — the
/// `entries` segment carries one [`Reloc`] per record-cell, pointing
/// each entry's `fields.ptr` at the matching range inside the
/// `tuples` segment. Per-(fn, param) range pointers are [`SymRef`]s
/// into `entries`; the layout phase resolves both layers in one
/// reloc-pass once each segment has a base.
pub(super) struct RecordInfoBlobs {
    /// `record-info` entries: one entry per `CellOp::RecordOf` across
    /// all plans, laid out per-(fn, param) in plan order. Carries
    /// relocs for each entry's `fields.ptr` → tuples segment.
    pub entries: Segment,
    /// `(name, cell-idx)` tuples arena, referenced from each entry's
    /// `fields: list<tuple<string, u32>>`.
    pub tuples: Segment,
    /// Per (fn, param): the param's contiguous record-info range,
    /// targeting the entries segment.
    pub per_param_range: Vec<Vec<SymRef>>,
    /// Per (fn, param): for each plan cell, its assigned record-info
    /// side-table index (None for non-`RecordOf` cells). The lift
    /// codegen reads this when emitting `cell::record-of(idx)`.
    pub per_param_cell_idx: Vec<Vec<Vec<Option<u32>>>>,
    /// Per (fn): result-side range. `SymRef::EMPTY` for void /
    /// non-Compound results; populated for `Compound` results so the
    /// result tree's `record-infos` slot can patch in.
    pub per_result_range: Vec<SymRef>,
    /// Per (fn): for each cell of the result's plan, its assigned
    /// record-info side-table index (None for non-`RecordOf` cells).
    /// Empty Vec for non-Compound results.
    pub per_result_cell_idx: Vec<Vec<Option<u32>>>,
}

/// Lay out the per-(fn, param) and per-(fn, compound-result) record-
/// info entries + their (name, cell-idx) tuples arena. Each
/// `CellOp::RecordOf` in a plan contributes one entry; the entry's
/// side-table index is its position in that plan's contiguous range.
pub(super) fn build_record_info_blob(
    per_func: &[FuncClassified],
    strings: &RecordStringTable,
    schema: &SchemaLayouts,
    entries_id: SymbolId,
    tuples_id: SymbolId,
) -> RecordInfoBlobs {
    let entry_layout = &schema.record_info_layout;
    let tuple_layout = &schema.record_field_tuple_layout;

    let mut entries: Vec<u8> = Vec::new();
    let mut tuples: Vec<u8> = Vec::new();
    let mut entry_relocs: Vec<Reloc> = Vec::new();
    let mut per_param_range: Vec<Vec<SymRef>> = Vec::with_capacity(per_func.len());
    let mut per_param_cell_idx: Vec<Vec<Vec<Option<u32>>>> = Vec::with_capacity(per_func.len());
    let mut per_result_range: Vec<SymRef> = Vec::with_capacity(per_func.len());
    let mut per_result_cell_idx: Vec<Vec<Option<u32>>> = Vec::with_capacity(per_func.len());

    /// Append entries for one plan's `CellOp::RecordOf` cells; returns
    /// the contiguous range [`SymRef`] (into the entries segment) +
    /// the per-cell side-table index map. Each entry's `fields.ptr`
    /// slot gets a [`Reloc`] into the tuples segment.
    fn append_plan(
        plan: &LiftPlan,
        strings: &RecordStringTable,
        entries: &mut Vec<u8>,
        tuples: &mut Vec<u8>,
        entry_relocs: &mut Vec<Reloc>,
        entry_layout: &RecordLayout,
        tuple_layout: &RecordLayout,
        entries_id: SymbolId,
        tuples_id: SymbolId,
    ) -> (SymRef, Vec<Option<u32>>) {
        let range_start = entries.len() as u32;
        let mut count: u32 = 0;
        let mut cell_idx_map: Vec<Option<u32>> = vec![None; plan.cells.len()];
        for (cell_pos, op) in plan.cells.iter().enumerate() {
            let CellOp::RecordOf { type_name, fields } = op else {
                continue;
            };
            let s = strings
                .get(type_name.as_str())
                .expect("register_record_strings registered every record type");
            let side_idx = count;
            cell_idx_map[cell_pos] = Some(side_idx);
            count += 1;

            let tuples_off = tuples.len() as u32;
            let tuples_len = fields.len() as u32;
            for (i, (_, child_cell_idx)) in fields.iter().enumerate() {
                let tuple = RecordWriter::extend_zero(tuples, tuple_layout);
                tuple.write_slice(tuples, RECORD_FIELD_TUPLE_NAME, s.field_names[i]);
                tuple.write_i32(tuples, RECORD_FIELD_TUPLE_IDX, *child_cell_idx as i32);
            }

            let entry = RecordWriter::extend_zero(entries, entry_layout);
            entry.write_slice(entries, INFO_TYPE_NAME, s.type_name);
            entry.write_slice_reloc(
                entries,
                entry_relocs,
                RECORD_INFO_FIELDS,
                SymRef {
                    target: tuples_id,
                    off: tuples_off,
                    len: tuples_len,
                },
            );
        }
        (
            SymRef {
                target: entries_id,
                off: range_start,
                len: count,
            },
            cell_idx_map,
        )
    }

    for fd in per_func {
        let mut params_ranges = Vec::with_capacity(fd.params.len());
        let mut params_cell_idx = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            let (range, cell_idx_map) = append_plan(
                &p.plan,
                strings,
                &mut entries,
                &mut tuples,
                &mut entry_relocs,
                entry_layout,
                tuple_layout,
                entries_id,
                tuples_id,
            );
            params_ranges.push(range);
            params_cell_idx.push(cell_idx_map);
        }
        per_param_range.push(params_ranges);
        per_param_cell_idx.push(params_cell_idx);

        let (result_range, result_cell_idx_map) =
            match fd.result_lift.as_ref().and_then(|rl| rl.compound()) {
                Some(c) => append_plan(
                    &c.plan,
                    strings,
                    &mut entries,
                    &mut tuples,
                    &mut entry_relocs,
                    entry_layout,
                    tuple_layout,
                    entries_id,
                    tuples_id,
                ),
                None => (SymRef::EMPTY, Vec::new()),
            };
        per_result_range.push(result_range);
        per_result_cell_idx.push(result_cell_idx_map);
    }

    RecordInfoBlobs {
        entries: Segment {
            id: entries_id,
            align: entry_layout.align,
            bytes: entries,
            relocs: entry_relocs,
        },
        tuples: Segment {
            id: tuples_id,
            align: tuple_layout.align,
            bytes: tuples,
            relocs: Vec::new(),
        },
        per_param_range,
        per_param_cell_idx,
        per_result_range,
        per_result_cell_idx,
    }
}

// ─── Wrapper-body locals + lift codegen ───────────────────────────

/// Locals used by the wrapper body. Allocated once up front so all
/// downstream emit phases (param lift, hook calls, result lift, async
/// task.return load) reference the same indices. Result-lift-only
/// locals (Compound addr + synth slot locals, plus the pre-built
/// `lift_from_memory` instruction sequence) live on
/// [`ResultEmitPlan`] instead — that type bundles the result emit's
/// per-variant data so the four-fields-must-agree invariant disappears
/// into the sum-type discriminant.
pub(super) struct WrapperLocals {
    /// Scratch for the cell write address.
    pub addr: u32,
    /// Packed status from canon-async hook calls.
    pub st: u32,
    /// Waitable-set handle for the wait loop.
    pub ws: u32,
    /// i64 widening source for IntegerSignExt/ZeroExt.
    pub ext64: u32,
    /// f64 promoted source for FloatingF32.
    pub ext_f64: u32,
    /// Direct-return value when the export sig has a single flat
    /// result; `None` otherwise.
    pub result: Option<u32>,
    /// Address local that drives `lift_from_memory` for async
    /// `task.return` flat loads. `None` for sync, void async, and
    /// async with retptr-passthrough task.return.
    pub tr_addr: Option<u32>,
}

/// Per-function emit-time bundle for the result-side lift. Built once
/// in [`alloc_wrapper_locals`] from the layout-phase
/// [`ResultLayout`] + the locals just allocated, then consumed by
/// [`emit_wrapper_function`]'s phase-3 result-lift block via a single
/// pattern match. Replaces the prior pile of parallel `Option`-shaped
/// fields whose Some-ness had to agree by hand. Borrows the
/// layout-time per-cell record-info index map from the owning
/// [`FuncDispatch`].
pub(super) enum ResultEmitPlan<'a> {
    /// Void function or unsupported result kind: no lift fires.
    None,
    /// Direct primitive return — source value already in
    /// `source_local` (captured from the handler's flat return after
    /// the call).
    Direct { kind: LiftKind, source_local: u32 },
    /// `(ptr, len)` pair lives at `retptr_offset` in static scratch.
    /// The wrapper loads the pair into `ptr_local` / `len_local`
    /// before lifting (today these are always `lcl.ptr_scratch` /
    /// `lcl.len_scratch` — the variant carries them so the consumer
    /// doesn't re-thread `&WrapperLocals` for that lookup).
    RetptrPair {
        kind: LiftKind,
        retptr_offset: i32,
        ptr_local: u32,
        len_local: u32,
    },
    /// Compound result: emit-time cell plan + layout offsets +
    /// emit-time locals/loads. `addr_local` drives the
    /// `lift_from_memory`-built `loads` sequence (which pushes
    /// canonical-ABI flat values onto the wasm stack); the wrapper
    /// then `local.set`s those into `synth_locals` (in reverse) for
    /// the plan walker. The owned `plan` is rebuilt here with absolute
    /// wasm-local indices (= `synth_locals[i]`) baked into each cell —
    /// the classify-time [`CompoundResult::plan`] only carries
    /// placeholder indices and is consumed by the side-table builders.
    /// `record_info_cell_idx` is the layout-phase per-cell `RecordOf`
    /// side-table index map (borrowed off [`ResultSourceLayout::Compound`]).
    Compound {
        plan: LiftPlan,
        retptr_offset: i32,
        addr_local: u32,
        synth_locals: Vec<u32>,
        loads: Vec<Instruction<'static>>,
        record_info_cell_idx: &'a [Option<u32>],
    },
}

pub(super) fn alloc_wrapper_locals<'a>(
    resolve: &Resolve,
    size_align: &SizeAlign,
    locals: &mut FunctionIndices,
    fd: &'a FuncDispatch,
) -> (WrapperLocals, ResultEmitPlan<'a>) {
    let addr = locals.alloc_local(ValType::I32);
    let st = locals.alloc_local(ValType::I32);
    let ws = locals.alloc_local(ValType::I32);
    let ptr_scratch = locals.alloc_local(ValType::I32);
    let len_scratch = locals.alloc_local(ValType::I32);
    let ext64 = locals.alloc_local(ValType::I64);
    let ext_f64 = locals.alloc_local(ValType::F64);
    let result = direct_return_type(&fd.export_sig).map(|t| locals.alloc_local(t));
    // Async with a non-retptr-passthrough task.return needs an
    // i32 addr local so `lift_from_memory` can flat-load result
    // values out of the retptr scratch.
    let tr_uses_flat_loads = fd
        .shape
        .task_return()
        .is_some_and(|tr| !tr.sig.indirect_params && fd.result_ty.is_some());
    let tr_addr = tr_uses_flat_loads.then(|| locals.alloc_local(ValType::I32));

    // Result-emit plan: discriminate on the layout-phase `ResultLayout`
    // and pull together the variant-specific locals/offsets/loads.
    // Compound allocates extra locals (one i32 addr + one synth per
    // flat slot) AND drives the bindgen for `lift_from_memory` —
    // bindgen may allocate further scratch locals, so this must run
    // before the wrapper-body emit freezes the locals list.
    let result_emit = match fd.result_lift.as_ref() {
        None => ResultEmitPlan::None,
        Some(rl) => match &rl.source {
            ResultSourceLayout::Direct(kind) => ResultEmitPlan::Direct {
                kind: *kind,
                source_local: result
                    .expect("ResultSourceLayout::Direct → direct-return local allocated"),
            },
            ResultSourceLayout::RetptrPair {
                kind,
                retptr_offset,
            } => ResultEmitPlan::RetptrPair {
                kind: *kind,
                retptr_offset: *retptr_offset,
                ptr_local: ptr_scratch,
                len_local: len_scratch,
            },
            ResultSourceLayout::Compound {
                compound,
                retptr_offset,
                record_info_cell_idx,
            } => {
                let addr_local = locals.alloc_local(ValType::I32);
                let flat = super::super::abi::flat_types(resolve, &compound.ty, None).expect(
                    "Compound result must flatten within MAX_FLAT_PARAMS — \
                     classify_result_lift only returns Compound for kinds that do",
                );
                debug_assert_eq!(
                    flat.len(),
                    compound.plan.flat_slot_count as usize,
                    "canonical-ABI flat count must match classify-time plan"
                );
                let synth_locals: Vec<u32> = flat
                    .into_iter()
                    .map(|wt| locals.alloc_local(wasm_type_to_val(wt)))
                    .collect();
                // Rebuild the cell plan with absolute wasm-local
                // indices baked in: `local_base = synth_locals[0]`
                // means cell N's local = synth_locals[0] + N, which
                // matches our contiguous synth-local allocation.
                let synth_base = synth_locals[0];
                let mut builder = LiftPlanBuilder::new(synth_base);
                builder.push(&compound.ty, resolve);
                let plan = builder.into_plan();
                debug_assert_eq!(
                    plan.cells.len(),
                    compound.plan.cells.len(),
                    "rebuilt emit-time plan must have same cell count as classify-time plan"
                );
                let mut bindgen = WasmEncoderBindgen::new(size_align, addr_local, locals);
                lift_from_memory(resolve, &mut bindgen, (), &compound.ty);
                let loads = bindgen.into_instructions();
                ResultEmitPlan::Compound {
                    plan,
                    retptr_offset: *retptr_offset,
                    addr_local,
                    synth_locals,
                    loads,
                    record_info_cell_idx,
                }
            }
        },
    };

    (
        WrapperLocals {
            addr,
            st,
            ws,
            ext64,
            ext_f64,
            result,
            tr_addr,
        },
        result_emit,
    )
}

/// Emit the wasm that lifts one plan into its cells slab. Walks
/// `plan.cells` in allocation order and, for each cell, sets
/// `lcl.addr` to that cell's absolute address (`cells_offset + i *
/// cell_size`) and dispatches on the cell's variant. The cells
/// already carry their absolute wasm-local indices; no further base
/// resolution happens here.
pub(super) fn emit_lift_plan(
    f: &mut Function,
    cell_layout: &CellLayout,
    cells_offset: u32,
    plan: &LiftPlan,
    record_info_indices: &[Option<u32>],
    lcl: &WrapperLocals,
) {
    debug_assert_eq!(record_info_indices.len(), plan.cells.len());
    for (cell_idx, op) in plan.cells.iter().enumerate() {
        let cell_addr = cells_offset + cell_idx as u32 * cell_layout.size;
        f.instructions().i32_const(cell_addr as i32);
        f.instructions().local_set(lcl.addr);
        emit_cell_op(f, cell_layout, op, record_info_indices[cell_idx], lcl);
    }
}

/// Emit one cell's worth of wasm at the address held in `lcl.addr`.
///
/// `record_info_idx` is the side-table index for `CellOp::RecordOf`
/// cells (set by the layout phase via the static record-info builder
/// — adapter-build-time-known, emitted as `i32.const`). Other cells
/// don't read it.
fn emit_cell_op(
    f: &mut Function,
    cell_layout: &CellLayout,
    op: &CellOp,
    record_info_idx: Option<u32>,
    lcl: &WrapperLocals,
) {
    let addr = lcl.addr;
    match op {
        CellOp::Bool { local } => cell_layout.emit_bool(f, addr, *local),
        CellOp::IntegerSignExt { local } => {
            f.instructions().local_get(*local);
            f.instructions().i64_extend_i32_s();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        CellOp::IntegerZeroExt { local } => {
            f.instructions().local_get(*local);
            f.instructions().i64_extend_i32_u();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        CellOp::Integer64 { local } => cell_layout.emit_integer(f, addr, *local),
        CellOp::FloatingF32 { local } => {
            f.instructions().local_get(*local);
            f.instructions().f64_promote_f32();
            f.instructions().local_set(lcl.ext_f64);
            cell_layout.emit_floating(f, addr, lcl.ext_f64);
        }
        CellOp::FloatingF64 { local } => cell_layout.emit_floating(f, addr, *local),
        CellOp::Text { ptr, len } => {
            cell_layout.emit_text(f, addr, *ptr, *len);
        }
        CellOp::Bytes { ptr, len } => {
            cell_layout.emit_bytes(f, addr, *ptr, *len);
        }
        CellOp::EnumCase { local, .. } => {
            cell_layout.emit_enum_case(f, addr, *local);
        }
        CellOp::RecordOf { .. } => {
            let idx = record_info_idx
                .expect("record-info index missing — layout phase didn't backfill RecordOf cell");
            cell_layout.emit_record_of(f, addr, idx);
        }
    }
}

/// Shared lift body for direct-return result values. `slot0` /
/// `slot1` are wasm locals carrying the source value(s); for single-
/// slot kinds only `slot0` is used. Multi-slot kinds (Text/Bytes)
/// expect `(ptr, len)` in (slot0, slot1).
fn emit_lift_kind(
    f: &mut Function,
    cell_layout: &CellLayout,
    kind: LiftKind,
    slot0: u32,
    slot1: u32,
    lcl: &WrapperLocals,
) {
    let addr = lcl.addr;
    match kind {
        LiftKind::Bool => cell_layout.emit_bool(f, addr, slot0),
        LiftKind::IntegerSignExt => {
            f.instructions().local_get(slot0);
            f.instructions().i64_extend_i32_s();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        LiftKind::IntegerZeroExt => {
            f.instructions().local_get(slot0);
            f.instructions().i64_extend_i32_u();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        LiftKind::Integer64 => cell_layout.emit_integer(f, addr, slot0),
        LiftKind::FloatingF32 => {
            f.instructions().local_get(slot0);
            f.instructions().f64_promote_f32();
            f.instructions().local_set(lcl.ext_f64);
            cell_layout.emit_floating(f, addr, lcl.ext_f64);
        }
        LiftKind::FloatingF64 => cell_layout.emit_floating(f, addr, slot0),
        LiftKind::Text => cell_layout.emit_text(f, addr, slot0, slot1),
        LiftKind::Bytes => cell_layout.emit_bytes(f, addr, slot0, slot1),
        LiftKind::Enum => cell_layout.emit_enum_case(f, addr, slot0),
        // Compound result lifts aren't yet supported; classify_result_lift
        // returns None for these so we never reach here at emit time.
        kind => unreachable!(
            "emit_lift_kind reached unsupported result kind {kind:?} — \
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
pub(super) fn emit_lift_result(
    f: &mut Function,
    cell_layout: &CellLayout,
    plan: &ResultEmitPlan<'_>,
    lcl: &WrapperLocals,
) {
    match plan {
        ResultEmitPlan::Direct { kind, source_local } => {
            emit_lift_kind(f, cell_layout, *kind, *source_local, *source_local, lcl);
        }
        ResultEmitPlan::RetptrPair {
            kind,
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
            emit_lift_kind(f, cell_layout, *kind, *ptr_local, *len_local, lcl);
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
/// order, ready for [`emit_lift_plan`] to walk the cell plan whose
/// cells already reference these absolute synth-local indices.
pub(super) fn emit_lift_compound_prefix(
    f: &mut Function,
    plan_flat_slot_count: u32,
    retptr_offset: i32,
    loads: &[Instruction<'static>],
    addr_local: u32,
    synth_locals: &[u32],
) {
    debug_assert_eq!(
        synth_locals.len(),
        plan_flat_slot_count as usize,
        "synthetic-local count must match plan flat slot count"
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
