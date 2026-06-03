# Per-function interposition filter

Today a tier-1 adapter wraps **every** exported function of the
target interface with the same middleware. The middleware can filter
at runtime via the `name` param, but the hook round-trip still fires
on every call — including the ones the middleware immediately no-ops.
That's fine for single-function interfaces like `wasi:http/handler`
but gets expensive as interfaces grow.

## Proposal

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

## Implementation sketch

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

## When to build it

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
