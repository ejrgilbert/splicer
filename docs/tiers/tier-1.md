# Tier 1: Name-Only Hooks

**Status:** currently supported.

The middleware receives the function name as a string and can run logic
before/after the downstream call, or conditionally block it. It never sees
the types or values of the function's parameters or return values.

For the cross-tier framework (one-tier-per-middleware rule, async
convention, hook-trap propagation, chain composition), see
[`adapter-components.md`](../adapter-components.md).

**WIT definition:** [`wit/tier1/world.wit`](../../wit/tier1/world.wit)

A middleware is tier-1 compatible when it exports **at least one** of the
interfaces defined in the tier-1 WIT package. The generated adapter only
wires up the hooks that are actually present, any non-empty subset is
valid.

## What "interface" means here (one middleware wraps N functions)

The unit of interposition is a **WIT interface**, not a single function.
An interface is an instance type that can export any number of functions.
Splicer's adapter wraps **every** function in the target interface with
the same middleware — the middleware doesn't get to pick and choose, but
it can discriminate at runtime via the `name` parameter the hooks
receive.

Concrete shapes:

| Target interface       | Functions in it            | Adapter generates |
|------------------------|----------------------------|-------------------|
| `wasi:http/handler`    | `handle`                   | 1 wrapper         |
| `my:service/adder`     | `add`                      | 1 wrapper         |
| `my:service/math`      | `add`, `sub`, `mul`, `div` | 4 wrappers        |

All the wrappers share the same hook imports (`splicer:tier1/before`
etc.). When `add` is called, the adapter calls `before-call("add")`; when
`div` is called, the adapter calls `before-call("div")`. The middleware
sees one stream of hook calls with the function name as the
discriminator — one middleware, N functions.

### If your middleware only cares about some of the functions

Because the adapter invokes every hook your middleware exports on every
wrapped call, **you pay the before/after/block round-trip uniformly**,
even for the calls your middleware will immediately no-op. For a
4-function interface where your logging middleware only cares about one,
`before-call` still fires 4 × per mixed workload and you filter by name
inside the middleware. Typical per-hook cost is an async subtask +
name-string lower/lift; small in isolation, but it scales linearly with
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

1. Calls `before-call(fn_name)` if the middleware exports `splicer:tier1/before`
2. Calls `should-block-call(fn_name)` if the middleware exports
   `splicer:tier1/blocking`; skips the downstream invocation when it
   returns `true` (void functions only)
3. Forwards the call to the handler with all arguments and return values
   passed through unchanged
4. Calls `after-call(fn_name)` if the middleware exports `splicer:tier1/after`

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

The generated adapter appears in the `SpliceOutput::generated_adapters`
list (programmatic API) or as a file in the splits directory (CLI).
