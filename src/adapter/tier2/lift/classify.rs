//! Classify-phase per-(param | result) lift descriptors.
//!
//! For each WIT param / result, build a [`LiftPlan`] (see
//! [`super::plan`]) plus the side-table-info needed to populate
//! per-tree side tables. The layout phase wraps these into
//! [`ParamLayout`] / [`ResultLayout`] once cells-slab + retptr-scratch
//! offsets are known.
//!
//! Params and results share one classify pattern:
//! [`LiftPlan::for_type`] over the param/result `Type`, wrapped in a
//! per-side struct ([`ParamLift`] / [`CompoundResult`]). All flat-slot
//! positions are plan-relative; the emit phase supplies `local_base`
//! per call (cumulative cursor for params; `synth_locals[0]` for
//! compound results) so the same plan flows unchanged through
//! side-table builders and codegen.

use wit_parser::{Function as WitFunction, Resolve, Type};

use super::super::super::abi::emit::BlobSlice;
use super::super::blob::NameInterner;
use super::plan::{Cell, LiftPlan, NamedListInfo};
use super::sidetable::CellSideData;

// ─── Result-lift descriptors (classify-time, immutable) ───────────
//
// Two shapes:
//
// - `Direct(kind)` — sync flat return: a single-flat-slot value
//   captured into `lcl.result` after the handler call. No memory
//   load.
// - `Compound(CompoundResult)` — anything routed through retptr
//   scratch. Driven by a [`LiftPlan`] symmetric with the per-param
//   plans; `wit_bindgen_core::abi::lift_from_memory` pushes flat
//   values onto the wasm stack from `retptr_offset`, and the
//   wrapper `local.set`s those into per-result synthetic locals
//   (in reverse, since the stack is LIFO) for the plan walker.
//   Multi-cell compounds (record / tuple / option / result /
//   variant) and single-cell-at-retptr kinds (string / `list<u8>`
//   / async-retptr'd flags / char / handle) share this path.
//
// All offsets (retptr_offset + cells_offset + per-cell side-table
// data for Compound) live on the post-layout [`ResultLayout`] /
// [`ResultSourceLayout`]; this classify-time type has no offsets
// and never gets mutated.

/// Classify-time descriptor for the function's return value. The
/// layout phase wraps it into a [`ResultLayout`] with the offsets
/// once those are known. `side_table` populates the result tree's
/// side tables at adapter-build time. Compound results carry their
/// side-table contributions inline on the plan's `Cell`s instead.
pub(crate) struct ResultLift {
    pub source: ResultSource,
    pub(super) side_table: SideTableInfo,
}

pub(crate) enum ResultSource {
    /// Sync flat return: the value lands in `lcl.result` after the
    /// handler call — no memory load. The [`Cell`] carries the variant
    /// tag for emit dispatch; its `flat_slot` field is a placeholder
    /// (the source is `lcl.result`, not a plan slot).
    Direct(Cell),
    /// Result loaded from retptr scratch via [`LiftPlan`] +
    /// `lift_from_memory`. Covers both multi-cell compound shapes
    /// (record / tuple / option / result / variant) and single-cell
    /// kinds whose flat representation lives in memory (string /
    /// `list<u8>` / async-retptr'd flags / char / handle).
    Compound(CompoundResult),
}

/// Per-fn compound-result classify output: which WIT type to lift
/// plus a structural cell-tree plan. The retptr scratch byte offset,
/// the cells-slab byte offset, and the per-cell side-table data are
/// all layout-phase outputs — they live on
/// [`ResultSourceLayout::Compound`], not here.
///
/// `plan`'s cells carry plan-relative flat-slot positions; the emit
/// phase supplies a `local_base` (= `synth_locals[0]`) at
/// [`super::emit::emit_lift_plan`] call time. The same plan flows
/// unchanged through both the side-table builders (which read
/// structural fields only) and the emit phase.
pub(crate) struct CompoundResult {
    /// WIT type of the result value — kept around so the wrapper
    /// body can drive `lift_from_memory` through `WasmEncoderBindgen`.
    pub ty: Type,
    /// Structural cell plan with plan-relative flat slots; the emit
    /// phase adds the synth-local base when walking it.
    pub plan: LiftPlan,
}

impl ResultLift {
    /// Returns `Some(&CompoundResult)` for compound result lifts;
    /// `None` otherwise.
    pub(crate) fn compound(&self) -> Option<&CompoundResult> {
        match &self.source {
            ResultSource::Compound(c) => Some(c),
            _ => None,
        }
    }
}

/// Classify-time per-parameter lift recipe. The plan's cells carry
/// plan-relative flat-slot positions; the emit phase supplies the
/// `local_base` (cumulative slot cursor across preceding params) at
/// [`super::emit::emit_lift_plan`] call time. Cells-slab offset +
/// per-cell side-table data live on the post-layout [`ParamLayout`].
pub(crate) struct ParamLift {
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
/// data plus its cells-slab offset + per-cell side-table bookkeeping.
pub(crate) struct ParamLayout {
    pub lift: ParamLift,
    /// Byte offset of this param's contiguous cells slab within
    /// the static data segment; the slab holds `lift.plan.cell_count()`
    /// cells, each `cell_layout.size` bytes.
    pub cells_offset: u32,
    /// One [`CellSideData`] entry per `lift.plan.cells` position,
    /// holding the side-table bookkeeping the emit phase needs (idx,
    /// blob slice, runtime-fill, …) for cells whose kind has any.
    pub cell_side: Vec<CellSideData>,
}

/// Post-layout per-result lift descriptor: a sum-type `source`
/// carrying both the lift kind and any layout-derived offsets
/// per-variant. The classify-time `side_table` info isn't carried
/// here — it's consumed by the side-table builders during the
/// layout phase (which see [`super::super::FuncClassified::result_lift`]'s
/// pre-layout `side_table` directly).
pub(crate) struct ResultLayout {
    pub source: ResultSourceLayout,
}

pub(crate) enum ResultSourceLayout {
    /// Sync flat return: source is `lcl.result`. See
    /// [`ResultSource::Direct`] for the placeholder-slot convention.
    /// `side_data` carries any per-kind layout-phase bookkeeping
    /// (Flags / Char / Handle); `None` for kinds that lift purely
    /// from the source local.
    Direct { cell: Cell, side_data: CellSideData },
    /// Result loaded from retptr scratch: classify-time recipe +
    /// retptr offset + per-cell side-table data. The cells-slab base
    /// lives on [`super::super::AfterSetup::result_cells_offset`].
    /// Both multi-cell compounds and single-cell-at-retptr kinds
    /// route through here; their plan handles N≥1 cells uniformly.
    Compound {
        compound: CompoundResult,
        retptr_offset: i32,
        /// One entry per `compound.plan.cells` position. See
        /// [`ParamLayout::cell_side`].
        cell_side: Vec<CellSideData>,
    },
}

/// Result-side per-kind info. Populated when a single-cell direct
/// result needs side-table entries (enum cases / flag names).
#[derive(Default, Clone)]
pub(super) struct SideTableInfo {
    /// `Some` for enum-typed result lifts: type-name + case names in
    /// disc order.
    pub(super) enum_info: Option<NamedListInfo>,
    /// `Some` for flags-typed result lifts: type-name + flag names in
    /// declaration (= bit) order.
    pub(super) flags_info: Option<NamedListInfo>,
}

// ─── Classifiers ──────────────────────────────────────────────────

/// Build a [`LiftPlan`] for every WIT param of `func`. Each plan's
/// cells carry plan-relative flat-slot positions in
/// `0..plan.flat_slot_count`; the emit phase threads a cumulative
/// `local_base` across params to recover absolute wasm-local indices.
pub(crate) fn classify_func_params(
    resolve: &Resolve,
    func: &WitFunction,
    names: &mut NameInterner,
) -> Vec<ParamLift> {
    let mut params_lift: Vec<ParamLift> = Vec::with_capacity(func.params.len());
    for param in &func.params {
        let name = names.intern(&param.name);
        params_lift.push(ParamLift {
            name,
            plan: LiftPlan::for_type(&param.ty, resolve, names),
        });
    }
    params_lift
}

/// Classify the function's return value for on-return lift. Two
/// shapes:
///
/// - **Direct**: sync flat return — the value lands in `lcl.result`
///   after the handler call, no memory load. Single flat slot only
///   (canonical-ABI for sync flat returns).
/// - **Compound**: result loaded from retptr scratch via
///   `lift_from_memory` + a [`LiftPlan`]. Covers every retptr-routed
///   shape (multi-cell compounds + single-cell kinds whose flat
///   representation lives in memory: string / `list<u8>` / async-
///   retptr'd flags/char/handle).
///
/// `result_at_retptr` selects which sig's retptr decides where the
/// result lands: for sync funcs that's the export sig, for async the
/// import sig (canon-lower-async always retptr's non-void). Returns
/// `None` for void or unsupported result types — the wrapper still
/// calls after-hook with `result: option::none`.
pub(crate) fn classify_result_lift(
    resolve: &Resolve,
    func: &WitFunction,
    result_at_retptr: bool,
    names: &mut NameInterner,
) -> Option<ResultLift> {
    let ty = func.result.as_ref()?;

    // Retptr-routed: every wired result type drives a LiftPlan over
    // `lift_from_memory`-loaded slots. Multi-cell compounds AND
    // single-cell kinds use the same machinery; the plan handles
    // either by walking N=1+ cells. The retptr gate skips single-
    // flat-slot compounds (e.g. `tuple<u32>`, `record { a: u32 }`,
    // `result<_, _>`) — they come back flat with no retptr scratch
    // for `lift_from_memory` to read from, so they fall through to
    // the no-lift path.
    if result_at_retptr && is_supported_result(ty, resolve) {
        let plan = LiftPlan::for_type(ty, resolve, names);
        return Some(ResultLift {
            source: ResultSource::Compound(CompoundResult { ty: *ty, plan }),
            side_table: SideTableInfo::default(),
        });
    }

    // Direct (sync flat return): the value sits in `lcl.result`. Only
    // single-flat-slot kinds reach here. Returns None for un-wired
    // result types — wrapper still calls after-hook with
    // `result: option::none`.
    let cell = single_cell_for_result(ty, resolve, names)?;
    let side_table = side_table_info_for_cell(&cell);
    Some(ResultLift {
        source: ResultSource::Direct(cell),
        side_table,
    })
}

/// Whether `ty`'s result-side codegen is wired — i.e., we can build
/// a [`LiftPlan`] whose every cell is a wired variant. Union of
/// [`is_compound_result`] (multi-cell shapes) and
/// [`is_supported_direct_result`] (single-cell shapes).
fn is_supported_result(ty: &Type, resolve: &Resolve) -> bool {
    is_compound_result(ty, resolve) || is_supported_direct_result(ty, resolve)
}

/// Whether `ty` resolves (through type aliases) to a compound kind
/// whose result-side codegen is wired today: `record`, `tuple<...>`,
/// `option<T>`, `result<T, E>`, or `variant`.
fn is_compound_result(ty: &Type, resolve: &Resolve) -> bool {
    let Type::Id(id) = ty else {
        return false;
    };
    match &resolve.types[*id].kind {
        wit_parser::TypeDefKind::Record(_)
        | wit_parser::TypeDefKind::Tuple(_)
        | wit_parser::TypeDefKind::Option(_)
        | wit_parser::TypeDefKind::Result(_)
        | wit_parser::TypeDefKind::Variant(_) => true,
        wit_parser::TypeDefKind::Type(t) => is_compound_result(t, resolve),
        _ => false,
    }
}

/// Build a single-cell [`Cell`] for a Direct (sync flat) result.
/// Returns `None` for un-wired result types — the supported set
/// tracks the wired arms in [`super::emit::emit_lift_kind`]. Direct
/// kinds never produce a `RecordOf`, so `names` is just threaded
/// through for [`LiftPlan::for_type`]'s uniform signature.
fn single_cell_for_result(ty: &Type, resolve: &Resolve, names: &mut NameInterner) -> Option<Cell> {
    if !is_supported_direct_result(ty, resolve) {
        return None;
    }
    let plan = LiftPlan::for_type(ty, resolve, names);
    Some(plan.cells.into_iter().next().expect("push appended a cell"))
}

/// Whitelist of WIT types whose lift codegen [`super::emit::emit_lift_kind`]
/// can drive. Mirrors the wired primitive / text / bytes / enum / flags
/// / char / handle arms of [`LiftPlanBuilder::push`]; new direct/retptr-
/// pair kinds wire up here.
fn is_supported_direct_result(ty: &Type, resolve: &Resolve) -> bool {
    match ty {
        Type::Bool
        | Type::S8
        | Type::S16
        | Type::S32
        | Type::U8
        | Type::U16
        | Type::U32
        | Type::S64
        | Type::U64
        | Type::F32
        | Type::F64
        | Type::String
        | Type::Char => true,
        Type::ErrorContext => false,
        Type::Id(id) => match &resolve.types[*id].kind {
            wit_parser::TypeDefKind::List(Type::U8) => true,
            wit_parser::TypeDefKind::Enum(_) => true,
            wit_parser::TypeDefKind::Flags(_) => true,
            wit_parser::TypeDefKind::Handle(_) => true,
            wit_parser::TypeDefKind::Stream(_) => true,
            wit_parser::TypeDefKind::Future(_) => true,
            wit_parser::TypeDefKind::Type(t) => is_supported_direct_result(t, resolve),
            _ => false,
        },
    }
}

/// Build the `SideTableInfo` for a single-cell result. Empty for
/// primitive lifts; populated for kinds that need per-tree side-table
/// entries (today: enum, flags).
fn side_table_info_for_cell(cell: &Cell) -> SideTableInfo {
    let mut info = SideTableInfo::default();
    match cell {
        Cell::EnumCase {
            info: enum_info, ..
        } => info.enum_info = Some(enum_info.clone()),
        Cell::Flags {
            info: flags_info, ..
        } => info.flags_info = Some(flags_info.clone()),
        _ => {}
    }
    info
}
