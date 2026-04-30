# Tier 2: Observation

**Status:** planned.

The middleware can see the function name, the types of the parameters
and return values, and the actual data being passed, but _cannot
modify_ any of it. The call flows through to the downstream unchanged;
_the middleware only observes_.

For the cross-tier framework (one-tier-per-middleware rule, async
convention, hook-trap propagation, chain composition), see
[`adapter-components.md`](../adapter-components.md).

## Value representation: flattened cells

The adapter lifts canonical-ABI values into a **flat array of cells**.
Compound cells reference children by `u32` index into the same array
rather than by direct self-reference. A helper library (or hand-rolled
walker) presents this as a tree; the wire format itself is a single
linear `list<cell>` plus a root index.

This shape is dictated by a WIT-spec constraint: WIT does not yet
support recursive types ([component-model
issue #56](https://github.com/WebAssembly/component-model/issues/56)),
so the recursion is "compiled out" into an index-keyed array layout.

```wit
variant cell {
    // ── primitives ────────────────────────────────────────────────
    %bool(bool),
    integer(s64),                          // s8/s16/s32/s64/u8/u16/u32/u64
    floating(f64),                         // f32/f64 (widened)
    text(string),                          // string and char
    bytes(list<u8>),                       // list<u8> fast-path

    // ── structural / anonymous types ──────────────────────────────
    list-of(list<u32>),                    // child indices
    tuple-of(list<u32>),                   // child indices
    option-some(u32),                      // index of inner value
    option-none,
    result-ok(option<u32>),                // index, or none for unit ok
    result-err(option<u32>),               // index, or none for unit err

    // ── nominal types — name carried alongside the value ──────────
    record-of(record-info),
    flags-set(flags-info),
    enum-case(enum-info),
    variant-case(variant-info),

    // ── opaque correlation handles — adapter owns lifecycle ───────
    resource-handle(handle-info),
    stream-handle(handle-info),
    future-handle(handle-info),
}

record record-info {
    type-name: string,
    fields: list<tuple<string, u32>>,      // (field-name, child-index)
}
record flags-info  { type-name: string, set-flags: list<string>, }
record enum-info   { type-name: string, case-name: string, }
record variant-info {
    type-name: string,
    case-name: string,
    payload: option<u32>,                  // index of payload, or none
}
record handle-info { type-name: string, id: u64, }

record field-tree {
    cells: list<cell>,
    root: u32,
}

record field {
    name: string,
    tree: field-tree,
}
```

A function call's parameters surface as a `list<field>` (each field
carries its parameter name + a tree). The result surfaces as
`option<field-tree>` (none for void; results are unnamed in WIT, so no
`field` wrapper).

Every WIT type constructor maps to a distinct `cell` variant case, so
the lifted value is self-describing — middleware code can pattern-match
exhaustively without consulting the schema, and a generic trace
consumer can render a value correctly even without the WIT.

Type names inside cells use **simple** names (`"color"`, not
`"my:pkg/types@1.0.0.color"`). The fully-qualified interface identity
surfaces at the **call** level; tier-2's per-call hook receives the
fully-qualified interface plus the function name, so simple names
inside values are always unambiguous.

The adapter handles all canonical-ABI lifting; the middleware works
entirely with the cell representation. Tools that want a flat string
can format the tree themselves; tools that want structured access
(jsonpath-style metric extraction, schema-aware routing) can walk the
tree directly. Splicer emits one format and lets the tool decide what
to do with it.

## Resource, stream, and future handles

Resource, stream, and future handles all surface as opaque
`handle-info { type-name, id }` correlation records (`resource-handle`,
`stream-handle`, `future-handle`). The type-name identifies the kind
(`"request"`, `"u8"` for `stream<u8>`, `"response"` for
`future<response>`); the `u64` is **not** a usable handle. The
middleware cannot invoke methods on it, read its contents, escape it
past the call boundary, or drop it. The adapter still owns
canonical-ABI ownership semantics (`own<R>`'s drop, `borrow<R>`'s
lifetime, stream/future cleanup); the ID is purely for reasoning about
identity (e.g. "this `request` was seen on `handle` and again as the
parent of the `body` resource three calls later").

### What this means for resource-bearing target interfaces

Tier-2 lifting is bounded by what the canonical ABI exposes. For
target interfaces that pass resources by handle (e.g.
`wasi:http/handler@0.3.0`'s `handle: async func(request: request) -> ...`),
the middleware sees only the handle — not the request's headers,
method, body, or any other contents. The contents live behind methods
on the resource that the wasi:http host implements; from the
middleware's vantage point at the `handler` boundary, those are
unreachable.

To observe what's *inside* a resource, you have three paths:

1. **Multi-WIT instrumentation (recommended).** Apply tier-2 to **both**
   `wasi:http/handler` (sees the top-level call) **and**
   `wasi:http/types` (sees every method invocation on the request /
   response / headers / body resources). Correlate by handle id —
   `("request", 42)` at the handler boundary is the same logical
   request as `("request", 42)` flowing as `self` into
   `[method]request.headers`. Reconstruct the picture from the call
   stream. This is the canonical recorder pattern.
2. **Specialized middleware** (loses target-agnosticism). The
   middleware imports `wasi:http/types` directly and calls methods on
   the handles it receives. Now the middleware is HTTP-specific, not
   reusable across interfaces.
3. **Don't observe the contents.** A throttler / tracer / circuit
   breaker that only cares about call shape and handle correlation
   doesn't need to peer inside.

A future UX improvement (tracked in
[`docs/TODO/adapter-comp-planning.md`](../TODO/adapter-comp-planning.md))
is an `instrument-resources: true` rule modifier that auto-attaches
the same middleware to the resource-defining interface alongside the
target. For now, multi-WIT setup is explicit.

### Stream / future content observation

For streaming protocols where the middleware actually wants to observe
**content** (e.g. logging an HTTP body element-by-element), tier-2 v1
deliberately does **not** support that. It's planned as a separate
opt-in interface (`splicer:tier2/stream-observer`) once a concrete use
case justifies the implementation cost.

## Tier-2 hook interfaces

The tier-2 WIT package mirrors tier-1's split-by-hook structure:

```wit
package splicer:tier2@0.1.0;

interface before {
    use splicer:common/types@0.1.0.{call-id, field};
    on-call: async func(call: call-id, args: list<field>);
}

interface after {
    use splicer:common/types@0.1.0.{call-id, field-tree};
    on-return: async func(call: call-id, result: option<field-tree>);
}

interface trap {
    use splicer:common/types@0.1.0.{call-id};
    on-trap: async func(call: call-id, reason: string);
}
```

**Receiver convention.** For resource methods (`request.body()`, etc.),
the receiver `borrow<request>` / `own<request>` surfaces as the first
entry in `args` with `name: "self"`. The remaining declared parameters
follow in WIT-declaration order.

**Function naming.** `call-id.function-name` uses the **canonical-ABI**
function name verbatim — `"[constructor]request"`,
`"[method]request.body"`, `"[static]request.from-uri"`, `"handle"` for
plain functions. No special-casing or pretty-printing; the middleware
sees what the canonical ABI sees.

A middleware can export any non-empty subset:

- `before` only — pre-call observation (e.g. throttler that counts inbound shapes)
- `after` only — post-call observation (e.g. response logger)
- `before` + `after` — full lifecycle (e.g. tracer, recorder, metrics)
- `trap` (optional) — fires when the wrapped function traps

The adapter only fires hooks the middleware actually exports, so a
`before`-only middleware never pays the lift cost on the result.

**Result representation.** WIT functions have at most one result and
results are unnamed, so `on-return` carries `option<field-tree>`
directly (`none` for void functions, `some(tree)` otherwise) rather
than wrapping in a `field` with a synthetic name.

**Trap observability.** `on-trap` fires when an **async** downstream
function traps — the component model's async subtask state machine
exposes the trap-vs-return distinction, and the adapter inspects it
before re-propagating. For **sync** downstream functions, traps
propagate without firing `on-trap`: core wasm has no exception-handling
primitive that lets the adapter intercept synchronously. If you need
trap visibility on sync targets, file an issue; opt-in async wrapping
(at a per-call latency cost) is plausible future work.

**WIT definition:** [`wit/tier2/world.wit`](../../wit/tier2/world.wit)

**Good for:** request/response logging with payload inspection, metrics
extraction from request fields, content-based routing decisions,
throttling by request shape, authentication/authorization, security
policy enforcement, parameter validation. When applied at multiple WIT
boundaries simultaneously (e.g. `wasi:http/handler` plus
`wasi:http/types`), tier-2 also enables **span-based recording**: the
middleware can correlate the resource handles that surface across
nested calls within a single top-level invocation, then log the entire
causal trace as one record.

## Planned: resource-shape adapter-adapter

The cell-array wire format is chosen for **performance and polyglot
neutrality**: a single canonical-ABI lower per call, no per-language
helper library required *to be correct*. But the index-walking pattern
is awkward to write directly; languages without a splicer-provided
walker library will find the cells gnarly.

The plan is to ship a **second WIT package**, `splicer:tier2-resources`,
that exposes the same observation hooks but with the lifted value
wrapped as a `resource lifted-value` with lazy accessor methods
(`kind()`, `as-integer()`, `as-list() -> list<lifted-value>`, etc.).
Resource bindings are first-class in every wit-bindgen target, so
middleware authors writing in TS, Python, Go, or any other language
get an idiomatic API without splicer needing to ship per-language
helpers.

The bridge will be an **adapter-adapter component** that splicer ships
and auto-wires when it detects a middleware exporting
`splicer:tier2-resources/*`:

```
caller
  → splicer's tier-2 adapter  (lifts to cells, the canonical wire format)
      → adapter-adapter        (cells → resource methods, opt-in)
          → user middleware
              → handler
```

This pattern gives both worlds:

- **Default (cells, fast)**: middleware exports `splicer:tier2/*`,
  consumes the cell array directly, walks with the splicer-supplied
  Rust helper crate or its own walker. Single canonical-ABI lower per
  call, in-process traversal — sub-microsecond walk for typical
  HTTP-scale payloads.
- **Ergonomic (resources, polyglot)**: middleware exports
  `splicer:tier2-resources/*`, never touches indices. Works
  idiomatically in every language without a splicer-provided helper.

  **Runtime cost** (the price of opting in): every accessor on
  `lifted-value` is a component-boundary call. Walking a 50-field
  record is ~150 boundary crossings (~5–10 μs at wasmtime's current
  overhead) vs. ~250 ns for the direct-cells path — roughly **30×
  slower per walk**. For light-touch middleware (auth, throttling,
  tracer reading a few fields) this is irrelevant. For
  traversal-heavy middleware (logger, recorder dumping the entire
  tree) it's meaningful — at HTTP scale (~10 ms request budget),
  ~0.1% added latency per traversal, still acceptable for most use
  cases. If perf matters, drop the adapter-adapter and walk cells
  directly.

Tracked in
[`docs/TODO/adapter-comp-planning.md`](../TODO/adapter-comp-planning.md).
Not in scope for tier-2 v1; the cell wire format is forward-compatible
with this shim landing later.
