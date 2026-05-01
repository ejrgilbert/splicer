# Adapter Components

When you splice middleware into a composition, the middleware component
needs to export the same interface it's being inserted on. A logging
middleware that wraps `wasi:http/handler` would normally need to import
and export the full `wasi:http/handler` interface — complete with all
its resource types, error variants, and async function signatures.

That's a lot of boilerplate, and it means every middleware is locked to
one specific interface. A logging component built for `wasi:http/handler`
can't be reused on `my:service/adder` without being rewritten.

**Adapter components** solve this. Instead of requiring middleware to
match the target interface signature, splicer generates a thin wrapper
component, the **adapter**, that bridges between a generic middleware
WIT interface and the specific target interface. The middleware author
writes against a tier-specific WIT contract — fully type-erased at
tier 1 (the middleware sees only call identity), structural / typed but
target-agnostic at tiers 2 and up (the adapter lifts canonical-ABI
values into a uniform field representation). Splicer handles all the
type plumbing at composition time either way.

This document covers the cross-tier framework: tier taxonomy, the rules
that apply to every tier, eligibility detection, and chain composition.
For each tier's deep dive, see the per-tier docs:

- [Tier 1: Name-Only Hooks](./tiers/tier-1.md) — currently supported
- [Tier 2: Observation](./tiers/tier-2.md) — planned
- [Tier 3: Transform](./tiers/tier-3.md) — planned
- [Tier 4: Virtualize](./tiers/tier-4.md) — planned

For a low-level architecture walkthrough of the generator itself, see
[`adapter-internals.md`](./adapter-internals.md). For broader planning
notes on tier work, see
[`docs/TODO/adapter-comp-planning.md`](./TODO/adapter-comp-planning.md).

## Middleware Tiers

Not all middleware needs the same level of access to function arguments
and return values. Splicer defines four tiers of middleware capability,
each with its own WIT interface. The generated adapter component knows
which tier to use based on which interfaces the middleware exports.

The four tiers split along two cuts: whether the middleware can see the
call's typed payload, and what it does with that visibility. Tiers 1
and 2 leave the call flowing through unchanged (observation only).
Tier 3 lets the middleware modify what flows through; tier 4 lets it
replace the downstream entirely.

| Tier | See call name | See typed data | Modify data | Bypass downstream | Status        |
|------|---------------|----------------|-------------|-------------------|---------------|
| [1](./tiers/tier-1.md) | yes | no  | no  | partial (block) | **supported** |
| [2](./tiers/tier-2.md) | yes | yes | no  | no              | planned       |
| [3](./tiers/tier-3.md) | yes | yes | yes | no              | planned       |
| [4](./tiers/tier-4.md) | yes | yes | yes | yes             | planned       |

The tiers split along two emit-path families in the adapter generator.
Tiers 1 and 2 share a **pass-through observation** path: arguments flow
to the downstream unchanged, hooks fire on the side. Tiers 3 and 4
share a **value synthesis** path: the wrapper materializes
canonical-ABI values from structural attributes — tier 3 uses the
synthesized values to call the downstream, tier 4 uses them as the
return value directly and elides the downstream call entirely.

Each tier strictly adds one capability over the previous. Middleware
written for a lower tier works unchanged when higher tiers become
available — the tier is determined by which WIT interfaces the
middleware exports, and the adapter generator picks the right strategy
automatically.

## Cross-tier rules

These apply to every tier; the per-tier docs only cover what's
tier-specific.

### Middleware imports

A middleware is just a regular component — it can declare any imports
it needs (`wasi:filesystem` for backing storage, `wasi:io` for streams,
custom interfaces for fixtures, etc.). The adapter only wires up the
middleware's tier-N export to the target interface; the middleware's
own imports are left untouched and get satisfied by the surrounding
composition (or by the host) like any other component would. This
matters most for tier 4 (a virt almost always needs a backend), but
the rule is the same everywhere.

### Async-only hooks

All tier WIT worlds export their hook functions as **async**. The
adapter emits async dispatch unconditionally, so middleware authors
write `async fn` regardless of whether the wrapped target function is
sync or async. This keeps the adapter's async machinery uniform and
lets the middleware freely await imports of its own without needing a
separate sync code path.

### Adapter behavior when a middleware hook traps

If a middleware's hook (any tier's `on-call`, `on-return`,
`should-block`, etc.) itself traps — e.g. the middleware panics,
dereferences out-of-bounds memory, or otherwise hits an unrecoverable
error — the trap propagates as a wasm trap through the adapter and
on up to the host. The adapter does nothing special; the runtime's
backtrace points at the adapter's dispatch wrapper at the hook-call
site, so an operator can tell from the backtrace alone that the trap
originated in the middleware hook (not in the wrapped target
function).

A middleware whose code is broken fails loudly so the operator
notices. A configurable `on-middleware-trap: propagate | log |
swallow` policy is plausible future work if a concrete use case
justifies it.

### One tier per middleware

A given middleware component must implement **exactly one tier**.
Within that tier, any non-empty subset of the tier's interfaces is
fine (e.g. a tier-1 middleware can export any combination of `before` /
`after` / `blocking`), but exporting interfaces from multiple tier
packages is rejected at splice time.

This is by design, not a missing feature. Higher tiers strictly subsume
the capabilities of lower ones — tier 2's `on-call` already carries the
function name, so a tier-2-aware component never needs tier-1 hooks
too. If you want to combine behaviors (say, observation plus
modification), ship them as **separate components** and chain them via
`inject: [...]` in the splice config. That makes the layering visible
at the configuration level rather than hidden inside one component's
exports.

### Composing middleware in a chain

A splice rule's `inject: [m1, m2, m3]` produces a chain where `m1` is
the outermost wrapper (closest to the caller) and `m3` is innermost
(closest to the downstream). Tiers 1-3 compose freely in any order —
the resulting behavior is always well-defined, though not always
commutative:

- **Tier 1 (name only)** never sees or touches values, so its presence
  is invisible to other middleware. Slots in anywhere; commutes with
  everything.
- **Tier 2 (observe)** sees values but doesn't modify them, so its
  presence doesn't change what flows through to neighbors. Commutes
  with everything; what *it* observes depends on what its outer/inner
  neighbors decided to do.
- **Tier 3 (transform)** modifies values, so order matters: a tier-3
  closer to the caller sees args before any inner transformations are
  applied, and sees results after they've been applied. Two tier-3s
  produce different results in different orders. This is the same
  decorator-chain semantics as Express, Tower, Rack, etc. —
  well-defined for any order, just not commutative.

**Tier 4 is a chain terminator.** A tier-4 middleware replaces the
downstream entirely, so anything *past* it in the chain is unreachable
— no calls flow through. Tier 4 must therefore be the **innermost**
entry in `inject`. Splicer warns if it sees middleware listed after a
tier-4 entry (the trailing entries can never fire).

Concrete walk-through with `inject: [t1, t2, t3]` and a single
`handle(req) → resp` call:

```
caller → t1.before("handle")                         // tier 1: name only
       → t2.on-call("handle", lifted-args)           // tier 2: observe
       → t3 lifts args, mutates → args', lowers      // tier 3: transform
       → downstream(args') → resp
       → t3 lifts resp, mutates → resp', lowers      // tier 3: transform back
       → t2.on-return("handle", lifted-resp')        // tier 2: observe post-transform
       → t1.after("handle")                          // tier 1: name only
       → caller gets resp'
```

Reorder the same three to `[t3, t2, t1]` and `t2` will observe the
post-transform args on the way in (because `t3` is now outside it) and
the pre-back-transform result on the way out — different snapshots,
same overall correctness.

## How Splicer Detects Adapter Eligibility

When processing a splice rule, splicer checks each middleware component:

1. **Does it export the target interface directly?** If yes, no adapter
   is needed — the middleware is wired in as-is. A type fingerprint
   check ensures the middleware's export is structurally compatible
   with the interface it's being placed on.

2. **Does it export interfaces from exactly one `splicer:tierN/*`
   package?** If yes, splicer classifies the middleware as tier-N
   compatible and generates an adapter component automatically. The
   adapter file is written to the splits directory alongside the split
   sub-components. Within that tier, any non-empty subset of the tier's
   interfaces is valid.

3. **Does it export interfaces from multiple tier packages?** Splicer
   rejects with an error:
   ```
   middleware `my-middleware.wasm` exports interfaces from multiple tiers
   (tier 1: splicer:tier1/before; tier 2: splicer:tier2/observe).

   A middleware must implement exactly one tier. To combine behaviors,
   ship them as separate components and chain them in `inject: [...]`.
   ```
   See ["One tier per middleware"](#one-tier-per-middleware) for the
   rationale.

4. **None of the above?** Splicer emits a warning: the middleware
   doesn't match the target interface and isn't adapter-compatible. It
   can still be injected (the user may know something splicer doesn't),
   but type safety is unconfirmed.

## Adapter Component Internals (Brief)

For those curious about what's inside the generated `.wasm`: the
adapter is a self-contained WebAssembly component that contains two
nested core modules (a memory provider and a dispatch module) plus the
canonical-ABI glue to lift and lower between the component model and
core Wasm. The dispatch module implements the before/call/after/block
sequencing in straight-line Wasm, using the component model's async
primitives (`waitable-set`, `subtask`, `task.return`) for async
function support.

The adapter handles sync functions, async functions, functions with
string parameters, functions with resource types, and functions with
complex result types (like `result<response, error-code>`) — all
transparently. The middleware component never needs to know about any
of this.

For a low-level architecture walkthrough of the generator itself —
module layout, type-flow from cviz through `wit-parser` to emitted
wasm, how `wit-bindgen-core::abi::lift_from_memory` drives the
`task.return` loads, heterogeneous-variant widening, and what splicer
still owns vs. inherits from upstream — see
[`adapter-internals.md`](./adapter-internals.md).
