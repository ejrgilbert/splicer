# Splicer roadmap — tier-3/4 + multi-edge

Working plan for the tier-3/4 substrate and the multi-edge recorder /
replayer work. Cross-references the design docs that contain the
detail; this doc is the calendar overlay.

**Detail lives in:**
- `docs/TODO/tier3-tier4-substrate.md` — substrate (trait, codegen, SDK).
- `builtins/recorder/TODO-multi-edge.md` — multi-edge mechanics, edge_id, selectors.

## Timeline at a glance

| Phase | Calendar | What lands |
|-------|----------|------------|
| 1     | ~2 weeks | 3 streams in parallel: finish recorder, tier-3 substrate foundation, `between_subgraph` selector |
| 2     | 1-2 weeks| `fuzz-input`, `redact-strings`, smoke tests, multi-edge UX (`on_edge` + `splicer edges` CLI) |
| 4     | 2-3 weeks| Record + replay loop (multi-edge step 7 + record/replay strategies) |
| 5     | ~1 week  | Trace diff CLI + differential-testing demo (v1 paper demo) |
| 6     | 2-3 weeks| v2 resource support → HTTP record/replay (v2 paper demo if needed) |

**v1 demo at ~8 weeks. v2 (HTTP) at ~11 weeks.** Solo, focused. A
collaborator on one stream cuts calendar time roughly in half.

## Phase 1: three streams in parallel (~2 weeks)

### Stream A — finish recorder (~1-1.5 weeks)

Multi-edge doc steps 2-3:
- [x] splicer: `edge_id` derivation (`{interface}::{from}->{to}`)
- [x] splicer: auto-inject `_splicer_edge_id` into builtin config substrate
- [x] recorder: import `wasi:filesystem`, add file sink to `<dir>/<edge_id>.bin`
- [x] recorder: read `_splicer_edge_id`, add `dir` config key
- [x] flip default sink to file; stdout/stderr stay as single-instance debug
- [x] end-to-end multi-edge test (two edges, two files)

### Stream B — tier-3 substrate foundation (~2 weeks)

Substrate foundation items:
- [x] Strategy traits in `splicer-tool-sdk` (`TransformStrategy`, `VirtualizeStrategy` — split per behavior instead of one unified trait)
- [ ] `#[derive(WitTyped)]` proc-macro for user types (codegen auto-impls it for generated wrapper types today; standalone derive is for user code)
- [ ] `TraceReader` skeleton (no resource correlation yet)
- [x] Codegen template (`syn`/`quote`) at `src/adapter/typed/`
- [x] Cargo build pipeline (persistent per-build dirs under `<user-cache>/splicer/typed-builtins/builds/` + shared `CARGO_TARGET_DIR`; cargo's incremental handles staleness, no custom wasm cache)
- [x] `hello-tier3` + `hello-tier4` smoke builtins (end-to-end through `./run.sh --builtin-hello-tier{3,4}`)

### Stream C — subgraph selection (~1-1.5 weeks)

Multi-edge doc step 6:
- [ ] YAML grammar: `between_subgraph`, `on_node`, `on_interface` selectors
- [ ] Parse-time expansion to per-edge rules
- [ ] Composition graph walk (reuses splicer's existing wac graph)
- [ ] Filter blocks (narrow by interface name or explicit edge_id list)

### Coordination

`edge_id` derivation is shared between Stream A and Stream C. Place it
once (e.g. `src/edge_id.rs`), both streams consume it. Canonical format
is already specced in the multi-edge doc; no design negotiation needed.

## Phase 2: smoke + first builtins + UX (1-2 weeks)

- [ ] `fuzz-input` builtin (drives `Args: Arbitrary` + `wit-bindgen additional_derives`)
- [ ] `redact-strings` builtin (drives `TypedVisit` derive + type-predicate matcher)
- [ ] Multi-edge step 4: `on_edge: <id>` literal selector
- [ ] Multi-edge step 5: `splicer edges <composition>` CLI
- [x] **User-form tier-3/4 middleware.** YAML `name:` + `path:` to a strategy
  crate directory (Cargo.toml + manifest.toml) flows through the same codegen
  pipeline as builtins. See [`docs/tiers/tier-3.md`](../tiers/tier-3.md#referencing-your-strategy-from-a-splice-config).
- [ ] **Sync-target support in tier-3/4 codegen.** Today wit-bindgen emits sync
  `fn` Guest methods for `func` WIT signatures but `emit_method.rs` always emits
  `async fn` bodies (matching the async `TransformStrategy` / `VirtualizeStrategy`
  traits), so sync targets fail the wrapper crate's compile with E0053. Options:
  emit sync `fn` bodies that `block_on` the async strategy, or split the
  strategy traits into sync + async variants. Currently restricts the harness
  to `async func` targets only (see
  `tests/component-interposition/splicer-rules/builtin-hello-tier3.yaml`).
  When this lands, extend the matrix tests in
  `src/adapter/typed/matrix_tests.rs` with sync `func` fixtures so both paths
  are exercised.

Skipped Phase 3 — Phase 1's Stream C already covered `between_subgraph`,
and Phase 2 picks up the rest.

## Phase 4: record + replay loop (2-3 weeks)

- [ ] Multi-edge step 7: replayer builtin (tier-4 virtualize)
- [ ] `record` strategy (cells to sink)
- [ ] `replay` strategy (cells → typed values, value-typed targets)

## Phase 5: capstone + v1 demo (~1 week)

- [ ] Trace diff library in `splicer-tool-sdk`
- [ ] `splicer trace diff <old.cells> <new.cells>` CLI
- [ ] Pick value-typed service WIT for non-HTTP eval leg
- [ ] End-to-end differential-testing demo runnable from one splice config

## Phase 6: v2 resource support (2-3 weeks, if needed for paper)

proxy-component is the blueprint; adapt with cells in place of WAVE:
- [ ] WIT walker detects resources
- [ ] `wrapped-` namespace WIT rewriting
- [ ] `MockedResource { handle, name }` pattern + `GuestResource` impls
- [ ] Resource correlation map in `TraceReader`
- [ ] `WitTyped` impls for `Resource<T>`
- [ ] wac composition wiring for types interfaces (full virt)
- [ ] HTTP record/replay demo

## Back-burner

Defer until post-paper, no blocking impact:
- `tier2-should-call.md` (tier-2 hook gap, no committed consumer)
- `tier2-generic-resource-handles.md` (long-term design)
- `tier2-nested-list-cabi-realloc-batching.md` (perf)
- `tier2-list-compound-elements.md` (only items with concrete demand)
- `sync-wit-suspend-limit.md` (bug fix on demand)
- Open questions in `adapter-comp-planning.md`

## Risk points

Where the calendar can slip:
1. **`wasi:filesystem` integration in recorder** (Stream A, step 3). Resource-heavy API.
2. **`WitTyped` derive for resources** (Phase 6). Trickiest derive corner.
3. **wac composition rewriting for full virt** (Phase 6). May need splicer's WAC emitter restructuring.
4. **Cells binary on-disk format** (Phase 1 Stream A / 4). Framing, versioning, edge_id tagging details.
