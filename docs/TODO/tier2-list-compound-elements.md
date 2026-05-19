# Tier-2 lift: outstanding payload-type extensions

Every other type lifts today (primitives, `string`, `list<u8>`,
`enum`, `record`, `tuple`, `option`, `result`, `flags`, `variant`,
`char`, `own<R>` / `borrow<R>`, `stream<T>` / `future<T>`,
`error-context`, `list<T, N>`, `list<T>` over every kind, and
`list<list<…>>` over every supported leaf kind at any depth).

## What's left

### `list<wrapper-with-list>` (list-in-wrapper inside another list)

The position cap in `push_list_of` (`lift/plan.rs`) rejects shapes
where a `Cell::ListOf` element-cell appears alongside other cells in
its parent list's element plan:

- `list<option<list<u32>>>`
- `list<tuple<u32, list<u32>>>`
- `list<record { ys: list<u32> }>`
- `list<variant { v(list<u32>), empty }>`
- `list<result<list<u32>, u32>>`
- `map<K, list<list<T>>>` (desugars to
  `list<tuple<K, list<list<T>>>>`, so falls under the same cap)

Emit's `NestedListLocals` (`lift/emit.rs`) hangs off a single
element-plan position — the `[Cell::ListOf]` match arm in
`build_list_emit_locals_for_plan`. To lift these shapes, that
singular `nested: Option<NestedListLocals>` becomes per-`Cell::ListOf`
locals (e.g. `Vec<NestedListLocals>` keyed by `list_idx`, or a
position-indexed map). Then the pre-pass spec iteration and
`emit_list_of_arm`'s element loop look up by cell position rather
than `nested_of(ll)`. No per-WIT-type matching needed — the cell
tree already represents any nested structure.

## Recently landed

- `list<list<…>>` depth ≥ 3 — `Cell::list_element_class` now returns
  `PrestagedNestedList` for any `Cell::ListOf`, so the cell tree
  recurses freely. The pre-pass data walk in `lift/emit.rs` splits
  into `emit_nested_list_pre_pass` (top-level entry — seeds outer
  cursors, loads outer.ptr from a flat slot) and a recursive
  `emit_pre_pass_data_walk` (two passes per level: bump globals by
  `inner.len * per_elem`, then if inner is itself a nested list,
  loop again to load per-element inner pointer + length and recurse).
  Cell-slab-overflow trap fires at every level. The `allowed_as_list_element`
  helper and `Option` wrapping on `list_element_class` are dropped
  since every Cell variant lifts. Position cap unchanged: a
  `Cell::ListOf` element-cell must still be the sole cell of its
  parent list's element plan. Validator-fixture entries
  `f-list-of-list-of-list-u32`, `-flags`, and
  `f-list-of-list-of-list-of-list-of-list-u32` plus canned shapes
  `list<list<list<u32>>>`, `list<list<list<fperms>>>`, and
  `list<list<list<list<list<u32>>>>>` cover the depth axis at scalar,
  per-kind-contributed, and depth-5.

- Nested-list cursor unification — `NestedListLocals`'s four flat
  `Option<u32>` cursor fields collapsed into a `NestedKindCursors`
  substruct mirroring `KindBuffers`, and a `nested_kind_rows()`
  helper now owns the single kind list. The three call sites
  (pre-pass seed, pre-pass walk-loop wrapper-counter advance,
  iter-start inner-slot snap, iter-end cursor advance) iterate
  `for row in nested_kind_rows(...)` instead of hand-aligning
  parallel 4-row tables. Pre-pass cursor seeds also moved inside
  the `if outer.len > 0` guard so the empty-outer fast path skips
  them. Adding a new kind now touches two sites instead of five:
  one row in `nested_kind_rows` and one field on `NestedKindCursors`
  (plus the existing `KindBuffers`).

- `list<list<handle>>` — `PrestagedHandle` joins the allowed inner
  classes. `NestedListLocals` gains `handle_cursor`;
  `emit_nested_list_pre_pass` bumps `lcl.next_handle_idx` by
  `Σ inner_len_j * inner.handle.count_per_elem`, and
  `emit_list_of_arm`'s nested arm snaps/advances
  `inner.handle.slot_base` from the cursor per outer iter. Drops
  into the existing per-kind cursor / snap / advance arrays
  (alongside flags / record / variant) as one more row. Test-
  harness fix: `consumer_pass_expr` + `Shape::List` rust_literal
  now recurse via `Shape::contains_resource()` so
  `list<list<own<R>>>` constructs handles in the outer mode (no
  spurious `.await`) and passes by value (no spurious `&`). Canned
  shape `list<list<own<cat>>>` joins `tier2_shapes()`.

- `list<list<flags>>` — `PrestagedFlags` joins the allowed inner
  classes. The refactor that landed alongside it collapsed the 12
  flat per-kind fields on `ListEmitLocals` (handle / flags / record
  / variant × `{slot_base, count_per_elem, buf_base, bytes_per_elem}`)
  into four `KindBuffers` substructures, so the nested-list per-kind
  loops (snap-and-advance in the pre-pass, snap-from-cursor at outer
  iter start, advance-by-len at outer iter end) iterate over a
  homogeneous `[(cursor, kb, label); N]` array. `NestedListLocals`
  gains `flags_cursor`; `emit_nested_list_pre_pass` bumps
  `lcl.next_flags_idx` by `Σ inner_len_j * inner.flags.count_per_elem`.
  Per-call flags-set-flags scratch (`inner.flags.buf_base`) stays
  per-outer-iter `cabi_realloc`'d inside the inner's
  `emit_list_of_arm` — no new wrapper-level allocation. Canned
  shapes: `list<list<fperms>>` (single-kind cursor flow) and
  `list<list<tuple<fperms, fcaps>>>` (3-bit / 5-bit mismatch pins
  cumulative `scratch_offset_in_elem` stride math, analogous to
  `list_list_variant_tagged-pair_fst`).

- `list<list<variant>>` — same shape as the record case below.
  `PrestagedVariant` joins the allowed inner classes; `ListEmitLocals`
  gains `nested_inner_variant_cursor`; `emit_nested_list_pre_pass`
  bumps `lcl.next_variant_idx` by
  `Σ inner_len_j * inner.variants_per_elem`, and `emit_list_of_arm`
  snaps/advances `inner.variant_slot_base` per outer iter.
  `has_list_elem_variant` already recursed via
  `any_list_element_has_class` (record's swap had set the shape). The
  per-block cursor-advance pattern got two helpers
  (`emit_cursor_advance_by_len`, `emit_snap_and_advance_cursor`) so
  the 11 existing sites — plus the future flags / handle additions —
  collapse to one-liners. Canned shape `list<list<shape>>` joins
  `tier2_shapes()`.

- `list<list<record>>` — `PrestagedRecord` joins the allowed inner
  classes. `ListEmitLocals` gains `nested_inner_record_cursor`;
  `emit_nested_list_pre_pass` bumps `lcl.next_record_idx` by
  `Σ inner_len_j * inner.records_per_elem`, and `emit_list_of_arm`
  snaps/advances `inner.record_slot_base` from the cursor per outer
  iter. `LiftPlan::has_list_elem_record` switched to the recursive
  `any_list_element_has_class(PrestagedRecord)` so the wrapper-level
  `record-info` buffer gets the runtime-sized alloc path. Canned
  shape `list<list<point>>` lives in `tier2_shapes()`; fixture
  `f-list-of-list-point` covers plan + emit validation.

- `list<list<T>>` for `T` ∈ scalar / char / option / result / tuple /
  enum — `Cell::ListOf` becomes an allowed list element (class
  `PrestagedNestedList`) when the inner element plan stays on the
  per-call-buffer-free subset (`lift/plan.rs`). Emit adds a nested
  pre-pass that walks outer's memory to size the cell slab
  (`emit_nested_list_pre_pass` in `lift/emit.rs`) and threads inner
  `ListEmitLocals` via `ListEmitLocals.nested_inner`. The inner
  list's `start_i` doubles as a running cursor — pre-pass seeds
  `outer.start_i + outer.len`, the outer loop advances per iter.
  `is_compound_result` already accepted `list<T>` non-u8, so no
  classify change. Canned shapes per inner T live in
  `tier2_shapes()`; gates fire on `f-map-of-list-of-list` (nested
  list inside a tuple-element list rejects via the depth-2 cap).

- `FixedLengthList(elem, N)` — canon-ABI flattens to `N × flat(T)`
  inlined, structurally identical to `tuple<T;N>`. Desugared at
  plan-build by `push_fixed_length_list` (`lift/plan.rs`), which
  walks the element N times and wraps in `Cell::TupleOf` — no
  new cell variant, no new emit, no new side-table, reuses every
  TupleOf code path (static cell-index array at top level,
  `PrestagedTupleIndices` as a list element).
  `is_compound_result` accepts the FixedLengthList kind so
  retptr'd results route through `lift_from_memory`, which natively
  emits `FixedLengthListLiftFromMemory` for the per-element loads.
  Pre-bails on `N == 0` (parseable in WIT, no canonical-ABI
  meaning) and `N > MAX_FLAT_SLOTS_PER_FN` / `MAX_CELLS_PER_PARAM`
  (single integer literal would otherwise overflow
  `bump_flat_slot`'s u32 counter).

  `Shape::FixedLengthList { inner, n }` lives in
  `tests/fuzz_and_run.rs` with match arms wired through. The
  predict-side renders as `tuple(<elem>, …, <elem>)` — matches
  `Cell::TupleOf`'s `fmt_cell` output character-for-character.
  `wasm_component_model_fixed_length_lists(true)` on the wasmtime
  `Config` is required for the engine to parse `list<T, N>`
  typedefs at all.

- `Map(K, V)` — canon-ABI ≡ `list<tuple<K, V>>`. Desugared at
  build time via `desugar_map_aliases` (`lift/plan.rs`), which
  allocs a synthetic `tuple<K, V>` typedef per Map and threads
  a `MapAliases` table into the plan builder. The plan-builder
  Map arm forwards to `push_list_of` with the synthetic tuple
  as element type — no new cell variant, no WIT changes, no
  runtime/middleware changes. `is_compound_result` accepts the
  Map kind. Synthetic tuples are unowned + unreferenced from
  any world, so `LiveTypes` keeps them out of embedded
  component metadata.

  `Shape::Map` + match arms are wired in `tests/fuzz_and_run.rs`,
  along with a `MapLift` no-op arm in `WasmEncoderBindgen`
  (`abi/bindgen.rs`) and a bump of the scaffold-side
  `wit-bindgen` to 0.57.1. End-to-end is currently blocked at
  `wac compose` — `wac-types 0.10.0` (latest) rejects
  `ComponentDefinedType::Map`. `test_tier2_map_blocked_on_wac`
  pins the failure mode; the day `wac` ships Map support, that
  test breaks (success becomes the new failure) and
  `Shape::Map { … }` moves into `tier2_shapes()`.

## Per-type workflow (for future kinds)

Five phases, each its own commit so review is small and bisect
stays narrow.

1. **Params** — plan-builder arm (`LiftPlanBuilder::push`, drop
   the matching `todo!()`), cell emit helper in `cells.rs`,
   emit-phase dispatch in `lift/emit.rs::emit_cell_op`,
   side-table builder under `lift/sidetable/` if needed (new
   per-cell maps go through [`PerCellIndices<T>`]). Tests:
   cell-emit unit test, plan-shape tests covering leaf /
   nested-in-record / nested-in-self, integration roundtrip.
2. **Self-review params** — walk every touched file: stale
   comments, edge cases (empty / single-element / zero-flat-slot),
   nested `Vec<Vec<…>>` without typed accessors, `expect()`s on
   new-path invariants.
3. **Results** — extend `is_compound_result` in `lift/classify.rs`.
   Multi-cell kinds take Compound; single-cell take Direct via
   `single_cell_for_result` + `is_supported_direct_result`. Pin
   the single-flat-slot fall-through (`tuple<u32>`,
   `record { a: u32 }`) at `result_at_retptr`.
4. **Self-review results** — same as phase 2 with attention to
   retptr + `lift_from_memory` + synth-locals. Runtime side-table
   growth (flags / variant / handle) needs extra care; grep for
   adapter-build-time-known cell-count assumptions.
5. **Canned shape** — new `Shape::<Kind>(…)` in `tier2_shapes()`
   (`tests/fuzz_and_run.rs`), `Cell::<Kind>(…)` arm in
   `MIDDLEWARE_TIER2_LIB_RS::fmt_cell`, `Shape::<Kind>` arm in
   `predict_tier2_arg_inner` matching the middleware's rendering
   character-for-character. Run via `cargo test --test
   fuzz_and_run test_tier2_canned -- --ignored` (filter with
   `SPLICER_RUNTIME_SHAPES=<name>`).

## Out of scope

- `Resource` / `Unknown` typedefs at payload position — both
  `unreachable!()` in `lift/plan.rs` (canon-ABI forbids bare
  resources; `Unknown` is wit-parser's unresolved sentinel).
- Direct-compound emit (lifting single-flat-slot compounds rather
  than dropping them to no-lift). Would let `tuple<u32>` and
  `record { a: u32 }` results show up in the after-hook; no use
  case has surfaced.

[`PerCellIndices<T>`]: ../../src/adapter/tier2/lift/sidetable/mod.rs
