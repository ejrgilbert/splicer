# Adapter Components

When you splice middleware into a composition, the middleware component needs to
export the same interface it's being inserted on. A logging middleware that wraps
`wasi:http/handler` would normally need to import and export the full
`wasi:http/handler` interface — complete with all its resource types, error
variants, and async function signatures.

That's a lot of boilerplate, and it means every middleware is locked to one
specific interface. A logging component built for `wasi:http/handler` can't be
reused on `my:service/adder` without being rewritten.

**Adapter components** solve this. Instead of requiring middleware to match the
target interface signature, splicer generates a thin wrapper component, the
**adapter**, that bridges between a generic middleware WIT interface and the
specific target interface. The middleware author writes against a simple,
type-erased WIT contract; splicer handles all the type plumbing at composition
time.

## Middleware Tiers

Not all middleware needs the same level of access to function arguments and
return values. Splicer defines four tiers of middleware capability, each with
its own WIT interface. The generated adapter component knows which tier to use
based on which interfaces the middleware exports.

The four tiers split along two cuts: whether the middleware can see the call's
typed payload, and what it does with that visibility. Tiers 1 and 2 leave the
call flowing through unchanged (observation only). Tier 3 lets the middleware
modify what flows through; tier 4 lets it replace the downstream entirely.

Across all tiers, a middleware is just a regular component — it can declare
any imports it needs (`wasi:filesystem` for backing storage, `wasi:io` for
streams, custom interfaces for fixtures, etc.). The adapter only wires up
the middleware's tier-N export to the target interface; the middleware's
own imports are left untouched and get satisfied by the surrounding
composition (or by the host) like any other component would. This matters
most for tier 4 (a virt almost always needs a backend), but the rule is
the same everywhere.

All tier WIT worlds export their hook functions as **async**. The adapter
emits async dispatch unconditionally, so middleware authors write `async
fn` regardless of whether the wrapped target function is sync or async.
This keeps the adapter's async machinery uniform and lets the middleware
freely await imports of its own without needing a separate sync code path.

### Adapter behavior when a middleware hook traps

If a middleware's hook (any tier's `on-call`, `on-return`, `should-block`,
`on-trap`, etc.) itself traps — e.g. the middleware panics, dereferences
out-of-bounds memory, or otherwise hits an unrecoverable error — the
adapter **propagates the trap** rather than swallowing it.

Concretely: hooks run as async subtasks, so the adapter awaits the
subtask and inspects its terminal state. If the subtask is in `errored`
state, the adapter traps too. The runtime's backtrace points at the
adapter's dispatch wrapper at the hook-call site, so an operator can
tell from the backtrace alone that the trap originated in the middleware
hook (not in the wrapped target function).

The adapter does **not** attach a custom string reason to the
re-propagated trap (core wasm has no parameterized-trap primitive), and
it does **not** import a logging interface to write a more readable
error before trapping. Both are deliberate: keep the adapter
zero-imports beyond what its target requires, and rely on the runtime's
backtrace to identify the fault site.

The adapter does **not** silently swallow middleware traps and proceed
with the downstream call. A middleware whose code is broken should fail
loudly so the operator notices. A configurable
`on-middleware-trap: propagate | log | swallow` policy is plausible
future work if a concrete use case justifies it.

### Tier 1: Name-Only Hooks (currently supported)

The middleware receives the function name as a string and can run logic
before/after the downstream call, or conditionally block it. It never sees the
types or values of the function's parameters or return values.

**WIT definition:** [`wit/tier1/world.wit`](../wit/tier1/world.wit)

A middleware is tier-1 compatible when it exports **at least one** of the
interfaces defined in the tier-1 WIT package. The generated adapter only wires
up the hooks that are actually present, any non-empty subset is valid.

#### What "interface" means here (one middleware wraps N functions)

The unit of interposition is a **WIT interface**, not a single function. An
interface is an instance type that can export any number of functions.
Splicer's adapter wraps **every** function in the target interface with the
same middleware — the middleware doesn't get to pick and choose, but it can
discriminate at runtime via the `name` parameter the hooks receive.

Concrete shapes:

| Target interface       | Functions in it            | Adapter generates |
|------------------------|----------------------------|-------------------|
| `wasi:http/handler`    | `handle`                   | 1 wrapper         |
| `my:service/adder`     | `add`                      | 1 wrapper         |
| `my:service/math`      | `add`, `sub`, `mul`, `div` | 4 wrappers        |

All the wrappers share the same hook imports (`splicer:tier1/before` etc.).
When `add` is called, the adapter calls `before-call("add")`; when `div` is
called, the adapter calls `before-call("div")`. The middleware sees one
stream of hook calls with the function name as the discriminator — one
middleware, N functions.

##### If your middleware only cares about some of the functions

Because the adapter invokes every hook your middleware exports on
every wrapped call, **you pay the before/after/block round-trip
uniformly**, even for the calls your middleware will immediately
no-op. For a 4-function interface where your logging middleware only
cares about one, `before-call` still fires 4 × per mixed workload and
you filter by name inside the middleware. Typical per-hook cost is an
async subtask + name-string lower/lift; small in isolation, but it
scales linearly with the number of interposed functions the
middleware ignores.

There's no config-level way to restrict which functions are wrapped
yet — if you have a concrete use case (large fan-out interface,
per-function policy, measurable overhead on ignored calls), **please
[open an issue](https://github.com/ejrgilbert/splicer/issues)** with
details. A config-level `funcs: [...]` filter is on the roadmap (see
[`docs/TODO/adapter-comp-planning.md`](./TODO/adapter-comp-planning.md)) and
real use cases drive the priority.

**What the generated adapter does:**

For each function in the target interface, the adapter:

1. Calls `before-call(fn_name)` if the middleware exports `splicer:tier1/before`
2. Calls `should-block-call(fn_name)` if the middleware exports `splicer:tier1/blocking`;
   skips the downstream invocation when it returns `true` (void functions only)
3. Forwards the call to the handler with all arguments and return values passed through unchanged
4. Calls `after-call(fn_name)` if the middleware exports `splicer:tier1/after`

The adapter handles all canonical-ABI lifting/lowering, resource handle
threading, async machinery, and type plumbing internally. The middleware
component is completely decoupled from the target interface's type signature.

**Good for:** tracing, logging, rate limiting, access control (allow/deny),
circuit breakers (on/off), audit trails.

### Tier 2: Observation (planned)

The middleware can see the function name, the types of the parameters and
return values, and the actual data being passed, but _cannot modify_ any of it.
The call flows through to the downstream unchanged; _the middleware only
observes_.

Rather than serializing arguments to a string format (e.g. WAVE), the adapter
lifts the canonical-ABI values into a **structural list of named fields**
that preserves the WIT type tree. The shape covers every WIT type:

```wit
variant field-value {
    // primitives
    %bool(bool),
    integer(s64),                                // s8/s16/s32/s64/u8/u16/u32/u64
    floating(f64),                               // f32/f64 (widened)
    text(string),                                // string and char
    bytes(list<u8>),                             // list<u8> fast-path

    // structural / anonymous types — no declared name to carry
    list-of(list<field-value>),                  // list<T> for non-u8 T
    tuple-of(list<field-value>),                 // tuple<T...>
    option-some(field-value),                    // option::some(v)
    option-none,                                 // option::none
    result-ok(option<field-value>),              // result::ok — none if no T
    result-err(option<field-value>),             // result::err — none if no E

    // nominal types — type name carried alongside the value
    record-of(string, list<tuple<string, field-value>>),  // ("person", [("age", integer(30)), ...])
    flags-set(string, list<string>),                      // ("permissions", ["read", "write"])
    enum-case(string, string),                            // ("color", "hot-pink")
    variant-case(string, string, option<field-value>),    // ("allowed-destinations", "restricted", some(...))

    // opaque correlation handles — adapter owns lifecycle; middleware sees an id only
    resource-handle(string, u64),                         // ("request", 42) — resource type + id
    stream-handle(string, u64),                           // ("u8", 7)       — element type + id
    future-handle(string, u64),                           // ("response", 3) — element type + id
}
record field {
    name: string,
    value: field-value,
}
```

A function call's parameters and return values each surface as a
`list<field>`. Every WIT type constructor maps to a distinct `field-value`
case, so the lifted value is self-describing — middleware code can
pattern-match exhaustively without consulting the schema, and a generic
trace consumer can render a value correctly even without the WIT.

Type names inside `field-value` use **simple** names (`"color"`, not
`"my:pkg/types@1.0.0.color"`). The fully-qualified interface identity
surfaces at the **call** level; tier-2's per-call hook receives the
fully-qualified interface plus the function name, so simple names inside
values are always unambiguous.

The adapter handles all canonical-ABI lifting; the middleware works
entirely with the field representation. Tools that want a flat string can
format `list<field>` themselves; tools that want structured access
(jsonpath-style metric extraction, schema-aware routing) can walk the tree
directly. Splicer emits one format and lets the tool decide what to do
with it.

**Resource, stream, and future handles** all surface as opaque
`(type-name, u64)` correlation IDs (`resource-handle`, `stream-handle`,
`future-handle`). The type-name string identifies the kind (`"request"`,
`"u8"` for `stream<u8>`, `"response"` for `future<response>`); the `u64`
is **not** a usable handle. The middleware cannot invoke methods on it,
read its contents, escape it past the call boundary, or drop it. The
adapter still owns canonical-ABI ownership semantics (`own<R>`'s drop,
`borrow<R>`'s lifetime, stream/future cleanup); the ID is purely for
reasoning about identity (e.g. "this `request` was seen on `handle` and
again as the parent of the `body` resource three calls later").

For streaming protocols where the middleware actually wants to observe
**content** (e.g. logging an HTTP body element-by-element), tier-2 v1
deliberately does **not** support that. It's planned as a separate
opt-in interface (`splicer:tier2/stream-observer`) once a concrete use
case justifies the implementation cost.

#### Tier-2 hook interfaces

The tier-2 WIT package mirrors tier-1's split-by-hook structure:

```wit
package splicer:tier2@0.1.0;

record call-id {
    interface: string,    // "wasi:http/types@0.3.0" — fully-qualified
    function: string,     // canonical-ABI name, e.g. "[method]request.body"
}

interface before {
    on-call: async func(call: call-id, args: list<field>);
}

interface after {
    on-return: async func(call: call-id, result: option<field-value>);
}

interface trap {
    on-trap: async func(call: call-id, reason: string);
}
```

**Receiver convention.** For resource methods (`request.body()`, etc.),
the receiver `borrow<request>` / `own<request>` surfaces as the first
entry in `args` with `name: "self"`. The remaining declared parameters
follow in WIT-declaration order.

**Function naming.** `call-id.function` uses the **canonical-ABI**
function name verbatim — `"[constructor]request"`,
`"[method]request.body"`, `"[static]request.from-uri"`,
`"handle"` for plain functions. No special-casing or pretty-printing;
the middleware sees what the canonical ABI sees.

A middleware can export any non-empty subset:

- `before` only — pre-call observation (e.g. throttler that counts inbound shapes)
- `after` only — post-call observation (e.g. response logger)
- `before` + `after` — full lifecycle (e.g. tracer, recorder, metrics)
- `trap` (optional) — fires when the wrapped function traps

The adapter only fires hooks the middleware actually exports, so a
`before`-only middleware never pays the lift cost on the result.

**Result representation.** WIT functions have at most one result and
results are unnamed, so `on-return` carries `option<field-value>`
directly (`none` for void functions, `some(value)` otherwise) rather
than wrapping in a `field` with a synthetic name.

**Trap observability.** `on-trap` fires when an **async** downstream
function traps — the component model's async subtask state machine
exposes the trap-vs-return distinction, and the adapter inspects it
before re-propagating. For **sync** downstream functions, traps
propagate without firing `on-trap`: core wasm has no exception-handling
primitive that lets the adapter intercept synchronously. If you need
trap visibility on sync targets, file an issue; opt-in async wrapping
(at a per-call latency cost) is plausible future work.

**WIT definition:** `wit/tier2/world.wit` (not yet published)

**Good for:** request/response logging with payload inspection, metrics
extraction from request fields, content-based routing decisions, throttling
by request shape, authentication/authorization, security policy enforcement,
parameter validation. When applied at multiple WIT boundaries simultaneously
(e.g. `wasi:http/handler` plus `wasi:http/types`), tier-2 also enables
**span-based recording**: the middleware can correlate the resource handles
that surface across nested calls within a single top-level invocation, then
log the entire causal trace as one record.

### Tier 3: Transform (planned)

The middleware can see AND modify both the arguments going to the downstream
and the results coming back. The downstream is still invoked — the middleware
sits in the data path, not in place of it.

Modifications round-trip through the same structural `list<attr>`
representation used by tier 2. The adapter deserializes the modified
attribute list back into canonical-ABI values before forwarding to the
downstream (or returning to the caller).

**WIT definition:** `wit/tier3/world.wit` (not yet published)

**Good for:** request enrichment (adding headers, injecting context),
response transformation, payload encryption/redaction, content filtering,
A/B testing (routing different request variants to the same downstream),
retry-with-backoff that mutates request state between attempts.

### Tier 4: Virtualize (planned)

The middleware **replaces** the downstream entirely. There is no inner call;
the wrapper synthesizes the return value itself from the lifted parameters
and any state it carries. Downstream resource handles, when present in the
return type, can be fabricated by the wrapper or threaded through to a
backend it controls.

This is the tier where the wrapper *is* the downstream from the caller's
perspective. The caller can't tell the difference between a real
`wasi:http/handler` and a tier-4 implementation that synthesizes responses
locally.

**WIT definition:** `wit/tier4/world.wit` (not yet published)

**Good for:** WASI-Virt-style virtualization (intercepting `wasi:filesystem`
or `wasi:keyvalue` to redirect or mock), test mocks that synthesize fixed
responses, shadow replayers that serve a recorded trace back to the caller,
fuzzing harness backends that generate inputs from a model rather than
forwarding to a real implementation.

### Summary

| Tier | See call name | See typed data | Modify data | Bypass downstream | Status        |
|------|---------------|----------------|-------------|-------------------|---------------|
| 1    | yes           | no             | no          | partial (block)   | **supported** |
| 2    | yes           | yes            | no          | no                | planned       |
| 3    | yes           | yes            | yes         | no                | planned       |
| 4    | yes           | yes            | yes         | yes               | planned       |

The tiers split along two cuts in the adapter generator. Tiers 1 and 2 share
a **pass-through observation** emit path: arguments flow to the downstream
unchanged, hooks fire on the side. Tiers 3 and 4 share a **value synthesis**
emit path: the wrapper materializes canonical-ABI values from structural
attributes — tier 3 uses the synthesized values to call the downstream, tier
4 uses them as the return value directly and elides the downstream call
entirely.

Each tier strictly adds one capability over the previous. Middleware written
for a lower tier works unchanged when higher tiers become available — the
tier is determined by which WIT interfaces the middleware exports, and the
adapter generator picks the right strategy automatically.

### One tier per middleware

A given middleware component must implement **exactly one tier**. Within
that tier, any non-empty subset of the tier's interfaces is fine (e.g. a
tier-1 middleware can export any combination of `before` / `after` /
`blocking`), but exporting interfaces from multiple tier packages is
rejected at splice time.

This is by design, not a missing feature. Higher tiers strictly subsume
the capabilities of lower ones — tier 2's `on-call` already carries the
function name, so a tier-2-aware component never needs tier-1 hooks too.
If you want to combine behaviors (say, observation plus modification),
ship them as **separate components** and chain them via `inject: [...]`
in the splice config. That makes the layering visible at the
configuration level rather than hidden inside one component's exports.

### Composing middleware in a chain

A splice rule's `inject: [m1, m2, m3]` produces a chain where `m1` is the
outermost wrapper (closest to the caller) and `m3` is innermost (closest
to the downstream). Tiers 1-3 compose freely in any order — the resulting
behavior is always well-defined, though not always commutative:

- **Tier 1 (name only)** never sees or touches values, so its presence is
  invisible to other middleware. Slots in anywhere; commutes with
  everything.
- **Tier 2 (observe)** sees values but doesn't modify them, so its
  presence doesn't change what flows through to neighbors. Commutes with
  everything; what *it* observes depends on what its outer/inner
  neighbors decided to do.
- **Tier 3 (transform)** modifies values, so order matters: a tier-3
  closer to the caller sees args before any inner transformations are
  applied, and sees results after they've been applied. Two tier-3s
  produce different results in different orders. This is the same
  decorator-chain semantics as Express, Tower, Rack, etc. — well-defined
  for any order, just not commutative.

**Tier 4 is a chain terminator.** A tier-4 middleware replaces the
downstream entirely, so anything *past* it in the chain is unreachable —
no calls flow through. Tier 4 must therefore be the **innermost** entry
in `inject`. Splicer warns if it sees middleware listed after a tier-4
entry (the trailing entries can never fire).

Concrete walk-through with `inject: [t1, t2, t3]` and a single
`handle(req) → resp` call:

```
caller → t1.before("handle")                         // tier 1: name only
       → t2.on-call("handle", lifted-args)           // tier 2: observe
       → t3 lifts args, mutates → args', lowers      // tier 3: transform
       → downstream(args') → resp
       → t3 lifts resp, mutates → resp', lowers      // tier 3: transform back
       → t2.on-return("handle", lifted-resp')        // tier 2: observe post-transform
       → t1.after("handle")                          // tier 1: name only
       → caller gets resp'
```

Reorder the same three to `[t3, t2, t1]` and `t2` will observe the
post-transform args on the way in (because `t3` is now outside it) and
the pre-back-transform result on the way out — different snapshots, same
overall correctness.

## Writing a Tier-1 Middleware

A tier-1 middleware is a standard WebAssembly component that exports one or more
of the `splicer:tier1/{before,after,blocking}` interfaces. Here's a minimal
example in Rust (using `wit-bindgen`):

```rust
wit_bindgen::generate!({
    world: "type-erased-middleware",
    async: true,
    generate_all
});

use crate::bindings::exports::splicer::adapter::before::Guest as BeforeGuest;
use crate::bindings::exports::splicer::adapter::after::Guest as AfterGuest;
use crate::bindings::exports::splicer::adapter::blocking::Guest as BlockGuest;

pub struct MyMiddleware;
impl BeforeGuest for MyMiddleware {
    async fn before_call(name: String) {
        println!("[middleware] about to call: {name}");
    }
}

impl AfterGuest for MyMiddleware {
    async fn after_call(name: String) {
        println!("[middleware] finished calling: {name}");
    }
}

impl BlockGuest for MyMiddleware {
    async fn should_block_call(name: String) -> bool {
        println!("[middleware] blocking call to: {name}");
        true
    }
}
```

Compile this to `wasm32-wasip1` and convert to a component with
`wasm-tools component new`. Then reference it in your splice configuration:

```yaml
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
      provider:
        name: my-service
    inject:
      - name: my-middleware
        path: ./my-middleware.wasm
```

When you run `splicer splice`, it will:

1. Detect that `my-middleware` exports `splicer:tier1/before` and
   `splicer:tier1/after` (but not `wasi:http/handler@0.3.0` directly)
2. Classify it as tier-1 compatible
3. Generate an adapter component that bridges between the middleware's
   `splicer:tier1/*` interfaces and `wasi:http/handler@0.3.0`
4. Substitute the adapter into the composition in place of the middleware

The generated adapter appears in the `SpliceOutput::generated_adapters` list
(programmatic API) or as a file in the splits directory (CLI).

## How Splicer Detects Adapter Eligibility

When processing a splice rule, splicer checks each middleware component:

1. **Does it export the target interface directly?** If yes, no adapter is
   needed — the middleware is wired in as-is. A type fingerprint check ensures
   the middleware's export is structurally compatible with the interface it's
   being placed on.

2. **Does it export interfaces from exactly one `splicer:tierN/*` package?**
   If yes, splicer classifies the middleware as tier-N compatible and
   generates an adapter component automatically. The adapter file is
   written to the splits directory alongside the split sub-components.
   Within that tier, any non-empty subset of the tier's interfaces is
   valid.

3. **Does it export interfaces from multiple tier packages?** Splicer
   rejects with an error:
   ```
   middleware `my-middleware.wasm` exports interfaces from multiple tiers
   (tier 1: splicer:tier1/before; tier 2: splicer:tier2/observe).

   A middleware must implement exactly one tier. To combine behaviors,
   ship them as separate components and chain them in `inject: [...]`.
   ```
   See ["One tier per middleware"](#one-tier-per-middleware) for the
   rationale.

4. **None of the above?** Splicer emits a warning: the middleware doesn't
   match the target interface and isn't adapter-compatible. It can still
   be injected (the user may know something splicer doesn't), but type
   safety is unconfirmed.

## Adapter Component Internals (Brief)

For those curious about what's inside the generated `.wasm`: the adapter is a
self-contained WebAssembly component that contains two nested core modules (a
memory provider and a dispatch module) plus the canonical-ABI glue to lift and
lower between the component model and core Wasm. The dispatch module implements
the before/call/after/block sequencing in straight-line Wasm, using the
component model's async primitives (`waitable-set`, `subtask`, `task.return`)
for async function support.

The adapter handles sync functions, async functions, functions with string
parameters, functions with resource types, and functions with complex result
types (like `result<response, error-code>`) — all transparently. The middleware
component never needs to know about any of this.

For a low-level architecture walkthrough of the generator itself — module
layout, type-flow from cviz through `wit-parser` to emitted wasm, how
`wit-bindgen-core::abi::lift_from_memory` drives the `task.return` loads,
heterogeneous-variant widening, and what splicer still owns vs. inherits from
upstream — see [`adapter-internals.md`](./adapter-internals.md).

For broader planning notes on the tier-1 work, see
[`docs/TODO/adapter-comp-planning.md`](./TODO/adapter-comp-planning.md).
