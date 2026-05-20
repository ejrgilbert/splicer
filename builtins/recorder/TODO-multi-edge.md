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
   `on_edge: "<id>"` becomes a first-class selector alongside
   `between` and `before`. Higher-level selectors (`on_node`,
   `on_interface`, `between_subgraph`) expand to a set of per-edge
   rules at parse time, so operators can think in components or
   subsystems while the runtime primitive stays per-edge. Higher-level
   selectors accept optional refinement filters (specific interfaces
   or edge_ids to include/exclude) so the operator can wrap a subset
   of a unit's edges; default is "all edges in the unit."

   The selector vocabulary forms a clean generalization:

   - `between` / `on_edge`: one specific edge.
   - `on_node`: every edge touching one node (in a given direction).
   - `between_subgraph`: every edge crossing the boundary of a set of
     nodes. A subgraph is a set of nodes plus its internal edges; its
     boundary is "edges with exactly one endpoint in the set." Internal
     edges stay untouched and run live during replay, so the subgraph's
     guts execute for real with recorded boundary inputs.

   Example YAML for subgraph-level recording:

   ```yaml
   - between_subgraph:
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
            -> splicer derives edge_id, splices recorder, auto-injects edge_id
            -> recorder writes recordings/{edge_id}.bin

Replay phase:  user writes on_edge: "<edge_id>" rule
            -> splicer finds that edge in the composition, splices replayer
            -> replayer auto-receives the same edge_id, reads recordings/{edge_id}.bin
```

Same identity (`edge_id`) flows in both directions; the SDK never sees
it.

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
  change with a major version bump because they appear in user YAMLs
  and recording filenames.

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
| 2    | Splicer auto-injects `_splicer_edge_id` into every spliced builtin's config.     | not started         |
| 3    | Recorder reads `_splicer_edge_id`; file-sink lands; default sink switches to file (one file per edge). Stdout/stderr documented as single-instance-only. | not started |
| 4    | `on_edge: "<id>"` YAML selector. Splicer enumerates composition edges, derives ids, matches against the literal. Error message lists available edges when no match. | not started |
| 5    | `splicer edges <composition>` CLI subcommand for enumeration. Splicer also logs matched edge_ids during `splice` runs so operators can copy them from output. | not started |
| 6    | Higher-level selectors (`on_node`, `on_interface`, `between_subgraph`). Expand to multi per-edge rules at parse time. Default is "all edges in the unit"; optional `filter:` block narrows to a subset by interface name or explicit edge_id list. | not started |
| 7    | Replayer builtin (tier-4 virtualize). Consumes steps 2-6; no new primitives. Subset replay (virtualize some boundary edges, leave the rest live) falls out of step 6's filter block. See [`docs/TODO/tier3-tier4-substrate.md`](../../docs/TODO/tier3-tier4-substrate.md) for the `WrapperStrategy` + codegen template architecture that lands here. | not started |

Steps 2 and 3 unlock the recorder for multi-edge use. Steps 4 and 5
unlock the replayer's user-facing config. Step 6 is pure UX polish on
top. Step 7 is the actual replay implementation.

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
