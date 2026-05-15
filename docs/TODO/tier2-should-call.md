# Tier-2 `should-call` (content-aware blocking)

Tier 1 ships a `blocking` interface (`should-block(call) -> bool`); tier 2
does not. That breaks the "strict superset" property the tier ladder
otherwise has, and it leaves an obvious capability gap: **content-aware
policy gates** ("skip the downstream call if the request body matches X")
have nowhere clean to live today.

- Tier 1 can decide whether to call downstream, but can't see the value.
- Tier 2 can see the value, but can't decide whether to call downstream.
- Tier 3 mutates the value through; doesn't skip the call.
- Tier 4 replaces the call entirely (downstream isn't in the adapter at all).

The "observe only" tagline on tier 2 is about **value mutation**, not
**control flow** — those are orthogonal axes. Adding a flow-control hook
that doesn't return a modified value preserves the "values are read-only"
property cleanly.

## Naming: `should-call` vs `should-block`

Tier 1's hook is `should-block(call) -> bool` — `true` means skip
downstream. Two options for tier 2:

1. **`should-block(call, args) -> bool`** — mirror tier 1 verbatim.
   Predictable; same mental model as the tier-1 hook.
2. **`should-call(call, args) -> bool`** — invert polarity. `true` means
   call downstream. Matches the framing of the README's
   "Calls downstream?" column and reads more naturally: "given these
   args, should we call?".

Recommendation: go with `should-call` and revise tier 1 to match in the
same release window if the polarity flip is acceptable as a breaking
change. Inverted polarity is what users keep tripping over in tier-1
demos. If we don't want to touch tier 1, keep `should-block` for tier 2
so the two tiers stay aligned.

## Shape

```wit
interface blocking {
    use splicer:common/types@<v>.{call-id, field};
    /// Called before every invocation of a target-interface function.
    /// Return `true` to invoke downstream; `false` to skip.
    /// Only supported for void-returning functions (see below).
    should-call: async func(call: call-id, args: list<field>) -> bool;
}

world tier2-middleware {
    export before;
    export after;
    export blocking;
}
```

Separate `blocking` interface, opt-in by export — same pattern as tier 1.
A middleware that just wants observation doesn't export it and pays
nothing.

## Constraints inherited from tier 1

`should-block` at tier 1 is **only supported for void-returning
functions today** (`tests/component-interposition` has the bailout test).
A non-void function whose downstream call is skipped leaves the adapter
holding the question of what to return — that escalates into tier-4
territory (fabricated result) and is out of scope for "skip the call".

Tier 2 inherits the same limitation. The implementation must:

- Refuse to wire `blocking` against a non-void target function with a
  clear error (same surface as the tier-1 bailout).
- Plumb the lifted `args` into the call site so the middleware can
  inspect them before answering — this is the new bit vs tier 1, which
  only has the call-id.

## Implementation sketch

- WIT: add `blocking` interface to `wit/tier2/world.wit` with `should-call`
  (or `should-block`). Bump `splicer:tier2` version.
- Adapter codegen: extend `src/adapter/tier2/` to detect a middleware
  that exports `splicer:tier2/blocking`, mirroring the tier-1 detection in
  `src/adapter/tier1/`. The void-return precondition check can be lifted
  from the tier-1 path verbatim.
- The lifted `args` reuse the same `field-tree` plan that
  `before::on-call` already builds — `blocking::should-call` reuses that
  plan, then branches on the returned bool to either call downstream or
  jump to the post-call path (with `after::on-return` still firing, since
  observation hooks are independent of the gate).
- Tests: mirror the tier-1 blocking integration tests with a tier-2
  variant that asserts the `bool` return correctly gates the call.

## Open questions

1. **Polarity.** `should-call` (true = call) vs `should-block` (true =
   skip). Pick one and live with it.
2. **Does `after::on-return` fire when `should-call` returns false?**
   At tier 1 today it does **not** — the call didn't happen, so there's
   no return. Tier 2 should match: hooks fire iff the call actually
   ran. (Alternative: fire `on-return` with `result: none` to signal a
   gated call. Probably worse — `none` already means void.)
3. **Interaction with the planned tier-3 modify path.** When tier 3
   lands, the `blocking` interface is orthogonal — a middleware that
   exports `blocking` + `modify` would gate the call AND modify args
   for the call when it does happen. Worth pinning before we ship to
   avoid retroactively re-shaping the WIT.

## Why this is queued, not in flight

- The strict-superset argument is design preference, not a correctness
  bug.
- Tier 1's `should-block` covers identity-keyed policy gates today; the
  gap is specifically content-aware gates, and we have no committed
  consumer of that capability yet.
- Naming/polarity decision wants real consumer input (an unprefixed
  OTel builtin alone won't surface the awkward case the way a real
  policy middleware would).

Land once a concrete consumer asks for it, or as part of the same
release as `splicer:tier2 1.0` if we want to stop bumping the WIT every
time a new hook is added.

## Pointers

- Tier-1 blocking WIT: [`wit/tier1/world.wit`](../../wit/tier1/world.wit)
  (`interface blocking`).
- Tier-1 blocking codegen: `src/adapter/tier1/` (search for `should-block`).
- Tier-2 codegen entry: `src/adapter/tier2/`.
- README ladder + the "skippable vs not-in-adapter" distinction:
  [`README.md`](../../README.md#middleware-tiers).
