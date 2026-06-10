# Splicer roadmap — tier-3/4 + multi-edge

Working plan for the tier-3/4 substrate and the multi-edge recorder /
replayer work. Cross-references the design docs that contain the
detail; this doc is the calendar overlay.

**Detail lives in:**
- `docs/TODO/tier3-tier4-builtins.md` — builtin design (strategy catalog, resources, type predicates, `on_subgraph` composition).

## Timeline at a glance

| Phase | Calendar  | What lands                                                                                  |
|-------|-----------|---------------------------------------------------------------------------------------------|
| 1     | ~2 weeks  | 3 streams in parallel: finish recorder, tier-3 substrate foundation, `on_subgraph` selector |
| 2     | 1-2 weeks | `fuzz-input`, `redact-strings`, smoke tests                                                 |
| 4     | 2-3 weeks | Record + replay loop (replayer builtin + record/replay strategies)                          |
| 5     | ~1 week   | Trace diff CLI + differential-testing demo (v1 paper demo)                                  |
| 6     | 2-3 weeks | v2 handle support (resources, error-context, future, stream); HTTP record/replay demo       |

**v1 demo at ~8 weeks. v2 (HTTP) at ~11 weeks.** Solo, focused. A
collaborator on one stream cuts calendar time roughly in half.

## Phase 1: three streams in parallel (~2 weeks)

### Stream A — finish recorder (~1-1.5 weeks)

- [x] splicer: `edge_id` derivation (`{interface}::{from}->{to}`)
- [x] splicer: auto-inject `_splicer_edge_id` into builtin config substrate
- [x] recorder: import `wasi:filesystem`, add file sink to `<dir>/<edge_id>.bin`
- [x] recorder: read `_splicer_edge_id`, add `dir` config key
- [x] flip default sink to file; stdout/stderr stay as single-instance debug
- [x] end-to-end multi-edge test (two edges, two files)

### Stream B — tier-3 substrate foundation (~2 weeks)

Substrate foundation items:
- [x] Strategy traits in `splicer-tool-sdk` (`TransformStrategy`, `VirtualizeStrategy` — split per behavior instead of one unified trait)
- [x] `#[derive(WitTyped)]` proc-macro for user types (codegen auto-impls it for generated wrapper types today; standalone derive is for user code) — `splicer-tool-sdk/derive/`, behind the off-by-default `derive` feature
- [x] `TraceReader` (no resource correlation yet) — `splicer-tool-sdk/src/trace.rs`; forward cursor with typed call + return decode (`next_call`/`next_return`, `next_call_typed`/`next_return_typed` + `bridge::args_to_typed`), backing both the driver (call) and virtualize (return) paths
- [x] Codegen template (`syn`/`quote`) at `src/adapter/typed/`
- [x] Cargo build pipeline (persistent per-build dirs under `<user-cache>/splicer/typed-builtins/builds/` + shared `CARGO_TARGET_DIR`; cargo's incremental handles staleness, no custom wasm cache)
- [x] `hello-tier3` + `hello-tier4` smoke builtins (end-to-end through `./run.sh --builtin-hello-tier{3,4}`)

### Stream C — subgraph selection (~1-1.5 weeks)

- [x] YAML grammar: `on_node`, `on_subgraph` selectors
- [x] `on_node` survives parse as `SpliceRule::OnNode`; `resolve_rules`
  expands into one or two `before`/`between` rules. Preview overlays
  the matched node in a context color.
- [x] `on_subgraph` resolves against the composition graph (boundary =
  "exactly one endpoint in the set"). Missing-node and
  disconnected-set checks reject at splice time.
- [x] `interface:` narrowing on `on_node` / `on_subgraph` (glob).
- [x] `splicer preview` paints subgraph nodes + internal edges (context
  color) and matched boundary edges (default color).

`on_edge` and `on_interface` were earlier candidates but offered no
expressive delta — `on_edge` is a fully-specified `between`, and
`on_interface` is `before`/`between` with no node constraint. The
recorder's `edge_id` stays a purely internal addressing scheme (filename
key + auto-injected config substrate); it never appears in user YAML.

## Phase 2: smoke + first builtins + UX (1-2 weeks)

- [ ] `fuzz-input` builtin (drives `Args: Arbitrary` + `wit-bindgen additional_derives`)
- [ ] `redact-strings` builtin (drives `TypedVisit` derive + type-predicate matcher)
- [x] **User-form tier-3/4 middleware.** YAML `name:` + `path:` to a strategy
  crate directory (Cargo.toml + manifest.toml) flows through the same codegen
  pipeline as builtins. See [`docs/tiers/tier-3.md`](../tiers/tier-3.md#referencing-your-strategy-from-a-splice-config).
- [x] **Sync-target support in tier-3/4 codegen.** Splicer synthesizes
  an async-WIT mirror of the sync target, the wrapper lifts the mirror,
  and a bridge component at the chain head translates sync caller calls
  into the wrapper's async surface (PR #100,
  `feat/support-sync-target`). Exercised by the `my:service/adder` rule
  in `tests/component-interposition/splicer-rules/builtin-hello-tier3.yaml`.

Skipped Phase 3 — Phase 1's Stream C already covered `on_node` and
`on_subgraph`, and Phase 2 picks up the rest.

## Phase 4: record + replay loop (2-3 weeks)

- [x] Replayer builtin (tier-4 virtualize) — `R: WitTypedWithResources`, reads `<dir>/<sanitized-edge-id>.bin` via `splicer:builtin-config` substrate (mirrors recorder layout); demo at `--builtin-replayer`
- [x] `record` strategy (cells to sink) — recorder builtin (Phase 1 Stream A)
- [x] `replay` strategy (cells → typed values, value-typed targets) — replayer builtin uses `TraceReader::next_return_typed_with_resources`

## Phase 5: capstone + v1 demo (~1 week)

- [ ] Trace diff library in `splicer-tool-sdk`
- [ ] `splicer trace diff <old.cells> <new.cells>` CLI
- [ ] Pick value-typed service WIT for non-HTTP eval leg
- [ ] End-to-end differential-testing demo runnable from one splice config

## Phase 6: v2 handle support (2-3 weeks for resources; non-resource handles land per kind)

Tier-3/4 currently bails on every correlation handle (resource, future,
stream, error-context). v2 lifts those bails. Resources are the paper
demo path; the other three share the SDK-decode and codegen-IR plumbing
but have their own synthesis story (or lack of one).

**Resource branch (paper demo path).** proxy-component is the
blueprint; local PoC at `../../research/proxy-component`. Adapt with
cells in place of WAVE:
- [x] WIT walker detects resources
- [x] `wrapped-` namespace WIT rewriting
- [x] `MockedResource { handle, name }` pattern + `GuestResource` impls
- [x] Resource correlation map in `TraceReader` (`next_return_typed_with_resources`)
- [x] `WitTyped` impls for `Resource<T>` (`WitTypedWithResources` trait + wrapper-newtype macro)
- [x] wac composition wiring for types interfaces (full virt)
- [ ] HTTP record/replay demo

**Non-resource handle branches** (pass-through unblocks
redact-strings/timeout/retry on interfaces that mention these types;
return-synthesis is the harder step):
- [ ] `error-context`: SDK `decode_cell` + tier-3/4 codegen IR mapping; synthesis blocked on wasmtime cross-component lift bug (track upstream)
- [ ] `future`: SDK + codegen IR mapping for pass-through; return-synthesis deferred pending host primitives
- [ ] `stream`: SDK + codegen IR mapping for pass-through; return-synthesis deferred pending host primitives

## Back-burner

Defer until post-paper, no blocking impact:
- `tier2-generic-resource-handles.md` (long-term design)
- `tier2-list-compound-elements.md` (only items with concrete demand)
- `sync-suspend-limitation.md` (bug fix on demand)
- `per-function-interposition-filter.md` (UX, gated on real demand)
- `canonical-abi-gaps.md` (real `bail!`s; fix when a user hits them)

## Risk points

Where the calendar can slip:
1. **`wasi:filesystem` integration in recorder** (Stream A, step 3). Resource-heavy API.
2. **`WitTyped` derive for handles** (Phase 6). Trickiest derive corner; resources are the headline case, but the SDK + codegen plumbing has to land for all four handle kinds.
3. **wac composition rewriting for full virt** (Phase 6). May need splicer's WAC emitter restructuring.
4. **Cells binary on-disk format** (Phase 1 Stream A / 4). Framing, versioning, edge_id tagging details.
