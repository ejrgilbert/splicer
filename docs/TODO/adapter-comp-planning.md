# Adapter generator — planning and future work

Forward-looking notes for the adapter-component generator. The
cross-tier framework is documented in
[`adapter-components.md`](../adapter-components.md), with per-tier deep
dives under [`tiers/`](../tiers/). The architecture walkthrough lives
in [`adapter-internals.md`](../adapter-internals.md). This file focuses
on what hasn't been built yet.

## Middleware tier roadmap

Tiers 1 and 2 are shipped. Tiers 3 and 4 are planned. The
user-facing taxonomy and per-tier WIT shapes live in
[`adapter-components.md`](../adapter-components.md) and the per-tier
docs under [`tiers/`](../tiers/); this section captures only the
open design questions that aren't settled there.

| Tier | New capability                                       | Status      |
|------|------------------------------------------------------|-------------|
| 1    | see function name                                    | **shipped** |
| 2    | observe typed args / results (no modify)             | **shipped** |
| 3    | modify typed args / results, downstream still called | planned     |
| 4    | replace the downstream entirely (virtualize)         | planned     |

### Open design questions for tier 3 / 4

- **Tier-3 short-circuit.** Tier 3 mutates in-flight values but still
  forwards to the downstream. Should it have a way to bail (return a
  synthesized result without calling the downstream)? If yes, that's
  basically a per-call escape hatch into tier-4 behavior — worth
  thinking through whether it's a separate hook or a return-shape
  signal from `on-call`. (Tier-3 short-circuit would blur the
  one-tier-per-middleware rule; alternative is to require users to
  ship a separate tier-4 component for that case.)

### Tier 4 absorbs the "one-per-signature" cases

The original "one-per-signature" section below described middleware
that has to *fabricate* structurally valid values from scratch
(fuzzers, mocks, property harnesses). With tier 4 in the picture,
those use cases land naturally: a tier-4 middleware exports the
target interface's tier-4 world and synthesizes the return value
itself. The Rust-codegen path is still useful for built-ins that want
`arbitrary`-style auto-generation, but it's now an implementation
strategy *for tier 4*, not a separate fourth category.

## Auto-instrument resources alongside their target

When a tier-2 (or tier-3) splice rule targets an interface that
**uses resources from a sibling interface** (very common in WIT — e.g.
`wasi:http/handler` uses `request` / `response` from
`wasi:http/types`), the user currently has to write two splice rules
to observe both the top-level call AND the resource methods that fire
during it. Verbose and easy to forget.

A reasonable UX addition: a rule modifier like
`instrument-resources: true` that asks splicer to auto-attach the
same middleware to every interface that defines resources used by
the target. The middleware sees the top-level call AND the resource
methods, correlated naturally by handle id.

```yaml
rules:
  - before:
      interface: wasi:http/handler@0.3.0
      provider: { name: my-service }
    instrument-resources: true     # also wire to wasi:http/types automatically
    inject:
      - name: my-logger
        path: ./logger.wasm
```

Implementation sketch:

- During contract validation, walk the target interface's WIT to
  collect every resource type referenced.
- For each resource, identify the interface it's defined in (the
  resource owner).
- Generate an additional internal splice rule (the user doesn't see
  it) for each owner interface, with the same middleware and chain
  shape.
- Emit a one-line info log so users know what got auto-wired.

When to build: once a user complains about the multi-WIT setup
ceremony. Tier 2 has shipped, so the underlying observation surface
is in place; only the UX modifier is missing.

## Span-based recording and record/replay

Tier 2 (record) + tier 4 (replay) is the canonical capture-and-relive
pair, but several design pieces aren't worked out yet.

### The span / correlation problem

Recording is interesting only when applied to **multiple WIT
boundaries at once**. A trace of just `wasi:http/handler::handle`
captures the request and response, but everything that happened
*inside* that handler — header reads, body chunk pulls, kv lookups,
filesystem writes — is invisible. To record those, the same tier-2
middleware has to also be attached to `wasi:http/types`,
`wasi:keyvalue/store`, `wasi:filesystem/preopens`, etc.

That makes the recorder see a **stream** of calls from many
interfaces. To reconstruct what happened during one top-level
invocation, it has to group those calls into a span. The grouping
identity needs to come from the adapter, not from the middleware
guessing — middleware guessing breaks under concurrency (two
in-flight `handle` calls would see their inner `request.body` reads
intermixed in the hook stream with no way to disambiguate).

The shipped `call-id` (in `wit/common/world.wit`) carries
`(interface-name, function-name, id: u64)` — the `id` field is the
per-invocation correlation token. Span-based recording across
nested calls still needs a way to relate inner-call ids back to
their enclosing top-level span; that's the unresolved part.

A sketch of what `call-id` might need to grow:

```wit
// shipped today: interface-name, function-name, id: u64
// span work would add:
//   parent: option<u64>   — immediate-caller call id, set when
//                           the adapter is nested under another
//                           instrumented boundary
```

Open questions:

- **How does the adapter learn the parent's call id?** A natural
  answer: the adapter at the **outermost** instrumented boundary
  threads its own call id through async-context state (or
  task-local storage) so inner adapters at lower boundaries can
  read it and populate `parent`. Needs a concrete mechanism; the
  component model's `task` API may or may not give enough plumbing
  here.
- **What about non-tree fan-in?** If two top-level calls share a
  resource handle (e.g. a long-lived `wasi:keyvalue::bucket`), inner
  calls on that resource may legitimately span both top-level spans.
  Probably modelled as `parent: list<u64>` or a separate "resources
  alive across spans" view; design is open.
- **Does the recorder export the span tokens, or are they internal
  bookkeeping?** Replayers care about call ordering within a span,
  not the token itself. Trace format probably stores ordered call
  groups keyed by span-internal index, not by `u64`.

### Replayer as tier-4

A replayer is a tier-4 component that exports the target interface
and consumes a recorded trace as state (data segment, imported
`wasi:filesystem` read, etc.). On each call it looks up the next
recorded call for that interface/function in its span, returns the
recorded result, and advances the cursor.

Open questions:

- **Trace format identity.** Trace metadata header records
  `(interface-id, schema-hash)` for each instrumented interface. The
  replayer refuses to load a trace whose schema-hash doesn't match
  the WIT it was generated against — protects against
  silently-broken replay when the WIT evolves.
- **Span replay determinism.** If the recorded trace contains
  concurrent calls within one span, what order does the replayer
  serve them in? Probably "in recorded order, regardless of
  caller-side concurrency"; means the replayer needs to gate calls
  until the predecessor in trace order has been served.
- **Resource handle correlation across record→replay.** The
  recording sees `resource-handle("request", 42)`. The replay needs
  to mint a fresh handle for the same role. Probably: replayer
  rewrites recorded `u64` IDs through a per-span identity map as it
  serves calls. Needs care for resources that escape the span (rare
  but possible in `wasi:keyvalue`).

### Recorder as tier-2

A recorder is a tier-2 component that observes the lifted
`list<field>` for each call in its span and writes them out (data
segment, `wasi:io` stream, custom sink interface). Tier-2 has
shipped, so this is buildable today modulo the span / correlation
question. Trace format design is the remaining open question.

## Multi-middleware chain diagnostics

The chain composition rules themselves are settled in the user-facing
doc — tiers 1-3 compose freely, tier 4 is a chain terminator, ordering
of tier-3s matters but is well-defined. What's still open is **how
loud splicer should be about questionable configurations**.

Concrete diagnostics worth adding:

- **Reject (hard error): middleware after a tier-4 entry.** Anything
  past a tier-4 in `inject: [...]` is unreachable. The current plan
  is a warning at splice time; promoting to a hard error costs
  nothing and prevents silent dead-code.
- **Warn: tier-3 chain whose ordering looks accidental.** E.g. two
  tier-3 transformers where one is `redact-pii` and the other is
  `compress` — putting `compress` outside `redact-pii` means the
  PII gets compressed before redaction, which is almost certainly
  unintended. Hard to detect generically (we don't know what the
  middleware does); could surface as a `splicer doctor`-style
  command that lints config patterns the user opts into.
- **Info: chain summary output.** When `splicer splice` runs, print
  a one-line per-rule chain visualization showing tier per entry —
  helps users see what they configured.

No code changes needed for the chain mechanism itself; this is
purely a UX / diagnostics question.

## Tier-2 future hook: `on-trap`

A trap-observability hook (`on-trap(call, reason)`) was scoped but
intentionally not shipped with tier-2. The motivating use case is
real: instrumenting a target interface and seeing when a
downstream call fails. The blocker is at the runtime layer rather
than splicer's codegen — canon-async on current wasmtime releases
propagates child-task traps as wasm traps that unwind the parent's
stack, so the parent guest never gets a chance to observe the
trap before unwinding alongside it. There's no `Status::Failed` or
`Event::TaskFailed` for the parent's wait-loop to dispatch on,
neither for guest-implemented nor host-implemented targets.

Wiring `on-trap` would require either (1) canon-async growing a
guest-visible terminal-error event the parent can poll for, or (2)
the adapter wrapping every async call in an exception-catching
shell. Both depend on upstream evolution that may or may not
happen; revisit when upstream lands the event semantics.

## Per-tier performance characterization

Every tier above 1 lifts canonical-ABI values into `field-value`
trees on every call, then (for tier 3) lowers them back. That cost
scales with payload size, not just call count.

Worth measuring before locking in the design:

- **Tier 1 baseline.** Sub-microsecond per hook call on a mid-size
  multi-function interface. Already in the perf doc.
- **Tier 2 per-call lifting.** How does it scale with payload size?
  A 1MB HTTP body should hit the `bytes` fast path (no per-element
  variant boxing); a 10k-element `list<u32>` won't. Need numbers for
  representative shapes.
- **Tier 2 multi-boundary recording overhead.** When a single
  `wasi:http::handle` invocation triggers 50 inner calls on
  `wasi:http/types` + `wasi:keyvalue` + `wasi:filesystem`, the
  recorder pays the lift cost on every one. Aggregate cost matters
  more than per-call.
- **Tier 3 round-trip cost.** Lifting + middleware processing +
  lowering back. Probably 2× tier 2 plus the middleware's own work.
- **Tier 4 vs direct call.** A tier-4 middleware replaces the
  downstream, so the relevant comparison isn't "wrapping overhead"
  but "would the same logic written as a normal component be
  faster?" The answer should be "no, modulo the lift overhead on the
  way in," but worth confirming.

Action: add benchmarks to `bench/` once tier 2 lands. Don't design
tier 3 / 4 around a perf model that hasn't been measured.

## The "one-per-signature" case

Some middleware genuinely can't be expressed generically over
serialized values because it must **fabricate structurally valid new
values from scratch**. This requires knowing the full type structure
at code-generation time, not just at runtime.

Known one-per-sig cases:

- **Type-generating fuzzer** — must construct valid values of every
  parameter type from raw random bytes. Mutation-based fuzzers (start
  from a real value, perturb the WAVE string) fit in tier 2.
- **Mock / stub generator** — must return a valid fake of the return
  type. Replay from a recorded trace fits tier 2 (the WAVE bytes
  already exist); mocks that synthesize responses from scratch are
  one-per-sig.
- **Property-based test harness** — must generate and shrink typed
  counterexamples; shrinking requires constructing smaller valid
  values, not just mutating existing ones.
- **Argument defaulting / enrichment** — filling in missing or zero
  fields requires knowing which fields are optional vs required and
  what sensible defaults look like per type.

### Implementation approach: Rust codegen, not raw wasm

The tempting alternative is to generate the wasm component directly
using `wirm` or `wasm-encoder`. For tiers 1 and 2 that's the right
tool — the adapter is pure dispatch glue with no value construction
from scratch. For the one-per-sig cases above it would be an enormous
amount of work: canonical-ABI lowering/lifting per WIT type, recursive
valid-value construction per WIT type (records, variants, lists,
options, resources), and random value generation over all of that.

The [`proxy-component`](https://github.com/chenyan2002/proxy-component)
project demonstrates a much leaner path: generate a small Rust file
using `syn` / `quote`, then compile it with `cargo`. This works
because `wit-bindgen` already derives `Arbitrary` on every generated
type, so the entire type-correct random value construction reduces
to:

```rust
let mut u = Unstructured::new(&random_bytes);
let value: SomeWitType = u.arbitrary().unwrap();
```

The actual codegen in `proxy-component` (`generate_fuzz_func`) is
only ~120 lines of `quote!` macros. The hard type-specific work is
fully delegated to `wit-bindgen` + `arbitrary`, neither of which
needs to be re-implemented.

For natively-provided one-per-sig middleware (fuzzer, mock, property
harness), splicer would generate the complete component. There is no
separate "strategy" component. The algorithm lives in splicer's Rust
code generator, and `wirm` is not involved. The cost is an external
`cargo build` step, but since these are code-generation artifacts
(not runtime operations), that's acceptable.

### Generation strategy summary

Tier 1 and tier 2 both use `wasm-encoder` + `wit-bindgen-core::abi`
for direct core-module construction; tier 2 additionally lifts
canonical-ABI values into the `field-tree` cell-array representation
defined in `wit/common/world.wit` (no WAVE serialization on the wire
path). Tier 3 will reuse the tier-2 lift plus add a lower-back path
once it's specified. The one-per-sig case (fuzzer / mock) is where
the strategies diverge: Rust codegen via `syn` / `quote` +
`wit-bindgen` + `arbitrary`, then an external `cargo build` — the
`arbitrary` derive handles all type complexity, so the codegen stays
small.

Useful references for tier-3 / one-per-sig work:

- [`wit-dylib`](https://github.com/bytecodealliance/wasm-tools/tree/main/crates/wit-dylib)
  — dynamic-linking bindings generator with canonical-ABI lift/lower
  codegen patterns worth studying.
- [Example in `wit-dylib/src/bindgen.rs`](https://github.com/bytecodealliance/wasm-tools/blob/main/crates/wit-dylib/src/bindgen.rs#L768)
  — how it generates lift code.

## Per-function interposition filter

Today a tier-1 adapter wraps **every** exported function of the
target interface with the same middleware. The middleware can filter
at runtime via the `name` param, but the hook round-trip still fires
on every call — including the ones the middleware immediately no-ops.
That's fine for single-function interfaces like `wasi:http/handler`
but gets expensive as interfaces grow.

### Proposal

Optional `funcs: [...]` include-list per injection in the splice
config. When present, the adapter emits a dispatch wrapper only for
the listed functions; the rest become direct
`alias export <handler_inst> "<func_name>"` — zero runtime cost for
excluded funcs, zero coupling between the middleware and specific
target names.

```yaml
rules:
  - before:
      interface: my:service/math
    inject:
      - name: metrics-mdl
        path: ./metrics.wasm
        funcs: [add, div]   # only wrap these; sub/mul pass through
```

### Implementation sketch

- `SpliceRule` / `Injection` grows an `Option<Vec<String>>`.
- `extract_adapter_funcs` partitions the interface's functions into
  `(wrapped, passthrough)` using the filter. The passthrough list
  just needs the name + signature enough for `alias export`.
- `build_adapter_bytes` emits dispatch wrappers for `wrapped` (same
  as today) and direct aliases for `passthrough`; both groups end up
  under the same target-interface export instance.
- `validate_contract` grows a new check: names in `funcs` must exist
  in the target interface, reported next to the existing "available
  interfaces" diagnostic.

Nothing changes in the closure walker, the canonical-ABI machinery,
or the memory module — this is purely a phase-1 dispatch decision.

### When to build it

Hold off until there's a concrete multi-function target where the
runtime-hook-per-excluded-call overhead is a real pain, or a user
hits the "my middleware shouldn't need to know the function names of
every target it attaches to" decoupling problem. Until then the
include-list is a solution looking for a problem.

Open design questions for when we revisit:

- Exclude-list form (`except_funcs: [...]`) as a convenience for
  "wrap everything except these"? Keep to a single form for v1.
- Glob / regex patterns? Probably not — function names are
  well-defined at config time and a bounded list is unambiguous.
- Interaction with tier 2 / tier 3, where filtering also affects
  whether we need to lift/lower payloads — spec this when we're
  closer to tier 2.

## Built-in middleware keyword

Today, adding any middleware means the user writes a component
(`wit-bindgen::generate!`, implement the tier-1 guest traits, compile
to wasm, point the YAML at the file). That's a lot of ceremony for
well-known cases like tracing, logging, OpenTelemetry spans, or
fuzzing where the middleware's behavior is entirely standard.

### Sketch

A `builtin:` keyword in the YAML that names a splicer-provided
middleware. No file path, no hand-authored component:

```yaml
rules:
  - before:
      interface: wasi:http/handler@0.3.0
    inject:
      - builtin: logging
      - builtin: otel
        config:
          endpoint: http://collector:4317
          service_name: my-svc
      - name: my-custom-mdl        # hand-authored still works alongside
        path: ./mine.wasm
```

Splicer resolves `builtin: X` to either a pre-built component it
ships with, or a generated one — transparently to the user. The
interesting entries cover different mechanisms:

- **`logging` / `tracing`** — pure tier 1 (name-only). Ship as
  a bundled `.wasm` blob in the splicer binary. The current
  `otel-bare-logs` / `otel-bare-spans` / `otel-bare-metrics`
  builtins under `builtins/` already cover this shape.
- **`otel`** — tier 2 (value-aware spans with request fields).
  Tier 2 has shipped, so the gate is open; the value-aware
  builtins can now be authored.
- **`fuzz` / `mock`** — one-per-sig. Generated via the Rust-codegen
  path described in the "one-per-signature" section above.

### Open design questions for when we revisit

- **Where do built-in components live?** Embedded in the splicer
  binary (simple, but grows binary size and forces lockstep versioning)
  vs. a separate registry of published components fetched on first
  use (leaner binary, but adds a network + supply-chain surface) vs.
  a sibling crate that builds them locally (clean from-source, awkward
  for `cargo install` distribution).
- **Typed vs free-form config.** Typed per-builtin gives us
  compile-time-like validation of `config:` keys (misspell `endpoint`
  as `endoint` and get a parse error, not a runtime surprise) but
  requires a Serde schema per builtin. Free-form map is simpler to
  implement but offers no validation. For the UX goal here, typed
  seems to win.
- **How does config reach the component at runtime?** `wasi:config/store`
  imports, a bundled data segment, env vars, or custom component-level
  imports splicer wires up at compose time. Each option has different
  implications for what the built-in component itself looks like and
  whether the same mechanism generalizes across all builtins.
- **Do users see the tier?** `builtin: logging` vs. `builtin: { kind:
  tier1, name: logging }`. The point of built-ins is UX, so leaning
  transparent — the tier is a static property of the registry entry.
- **Namespacing / extensibility.** `builtin: otel` (bare) vs.
  `builtin: splicer:otel` (namespaced). Matters if we ever want
  third-party built-in registries. For v1: bare names, splicer owns
  the namespace, design extensibility later.
- **Composition / ordering.** Already free via the existing
  `inject: [...]` list — each entry picks `builtin:` or `name: +
  path:` independently, ordering drives the call stack the same way
  it does today.
- **MVP scope.** Probably pick two that exercise different paths —
  one bundled (`logging` proves the embedded-blob path) and one
  generated (`fuzz` proves the Rust-codegen path) — so both arms of
  the design land at once. `otel` is the obvious third candidate
  now that tier 2 has shipped.

### Interaction with other planning items

- **Tier 3 roadmap** — built-ins that need to *modify* values can't
  ship until tier 3 does. Tier 2 (value observation) has shipped, so
  value-aware read-only built-ins (otel, content-aware logging) are
  unblocked.
- **One-per-signature case** — `fuzz` / `mock` are the direct
  motivating examples for that section's Rust-codegen path. If we
  build built-ins before tier 2, the first generated built-in
  exercises exactly that pipeline.
- **Per-function interposition filter** — `funcs: [...]` should
  compose with `builtin:` cleanly (e.g., run `otel` only on `handle`
  but not `ping`). No design conflict, just a test-matrix entry.

## Canonical-ABI gaps

Two known limitations that still surface as `anyhow::bail!` errors:

- **Flat params / results > 16 — pointer-form lowering.** The
  canonical ABI collapses to `(i32)` pointer form when a function's
  flat representation exceeds the `MAX_FLAT_PARAMS` cap (16, defined
  in `abi/compat.rs`). Tier-1 and tier-2 both bail at this boundary
  today instead of silently declaring wrong core types.
  Implementing pointer-form needs: `params_are_ptr` /
  `results_are_ptr` flags carried through the per-fn dispatch
  records, pointer-form type declarations in every dispatch emitter,
  and a memory-layout buffer reservation for the spilled args.

- **Anonymous compound types as top-level results.** When a Record /
  Variant / Enum appears as a func result but isn't in
  `iface.type_exports` (unusual in WIT-compiled interfaces, but
  legal at the component-model level), the adapter's export-instance
  construction can't re-export the compound — the binary fails
  validation with "instance not valid to be used as export." Fix:
  synthesize names + auto-export in the export-emit pass. Low
  priority since real WIT always names its compounds.
