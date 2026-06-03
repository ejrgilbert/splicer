# Multi-edge recorder/replayer architecture

The recorder as shipped today is correct for single-edge splices. When
spliced onto multiple edges in one composition, every instance writes
to the same sink (stdout/stderr by default), and the SDK's wire-format
stream headers + per-instance call.id counters collide. The events
cannot be demultiplexed, and a future replayer would have no way to
tell which recorded events belong to which edge.

This doc captures the architectural shape that resolves this, plus the
incremental implementation roadmap that lands it without rewriting any
of the existing pieces.

## The architecture, in four layers

Each layer owns exactly one concern. Nothing crosses lanes.

1. **SDK** owns "typed event ↔ binary bytes." Pure value-level
   serialization. No awareness of graph position, sink routing, or
   identity. The wire format is exactly what `splicer-tool-sdk` ships
   today and does not grow new fields.

2. **Recorder / replayer** own "encode/decode into a sink that
   identifies the edge." Each spliced instance writes (or reads) one
   file named from its edge id. One file = one stream = one edge.
   Sink sharing (stdout/stderr) stays as a debug option, documented
   as single-instance-only.

3. **Splicer runtime injection** owns "tell each spliced builtin which
   edge it occupies." At splice time, splicer derives a canonical
   `edge_id` per matched rule and auto-injects it into the builtin's
   config substrate under a reserved key (`_splicer_edge_id` or
   similar). The recorder reads it to pick its sink path; the replayer
   reads it to locate its recording. Every future builtin that needs
   graph-position context gets the same key for free.

4. **YAML grammar** owns "let the operator describe what to wrap."
   The existing `before` and `between` already address one specific
   edge by (interface, provider, caller); the new selectors are
   structural shorthands that resolve to a set of per-edge rules:

   - `on_node`: every edge touching one node (in a given direction).
     Inbound = node is the provider side (`inner`); outbound = node
     is the caller side (`outer`); both = the union. Desugars at
     parse time to one or two `before`/`between` rules with the
     node name as the inner or outer constraint.
   - `on_subgraph`: every edge crossing the boundary of a set
     of nodes. A subgraph is a set of nodes plus its internal edges;
     its boundary is "edges with exactly one endpoint in the set."
     Internal edges stay untouched and run live during replay, so
     the subgraph's guts execute for real with recorded boundary
     inputs. This one needs the composition graph to resolve (set
     negation isn't expressible as a glob), so it expands during a
     graph-aware resolve pass after `parse_yaml`.

   `on_edge: "<id>"` and `on_interface: <name>` were earlier candidates
   but offered no expressive delta over what `between` / `before` already
   provide. `on_edge` is a fully-specified `between` with the structure
   flattened into an opaque string; `on_interface` is `before`/`between`
   with no node constraint. Neither shipped — `edge_id` stays a purely
   internal addressing scheme (filename key + auto-injected config
   substrate), and `on_interface` is just `before: { interface: ... }`.

   Higher-level selectors accept an optional `filter: { interface:
   <glob> }` block that narrows the set by interface — useful when a
   subgraph spans many interfaces and the operator only wants to
   wrap, e.g., the HTTP boundary.

   Example YAML for subgraph-level recording:

   ```yaml
   - on_subgraph:
       nodes: [B, C, D]
       direction: inbound
     inject:
       - builtin: recorder
         config:
           dir: ./recordings/bcd/
   ```

   Expands to one per-edge rule per boundary edge of {B, C, D}. Same
   edge_id format, same file-per-edge sink convention. Replaying the
   same subgraph uses the same selector with the replayer; matching
   `filter:` blocks let the operator virtualize only a subset of the
   boundary (e.g. mock the slow external service, leave in-process
   peers live).

   The `nodes: [...]` form is topology-agnostic by construction. The
   boundary is defined as "edges with exactly one endpoint in the set,"
   which works for any shape the chosen nodes happen to form
   internally: linear chains, fanin, fanout, diamonds, arbitrary
   subgraphs. Internal edges (both endpoints in the set) stay untouched
   and run live during replay, so the subgraph's internal interaction
   logic executes for real with recorded boundary inputs. Operators
   describe *which nodes are in*; splicer computes *what crosses the
   boundary*.

The round-trip becomes:

```
Record phase:  user writes structural rule (between/on_node/...)
            -> splicer derives edge_id per matched edge, splices recorder
            -> recorder writes recordings/{edge_id}.bin

Replay phase:  user writes the same structural rule with the replayer
            -> splicer re-derives the same edge_id deterministically
            -> replayer auto-receives the edge_id, reads recordings/{edge_id}.bin
```

`edge_id` derivation is deterministic on (interface, from, to), so the
record and replay rules don't have to be textually identical — any rule
that matches the same edge produces the same id. The SDK never sees the
id directly; it reaches builtins via the auto-injected
`_splicer_edge_id` config substrate.

## Canonical edge_id format

A descriptive structured id that operators can read without tooling.

```
{interface_name}::{from_node}->{to_node}             # between rules
{interface_name}::*->{to_node}                       # before rules
```

Example: `wasi:http/handler@0.3.0-rc-2026-01-06::srv-b->srv-a`.

Properties this format must hold:

- **Deterministic.** Same composition + same rules produce same ids
  across runs.
- **Stable to inconsequential changes.** Re-encoding a wasm, reordering
  unrelated nodes, etc. should not change the id.
- **Unique within a composition.** Two distinct edges never collide.
- **Public contract.** Once published, the derivation rules can only
  change with a major version bump — they appear in recording
  filenames and in the auto-injected `_splicer_edge_id` builtin
  config. They do **not** appear in user YAML; the higher-level
  selectors (`on_node`, `on_subgraph`) and the existing
  `before` / `between` are the only edge-naming surface operators
  see.

Recording filenames use a filesystem-sanitized form of the id (`:` and
`/` and `>` are not portable across all OSes). The recording file's
content stays pure SDK-encoded bytes; only the path encodes the id.

## Implementation roadmap

Each step is a separate PR with its own user-visible win. None
invalidate the SDK's wire format, the recorder's existing flush
logic, or the encode/decode contracts.

| Step | What lands                                                                       | Status              |
| ---- | -------------------------------------------------------------------------------- | ------------------- |
| 1    | Recorder ships as-is (single-edge, single-sink).                                 | **done**            |
| 2    | Splicer auto-injects `_splicer_edge_id` into every spliced builtin's config.     | **done**            |
| 3    | Recorder reads `_splicer_edge_id`; file-sink lands; default sink switches to file (one file per edge). Stdout/stderr documented as single-instance-only. | **done**    |
| 4    | `on_node: { name, direction }` YAML selector. Desugars at parse time to one or two `before`/`between` rules. Optional `filter: { interface: <glob> }`. | not started |
| 5    | `on_subgraph: { nodes, direction }` YAML selector. Resolves against the composition graph (boundary = "exactly one endpoint in the set"). Optional `filter: { interface: <glob> }`. | not started |
| 6    | `splicer edges <composition>` CLI subcommand listing each edge with its canonical id and the equivalent `between` block. Discovery aid for reading recorder output and writing matching replay rules. Splicer also logs matched edge_ids during `splice` runs. | not started |
| 7    | Replayer builtin (tier-4 virtualize). Consumes steps 2-5; the same structural rule the recorder used (re-derives the matching edge_id). Subset replay (virtualize some boundary edges, leave the rest live) falls out of step 5's filter block. See [`docs/TODO/tier3-tier4-substrate.md`](../../docs/TODO/tier3-tier4-substrate.md) for the `WrapperStrategy` + codegen template architecture that lands here. | not started |

Steps 2 and 3 unlock the recorder for multi-edge use. Steps 4 and 5
unlock the higher-level selectors that make multi-edge rules ergonomic.
Step 6 is a discovery aid. Step 7 is the actual replay implementation.

## Known gaps / future work

- **Causal ordering across edges.** Per-edge recording captures each
  boundary's events in isolation. If a node makes concurrent calls on
  different edges and its behavior depends on which result arrives
  first, replay needs cross-edge sequencing info that isn't in any
  single recording file. Sequential request-response (the common case)
  is unaffected; genuinely concurrent / timing-sensitive replay needs
  a future "trace" layer with global causal ordering. Scope cut for
  now; call it out in the paper.

- **Edge_id derivation drift.** Once edge_id format is published as a
  public contract, changing the derivation (e.g., to include
  composition hash, or to drop the interface version) is a breaking
  change that invalidates every archived recording. The format should
  be designed deliberately before any first publish, with version
  tagging in mind.

- **Aliases for long ids.** A user-defined `edges: { http_main: "..." }`
  YAML block would let operators reference long edge_ids by short
  names. Useful but not load-bearing; add when there's a real demo
  pain point.

- **Sidecar metadata for recordings.** A `.meta` file alongside each
  `.bin` carrying provenance info (composition hash, splicer version,
  edge_id in full canonical form, timestamp) would let replayers
  verify they're loading the right recording for the right
  composition. Probably wanted before any archived-recording use case
  ships.

- **Visual selection / verification via cviz.** Extending cviz to
  highlight a set of nodes and emphasize the boundary edges that would
  be wrapped gives operators a graphical companion to the text tools
  (`splicer edges`, `splicer subgraphs`). Same underlying graph
  computation, different output medium. Two flows pay off immediately:
  `cviz comp.wasm --highlight A,B,C` for visual selection, and
  `cviz comp.wasm --from-yaml splice.yaml` to see what a draft YAML
  would actually touch before running splicer. Picks up filter blocks
  and subgraph selectors cleanly (greyed-out vs. selected edges).
  Contiguity gaps in a chosen node-set become self-evident from the
  picture, surfacing misconfigurations earlier than splicer's own
  warning would.
