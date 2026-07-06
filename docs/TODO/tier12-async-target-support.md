# Tier-1/2 middleware requires sync-WIT targets

Tier-1 and tier-2 adapters export the **same WIT interface** as their target. For
sync-WIT targets the adapter export is also sync, the adapter task carries no
canon-async machinery, and the full performance benefit of a sync middleware is
preserved. That property is the point: paying async overhead on a sync target
for no benefit was the wrong trade-off.

As a result, tier-1 and tier-2 are **restricted to sync-WIT targets**. Async-WIT
targets are rejected at splice time.

Tier-3 and tier-4 are unrestricted -- the async-mirror bridge
(`src/adapter/abi/async_mirror.rs`) synthesizes an async-WIT mirror for any
sync-WIT target, so the wrapper crate always sees an async-WIT interface at build
time. Both sync and async WIT targets work for tier-3/4.

## The gap

There is no path to apply tier-1 or tier-2 middleware to an **async-WIT target**
today. The splice is rejected.

## Options for async-target support in tier-1/2

### Option A -- route async targets through the async-mirror bridge

When a tier-1 or tier-2 middleware is spliced onto an async-WIT target, use the
same bridge tier-3/4 already uses. The adapter exports async-WIT to match the
target. Hook bodies that complete inline pass through; the sync-performance
guarantee simply doesn't apply to targets that were already async.

Trade-off: the performance character of tier-1/2 becomes conditional on the
target WIT shape. A user pointing an existing tier-1 builtin at an async target
would silently get different runtime behavior than on a sync target.

### Option B -- extend tier-1/2 adapter emission to support async exports

The wasm-encoder path in tier-1/2 emits sync canon-lift for exported functions
today. Adding async canon-lift (and async-WIT exports) for async targets would
let tier-1/2 cover async targets natively without borrowing the bridge machinery.
The hook call path is already async internally, so the delta is in how the export
is lifted, not how hooks are called.

Trade-off: more emission complexity and a second code path inside the
wasm-encoder generator.

### Option C -- expose both a sync and an async WIT interface from one adapter

Synthesize an adapter that exports both a sync variant and an async variant of
the target interface. Sync callers consume the sync export; async callers consume
the async export.

Trade-off: genuinely muddy. The sync export of an async target still can't
suspend, so the sync-task invariant problem is not resolved -- it's just deferred
to the export the caller picks. Composition tooling would need to understand
which export to wire. Likely more complexity than it buys.

### Current status

No option is implemented for async targets in tier-1/2. The correct choice
depends on whether async targets are a first-class use case or an edge case --
Option B is the cleanest if tier-1/2 should have full parity with tier-3/4, but
Option A is lower effort if routing through the bridge is acceptable.
