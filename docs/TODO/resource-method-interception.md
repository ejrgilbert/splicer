# Resource-method interception via forwarding wrappers

**Status: design verified end-to-end (June 2026), implementation pending.**
POC at `tests/resource-wrap-poc/` (`./run.sh`).

## Goal (done criteria)

Resource interception working across **all tiers (1-4)** for **both inline and
standalone (factored) resource declarations**. Current state, verified against
code:

| tier | inline decl | standalone decl |
| ---- | ----------- | --------------- |
| 1-2  | hard-bail at codegen (`require_no_inline_resources`, `abi/emit.rs:1090`) | passes the inline check; method `[method]` dispatch + delivery still unbuilt |
| 3-4  | no codegen bail; per-method codegen runs | no codegen bail; per-method codegen runs |

The tier-3/4 cells are not "done": the gap was always **runtime delivery** of
method calls to the wrapper (the old wac "resource types are not the same"
mismatch), which hit inline *and* standalone alike. So the real axis is
delivery, not declaration style. The fresh-`T'` mechanism is the single fix
that unifies all four tiers across both decl styles. Verifying exactly what
tier-4 delivers today (vs. only passing structural/matrix tests) is an explicit
task below.

## The problem

A splice wrapper can intercept interface-level functions that take or
return resources (factories, value-typed methods), but cannot intercept
methods **on** the resource itself. WIT resource ownership is tied to the
interface that DECLARES the resource, and the canonical-ABI runtime
delivers a method call on `Resource<R>` to whichever component implements
`GuestR`. A thin re-export wrapper does not own `GuestR`, so
`bucket.get(k)` dispatches straight to the producer, bypassing the
wrapper. Any tier-2/3/4 strategy over a resource-bearing interface
(recorder, fault-injector, replayer) therefore captures `open() -> bucket`
but nothing the consumer subsequently does to that bucket. And that is only
the *factored* case: for an inline-declared resource the wrapper pattern is
rejected outright at compose (even the interface-level interception, via
`require_no_inline_resources`), see below.

## Why the obvious fix is blocked

The natural fix is to make the wrapper re-export the *same* interface and
preserve the resource's type identity across it (import `bucket`,
re-export the *same* `bucket`). This is **unsupported in WIT**. wac mints a
fresh contextual resource id for the wrapper's `export.bucket`, distinct
from the imported bucket, and cannot bridge them:

```
type mismatch in instance export `bucket`
resource types are not the same (ResourceId .. vs ResourceId ..)
```

See [wasm-tools#2506](https://github.com/bytecodealliance/wasm-tools/issues/2506):
"there's not actually even syntax in WIT to describe this world." This is
the wall the previous wrapper-as-type-owner / re-export approaches hit, for
both inline and factored resource declarations.

## The solution (verified)

Stop trying to preserve resource identity across the wrapper. Instead:

1. The wrapper owns a **fresh** resource type `T'` in its own interface,
   distinct from the producer's `T`.
2. `T'` **holds** the real `T` handle inside and **forwards** each method
   call in Rust (this is where record/replay/perturb hooks live).
3. The consumer is **rewired cross-name** to `T'` at wac time. It was built
   against the producer's interface; wac binds that import to the wrapper's
   fresh interface instead.
4. A **bridge** interface exposes `wrap(raw) -> wrapped` / `unwrap(wrapped)
   -> raw` so edge interposers can box/unbox handles as they cross the
   subgraph boundary.

Because no resource identity is bridged across the wrapper, the WIT
limitation never applies. The fresh id wac would mint anyway *is* the
wrapper's own type, on purpose.

### Verified facts (POC: `tests/resource-wrap-poc/`)

Against wac 0.10.0 / wasm-tools 1.247.0 / wasmtime 34 / wit-bindgen 0.51 /
cargo-component 0.21. All run at runtime, not just typecheck:

- **Forwarding + round-trip identity.** Full `realprov -> wrapper -> edge`
  graph composes, validates, runs. A value written through a raw handle is
  visible through `T'` (forwarding works) and through the `unwrap`-recovered
  raw handle (`unwrap` returns the exact original handle, no copy). Output
  `via_t=Some("fromraw") via_raw=Some("fromraw")`. The producer declares its
  bucket **inline** -- the case the old approach flagged as impossible.
- **Width subtyping.** A consumer importing `bucket {ctor,get,set}` composes
  against a provider exporting the same id plus extra methods. So
  `wrap`/`unwrap` *may* live as extra statics on `T'` instead of a separate
  bridge, if preferred. Negative control (provider missing `set`) is
  correctly rejected. (`wac-checks/width-subtyping/`)
- **Cross-name rewire.** A consumer importing `test:orig/store` binds to a
  provider exporting structurally-identical `test:wrapped/store` via explicit
  `wac compose` wiring (auto plug-by-name does not match; explicit wiring
  does). This is the consumer rewire the real splice performs.
  (`wac-checks/cross-name/`)

### Component shapes

```wit
// producer (unchanged): exports the real interface, bucket inline or factored
package host:kv;
interface store { resource bucket { constructor(name: string); get: ...; set: ...; } }

// wrapper: owns T', forwards, exposes the bridge
package splice:wrap;
interface store { resource bucket { constructor(name: string); get: ...; set: ...; } } // T'
interface bridge {
  use host:kv/store.{bucket as raw-bucket};
  use store.{bucket as wrapped-bucket};
  wrap:   func(inner: raw-bucket) -> wrapped-bucket;
  unwrap: func(w: wrapped-bucket) -> raw-bucket;
}
world wrapper { import host:kv/store; export store; export bridge; }
```

The wrapper's `GuestBucket` impl: `struct WrappedBucket { inner: RawBucket }`,
methods forward to `inner` (hooks here), `wrap = WrappedHandle::new(...)`,
`unwrap = w.into_inner().inner`. See `tests/resource-wrap-poc/wrapper/src/lib.rs`.

### Boundary-edge shim (step 4, not yet built)

The POC's `edge` is a mint-then-wrap driver, not a real interposer. The real
shim sits on a boundary edge that carries `T` as a function param/return and
applies wrap/unwrap per occurrence. Take the producer's `store` interface with
a factory `open` and a sink `close` that pass `bucket` by value (added to the
running example to show `T` in both a return and a param position):

```wit
interface store {
  open:  func(name: string) -> bucket;   // resource flows OUT to the consumer
  close: func(b: bucket);                 // resource flows IN toward the owner
}
```

The shim imports the real `store` from the producer plus the wrapper's
`bridge`, and re-exports `store` to the (rewired) consumer. Direction rule:
make the representation match the side the handle lands on (raw on the owner
side, `T'` on the other). Generated bodies (pseudocode):

```rust
// shim exports store to the consumer; consumer now holds T' handles
fn open(name: String) -> WrappedBucket {     // returns T'
    let raw = real_store::open(&name);       // forward to producer -> own<bucket>
    bridge::wrap(raw)                         // lands on consumer (non-owner) -> WRAP
}
fn close(b: WrappedBucket) {                  // takes T'
    real_store::close(bridge::unwrap(b));     // lands on producer (owner) -> UNWRAP
}
```

So `open`'s return is wrapped and `close`'s param is unwrapped; a function with
`T` in both positions does both. Nested `T` (inside a record/list/option) is
the same walk applied at each leaf, folded into the existing lift/lower. The
`borrow<T>` param case (e.g. `use: func(b: borrow<bucket>)`) is harder: you
can't consume a borrow, so the shim borrows the held inner handle for the call
duration rather than calling `unwrap` (see open questions). Once a handle is
wrapped here, the consumer's later `bucket.get(...)` calls dispatch to the
wrapper's `GuestBucket` (where the record/replay hooks live); the shim only
handles the function-level crossings.

### When is a resource wrapper actually needed?

Depends on inline-vs-factored and whether we instrument the resource's
methods. The fresh-`T'` machinery is only required in three of four cells:

|                | functions only            | + methods                |
| -------------- | ------------------------- | ------------------------ |
| **factored**   | no wrapper (works today)  | forwarding `T'` + hooks  |
| **inline**     | passthrough `T'` (no hooks) | forwarding `T'` + hooks  |

- **Factored + functions-only** already works: interpose only on the
  operations interface and leave the resource type shared producer<->consumer
  via the `-types` interface. The handle passes through untouched (same
  identity, not re-minted), no wrapper. Keep this path when method
  interception isn't wanted.
- **Inline + functions-only** still needs a passthrough `T'` (forwarding with
  empty hooks). An inline resource can't be shared separately, so interposing
  on its interface re-types it and the producer's original handle can't be
  handed through (the #2506 wall, which also hits factored re-export). The
  wrapper is the unavoidable cost of touching an inline-resource interface, or
  factor the resource first.
- **Either + methods** needs the forwarding `T'` with hooks.

So the fresh-`T'` approach contributes two things: it makes **inline work at
all** (previously rejected at compose) and adds **method interception** for
both inline and factored.

Caveat not yet exercised: the POC validated the mechanism with an inline
producer via a mint-then-wrap path (the edge minted a raw handle and wrapped
it). The real splice flow instead has an interposed factory *return* the
resource, with the outgoing-edge shim boxing it at the boundary (step 4). Not
expected to surprise, but unrun.

## Recording semantics (so the wrapping is placed correctly)

Pinned replay direction: **S is real at replay; its outgoing invocations on
T return recorded results** (virtualize S's environment, not S itself).
Consequences:

- Record only the invocations S *makes* on resources that crossed *into* S.
  `T'` lives on the non-owner side; recording happens in the forwarding body.
- "Ownership" reduces to the in-vs-out direction of the boundary crossing:
  wrap a handle when it crosses *into* S, unwrap when it crosses back out, so
  `T'` is live exactly while the handle is inside S. Round-trip identity
  (verified) means unwrap returns the exact original, so no identity registry
  is needed; per-visit binding suffices.
- Lone exception: if the environment calls *back into* S on a resource S
  handed out and S depends on those callbacks, faithful replay must reproduce
  them as stimuli (a separate incoming-edge recording). Dependency-consumer
  subgraphs never hit this.

Full rationale in memory note `project_resource_record_wiring`.

## Implementation plan

1. **Match: resources to wrap.** Identify resource types used inside the
   target subgraph or in a target interface, and for each, which side owns it
   (equivalently, the direction it first crosses the boundary). Design detail:
   what exactly qualifies a resource, and the closure requirement over T
   (global redirect of all importers vs a bounded-subgraph cut that must be
   proven closed; see `project_subgraph_bounded_replay`).

2. **Match: classify edges.** For every edge, tag it internal / incoming /
   outgoing w.r.t. the subgraph, and whether it carries T (as receiver, param,
   return, or nested in a record/list/option). This drives codegen and wiring.

3. **Codegen: T' forwarding wrapper(s) + bridge.** Per resource, emit the
   fresh-named `T'` interface and world. The per-method bodies are **not new
   codegen**: reuse the typed per-method codegen in
   `src/adapter/typed/emit_method.rs`, which already emits per-method strategy
   dispatch for tier-3/4 ("Resource-level emissions dispatch through the
   strategy per method", `emit_method.rs:68`). It is strategy-parameterized, so
   observe/record (tier-2 semantics), tier-3 perturb, and tier-4 virtualize all
   fall out of the same path. (Methods use this typed codegen, not the abi
   tier-1/2 envelope, because the wrapper must *own* the type.) The gap was
   never the method bodies, it was delivery: calls weren't reaching the
   wrapper. So the genuinely new substrate is narrow: (a) replace the
   inline-resource bail `require_no_inline_resources`
   (`src/adapter/abi/emit.rs:1090`, the #2506 wall in code) with fresh-`T'`
   type emission so the wrapper owns a forwarding type
   (`src/adapter/typed/target_wit.rs` emits `export <wrapped-iface>` +
   `export bridge`, not the identity-preserving re-export), and (b) the
   wrap/unwrap bridge to carry handles across boundaries. `should-call`/gate
   keeps its pre-existing general limit (void-returning only); value-returning
   replay uses the tier-4 result-synthesis path, not the gate.

4. **Codegen: edge wrapper components.** A component interposed on each
   incoming/outgoing boundary edge that carries T. It calls
   `bridge.wrap`/`unwrap` at each T-occurrence in its signatures, folded into
   the existing lift/lower walk. Direction rule: make the representation match
   the side it lands on (raw on the owner side, T' on the other). One shared
   wrapper component per resource type across all of S's boundary edges, so a
   handle wrapped at one edge can be unwrapped at another.

5. **wac: redirect.** Rewire consumer imports of the producer interface to the
   wrapper's fresh `store` export (cross-name, verified); wire edge shims'
   bridge imports to the wrapper's bridge export; wire the wrapper's import to
   the real producer. Touch point: `src/wac.rs`.

## Checklist (to working e2e)

Verified primitives (POC): forwarding `T'` + round-trip identity, width
subtyping, cross-name rewire. Remaining:

**Match / analysis**
- [ ] Implement the `scope: resource` match predicate. `FuncScope::Resource`
  is a `todo!()` at `src/select.rs:308` (config parses fine via
  `src/parse/config.rs:165` / `compile_scopes`; `scope: interface` already
  works as `!name.starts_with('[')`). Resource scope = `name.starts_with('[')`
  i.e. `[method]` / `[constructor]` / `[static]` surfaces. This is how a rule
  opts into method interception (the "+ methods" column) vs interface-level,
  and it's what constrains matching for inline resources. The `todo!`'s own
  note points at `src/adapter/abi/emit.rs` to carry resource handles across the
  wrap, i.e. the wrapper/edge codegen below.
- [ ] Enumerate resource types in the target subgraph; mark each
  instrument-methods vs functions-only (the `scope` predicate selects this;
  see the matrix above).
- [ ] Determine owner side / first-crossing direction per resource type.
- [ ] Classify every edge: internal / incoming / outgoing x carries-T
  (incl. nested in record/list/option).
- [ ] Decide closure strategy: global redirect vs bounded cut (prove closed).

**Codegen: wrapper** (`target_wit.rs`, `emit_method.rs`)
- [ ] Replace the inline-resource bail `require_no_inline_resources`
  (`src/adapter/abi/emit.rs:1090`, tier-1/2 only) with fresh-`T'` type emission.
  The #2506 wall in code; our approach removes the need for it.
- [ ] Emit fresh-named `T'` interface (mirror original methods) + world
  (import real / export `T'` / export bridge).  [`target_wit.rs`]
- [ ] Reuse `emit_method.rs`'s existing per-method dispatch over the
  `[method]`/`[constructor]`/`[static]` surfaces. Already emitted for tier-3/4
  and strategy-parameterized, so it generalizes to tier-1/2/3. NOT new body
  codegen; the `GuestT'` forwarding impl comes from this path. Value-returning
  replay routes through tier-4 result synthesis (the gate can't synthesize
  results, pre-existing general limit).
- [ ] Emit bridge `wrap`/`unwrap` (or statics on `T'`).
- [ ] Constructor (sync hook), drop (optional event), statics, `borrow<T>`
  (call-scoped wrapper).

**Codegen: edge shims** (new substrate)
- [ ] Emit boundary-edge component(s): import bridge + boundary iface,
  re-export boundary iface.
- [ ] Weave per-occurrence wrap/unwrap into the lift/lower walk (params,
  returns, receiver, nested); direction = match the landing side.
- [ ] One shared wrapper component per resource type across all boundary edges.

**wac wiring** (`wac.rs`)
- [ ] Redirect consumer imports of the producer interface to `T'` (cross-name).
- [ ] Wire edge bridge imports to the wrapper's bridge export.
- [ ] Wire wrapper import to the real producer.

**Validation** (done criteria: tiers 1-4 x {inline, standalone})
- [ ] Verify what tier-4 actually delivers today: it has no inline codegen bail
  (that's tier-1/2 only), but confirm whether resource-method calls reach the
  wrapper at runtime for inline and standalone, vs. only passing structural /
  matrix tests. Adopt fresh-`T'` delivery wherever they don't.
- [ ] e2e: interposed factory returns a resource, outgoing-edge shim boxes it
  (the unrun step-4 flow).
- [ ] e2e: record then replay a resource-method trace through recorder/replayer
  (tier-2 + tier-4).
- [ ] Runtime smoke test in the suite (`GuestT'::method` fires on consumer call).
- [ ] Cover the full matrix: tier-1/2/3/4 x {inline, standalone}.

**Integration tests** (`tests/component-interposition/`)
- [ ] Add e2e interposition fixtures that exercise resource *methods* across
  tier-1/2/3/4 x {inline, standalone}, with `expected-output/` baselines. The
  harness already runs `builtin-{tier1..4}` + `recorder` cases; add
  resource-bearing targets alongside them.

**Fuzz coverage** (resources are barely covered today; this work unblocks it)
- [ ] `tests/fuzz_and_run.rs`: today only `own<T>`/`borrow<T>` as
  interface-level values, factored `my:shape/types`, nullary-constructor
  ("methods, static funcs, and constructors-with-params are out of scope",
  line 168). Extend to methods / statics / param-ctors and inline decls.
- [ ] `src/adapter/tier1/tests/fuzz.rs` (line 518) and
  `src/adapter/tier2/tests/fuzz.rs` (line 58): add `Resource` to the value-type
  generators (currently excluded; tier-2 already parks the inline-rejection
  allowlist entry at line 192).
- [ ] `src/adapter/typed/tests/fuzz.rs` (line 4): add resources/handles to the
  tier-3/4 generator scope (currently excluded).

## Open design questions

- `borrow<T>` cannot be stored in a persistent wrapper. Lent handles need a
  call-scoped wrapper (reuse the tier-4 borrow-lifetime threading); unwrap of
  a borrow = borrow the held `inner`.
- Constructor is sync while methods are async: the ctor-time hook must be
  sync-safe (buffer/flush). Same constraint tier-4 already hit.
- Drop: owning `inner` drops the real handle for free; add a destructor hook
  only if the drop event is wanted in the trace.
- Bridge-as-separate-interface vs wrap/unwrap as statics on T' (width
  subtyping makes both legal; the bridge keeps the consumer-facing T' clean).
- Edge topology: a separate edge component must call the wrapper's exported
  bridge (the wrap/unwrap logic cannot be copied into the edge, since only the
  type owner can box/unbox its resource). Fusing the edge into the wrapper
  keeps it internal Rust but constrains routing.

## References

- POC: `tests/resource-wrap-poc/` (`./run.sh`, README documents each check).
- Code: `src/wac.rs`, `src/adapter/typed/target_wit.rs`,
  `src/adapter/typed/emit_method.rs`.
- Blocker for the naive approach: wasm-tools#2506.
- Related: `tier2-generic-resource-handles.md`,
  `tier4-shared-resource-family.md`, `user-types-containing-resources.md`.
- Memory: `project_resource_record_wiring`, `project_subgraph_bounded_replay`,
  `project_tier3_producer_owned_types`.
