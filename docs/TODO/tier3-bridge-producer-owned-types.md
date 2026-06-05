# Tier-3 wrap of producer-owned types interfaces

Tier-3 wrap of a target interface that `use`s types (including
resources) from a sibling types interface works when the types
interface is host-provided (e.g. `wasi:http/types`) and fails when the
types interface is producer-owned (e.g. `my:service/async-bucket-types`
exported by `shapes_handles`). This doc captures the diagnosis and the
options for closing the gap.

The earlier writeup
([resource-method-interception.md](resource-method-interception.md))
claimed the failure was unavoidable at the WIT spec level. That holds
for "wrapper owns the resource and exports it back," but the wasi case
shows tier-3 wrap of a resource-bearing interface CAN work today when
the types interface is a shared external import. The gap is
specifically about producer-owned types interfaces.

## Why wasi:http works

The wrapper world for tier-3 wrap of `wasi:http/handler`:

```wit
world target {
  import wasi:http/types@...;
  export wasi:http/handler@...;
  import wasi:http/handler@...;
}
```

`wasi:http/types` is imported by the wrapper, by `service-comp`, and by
the host's wasi:http implementation. wac threads a single
`wasi:http/types` instance through all three. wit_component encodes the
wrapper's export of `handler` such that its `use types.{...}`
references resolve to the wrapper's own `wasi:http/types` import,
because in this shape the encoder picks a shared "imported types
interface" reference rather than synthesizing fresh types on the
export side. Type identity survives the round trip; wac compose
accepts it.

## Why my:service/async-bucket doesn't

Structurally identical wrapper world:

```wit
world target {
  import my:service/async-bucket-types;
  export my:service/async-bucket;
  import my:service/async-bucket;
}
```

But wac compose fails:

```
type mismatch for import `my:service/async-bucket`
type mismatch in instance export `bucket`
resource types are not the same
(ResourceId { globally_unique_id: 26, contextually_unique_id: 95 }
 vs ResourceId { globally_unique_id: 26, contextually_unique_id: 9 })
```

The wrapper's encoded component-type carries two distinct `bucket`
types: one for `import.async-bucket-types.bucket` and a freshly-minted
one for `export.async-bucket.bucket`. wac wires *instances* between
producers and consumers; it does not bridge two type indices inside a
single component's own type signature. The duplication is baked in at
wit_component encode time.

## Why wac wiring alone isn't enough

Threading `shapes_handles.exports.async-bucket-types` to the wrapper's
import slot is necessary but not sufficient. Even with the import side
correctly wired, the wrapper's own export side still declares a
distinct `bucket` type. wac compose's typecheck operates on the
wrapper's component-type as encoded, not on the wac wiring graph.

The same problem applies to non-resource named types from sibling
factored interfaces (records, variants, enums, flags) when the
producer owns the types interface, but for non-resource types wac
compose typically accepts structural equality so the failure mode is
narrower in practice.

## Why wasi succeeds where the parallel case should fail

Working theory: wit_component's encode path treats an `import X / use
X.{...} / export Y` pattern as "shared imported type" only when X is
referenced as an import from the world (vs. being declared inline in
the same package). Both wasi and producer-owned cases nominally meet
that pattern, so the divergence is in some other detail of how the
sibling types interface is referenced. Concretely: confirming the
hypothesis means dumping both wrapper components' `component-types`
sections side by side.

## Options for closing the gap

### (a) Post-process the wrapper's component-types section

After wit_component encodes the wrapper, walk the encoded component's
type section and rewrite `export.<target>` so its references to the
sibling types interface's items point at the wrapper's import type
index instead of the freshly-minted export-side type indices. Brittle
because it depends on the component-types byte layout, but surgical:
no change to codegen, wit-bindgen, or wac.

### (b) Hand-encode the wrapper component shell

Skip wit_component for the wrapper-shell encoding step. Use
`wasm-encoder` to build the wrapper component's type section directly
with the correct type-identity bridging baked in. wit-bindgen still
generates the core wasm; we wrap it ourselves. This is the pattern in
the proxy-component research checkout at
`../../research/proxy-component/`. Higher up-front cost but more
robust than (a).

### (c) Diagnose then fix in wit_component

If the wasi-vs-producer-owned divergence is a wit_component encoding
detail rather than a deeper WIT spec issue, file an upstream fix or
PR against wit_component to make the encoder pick the shared-import
type reference uniformly. Lowest splicer surface area; depends on
upstream willingness.

### (d) Wait for the WIT spec extension

[wasm-tools#2506](https://github.com/bytecodealliance/wasm-tools/issues/2506)
captures the missing WIT syntax. If accepted, the spec extension
would let us declare "this export uses that imported type" directly
in the wrapper's WIT, sidestepping the encoding ambiguity. Long road,
and tier-3 wrap of producer-owned types remains blocked until then.

## Recommended next step

Before committing to (a) or (b), run the side-by-side experiment:

1. Build the wasi:http/handler wrapper component (tier-3 wrap that
   currently composes).
2. Build the my:service/async-bucket wrapper component (tier-3 wrap
   that currently fails to compose).
3. `wasm-tools component wit` and `wasm-tools dump` on both, focusing
   on the component-types section.
4. Identify the exact encoding divergence between the two.

The output of step 4 decides between (a) (if the divergence is a
fixable encoding detail), (c) (if upstream wit_component can fix it),
or (b) (if neither and we need full control of the type section).

## Scope reminder

This is about tier-3 wrap of interfaces that USE types from a
producer-owned sibling types interface. It is NOT about tier-3 wrap
of interfaces that DECLARE inline resources (which has a separate
inline-resource-rejection at compose time) and NOT about
resource-METHOD interception (the wrapper-as-resource-owner pattern
in [resource-method-interception.md](resource-method-interception.md);
that's a different layer of the same problem).

Tier-4 wrap is independent of all three: the wrapper is the type
owner, exports the types interface, and synthesizes resources via
the strategy. No type-identity bridging needed.

## References

- `src/adapter/typed/target_wit.rs` — wrapper world WIT emission,
  including the sibling types iface inclusion.
- `src/adapter/typed/assemble.rs` — wrapper component encoding via
  wit_component.
- `src/wac.rs` — wac composition.
- `tests/component-interposition/splicer-rules/builtin-hello-tier3.yaml`
  — has both the working wasi:http/handler case and the failing
  async-bucket case (currently rules out async-bucket; restore it to
  reproduce the failure).
- proxy-component research checkout at
  `../../research/proxy-component/` — blueprint for option (b).
- [resource-method-interception.md](resource-method-interception.md)
  — earlier writeup; superseded by this doc on the "no splicer-side
  workaround possible" claim.
- [tier3-tier4-builtins.md](tier3-tier4-builtins.md) — broader
  tier-3/4 substrate context.
