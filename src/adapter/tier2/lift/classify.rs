//! Classify-phase per-(param | result) lift descriptors. Builds a
//! `LiftPlan` per param/result; the layout phase wraps these into
//! `ParamLayout` / `ResultLayout` with offsets filled in.

use anyhow::Result;
use wit_parser::{Function as WitFunction, Resolve, Type};

use super::super::super::abi::emit::BlobSlice;
use super::super::blob::NameInterner;
use super::plan::{Cell, LiftPlan, MapAliases};
use super::sidetable::CellSideData;

// ─── Result-lift descriptors (classify-time, immutable) ───────────

/// Classify-time descriptor for the function's return value.
pub(crate) struct ResultLift {
    pub source: ResultSource,
}

pub(crate) enum ResultSource {
    /// Sync flat return — value lands in `lcl.result`. The cell's
    /// `flat_slot` is a placeholder (source is `lcl.result`).
    Direct(Cell),
    /// Result loaded from retptr scratch via `lift_from_memory`.
    /// Covers multi-cell compounds and single-cell-at-retptr kinds.
    Compound(CompoundResult),
}

/// Compound-result classify output. Offsets and per-cell side-table
/// data live on `ResultSourceLayout::Compound` (layout phase).
pub(crate) struct CompoundResult {
    /// WIT type of the result — drives `lift_from_memory`.
    pub ty: Type,
    pub plan: LiftPlan,
}

impl ResultLift {
    pub(crate) fn compound(&self) -> Option<&CompoundResult> {
        match &self.source {
            ResultSource::Compound(c) => Some(c),
            _ => None,
        }
    }
}

/// Classify-time per-parameter lift recipe. Offsets and per-cell
/// side-table data live on the post-layout `ParamLayout`.
pub(crate) struct ParamLift {
    pub name: BlobSlice,
    pub plan: LiftPlan,
}

// ─── Layout-phase outputs (immutable, includes offsets) ───────────

/// Per-kind outer-cell counts (excluding list-element cells, which
/// fold in at runtime via the per-list bump). Sizes static buffers
/// and seeds the per-list-pre-pass.
#[derive(Clone, Copy, Default)]
pub(crate) struct InfoCounts {
    pub handle: u32,
    pub flags: u32,
    pub record: u32,
    pub variant: u32,
}

/// Post-layout per-parameter lift descriptor. Cells slab is
/// `cabi_realloc`'d per-call (no static offset).
pub(crate) struct ParamLayout {
    pub lift: ParamLift,
    /// One entry per `lift.plan.cells` position.
    pub cell_side: Vec<CellSideData>,
    pub info_counts: InfoCounts,
}

/// Post-layout per-result lift descriptor.
pub(crate) struct ResultLayout {
    pub source: ResultSourceLayout,
    /// Direct contributes at most 1 to `handle` or `flags` (never
    /// `record` / `variant` — those always retptr).
    pub info_counts: InfoCounts,
}

pub(crate) enum ResultSourceLayout {
    /// Sync flat return; source is `lcl.result`.
    Direct { cell: Cell, side_data: CellSideData },
    /// Retptr-loaded; both multi-cell compounds and single-cell-at-
    /// retptr kinds route here.
    Compound {
        compound: CompoundResult,
        retptr_offset: i32,
        /// One entry per `compound.plan.cells` position.
        cell_side: Vec<CellSideData>,
    },
}

// ─── Classifiers ──────────────────────────────────────────────────

/// Build a `LiftPlan` for every WIT param of `func`.
pub(crate) fn classify_func_params(
    resolve: &Resolve,
    func: &WitFunction,
    names: &mut NameInterner,
    map_aliases: &MapAliases,
) -> Result<Vec<ParamLift>> {
    let mut params_lift: Vec<ParamLift> = Vec::with_capacity(func.params.len());
    for param in &func.params {
        let name = names.intern(&param.name);
        params_lift.push(ParamLift {
            name,
            plan: LiftPlan::for_type(&param.ty, resolve, names, map_aliases)?,
        });
    }
    Ok(params_lift)
}

/// Classify the function's return value. `result_at_retptr` picks
/// the deciding sig (export for sync, import for async — async always
/// retptr's non-void). Returns `None` for void or unsupported.
pub(crate) fn classify_result_lift(
    resolve: &Resolve,
    func: &WitFunction,
    result_at_retptr: bool,
    names: &mut NameInterner,
    map_aliases: &MapAliases,
) -> Result<Option<ResultLift>> {
    let Some(ty) = func.result.as_ref() else {
        return Ok(None);
    };

    // Retptr gate skips single-flat-slot compounds (e.g. `tuple<u32>`):
    // they return flat with no retptr scratch and fall through.
    if result_at_retptr && is_supported_result(ty, resolve) {
        let plan = LiftPlan::for_type(ty, resolve, names, map_aliases)?;
        return Ok(Some(ResultLift {
            source: ResultSource::Compound(CompoundResult { ty: *ty, plan }),
        }));
    }

    let Some(cell) = single_cell_for_result(ty, resolve, names, map_aliases)? else {
        return Ok(None);
    };
    Ok(Some(ResultLift {
        source: ResultSource::Direct(cell),
    }))
}

/// Whether `ty`'s result-side codegen is wired.
fn is_supported_result(ty: &Type, resolve: &Resolve) -> bool {
    is_compound_result(ty, resolve) || is_supported_direct_result(ty, resolve)
}

/// Compound kinds wired today: `record`, `tuple`, `option`, `result`,
/// `variant`, `list<T>` non-u8 (`list<u8>` takes the bytes Direct path),
/// `map<K, V>` (lift desugars to `list<tuple<K, V>>`),
/// `list<T, N>` (lift desugars to `Cell::TupleOf` with N children).
fn is_compound_result(ty: &Type, resolve: &Resolve) -> bool {
    let Type::Id(id) = ty else {
        return false;
    };
    match &resolve.types[*id].kind {
        wit_parser::TypeDefKind::Record(_)
        | wit_parser::TypeDefKind::Tuple(_)
        | wit_parser::TypeDefKind::Option(_)
        | wit_parser::TypeDefKind::Result(_)
        | wit_parser::TypeDefKind::Variant(_)
        | wit_parser::TypeDefKind::Map(_, _)
        | wit_parser::TypeDefKind::FixedLengthList(_, _) => true,
        wit_parser::TypeDefKind::List(elem) => !matches!(elem, Type::U8),
        wit_parser::TypeDefKind::Type(t) => is_compound_result(t, resolve),
        _ => false,
    }
}

/// Build a single-cell `Cell` for a Direct result; `None` if un-wired.
fn single_cell_for_result(
    ty: &Type,
    resolve: &Resolve,
    names: &mut NameInterner,
    map_aliases: &MapAliases,
) -> Result<Option<Cell>> {
    if !is_supported_direct_result(ty, resolve) {
        return Ok(None);
    }
    let plan = LiftPlan::for_type(ty, resolve, names, map_aliases)?;
    Ok(Some(
        plan.cells.into_iter().next().expect("push appended a cell"),
    ))
}

/// Whitelist of single-cell result types whose lift codegen is wired.
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
        | Type::Char
        | Type::ErrorContext => true,
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
