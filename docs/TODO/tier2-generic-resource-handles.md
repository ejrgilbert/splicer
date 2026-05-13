# Tier-2 generic resource handles

Tier-2 today encodes every resource it lifts as
`cell::resource-handle(u32) → handle-infos[idx]` where `handle-info`
is `{ type-name: string, id: u64 }` — opaque correlation id, no live
handle. Middleware can name the resource and tell when the same one
appeared twice, but cannot read anything off it. For OTel-style
attribute extraction (e.g. "url + method off the wrapped
`wasi:http/types.request`") that's a hard ceiling — the only paths
forward today are (a) accept opaque-only observation, (b) author
per-target middleware with concrete `borrow<R>` in its WIT,
sacrificing the interface-agnostic contract that makes tier-2
worth using.

Worth designing the third path: a single `borrow<R>`-shaped surface
on tier-2's contract that's still polymorphic over the wrapped
interface, with a discoverable extension surface so middleware can
actually use it without per-target compile-time knowledge.

## Proposal: opaque-wrapper resource

Define one resource in `splicer:common@<next>`:

```wit
resource resource-ref {
    /// Fully-qualified name of the wrapped resource type
    /// (e.g. "wasi:http/types@0.3.0.request").
    kind: func() -> string;
    /// Stable identity. Same underlying handle returns the same
    /// id across callbacks; replaces today's `handle-info.id`.
    identity: func() -> u64;
    // ── attribute surface — see "discoverability" below ──
}
```

The splicer-generated adapter constructs a fresh `resource-ref`
around each intercepted underlying handle and passes
`borrow<resource-ref>` to the middleware. Tier-2's `cell` variant
gains:

```wit
variant cell {
    ...
    // Replaces resource-handle(u32) → handle-infos[idx].
    // Borrow lifetime = the tier-2 callback invocation.
    resource-borrow(borrow<resource-ref>),
    ...
}
```

Two things this buys versus today's `handle-info` record:

- Borrow lifetime is enforced by the component model — middleware
  can't accidentally retain a handle across the callback boundary.
- The wrapper is a real handle, so adapter-side state (live
  underlying handle, lazy-computed attributes) can hang off it
  cleanly without piggybacking on the lifted tree.

## The discoverability problem

`attribute(key: string) -> option<string>` is the natural-looking
shape, but it assumes the middleware knows which keys to ask for —
which it can't, because that's the load-bearing constraint tier-2
exists to relax (middleware doesn't know the wrapped interface's
types). The adapter knows what attributes the underlying resource
exposes; the middleware doesn't. Bridging that gap is the design
problem this doc exists to scope.

Four sketches, listed cheap-to-rich:

### A — Key list + per-key fetch

```wit
resource resource-ref {
    kind: func() -> string;
    identity: func() -> u64;
    attribute-keys: func() -> list<string>;
    attribute: func(key: string) -> option<string>;
}
```

Middleware calls `attribute-keys()` to learn what's available, then
queries the ones it cares about. Two host calls per resource if the
middleware wants all of them; cheap if it wants a fixed subset.
Type-erased — every value is `string`, so binary fields and numeric
fields have to round-trip through their string encodings.

### B — Pre-lifted map

```wit
resource resource-ref {
    kind: func() -> string;
    identity: func() -> u64;
    attributes: func() -> list<tuple<string, string>>;
}
```

One host call per resource; middleware walks the result. Adapter
serializes every attribute whether the middleware reads it or not,
so it's wasteful when a middleware only cares about `identity()`.
Mitigation: adapter-side caching keyed by underlying handle, so the
serialization cost is paid once per resource.

### C — Typed attributes

Borrow tier-2's own `cell` representation for attribute values:

```wit
resource resource-ref {
    kind: func() -> string;
    identity: func() -> u64;
    attributes: func() -> list<tuple<string, cell>>;
}
```

Now attributes can be `cell::integer`, `cell::list-of`, nested
records, etc. — full type fidelity. Cost: the adapter has to lift
every exposed attribute through the tier-2 lifting machinery,
including allocating side-table entries for any nominal types. For
attribute-heavy resources this is the same lift the field-tree
already does for primitive function args, so the machinery is
reusable.

Probably the right shape long-term; the right shape for β depends on
whether any planned consumer needs typed attribute values rather
than stringly-typed ones.

### D — Lift attributes into the field-tree itself

Drop the wrapper resource entirely. Have the adapter lift each
resource's attributes as a record inline in the field-tree:

```wit
variant cell {
    ...
    // Was resource-handle(u32) → handle-infos[idx].
    // Now also references a record cell for the attributes.
    resource-attrs(u32),                       // → resource-info side-table
}

record resource-info {
    type-name: string,
    identity: u64,
    /// Index of a record cell holding the attribute map.
    attributes: u32,
}
```

Maximum discoverability — the tree contains everything; the
middleware doesn't have to call any methods. Side effect: kills the
`borrow<R>` story entirely. No handle to interact with, no
tier-3-friendly path. Strictly weaker than C for any future
"transform the resource" use case, even if it's nicer for pure
observation today.

## Open questions

- **Which discoverability shape for β?** C feels right but needs a
  concrete consumer to validate the typed-attribute lift cost. A is
  the cheapest landing pad; B is the worst of both. D simplifies the
  middleware side at the cost of foreclosing tier-3.

- **Who decides the attribute set per resource?** The adapter
  codegen has to know what `wasi:http/types.request`'s observable
  attributes are. Options: hardcode a registry inside splicer per
  well-known interface (small, doesn't scale); read attribute
  declarations from a sibling WIT custom section (forces resource
  authors to opt in); reflectively walk the resource's exported
  method set and treat zero-arg getters as attributes (clever but
  brittle — methods can have side effects, return types aren't
  always lift-friendly).

- **Drop semantics under panic.** The adapter constructs
  `resource-ref` before each callback. If the wrapped call traps
  partway through, the borrow's destructor runs through normal
  component-model rules, but the host-side state attached to the
  wrapper (cached attributes, underlying-handle reference) needs to
  release deterministically. Mostly a host-implementation concern,
  worth confirming the wasmtime semantics match before relying on
  it.

- **Cross-call identity vs cross-instance identity.** Today's `id`
  is per-component-instance-lifetime (handle-table-stable). Should
  `identity()` preserve that, or upgrade to something stronger
  (e.g. a hash of the resource's content so the same logical
  resource appearing through two adapter chains compares equal)?
  Stronger identity is a much harder design problem; per-instance
  parity with today is the safe default.

- **Interaction with tier-3.** Tier-3 (transform) requires concrete
  resource types — `borrow<request>` so the middleware can call
  `request.uri()` etc. Tier-3's WIT will necessarily be per-target.
  Worth confirming that tier-3's per-target contract and tier-2's
  generic contract can coexist on the same wrapped interface
  without the field-tree representation forking awkwardly.

## Why not just keep the opaque id?

Acceptable position; this design only justifies itself if a
non-trivial consumer actually wants resource attribute extraction.
The tier-1 OTel builtins don't (they only see `call-id`, no
resources). A tier-2 OTel sibling (`splicer:otel-spans` etc., not
yet implemented) might want it for HTTP semantic-convention
attributes (`http.url`, `http.method`, `http.status_code`). If that
sibling's design lands on "extract from the request/response
resource" instead of "extract from the lifted argument tree," that's
the moment to revisit this doc.
