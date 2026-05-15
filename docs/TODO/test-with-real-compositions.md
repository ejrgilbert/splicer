# Composition shapes — test coverage tracker

Shapes still missing from end-to-end coverage. For the current covered
set, grep `tier1_shapes()` / `tier2_shapes()` in
`tests/fuzz_and_run.rs`.

## Harnesses
- `tests/fuzz_and_run.rs::test_tier1_canned` — explicit `tier1_shapes()`, full pipeline, sync + async.
- `tests/fuzz_and_run.rs::test_tier2_canned` — explicit `tier2_shapes()`, full pipeline.
- `tests/fuzz_and_run.rs::test_fuzz` — arbitrary-driven, full pipeline.
- `src/adapter/tier1/tests/fuzz.rs::fuzz_structural_shapes` — arbitrary-driven, generate + validate only.
- Targeted unit tests in `src/adapter/tier1/tests.rs`, `src/adapter/tier2/lift/tests.rs`, `src/adapter/abi/bindgen.rs`.
- `tests/component-interposition/` — runtime smoke (wasi:http handler).

## Legend
- `[ ]` — no coverage. Action: add a canned shape or named test.
- `[-]` — partial coverage (structural only, or only reachable via random fuzz). Action: pin with a deterministic test.

## Type-shape gaps

Discriminated types:
- `[ ]` `result<T, E>` BOTH subword (homogeneous walk). Canned only has `result<u32, u32>`.
- `[-]` `result<T, E>` heterogeneous arms — structural via `test_adapter_result_u8_u8_async_result` + `_heterogeneous_numeric_variant_`. **Runtime non-ok arm not exercised.**
- `[-]` `option<record>` / `option<variant>` — random-fuzz only.
- `[ ]` top-level `variant` with ≤256 payload cases (u8-disc range, multiple payload arms).
- `[ ]` `enum` >256 cases (u16 disc transition).

Compounds:
- `[-]` `tuple<u8, u32>` — tier-2 canned uses `tuple<u32, string>`; random-fuzz generates arbitrary tuples.
- `[-]` `record` with a `list` field — random-fuzz only.
- `[ ]` `tuple` with resource handles.
- `[ ]` empty `record` / empty `tuple` (zero-sized).

Resources:
- `[ ]` multiple distinct resource types in one signature (e.g., `request`, `response`, `headers`).
- `[ ]` `own<T>` inside a `variant` case.

Boundary:
- `[ ]` function with >16 flat **results** (`task.return` side; bails at `extract_func_sig`, no pinned test).
- `[ ]` function with 0 params, 0 results.
- `[-]` deeply nested compound — random-fuzz with `SPLICER_FUZZ_DEPTH`.

## Composition topology gaps

Fuzzer is type-driven and doesn't address any of these.

- fanin over non-trivial result types (current fanin uses primitives/strings).
- mixed sync/async middleware on a single provider.
- blocking middleware (`should-block`) with a non-void handler — test the rejection.
- adapter chain >3 deep.
- multiple splicer rules on overlapping interface sets.
- middleware whose target interface has subword types.
- middleware on a provider exporting a variant-heavy interface.

## Unsupported by design (adapter rejects, not "missing tests")
- `future<T>` / `stream<T>` — no codegen.
- `map<K, V>` — blocked on `wac-types`; pinned by `test_tier2_map_blocked_on_wac`.
- anonymous compound top-level results — see [`adapter-comp-planning.md`](./adapter-comp-planning.md) § Canonical-ABI gaps.
