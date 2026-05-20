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
| 4     | 2-3 weeks| Record + replay loop (multi-edge step 7 + substrate steps 5-6) |
| 5     | ~1 week  | Trace diff CLI + differential-testing demo (v1 paper demo) |
| 6     | 2-3 weeks| v2 resource support → HTTP record/replay (v2 paper demo if needed) |

**v1 demo at ~8 weeks. v2 (HTTP) at ~11 weeks.** Solo, focused. A
collaborator on one stream cuts calendar time roughly in half.

## Phase 1: three streams in parallel (~2 weeks)

### Stream A — finish recorder (~1-1.5 weeks)

Multi-edge doc steps 2-3:
- [ ] splicer: `edge_id` derivation (`{interface}::{from}->{to}`)
- [ ] splicer: auto-inject `_splicer_edge_id` into builtin config substrate
- [ ] recorder: import `wasi:filesystem`, add file sink to `<dir>/<edge_id>.bin`
- [ ] recorder: read `_splicer_edge_id`, add `dir` config key
- [ ] flip default sink to file; stdout/stderr stay as single-instance debug
- [ ] end-to-end multi-edge test (two edges, two files)

### Stream B — tier-3 substrate foundation (~2 weeks)

Substrate doc TL;DR foundation items:
- [ ] `WrapperStrategy` trait in `splicer-tool-sdk`
- [ ] `TypedFromCells` derive macro
- [ ] `TraceReader` skeleton (no resource correlation yet)
- [ ] Codegen template skeleton (`syn`/`quote`) in `src/codegen/typed_builtins/`
- [ ] Cargo build pipeline + cache (`(WIT-hash, template-version, sdk-version) -> .wasm`)
- [ ] `hello-tier3` + `hello-tier4` smoke builtins (if time remains in the phase)

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

Skipped Phase 3 — Phase 1's Stream C already covered `between_subgraph`,
and Phase 2 picks up the rest.

## Phase 4: record + replay loop (2-3 weeks)

- [ ] Multi-edge step 7: replayer builtin (tier-4 virtualize)
- [ ] Substrate step 5: `record` strategy (cells to sink)
- [ ] Substrate step 6: `replay` strategy (cells → typed values, value-typed targets)

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
- [ ] `TypedFromCells` impls for `Resource<T>`
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
2. **`TypedFromCells` derive for resources** (Phase 6). Trickiest derive corner.
3. **wac composition rewriting for full virt** (Phase 6). May need splicer's WAC emitter restructuring.
4. **Cells binary on-disk format** (Phase 1 Stream A / 4). Framing, versioning, edge_id tagging details.
