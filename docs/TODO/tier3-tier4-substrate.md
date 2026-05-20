# Tier-3 / Tier-4 substrate: design ideation

Planning doc for the tier-3 (transform) and tier-4 (virtualize) substrate
in splicer. Documents what the substrate is, how it composes with the
existing rule layer, and the sequencing for landing it. Active document;
expect edits as the design firms up.

For the user-facing tier definitions see
[`docs/tiers/tier-3.md`](../tiers/tier-3.md) and
[`docs/tiers/tier-4.md`](../tiers/tier-4.md). For cross-tier framework rules
see [`docs/adapter-components.md`](../adapter-components.md). For sibling
planning notes see [`adapter-comp-planning.md`](./adapter-comp-planning.md).
For the multi-edge recorder/replayer architecture, edge_id format, and
selector vocabulary that this doc builds on, see
[`builtins/recorder/TODO-multi-edge.md`](../../builtins/recorder/TODO-multi-edge.md).

## TL;DR

Splicer is building a substrate that lets users author tier-3 / tier-4
capabilities (replay, fuzz, mock, chaos, retry, latency, redact, etc.) as
plug-in strategies on top of a per-target-WIT codegen template. The same
substrate composes with the **`between_subgraph` selector** documented in
the recorder's multi-edge architecture
([`builtins/recorder/TODO-multi-edge.md`](../../builtins/recorder/TODO-multi-edge.md))
to scope record / replay / fuzz / observation to user-defined subgraphs of
a composition. Cells is the wire format end-to-end.

### Substrate pieces (foundation)

- [ ] `WrapperStrategy` trait in `splicer-tool-sdk`
- [ ] `TypedFromCells` derive macro
- [ ] `TypedVisit` derive macro
- [ ] `TraceReader` + cells trace format keyed by `edge_id` (see recorder doc for `edge_id` derivation contract)
- [ ] Codegen template (`syn`/`quote`) that turns a target WIT into a wrapper crate
- [ ] Cargo build pipeline + cache (`(WIT-hash, template-version, sdk-version) -> .wasm`)
- [ ] `between_subgraph` selector (per recorder doc step 6) — additive to existing `before` / `between` selectors

### v1 ship (value-typed targets)

- [ ] **Step 1**: `hello-tier3` builtin. Pass-through tier-3 strategy; substrate smoke test.
- [ ] **Step 2**: `hello-tier4` builtin. No-downstream variant; tier-4 mode smoke test.
- [ ] **Step 3**: `fuzz-input` builtin. Drives `Args: Arbitrary` bound and `wit-bindgen additional_derives`.
- [ ] **Step 4**: `redact-strings` builtin. Drives type-predicate matching + `TypedVisit` derive.
- [ ] **Step 5**: `record` builtin. Drives cells-to-sink writing with edge-identity tagging.
- [ ] **Step 6**: `replay` builtin (value-typed). Drives `TypedFromCells` end-to-end.
- [ ] **Step 7**: `splicer trace diff` CLI + differential-testing capstone demo.

### v2 ship (adds resource support, driven by HTTP record/replay)

- [ ] WIT walker detects resources
- [ ] `wrapped-` namespace WIT rewriting
- [ ] Conversion interface + `MockedResource { handle, name }` pattern
- [ ] Resource correlation map in `TraceReader`
- [ ] `TypedFromCells` impls for `Resource<T>`
- [ ] wac composition wiring for types interfaces (full virt)
- [ ] HTTP record/replay shipped as the forcing function and v2 demo

### Design discipline applied across v1

- [ ] Trait bounds on `WrapperStrategy` stay minimal; per-strategy bounds go on impl where-clauses
- [ ] `TypedFromCells` derive designed to accommodate resource types even if v1 emits no impls for them
- [ ] Codegen template iterates over `(interfaces, functions)` AND `(interfaces, resources, methods)` with the resource list always empty in v1
- [ ] Composition wiring treats "interfaces to rewire" as a list (length-1 in v1, length-many in v2)

Mantra: **design with resources, ship without.**

The rest of this doc explains the substrate pieces, the rule-layer
additions, the resource constraints, and the use cases enabled. Each
section below corresponds to a checklist item or group above.

## What this doc covers

The substrate splicer provides for users to build tier-3 / tier-4 capabilities
on top of WIT-typed component compositions. Three pieces:

1. **`WrapperStrategy`**: a Rust trait + codegen template combination that
   produces typed per-target wrapper components from a target's WIT plus a
   strategy implementation. Same trait covers tier-3 (forward) and tier-4
   (virtualize) per whether the strategy calls `downstream`.
2. **`between_subgraph` selector**: an additive rule-layer mechanism
   letting users declaratively scope interposition to the boundary edges
   of a chosen subgraph of the composition. Coexists with the existing
   `before` / `between` selectors. See
   [`builtins/recorder/TODO-multi-edge.md`](../../builtins/recorder/TODO-multi-edge.md)
   for the selector grammar (`on_edge`, `on_node`, `between_subgraph`),
   `edge_id` derivation contract, and four-layer architecture (SDK,
   recorder/replayer, splicer-runtime-injection, YAML grammar) that
   this substrate builds on.
3. **Cells end-to-end**: the wire format used by both tier-2 observation
   and the recorder/replay loop, threaded through per-target wrapper
   codegen via SDK derives.

## Resource semantics

Cells (the tier-2 wire format) lift resources as opaque `resource-handle(id)`
correlation cells. The middleware sees the type-name and an opaque u64; it
cannot call methods on the resource, read its state, or fabricate a new one.
That property is intentional for tier-2 (target-agnostic observation) and it
shapes how the substrate handles resources at tier-3 / tier-4:

- **Value-typed return synthesis** (tier-4 builtins such as replay, fuzz,
  mock, chaos): the strategy returns a typed Rust value; wit-bindgen
  handles canonical-ABI lowering. Works directly out of the substrate.
- **Resource return synthesis** (replay/mock returning a `Response`,
  etc.): the wrapper exports the target's types interface and hosts the
  resource implementation itself. Mints `Resource::new(MockedResource {
  handle, name })` for each recorded correlation id. Requires per-target
  codegen that emits `GuestResource` impls for every resource the WIT
  references. proxy-component established this pattern; splicer's tier-4
  resource support adopts it with cells as the wire format instead of
  WAVE strings.
- **Resource state mutation** (e.g., HTTP header injection via
  `request.headers().append(...)`): requires importing the resource's
  types interface and dispatching to its methods. This is target-specific
  user code, not substrate territory. Users write a wit-bindgen wrapper
  component directly and splicer composes it in.
- **Subset replay** (replay the operation interface, leave the types
  interface host-owned): does not reproduce original behavior; resource
  methods on the returned handle hit the real host, not the trace.
  Resource virt is full-virt or nothing.

## The substrate: `WrapperStrategy`

One trait, target-agnostic, lives in `splicer-tool-sdk`. Both tier-3 and tier-4
strategies implement it; the tier is a runtime property of whether the strategy
chooses to call `downstream`.

```rust
#[async_trait]
pub trait WrapperStrategy: Send {
    async fn handle<Args, R>(
        &mut self,
        ctx: CallCtx,
        args: Args,
        downstream: impl AsyncFnOnce(Args) -> R + Send,
    ) -> R
    where
        Args: TypedArgs,
        R: TypedResult;
}
```

- Tier-4 strategies (replay, fuzz, mock, chaos): ignore `downstream`, synthesize
  `R`.
- Tier-3 strategies (retry, latency, circuit-breaker, rate-limiter): call
  `downstream(maybe_mutated_args)`, optionally mutate `R`.

Per-strategy extra bounds (`Clone`, `Hash`, `Arbitrary`, `TypedVisit`) are
added as where-clauses on the impl, not on the substrate trait. This keeps
the substrate's surface area minimal and lets each strategy declare what it
needs without restricting others.

## The codegen template

Per unique target WIT, splicer emits a Rust crate that:

1. Invokes `wit_bindgen::generate!` with `additional_derives: [TypedFromCells]`.
2. Emits one `impl Guest for Wrapper<S>` block per interface in the target.
3. Emits one `impl GuestResource for Wrapper<S>` block per resource the WIT
   walker finds. (Zero of these for value-typed targets; many for HTTP-style
   targets.)
4. Every emitted method body is the uniform
   `STRATEGY.with(|s| s.handle::<Args, R>(ctx, args, downstream))` pattern.
5. The `downstream` closure is either `bindings::target::import::fn(args).await`
   (tier-3, wrapper imports the target) or `unreachable!()` (tier-4, wrapper
   does not import the target).

The codegen template structure is **stable**; new builtins do not require new
codegen, they ship as new strategy implementations.

## Strategies and what each needs

Each strategy declares its requirements via where-clauses on its
`WrapperStrategy` impl. The substrate trait stays minimal; per-strategy
bounds carry the specifics.

| Tool                          | Tier   | What the strategy needs                                  |
|-------------------------------|--------|----------------------------------------------------------|
| `replay`                      | tier-4 | `TraceReader` + `R: TypedFromCells`; resource impls (v2) |
| `fuzz` (input)                | tier-3 | `Args: Arbitrary` + RNG; predicate `no-resources-in-args` |
| `mock`                        | tier-4 | configured cells + `R: TypedFromCells`                   |
| `chaos-err`                   | tier-4 | configured err variant + `R: TypedFromCells` on err type |
| `retry-with-backoff`          | tier-3 | `R: IntoResult` + predicate `returns-result`             |
| `timeout`                     | tier-3 | async race primitives + `R: IntoResult`                  |
| `circuit-breaker`             | tier-2 | (post-`should-skip`); not a tier-3 builtin in practice   |
| `rate-limiter`                | tier-3 | state machine, no type bounds                            |
| `memoize`                     | tier-3 | `Args: Hash, R: Clone` + predicate `no-resources-anywhere` |
| `redact-strings`              | tier-3 | `Args, R: TypedVisit` + predicate `contains-type: string` |
| `normalize-strings`           | tier-3 | `Args, R: TypedVisit` + predicate `contains-type: string` |
| `default-fill-options`        | tier-3 | `Args: TypedVisit` + predicate `has-option`              |
| `clamp-numerics`              | tier-3 | `Args, R: TypedVisit` + predicate `has-numeric`          |
| `mutation-fuzz-seen-shapes`   | tier-3 | `Args: TypedVisit` + RNG                                 |

Tools whose value is target-specific (HTTP header inject, KV transparent
encryption, filesystem path sandbox, custom request validation) are
user-authored wit-bindgen wrapper components, composed in by splicer like
any other component. The substrate is not the right home for them.

## Type-predicated rule matching

Walking strategies (Family 3 above) need to be wired only to interfaces
that have the relevant types. The substrate stays unchanged; what we add is
a type-predicate filter at splice-time rule matching:

```yaml
inject:
  - builtin: redact-strings
    match:
      contains-type: string
    config:
      patterns: ["email", "ssn"]
```

Predicates are static WIT walkers (`contains-type: string`,
`returns-result`, `has-option`, `has-numeric`, etc.). Evaluated once at
splice time. They decide which interfaces a builtin gets wired to; the
strategy itself assumes the predicate held. Walking happens at runtime via
`TypedVisit` derives.

Type-predicate matching also helps non-walking strategies. `retry` matches
only `returns-result`; `memoize` matches `no-resources-anywhere`. Composable
with existing name-based matching.

## Wire format: cells end-to-end

Splicer uses cells (the schema defined in `wit/common/world.wit`) as the
single wire format across the recorder, replayer, fuzz seed corpus, and
any future cells-consuming tool. Tier-2 already lifts to cells; the
recorder writes cells; the replayer reads cells back through
`TypedFromCells` derives.

Cells is self-describing, which tier-2 needs because the middleware does
not know the target's WIT. The typed-codegen path (where the codegen *does*
know the target WIT) reads cells via the SDK's `TypedFromCells` derive,
which wit-bindgen emits per generated type via `additional_derives`. The
recorder/replayer loop is single-format, no bridge layer between recording
and replaying.

proxy-component uses WAVE for its trace format. Splicer cites proxy-component
as prior art for the `wrapped-` namespace + `MockedResource` codegen
pattern but adopts cells in place of WAVE for the trace wire format.

## Where splicer's contribution lives

Three places. Worth being explicit so we do not confuse ourselves later:

1. **`WrapperStrategy` trait + SDK helpers** in `splicer-tool-sdk`:
   `TypedFromCells` derive, `TypedVisit` derive, `TraceReader`,
   `CorrelationMap`. Shared across every typed builtin.
2. **Codegen template** under `src/codegen/typed_builtins/` (proposed):
   one `syn`/`quote` template that turns a target WIT into a wrapper crate.
3. **Builtin strategies** as Rust crates: `ReplayStrategy`, `FuzzStrategy`,
   `MockStrategy`, `ChaosStrategy`, `RetryStrategy`, `LatencyStrategy`, etc.
   Each is small (~100-300 LoC); adding a new builtin is "write a strategy."

The codegen template is stable surface; strategies are growable surface.
Resources, when added, extend the codegen template (more impl blocks emitted)
and the SDK helpers (`TypedFromCells` for `Resource<T>`, correlation map
plumbing), not the strategy trait.

## v1 / v2 split

The value-typed path is a clean subset of the resource path. Resources are
additive on every axis if the foundation is right.

**v1 scope (value-typed targets only).** Sequenced so each step exercises a
specific substrate piece and de-risks the next:

| Step | Builtin             | What it proves                                                   |
|------|---------------------|------------------------------------------------------------------|
| 1    | `hello-tier3`       | pass-through tier-3 strategy: codegen template + dispatch + composition wiring + async plumbing. Mirrors `hello-tier1` / `hello-tier2`. |
| 2    | `hello-tier4`       | tier-4 mode: codegen template emits wrapper that does not import the target; strategy ignores `downstream`. |
| 3    | `fuzz-input`        | first shipped useful builtin. Drives `Args: Arbitrary` bound, `wit-bindgen additional_derives`. Tier-3, value-typed, no result introspection, no `TypedFromCells`. |
| 4    | `redact-strings`    | drives type-predicate matching (`contains-type: string`) and the `TypedVisit` derive. Family 3 walking strategy. |
| 5    | `record`            | drives cells-to-sink writing. Reuses tier-2 lift; new piece is the sink + framing. |
| 6    | `replay` (value)    | drives `TypedFromCells` derive end-to-end. Tier-4 strategy that consumes the recorder's output and serves typed values. |

Why this order:
- `hello-tier3` / `hello-tier4` are the absolute smoke tests, parallel to
  `hello-tier1` / `hello-tier2`. No SDK derives, no predicates. Validates
  the substrate-and-codegen plumbing alone.
- `fuzz-input` is the first builtin that buys a real research-paper-shaped
  capability and pulls in the smallest extra dependency (`Arbitrary`), which
  wit-bindgen already supports.
- `redact-strings` forces the type-predicate matcher and `TypedVisit` to
  land in the SDK. Without something in v1 that needs these, predicates
  and walking become v2 hazards.
- `record` and `replay` together prove the cells round-trip. Recorder reuses
  tier-2 lift cells; replayer drives `TypedFromCells` from cells back to typed
  values. Splits the round-trip into two shippable halves.

Other strategies (memoize, normalize, default-fill, clamp, mutation-fuzz,
bit-flip, chaos-err, retry, timeout) layer on after v1 lands. Each is "write
a strategy" once the substrate plus SDK derives plus predicates exist.

**v2 scope (adds resource support).** Driven by HTTP record/replay as the
forcing function:

- WIT walker detects resources.
- `wrapped-` namespace WIT rewriting.
- Conversion interface generation (`get-mock-X(handle: u32) -> X`).
- `MockedResource { handle, name }` pattern + emitted `GuestResource` impls.
- Correlation map plumbing in `TraceReader`.
- `TypedFromCells` impls for `Resource<T>`.
- wac composition wiring for the types interface (full virt).
- Target: wasi:http first, then wasi:keyvalue (where WIT permits),
  wasi:filesystem (where WIT permits).

Record/replay is the only tier-3/4 use case that genuinely needs the
resource machinery; everything else (retry, timeout, chaos-err, etc.)
either works on resources without the machinery or does not generalize
to resources at all. Using HTTP record/replay as the forcing function
keeps the v2 investment focused on a marquee target.

**Design discipline for v1 to keep v2 additive:**

1. Trait bounds on `WrapperStrategy` stay minimal (no `Clone`, no `Hash`).
   Per-strategy bounds go on impl where-clauses.
2. `TypedFromCells` derive macro is designed to accommodate resource types
   even if v1 emits no impls for them.
3. Codegen template iterates over `(interfaces, functions)` AND
   `(interfaces, resources, methods)` from v1, with the resource list always
   empty. v2 fills it.
4. Composition wiring treats "interfaces to rewire" as a list-of-one in v1,
   list-of-many in v2.
5. Trace format already supports `cell::resource-handle` cells from tier-2;
   no v1 changes needed.

Mantra: **design with resources, ship without.**

## `between_subgraph`: how this substrate consumes it

The multi-edge selector vocabulary and `edge_id` mechanism are documented
in [`builtins/recorder/TODO-multi-edge.md`](../../builtins/recorder/TODO-multi-edge.md).
That doc defines the four-layer architecture (SDK, recorder/replayer,
splicer-runtime-injection, YAML grammar), the `on_edge` / `on_node` /
`between_subgraph` selector vocabulary, the canonical `edge_id` format,
and the 7-step roadmap. This section just notes how the typed-wrapper
substrate consumes those primitives.

**Example YAML** (per recorder doc, reproduced here for context):

```yaml
- between_subgraph:
    nodes: [billing-frontend, billing-core, billing-db-shim]
    direction: inbound   # or "outbound" or "both"
  inject:
    - builtin: recorder
      config:
        dir: ./recordings/billing/
```

`between_subgraph` expands at parse time to a set of per-edge rules,
one per boundary edge. The runtime primitive stays per-edge. From the
typed-wrapper substrate's perspective this is just "the rule layer
produced N target edges, generate wrappers for each unique WIT among
them and wire them in."

**Modes the substrate enables on top of `between_subgraph`:**

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
does not exist during fuzz, the fuzz driver is the only caller. Standard
fuzzing semantics scoped to an arbitrary subgraph.

**Single-component is a special case** of `between_subgraph: { nodes: [target],
direction: both }`, equivalent to `on_node: target`. The selector
vocabulary handles single-node and multi-node uniformly.

**What the substrate adds on top:**

1. **Codegen template applies per unique target WIT** among the
   boundary edges, not per-edge. Multiple boundary edges that share
   the same target WIT share one codegen output.
2. **`WrapperStrategy` selection** decides the per-rule behavior
   (recorder, replayer, fuzzer, ...). Strategies are target-agnostic;
   the codegen monomorphizes them per-target.
3. **Wire format**: cells trace keyed by `edge_id` (the recorder doc's
   canonical format). The `_splicer_edge_id` config substrate from the
   recorder doc threads identity through; SDK `TraceReader` keys reads
   and writes by it.

**Coexistence with per-interface targeting.** Both selector families
work in the same splice config. Apply `redact-strings` to every
`wasi:http/handler` edge via per-interface targeting (`before` /
`between`), and apply `record` to the billing subgraph's boundary via
`between_subgraph` in the same YAML. Different rules, different
mechanics, both valid. The substrate handles both uniformly because both
reduce to "wire wrappers on some set of edges."

**Sequencing.** The substrate doc's v1 ship presupposes recorder doc
steps 2-5 (edge_id auto-injection, file-sink, `on_edge` selector,
`splicer edges` CLI). v1 step 5 (`record`) and step 6 (`replay`,
value-typed) of *this* doc correspond to recorder doc step 7
(replayer as tier-4 virtualize). `between_subgraph` (recorder doc
step 6) is the prerequisite for the differential-testing capstone.

## Use cases that drop out of the substrate

The substrate plus `between_subgraph` combine to enable several
high-value capabilities beyond the explicit builtins. Each is mostly
reuse of pieces already in the v1 plan plus a small piece of new glue.

### Subgraph differential testing across refactors

Workflow: record both directions at a subgraph's boundary on version A
of the composition; refactor internals to produce version B; replay
version B with the version-A inputs at the inbound boundary, capturing
version-B's outbound calls; compare the version-B outbound trace to
the version-A outbound trace. Differences flag behavioral regressions
introduced by the refactor.

**Pieces reused:** `between_subgraph` selector (recorder doc step 6),
recorder writing cells keyed by `edge_id`, value-typed replayer driving
the subgraph with recorded inbound inputs (recorder doc step 7).

**Pieces new:** a cells **trace diff** (read two cells streams in
parallel, compare call-by-call, report differences with paths into the
cell trees). Implemented as a library in `splicer-tool-sdk` plus a CLI
surface (`splicer trace diff <old.cells> <new.cells>`).

**Roadmap slot:** after recorder doc step 7 (replayer) and substrate
doc step 6 (value-typed replay) land. Drops out cheaply because the
heavy machinery is already shipped; the diff itself is tens to
low-hundreds of lines.

**Why this is a high-value demo:** "refactor this subsystem and prove
the boundary contract did not change" is a concrete pain point that
maps onto a one-rule splice config plus one diff invocation. Clean
story for the paper's "scoped composition-level interposition"
contribution.

### Other use cases enabled

Dependency isolation testing, bounded chaos engineering, subgraph
extraction / decomposition, scoped observability, capability
attenuation at trust boundaries, behavioral diff between versions,
scoped profiling. All compose the same substrate pieces with
different strategy configurations applied to `between_subgraph`
boundaries.

## Open questions

- **v1 demo target.** If the research paper deadline requires a wasi:http
  replay demo, v1 and v2 collapse (resources have to ship). If the paper's
  non-HTTP eval leg is enough, v1 covers it.
- **Cargo as a splicer-time dependency.** Codegen-family builtins require
  `cargo` on PATH. Failure mode: precise error if absent. Acceptable?
- **Codegen caching.** Cache key probably
  `(WIT-hash, template-version, sdk-version) -> .wasm path`. Layout under
  `~/.cache/splicer/typed-builtins/`. Concrete location and invalidation
  rules TBD.
- **Strategy registration.** How do users opt in to third-party strategies?
  For v1, splicer ships a fixed registry of strategies; third-party slot in
  via path. v2+ may want an extension story.
- **Tier-3 vs tier-4 codegen modes.** Same template, two modes (with-import
  vs without-import). Determined by the strategy's tier classification.
  Codegen needs to know which mode per (builtin, target).

## References

- [`adapter-comp-planning.md`](./adapter-comp-planning.md): sibling planning,
  in particular the "one-per-signature case" section that originally framed
  the typed-codegen approach.
- [`tier2-generic-resource-handles.md`](./tier2-generic-resource-handles.md):
  the dual-of-this constraint at tier-2 (cell-array vs resource-wrapper
  ergonomics).
- [proxy-component](https://github.com/chenyan2002/proxy-component): prior
  art. Uses WAVE wire format and `wrapped-` namespace + `MockedResource`
  pattern. splicer adopts the structural approach, switches wire to cells.
- `wit/common/world.wit`: cell representation that flows end-to-end.
- `splicer-tool-sdk/`: target home for `WrapperStrategy` + helpers.
  Currently gated `publish = false` until tier-3/4 lands.
