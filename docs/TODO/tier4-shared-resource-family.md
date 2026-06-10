# Tier-4 wrap of a single iface from a shared resource family

Tier-4 (`virtualize`) takes ownership of the resource type identity for
the target iface's sibling types iface (the wrapper world EXPORTS the
types iface; see `src/adapter/typed/target_wit.rs:Behavior::Virtualize`).
That's correct when the wrapped iface is the only consumer / producer
of that resource. It is **structurally inconsistent** when the same
sibling types iface is referenced by other ifaces the consumer also
imports unwrapped.

## The failure mode

Example fan-in topology (from `tests/component-interposition`):

```wit
interface async-bucket-types { resource bucket { /* ... */ } }
interface async-bucket       { use async-bucket-types.{bucket}; open: ...; }
interface bucket-as-arg      { use async-bucket-types.{bucket}; get: ...; }
```

Rule: tier-4 wrap `my:service/bucket-as-arg` between `shapes-handles-comp`
and `service-comp`. The wrapper's emitted world (per `target_wit.rs`):

```wit
world wrapper {
    export my:service/async-bucket-types;
    export my:service/bucket-as-arg;
}
```

The wrapper owns `async-bucket-types.bucket`. WAC routing (after the
fix in `with_chain_routing`'s second pass) correctly threads
`service-comp.imports.async-bucket-types` through the wrapper. But
`service-comp` *also* imports `my:service/async-bucket` from
`shapes-handles-comp`, and `shapes-handles-comp`'s exported
`async-bucket.bucket` references *its own* `async-bucket-types.bucket`,
not the wrapper's. wac compose rejects with:

```
type mismatch for import `my:service/async-bucket`
type mismatch in instance export `bucket`
resource types are not the same
```

In other words: the wrapper takes over the resource identity for one
slice of the topology but can't take it over for the other ifaces that
share the same factored types iface. The consumer's bucket-typed
imports straddle two distinct resource identities and wac rejects.

## Current workaround

Give the wrapped iface its own factored types iface (i.e., its own
private resource family). `tests/component-interposition` does this:

```wit
interface bucket-as-arg-types { resource bucket { /* ... */ } }
interface bucket-as-arg       { use bucket-as-arg-types.{bucket}; get: ...; }
```

Now `bucket-as-arg` shares no resource identity with `async-bucket` /
`async-bucket-types`. Tier-4 can wrap it without conflicting with the
other iface's bucket. This is what the `--builtin-hello-tier4` test
exercises.

Caveat: this works when the iface is purpose-built for the tier-4
demo, but is a structural change the iface author has to make
upfront. A user can't graft tier-4 onto an existing iface that
already shares a sibling types iface with other consumers.

## Why tier-3 doesn't hit this

Tier-3 (`transform`) IMPORTS the sibling types iface (see
`Behavior::Transform` in `target_wit.rs`) and does NOT take ownership
of the resource type. WAC routes both the target iface and the
sibling types iface from the same upstream producer (the wac.rs
sibling-types-wiring fix that landed alongside this doc); the
consumer's other unwrapped ifaces continue to reference the same
producer-provided resource identity. No conflict.

## Possible fixes

### (a) Config-time check + clear error

At preview / plan time, walk the topology: for each tier-4 rule on a
target iface T with sibling types iface S, find every iface in the
graph that `use`s S. If any such iface is NOT also wrapped (or
otherwise routed away from the original producer for the consumer in
question), bail with a precise message:

```
cannot tier-4-wrap `my:service/bucket-as-arg`: its sibling types
iface `my:service/async-bucket-types` is also used by
`my:service/async-bucket`, which this rule does not wrap.
Workarounds: give `bucket-as-arg` its own factored types iface, or
add a tier-4 rule for `async-bucket` too.
```

Lowest user-experience cost: the wac validation error becomes a
splicer config error with actionable guidance. Doesn't increase
tier-4's coverage, but stops surprising people. Estimated 2-4h
including a fanin fixture that asserts the rejection path.

### (b) Wrap the whole resource family at once

Generalize tier-4 to accept a "wrap this resource family" target:
when the rule names one iface and the topology has others using the
same sibling types iface, splicer auto-emits wrapper code for the
others too. Open questions:

- What strategy handles those auxiliary ifaces? Each has its own
  arg / return shape; one strategy probably can't cover all (the
  `R: Default` bound mismatch for `open: ... -> bucket` is exactly
  this shape).
- Does the user expect to write per-iface strategies, or a single
  strategy that handles the family? The substrate doesn't have a
  vocabulary for either yet.
- How does this compose with the existing
  [bound-mismatch-skip-and-warn](bound-mismatch-skip-and-warn.md)
  story? Auto-expanded ifaces might individually fail bounds.

Architectural cost is high. Defer until there's a real use case that
can't be addressed by (a) + factored-types refactoring.

### (c) Leave the iface's types iface imported (drop ownership)

Symmetric to tier-3: emit the wrapper world as
`import sibling-types; export target` instead of
`export sibling-types; export target`. The wrapper no longer owns the
resource type; consumer routes both ifaces from the original producer.
But then the tier-4 strategy can't synthesize resource handles via
`MockedResource::mint(...)` — it would have to forward to the original
producer just to materialize a handle. That's tier-3 dressed as tier-4
and probably doesn't pull its weight as a separate behavior.

## Recommended next step

Implement (a). It's small, it converts a confusing wac-validation
error into a clear config-time message, and it doesn't preclude (b)
later if there's demand. (c) is dead-on-arrival unless someone finds
a use case where tier-3 semantics with tier-4 codegen pays for itself.

## References

- `src/adapter/typed/target_wit.rs` — `Behavior::Virtualize` world
  emission (exports sibling types iface).
- `src/wac.rs` — `with_chain_routing` second pass that routes
  consumer's sibling-types iface through the wrapper for tier-4. The
  wac wiring is correct; the structural conflict surfaces a layer
  above.
- `tests/component-interposition/splicer-rules/builtin-hello-tier4.yaml`
  — the test that exercises the working case (`bucket-as-arg` with
  its own factored types iface).
- `tests/component-interposition/fan-in/shared-wit/my.service/package.wit`
  — see how `bucket-as-arg` factored its own `bucket-as-arg-types`
  to avoid the conflict.
- [bound-mismatch-skip-and-warn.md](bound-mismatch-skip-and-warn.md)
  — relevant for (b): per-iface bound failures during auto-expansion
  would need graceful handling.
- [resource-method-interception.md](resource-method-interception.md)
  — broader resource-method dispatch context.
