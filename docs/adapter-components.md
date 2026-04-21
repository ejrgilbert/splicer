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
return values. Splicer defines three tiers of middleware capability, each with
its own WIT interface. The generated adapter component knows which tier to use
based on which interfaces the middleware exports.

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

### Tier 2: Read-Only Reflection (planned)

The middleware can see the function name, the types of the parameters and return
values, and the actual data being passed, but cannot modify it. Arguments and
results are passed as serialized strings (e.g. WAVE-encoded). The adapter
handles all canonical-ABI serialization; the middleware works entirely with
strings.

**WIT definition:** `wit/tier2/world.wit` (not yet published)

**Good for:** request/response logging with payload inspection, metrics
extraction from request fields, cache key computation, content-based routing
decisions.

### Tier 3: Read-Write Interception (planned)

The middleware can see AND modify both the arguments going to the downstream and
the results coming back. Modifications are also expressed as serialized strings
that the adapter deserializes back into the correct types.

**WIT definition:** `wit/tier3/world.wit` (not yet published)

**Good for:** request enrichment (adding headers, injecting context), response
transformation, result caching with replay, content filtering, A/B testing
(routing different request variants to the same downstream).

### Summary

| Tier | See function names | See types & data | Modify data | Status        |
|------|--------------------|------------------|-------------|---------------|
| 1    | yes                | no               | no          | **supported** |
| 2    | yes                | yes              | no          | planned       |
| 3    | yes                | yes              | yes         | planned       |

Each tier strictly adds one capability over the previous. Middleware written for
a lower tier works unchanged when higher tiers become available — the tier is
determined by which WIT interfaces the middleware exports, and the adapter
generator picks the right strategy automatically.

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

2. **Does it export any `splicer:tier1/*` interfaces?** If yes, splicer
   classifies it as tier-1 compatible and generates an adapter component
   automatically. The adapter file is written to the splits directory alongside
   the split sub-components.

3. **Neither?** Splicer emits a warning: the middleware doesn't match the target
   interface and isn't adapter-compatible. It can still be injected (the user may
   know something splicer doesn't), but type safety is unconfirmed.

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
