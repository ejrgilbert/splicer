# Tier 2: Observation

**Status:** planned.

The middleware can see the function name, the types of the parameters
and return values, and the actual data being passed, but _cannot
modify_ any of it. The call flows through to the downstream unchanged;
_the middleware only observes_.

For the cross-tier framework (one-tier-per-middleware rule, async
convention, hook-trap propagation, chain composition), see
[`adapter-components.md`](../adapter-components.md).

## Value representation: `field-value`

Rather than serializing arguments to a string format (e.g. WAVE), the
adapter lifts the canonical-ABI values into a **structural list of
named fields** that preserves the WIT type tree. The shape covers every
WIT type:

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
`list<field>`. Every WIT type constructor maps to a distinct
`field-value` case, so the lifted value is self-describing — middleware
code can pattern-match exhaustively without consulting the schema, and
a generic trace consumer can render a value correctly even without the
WIT.

Type names inside `field-value` use **simple** names (`"color"`, not
`"my:pkg/types@1.0.0.color"`). The fully-qualified interface identity
surfaces at the **call** level; tier-2's per-call hook receives the
fully-qualified interface plus the function name, so simple names
inside values are always unambiguous.

The adapter handles all canonical-ABI lifting; the middleware works
entirely with the field representation. Tools that want a flat string
can format `list<field>` themselves; tools that want structured access
(jsonpath-style metric extraction, schema-aware routing) can walk the
tree directly. Splicer emits one format and lets the tool decide what
to do with it.

## Resource, stream, and future handles

**Resource, stream, and future handles** all surface as opaque
`(type-name, u64)` correlation IDs (`resource-handle`, `stream-handle`,
`future-handle`). The type-name string identifies the kind
(`"request"`, `"u8"` for `stream<u8>`, `"response"` for
`future<response>`); the `u64` is **not** a usable handle. The
middleware cannot invoke methods on it, read its contents, escape it
past the call boundary, or drop it. The adapter still owns
canonical-ABI ownership semantics (`own<R>`'s drop, `borrow<R>`'s
lifetime, stream/future cleanup); the ID is purely for reasoning about
identity (e.g. "this `request` was seen on `handle` and again as the
parent of the `body` resource three calls later").

For streaming protocols where the middleware actually wants to observe
**content** (e.g. logging an HTTP body element-by-element), tier-2 v1
deliberately does **not** support that. It's planned as a separate
opt-in interface (`splicer:tier2/stream-observer`) once a concrete use
case justifies the implementation cost.

## Tier-2 hook interfaces

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
extraction from request fields, content-based routing decisions,
throttling by request shape, authentication/authorization, security
policy enforcement, parameter validation. When applied at multiple WIT
boundaries simultaneously (e.g. `wasi:http/handler` plus
`wasi:http/types`), tier-2 also enables **span-based recording**: the
middleware can correlate the resource handles that surface across
nested calls within a single top-level invocation, then log the entire
causal trace as one record.
