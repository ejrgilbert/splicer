# Tier 2: Observation

**Status:** shipped.

The middleware can see the function name, the types of the parameters
and return values, and the actual data being passed, but _cannot
modify_ any of it. The call flows through to the downstream unchanged;
_the middleware only observes_.

For the shared framework that applies to every tier (one-tier-
per-middleware rule, async convention, hook-trap propagation,
chain composition), see [`adapter-components.md`](../adapter-components.md).

## Payload integrity

The complement to tier-1's
[payload isolation](./tier-1.md#payload-isolation): tier-2
middleware **can see the payload but can't modify it**. What
flows from caller to handler is bit-for-bit identical to what the
caller sent — the lifted `field-tree` the middleware reads is a
separate observation artifact, not the value being forwarded.

Three reinforcing reasons:

1. **The WIT contract.** The tier-2 hooks
   (`on-call(call, args)`, `on-return(call, result)`) return
   nothing. There's no shape by which the middleware can
   communicate a modified value back to the adapter; after a hook
   returns, the adapter forwards the originals.
2. **Shared-nothing memory.** The middleware receives the
   field-tree as data copied into *its own* linear memory by the
   canonical-ABI trampoline. Modifying that local copy has no
   effect on the adapter's original canonical-ABI flat values,
   which are what get forwarded downstream. (See
   [`adapter-internals.md`](../adapter-internals.md#shared-nothing-components-and-the-canon-abi-trampoline)
   for the cross-component memory model.)
3. **Handles are opaque correlation IDs, not usable handles.**
   When a resource (`own<R>` / `borrow<R>`) or `stream<T>` /
   `future<T>` / `error-context` crosses the boundary, the
   middleware sees a `handle-info { type-name, id }` correlation
   record, not the actual handle. The `u64` is not invokable —
   the middleware cannot call methods on the resource, read its
   contents, drop it, or escape it past the call. Without this,
   "observation only" would have a back door: a middleware that
   received a real `borrow<request>` could call
   `request.headers().append(...)` and silently mutate the
   payload. The adapter retains canonical-ABI handle ownership;
   the middleware just gets an identity tag useful for
   correlating the same handle's appearances across calls.

So tier-2 lets you build observation middleware — tracing,
metrics, content-aware logging, schema-aware routing — with the
guarantee that adding a middleware **cannot change call
semantics**. A handler downstream sees exactly the same bytes
whether it was called directly or through a tier-2 interposition.

Tier-3 relaxes this property by design: middleware sees the
payload and can modify it (the hook signatures grow a return
value). Tier-4 replaces the downstream entirely. Picking tier-2
is opting into observation without modification authority.

## Tier-2 hook interfaces

The tier-2 WIT package mirrors tier-1's split-by-hook structure:
two interfaces, `before` and `after`. `before::on-call(call,
args)` carries the function's params as a `list<field>`;
`after::on-return(call, result)` carries the return as
`option<field-tree>` (`none` for void). Both hooks are `async`.

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

The adapter only fires hooks the middleware actually exports, so a
`before`-only middleware never pays the lift cost on the result.

**Result representation.** WIT functions have at most one result and
results are unnamed, so `on-return` carries `option<field-tree>`
directly (`none` for void functions, `some(tree)` otherwise) rather
than wrapping in a `field` with a synthetic name.

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

## Value representation: flattened cells with side tables

The adapter lifts canonical-ABI values into a **flat array of cells**.
Compound cells reference children by `u32` index into the same array
rather than by direct self-reference. Nominal-typed metadata
(record-of, variant-case, etc.) lives in **per-kind side tables**
that cells reference by `u32` index — these are the `record-infos`
/ `flags-infos` / `enum-infos` / `variant-infos` / `handle-infos`
lists on `field-tree` in the WIT; the codegen docs call them "side
tables", the WIT names them `*-infos`, same thing either way. A
helper library (or hand-rolled walker) presents this as a tree;
the wire format itself is a single linear `list<cell>` plus a
small set of side lists plus a root index.

Two design constraints shape this layout:

1. **WIT lacks recursive types**
   ([component-model #56](https://github.com/WebAssembly/component-model/issues/56))
   — so the tree's recursion is encoded as `u32` indices into the
   cells array, not inline nesting.
2. **Cells need a small, uniform size** — canonical-ABI's
   fixed-stride variant layout would force every cell to pay
   padding for the widest case, so nominal metadata goes in side
   tables to keep cells tight.

The `cell` variant is a tagged union over three families of cases:

- **Primitive** cases (`bool`, `integer`, `floating`, `text`, `bytes`)
  hold their payload inline. `integer` widens every signed/unsigned
  width up to s64; `floating` widens f32 to f64; `text` carries both
  `string` and `char`; `bytes` is the `list<u8>` fast-path.
- **Structural** cases (`list-of`, `tuple-of`, `option-some`,
  `option-none`, `result-ok`, `result-err`) carry **child cell-
  indices** into the same cells array — never inline payloads.
- **Nominal** cases (`record-of`, `flags-set`, `enum-case`,
  `variant-case`, and the four `*-handle`s — `resource-handle`,
  `stream-handle`, `future-handle`, `error-context-handle`) carry
  a `u32` index into the per-kind side table that holds the
  type-name + structural metadata (record field tuples, enum/
  variant case-name, flag-set bit names, handle correlation id).

A `field-tree` bundles the cells slab, one side-table slice per
nominal kind, and the `root` cell index. A `field` wraps a tree
with the param name. A function's args surface as `list<field>`;
the result as `option<field-tree>` (none for void; results are
unnamed in WIT, so no `field` wrapper).

The authoritative schema lives in
[`wit/common/world.wit`](../../wit/common/world.wit).

Every WIT type constructor maps to a distinct `cell` variant case, so
the lifted value is self-describing — middleware code can pattern-match
exhaustively without consulting the schema, and a generic trace
consumer can render a value correctly even without the WIT.

### Memory savings from the side-table split

Without the split (metadata inline in cell variant cases), the
cell stride is dominated by the largest case's payload size
padded to alignment — every cell pays for the worst case
regardless of which case is present. Pulling nominal metadata
into side tables caps every cell payload at 8 bytes (`s64`),
collapsing the stride to whatever the smallest viable shape
allows.

For primitive-dominated trees (the realistic shape — record
leaves are mostly primitives) this is roughly **50% savings**
vs. the inline alternative. Record-heavy trees roughly break
even, because each nominal cell trades the padding it would have
paid for a side-table entry of comparable size. Never
meaningfully worse, often dramatically better.

The cost is one extra `tree.<kind>_infos[idx]` lookup in middleware
code per nominal cell. Helper libraries hide this; without one, the
indirection is mechanical.

### Middleware contract

Type names inside cells use **simple** names (`"color"`, not
`"my:pkg/types@1.0.0.color"`). The fully-qualified interface
identity surfaces at the **call** level; tier-2's per-call hook
receives the fully-qualified interface plus the function name, so
simple names inside values are always unambiguous.

The adapter handles all canonical-ABI lifting; the middleware
works entirely with the cell representation. Tools that want a
flat string can format the tree themselves; tools that want
structured access (jsonpath-style metric extraction, schema-aware
routing) can walk the tree directly. Splicer emits one format and
lets the tool decide what to do with it.

**Walking the cells by hand is awkward**, so splicer plans to ship
a Rust helper crate that wraps the `field-tree` in a typed walker
(cell-by-cell traversal, automatic side-table lookups, conversion
to native Rust types where it makes sense). Middleware authors in
other languages will eventually get a polyglot-friendly path via
the planned [resource-shape adapter-adapter](#planned-resource-shape-adapter-adapter)
described below. Neither is shipped yet; today, middleware authors
either use Rust and consume the cells directly, or hand-roll a
walker in their language of choice.

## Resource, stream, future, and error-context handles

Resource, stream, future, and error-context handles all surface as
opaque `handle-info { type-name, id }` correlation records
(`resource-handle`, `stream-handle`, `future-handle`,
`error-context-handle`). The type-name identifies the kind
(`"request"`, `"u8"` for `stream<u8>`, `"response"` for
`future<response>`; **empty** for `error-context` — the cell-disc
already names the kind and there is no nested type to surface). The
`u64` is **not** a usable handle. The middleware cannot invoke methods
on it, read its contents, escape it past the call boundary, or drop
it. The adapter still owns canonical-ABI ownership semantics
(`own<R>`'s drop, `borrow<R>`'s lifetime, stream/future cleanup); the
ID is purely for reasoning about identity (e.g. "this `request` was
seen on `handle` and again as the parent of the `body` resource three
calls later").

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

### `error-context` is id-only — host limitation

The canonical ABI defines `error-context.debug-message`, which would
let the wrapper read the debug string in its own component (no
cross-component hop needed for the string itself). We deliberately
do **not** call it today: wasmtime currently ships an incomplete
`error-context` implementation ("very incomplete" per its own
`wasm_component_model_error_context` config docstring). The
`error_context_transfer` libcall fired by the FACT trampoline
crashes with `unknown handle index` when an `error-context`
crosses any component boundary — including a wrapper interposed
via splicer or even a wac-shim in a fan-in topology. The wrapper's
wasm code never runs, so option-2 (read the debug message) cannot
be validated end-to-end on current hosts.

When wasmtime's error-context support matures, this will be upgraded
to surface the debug string. The on-the-wire shape will change (likely
a sibling cell variant, e.g. `error-context-message(string)`, or
extending `handle-info.type-name` to carry the message); middleware
that pattern-matches on `error-context-handle` today should be ready
to switch.

## Oversized lists trap

The lift codegen reserves slabs sized `count * elem_bytes` (cells
slab) and `len * 4` (per-list child-index buffer). Both go through
`cabi_realloc`, whose size param is canonical-ABI i32 (signed). When a
dynamic `len` would make the multiplication overflow signed i32 — at
roughly 134M cells (16-byte cell stride) or 536M list-of indices — the
wrapper traps via `unreachable` rather than wrapping silently and
under-allocating.

In practice this fires only on pathological or adversarial inputs.
The trade-off is **trap (loud, clean abort) vs. clip (truncate the
lifted view, keep the call running)**. Clipping would let the call
survive at the cost of a divergence between what the handler sees
(full list) and what the lifted view records (truncated), and the
wire format doesn't yet carry a "this list was truncated" marker —
so the conservative choice today is trap.

If a real workload hits these traps, please open an issue with the
call shape. The policy is revisitable, but we'd rather see concrete
call-size data than guess at the right cutoff up front.

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

- **Default (cells)**: middleware exports `splicer:tier2/*`, consumes
  the cell array directly, walks with the splicer-supplied Rust helper
  crate or its own walker. Single canonical-ABI lower per call,
  in-process traversal — no cross-component boundary calls during the
  walk itself.
- **Ergonomic (resources, polyglot)**: middleware exports
  `splicer:tier2-resources/*`, never touches indices. Works
  idiomatically in every language without a splicer-provided helper.

  **Runtime cost** (the price of opting in): every accessor on
  `lifted-value` is a component-boundary call, so a walk of an
  N-field record becomes O(N) boundary crossings vs. a single
  in-process traversal for the direct-cells path. The crossover
  where the resource path matters in practice depends on payload
  shape and wasmtime's per-crossing cost; neither is benchmarked
  yet. Light-touch middleware (auth, throttling, reading a few
  fields) is plausibly fine either way; traversal-heavy middleware
  (logger, recorder dumping the entire tree) is where the gap
  shows up. If perf matters, drop the adapter-adapter and walk
  cells directly.

Not in scope for tier-2 v1; the cell wire format is forward-compatible
with this shim landing later.
