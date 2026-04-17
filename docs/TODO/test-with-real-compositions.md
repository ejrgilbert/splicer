# Composition shapes to fuzz / test against real compositions

`tests/component-interposition` covers a specific set of shapes today
(sync / async primitives, strings, and one HTTP-handler-shaped async
function with a single heterogeneous result). That's a narrow slice
of what splicer will encounter in the wild. This file tracks the
space of shapes we should exercise before trusting the tier-1
adapter against arbitrary real-world interfaces.

Each bullet is a shape that either (a) the adapter handles correctly
today but isn't covered by an end-to-end test, or (b) the adapter
has a known limitation for. Mark items `[x]` when a real
composition + runtime test exists.

## Why fuzz?

The correctness bugs we've uncovered so far (FlatLayout subword
offsets, variant-arm heterogeneity, `needs_realloc` missing lists)
all came from shapes outside our existing test suite. The bugs that
are still latent — and the ones we haven't imagined yet — are
hiding in shapes we don't exercise. Until we have a broader matrix,
every new real-world composition is a potential foot-gun.

## Type-shape matrix (per-function result type)

### Primitives and primitive-adjacent
- [x] `s32` / `u32` / `f32` (sync result)
- [x] `s64` / `u64` / `f64` (sync result)
- [ ] `bool` / `u8` / `s8` (subword — async result)
- [ ] `u16` / `s16` / `char` (subword — async result)
- [ ] `bool` / `u8` / `s8` as RECORD FIELDS (validate subword offset maths)
- [x] `string` (sync return, retptr pattern)
- [x] `string` (async return)
- [x] `list<primitive>` param + result (exercises `has_lists` needs_realloc)
- [ ] `list<string>` (nested variable-length)
- [ ] `list<list<T>>` (doubly-nested)
- [ ] `list<T, N>` (fixed-size — currently silently degraded; correctness bug)

### Discriminated types (the interesting ones)
- [x] `result<own<T>, variant>` (wasi:http/handler shape — via
  component-interposition `single` test; only the ok arm is
  exercised in that test, err arms trap via the heterogeneity
  guard)
- [ ] `result<T, E>` where T, E are BOTH subword (currently uses
  homogeneous canonical walk; should work but untested)
- [ ] `result<T, E>` heterogeneous → exercise EACH err arm at
  runtime (requires a runtime-dispatch implementation of
  `emit_task_return_loads`; currently traps for err arms)
- [ ] `option<u8>` / `option<u16>` (subword payload — canonical
  offsets 1 / 2)
- [ ] `option<record>` and `option<variant>` (nested compound
  inside a discriminator)
- [ ] Top-level `variant` result with many cases (≤256 — 1-byte
  disc)
- [ ] `variant` with >256 cases (multi-byte disc; currently
  untested)
- [ ] `variant` with heterogeneous arm widths (i32 vs i64 arms —
  this is the wasi:http error-code shape)
- [ ] `enum` with ≤8 / ≤16 / ≤256 / >256 cases (subword disc
  widths)

### Compounds
- [ ] `record { a: u8, b: u32, c: u16, d: u64 }` (mixed-alignment
  fields — stresses field padding)
- [ ] `tuple<u8, u32>` (same as above in tuple form)
- [ ] `record` with a `list` field
- [ ] `record` nested inside `variant` case (the DNS-error-payload
  pattern)
- [ ] `tuple` with resource handles
- [ ] Empty `record` / empty `tuple` (zero-sized)

### Resources and async handles
- [x] Single `own<T>` param + result (wasi:http/handler)
- [ ] Multiple resources of different types (e.g., `request`,
  `response`, `headers`)
- [ ] `borrow<T>` param (read-only resource access)
- [ ] `own<T>` inside a `variant` case (resource in compound)
- [ ] `future<T>` / `stream<T>` (async handle types — currently
  unsupported)

### Flags
- [ ] 0 labels (edge case)
- [ ] 1-8 labels (1-byte storage)
- [ ] 9-16 labels (2-byte storage)
- [ ] 17-32 labels (4-byte storage, single i32 slot)
- [ ] 33+ labels (multi-i32-word storage)

### Boundary and stress cases
- [ ] Function with >16 flat params (forces pointer-form lowering —
  currently unimplemented; validation fails at link time)
- [ ] Function with >16 flat results (same on task.return side)
- [ ] Function with 0 params, 0 results (minimal)
- [ ] Deeply nested compound (e.g., 5+ levels of record/variant)

## Composition shapes (per splice config)

### Coverage we have
- [x] single consumer split (`single`)
- [x] chain (`chain`)
- [x] nested (`nested`)
- [x] fanin (`fanin`)
- [x] sync primitives (adder)
- [x] async void with string (printer)

### Missing
- [ ] `fanin-all1` / `fanin-allN` / `fanin1` / `faninN` variants
  across non-trivial result types
- [ ] Mixed sync/async middleware on a single provider
- [ ] Blocking middleware (`should-block-call`) with a non-void
  handler — currently rejected; test the rejection path
- [ ] Adapter chain >3 deep
- [ ] Multiple splicer rules on overlapping interface sets
- [ ] Middleware whose target interface contains subword types
- [ ] Middleware on a provider that exports a variant-heavy interface

## Known latent bugs this matrix would surface

- **Subword field offsets in records/tuples**: needs a test with
  `record { u8, u32, u16 }` as a result type. Consumer WAT template
  would need to pre-export compound types via `(eq N)` — see
  `docs/adapter-comp-planning.md`.
- **Heterogeneous variant arms beyond ok (disc=0)**: currently
  traps. Fuzzing err-arm-producing handlers would force the
  runtime-dispatch implementation.
- **>16 flat values**: function with a 20-field record param would
  fail at link time. Build a test to assert the failure mode is
  clear (panic at codegen vs silent wrong output).
- **FixedSizeList**: a `list<u32, 4>` result currently degrades to
  dynamic `list<u32>`. Test would catch.

## Fuzzing harness ideas

- Generate WIT interfaces procedurally (bounded type grammar) and
  run the full splicer → wac compose → validate → run pipeline on
  each. Check that either validation succeeds and runtime matches
  expected, or generation fails with a clear error.
- Use `wit-parser` + `proptest` to build this.
- Treat any combination producing silent-wrong-output as a bug to
  fix.
