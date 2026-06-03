# Tier-3 / Tier-4 builtins: forward-looking design

Forward-looking design for tier-3/4 builtins — strategy catalog,
resource handling, type predicates, cells wire format, `on_subgraph`
composition. The substrate they ride on (strategy traits, codegen
template, cargo build pipeline, hello-tier3 / hello-tier4 smoke
builtins) has shipped; see [`roadmap.md`](./roadmap.md) for the
calendar overlay.

For the user-facing tier definitions see
[`docs/tiers/tier-3.md`](../tiers/tier-3.md) and
[`docs/tiers/tier-4.md`](../tiers/tier-4.md). For the splicer
framework rules see
[`docs/adapter-components.md`](../adapter-components.md).

Mantra: **design with resources, ship without.**

## Resource semantics

Cells (the tier-2 wire format) lift resources as opaque
`resource-handle(id)` correlation cells. The middleware sees the
type-name and an opaque u64; it cannot call methods on the resource,
read its state, or fabricate a new one. That shapes how the substrate
handles resources at tier-3 / tier-4:

- **Value-typed return synthesis** (tier-4 builtins such as replay,
  fuzz, mock, chaos): the strategy returns a typed Rust value;
  wit-bindgen handles canonical-ABI lowering. Works directly out of
  the substrate.
- **Resource return synthesis** (replay/mock returning a `Response`,
  etc.): the wrapper exports the target's types interface and hosts
  the resource implementation itself. Mints `Resource::new(
  MockedResource { handle, name })` for each recorded correlation id.
  Requires per-target codegen that emits `GuestResource` impls for
  every resource the WIT references. proxy-component established this
  pattern; splicer's tier-4 resource support adopts it with cells as
  the wire format instead of WAVE strings.
- **Resource state mutation** (e.g., HTTP header injection via
  `request.headers().append(...)`): requires importing the resource's
  types interface and dispatching to its methods. Target-specific user
  code, not substrate territory. Users write a wit-bindgen wrapper
  component directly and splicer composes it in.
- **Subset replay** (replay the operation interface, leave the types
  interface host-owned): does not reproduce original behavior;
  resource methods on the returned handle hit the real host, not the
  trace. Resource virt is full-virt or nothing.

## Strategies and what each needs

Each strategy declares its requirements via where-clauses on its
`TransformStrategy` or `VirtualizeStrategy` impl. The substrate traits
stay minimal; per-strategy bounds carry the specifics.

| Tool                          | Tier   | What the strategy needs                                   |
|-------------------------------|--------|-----------------------------------------------------------|
| `replay`                      | tier-4 | `TraceReader` + `R: WitTyped` (decoded via `cells_to_typed`); resource impls (resource path) |
| `fuzz` (input)                | tier-3 | `Args: Arbitrary` + RNG; predicate `no-resources-in-args` |
| `mock`                        | tier-4 | configured cells + `R: WitTyped`                          |
| `chaos-err`                   | tier-4 | configured err variant + `R: WitTyped` on err type        |
| `retry-with-backoff`          | tier-3 | `R: IntoResult` + predicate `returns-result`              |
| `timeout`                     | tier-3 | async race primitives + `R: IntoResult`                   |
| `circuit-breaker`             | tier-2 | (post-`should-skip`); not a tier-3 builtin in practice    |
| `rate-limiter`                | tier-3 | state machine, no type bounds                             |
| `memoize`                     | tier-3 | `Args: Hash, R: Clone` + predicate `no-resources-anywhere` |
| `redact-strings`              | tier-3 | `Args, R: TypedVisit` + predicate `contains-type: string` |
| `normalize-strings`           | tier-3 | `Args, R: TypedVisit` + predicate `contains-type: string` |
| `default-fill-options`        | tier-3 | `Args: TypedVisit` + predicate `has-option`               |
| `clamp-numerics`              | tier-3 | `Args, R: TypedVisit` + predicate `has-numeric`           |
| `mutation-fuzz-seen-shapes`   | tier-3 | `Args: TypedVisit` + RNG                                  |

Tools whose value is target-specific (HTTP header inject, KV
transparent encryption, filesystem path sandbox, custom request
validation) are user-authored wit-bindgen wrapper components, composed
in by splicer like any other component. The substrate is not the right
home for them.

## Type-predicated rule matching

Walking strategies need to be wired only to interfaces that have the
relevant types. The substrate stays unchanged; the filter lives at
splice-time rule matching.

**v1 framework — landed.** `all-funcs:` on a `before`/`between` rule
gates the chain on properties of every function of the matched
interface: `async`, `scope`, and the `args` / `results` value-property
predicates `concrete` / `defaultable`. Evaluated once per chain in
`select`; composes with the interface glob and node-name constraints.
See [`docs/splice-config.md`](../splice-config.md#function-shape-matching-all-funcs)
for the surface, vocabulary, and semantics.

```yaml
between:
  interface: "*"
  all-funcs:
    async: true
    results: [concrete, defaultable]
inject:
  - builtin: redact-strings
```

## Wire format vs user-facing format

Two distinct layers, deliberately separated:

- **Cells** is the **wire format** — the schema defined in
  `wit/common/world.wit`. Tier-2 lifts canonical-ABI values into
  cells; the recorder writes cells; the replayer reads cells.
  Self-describing, so tier-2 middleware can produce cells without
  knowing the target's WIT.
- **WAVE** is the **user-facing typed format** — the wasm component
  model's canonical text representation. Debug logs, fixture
  authoring, `splicer trace diff`, golden-file capture, and any
  other tooling that surfaces typed values to a human or another
  toolchain speaks WAVE.

The bridge between the two is [`WitTyped`](../../splicer-tool-sdk/src/bridge.rs)
+ [`cells_to_typed`](../../splicer-tool-sdk/src/bridge.rs). The
tier-3/4 codegen auto-impls `WitTyped` for every type wit-bindgen
generates; `cells_to_typed::<R>(cells)` walks a cells stream and
constructs an `R` by dispatching through the impls.

A tier-4 replay strategy then takes a cells trace from disk (written
by the tier-2 recorder), threads it through `cells_to_typed`, and
returns the resulting `R`. Cells stays the on-disk format; the
strategy sees typed Rust.

proxy-component uses WAVE for its trace format. Splicer cites
proxy-component as prior art for the `wrapped-` namespace +
`MockedResource` codegen pattern but adopts cells in place of WAVE
for the **trace wire format** — WAVE remains the user-facing typed
surface.

## v2 scope (resource support)

The value-typed path is a clean subset of the resource path; v2 work
items are tracked in [`roadmap.md`](./roadmap.md) Phase 6.

Record/replay is the only tier-3/4 use case that genuinely needs the
resource machinery; everything else (retry, timeout, chaos-err, etc.)
either works on resources without the machinery or does not generalize
to resources at all.

**Design discipline for v1 to keep v2 additive:**

1. Trait bounds on the strategy traits stay minimal (no `Clone`, no
   `Hash`). Per-strategy bounds go on impl where-clauses.
2. The `WitTyped` derive (planned) is designed to accommodate resource
   types even if the value-typed substrate emits no impls for them.
3. Codegen template iterates over `(interfaces, functions)` AND
   `(interfaces, resources, methods)` from v1, with the resource list
   always empty. v2 fills it.
4. Composition wiring treats "interfaces to rewire" as a list-of-one
   in v1, list-of-many in v2.
5. Trace format already supports `cell::resource-handle` cells from
   tier-2; no v1 changes needed.

## `on_subgraph`: how this substrate consumes it

The `on_subgraph` selector and `edge_id` derivation have shipped — see
[`docs/splice-config.md`](../splice-config.md) for the user surface.
This section notes how the typed-wrapper substrate consumes those
primitives.

**Example YAML:**

```yaml
- on_subgraph:
    nodes: [billing-frontend, billing-core, billing-db-shim]
    direction: inbound   # or "outbound" or "both"
  inject:
    - builtin: recorder
      config:
        dir: ./recordings/billing/
```

`on_subgraph` expands at parse time to a set of per-edge rules,
one per boundary edge. The runtime primitive stays per-edge. From the
typed-wrapper substrate's perspective this is just "the rule layer
produced N target edges, generate wrappers for each unique WIT among
them and wire them in."

**Modes the substrate enables on top of `on_subgraph`:**

| Mode               | What gets wired                                              | Useful for                                                |
|--------------------|--------------------------------------------------------------|-----------------------------------------------------------|
| `record`           | tier-2 recorder on boundary edges (no composition rewrite)   | capture a tagged trace of boundary calls                  |
| `replay` inbound   | replayer driver replaces external callers                    | reproduce how the subgraph responded to recorded inputs   |
| `replay` outbound  | replayer virtualizers replace external deps                  | run subgraph against frozen environment behavior          |
| `replay` both      | replayers replace everything outside the subgraph            | maximally isolated execution; reproduce bugs locally      |
| `fuzz` inbound     | fuzz driver generates Arbitrary inputs into boundary         | exercise subgraph with random typed inputs                |
| `fuzz` outbound    | fuzz mocks fabricate random responses to subgraph's calls    | subgraph robustness to misbehaving dependencies           |

`fuzz` inbound is the framing that removes the single-edge-fuzz
semantic weirdness: the subgraph is the SUT, the rest of the graph
does not exist during fuzz, the fuzz driver is the only caller.
Standard fuzzing semantics scoped to an arbitrary subgraph.

**Single-component is a special case** of `on_subgraph: { nodes:
[target], direction: both }`, equivalent to `on_node: target`. The
selector vocabulary handles single-node and multi-node uniformly.

**What the substrate adds on top:**

1. **Codegen template applies per unique target WIT** among the
   boundary edges, not per-edge. Multiple boundary edges that share
   the same target WIT share one codegen output.
2. **Strategy selection** decides the per-rule behavior (recorder,
   replayer, fuzzer, ...). Strategies are target-agnostic; the codegen
   monomorphizes them per-target.
3. **Wire format**: cells trace keyed by `edge_id`. The
   `_splicer_edge_id` config substrate (auto-injected by splicer into
   every spliced builtin) threads identity through; SDK `TraceReader`
   keys reads and writes by it.

**Coexistence with per-interface targeting.** Both selector families
work in the same splice config. Apply `redact-strings` to every
`wasi:http/handler` edge via per-interface targeting (`before` /
`between`), and apply `record` to the billing subgraph's boundary via
`on_subgraph` in the same YAML. Different rules, different
mechanics, both valid. The substrate handles both uniformly because both
reduce to "wire wrappers on some set of edges."

**Sequencing.** The substrate doc's v1 ship presupposes the shipped
recorder pieces (edge_id auto-injection, file-sink, `on_node` and
`on_subgraph` selectors). The replay strategies of *this* doc map
onto the still-pending replayer builtin (tier-4 virtualize).
`on_subgraph` is the prerequisite for the differential-testing
capstone.

## Subgraph differential testing across refactors

The substrate plus `on_subgraph` combine to enable this capstone beyond
the explicit builtins; mostly reuse of pieces already planned plus a
small piece of new glue.

Workflow: record both directions at a subgraph's boundary on version A
of the composition; refactor internals to produce version B; replay
version B with the version-A inputs at the inbound boundary, capturing
version-B's outbound calls; compare the version-B outbound trace to
the version-A outbound trace. Differences flag behavioral regressions
introduced by the refactor.

**Pieces reused:** `on_subgraph` selector (shipped), recorder writing
cells keyed by `edge_id` (shipped), value-typed replayer driving the
subgraph with recorded inbound inputs (pending replayer builtin).

**Pieces new:** a cells **trace diff** (read two cells streams in
parallel, compare call-by-call, report differences with paths into the
cell trees). Implemented as a library in `splicer-tool-sdk` plus a CLI
surface (`splicer trace diff <old.cells> <new.cells>`).

**Roadmap slot:** after the replayer builtin and substrate
doc step 6 (value-typed replay) land. Drops out cheaply because the
heavy machinery is already shipped; the diff itself is tens to
low-hundreds of lines.

**Why this is a high-value demo:** "refactor this subsystem and prove
the boundary contract did not change" is a concrete pain point that
maps onto a one-rule splice config plus one diff invocation. Clean
story for the paper's "scoped composition-level interposition"
contribution.

## Open questions

- **v1 demo target.** If the research paper deadline requires a
  wasi:http replay demo, v1 and v2 collapse (resources have to ship).
  If the paper's non-HTTP eval leg is enough, v1 covers it.
- **Strategy registration.** How do users opt in to third-party
  strategies? For v1, splicer ships a fixed registry of strategies;
  third-party slot in via path. v2+ may want an extension story.

## References

- [`tier2-generic-resource-handles.md`](./tier2-generic-resource-handles.md):
  the dual-of-this constraint at tier-2 (cell-array vs resource-wrapper
  ergonomics).
- [proxy-component](https://github.com/chenyan2002/proxy-component):
  prior art. Uses WAVE wire format and `wrapped-` namespace +
  `MockedResource` pattern. splicer adopts the structural approach,
  switches wire to cells.
- `wit/common/world.wit`: cell representation that flows end-to-end.
- `src/adapter/typed/`: shipped codegen + cargo build pipeline.
- `splicer-tool-sdk/`: home for `TransformStrategy`,
  `VirtualizeStrategy`, `WitTyped`, `cells_to_typed`, `TraceReader`,
  and `#[derive(WitTyped)]` (in `splicer-tool-sdk/derive/`, behind the
  `derive` feature). The SDK helper still to land is `TypedVisit`.
