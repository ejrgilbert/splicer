# Generating a Proxy Component #

Right now, if someone wants to splice middleware on some function signature, the middleware has to import and export
that exact function signature. This means there needs to be one middleware provided per unique function signature it
runs on! This becomes unmanageable for any real-world application where middleware must be placed on every function
call (e.g. a pluggable OpenTelemetry middleware).

To help alleviate this developer burden, the next step in this project is to generate "proxy components" from middleware
that needs to be adapted to fit on some function signature. The following constraints must hold true for a middleware
to be eligible for generating a proxy component wrapper:
1. The component can only import _from the host_
2. TODO: Fill in constraints as more are discovered

Some resources that could be helpful here:
- https://github.com/chenyan2002/proxy-component/tree/main/src
- https://github.com/bytecodealliance/wasm-tools/tree/main/crates/wit-dylib
    - [Example that generates the lift](https://github.com/bytecodealliance/wasm-tools/blob/main/crates/wit-dylib/src/bindgen.rs#L768)

# Use cases
I see the following requirements for middleware capabilities where a middleware can do none or all of these things.

TODO: It's possible there are more cases, if so make sure to note them here!

Timing
1. Runs _before_
2. Runs _after_
3. Runs _before_ AND _after_

Block
1. Can _conditionally_ block the downstream call from being invoked

Data flow (requires type-aware value access -- see Middleware Tiers below)
1. **Inspect** the data being _passed to_ the downstream function
2. **Modify** the data being _passed to_ the downstream function (must run _before_ the invocation)
3. **Inspect** the data being _returned from_ the downstream function (must run _after_ the invocation)
4. **Modify** the data being _returned from_ the downstream function (must run _after_ the invocation)

# Approaches
I have thought of the following approaches, but am open to more ideas (especially if they are cleaner to implement):
1. Rust macros (that the middleware writer invokes)
2. Rust meta-programming (similar to what is done here: `/Users/evgilber/git/research/proxy-component`)
3. Wasm component creation (like from the bottom up using `wirm` library)

The best approach depends on the middleware tier (see Middleware Tiers below). For Tier 1 and Tier 2
proxy generation, use `wirm` as this stress-tests the codegen side of the crate and the complexity is
manageable since no value construction from scratch is required. For one-per-sig middleware,
use the [`proxy-component`] approach (`syn`/`quote` Rust codegen)! Attempting to do type-aware value
generation in raw Wasm bytecode via `wirm` would be orders of magnitude more complex with no benefit.

# Middleware Tiers

Not all middleware needs the same level of access to function arguments and return values. Two tiers are
proposed, each with its own middleware WIT interface. The generated proxy component knows which tier to
use based on which interface the middleware exports.

## Tier 1: Type-Erased Middleware

The middleware only needs timing and/or blocking behavior. It never touches argument or return values.
The proxy handles all type plumbing; the middleware just receives the function name.

WIT interface:
```wit
interface type-erased-middleware {
    before-call: func(name: string);
    should-block-call: func(name: string) -> bool;  // true = block downstream
    after-call: func(name: string);
}
```

Generated proxy shape:
```
export handle(req: request) -> response:
    middleware.before-call("handle")
    proceed = middleware.should-block-call("handle")
    if proceed:
        result = downstream.handle(req)
        middleware.after-call("handle")
        return result
    else:
        // block case constraints apply (see above)
```

The block case has one wrinkle: if before-call returns false, you need to return something from the exported function
without calling downstream. This is fine for:
- `void functions`: just return
- `result<T, E>` return types: return an Err (very common in WASI, e.g. wasi:http/handler)

But for functions returning plain values (not wrapped in result), there's no sensible "blocked" value to synthesize.
Worth documenting as a constraint on which interfaces support the block use case.

**Suitable for**: OpenTelemetry tracing, logging, rate limiting, auth (allow/deny only).

## Tier 2: Value-Aware Middleware

The middleware needs access to serialized argument and return values (e.g. for caching or inspection).
Arguments and results are passed as WAVE-encoded strings. The proxy handles canonical ABI
lifting/lowering; the middleware works entirely with strings.

WIT interface:
```wit
interface value-aware-middleware {
    // return some(wave-encoded-result) to short-circuit and skip downstream entirely
    before-call: func(name: string, args: string) -> option<string>
    // return some(wave-encoded-result) to replace the downstream result
    after-call: func(name: string, result: string) -> option<string>
}
```

Generated proxy shape:
```
export handle(req: request) -> response:
    wave_args = wave_encode(req)
    cached = middleware.before-call("handle", wave_args)
    if cached is some:
        return wave_decode(cached)
    result = downstream.handle(req)
    wave_result = wave_encode(result)
    override = middleware.after-call("handle", wave_result)
    return wave_decode(override) if override is some else result
```

**Suitable for**: memoizers, circuit breakers, result caching, mutation-based fuzzers.

## The "One-Per-Signature" Case

Some middleware genuinely cannot be expressed generically over serialized values because they must
**fabricate structurally valid new values from scratch**. This requires knowing the full type structure
at code-generation time, not just at runtime.

Known one-per-sig cases:
- **Type-generating fuzzer**: must construct valid values of every parameter type from raw random
  bytes. Mutation-based fuzzers (start from a real value, perturb the WAVE string) fit in Tier 2.
- **Mock/stub generator**: must return a valid fake value of the return type. (Replay from a
  recorded trace is Tier 2 since the WAVE bytes already exist; mocks that synthesize responses
  from scratch are one-per-sig.)
- **Property-based test harness**: must generate and shrink typed counterexamples; shrinking
  requires constructing smaller valid values, not just mutating existing ones.
- **Argument defaulting/enrichment**: filling in missing or zero fields requires knowing which
  fields are optional vs. required and what sensible defaults look like per type.

### Why Rust codegen ([`proxy-component`] approach) is the right tool here

The tempting alternative is to generate the Wasm component directly using `wirm`. However, that
would require implementing, in raw Wasm bytecode:
- Canonical ABI lowering/lifting per WIT type
- Recursive valid-value construction per WIT type (records, variants, lists, options, resources...)
- Random value generation over that construction

This is an enormous amount of work and very hard to get right.

The [`proxy-component`] project demonstrates a much leaner path: generate a small Rust file using `syn`/`quote`,
then compile it with `cargo`. This works because `wit-bindgen` already derives `Arbitrary` on every generated type,
so the entire type-correct random value construction reduces to:

```rust
let mut u = Unstructured::new(&random_bytes);
let value: SomeWitType = u.arbitrary().unwrap();
```

The actual codegen in [`proxy-component`] (`generate_fuzz_func`) is only ~120 lines of `quote!` macros.
The hard type-specific work is fully delegated to `wit-bindgen` + the `arbitrary` crate, neither
of which needs to be re-implemented.

### The Implementation Split

| Middleware tier      | Generation approach                                          | Rationale                                                                    |
|----------------------|--------------------------------------------------------------|------------------------------------------------------------------------------|
| Tier 1 (type-erased) | `wirm`                                                       | Pure dispatch, no value construction; good wirm codegen stress-test          |
| Tier 2 (value-aware) | `wirm`                                                       | Dispatch + WAVE encode/decode; still no value construction from scratch      |
| One-per-sig          | Rust codegen via `syn`/`quote` + `wit-bindgen` + `arbitrary` | `arbitrary` derive handles all type complexity for free; codegen stays small |

For natively-provided one-per-sig middleware (fuzzer, mock, property harness), splicer generates
the complete component. There is no separate "strategy" component. The algorithm lives in splicer's
Rust code generator, and `wirm` is not involved. The cost is an external `cargo build` step, but
since these are code-generation artifacts (not runtime operations), that is acceptable.

[`proxy-component`]: https://github.com/chenyan2002/proxy-component/tree/main