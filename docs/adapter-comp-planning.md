# Adapter generator — planning and future work

Forward-looking notes for the adapter-component generator. The
currently-shipped tier-1 path is documented end-to-end in
[`adapter-components.md`](./adapter-components.md) (user-facing) and
[`adapter-internals.md`](./adapter-internals.md) (architecture). This
file focuses on what hasn't been built yet.

## Middleware tier roadmap

Tier 1 is shipped. Tiers 2 and 3 are planned. Middleware capability
strictly accumulates: a tier-N middleware can do everything an earlier
tier can, plus one new thing.

| Tier | New capability                       | Status      |
|------|--------------------------------------|-------------|
| 1    | see function names                   | **shipped** |
| 2    | see arg / result values (serialized) | planned     |
| 3    | modify arg / result values           | planned     |

### Tier 2: value-aware middleware (planned)

The middleware gets access to arguments and return values, serialized
as strings (e.g. WAVE-encoded). The adapter handles canonical-ABI
lifting/lowering; the middleware works entirely with strings and
never sees the typed values directly.

Proposed WIT interface:

```wit
interface value-aware-middleware {
    // return some(wave-encoded-result) to short-circuit and skip downstream
    before-call: async func(name: string, args: string) -> option<string>;
    // return some(wave-encoded-result) to replace the downstream result
    after-call: async func(name: string, result: string) -> option<string>;
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

**Suitable for**: memoizers, result caching, circuit breakers with
response replay, content-based routing, mutation-based fuzzers.

### Tier 3: read-write middleware (planned)

Tier 2 but with modification authority over both inbound args and
outbound results. Same serialized-string contract; the difference is
that splicer decodes the middleware's returned string back into the
canonical-ABI typed form before the call continues.

**Suitable for**: request enrichment (injecting headers / context),
response transformation, content filtering, A/B testing (routing
request variants to the same downstream), mutation-testing
frameworks.

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

| Middleware kind              | Generator approach                                           | Rationale                                                                        |
|------------------------------|--------------------------------------------------------------|----------------------------------------------------------------------------------|
| Tier 1 (type-erased)         | `wasm-encoder` + `wit-bindgen-core::abi` (shipped)           | Pure dispatch, no value construction; direct binary construction is simplest     |
| Tier 2 (value-aware)         | `wasm-encoder` + WAVE encode/decode glue                     | Dispatch + serialization; still no value construction from scratch               |
| Tier 3 (read-write)          | `wasm-encoder` + WAVE encode/decode glue + response injection | Same machinery as tier 2 plus a typed deserialization path                      |
| One-per-sig (fuzzer / mock)  | Rust codegen via `syn` / `quote` + `wit-bindgen` + `arbitrary` | `arbitrary` derive handles all type complexity; codegen stays small at cost of a `cargo build` step |

Useful references for when tier-2/3 work starts:

- [`wit-dylib`](https://github.com/bytecodealliance/wasm-tools/tree/main/crates/wit-dylib)
  — dynamic-linking bindings generator in `wasm-tools`. Has
  canonical-ABI lift/lower codegen patterns worth studying.
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

## Canonical-ABI gaps

Two known limitations that still surface as `anyhow::bail!` errors:

- **Flat params / results > 16 — pointer-form lowering.** The
  canonical ABI collapses to `(i32)` pointer form when a function's
  flat representation exceeds 16 values. `func.rs::extract_func_sig`
  currently bails at this boundary with a clear error (instead of
  silently declaring wrong core types). Implementing pointer-form
  needs: `params_are_ptr` / `results_are_ptr` flags on
  `AdapterFunc`, pointer-form type declarations in every dispatch
  emitter, and a memory-layout buffer reservation for the spilled
  args.

- **Anonymous compound types as top-level results.** When a Record
  / Variant / Enum appears as a func result but isn't in
  `iface.type_exports` (unusual in WIT-compiled interfaces, but
  legal at the component-model level), the adapter's export-instance
  construction can't re-export the compound — the binary fails
  validation with "instance not valid to be used as export." Fix:
  synthesize names + auto-export in `component.rs::emit_export_phase`.
  Low priority since real WIT always names its compounds.

## Silent-fallback audit

Several sites across `filter/`, `wac.rs`, and `build/dispatch.rs`
handle "unknown" enum discriminants or missing map entries with an
`unwrap_or(Other)` / `unwrap_or(None)` / `unwrap_or(ty)` fallback.
These don't affect correctness under today's test fixtures, but an
upstream change — `wasmparser` or `wirm` adding a new enum variant,
a new `ComponentExternalKind`, an `AliasSpaceKind` expansion — could
silently drop tracked items without any loud failure.

Audit pass to file when we next touch these files:

- Grep `src/adapter/filter/` and `src/wac.rs` for
  `unwrap_or(`/`unwrap_or_else(`/`=> Other`/`=> None` on enum
  discriminant or index-translation sites.
- For each, replace the fallback with an explicit `anyhow::bail!`
  that names the unexpected variant, so a future upstream addition
  fails loud at the filter/reencoder layer instead of producing a
  structurally-invalid adapter.

No correctness impact today; prioritize when a new `wasmparser` /
`wirm` major version lands.
