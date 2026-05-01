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

use wasm_encoder::{Function, MemArg, ValType};
use wit_parser::abi::WasmSignature;
use wit_parser::{Function as WitFunction, Resolve, Type};

use super::super::abi::emit::{
    direct_return_type, RecordLayout, SLICE_LEN_OFFSET, SLICE_PTR_OFFSET,
};
use super::super::indices::FunctionIndices;
use super::blob::{BlobSlice, RecordWriter};
use super::cells::CellLayout;
use super::emit::{FuncDispatch, SchemaLayouts};

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
    /// Number of flat wasm slots this param consumes. Hard-coded
    /// for wired primitive kinds; `todo!()` for compound kinds
    /// because their flat-slot count depends on the inner type's
    /// canonical-ABI lowering — driving that off `wit-parser`'s flat
    /// representation lands alongside the actual lift codegen.
    pub(super) fn slot_count(self) -> u32 {
        match self {
            LiftKind::Bool
            | LiftKind::IntegerSignExt
            | LiftKind::IntegerZeroExt
            | LiftKind::Integer64
            | LiftKind::FloatingF32
            | LiftKind::FloatingF64 => 1,
            LiftKind::Text | LiftKind::Bytes => 2,
            LiftKind::Char => todo!("Phase 2-2b: char param slot_count = 1 (i32 code point)"),
            LiftKind::ListOf => todo!("Phase 2-2b: list<T> param slot_count = 2 (ptr, len)"),
            LiftKind::TupleOf => {
                todo!("Phase 2-2b: tuple param slot_count = sum of element flat-slot counts")
            }
            LiftKind::Option => {
                todo!("Phase 2-2b: option<T> param slot_count = 1 (disc) + flat(T)")
            }
            LiftKind::Result => {
                todo!(
                    "Phase 2-2b: result<T,E> param slot_count = 1 (disc) + max(flat(T), flat(E)) joined"
                )
            }
            LiftKind::Record => {
                todo!("Phase 2-2b: record param slot_count = sum of field flat-slot counts")
            }
            LiftKind::Flags => {
                todo!("Phase 2-2b: flags param slot_count = 1 (i32 unless > 32 flags, then more)")
            }
            // Enum lowers to a single i32 disc.
            LiftKind::Enum => 1,
            LiftKind::Variant => {
                todo!(
                    "Phase 2-2b: variant param slot_count = 1 (disc) + max-payload flat-slot count joined"
                )
            }
            LiftKind::Handle => todo!("Phase 2-4: handle param slot_count = 1 (i32 handle index)"),
            LiftKind::Future => todo!("Phase 2-4: future param slot_count = 1 (i32 future handle)"),
            LiftKind::Stream => todo!("Phase 2-4: stream param slot_count = 1 (i32 stream handle)"),
            LiftKind::ErrorContext => todo!("error-context param slot_count = 1 (i32)"),
        }
    }

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
// each cell bound to its source wasm locals (via `LocalRef`) and
// any nominal-cell side-table info it carries. `cells[0]` is the
// root that the field-tree's `root` field points at.
//
// Why a flat Vec instead of a nested IR: cell indices in
// nominal-cell side-table entries (e.g., a record's `fields`
// list) are just `Vec`-positions in `cells`. The same vector that
// drives codegen also drives side-table emission; child indices
// can't desync because they're a property of allocation order.
// `cells.len()` is the slab size; total flat-slot consumption is
// known by counting `LocalRef::Param(_)`s allocated. See
// [`docs/tiers/lift-codegen.md`](../../../docs/tiers/lift-codegen.md).

/// One cell to write at a known cell-array index. Each variant
/// captures the cell's runtime-disc semantics, its source wasm
/// locals (via [`LocalRef`]), and any side-table info this cell
/// contributes (e.g., enum-info / record-info entries).
pub(super) enum CellOp {
    Bool { local: LocalRef },
    IntegerSignExt { local: LocalRef },
    IntegerZeroExt { local: LocalRef },
    Integer64 { local: LocalRef },
    FloatingF32 { local: LocalRef },
    FloatingF64 { local: LocalRef },
    Text { ptr: LocalRef, len: LocalRef },
    Bytes { ptr: LocalRef, len: LocalRef },
    EnumCase {
        local: LocalRef,
        info: NamedListInfo,
    },
    RecordOf {
        type_name: String,
        /// `(field-name, child-cell-idx)` per field, in WIT order.
        /// `child-cell-idx` indexes into the same `LiftPlan::cells`.
        fields: Vec<(String, u32)>,
    },
}

/// Where a payload value comes from at emit time. Kept abstract at
/// classify time so the plan is reusable across param vs. result
/// lifts; resolved to a concrete wasm local index via
/// [`LocalRef::resolve`] at emit time.
#[derive(Clone, Copy)]
pub(super) enum LocalRef {
    /// nth flat-slot wasm local of the function's params.
    Param(u32),
    /// `lcl.ptr_scratch` — set up before the lift for retptr-loaded results.
    PtrScratch,
    /// `lcl.len_scratch` — paired with `PtrScratch`.
    LenScratch,
    /// `lcl.result` — direct primitive return captured into a local.
    Result,
}

impl LocalRef {
    /// Resolve to a concrete wasm local index. Panics for unset
    /// scratch locals (which only happens if the plan-builder picked
    /// an inappropriate `LocalRef` for the lift's source).
    pub(super) fn resolve(&self, lcl: &WrapperLocals) -> u32 {
        match self {
            LocalRef::Param(i) => *i,
            LocalRef::PtrScratch => lcl.ptr_scratch,
            LocalRef::LenScratch => lcl.len_scratch,
            LocalRef::Result => lcl
                .result
                .expect("LocalRef::Result requires a direct-return local"),
        }
    }
}

/// Plan for lifting one (param | result) into a cell tree. Cells
/// are listed in allocation order; `cells[0]` is the root that the
/// field-tree's `root` field points at. Walked top-to-bottom by the
/// emit-code phase; the side-table builder also walks `cells` to
/// pull out per-kind side-table contributions.
pub(super) struct LiftPlan {
    pub cells: Vec<CellOp>,
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
}

// ─── Lift plan builder ────────────────────────────────────────────

/// Allocates cells + flat-slot locals while walking a WIT type.
/// The "parent before children" recursion in [`Self::push`] is what
/// makes child cell indices observable from the parent's side-table
/// info (a child's index is just `cells.len()` after its sub-call
/// has appended).
struct LiftPlanBuilder {
    cells: Vec<CellOp>,
    /// Next available wasm flat-slot local (for `LocalRef::Param`).
    next_local: u32,
}

impl LiftPlanBuilder {
    fn new(first_local: u32) -> Self {
        Self {
            cells: Vec::new(),
            next_local: first_local,
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

    fn bump_local(&mut self) -> LocalRef {
        let r = LocalRef::Param(self.next_local);
        self.next_local += 1;
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
        LiftPlan { cells: self.cells }
    }
}

// ─── Result-lift descriptors (legacy LiftKind+ResultSource path) ──
//
// Result lifts stay on the (LiftKind, ResultSource, SideTableInfo)
// triple until compound result lifts land — they need a
// memory-load prefix before the cell write (canonical-ABI returns
// records via retptr, not flat locals), which doesn't fit the
// flat-LocalRef LiftPlan model. Migrate when records-as-result is
// in scope.

/// How to extract the function's return value when lifting it for
/// on-return. `side_table` populates the result tree's side-tables
/// at adapter-build time.
pub(super) struct ResultLift {
    pub source: ResultSource,
    pub side_table: SideTableInfo,
}

#[derive(Clone, Copy)]
pub(super) enum ResultSource {
    /// Direct primitive (no retptr): source is `lcl.result`.
    Direct(LiftKind),
    /// `(ptr, len)` pair in retptr scratch (string / `list<u8>`).
    RetptrPair { kind: LiftKind, retptr_offset: i32 },
}

impl ResultLift {
    /// Re-anchor the retptr scratch offset back-filled by the layout
    /// phase. No-op for `Direct` results.
    pub(super) fn set_retptr_offset(&mut self, off: i32) {
        if let ResultSource::RetptrPair { retptr_offset, .. } = &mut self.source {
            *retptr_offset = off;
        }
    }
}

/// Per-parameter lift recipe. `cells_offset` is the byte offset of
/// this param's contiguous cells slab within the static data segment;
/// the slab holds `plan.cell_count()` cells, each `cell_layout.size`
/// bytes. Filled in by the layout phase.
pub(super) struct ParamLift {
    pub name: BlobSlice,
    pub plan: LiftPlan,
    pub cells_offset: u32,
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
/// threads across params so each param's plan starts at the next
/// flat-slot local.
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
        slot_cursor = builder.next_local;
        params_lift.push(ParamLift {
            name,
            plan: builder.into_plan(),
            cells_offset: 0, // back-filled by lay_out_static_memory
        });
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
    if !result_kind_supported(kind) {
        // Records/variants/etc as result types: defer until result
        // lifts learn about retptr-memory-load prefixes. The wrapper
        // still calls after-hook with option::none for `result`.
        return None;
    }
    let side_table = side_table_info_for(ty, kind, resolve);
    let result_at_retptr = if is_async {
        import_sig.retptr
    } else {
        export_sig.retptr
    };
    let source = if result_at_retptr {
        ResultSource::RetptrPair {
            kind,
            retptr_offset: 0, // back-filled by the layout phase.
        }
    } else {
        ResultSource::Direct(kind)
    };
    Some(ResultLift { source, side_table })
}

/// Whether the result lift codegen (`emit_lift_result`) can handle
/// this kind today. Compound kinds need a memory-load-prefix variant
/// that doesn't exist yet.
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
//   3. Hand back per-(fn, param) and per-(fn, result) `BlobSlice`
//      pointers (relative to the segment start; caller translates
//      to absolute via `BlobSlice::translate`).
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

/// Output of [`build_side_table_blob`]: the entry-record bytes plus
/// per-(fn, param) and per-(fn, result) slice pointers (relative to
/// the segment start).
pub(super) struct SideTableBlob {
    pub bytes: Vec<u8>,
    pub per_param: Vec<Vec<BlobSlice>>,
    pub per_result: Vec<BlobSlice>,
}

impl SideTableBlob {
    /// Translate every per-param / per-result slice from
    /// segment-relative to absolute. Called by the layout phase
    /// once the segment's data offset is known.
    pub(super) fn translate_to(&mut self, base: u32) {
        for params in self.per_param.iter_mut() {
            for slice in params.iter_mut() {
                slice.translate(base);
            }
        }
        for slice in self.per_result.iter_mut() {
            slice.translate(base);
        }
    }
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
    per_func: &[FuncDispatch],
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
    per_func: &[FuncDispatch],
    strings: &StringTable,
    spec: &SideTableSpec<'_>,
    from_plan: impl Fn(&LiftPlan) -> Option<&NamedListInfo>,
    from_result: impl Fn(&SideTableInfo) -> Option<&NamedListInfo>,
) -> SideTableBlob {
    let mut bytes: Vec<u8> = Vec::new();
    let mut per_param: Vec<Vec<BlobSlice>> = Vec::with_capacity(per_func.len());
    let mut per_result: Vec<BlobSlice> = Vec::with_capacity(per_func.len());
    for fd in per_func {
        let mut params = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            params.push(append_entries(&mut bytes, strings, spec, from_plan(&p.plan)));
        }
        per_param.push(params);
        let result_info = fd.result_lift.as_ref().and_then(|r| from_result(&r.side_table));
        per_result.push(append_entries(&mut bytes, strings, spec, result_info));
    }
    SideTableBlob {
        bytes,
        per_param,
        per_result,
    }
}

fn append_entries(
    blob: &mut Vec<u8>,
    strings: &StringTable,
    spec: &SideTableSpec<'_>,
    info: Option<&NamedListInfo>,
) -> BlobSlice {
    let Some(info) = info else {
        return BlobSlice::EMPTY;
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
    BlobSlice { off: blob_off, len }
}

// ─── Convenience facades for emit.rs (one per side-table kind) ────

/// Register enum-info strings for every enum-typed lift across all
/// funcs. Thin wrapper over [`register_side_table_strings`] tied to
/// `CellOp::EnumCase`.
pub(super) fn register_enum_strings(
    per_func: &[FuncDispatch],
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
    per_func: &[FuncDispatch],
    strings: &StringTable,
    schema: &SchemaLayouts,
) -> SideTableBlob {
    build_side_table_blob(
        per_func,
        strings,
        &SideTableSpec {
            entry_layout: &schema.enum_info_layout,
            item_name_field: ENUM_INFO_CASE_NAME,
        },
        |plan| plan.enum_infos().next(),
        |st| st.enum_info.as_ref(),
    )
}

// ─── Wrapper-body locals + lift codegen ───────────────────────────

/// Locals used by the wrapper body. Allocated once up front so all
/// downstream emit phases (param lift, hook calls, result lift, async
/// task.return load) reference the same indices.
pub(super) struct WrapperLocals {
    /// Scratch for the cell write address.
    pub addr: u32,
    /// Packed status from canon-async hook calls.
    pub st: u32,
    /// Waitable-set handle for the wait loop.
    pub ws: u32,
    /// Retptr-loaded ptr for Text/Bytes result lift.
    pub ptr_scratch: u32,
    /// Retptr-loaded len for Text/Bytes result lift.
    pub len_scratch: u32,
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

pub(super) fn alloc_wrapper_locals(
    locals: &mut FunctionIndices,
    fd: &FuncDispatch,
) -> WrapperLocals {
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
    WrapperLocals {
        addr,
        st,
        ws,
        ptr_scratch,
        len_scratch,
        ext64,
        ext_f64,
        result,
        tr_addr,
    }
}

/// Emit the wasm that lifts one param into its cells slab. Walks
/// `param.plan.cells` in allocation order and, for each cell, sets
/// `lcl.addr` to that cell's absolute address (`cells_offset + i *
/// cell_size`) and dispatches on the cell's variant.
pub(super) fn emit_lift_param(
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
        CellOp::Bool { local } => cell_layout.emit_bool(f, addr, local.resolve(lcl)),
        CellOp::IntegerSignExt { local } => {
            f.instructions().local_get(local.resolve(lcl));
            f.instructions().i64_extend_i32_s();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        CellOp::IntegerZeroExt { local } => {
            f.instructions().local_get(local.resolve(lcl));
            f.instructions().i64_extend_i32_u();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        CellOp::Integer64 { local } => cell_layout.emit_integer(f, addr, local.resolve(lcl)),
        CellOp::FloatingF32 { local } => {
            f.instructions().local_get(local.resolve(lcl));
            f.instructions().f64_promote_f32();
            f.instructions().local_set(lcl.ext_f64);
            cell_layout.emit_floating(f, addr, lcl.ext_f64);
        }
        CellOp::FloatingF64 { local } => cell_layout.emit_floating(f, addr, local.resolve(lcl)),
        CellOp::Text { ptr, len } => {
            cell_layout.emit_text(f, addr, ptr.resolve(lcl), len.resolve(lcl));
        }
        CellOp::Bytes { ptr, len } => {
            cell_layout.emit_bytes(f, addr, ptr.resolve(lcl), len.resolve(lcl));
        }
        CellOp::EnumCase { local, .. } => {
            cell_layout.emit_enum_case(f, addr, local.resolve(lcl));
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

/// Emit the wasm to lift one return value into the cell at `addr_local`.
/// Direct primitive returns read from `result_local`; Text/Bytes
/// returns load `(ptr, len)` from the retptr scratch into `ptr_scratch`
/// / `len_scratch` and lift those.
pub(super) fn emit_lift_result(
    f: &mut Function,
    cell_layout: &CellLayout,
    source: ResultSource,
    lcl: &WrapperLocals,
) {
    match source {
        ResultSource::Direct(kind) => {
            let local = lcl
                .result
                .expect("ResultSource::Direct → result local must be allocated");
            emit_lift_kind(f, cell_layout, kind, local, local, lcl);
        }
        ResultSource::RetptrPair {
            kind,
            retptr_offset,
        } => {
            f.instructions().i32_const(retptr_offset);
            f.instructions().i32_load(MemArg {
                offset: SLICE_PTR_OFFSET as u64,
                align: 2,
                memory_index: 0,
            });
            f.instructions().local_set(lcl.ptr_scratch);
            f.instructions().i32_const(retptr_offset);
            f.instructions().i32_load(MemArg {
                offset: SLICE_LEN_OFFSET as u64,
                align: 2,
                memory_index: 0,
            });
            f.instructions().local_set(lcl.len_scratch);
            emit_lift_kind(f, cell_layout, kind, lcl.ptr_scratch, lcl.len_scratch, lcl);
        }
    }
}
