# Tier 1: Name-Only Hooks

**Status:** currently supported.

The middleware receives a **call identity** — the target interface name
plus the function name — and can run logic before/after the downstream
call, or conditionally block it. It never sees the types or values of
the function's parameters or return values.

**WIT definition:** [`wit/tier1/world.wit`](../../wit/tier1/world.wit)

A middleware is tier-1 compatible when it exports **at least one** of the
interfaces defined in the tier-1 WIT package. The generated adapter only
wires up the hooks that are actually present, any non-empty subset is
valid.

## Call-id shape

Every tier-1 hook takes a `call-id` record carrying the target
interface (fully-qualified), the canonical-ABI function name, and a
monotonic per-instance id for correlating `on-call` / `on-return` of
the same invocation:

```wit
record call-id {
    interface-name: string,    // "wasi:http/handler@0.3.0"
    function-name: string,     // "handle", "[method]request.body", ...
    id: u64,                   // monotonic per-instance call id
}
```

`call-id` is shared across all tiers via the
[`splicer:common`](../../wit/common/world.wit) package, so middleware
authors who later move from tier 1 to tier 2 see the same call-identity
shape — only the payload widens.

## Payload isolation

Tier-1's "name-only" framing isn't just an ergonomic simplification —
it's a **structural security property**: the middleware never sees
the call's payload bytes, and can't. Two reinforcing reasons:

1. **The WIT contract.** The tier-1 hooks
   (`on-call(call)`, `on-return(call)`, `should-block(call)`) take
   *only* `call-id`. There's no parameter shape that could carry
   the args or the result into the middleware.
2. **Shared-nothing memory.** Even if a middleware tried to peek,
   it can't. Components have isolated linear memories, and the
   payload bytes live in the adapter's memory (during the wrapper
   body) and the handler's memory (during the downstream call).
   The middleware's wasm has no way to dereference into either.
   (See [`adapter-internals.md`](../adapter-internals.md#shared-nothing-components-and-the-canon-abi-trampoline)
   for the cross-component memory model.)

So tier-1 lets you compose middleware into a call path with a
strong guarantee: it can throttle, authenticate, route, or log by
call identity, but **cannot see, log, exfiltrate, or be tricked
into leaking the payload bytes**. Useful for sensitive-data
contexts — auth interceptors in front of secrets, audit logging
over PII, rate-limiting on financial APIs — where the call-shape
metadata is enough to make the policy decision but you don't want
the policy code inside the trust boundary of the payload.

Higher tiers relax this property by design: tier-2 shows the
middleware a lifted typed view of the payload (read-only
observation), tier-3 lets the middleware modify the payload
in flight, tier-4 replaces the downstream entirely. The strongest
payload-isolation guarantee lives at tier-1.

## What "interface" means here (one middleware wraps N functions)

The unit of interposition is a **WIT interface**, not a single function.
An interface is an instance type that can export any number of functions.
Splicer's adapter wraps **every** function in the target interface with
the same middleware — the middleware doesn't get to pick and choose, but
it can discriminate at runtime via the `function-name` field on the
`call-id` it receives.

Concrete shapes:

| Target interface       | Functions in it            | Adapter generates |
|------------------------|----------------------------|-------------------|
| `wasi:http/handler`    | `handle`                   | 1 wrapper         |
| `my:service/adder`     | `add`                      | 1 wrapper         |
| `my:service/math`      | `add`, `sub`, `mul`, `div` | 4 wrappers        |

All the wrappers share the same hook imports (`splicer:tier1/before`
etc.). When `add` is called, the adapter calls
`on-call({ interface-name: "my:service/math", function-name: "add" })`;
when `div` is called, the adapter passes `function-name: "div"`. The
middleware sees one stream of hook calls with the function name as the
discriminator — one middleware, N functions.

### If your middleware only cares about some of the functions

Because the adapter invokes every hook your middleware exports on every
wrapped call, **you pay the before/after/block round-trip uniformly**,
even for the calls your middleware will immediately no-op. For a
4-function interface where your logging middleware only cares about one,
`on-call` still fires 4 × per mixed workload and you filter by name
inside the middleware. Typical per-hook cost is an async subtask +
two-string lower/lift; small in isolation, but it scales linearly with
the number of interposed functions the middleware ignores.

There's no config-level way to restrict which functions are wrapped yet
— if you have a concrete use case (large fan-out interface, per-function
policy, measurable overhead on ignored calls), **please [open an
issue](https://github.com/ejrgilbert/splicer/issues)** with details. A
config-level `funcs: [...]` filter is on the roadmap (see
[`docs/TODO/adapter-comp-planning.md`](../TODO/adapter-comp-planning.md))
and real use cases drive the priority.

## What the generated adapter does

For each function in the target interface, the adapter:

1. Calls `on-call(call_id)` if the middleware exports `splicer:tier1/before`
2. Calls `should-block(call_id)` if the middleware exports
   `splicer:tier1/blocking`; skips the downstream invocation when it
   returns `true` (void functions only)
3. Forwards the call to the handler with all arguments and return values
   passed through unchanged
4. Calls `on-return(call_id)` if the middleware exports `splicer:tier1/after`

The adapter handles all canonical-ABI lifting/lowering, resource handle
threading, async machinery, and type plumbing internally. The middleware
component is completely decoupled from the target interface's type
signature.

**Good for:** tracing, logging, rate limiting, access control
(allow/deny), circuit breakers (on/off), audit trails.

## Writing a Tier-1 Middleware

A tier-1 middleware is a standard WebAssembly component that exports one
or more of the `splicer:tier1/{before,after,blocking}` interfaces.
Here's a minimal example in Rust (using `wit-bindgen`):

```rust
wit_bindgen::generate!({
    world: "type-erased-middleware",
    async: true,
    generate_all
});

use bindings::exports::splicer::tier1::before::Guest as BeforeGuest;
use bindings::exports::splicer::tier1::after::Guest as AfterGuest;
use bindings::exports::splicer::tier1::blocking::Guest as BlockGuest;
use bindings::splicer::common::types::CallId;

pub struct MyMiddleware;

impl BeforeGuest for MyMiddleware {
    async fn on_call(call: CallId) {
        println!("[middleware] about to call {}#{}",
                 call.interface_name, call.function_name);
    }
}

impl AfterGuest for MyMiddleware {
    async fn on_return(call: CallId) {
        println!("[middleware] finished {}#{}",
                 call.interface_name, call.function_name);
    }
}

impl BlockGuest for MyMiddleware {
    async fn should_block(call: CallId) -> bool {
        println!("[middleware] blocking {}#{}",
                 call.interface_name, call.function_name);
        true
    }
}

bindings::export!(MyMiddleware with_types_in bindings);
```

The middleware's WIT world declares both packages as exports/deps:

```wit
package my:middleware@1.0.0;

world type-erased-middleware {
    export splicer:tier1/before@0.2.0;
    export splicer:tier1/after@0.2.0;
    export splicer:tier1/blocking@0.2.0;
}
```

Compile this to `wasm32-wasip1` and convert to a component with
`wasm-tools component new`. Then reference it in your splice
configuration:

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

The generated adapter appears in the `Bundle::generated_adapters`
list (programmatic API) or as a file in the splits directory (CLI).
