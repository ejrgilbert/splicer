# Wrap composition can't deliver calls to resource methods

L3 ships codegen that handles resource methods at the wrapper level
(`matrix_resource_pass_through` and friends pin per-method dispatch +
the `WrapperBucket(MockedResource)` newtype's `GuestBucket` impl).
But the splice composition substrate — the layer that takes the
codegen'd wrapper component and wires it between a producer and a
consumer via `wac` — can't actually deliver resource-method calls
through the wrapper, **regardless of whether the resource is declared
inline or factored into a sibling `-types` interface**. The two
failure modes:

- **Inline resource declaration.** Splicer rejects the splice at
  compose time with: *"interface X declares resource R inline.
  Splicer's wrapper-component pattern can't preserve resource type
  identity for inline resources."* (See
  `tests/component-interposition` recorder splice on
  `my:service/async-bucket`.)
- **Factored-types resource declaration.** Splicer composes, but the
  resource's `GuestBucket` impl lives on the producer side (the
  interface that DECLARES the resource), not on the wrapper. When the
  consumer holds a `Resource<bucket>` returned through the wrapper's
  `open(...)` and calls `bucket.get(k)` on it, dispatch routes to the
  producer, bypassing the wrapper entirely.

Net effect: a tier-3/4 strategy spliced over a resource-bearing
interface can intercept interface-level functions that **return** or
**take** resources (factory functions, value-typed methods on the
interface), but cannot intercept methods **on** the resource itself.

## The constraint

WIT resource ownership is tied to the interface that DECLARES the
resource. The canonical-ABI runtime delivers a method call on
`Resource<R>` to whichever component implements the trait `GuestR`
for that R. Three structural facts:

- **Inline resources can't be re-exported.** Re-exporting an interface
  that declares a resource inline creates a new resource type
  identity at the wrapper boundary; the wac compose stage rejects
  it because handles minted by the inner producer can't cross into
  the wrapper-typed table.
- **Factored resources can be re-exported.** The wrapper imports
  `<iface>-types` (to reference the resource type) and re-exports
  `<iface>` (the operations). Type identity flows through the shared
  `-types` interface. But the wrapper IMPORTS `-types` — it doesn't
  EXPORT it — so it doesn't own `GuestR`.
- **Proxy-component solves this by becoming the type owner.** The
  proxy-component generator emits a wrapper that EXPORTS the
  `-types` interface too, with its own `GuestR` impl. That makes the
  wrapper the canonical owner and method calls dispatch to it. But
  that breaks the wrap-as-thin-adapter pattern splicer uses
  everywhere else, because the wrapper now competes with the
  original producer for ownership.

## Where splicer hits this

Three layers conspire to produce the gap:

- `src/wac.rs` — wac composition pattern. Wraps a target interface
  by importing it on the wrapper and exporting it back. Doesn't
  handle the types-interface re-export pattern needed for
  resource-method interception.
- `src/adapter/typed/target_wit.rs:42-48` — constructs the wrapper
  world WIT. Only emits `export <target>` (+ `import <target>` for
  tier-3). Doesn't emit `export <types-iface>` so the wrapper would
  own GuestR for any resources.
- `src/adapter/typed/emit_method.rs` — codegen for `GuestBucket`
  impls. Works correctly for factored types — but the impls are
  emitted on the wrapper newtype, which lives in the wrapper's
  bindings. If the wrapper world doesn't export `<types-iface>`,
  those impls are unreachable from the runtime dispatch path.

The codegen-output-string matrix tests (which DO use inline
resources for the test fixtures) pass because they verify "the
emitted Rust is structurally correct," not "the spliced composition
delivers method calls to the wrapper." The composition layer is
unverified at runtime.

## Where it shows up in practice

A replayer e2e demo on `my:service/async-bucket` (where bucket is
declared inline) is the immediate manifestation: the splice rejects.
Refactoring to factored types makes the splice succeed, but only
interface-level functions (e.g. `open(seed) -> bucket`) are
intercepted; subsequent `bucket.get/put` calls bypass the wrapper.

The same gap applies to any tier-2 builtin (recorder, fault-injector)
that wants to capture or modify resource-method traffic. Recorder
currently records nothing about method calls on resources it
returned through an interface-level factory; the trace ends at
`open(seed) -> ResourceHandle(N)` and the method calls happen on the
producer side.

## Design options

### (a) Wrapper exports the `-types` interface

Make the wrapper a competing type owner. The wrapper world declares
`export <types-iface>` alongside `export <target>`. The wrapper's
codegen emits a `GuestR` impl that dispatches method calls through
the strategy (tier-4) or forwards to the import side (tier-3).

Pros: method calls reach the wrapper. Strategies can intercept any
resource method.

Cons:
- The producer ALSO exports `<types-iface>`. Two exporters of the
  same interface in the composition; wac has to disambiguate.
  Possibly solvable via shadowing (wrapper's export takes
  precedence) but needs design work.
- Tier-3 wrapper has to bridge: the wrapper imports `<types-iface>`
  (the producer's resource type) AND exports it. The same identity
  loop the inline case has, just at a different layer.
- For tier-4 (no inner producer to bridge), the wrapper has to
  fully synthesize the resource. That's actually simpler — strategy
  emits MockedResource via the existing `WrapperBucket` newtype.

### (b) Splice on the `-types` interface itself

Let users splice on `async-bucket-types` directly to wrap the
resource methods. The wrapper is a thin adapter that imports +
re-exports the types interface, intercepting method dispatch.
Splicing on `async-bucket` intercepts top-level fns; splicing on
`-types` intercepts methods. Two independent wrappers compose.

Pros: orthogonal axis. Users opt into method interception
explicitly.

Cons:
- Splicer's wrap-an-imports-interface pattern doesn't currently
  support `-types`-style interfaces (where the export shape IS the
  resource declaration). Has to grow new substrate.
- Type-identity loop: wrapper imports `bucket` from producer's
  `<types>`, exports `bucket` via its own `<types>`. wac
  composition has to wire the consumer's "bucket" to the
  wrapper's export, and the wrapper's import to the producer's
  export, while preserving handle ABI compatibility.

### (c) Document the limitation, ship resource-method codegen as-is

L3's codegen is structurally sound. The substrate gap is real but
narrowly scoped: it only matters when users actually want to
intercept resource methods. Many real strategies (chaos-err over
factory returns, retry over interface-level fns, replay of factory
outputs only) work fine with the current substrate. Document the
gap, ship L3, address (a) or (b) when a real consumer needs method
interception.

## Implementation plan

If (c): no code change. Document in the user-facing docs (likely
under `docs/tiers/` or the strategy author's guide) that resource
methods aren't interceptable.

If (a) or (b): substantial design + impl work spanning `wac.rs`,
`target_wit.rs`, `emit_method.rs`, and the wac composition tests.
Multi-week scope. Should be its own PR.

## What this means for existing matrix tests

The matrix tests don't catch this gap because they verify "codegen
emits structurally-valid Rust," not "the spliced wrapper actually
receives method calls at runtime." Adding a runtime smoke test for
"wrapper exports an interface using a factored resource → consumer
calls bucket.get → wrapper's GuestBucket::get fires" would catch
(a) / (b) regressions and is independent of any specific tier-3/4
strategy.

That smoke test would currently FAIL: it's the same shape as the
deferred runtime e2e fixture we've been flagging.

## Update (verified June 2026): the gap is at the WIT spec level

Empirical confirmation: a tier-3 splice over a factored-types
resource interface (`my:service/async-bucket` with `bucket`
factored into `async-bucket-types`) fails at wac compose with:

```
type mismatch for import `my:service/async-bucket`
type mismatch in instance export `bucket`
resource types are not the same
(ResourceId { globally_unique_id: 26, contextually_unique_id: 95 }
 vs ResourceId { globally_unique_id: 26, contextually_unique_id: 9 })
```

The wrapper's emitted WIT is correct (`use async-bucket-types.{bucket};`
flows through) and `wasm-tools component wit` on the encoded wrapper
shows the right import + export structure. The mismatch is at the
wac wiring layer: wac creates a fresh contextual resource id for the
wrapper's export.bucket, distinct from the imported bucket's id, and
can't bridge them.

[bytecodealliance/wasm-tools#2506](https://github.com/bytecodealliance/wasm-tools/issues/2506)
captures this. The maintainer's finding (closed with no fix):

> This is actually showcasing a component model feature that's
> entirely unsupported in WIT. […] there's not actually even syntax in
> WIT to describe this world at this time. […] to do this we'd need
> some sort of first-class syntax in a world.

So the substrate gap surfaces through wit_component's encoding, even
though the underlying issue maps to an unfilled WIT spec corner.

### Subsequent finding (updated June 2026)

The "tier-3 wrap of producer-owned types iface won't compose"
sub-claim is no longer accurate. The wasm-tools#2506 quote above
describes a real WIT-spec gap, but splicer was hitting it because its
WAC emission only explicitly bound the chained target iface and left
the sibling `-types` iface to `...` defaults. wac's default routing
gave the types iface a contextually-distinct resource id from the one
threaded through the target binding, and the wrapper's async-shim saw
inconsistent witnesses for the same bucket. Explicitly binding the
sibling types iface to the same source fixes it (see `src/wac.rs`,
the simple-middleware branch of `add_middleware` mirrors the tier-1
adapter's `factored_types_to_wire` call).

### What that means concretely

- **Tier-3 wrap of producer-owned resource-bearing interfaces:
  composes today** (verified via `--builtin-hello-tier3` on
  `my:service/async-bucket`). But the broader dispatch problem
  remains: methods invoked on a resource (e.g. `bucket.get`) still
  go to the producer that declared `GuestBucket`, bypassing the
  wrapper. So tier-3 can intercept iface-level functions that
  take/return resources, not methods on the resources themselves.
- **Tier-3 wrap of host-provided resource-bearing interfaces (wasi):
  works today.**
- **Tier-4 wrap is unaffected.** The wrapper IS the resource type
  owner (exports the types iface, synthesizes via the strategy); no
  import↔export type identity bridging needed.
- **Resource-method dispatch through the wrapper is still blocked**
  unless splicer takes the proxy-component route (wrapper exports
  the types iface, owns `GuestR` for every resource it virtualizes).
  That's a separate substrate change from the wac wiring fix above.

## References

- `src/wac.rs` — wac composition.
- `src/adapter/typed/target_wit.rs:42-48` — wrapper world WIT.
- `src/adapter/typed/emit_method.rs` — `GuestBucket` impl codegen.
- `tests/component-interposition/splicer-rules/builtin-recorder.yaml`
  — has rule for `my:service/async-bucket` that splicer rejected
  (inline-resource case).
- proxy-component (research checkout at
  `../../research/proxy-component/`) — the "wrapper owns the types
  interface" pattern.
- `docs/TODO/tier3-tier4-builtins.md` — broader L3/L4 context.
- L3 plan file (silly-squishing-prism.md) — the matrix tests that
  verify codegen but not composition.
