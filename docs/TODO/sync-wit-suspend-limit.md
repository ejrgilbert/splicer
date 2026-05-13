# Sync-WIT splice targets can't host suspending middleware bodies

Splicer's stated value proposition is "any tier-N middleware can be
interposed onto any interface". That's not quite true today: when the
**target interface is sync at WIT** (`func`, not `async func`) and the
middleware's hook body **actually suspends** during a call (awaits
anything that doesn't run to completion inline), the spliced
component deadlocks. The wedge is a hard wasmtime trap once the
suspend fires; in the wasi:http runner path it surfaces as a silent
hang because the trap is swallowed by the concurrent-IO wrapper.

## The constraint

The canon ABI defines a function's sync/async-ness by its **WIT type
signature**, not by the lift modifier on the canon-lift. A `canon
lift ... async (callback)` or `canon lift ... async` on a sync-WIT
function does change the wasm signature shape, but the wasmtime
runtime classifies the resulting task as a **synchronous task** —
because the WIT contract with the caller is sync (call returns
result atomically, no future). A sync task is not allowed to suspend
mid-execution. Anywhere down its subtree, a `canon lower ... async`
that needs to actually wait for completion (rather than completing
inline) trips wasmtime's invariant check:

```
wasm trap: cannot block a synchronous task before returning
```

This is correct behavior. Allowing sync to suspend would silently
break sync's defining property (caller gets a result, atomically).
The trap is wasmtime catching the wrong-axis-of-suspendability at
the moment the wait would actually block.

## Where splicer hits this

A tier-1 / tier-2 adapter exports the **same WIT interface** as the
target it wraps — that's the whole point: it has to be drop-in
replaceable. If the target is sync at WIT, the adapter's export is
also sync at WIT, so the adapter's task is sync.

The adapter body uses canon-async machinery for hook calls
(`splicer:tier1/before.on-call`, `splicer:tier1/after.on-return`,
the tier-2 equivalents — all `async func`). Today every hook
returns inline, so the wait machinery never actually blocks and
nothing trips. **As soon as a hook body awaits something that
doesn't complete inline**, the sync-task root traps.

The first such case to ship was `splicer:builtin-config/get` —
config-substrate-consuming middlewares (built-in `hello-tier1`,
`hello-tier2`, `otel-bare-{spans,metrics,logs}`) called `get`
through canon-lower-async. We've already worked around it for
shipped builtins by:

- declaring `splicer:builtin-config/get.get` as **sync** at WIT,
- building `config-provider` without `async: true` (so it lifts
  sync),
- and in each consumer's `wit_bindgen::generate!`, using the
  per-export async filter to mark **only** the `tier1`/`tier2`
  hook exports async — every import (the substrate `get`, plus
  `wasi:clocks`, `wasi:random`, `wasi:otel/*`) stays sync at the
  bindgen level, lowering as plain `canon lower` (no async).

That makes the substrate call run inline through wit-component's
sync-into-sync shim — no canon-async wait, no suspension, no trap.

But this is a per-builtin workaround. A **user-authored** tier-1 /
tier-2 middleware that imports any peer-component async-WIT
function from its hook body **will deadlock** when spliced on a
sync-WIT interface. There's no compile-time check stopping them,
and the failure surfaces as a runtime hang, not a clear error.

## What's broken in practice

- ✅ Sync-WIT target + middleware hook body uses only host imports
  that complete inline (e.g. `println!`-only) → works.
- ✅ Sync-WIT target + middleware hook body uses canon-lower-async
  imports that always return inline (`get_config` returning from a
  HashMap; the wit-component shim collapses canon-lower-async into
  sync-completing canon-lift to "Returned" status without ever
  waiting) → works.
- ✅ Async-WIT target + middleware hook body suspends → works
  fully; adapter task is async, suspension propagates naturally.
- ❌ Sync-WIT target + middleware hook body awaits something that
  is not synchronously completable (peer component that itself
  suspends, host import that yields control) → trap or hang.

## Options for a proper fix

### Option 1 — preflight error (cheap, ships first)

At splice time:

1. Inspect the target interface: does any function in it have sync
   WIT (`func`, not `async func`)?
2. Inspect the middleware wasm: any `canon lower ... async` op
   targeting an import that isn't host-provided (anything outside
   `wasi:*`)?

If both, refuse the splice with a message pointing at this doc.
Conservative — refuses some valid cases where the async-lowered
peer call happens to always run inline — but never wrong, and
always loud. Cost: ~a day.

### Option 2 — auto async-bridge component (restores generality)

When splicer sees a sync-WIT target, auto-generate two extra
components and re-wire the composition:

```
service-comp.handle (async-WIT, suspendable)
  └─ imports my:service/adder            [sync WIT, unchanged]
     └─ exported by: SYNC→ASYNC BRIDGE   [splicer-generated]
           body: sync canon-lower onto my:service/adder-suspendable
           └─ imports my:service/adder-suspendable  [async-WIT mirror, splicer-synthesized]
              └─ exported by: middleware adapter    [splicer-generated, ASYNC-WIT lift]
                    body: hooks + downstream
                    └─ imports my:service/adder     [sync WIT, real downstream]
                       └─ exported by: real adder
```

The sync→async bridge is the only sync-lifted component. Its body
does `canon lower` (sync, no async modifier) onto the async-WIT
mirror — the canon ABI defines this as the caller blocking while
wasmtime's stackful-async runtime drives the callee on a fiber.
The bridge never tries to canon-async-wait, so no sync-task
suspension is ever attempted. The middleware adapter is async-WIT
lifted, so its hook bodies can suspend freely.

Cost: a week. Adds one extra component per spliced sync-WIT site,
plus the codegen template for the bridge. Splicer's "any
middleware on any interface" claim becomes true again.

### Option 3 — make wasmtime allow sync to suspend

Off the table. Sync-suspends would violate the sync contract.

## What we shipped today

Option 1 (preflight error) — see splicer source at the splice
entry point. Targets the narrow detectable case (any canon-lower-async
import in the middleware against a sync-WIT splice target). Errors
out at splice time with a message pointing at this doc.

Built-in substrate stays as the sync workaround described in
"Where splicer hits this", because shipped builtins need to keep
working with the current substrate shape regardless of which
target interface they're spliced on.

Option 2 is the proper fix; budget when there's demand from a
user wanting to splice their own substrate-consuming middleware on
a sync-WIT interface.
