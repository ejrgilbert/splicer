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

### Option 2 — auto async-bridge component (partial fix; codegen only)

> ⚠️ The design described here ships for tier-3/4 but **does not
> restore generality** the way the original write-up claimed. See
> "What Option 2 actually buys us" below.

When splicer sees a sync-WIT target, auto-generate two extra
components and re-wire the composition:

```
service-comp.handle (async-WIT, suspendable)
  └─ imports my:service/adder            [sync WIT, unchanged]
     └─ exported by: SYNC→ASYNC BRIDGE   [splicer-generated]
           body: async canon-lower onto my:service/adder-suspendable
                 + subtask wait
           └─ imports my:service/adder-suspendable  [async-WIT mirror, splicer-synthesized]
              └─ exported by: middleware adapter    [splicer-generated, ASYNC-WIT lift]
                    body: hooks + downstream
                    └─ imports my:service/adder     [sync WIT, real downstream]
                       └─ exported by: real adder
```

The bridge is the only sync-lifted component. Its body does async
canon-lower onto the async-WIT mirror and then `[waitable-set-wait]`
on the resulting packed status. As long as the lift actually returns
status=Returned (subtask completed inline), the wait branch is
skipped and the call passes through fine.

#### What Option 2 actually buys us

The original write-up claimed the bridge lets hook / wrapper bodies
"suspend freely" because the adapter is async-lifted. That's not how
the canon ABI works: wasmtime classifies a task by the WIT contract
with its **caller**, not by the callee's lift modifier. The bridge's
caller is the sync-WIT consumer, so the **bridge's task is sync**,
even though its body issues async canon-lower. If the subtask the
bridge is waiting on hasn't completed by the time the wait fires,
`[waitable-set-wait]` from a sync task is exactly the operation
wasmtime refuses (see "The constraint" above). The trap moves from
the adapter to the bridge; it doesn't disappear.

So Option 2 in practice is **codegen unification, not runtime
unblock**:

- ✅ Tier-3/4 wrappers always lift against the async mirror →
  `wit-bindgen` Rust always emits an `async fn` Guest trait, regardless
  of whether the target is sync or async at WIT. One codegen path.
- ✅ Wrapper / hook bodies that complete inline keep working
  (their futures resolve immediately, the wait branch never fires,
  the bridge passes through). This is the common case for the
  `hello-tier{3,4}` builtins, recorder, otel-bare-metrics, etc.
- ❌ Wrapper / hook bodies that genuinely suspend on a non-host
  async operation still trap — just at the bridge's wait instead
  of the adapter's. The fundamental sync-task-can't-yield invariant
  is unchanged.

For tier-1/2 the bridge buys us nothing — adapter emission is
wasm-encoder-generated, so we control the lift shape directly with
no wit-bindgen Rust in the picture. Inserting a bridge would just
add a component to the chain without changing what works at runtime.
Tier-1/2 stays on the inline path (no bridge) and the sync-target
limits there match what shipped pre-bridge.

#### Cost / scope of what landed

A week of work + a per-target bridge wasm at splice time. Splicer's
"any middleware on any interface" claim is still **not** true at
runtime; what changed is that the tier-3/4 codegen path is the same
shape across sync and async targets, which makes the wrapper Cargo
build deterministic regardless of which target it's pointed at.

The true runtime unblock requires Option 3.

#### Resource-bearing sync targets

The async-WIT mirror synthesizer pushes a new package whose
interface redeclares each function as `async func` and shares
named types from the original via `use orig.{...}`. That covers
freestanding sync functions on records, variants, lists, etc.
It does **not** cover resource-bound functions (methods, statics,
constructors) on the target interface, for two reasons:

1. WIT syntax requires methods/constructors/statics inside a
   `resource { … }` block — they can't be redeclared as
   freestanding `[method]foo.bar: async func(...)` at the
   interface level.
2. Redeclaring the resource in the async mirror's `resource { … }`
   block creates a **new** resource type with its own identity.
   Runtime handle traffic between the bridge (which sees
   `host::foo`) and the adapter (which sees `wrapped::foo`) would
   be rejected — the same wedge `require_no_inline_resources`
   already guards against at `src/adapter/abi/emit.rs`.

The async mirror synth bails on resource-bound functions with a
message pointing here.

A working recipe exists in [`research/proxy-component`][proxy]: it
wraps a whole component with a sync record/replay/fuzz proxy and
mediates resource identity across the boundary by:

- Cloning the entire target package into a `wrapped-<ns>:<pkg>`
  namespace via `WitPrinter::print_package` after renaming
  `package_names` and every `pkg.name.namespace` in a Resolve clone
  (no hand-written WIT text — methods/constructors/statics inside
  `resource { … }` blocks come along for free).
- Synthesizing a `proxy:conversion/conversion` interface declaring
  one `wrap-<R>` and one `unwrap-<R>` per resource type that
  appears in target signatures:

  ```wit
  package proxy:conversion;
  interface conversion {
      use wasi:foo/bar.{client as host-client};
      use wrapped-wasi:foo/bar.{client as wrapped-client};
      get-wrapped-client: func(x: host-client) -> wrapped-client;
      get-host-client:    func(x: wrapped-client) -> host-client;
  }
  ```

- Implementing the conversion functions as **no-op handle
  reinterprets**. Resource handles are u32 indices at the canon
  ABI; the "type" is purely a static-typing decoration, so a
  function that takes `host-client` and returns `wrapped-client`
  lowers to `func(i32) -> i32` with body `local.get 0; return`.
  The canon ABI carries the identity bookkeeping; the wasm body
  is trivial.

Porting this into the bridge would mean:

1. Replace the function-by-function WIT-text async mirror in
   `src/adapter/abi/async_mirror.rs` with a `WitPrinter`-based
   whole-package clone-and-rename.
2. Have the bridge component additionally import the conversion
   interface and emit one trivial passthrough body per
   `wrap-<R>` / `unwrap-<R>`.
3. Wire the conversion interface as a self-export so the bridge
   satisfies its own import.

Roughly doubles the surface the bridge covers (resource-bearing
sync targets join the supported set) at a cost of one extra
WIT-clone helper plus per-resource wasm passthroughs in the bridge.

[proxy]: https://github.com/chenyan2002/proxy-component

### Option 3 — make wasmtime drive non-host async subtasks from sync tasks

Sync tasks "can't suspend" in the strictest reading — but
`[waitable-set-wait]` from a sync task could in principle be
implemented by letting wasmtime drive the awaited subtask to
completion on VM-managed fibers without yielding control back to
the host scheduler. The sync caller's wasm execution stalls (which
it does today anyway, for wasi async calls); the subtask makes
progress; the wait resolves; sync caller resumes. No host-side
yield, no violation of the sync contract from the caller's view.

This is a wasmtime runtime change, not something splicer can paper
over. If we want a true e2e test of suspending hook / wrapper bodies
on sync-WIT targets, that's the path: file an upstream issue with
a minimal repro (one sync-WIT target + one middleware whose hook
genuinely awaits a non-wasi peer-component async function) and see
what falls out. The bridge as shipped is the right shape to consume
that fix when it lands — just inert until then.

## What we shipped today

- **Option 1** (preflight error) — see splicer source at the splice
  entry point. Targets the narrow detectable case (any canon-lower-async
  import in the middleware against a sync-WIT splice target).
- **Option 2** for tier-3/4 only — bridges sync-WIT targets so the
  wrapper crate's `wit-bindgen` Rust always sees an async-WIT mirror
  and emits an `async fn` Guest trait. Codegen unification, not a
  runtime unblock. Wrapper bodies whose `.await`s complete inline
  pass through; bodies that genuinely suspend still trap (now at the
  bridge instead of the adapter). The `hello-tier3` and `hello-tier4`
  builtin demos exercise this end-to-end.
- **Option 2 NOT for tier-1/2** — adapter emission is wasm-encoder-
  generated so codegen unification isn't a problem the bridge solves
  there. Tier-1/2 stays on the inline path; the sync-target limits
  are the same as pre-bridge: works for hook bodies that complete
  inline, traps for bodies that genuinely suspend on a non-host
  async peer.

Built-in substrate stays as the sync workaround described in
"Where splicer hits this", because shipped builtins need to keep
working with the current substrate shape regardless of which
target interface they're spliced on.

Option 3 is the only path that genuinely closes the gap. Budget
that work alongside upstream wasmtime when there's user demand.
