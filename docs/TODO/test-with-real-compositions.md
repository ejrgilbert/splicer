# Composition shapes — test coverage tracker

Coverage map for the tier-1 adapter. Each bullet is a shape the
adapter should handle, marked:

- `[x]` — integration test exists (in `src/adapter/tests.rs`) or
  runtime test exists (in `tests/component-interposition/`).
- `[-]` — covered structurally (adapter generates + validates)
  but not exercised at runtime.
- `[ ]` — not covered; candidate for new coverage.

The correctness bugs surfaced so far — subword-offset math,
variant-arm heterogeneity, `needs_realloc` missing lists,
`Pointer`/`PointerOrI64` widening — all came from shapes outside
an earlier version of this test suite. Shapes still marked `[ ]`
are where the next class of silent bugs is hiding.

A procedurally-generated fuzz harness that mechanizes this matrix
is tracked under "Built-in middleware keyword" / fuzzing in
[`docs/TODO/adapter-comp-planning.md`](./adapter-comp-planning.md).

## Type-shape matrix (per-function result type)

### Primitives and primitive-adjacent
- [x] `s32` / `u32` / `f32` (sync result)
- [x] `s64` / `u64` / `f64` (sync result)
- [x] `bool` / `u8` / `s8` (subword — async result,
  via `test_adapter_option_u8_async_result` family)
- [ ] `u16` / `s16` / `char` (subword — async result)
- [x] `string` (sync return, retptr pattern)
- [x] `string` (async return)
- [x] `list<primitive>` param + result (exercises `has_lists` needs_realloc)
- [ ] `list<string>` (nested variable-length)
- [ ] `list<list<T>>` (doubly-nested)
- [x] `list<T, N>` (fixed-size — structural coverage via
  `test_adapter_fixed_size_list_param_sync` + unit test in
  `abi/bindgen.rs`'s `lift_fixed_size_list_unrolls_n_loads`)

### Discriminated types (the interesting ones)
- [x] `result<own<T>, variant>` (wasi:http/handler shape — runtime via
  `tests/component-interposition`)
- [ ] `result<T, E>` where T, E are BOTH subword (homogeneous
  canonical walk; should work but untested)
- [-] `result<T, E>` heterogeneous → structural via
  `test_adapter_result_u8_u8_async_result` +
  `test_adapter_heterogeneous_numeric_variant_async_result`.
  Runtime exercise of EACH err arm (not just disc=0) still missing;
  needs a handler component that returns non-ok arms at runtime.
- [x] `option<u8>` (subword payload — via
  `test_adapter_option_u8_async_result`)
- [x] `option<u16>` (subword payload — via
  `test_adapter_option_u16_async_result`)
- [ ] `option<record>` and `option<variant>` (nested compound
  inside a discriminator)
- [ ] Top-level `variant` result with many cases in the u8-disc
  range (≤256)
- [x] `variant` with >256 cases — u8 → u16 disc width transition
  (`test_adapter_variant_over_256_cases_async_result`)
- [x] `variant` with heterogeneous arm widths
  (`test_adapter_heterogeneous_numeric_variant_async_result` —
  exercises `cast(I32, I64)` + `cast(F64, I64)`;
  wasi:http `__testme` exercises `cast(Pointer, PointerOrI64)`)
- [x] `enum` (via `test_adapter_enum_async_result`)
- [ ] `enum` with >256 cases (subword disc widths)

### Compounds
- [x] `record { u8, u32, u16, u64 }` — mixed-alignment fields
  (`test_adapter_mixed_alignment_record_async_result`)
- [ ] `tuple<u8, u32>` (same as above in tuple form — tuples go
  through the same codepaths as records in the Bindgen, so this
  is lower-risk)
- [ ] `record` with a `list` field
- [x] `record` nested inside `variant` case (the DNS-error-payload
  pattern — indirectly via wasi:http `__testme` which has this
  shape)
- [ ] `tuple` with resource handles
- [ ] Empty `record` / empty `tuple` (zero-sized)

### Resources and async handles
- [x] Single `own<T>` param + result (wasi:http/handler)
- [ ] Multiple resources of different types (e.g., `request`,
  `response`, `headers`) — wasi:http is close but all handles flow
  through the same `handle` function
- [ ] `borrow<T>` param (read-only resource access)
- [ ] `own<T>` inside a `variant` case (resource in compound)
- [ ] `future<T>` / `stream<T>` (async handle types — currently
  unsupported at the adapter layer)

### Flags
- [x] 1 label (`test_adapter_flags_1_label_async_result`)
- [x] 8 labels — 1-byte storage (`test_adapter_flags_8_labels_async_result`)
- [x] 16 labels — 2-byte storage (`test_adapter_flags_16_labels_async_result`)
- [x] 32 labels — 4-byte storage
  (`test_adapter_flags_32_labels_async_result`)

The Component Model binary format caps `flags` at 32 members, so
the matrix stops there. wit-parser and the canonical-ABI spec
describe a multi-word encoding (`FlagsRepr::U32(n)`) for 33+
flags, but that encoding isn't reachable through the component
type system. 0 labels is similarly not a legal WIT flags type.

### Boundary and stress cases
- [x] Function with >16 flat params — error-path pinned via
  `test_adapter_too_many_flat_params_fails_cleanly`
- [ ] Function with >16 flat results (same on task.return side —
  currently bails at `extract_func_sig`, not yet tested)
- [ ] Function with 0 params, 0 results (minimal)
- [ ] Deeply nested compound (e.g., 5+ levels of record/variant)

## Composition shapes (per splice config)

### Coverage we have
- [x] single consumer split (`single`)
- [x] chain (`chain`)
- [x] nested (`nested`)
- [x] fanin (`fanin`)
- [x] fanin-all variants (`fanin-all1`, `fanin-allN`,
  `fanin1`, `faninN`)
- [x] sync primitives (adder)
- [x] async void with string (printer)

### Missing
- [ ] fanin variants across non-trivial result types
  (current fanin coverage uses primitive/string results)
- [ ] Mixed sync/async middleware on a single provider
- [ ] Blocking middleware (`should-block-call`) with a non-void
  handler — currently rejected; test the rejection path
- [ ] Adapter chain >3 deep
- [ ] Multiple splicer rules on overlapping interface sets
- [ ] Middleware whose target interface contains subword types
- [ ] Middleware on a provider that exports a variant-heavy interface

## Known gaps that still produce clear-error bailouts

Not silent-wrong-output cases — these surface as `anyhow::bail!`
at generation time with a clear message. Worth capturing so
they're not forgotten:

- **Heterogeneous variant arms at runtime beyond disc=0** —
  structural emission is correct (via `wit_bindgen_core::abi::lift_from_memory`),
  but none of the `__testme` configurations actually return non-ok
  arms. A handler component that returns an err variant would
  exercise the runtime path.
- **>16 flat values in results** — the `task.return` side of the
  same boundary as the params-side test above. Needs its own
  error-path test.
- **`future<T>` / `stream<T>` result types** — currently unsupported
  at the adapter layer.
- **Anonymous compound types as top-level results** — see
  `adapter-comp-planning.md`'s "Canonical-ABI gaps" section.
