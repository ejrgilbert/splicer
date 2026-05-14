# Tier-2 lift: outstanding payload-type extensions

Every other type lifts today (primitives, `string`, `list<u8>`,
`enum`, `record`, `tuple`, `option`, `result`, `flags`, `variant`,
`char`, `own<R>` / `borrow<R>`, `stream<T>` / `future<T>`,
`error-context`, `list<T, N>`, and `list<T>` over every kind
except nested lists).

## What's left

### `list<list<T>>` (tier-3, list-element gate)

The only gated list-element kind. `Cell::allowed_as_list_element`
(`lift/plan.rs`) is the gate; the bail fires at `push_list_of` and
is pinned by `nested_list_bails_at_plan_build` in `lift/tests.rs`.

Child indices live in static side-table segments that assume
build-time-known absolute indices. Lifting needs either per-call
dynamic side-table growth (the route in use for `list<own<R>>` /
`list<record>` / `list<variant>` etc. via per-call info buffers)
or a schema-level "template + per-instance base." Multi-day
recursive-design pass — settle the design before promoting.

## Recently landed

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
