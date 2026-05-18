//! Lift plan: flat Vec of [`Cell`]s in allocation order (children
//! before parents). Cells reference flat slots plan-relative; emit
//! supplies `local_base`. Side-table builders + emit walk the same
//! `cells` vec, so child indices can't desync. See
//! `docs/tiers/lift-codegen.md`.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use wit_parser::abi::WasmType;
use wit_parser::{
    Docs, Resolve, Span, Stability, Tuple, Type, TypeDef, TypeDefKind, TypeId, TypeOwner,
};

use super::super::super::abi::emit::{wasm_type_to_val, BlobSlice};
use super::super::super::abi::flat_types;
use super::super::blob::NameInterner;

const ISSUES_URL: &str = "https://github.com/ejrgilbert/splicer/issues";

/// One cell to write at a known cell-array index. Flat slots are
/// plan-relative; emit adds `local_base` for the absolute wasm-local.
///
/// **Joined-arm rule.** Cells inside a `result` / `variant` arm read
/// flat slots shared with sibling arms. Pure flat-slot writers emit
/// unconditionally — inactive-arm payloads land in cells the runtime
/// never reads, so the bytes are inert. Cells with side effects
/// beyond their own payload (today only [`Cell::ListOf`], whose
/// `(ptr, len)` feed `cabi_realloc` + an unbounded loop) must
/// disc-gate via `arm_guards`. Adding a side-effecting variant means
/// adding the same gate.
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
    /// `enum { ... }` → `cell::enum-case(u32)`.
    EnumCase {
        flat_slot: u32,
        type_name: BlobSlice,
        case_names: Vec<BlobSlice>,
    },
    /// `record { ... }` → `cell::record-of(u32)`. Children live
    /// elsewhere in the same plan; `fields` references them by
    /// `LiftPlan::cells` position.
    RecordOf {
        type_name: BlobSlice,
        /// `(field-name, child-cell-idx)` per field, in WIT order.
        fields: Vec<(BlobSlice, u32)>,
    },
    /// `tuple<...>` → `cell::tuple-of(list<u32>)`. `children` are
    /// plan-cell indices.
    TupleOf { children: Vec<u32> },
    /// `option<T>` → `cell::option-some(u32)` / `cell::option-none`.
    /// Flat `[i32 disc, ...flat(T)]`. The child cell is always
    /// emitted; canonical-ABI lower zeroes T's slots on `none`.
    ///
    /// `child_idx` is absolute at top level; inside an `element_plan`
    /// it's plan-relative — emit resolves as
    /// `PlanCursor::elem_cell_base + child_idx` per iteration.
    Option { disc_slot: u32, child_idx: u32 },
    /// `result<T, E>` → `cell::result-ok(option<u32>)` /
    /// `cell::result-err(option<u32>)`. Flat
    /// `[i32 disc, ...join(flat(T), flat(E))]`. `ok_idx` / `err_idx`
    /// are `None` for unit arms.
    ///
    /// **Load-bearing invariant.** Runtime gates on disc and **must
    /// not** follow the inactive index — inactive cells may hold
    /// garbage from joined slots, or (for disc-gated [`Cell::ListOf`])
    /// uninitialized `cabi_realloc` bytes.
    ///
    /// Top-level idx is absolute; element-plan idx is plan-relative
    /// (same convention as [`Cell::Option::child_idx`]).
    Result {
        disc_slot: u32,
        ok_idx: Option<u32>,
        err_idx: Option<u32>,
    },

    /// `flags { ... }` → `cell::flags-set(u32)`. Single i32 lift slot
    /// (canonical-ABI caps flags at 32 bits).
    Flags {
        flat_slot: u32,
        type_name: BlobSlice,
        flag_names: Vec<BlobSlice>,
    },
    /// `variant { ... }` → `cell::variant-case(u32)`. Flat
    /// `[disc, ...joined_flat_of_each_case]`. `per_case_payload[i]`
    /// is `Some(child_idx)` for cases with a payload, `None` for unit.
    /// Same load-bearing invariant as [`Cell::Result`].
    Variant {
        disc_slot: u32,
        per_case_payload: Vec<Option<u32>>,
        type_name: BlobSlice,
        case_names: Vec<BlobSlice>,
    },

    /// `own<R>` / `borrow<R>` / `stream<T>` / `future<T>` /
    /// `error-context` → `cell::*-handle(u32)`. Single i32 lift slot;
    /// `kind` picks the cell-disc.
    Handle {
        flat_slot: u32,
        type_name: BlobSlice,
        kind: HandleKind,
    },

    /// `list<T>` (non-u8; `list<u8>` fast-paths through `Cell::Bytes`)
    /// → `cell::list-of`. Flat `(i32 ptr, i32 len)`. `element_plan`
    /// is a NESTED [`LiftPlan`] with its own cell-index space.
    /// `list_idx` keys into the parallel `list_locals` array.
    ///
    /// `arm_guards` is non-empty when the list lives inside joined
    /// `result` / `variant` arm(s); the alloc pre-pass and per-list
    /// emit AND-stack them so inactive-arm bytes can't surface as `len`.
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

/// Which `cell::*-handle` variant a [`Cell::Handle`] emits. All four
/// share representation + side-table layout; only the cell-disc differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandleKind {
    /// `own<R>` / `borrow<R>` → `cell::resource-handle`.
    Resource,
    /// `stream<T>` → `cell::stream-handle`.
    Stream,
    /// `future<T>` → `cell::future-handle`.
    Future,
    /// `error-context` → `cell::error-context-handle`. Just-an-id
    /// rendering — cross-component error-context lift is broken in
    /// wasmtime (≤44, "very incomplete" per its own config docstring),
    /// so `error-context.debug-message` is unusable. Revisit when host
    /// catches up.
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
/// [`super::emit::elem_cell_side_data`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListElementClass {
    /// Reads only flat slots; folds to `CellSideData::None`.
    Scalar,
    /// Per-iteration utf-8 scratch from the per-list `cabi_realloc`.
    PrestagedChar,
    /// Cell payload carries a build-time-relative child cell-array
    /// index (Option, Result); emit resolves
    /// `elem_cell_base + relative_idx` per iteration.
    PrestagedChildIdx,
    /// TupleOf: per-call indices array, each iteration writes
    /// `[elem_cell_base + child_pos[i]]` into a `cabi_realloc`'d slot.
    PrestagedTupleIndices,
    /// `Cell::Handle`. Per-(fn, param | result) handle-info buffer is
    /// grown at runtime to fit `static_count + Σ_lists len * handles_per_elem`.
    PrestagedHandle,
    /// `Cell::Flags`. Two per-call buffers grow: flags-info entries
    /// (uniform stride) + set-flags scratch (per-cell variable stride).
    PrestagedFlags,
    /// `Cell::RecordOf`. β scope is scalar-field records — nested
    /// compound cells inside a list-element record stay gated.
    PrestagedRecord,
    /// `Cell::Variant`. Per-arm payload child indices resolve to
    /// `elem_cell_base + child_pos_in_elem` at the dispatch site.
    PrestagedVariant,
    /// `Cell::ListOf` — `list<list<T>>`. Inner classes are gated in
    /// the [`Cell::list_element_class`] arm. No further nesting.
    /// Inner `start_i` is a cursor staged off `outer.start_i + outer.len`.
    PrestagedNestedList,
}

impl Cell {
    /// Classify a cell shape as a `list<T>` element. `None` for kinds
    /// the lift codegen can't yet emit per-element. Exhaustive match.
    pub(crate) fn list_element_class(&self) -> Option<ListElementClass> {
        match self {
            Cell::Char { .. } => Some(ListElementClass::PrestagedChar),
            Cell::Option { .. } | Cell::Result { .. } => Some(ListElementClass::PrestagedChildIdx),
            Cell::TupleOf { .. } => Some(ListElementClass::PrestagedTupleIndices),
            Cell::Handle { .. } => Some(ListElementClass::PrestagedHandle),
            Cell::Flags { .. } => Some(ListElementClass::PrestagedFlags),
            Cell::RecordOf { .. } => Some(ListElementClass::PrestagedRecord),
            Cell::Variant { .. } => Some(ListElementClass::PrestagedVariant),
            Cell::Bool { .. }
            | Cell::IntegerSignExt { .. }
            | Cell::IntegerZeroExt { .. }
            | Cell::Integer64 { .. }
            | Cell::FloatingF32 { .. }
            | Cell::FloatingF64 { .. }
            | Cell::Text { .. }
            | Cell::Bytes { .. }
            | Cell::EnumCase { .. } => Some(ListElementClass::Scalar),
            // Inner cells must be sizeable by `emit_nested_list_pre_pass`.
            Cell::ListOf { element_plan, .. } => element_plan
                .cells
                .iter()
                .all(|c| {
                    matches!(
                        c.list_element_class(),
                        Some(ListElementClass::Scalar)
                            | Some(ListElementClass::PrestagedChar)
                            | Some(ListElementClass::PrestagedChildIdx)
                            | Some(ListElementClass::PrestagedTupleIndices)
                            | Some(ListElementClass::PrestagedHandle)
                            | Some(ListElementClass::PrestagedFlags)
                            | Some(ListElementClass::PrestagedRecord)
                            | Some(ListElementClass::PrestagedVariant)
                    )
                })
                .then_some(ListElementClass::PrestagedNestedList),
        }
    }

    /// Whether this cell shape is supported as a `list<T>` element.
    pub(crate) fn allowed_as_list_element(&self) -> bool {
        self.list_element_class().is_some()
    }
}

/// Plan for lifting one (param | result) into a cell tree. Cells in
/// allocation order — children before parents — so the parent can be
/// pushed fully constructed (no back-fill).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiftPlan {
    pub(super) cells: Vec<Cell>,
    /// Total flat-slot locals consumed; cells reference slots in
    /// `0..flat_slot_count` and emit adds `local_base`.
    pub flat_slot_count: u32,
    /// Per-flat-slot joined wasm type, recorded only when the joined
    /// differs from at least one arm's per-position type (slot inside
    /// a widening `result` / `variant`). Emit derives the per-leaf
    /// bitcast as `cast(joined, leaf_arm_ty)`.
    slot_widening: Vec<Option<WasmType>>,
    /// Index of the root cell — last-appended for compounds.
    root: u32,
    /// WIT type the plan was built from; drives `lift_from_memory`.
    pub source_ty: Type,
}

impl LiftPlan {
    /// Build a plan from a single WIT type. `names` interns every
    /// record type-name and field-name. Errors on unsupported shapes.
    pub(super) fn for_type(
        ty: &Type,
        resolve: &Resolve,
        names: &mut NameInterner,
        map_aliases: &MapAliases,
    ) -> Result<Self> {
        let mut builder = LiftPlanBuilder::new(map_aliases);
        let root = builder.push(ty, resolve, names);
        if let Some(err) = builder.error {
            return Err(err);
        }
        Ok(builder.into_plan(root, *ty))
    }

    pub(crate) fn cell_count(&self) -> u32 {
        self.cells.len() as u32
    }

    pub(crate) fn root(&self) -> u32 {
        self.root
    }

    /// Joined wasm type at `flat_slot` when inside a widening
    /// `result` / `variant` arm; `None` otherwise.
    pub(crate) fn widening_for(&self, flat_slot: u32) -> Option<WasmType> {
        self.slot_widening
            .get(flat_slot as usize)
            .copied()
            .flatten()
    }

    /// Walk every cell, including those nested in element plans.
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

    /// Any `Cell::Char` in the plan tree (top-level or in element plans).
    /// Drives wrapper-level allocation of the shared char-scratch local.
    pub(crate) fn contains_char(&self) -> bool {
        self.walk_cells_recursive()
            .iter()
            .any(|c| matches!(c, Cell::Char { .. }))
    }

    /// Any list (top-level or nested-list inner) has a `Cell::Handle`
    /// in its element plan. Picks the runtime-sized handle-info-buffer
    /// path over the static-count one.
    pub(crate) fn has_list_elem_handle(&self) -> bool {
        self.any_list_element_has_class(ListElementClass::PrestagedHandle)
    }

    /// Companion to [`has_list_elem_handle`] for `Cell::Flags`.
    pub(crate) fn has_list_elem_flags(&self) -> bool {
        self.any_list_element_has_class(ListElementClass::PrestagedFlags)
    }

    /// Companion to [`has_list_elem_handle`] for `Cell::RecordOf`.
    pub(crate) fn has_list_elem_record(&self) -> bool {
        self.any_list_element_has_class(ListElementClass::PrestagedRecord)
    }

    /// Companion to [`has_list_elem_handle`] for `Cell::Variant`.
    pub(crate) fn has_list_elem_variant(&self) -> bool {
        self.any_list_element_has_class(ListElementClass::PrestagedVariant)
    }

    /// Whether any list (top-level or nested-list inner) has a
    /// list-element cell of class `want`. Nested-list inner element
    /// cells contribute too — their per-call wrapper-locals are
    /// shared with top-level uses.
    pub(crate) fn any_list_element_has_class(&self, want: ListElementClass) -> bool {
        self.cells.iter().any(|c| {
            let Cell::ListOf { element_plan, .. } = c else {
                return false;
            };
            element_plan
                .cells
                .iter()
                .any(|inner| inner.list_element_class() == Some(want))
                || element_plan.any_list_element_has_class(want)
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

    /// Every `Cell::ListOf` in plan-cells order.
    pub(crate) fn list_specs(&self) -> impl Iterator<Item = ListSpec<'_>> + '_ {
        self.cells.iter().filter_map(|op| match op {
            Cell::ListOf {
                list_idx,
                ptr_slot,
                len_slot,
                element_plan,
                arm_guards,
            } => Some(ListSpec {
                list_idx: *list_idx,
                ptr_slot: *ptr_slot,
                len_slot: *len_slot,
                element_plan,
                arm_guards,
            }),
            _ => None,
        })
    }
}

/// Per-`Cell::ListOf` view used by alloc + emit. `ptr_slot` is read
/// by the nested-list pre-pass to walk outer's data in memory; other
/// callers ignore it.
#[derive(Clone, Copy)]
pub(crate) struct ListSpec<'a> {
    pub list_idx: u32,
    pub ptr_slot: u32,
    pub len_slot: u32,
    pub element_plan: &'a LiftPlan,
    /// Empty unless the list lives inside joined `result` / `variant` arm(s).
    pub arm_guards: &'a [ArmGuard],
}

// ─── Map → list<tuple<K, V>> desugaring ───────────────────────────

/// Synthetic `tuple<K, V>` typedef per `map<K, V>` in the resolve.
/// Lift treats `map<K, V>` as canon-ABI shorthand for
/// `list<tuple<K, V>>` (same flat `(ptr, len)`, same element memory
/// layout), so the plan builder routes Map through `Cell::ListOf`
/// with the synthetic tuple as the element type. The original Map
/// typedef is left untouched — wit-bindgen-core's lower path still
/// sees `TypeDefKind::Map(K, V)` and emits `MapLower` as before
/// (which `src/adapter/abi/bindgen.rs` collapses to `(ptr, len)`).
///
/// Synthetic tuples are unnamed + unowned and unreferenced from any
/// world / interface / function, so `LiveTypes` excludes them from
/// embedded component metadata.
/// Constructed only by [`desugar_map_aliases`]; the
/// `LiftPlanBuilder` Map arm relies on the invariant that every
/// reachable `Map(K, V)` typedef has a registered entry, so we do
/// not expose a public empty-constructor (`Default`) escape hatch.
#[derive(Clone, Debug)]
pub(crate) struct MapAliases {
    map_to_tuple: HashMap<TypeId, TypeId>,
}

impl MapAliases {
    /// Synthetic tuple TypeId for a `Map(K, V)`. Panics if the
    /// caller skipped [`desugar_map_aliases`] for this resolve —
    /// the invariant is built-time and cheaper to enforce here
    /// than at every call site.
    fn tuple_for(&self, map_id: TypeId) -> TypeId {
        *self
            .map_to_tuple
            .get(&map_id)
            .expect("desugar_map_aliases must run before classify for any Map typedef")
    }
}

/// Allocate a synthetic `tuple<K, V>` typedef for every `Map(K, V)`
/// in `resolve` and return the Map-id → tuple-id mapping. Call once
/// per resolve before classify.
pub(crate) fn desugar_map_aliases(resolve: &mut Resolve) -> MapAliases {
    let map_pairs: Vec<(TypeId, Type, Type)> = resolve
        .types
        .iter()
        .filter_map(|(id, td)| match &td.kind {
            TypeDefKind::Map(k, v) => Some((id, *k, *v)),
            _ => None,
        })
        .collect();

    let mut aliases = MapAliases {
        map_to_tuple: HashMap::new(),
    };
    for (map_id, k, v) in map_pairs {
        let tuple_id = resolve.types.alloc(TypeDef {
            name: None,
            kind: TypeDefKind::Tuple(Tuple { types: vec![k, v] }),
            owner: TypeOwner::None,
            docs: Docs::default(),
            stability: Stability::default(),
            span: Span::default(),
        });
        aliases.map_to_tuple.insert(map_id, tuple_id);
    }
    aliases
}

// ─── Lift plan builder ────────────────────────────────────────────

/// Allocates cells + plan-relative flat-slot positions while walking
/// a WIT type. Children-before-parent recursion, so the parent cell
/// is immutable as soon as it lands in `cells`.
pub(super) struct LiftPlanBuilder<'a> {
    cells: Vec<Cell>,
    next_flat_slot: u32,
    /// Per-flat-slot joined wasm type for widening inside
    /// variant / result arms. Grows lazily — arms rewinding
    /// `next_flat_slot` don't double-grow the table.
    slot_widening: Vec<Option<WasmType>>,
    next_list_idx: u32,
    /// Active arm guards while walking joined `result` / `variant`
    /// arms; outer→inner. `Cell::ListOf` clones this snapshot.
    arm_guard_stack: Vec<ArmGuard>,
    /// First error hit during the walk.
    error: Option<anyhow::Error>,
    /// `map<K, V>` → synthetic `tuple<K, V>` typedef ids. Looked up
    /// in `push` when a Map kind appears; the build must have run
    /// `desugar_map_aliases` first.
    map_aliases: &'a MapAliases,
}

impl<'a> LiftPlanBuilder<'a> {
    pub(super) fn new(map_aliases: &'a MapAliases) -> Self {
        Self {
            cells: Vec::new(),
            slot_widening: Vec::new(),
            next_flat_slot: 0,
            next_list_idx: 0,
            arm_guard_stack: Vec::new(),
            error: None,
            map_aliases,
        }
    }

    /// First error wins; the walk continues with stub cells.
    fn record_error(&mut self, err: anyhow::Error) {
        if self.error.is_none() {
            self.error = Some(err);
        }
    }

    /// Push cells for one lift; returns the root cell's index. Type
    /// aliases peel through and reclassify the underlying type.
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
                    let type_name = names.intern(&info.type_name);
                    let case_names = info.item_names.iter().map(|n| names.intern(n)).collect();
                    let flat_slot = self.bump_flat_slot();
                    self.push_cell(Cell::EnumCase {
                        flat_slot,
                        type_name,
                        case_names,
                    })
                }
                wit_parser::TypeDefKind::Record(_) => self.push_record(ty, resolve, names),
                wit_parser::TypeDefKind::Tuple(_) => self.push_tuple(ty, resolve, names),
                wit_parser::TypeDefKind::Type(t) => self.push(t, resolve, names),
                wit_parser::TypeDefKind::List(elem) => self.push_list_of(elem, resolve, names),
                wit_parser::TypeDefKind::Variant(_) => self.push_variant(ty, resolve, names),
                wit_parser::TypeDefKind::Flags(_) => {
                    let info = flags_lift_info_for_type(ty, resolve)
                        .expect("Flags kind implies flags-info available");
                    let type_name = names.intern(&info.type_name);
                    let flag_names = info.item_names.iter().map(|n| names.intern(n)).collect();
                    let flat_slot = self.bump_flat_slot();
                    self.push_cell(Cell::Flags {
                        flat_slot,
                        type_name,
                        flag_names,
                    })
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
                wit_parser::TypeDefKind::Resource => {
                    unreachable!(
                        "tier-2 lift: bare `Resource` at payload position is \
                         forbidden by canonical ABI"
                    )
                }
                wit_parser::TypeDefKind::Unknown => {
                    unreachable!("tier-2 lift: unresolved `Unknown` typedef")
                }
                wit_parser::TypeDefKind::Map(_, _) => {
                    // Canon-ABI: `map<K, V>` ≡ `list<tuple<K, V>>` at the
                    // wire. Route through the existing list-of-tuple path
                    // using the synthetic tuple alias registered by
                    // `desugar_map_aliases`.
                    let tuple_id = self.map_aliases.tuple_for(*id);
                    self.push_list_of(&Type::Id(tuple_id), resolve, names)
                }
                wit_parser::TypeDefKind::FixedLengthList(elem, n) => {
                    // Canon-ABI: `list<T, N>` flattens to `N × flat(T)`
                    // inlined — same flat shape as `tuple<T, T, …, T>`.
                    // Desugar at plan-build: walk the element N times,
                    // wrap in `Cell::TupleOf`. Reuses every TupleOf code
                    // path (static cell-index array at top level,
                    // `PrestagedTupleIndices` as a list element).
                    let elem = *elem;
                    let n = *n;
                    self.push_fixed_length_list(&elem, n, resolve, names)
                }
            },
        }
    }

    fn bump_flat_slot(&mut self) -> u32 {
        let r = self.next_flat_slot;
        self.next_flat_slot = self
            .next_flat_slot
            .checked_add(1)
            .expect("LiftPlanBuilder flat-slot counter overflowed u32");
        // Variant / result arms rewind to share slots; only extend the
        // widening table at a new high-water mark (preserves earlier-arm entries).
        if self.slot_widening.len() < self.next_flat_slot as usize {
            self.slot_widening.push(None);
        }
        r
    }

    /// Record the joined-flat type at `flat_slot`. Idempotent across
    /// arms — joined is structural over the parent type.
    fn set_widening(&mut self, flat_slot: u32, joined_ty: WasmType) {
        debug_assert!(
            (flat_slot as usize) < self.slot_widening.len(),
            "set_widening called for flat_slot {flat_slot} before bump_flat_slot reached it \
             (slot_widening len = {})",
            self.slot_widening.len(),
        );
        // Multi-arm overwrites expected; pin that they agree.
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

    /// Recurse on each field, then push the parent referencing the
    /// now-known child indices (no back-fill).
    fn push_record(&mut self, ty: &Type, resolve: &Resolve, names: &mut NameInterner) -> u32 {
        let Type::Id(id) = ty else {
            unreachable!("Record kind came from non-Id type")
        };
        let typedef = &resolve.types[*id];
        let wit_parser::TypeDefKind::Record(r) = &typedef.kind else {
            unreachable!("Record kind came from non-Record TypeDefKind")
        };
        let type_name = names.intern(typedef.name.as_deref().unwrap_or(""));
        let mut fields = Vec::with_capacity(r.fields.len());
        for field in &r.fields {
            let name_slice = names.intern(&field.name);
            let child_idx = self.push(&field.ty, resolve, names);
            fields.push((name_slice, child_idx));
        }
        self.push_cell(Cell::RecordOf { type_name, fields })
    }

    /// Same as `push_record` minus type/field names — `tuple<...>` is anonymous.
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
        // WIT grammar forbids 0-tuples; pin it here so the
        // list-element tuple-idx-buffer codegen (which divides by
        // children-count) can't fire opaquely on a malformed plan.
        // Same invariant guards `push_fixed_length_list`'s `N >= 1`
        // bail — both arms terminate in this constructor.
        debug_assert!(
            !children.is_empty(),
            "Cell::TupleOf must have ≥1 child — WIT forbids 0-tuples",
        );
        self.push_cell(Cell::TupleOf { children })
    }

    /// `list<T, N>` desugar: walk `T` N times, wrap in `Cell::TupleOf`.
    /// Canon-ABI gives `list<T, N>` the same flat shape as `tuple<T;N>`
    /// (N copies of `flat(T)` inlined), so emit / layout / classify all
    /// reuse the tuple path. Pre-bails on `N == 0` (parseable in WIT
    /// grammar, no canonical-ABI meaning) and `N` above the layout
    /// budget (would overflow `bump_flat_slot`'s u32 counter for a
    /// single integer literal). The stub plan is never observed —
    /// `for_type` returns `Err` whenever `record_error` fires.
    fn push_fixed_length_list(
        &mut self,
        elem: &Type,
        n: u32,
        resolve: &Resolve,
        names: &mut NameInterner,
    ) -> u32 {
        if n == 0 {
            self.record_error(anyhow!(
                "`list<T, N>` requires N >= 1; got N = 0. File a request at \
                 {ISSUES_URL} if a zero-length fixed list is intentional."
            ));
            return self.stub_fixed_length_list();
        }
        if n > super::super::layout::MAX_FLAT_SLOTS_PER_FN
            || n > super::super::layout::MAX_CELLS_PER_PARAM
        {
            self.record_error(anyhow!(
                "`list<T, N>` with N = {n} exceeds tier-2 layout budget \
                 (MAX_FLAT_SLOTS_PER_FN = {}, MAX_CELLS_PER_PARAM = {}). \
                 File a request at {ISSUES_URL} if this size is intentional.",
                super::super::layout::MAX_FLAT_SLOTS_PER_FN,
                super::super::layout::MAX_CELLS_PER_PARAM,
            ));
            return self.stub_fixed_length_list();
        }
        let mut children = Vec::with_capacity(n as usize);
        for _ in 0..n {
            children.push(self.push(elem, resolve, names));
        }
        self.push_cell(Cell::TupleOf { children })
    }

    /// Placeholder TupleOf for the `record_error`-then-stub paths in
    /// [`Self::push_fixed_length_list`]. Pushes a `Cell::Bool` leaf
    /// referencing slot 0 (which always exists) so the returned cell
    /// index is a valid TupleOf, even though `for_type` bails before
    /// any caller reads it.
    fn stub_fixed_length_list(&mut self) -> u32 {
        let stub = self.push_cell(Cell::Bool { flat_slot: 0 });
        self.push_cell(Cell::TupleOf {
            children: vec![stub],
        })
    }

    /// Disc slot then inner type — canonical-ABI `[disc, ...flat(T)]`.
    /// No `push_arm`: option's payload slots are dedicated (not joined)
    /// and lower zeroes them on `none`, so `option<list<T>>` runs
    /// `cabi_realloc(0)` + empty loop — wasteful but correct.
    fn push_option(&mut self, inner: &Type, resolve: &Resolve, names: &mut NameInterner) -> u32 {
        let disc_slot = self.bump_flat_slot();
        let child_idx = self.push(inner, resolve, names);
        self.push_cell(Cell::Option {
            disc_slot,
            child_idx,
        })
    }

    /// `result<T, E>`: disc slot + both arms over a shared flat-slot
    /// range. Per-arm/joined wasm-type mismatches stamp `slot_widening`.
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
        // Result has exactly 2 arms; release-mode length check via try_into.
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

    /// Push an `ArmGuard` for the duration of `walk` so nested
    /// `Cell::ListOf`s inherit the predicate.
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

    /// Stamp the joined wasm type onto any slot this arm widens.
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
            // Compare at wasm-level: Pointer/Length→I32, PointerOrI64→I64.
            if wasm_type_to_val(arm_ty) != wasm_type_to_val(joined_ty) {
                self.set_widening(arms_base + i as u32, joined_ty);
            }
        }
    }

    /// `variant { ... }`: generalizes `push_result` to N arms.
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
        let type_name = names.intern(&info.type_name);
        let case_names = info.item_names.iter().map(|n| names.intern(n)).collect();
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
            type_name,
            case_names,
        })
    }

    /// Walk N disc arms over a shared flat-slot range. Per arm:
    /// rewind cursor to `arms_base`, walk under an `ArmGuard`, stamp
    /// arm-vs-joined widening. `next_flat_slot` ends at the max
    /// across arms so the parent covers them all.
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

    /// `own<R>` / `borrow<R>` — single i32 handle. Anonymous → "".
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

    /// `stream<T>` / `future<T>` — single i32. Type-name peels
    /// alias + Handle wrappers (wit-parser auto-wraps `stream<my-res>`
    /// as `stream<own<my-res>>`); "" for primitives or unnamed chains.
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
                // Peel through alias / handle wrappers to a named typedef.
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
    fn push_list_of(&mut self, elem: &Type, resolve: &Resolve, names: &mut NameInterner) -> u32 {
        let list_idx = self.next_list_idx;
        self.next_list_idx += 1;
        let ptr_slot = self.bump_flat_slot();
        let len_slot = self.bump_flat_slot();
        let element_plan = match LiftPlan::for_type(elem, resolve, names, self.map_aliases) {
            Ok(plan) => plan,
            Err(err) => {
                self.record_error(err);
                LiftPlan::stub_for(*elem)
            }
        };
        let per_cell_ok = element_plan
            .cells
            .iter()
            .all(|c| c.allowed_as_list_element());
        // Depth-2 cap: a `Cell::ListOf` may only appear as a list-element
        // when it's the *sole* cell of the parent list's element plan
        // (i.e. outer element type is itself a `list<T>`). Wrapping a
        // `list<T>` in option / tuple / record / variant / result inside
        // another list violates the cap — emit's `nested_inner` lives on
        // a single element-plan position and would panic without this
        // gate. Tested by `nested_list_under_wrapper_bails_at_plan_build`.
        let nested_position_ok = !element_plan
            .cells
            .iter()
            .any(|c| matches!(c, Cell::ListOf { .. }))
            || matches!(element_plan.cells.as_slice(), [Cell::ListOf { .. }]);
        if !(per_cell_ok && nested_position_ok) {
            self.record_error(anyhow!(
                "`list<T>` element type {elem:?} contains a cell shape that \
                 isn't yet supported as a list element (allowed today: bool, \
                 integers, floats, string, list<u8>, enum, char, option, \
                 result, tuple, flags, record, variant, \
                 own/borrow/stream/future/error-context handles, plus \
                 `list<list<T>>` where the inner `T` stays on scalars / \
                 char / option / result / tuple / enum / record — with \
                 allowed inner cells throughout). Still gated: \
                 `list<list<T>>` whose inner element needs a per-call \
                 info buffer for variant / flags / handle, further \
                 nesting, or a nested-list wrapped in option / tuple / \
                 record / variant / result. File a request at \
                 {ISSUES_URL} to bump priority."
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

/// Type-name + ordered item names. Populates any `*-info` side-table
/// record sharing the `{ type-name, <item> }` shape (enum, flags,
/// variant).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NamedListInfo {
    pub(super) type_name: String,
    /// Item names in WIT declaration order (matches runtime disc /
    /// bit-position / field-index).
    pub(super) item_names: Vec<String>,
}

/// Extract `(type-name, item-names)` from a named TypeDef matching
/// `kind_extract`. `None` if not an `Id`, no match, or unnamed — the
/// runtime payload is meaningless without identifiers.
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
