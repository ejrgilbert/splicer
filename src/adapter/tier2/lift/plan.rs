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

use anyhow::{anyhow, Result};
use wit_parser::abi::WasmType;
use wit_parser::{Resolve, Type};

use super::super::super::abi::emit::{wasm_type_to_val, BlobSlice};
use super::super::super::abi::flat_types;
use super::super::blob::NameInterner;

const ISSUES_URL: &str = "https://github.com/ejrgilbert/splicer/issues";

/// One cell to write at a known cell-array index. Each variant
/// captures the cell's runtime-disc semantics, its source flat
/// slots (plan-relative, 0-based — the emit phase adds a
/// `local_base` to recover the absolute wasm-local index), and any
/// side-table info this cell contributes (e.g., enum-info /
/// record-info entries).
///
/// **Joined-arm rule.** Cells inside a `result` / `variant` arm read
/// flat slots shared with sibling arms. Pure flat-slot writers
/// (`Text`, `Bytes`, `Char`, `Flags`, `Variant`, `Handle`,
/// `Integer*`, `Float*`, `Bool`, `EnumCase`) emit unconditionally —
/// inactive-arm payloads land in cells the runtime never reads, so
/// the bytes are inert. Cells with side effects beyond their own
/// payload — today only [`Cell::ListOf`], whose `(ptr, len)` feed
/// `cabi_realloc` and an unbounded loop — must disc-gate via
/// `arm_guards`. Adding a side-effecting variant (allocator, host
/// call, scratch grow) means adding the same gate.
///
/// New WIT types: add one variant + one arm in
/// [`LiftPlanBuilder::push`] + one arm in
/// [`super::emit::emit_cell_op`]. Roadmap: `docs/tiers/lift-codegen.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Cell {
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
    /// `char` — 1 i32 slot (code point); utf-8 encode into a per-cell
    /// scratch buffer (1–4 bytes), then write `cell::text(ptr, len)`
    /// referencing the scratch.
    Char { flat_slot: u32 },
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
    /// `tuple<...>` → `cell::tuple-of(list<u32>)`. `children` are
    /// plan-cell indices into the same [`LiftPlan::cells`]. The layout
    /// phase packs each `children` array into the shared tuple-indices
    /// segment; the emit phase reads the resulting per-cell
    /// [`BlobSlice`] (off `tuple_indices_cell_idx`) and writes
    /// `(ptr, len)` constants.
    TupleOf { children: Vec<u32> },
    /// `option<T>` → `cell::option-some(u32)` / `cell::option-none`.
    /// Flat layout: `[i32 disc, ...flat(T)]`. The child cell is
    /// always emitted; canonical-ABI lower zeroes T's slots on `none`
    /// and readers gate on the parent's disc.
    Option { disc_slot: u32, child_idx: u32 },
    /// `result<T, E>` → `cell::result-ok(option<u32>)` /
    /// `cell::result-err(option<u32>)`. Flat layout:
    /// `[i32 disc, ...join(flat(T), flat(E))]`. Both arms' child cells
    /// live in `cells`; the wrong-arm cells read shared flat slots
    /// and produce harmless garbage on inactive disc. `ok_idx` /
    /// `err_idx` are `None` for unit arms (`result<_, E>` /
    /// `result<T, _>`).
    ///
    /// **Load-bearing invariant.** `ok_idx` / `err_idx` are emitted
    /// into the cell payload but the runtime gates on disc and
    /// **must not** follow the inactive index. The inactive cell
    /// holds either bytes from the active arm's flat slots
    /// (Text/Bytes/Char/etc., harmless once skipped) or — for
    /// disc-gated [`Cell::ListOf`] — bytes from raw `cabi_realloc`
    /// memory (no zero-init guarantee). See the joined-arm rule on
    /// the [`Cell`] enum doc.
    Result {
        disc_slot: u32,
        ok_idx: Option<u32>,
        err_idx: Option<u32>,
    },

    /// `flags { ... }` → `cell::flags-set(u32)`. Single i32 lift slot
    /// (canonical-ABI caps flags at 32 bits).
    Flags { flat_slot: u32, info: NamedListInfo },
    /// `variant { ... }` → `cell::variant-case(u32)`. Flat layout
    /// `[disc, ...joined_flat_of_each_case]`. `per_case_payload[i]`
    /// is `Some(child_idx)` for cases with a payload, `None` for unit.
    /// Inactive arms' children get garbage from joined slots; the
    /// runtime patches `case-name` + `payload` per call so readers
    /// gate on disc and never follow them.
    ///
    /// Same load-bearing invariant as [`Cell::Result`]: the runtime
    /// **must not** follow `per_case_payload[i]` for `i ≠ disc`.
    Variant {
        disc_slot: u32,
        per_case_payload: Vec<Option<u32>>,
        info: NamedListInfo,
    },

    /// `own<R>` / `borrow<R>` / `stream<T>` / `future<T>` →
    /// `cell::{resource,stream,future}-handle(u32)`. Single i32 lift
    /// slot (canonical-ABI handle); the side-table entry carries
    /// `(type-name, id)` with `id` = handle bits zero-extended per
    /// call. `type_name` is interned at plan-build time. `kind`
    /// picks the cell-disc; the lift codegen and side-table builder
    /// are otherwise identical across all three.
    Handle {
        flat_slot: u32,
        type_name: BlobSlice,
        kind: HandleKind,
    },

    /// `list<T>` (non-u8; `list<u8>` fast-paths through `Cell::Bytes`)
    /// → `cell::list-of`. Flat `(i32 ptr, i32 len)`. `element_plan`
    /// is a NESTED [`LiftPlan`] with its own cell-index space —
    /// distinct from the outer-plan indices used by other variants
    /// like [`Cell::TupleOf::children`] or [`Cell::Option::child_idx`].
    /// `element_plan.source_ty` is the WIT element type
    /// (drives `lift_from_memory` per iteration). `list_idx` keys
    /// into the parallel `list_locals` array, so per-list emit + alloc
    /// state is paired structurally rather than by iteration order.
    ///
    /// `arm_guards` is non-empty when the list lives inside joined
    /// `result` / `variant` arm(s) — outer→inner order. The alloc
    /// pre-pass and the per-list emit body AND-stack the predicates
    /// so an inactive arm's bytes can't surface as `len` (see
    /// [`ArmGuard`] and the joined-arm rule on the [`Cell`] doc).
    ListOf {
        list_idx: u32,
        ptr_slot: u32,
        len_slot: u32,
        element_plan: Box<LiftPlan>,
        arm_guards: Vec<ArmGuard>,
    },
}

/// Disc-equality predicate guarding a [`Cell::ListOf`]'s side
/// effects. Result ok = 0, err = 1; variant uses case index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArmGuard {
    pub(crate) disc_slot: u32,
    pub(crate) expected_disc: u32,
}

/// Which `cell::*-handle` variant a [`Cell::Handle`] should emit.
/// All four share the canonical-ABI representation (single i32
/// handle), the `handle-info` side-table layout, and the lift
/// codegen — only the cell-disc differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandleKind {
    /// `own<R>` / `borrow<R>` → `cell::resource-handle`.
    Resource,
    /// `stream<T>` → `cell::stream-handle`.
    Stream,
    /// `future<T>` → `cell::future-handle`.
    Future,
    /// `error-context` → `cell::error-context-handle`. Just-an-id
    /// rendering — the canonical-ABI `error-context.debug-message`
    /// builtin would let us surface the string, but cross-component
    /// error-context lift is currently broken in wasmtime (≤44, "very
    /// incomplete" per its own config docstring) so the wrapper never
    /// gets to call it. Revisit when host catches up.
    ErrorContext,
}

impl HandleKind {
    /// WIT case-name for the matching `cell::*-handle` disc.
    pub(crate) fn cell_disc_case(self) -> &'static str {
        match self {
            HandleKind::Resource => "resource-handle",
            HandleKind::Stream => "stream-handle",
            HandleKind::Future => "future-handle",
            HandleKind::ErrorContext => "error-context-handle",
        }
    }
}

/// How an `allowed_as_list_element` cell flows through the list-emit
/// body. New variants force a side-data decision in
/// [`super::emit::elem_cell_side_data`] at compile time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListElementClass {
    /// Reads only flat slots; folds to `CellSideData::None`.
    Scalar,
    /// Per-iteration utf-8 scratch from the per-list `cabi_realloc`
    /// in [`super::emit::emit_list_of_arm`]. Folds to a
    /// `Prestaged` `CellSideData::Char`.
    PrestagedChar,
}

impl Cell {
    /// Classify a cell shape as a `list<T>` element. `None` for kinds
    /// the lift codegen can't yet emit per-element (compound shapes,
    /// remaining scratch-bearing kinds, nested lists). Exhaustive
    /// match — adding a `Cell` variant forces a yes/no decision here.
    pub(crate) fn list_element_class(&self) -> Option<ListElementClass> {
        match self {
            Cell::Char { .. } => Some(ListElementClass::PrestagedChar),
            Cell::Bool { .. }
            | Cell::IntegerSignExt { .. }
            | Cell::IntegerZeroExt { .. }
            | Cell::Integer64 { .. }
            | Cell::FloatingF32 { .. }
            | Cell::FloatingF64 { .. }
            | Cell::Text { .. }
            | Cell::Bytes { .. }
            | Cell::EnumCase { .. } => Some(ListElementClass::Scalar),
            Cell::Flags { .. }
            | Cell::Handle { .. }
            | Cell::RecordOf { .. }
            | Cell::TupleOf { .. }
            | Cell::Option { .. }
            | Cell::Result { .. }
            | Cell::Variant { .. }
            | Cell::ListOf { .. } => None,
        }
    }

    /// Whether this cell shape is supported as a `list<T>` element.
    pub(crate) fn allowed_as_list_element(&self) -> bool {
        self.list_element_class().is_some()
    }
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiftPlan {
    pub(super) cells: Vec<Cell>,
    /// Total flat-slot locals consumed by the plan. Cells reference
    /// flat slots in `0..flat_slot_count`; the emit phase adds the
    /// caller-supplied `local_base` to recover absolute wasm-local
    /// indices.
    pub flat_slot_count: u32,
    /// Per-flat-slot joined wasm type, recorded only when the joined
    /// differs from at least one arm's per-position wasm type — i.e.
    /// the slot lives inside a widening `result` / `variant`. The emit
    /// phase derives the per-leaf bitcast as `cast(joined, leaf_arm_ty)`
    /// where `leaf_arm_ty` is implied by the cell's variant.
    /// Indexed by plan-relative flat slot. Length = `flat_slot_count`.
    slot_widening: Vec<Option<WasmType>>,
    /// Index of the root cell within `cells`. The plan-builder
    /// pushes children before parents, so for compound shapes this
    /// is the last-appended cell rather than `cells[0]`.
    root: u32,
    /// WIT type the plan was built from. Drives `lift_from_memory`
    /// in element-plan + compound-result codegen.
    pub source_ty: Type,
}

impl LiftPlan {
    /// Build a plan from a single WIT type. The unified entry point
    /// for both the per-param and the compound-result classifiers —
    /// each builds one plan from one `Type`, the only difference is
    /// what the caller wraps it in (`ParamLift` vs `CompoundResult`)
    /// and what `local_base` the emit phase supplies. `names` interns
    /// every record type-name and field-name as the plan is built.
    /// Errors on shapes the lift codegen doesn't yet support.
    pub(super) fn for_type(ty: &Type, resolve: &Resolve, names: &mut NameInterner) -> Result<Self> {
        let mut builder = LiftPlanBuilder::new();
        let root = builder.push(ty, resolve, names);
        if let Some(err) = builder.error {
            return Err(err);
        }
        Ok(builder.into_plan(root, *ty))
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

    /// Joined wasm type at `flat_slot` when the slot is inside a
    /// widening `result` / `variant` arm — `None` for the common
    /// matching case. The emit phase pairs this with the leaf cell's
    /// implied arm type to compute `cast(joined, arm)`.
    pub(crate) fn widening_for(&self, flat_slot: u32) -> Option<WasmType> {
        self.slot_widening
            .get(flat_slot as usize)
            .copied()
            .flatten()
    }

    /// Recursively walk every cell in the plan, including cells nested
    /// inside `Cell::ListOf::element_plan`. Side-table-info iterators
    /// build on this so an enum/flags/variant cell that lives in a list
    /// element still surfaces its info to the builder.
    fn walk_cells_recursive(&self) -> Vec<&Cell> {
        let mut out = Vec::with_capacity(self.cells.len());
        for cell in &self.cells {
            out.push(cell);
            if let Cell::ListOf { element_plan, .. } = cell {
                out.extend(element_plan.walk_cells_recursive());
            }
        }
        out
    }

    /// Whether any cell in the plan tree is a `Cell::Char` — top-level
    /// or nested inside a `Cell::ListOf::element_plan`. Drives the
    /// wrapper-level decision to allocate `lcl.char_scratch_addr`,
    /// the shared staging local that both static and list-element
    /// char-cell emit reads from.
    pub(crate) fn contains_char(&self) -> bool {
        self.walk_cells_recursive()
            .iter()
            .any(|c| matches!(c, Cell::Char { .. }))
    }

    /// Iterator over every `Cell::EnumCase` in the plan tree
    /// (including list element plans). Used by the side-table builder
    /// to register enum strings.
    pub(super) fn enum_infos(&self) -> impl Iterator<Item = &NamedListInfo> {
        self.walk_cells_recursive()
            .into_iter()
            .filter_map(|op| match op {
                Cell::EnumCase { info, .. } => Some(info),
                _ => None,
            })
    }

    /// Iterator over every `Cell::Flags` in the plan tree. Used by the
    /// side-table builder to register flag-type and flag-name strings.
    pub(super) fn flags_infos(&self) -> impl Iterator<Item = &NamedListInfo> {
        self.walk_cells_recursive()
            .into_iter()
            .filter_map(|op| match op {
                Cell::Flags { info, .. } => Some(info),
                _ => None,
            })
    }

    /// Iterator over every `Cell::Variant` in the plan tree. Used by
    /// the side-table builder to register variant-type and case-name
    /// strings.
    pub(super) fn variant_infos(&self) -> impl Iterator<Item = &NamedListInfo> {
        self.walk_cells_recursive()
            .into_iter()
            .filter_map(|op| match op {
                Cell::Variant { info, .. } => Some(info),
                _ => None,
            })
    }

    /// Placeholder plan after a sub-`for_type` error; never reaches emit.
    pub(super) fn stub_for(source_ty: Type) -> Self {
        Self {
            cells: vec![Cell::Bool { flat_slot: 0 }],
            flat_slot_count: 1,
            slot_widening: vec![None],
            root: 0,
            source_ty,
        }
    }

    /// Iterator over every `Cell::ListOf` in the plan in `plan.cells`
    /// order. Drives per-list locals allocation, the runtime total-
    /// cells pre-pass, and the per-list emit arm. The element type
    /// for `lift_from_memory` is `element_plan.source_ty`.
    pub(crate) fn list_specs(&self) -> impl Iterator<Item = ListSpec<'_>> + '_ {
        self.cells.iter().filter_map(|op| match op {
            Cell::ListOf {
                list_idx,
                len_slot,
                element_plan,
                arm_guards,
                ..
            } => Some(ListSpec {
                list_idx: *list_idx,
                len_slot: *len_slot,
                element_plan,
                arm_guards,
            }),
            _ => None,
        })
    }
}

/// Per-`Cell::ListOf` view used by alloc + emit. Source element type
/// is `element_plan.source_ty`. `list_idx` matches the cell's field
/// so callers can index `list_locals` structurally.
#[derive(Clone, Copy)]
pub(crate) struct ListSpec<'a> {
    pub list_idx: u32,
    pub len_slot: u32,
    pub element_plan: &'a LiftPlan,
    /// Empty unless the list lives inside joined `result` / `variant` arm(s).
    pub arm_guards: &'a [ArmGuard],
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
    /// Per-flat-slot joined wasm type for widening inside
    /// variant / result arms. Grows lazily — `bump_flat_slot` only
    /// appends `None` when extending past the current max, so arms
    /// rewinding `next_flat_slot` don't double-grow the table.
    /// Arms with widening write entries via [`Self::set_widening`].
    slot_widening: Vec<Option<WasmType>>,
    /// Running list-of cell counter; assigned to each `Cell::ListOf`
    /// via `list_idx` so emit + alloc can index `list_locals` directly.
    next_list_idx: u32,
    /// Active arm guards while walking joined `result` / `variant`
    /// arms. Outer→inner. `Cell::ListOf` clones this snapshot so
    /// emit can disc-gate `cabi_realloc` + the element loop.
    arm_guard_stack: Vec<ArmGuard>,
    /// First error hit during the walk; surfaced by [`LiftPlan::for_type`].
    error: Option<anyhow::Error>,
}

impl LiftPlanBuilder {
    pub(super) fn new() -> Self {
        Self {
            cells: Vec::new(),
            slot_widening: Vec::new(),
            next_flat_slot: 0,
            next_list_idx: 0,
            arm_guard_stack: Vec::new(),
            error: None,
        }
    }

    /// First error wins; the walk continues with stub cells.
    fn record_error(&mut self, err: anyhow::Error) {
        if self.error.is_none() {
            self.error = Some(err);
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
            Type::Char => {
                let flat_slot = self.bump_flat_slot();
                self.push_cell(Cell::Char { flat_slot })
            }
            Type::ErrorContext => {
                // No nested type to surface; the cell-disc already
                // names the kind. `handle-info.type-name` stays empty.
                let type_name = names.intern("");
                let flat_slot = self.bump_flat_slot();
                self.push_cell(Cell::Handle {
                    flat_slot,
                    type_name,
                    kind: HandleKind::ErrorContext,
                })
            }
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
                wit_parser::TypeDefKind::Tuple(_) => self.push_tuple(ty, resolve, names),
                wit_parser::TypeDefKind::Type(t) => self.push(t, resolve, names),
                wit_parser::TypeDefKind::List(elem) => self.push_list_of(elem, resolve, names),
                wit_parser::TypeDefKind::Variant(_) => self.push_variant(ty, resolve, names),
                wit_parser::TypeDefKind::Flags(_) => {
                    let info = flags_lift_info_for_type(ty, resolve)
                        .expect("Flags kind implies flags-info available");
                    let flat_slot = self.bump_flat_slot();
                    self.push_cell(Cell::Flags { flat_slot, info })
                }
                wit_parser::TypeDefKind::Option(inner) => self.push_option(inner, resolve, names),
                wit_parser::TypeDefKind::Result(_) => self.push_result(ty, resolve, names),
                wit_parser::TypeDefKind::Handle(h) => self.push_handle(h, resolve, names),
                wit_parser::TypeDefKind::Stream(elem) => {
                    self.push_stream_or_future(elem.as_ref(), HandleKind::Stream, resolve, names)
                }
                wit_parser::TypeDefKind::Future(elem) => {
                    self.push_stream_or_future(elem.as_ref(), HandleKind::Future, resolve, names)
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
        // Variant / result arms rewind `next_flat_slot` to share slots;
        // only extend the widening table when reaching a new high-water
        // mark (preserves entries set by an earlier arm at this slot).
        if self.slot_widening.len() < self.next_flat_slot as usize {
            self.slot_widening.push(None);
        }
        r
    }

    /// Record the joined-flat type at `flat_slot`. Called by
    /// `push_result` / `push_variant` for slots whose joined type
    /// differs from at least one arm's per-position type. Idempotent
    /// when arms agree on the joined (they always do — joined is
    /// structural over the parent type).
    fn set_widening(&mut self, flat_slot: u32, joined_ty: WasmType) {
        debug_assert!(
            (flat_slot as usize) < self.slot_widening.len(),
            "set_widening called for flat_slot {flat_slot} before bump_flat_slot reached it \
             (slot_widening len = {})",
            self.slot_widening.len(),
        );
        // Multi-arm overwrites are expected (each arm records its
        // own widening at shared slots); pin that they agree on the
        // joined wasm type — same parent type → same joined.
        if let Some(prev) = self.slot_widening[flat_slot as usize] {
            debug_assert_eq!(
                wasm_type_to_val(prev),
                wasm_type_to_val(joined_ty),
                "set_widening overwriting slot {flat_slot} with a different joined type \
                 ({prev:?} vs {joined_ty:?}) — joined should be structural"
            );
        }
        self.slot_widening[flat_slot as usize] = Some(joined_ty);
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

    /// Same shape as [`Self::push_record`], minus the type/field
    /// names — `tuple<...>` is anonymous; the cell payload is just
    /// child cell indices.
    fn push_tuple(&mut self, ty: &Type, resolve: &Resolve, names: &mut NameInterner) -> u32 {
        let Type::Id(id) = ty else {
            unreachable!("Tuple kind came from non-Id type")
        };
        let typedef = &resolve.types[*id];
        let wit_parser::TypeDefKind::Tuple(t) = &typedef.kind else {
            unreachable!("Tuple kind came from non-Tuple TypeDefKind")
        };
        let mut children = Vec::with_capacity(t.types.len());
        for elem_ty in &t.types {
            children.push(self.push(elem_ty, resolve, names));
        }
        self.push_cell(Cell::TupleOf { children })
    }

    /// Allocate the disc slot first, then recurse into the inner
    /// type — matches the canonical-ABI `[disc, ...flat(T)]` order.
    /// **No `push_arm` here**: option's payload slots are dedicated
    /// (not joined), and canonical-ABI lower zeroes them on `none`,
    /// so `option<list<T>>` runs `cabi_realloc(0)` + an empty loop
    /// — wasteful but correct. A guard would be load-bearing iff
    /// the slots were joined, which they aren't.
    fn push_option(&mut self, inner: &Type, resolve: &Resolve, names: &mut NameInterner) -> u32 {
        let disc_slot = self.bump_flat_slot();
        let child_idx = self.push(inner, resolve, names);
        self.push_cell(Cell::Option {
            disc_slot,
            child_idx,
        })
    }

    /// `result<T, E>`: bump disc, then walk both arms while sharing
    /// the same flat-slot range (the canonical-ABI joined layout has
    /// each post-disc slot serving both arms). The shared
    /// rewind/widening logic lives in [`Self::push_disc_arms`].
    ///
    /// Per-arm wasm-type mismatches against the joined are stamped
    /// into `slot_widening`; the emit phase bitcasts at leaf read.
    fn push_result(&mut self, ty: &Type, resolve: &Resolve, names: &mut NameInterner) -> u32 {
        let Type::Id(id) = ty else {
            unreachable!("Result kind came from non-Id type")
        };
        let wit_parser::TypeDefKind::Result(r) = &resolve.types[*id].kind else {
            unreachable!("Result kind came from non-Result TypeDefKind")
        };
        let r = r.clone();
        let joined = flat_types(resolve, ty, None)
            .expect("result<T, E> must flatten within MAX_FLAT_PARAMS");

        let disc_slot = self.bump_flat_slot();
        let arms_base = self.next_flat_slot;
        // Fixed arity: result has exactly 2 arms; force a release-mode
        // length check via try_into rather than blind indexing.
        let [ok_idx, err_idx]: [Option<u32>; 2] = self
            .push_disc_arms(disc_slot, arms_base, &joined, [r.ok, r.err], resolve, names)
            .try_into()
            .expect("push_disc_arms with 2-element input returns 2-element output");
        self.push_cell(Cell::Result {
            disc_slot,
            ok_idx,
            err_idx,
        })
    }

    /// Push an `ArmGuard` for the duration of `walk` so any
    /// `Cell::ListOf` pushed inside inherits the predicate.
    fn push_arm<R>(
        &mut self,
        disc_slot: u32,
        expected_disc: u32,
        walk: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.arm_guard_stack.push(ArmGuard {
            disc_slot,
            expected_disc,
        });
        let r = walk(self);
        self.arm_guard_stack.pop();
        r
    }

    /// Walk one variant / result arm's flat positions and stamp the
    /// joined wasm type onto any slot the arm widens. `arms_base` is
    /// the first arm-payload slot (joined position 1). No-op for
    /// empty / unit arms.
    fn record_arm_widening(
        &mut self,
        arm: Option<&Type>,
        arms_base: u32,
        joined: &[WasmType],
        resolve: &Resolve,
    ) {
        let Some(t) = arm else { return };
        let arm_flat =
            flat_types(resolve, t, None).expect("arm flat fits — joined fit, so arm fits");
        for (i, &arm_ty) in arm_flat.iter().enumerate() {
            let joined_ty = joined[1 + i];
            // Compare at wasm-level: `Pointer`/`Length` collapse to
            // I32, `PointerOrI64` to I64 — only the wasm type matters
            // for whether the leaf cell can read the slot directly.
            if wasm_type_to_val(arm_ty) != wasm_type_to_val(joined_ty) {
                self.set_widening(arms_base + i as u32, joined_ty);
            }
        }
    }

    /// `variant { ... }`: bump disc, walk each case's payload (if
    /// any) sharing the same flat-slot range — generalizes
    /// [`Self::push_result`] to N arms. Per-arm rewind/widening lives
    /// in [`Self::push_disc_arms`].
    fn push_variant(&mut self, ty: &Type, resolve: &Resolve, names: &mut NameInterner) -> u32 {
        let Type::Id(id) = ty else {
            unreachable!("Variant kind came from non-Id type")
        };
        let typedef = &resolve.types[*id];
        let wit_parser::TypeDefKind::Variant(v) = &typedef.kind else {
            unreachable!("Variant kind came from non-Variant TypeDefKind")
        };
        let v = v.clone();
        let info = variant_lift_info_for_type(ty, resolve)
            .expect("Variant kind implies variant-info available");
        let joined =
            flat_types(resolve, ty, None).expect("variant must flatten within MAX_FLAT_PARAMS");

        let disc_slot = self.bump_flat_slot();
        let arms_base = self.next_flat_slot;
        let per_case_payload = self.push_disc_arms(
            disc_slot,
            arms_base,
            &joined,
            v.cases.iter().map(|c| c.ty),
            resolve,
            names,
        );
        self.push_cell(Cell::Variant {
            disc_slot,
            per_case_payload,
            info,
        })
    }

    /// Walk N disc arms over a shared flat-slot range. Returns each
    /// arm's pushed cell index in disc order — `None` for unit arms.
    /// Updates `next_flat_slot` to the max-after-walking-any-arm so
    /// the parent's flat-slot count covers every arm.
    ///
    /// Per arm: rewind cursor to `arms_base`, walk under an
    /// [`ArmGuard`], stamp arm-vs-joined widening for any slot whose
    /// per-arm wasm type differs from the joined.
    fn push_disc_arms<I>(
        &mut self,
        disc_slot: u32,
        arms_base: u32,
        joined: &[WasmType],
        arms: I,
        resolve: &Resolve,
        names: &mut NameInterner,
    ) -> Vec<Option<u32>>
    where
        I: IntoIterator<Item = Option<Type>>,
    {
        let mut max_after = arms_base;
        let mut indices: Vec<Option<u32>> = Vec::new();
        for (disc, arm) in arms.into_iter().enumerate() {
            self.next_flat_slot = arms_base;
            let child_idx = self.push_arm(disc_slot, disc as u32, |b| {
                arm.map(|t| b.push(&t, resolve, names))
            });
            max_after = max_after.max(self.next_flat_slot);
            self.record_arm_widening(arm.as_ref(), arms_base, joined, resolve);
            indices.push(child_idx);
        }
        self.next_flat_slot = max_after;
        indices
    }

    /// `own<R>` / `borrow<R>` — single i32 (canonical-ABI handle).
    /// Anonymous resources fall back to "" for type-name.
    fn push_handle(
        &mut self,
        h: &wit_parser::Handle,
        resolve: &Resolve,
        names: &mut NameInterner,
    ) -> u32 {
        let resource_id = match h {
            wit_parser::Handle::Own(id) | wit_parser::Handle::Borrow(id) => *id,
        };
        let type_name = names.intern(resolve.types[resource_id].name.as_deref().unwrap_or(""));
        let flat_slot = self.bump_flat_slot();
        self.push_cell(Cell::Handle {
            flat_slot,
            type_name,
            kind: HandleKind::Resource,
        })
    }

    /// `stream<T>` / `future<T>` — single i32 (canonical-ABI handle).
    /// Type-name peels alias + Handle wrappers to find a named
    /// typedef (wit-parser auto-wraps `stream<my-res>` as
    /// `stream<own<my-res>>`); "" for primitives or unnamed chains.
    fn push_stream_or_future(
        &mut self,
        elem: Option<&Type>,
        kind: HandleKind,
        resolve: &Resolve,
        names: &mut NameInterner,
    ) -> u32 {
        let elem_name = elem
            .and_then(|t| match t {
                Type::Id(id) => Some(*id),
                _ => None,
            })
            .map(|id| {
                // Peel through alias / handle wrappers until a named
                // typedef appears or the chain dead-ends.
                // wit-parser implicitly wraps `stream<my-res>` as
                // `stream<own<my-res>>`, so a Handle hop is expected
                // when the element is a resource type.
                let mut tid = id;
                loop {
                    let td = &resolve.types[tid];
                    if let Some(name) = td.name.as_deref() {
                        return name;
                    }
                    match &td.kind {
                        wit_parser::TypeDefKind::Type(Type::Id(next)) => tid = *next,
                        wit_parser::TypeDefKind::Handle(
                            wit_parser::Handle::Own(next) | wit_parser::Handle::Borrow(next),
                        ) => tid = *next,
                        _ => return "",
                    }
                }
            })
            .unwrap_or("");
        let type_name = names.intern(elem_name);
        let flat_slot = self.bump_flat_slot();
        self.push_cell(Cell::Handle {
            flat_slot,
            type_name,
            kind,
        })
    }

    /// `list<T>` (non-u8) — `(ptr, len)` flat; element plan built in
    /// a fresh sub-builder so its slots are local to one element.
    /// Snapshots `arm_guard_stack` so emit can disc-gate the cell
    /// when the list lives inside a joined arm.
    fn push_list_of(&mut self, elem: &Type, resolve: &Resolve, names: &mut NameInterner) -> u32 {
        let list_idx = self.next_list_idx;
        self.next_list_idx += 1;
        let ptr_slot = self.bump_flat_slot();
        let len_slot = self.bump_flat_slot();
        let element_plan = match LiftPlan::for_type(elem, resolve, names) {
            Ok(plan) => plan,
            Err(err) => {
                self.record_error(err);
                LiftPlan::stub_for(*elem)
            }
        };
        if element_plan.cells.len() != 1 || !element_plan.cells[0].allowed_as_list_element() {
            self.record_error(anyhow!(
                "`list<T>` element type {elem:?} is not yet supported \
                 (only scalar element types are wired today: bool, integers, \
                 floats, string, list<u8>, enum). File a request at {ISSUES_URL} \
                 to bump priority."
            ));
        }
        let arm_guards = self.arm_guard_stack.clone();
        self.push_cell(Cell::ListOf {
            list_idx,
            ptr_slot,
            len_slot,
            element_plan: Box::new(element_plan),
            arm_guards,
        })
    }

    pub(super) fn into_plan(self, root: u32, source_ty: Type) -> LiftPlan {
        debug_assert_eq!(
            self.slot_widening.len() as u32,
            self.next_flat_slot,
            "slot_widening must mirror flat_slot_count (one entry per bump_flat_slot)",
        );
        LiftPlan {
            cells: self.cells,
            flat_slot_count: self.next_flat_slot,
            slot_widening: self.slot_widening,
            root,
            source_ty,
        }
    }
}

/// A type-name plus an ordered list of item names. Carries
/// enough info to populate any of the `*-info` side-table records
/// in `splicer:common/types` that share the `{ type-name, <item> }`
/// shape (enum-info's `case-name`, flags-info's `set-flags`,
/// eventually variant-info).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NamedListInfo {
    pub(super) type_name: String,
    /// Item names in WIT declaration order — the i'th entry's WIT
    /// declaration index equals `i` (matching the disc / bit-position
    /// / field-index used at runtime).
    pub(super) item_names: Vec<String>,
}

/// Extract `(type-name, item-names)` from a named TypeDef matching
/// `kind_extract`. The closure picks the per-item identifier list
/// off the `TypeDefKind` (case-names for enum / variant, flag-names
/// for flags). Returns `None` when the type isn't an `Id`, doesn't
/// match `kind_extract`, or lacks a name — in any of those cases the
/// runtime payload is meaningless without identifiers a reader would
/// render.
fn lift_info_for_type<F>(ty: &Type, resolve: &Resolve, kind_extract: F) -> Option<NamedListInfo>
where
    F: FnOnce(&wit_parser::TypeDefKind) -> Option<Vec<String>>,
{
    let Type::Id(id) = ty else {
        return None;
    };
    let typedef = &resolve.types[*id];
    let item_names = kind_extract(&typedef.kind)?;
    let type_name = typedef.name.as_ref()?.clone();
    Some(NamedListInfo {
        type_name,
        item_names,
    })
}

fn enum_lift_info_for_type(ty: &Type, resolve: &Resolve) -> Option<NamedListInfo> {
    lift_info_for_type(ty, resolve, |k| match k {
        wit_parser::TypeDefKind::Enum(e) => Some(e.cases.iter().map(|c| c.name.clone()).collect()),
        _ => None,
    })
}

fn variant_lift_info_for_type(ty: &Type, resolve: &Resolve) -> Option<NamedListInfo> {
    lift_info_for_type(ty, resolve, |k| match k {
        wit_parser::TypeDefKind::Variant(v) => {
            Some(v.cases.iter().map(|c| c.name.clone()).collect())
        }
        _ => None,
    })
}

fn flags_lift_info_for_type(ty: &Type, resolve: &Resolve) -> Option<NamedListInfo> {
    lift_info_for_type(ty, resolve, |k| match k {
        wit_parser::TypeDefKind::Flags(fl) => {
            Some(fl.flags.iter().map(|f| f.name.clone()).collect())
        }
        _ => None,
    })
}
