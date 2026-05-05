//! Lift plan: structural single-source-of-truth.
//!
//! A [`LiftPlan`] describes one (param | result) lift end-to-end:
//! every cell that needs to be written, in allocation order, with
//! each cell bound to its source flat slots (plan-relative, 0-based)
//! and any nominal-cell side-table info it carries. The root cell —
//! the one the field-tree's `root` field points at — lives at
//! [`LiftPlan::root`]; the plan-builder records it explicitly so the
//! root's position in `cells` doesn't have to coincide with index 0.
//!
//! Cells reference flat slots, NOT absolute wasm-local indices —
//! the emit phase supplies a `local_base: u32` and looks up
//! `local_base + cell.flat_slot` per cell. This keeps the same plan
//! usable for both side-table builders (which read structural fields
//! only) and the emit phase, regardless of where the plan's locals
//! end up in the wasm function.
//!
//! Why a flat Vec instead of a nested IR: cell indices in
//! nominal-cell side-table entries (e.g., a record's `fields`
//! list) are just `Vec`-positions in `cells`. The same vector that
//! drives codegen also drives side-table emission; child indices
//! can't desync because they're a property of allocation order.
//! `cells.len()` is the slab size; total flat-slot consumption is
//! recorded explicitly on [`LiftPlan::flat_slot_count`]. See
//! [`docs/tiers/lift-codegen.md`](../../../../docs/tiers/lift-codegen.md).

use wit_parser::{Resolve, Type};

use super::super::super::abi::emit::BlobSlice;
use super::super::blob::NameInterner;

/// One cell to write at a known cell-array index. Each variant
/// captures the cell's runtime-disc semantics, its source flat
/// slots (plan-relative, 0-based — the emit phase adds a
/// `local_base` to recover the absolute wasm-local index), and any
/// side-table info this cell contributes (e.g., enum-info /
/// record-info entries).
///
/// Wired variants carry full lift payload (flat-slot positions +
/// per-kind side-table info); un-wired variants carry no payload and
/// `todo!()` at codegen time. Un-wired variants are placeholder tags
/// — they're never constructed today (the plan-builder `todo!()`s
/// before reaching them), but listing them keeps the
/// [`super::emit::emit_cell_op`] match exhaustive without a `_`
/// catchall, so adding a new wired type forces the codegen arm to be
/// filled in. New WIT types: add one variant + one arm in
/// [`LiftPlanBuilder::push`] + one arm in
/// [`super::emit::emit_cell_op`]. Roadmap: `docs/tiers/lift-codegen.md`.
#[allow(dead_code)] // un-wired variants exist only for exhaustive match
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Cell {
    // ── Wired ─────────────────────────────────────────────────────
    /// `bool` — 1 i32 slot (0/1) → `cell::bool`.
    Bool { flat_slot: u32 },
    /// `s8`/`s16`/`s32` — 1 i32 slot, sign-extend → `cell::integer`.
    IntegerSignExt { flat_slot: u32 },
    /// `u8`/`u16`/`u32` — 1 i32 slot, zero-extend → `cell::integer`.
    IntegerZeroExt { flat_slot: u32 },
    /// `s64`/`u64` — 1 i64 slot, no widen → `cell::integer`.
    Integer64 { flat_slot: u32 },
    /// `f32` — 1 f32 slot, `f64.promote_f32` → `cell::floating`.
    FloatingF32 { flat_slot: u32 },
    /// `f64` — 1 f64 slot, no widen → `cell::floating`.
    FloatingF64 { flat_slot: u32 },
    /// `string` — 2 i32 slots (ptr, len) → `cell::text`.
    Text { ptr_slot: u32, len_slot: u32 },
    /// `list<u8>` — 2 i32 slots (ptr, len) → `cell::bytes`.
    Bytes { ptr_slot: u32, len_slot: u32 },
    /// `enum { ... }` → `cell::enum-case(u32)`. Carries the type-name +
    /// case-names so the side-table builder can register them.
    EnumCase { flat_slot: u32, info: NamedListInfo },
    /// `record { ... }` → `cell::record-of(u32)` (side-table index).
    /// Children live elsewhere in the same plan; `fields` references
    /// them by `LiftPlan::cells` position. `type_name` and each
    /// field's name are pre-interned [`BlobSlice`]s into the shared
    /// name blob — the side-table builder writes them straight into
    /// the `record-info` segment without re-interning.
    RecordOf {
        type_name: BlobSlice,
        /// `(field-name, child-cell-idx)` per field, in WIT order.
        /// `child-cell-idx` indexes into the same `LiftPlan::cells`.
        fields: Vec<(BlobSlice, u32)>,
    },

    // ── Un-wired compound (todo!() in `LiftPlanBuilder::push`
    //    + `emit_cell_op` until codegen lands) ─────────────────────
    /// `char` → `cell::text` (utf-8 encode the i32 code point).
    Char,
    /// `list<T>` (non-u8 element) → `cell::list-of`.
    ListOf,
    /// `tuple<...>` → `cell::tuple-of`.
    TupleOf,
    /// `option<T>` → `cell::option-some(u32)` / `cell::option-none`.
    Option,
    /// `result<T, E>` → `cell::result-ok(option<u32>)` / `cell::result-err(option<u32>)`.
    Result,
    /// `flags { ... }` → `cell::flags-set(u32)`.
    Flags,
    /// `variant { ... }` → `cell::variant-case(u32)`.
    Variant,

    // ── Un-wired handle ──────────────────────────────────────────
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

/// Plan for lifting one (param | result) into a cell tree. Cells
/// are listed in allocation order: children land in `cells` before
/// their parents, so a record's `fields` list always references
/// already-pushed indices and the parent cell can be appended fully
/// constructed (no back-fill). [`LiftPlan::root`] records the index
/// of the root cell — for primitives it's `0` (the only cell), for
/// records it's the parent at the end of the slab. The field-tree's
/// `root` field points at this index. Walked top-to-bottom by the
/// emit-code phase; the side-table builder also walks `cells` to
/// pull out per-kind side-table contributions.
pub(crate) struct LiftPlan {
    pub(super) cells: Vec<Cell>,
    /// Total flat-slot locals consumed by the plan. Cells reference
    /// flat slots in `0..flat_slot_count`; the emit phase adds the
    /// caller-supplied `local_base` to recover absolute wasm-local
    /// indices.
    pub flat_slot_count: u32,
    /// Index of the root cell within `cells`. The plan-builder
    /// pushes children before parents, so for compound shapes this
    /// is the last-appended cell rather than `cells[0]`.
    root: u32,
}

impl LiftPlan {
    /// Build a plan from a single WIT type. The unified entry point
    /// for both the per-param and the compound-result classifiers —
    /// each builds one plan from one `Type`, the only difference is
    /// what the caller wraps it in (`ParamLift` vs `CompoundResult`)
    /// and what `local_base` the emit phase supplies. `names` interns
    /// every record type-name and field-name as the plan is built.
    pub(super) fn for_type(ty: &Type, resolve: &Resolve, names: &mut NameInterner) -> Self {
        let mut builder = LiftPlanBuilder::new();
        let root = builder.push(ty, resolve, names);
        builder.into_plan(root)
    }

    pub(crate) fn cell_count(&self) -> u32 {
        self.cells.len() as u32
    }

    /// Index of the root cell — the entry point the field-tree's
    /// `root` field stores. Matches the value the plan-builder
    /// returned from the top-level [`LiftPlanBuilder::push`].
    pub(crate) fn root(&self) -> u32 {
        self.root
    }

    /// Iterator over every `Cell::EnumCase` in the plan. Used by
    /// the side-table builder to register enum strings.
    pub(super) fn enum_infos(&self) -> impl Iterator<Item = &NamedListInfo> {
        self.cells.iter().filter_map(|op| match op {
            Cell::EnumCase { info, .. } => Some(info),
            _ => None,
        })
    }
}

// ─── Lift plan builder ────────────────────────────────────────────

/// Allocates cells + plan-relative flat-slot positions while walking
/// a WIT type. Recursion is **children before parent**: each
/// [`Self::push`] sub-call appends its cells to `cells`, then the
/// caller pushes the parent referencing the now-known child indices.
/// Side-table entries that name a child cell (e.g. `RecordOf::fields`)
/// can therefore be built fully and pushed once, with no back-fill —
/// the parent cell is immutable as soon as it lands in `cells`.
///
/// Plans are local-base-independent: cells reference flat slots in
/// `0..flat_slot_count`. The emit phase adds the caller-supplied
/// `local_base` to recover the absolute wasm-local index — for
/// params that's the cumulative slot cursor across preceding params,
/// for compound results that's the first synth local allocated by
/// `alloc_wrapper_locals`.
pub(super) struct LiftPlanBuilder {
    cells: Vec<Cell>,
    /// Next available plan-relative flat-slot position. Incremented
    /// by `bump_flat_slot` as cells consume flat slots.
    next_flat_slot: u32,
}

impl LiftPlanBuilder {
    pub(super) fn new() -> Self {
        Self {
            cells: Vec::new(),
            next_flat_slot: 0,
        }
    }

    /// Push cells for one lift; returns the index of the just-pushed
    /// root cell. For primitives that's `cells.len()` before the
    /// push (single cell at the end); for compound shapes it's the
    /// parent cell, appended after its children. Type aliases peel
    /// through and reclassify the underlying type. `names` interns
    /// record type-names and field-names so the resulting
    /// [`Cell::RecordOf`]s carry pre-interned [`BlobSlice`]s.
    pub(super) fn push(&mut self, ty: &Type, resolve: &Resolve, names: &mut NameInterner) -> u32 {
        match ty {
            Type::Bool => {
                let flat_slot = self.bump_flat_slot();
                self.push_cell(Cell::Bool { flat_slot })
            }
            Type::S8 | Type::S16 | Type::S32 => {
                let flat_slot = self.bump_flat_slot();
                self.push_cell(Cell::IntegerSignExt { flat_slot })
            }
            Type::U8 | Type::U16 | Type::U32 => {
                let flat_slot = self.bump_flat_slot();
                self.push_cell(Cell::IntegerZeroExt { flat_slot })
            }
            Type::S64 | Type::U64 => {
                let flat_slot = self.bump_flat_slot();
                self.push_cell(Cell::Integer64 { flat_slot })
            }
            Type::F32 => {
                let flat_slot = self.bump_flat_slot();
                self.push_cell(Cell::FloatingF32 { flat_slot })
            }
            Type::F64 => {
                let flat_slot = self.bump_flat_slot();
                self.push_cell(Cell::FloatingF64 { flat_slot })
            }
            Type::String => {
                let ptr_slot = self.bump_flat_slot();
                let len_slot = self.bump_flat_slot();
                self.push_cell(Cell::Text { ptr_slot, len_slot })
            }
            Type::Char => todo!("plan-builder for un-wired Cell::Char"),
            Type::ErrorContext => todo!("plan-builder for un-wired Cell::ErrorContext"),
            Type::Id(id) => match &resolve.types[*id].kind {
                wit_parser::TypeDefKind::List(Type::U8) => {
                    let ptr_slot = self.bump_flat_slot();
                    let len_slot = self.bump_flat_slot();
                    self.push_cell(Cell::Bytes { ptr_slot, len_slot })
                }
                wit_parser::TypeDefKind::Enum(_) => {
                    let info = enum_lift_info_for_type(ty, resolve)
                        .expect("Enum kind implies enum-info available");
                    let flat_slot = self.bump_flat_slot();
                    self.push_cell(Cell::EnumCase { flat_slot, info })
                }
                wit_parser::TypeDefKind::Record(_) => self.push_record(ty, resolve, names),
                wit_parser::TypeDefKind::Type(t) => self.push(t, resolve, names),
                wit_parser::TypeDefKind::List(_) => {
                    todo!("plan-builder for un-wired Cell::ListOf")
                }
                wit_parser::TypeDefKind::Tuple(_) => {
                    todo!("plan-builder for un-wired Cell::TupleOf")
                }
                wit_parser::TypeDefKind::Variant(_) => {
                    todo!("plan-builder for un-wired Cell::Variant")
                }
                wit_parser::TypeDefKind::Flags(_) => {
                    todo!("plan-builder for un-wired Cell::Flags")
                }
                wit_parser::TypeDefKind::Option(_) => {
                    todo!("plan-builder for un-wired Cell::Option")
                }
                wit_parser::TypeDefKind::Result(_) => {
                    todo!("plan-builder for un-wired Cell::Result")
                }
                wit_parser::TypeDefKind::Handle(_) => {
                    todo!("plan-builder for un-wired Cell::Handle")
                }
                wit_parser::TypeDefKind::Future(_) => {
                    todo!("plan-builder for un-wired Cell::Future")
                }
                wit_parser::TypeDefKind::Stream(_) => {
                    todo!("plan-builder for un-wired Cell::Stream")
                }
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

    fn bump_flat_slot(&mut self) -> u32 {
        let r = self.next_flat_slot;
        self.next_flat_slot = self
            .next_flat_slot
            .checked_add(1)
            // Tripwire; realistic blow-ups are caught by `check_layout_budget`.
            .expect("LiftPlanBuilder flat-slot counter overflowed u32");
        r
    }

    /// Append `cell` and return the index it landed at.
    fn push_cell(&mut self, cell: Cell) -> u32 {
        let idx = self.cells.len() as u32;
        self.cells.push(cell);
        idx
    }

    /// Records: recurse on each field first (each sub-call appends
    /// its cells and returns the index of its root), then push the
    /// parent referencing those already-known child indices. The
    /// parent cell is constructed in full before it lands in `cells`,
    /// so there is no back-fill step. The type-name and field-name
    /// strings are interned into `names` up-front so the pushed cell
    /// already carries [`BlobSlice`]s.
    fn push_record(&mut self, ty: &Type, resolve: &Resolve, names: &mut NameInterner) -> u32 {
        let Type::Id(id) = ty else {
            unreachable!("Record kind came from non-Id type")
        };
        let typedef = &resolve.types[*id];
        let wit_parser::TypeDefKind::Record(r) = &typedef.kind else {
            unreachable!("Record kind came from non-Record TypeDefKind")
        };
        let type_name = names.intern(typedef.name.as_deref().unwrap_or(""));
        // Intern field-names + recurse on each field. Each recursive
        // push appends the child's cells to `cells` and returns the
        // child's root index — by the time this loop finishes, every
        // index in `fields` references a cell that already exists.
        let mut fields = Vec::with_capacity(r.fields.len());
        for field in &r.fields {
            let name_slice = names.intern(&field.name);
            let child_idx = self.push(&field.ty, resolve, names);
            fields.push((name_slice, child_idx));
        }
        // Push the fully-built parent. Lands at the current end of
        // `cells`, after all of its children.
        self.push_cell(Cell::RecordOf { type_name, fields })
    }

    pub(super) fn into_plan(self, root: u32) -> LiftPlan {
        LiftPlan {
            cells: self.cells,
            flat_slot_count: self.next_flat_slot,
            root,
        }
    }
}

/// A type-name plus an ordered list of item names. Carries
/// enough info to populate any of the `*-info` side-table records
/// in `splicer:common/types` that share the
/// `{ type-name, <item>-name }` shape (enum-info, eventually flags-info
/// + variant-info).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NamedListInfo {
    pub(super) type_name: String,
    /// Item names in WIT declaration order — the i'th entry's WIT
    /// declaration index equals `i` (matching the disc / bit-position
    /// / field-index used at runtime).
    pub(super) item_names: Vec<String>,
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
