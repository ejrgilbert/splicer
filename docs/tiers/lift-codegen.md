# Adapter lift codegen

Design and rationale behind tier-2's lift codegen. For each
function in the target interface, splicer emits a **wrapper** — a
wasm function inside the adapter component that sits between the
importer (caller) and the handler (downstream implementation).
The wrapper invokes the middleware's `before` / `after` hooks
around the forwarded call, handing them a lifted `field-tree`
view of the args and result. This doc is for contributors editing
`src/adapter/tier2/`.

The wire format (cell shape, field-tree fields, side-table records)
is specified in [`tier-2.md`](./tier-2.md). Implementation details
(struct shapes, match arms, helper names) live in the code; they
rot fastest and are easiest to read at the source.

> **Scope.** Tier-2 today. Tier-1 hooks pass raw canonical-ABI
> bytes through with no lift. Tier-3+ is not yet implemented; when
> it lands, extend this doc with its model.

---

## What this codegen does

The component model's **canonical ABI flattens** every typed WIT
value into a sequence of primitive wasm flat slots (i32 / i64 /
f32 / f64) when crossing a function boundary. Compound values
either fit in flat slots (small records, tuples) or get spilled
to a caller-provided "retptr" memory region. By the time control
reaches our wrapper, the typed value has already been reduced to
that flat shape — there is no typed Rust struct to inspect, just
wasm locals and pointers.

**Lift codegen produces wasm that re-typifies the flat input into
a structured observation** the middleware reads. For each value
position in the WIT signature the wrapper writes one **cell** — a
tagged-union record naming the kind (`bool`, `text`, `record-of`,
`variant-case`, `*-handle`, …) and a payload (a primitive value,
a `(ptr, len)` slice, an index into a side table). Cells for one
(function, param | result) sit contiguously in a **cells slab**;
a **field-tree** points at the slab plus per-kind **side tables**
carrying build-time-const metadata like type-names and case-name
lists. The side tables are the `record-infos` / `flags-infos` /
`enum-infos` / `variant-infos` / `handle-infos` lists on
`field-tree` in `wit/common/world.wit` — this doc calls them
"side tables", the WIT names them `*-infos`, same thing either way.

Compound cells reference their children as **siblings in the same
slab** via `cell-idx` rather than embedding payloads inline — so
records, tuples, options, results, variants, and list elements
are all addressed uniformly. Lists are dynamically sized: their cell
payload points at a `cabi_realloc`'d sub-slab the wrapper grew at
call time. The field-tree is **observation, not value
transformation** — the underlying canonical-ABI flat values pass
through to the handler unchanged.

---

## Wrapper-body shape

For each function in the target interface, splicer emits one
**wrapper** — a wasm function inside the adapter component that
sits between the importer (the caller) and the handler (the
downstream component supplying the real implementation). The
wrapper's job is to invoke the middleware's `before` / `after`
hooks around the forwarded call: it populates a `field-tree` from
the canonical-ABI flat args, calls `on-call`, forwards to the
handler, populates a `field-tree` from the result, calls
`on-return`, then returns to the importer. Body has four
sub-phases. The `on-call` and `on-return` phases are each only
emitted when the middleware exports the corresponding hook
(`before` for `on-call`, `after` for `on-return`) — a
middleware can export either, both, or only one.

```text
on-call:    lift every param cell → call $on_call hook
forward:    pass the canonical-ABI flat args through to the
            downstream handler, get the result back (flat or
            via retptr depending on the canonical-ABI shape)
on-return:  if the function has a liftable result, lift the
            result cell(s); call $on_return hook (with `none`
            when there's no result to lift)
tail:       async task.return (with retptr or flat) or sync return
```

---

## Pipeline

For each target function the adapter walks three phases:

```
WIT param/result types
       ↓ classify
  LiftPlan (per param, per result)
       ↓ layout
  static memory (cells slabs, side tables, fields blob)
       ↓ emit
  per-fn wrapper that lifts → $on_call → forwards → $on_return
```

Schema layouts (`field`, `field-tree`, `cell`, side-table records)
are derived once via `wit_parser::SizeAlign` in `compute_schema`
and threaded as `SchemaLayouts`. Field offsets are looked up by
name at use sites — no hardcoded offsets in adapter codegen.

---

## Library leverage

We consume `wit-parser` / `wit-bindgen-core` as **leaf operations**:
`SizeAlign` for sizes / aligns / field offsets, `Resolve::push_flat`
for canonical-ABI flat-slot counts, `wit_parser::abi::join` for
variant payload joining, and `wit_bindgen_core::abi::lift_from_memory`
to walk a WIT type from a memory address and push flat values onto
the stack (used for retptr-loaded compound results and async
`task.return` flat loads). `lower_to_memory` is the inverse; tier-3
shipped via a different route (Rust wrappers via wit-bindgen) and does
not extend the field-tree lift with a lower-back path.

We do **not** use `wit_bindgen_core::abi::call(...)` as a wrapper
driver. It's built around a typed recursive `Value` representation
that WIT doesn't support and that the cell format was specifically
designed to avoid. The hand-rolled `LiftPlan` is the right fit.

---

## The LiftPlan: a flat IR

Classification produces a `LiftPlan` per (function, param) and per
(function, result): a flat `Vec<Cell>` describing every cell the
lift writes, in allocation order. The cell variants live in the
code — read them there. This section is about why the IR is shaped
the way it is.

### Three invariants

1. **Single source of truth for cell count.** `plan.cells.len()`
   is what we allocate slab space for. A field-tree with
   `cells.len = 5` requires 5 cells, full stop.
2. **Cell indices are vector positions.** When a parent cell
   references a child by index, that index is literally the
   position the child got pushed to during plan-building. The
   same vector that codegen iterates over at emit time is the
   one side-table entries reference — they can't disagree
   because they read the same vector.
3. **Flat-slot count is the builder's final cursor value.** As
   `LiftPlanBuilder` pushes cells, it bumps a counter for each
   flat slot a cell consumes; when the plan is finalized that
   counter becomes `plan.flat_slot_count`. There's no separate
   "how many slots does each `Cell` variant take?" table that has
   to stay in sync with the builder's actual allocations — the
   count and the allocations come from the same counter, so they
   can't disagree.

### Children-first allocation

`LiftPlanBuilder::push` allocates children first: each sub-call
appends its own cells and returns the index where its root landed;
the caller pushes the parent referencing those already-known
indices. Parents are appended fully constructed — no mutation
after push, no back-fill.

A consequence: **the root cell lands at the last index in `cells`,
not the first** (leaves go in first; the root is constructed and
pushed last). `LiftPlan::root` records where the root actually
landed, and the wire format's `field-tree.root` field carries the
same number — so consumers follow `tree.root` and land on the
root wherever it sits, rather than assuming it's at `cells[0]`.

### Why cells store offsets, not absolute locals

Each WIT param gets its own `LiftPlan`, and the result has its own
plan too — different types produce different plans. But every plan
is **built at classify time**, before the layout phase has decided
where in the wasm function's locals each param's flat slice will
sit. And once built, each plan is **read by multiple consumers**:
the emitter (which writes the lift bytecode) and every side-table
builder (which collects per-kind metadata like type-names and
child-cell indices).

The locals layout matters here because one WIT param can consume
many wasm flat slots: `u32` is 1; `string` is 2 — `(ptr, len)`;
`record { a: u32, b: string }` is 3; and so on. A function's wasm
locals end up as a sequence of variable-width per-param slices.
The result, when compound, lives in its own slice of synth locals.

Since plans are built before any slice positions are known, cells
store flat-slot positions as **offsets** in `0..flat_slot_count`,
not absolute wasm-local indices. At emit time the caller computes
a **`local_base`** — the absolute wasm-local index where this
plan's flat-slot slice starts — and passes it in. The per-cell
formula is then just `local_base + offset(N)`; the cumulative-sum
work happens once, when `local_base` is computed for this emit
call, not per cell.

Where `local_base` comes from per destination:

- **Param emit**: a cumulative cursor over preceding params'
  flat-slot counts. Param 0's `local_base` is 0; param 1's is
  `params[0].flat_slot_count`; param 2's is
  `params[0].flat_slot_count + params[1].flat_slot_count`; etc.
- **Compound-result emit** — when the result is itself a compound
  type (record, tuple, variant) whose lift plan has multiple cells
  spanning multiple flat slots, the handler returns it via retptr
  and the wrapper allocates `flat_slot_count` *contiguous* synth
  locals to receive `lift_from_memory`'s flat output. `local_base`
  is the wasm-local index of the **first** of those synth locals;
  because the slice is contiguous, a cell with flat-slot offset K
  lands at absolute local `local_base + K`. The contiguity is
  enforced at the alloc site, so this calculation is the only
  thing the per-cell emit code needs.
- **Side-table builders**: no `local_base` — they only read
  structural fields like case names and child indices.

---

## Side-table storage: static vs per-call

Nominal cells (`enum-case`, `record-of`, `flags-set`, `variant-case`,
`*-handle`, …) carry a `u32` *side-table index*. The metadata
(type-names, case-names, child cell indices, etc.) lives in per-kind
side tables on the field-tree.

**Scope note.** This section is *only* about where side-table
entries live and how they get filled. The cells slab itself
handles variable-length lists with a separate mechanism (already
introduced in "What this codegen does"): each parent `cell::list-of`
holds a `(ptr, len)` that points at a per-call `cabi_realloc`'d
sub-slab of element cells. So when text below says a side-table
is in a "static segment" for `list<u32>`, that doesn't mean the
list itself is static — the list's elements live in their own
per-call sub-slab. It just means the `*-infos` lists for the five
nominal kinds don't need a per-call buffer.

### Vocabulary

There are five **nominal kinds** that have side-table entries:
`record`, `flags`, `enum`, `variant`, and `handle` (covering
`own<R>` / `borrow<R>` / `stream<T>` / `future<T>` /
`error-context`). The storage policy is chosen independently per
kind; to talk about one at a time, call it **K**.

Two timelines bracket every value in a side-table entry. **Build
time** is when splicer generates the adapter wasm and decides what
bytes go into the binary — values known here are **static** and
can be baked directly into the binary (data-segment bytes or
`i32.const`s in the wrapper body). **Execution time** is when the
generated adapter actually runs a call — values only known here
are **dynamic** and have to be computed by, or read from, the
running wasm. Each side-table entry mixes both: static fields
straight from the WIT (a `record-info`'s `type-name = "point"`,
a variant's case-name list), and dynamic fields that only land at
execution time (a `handle-info`'s `id: u64`, a `variant-info`'s
actual case-name selected by the disc, a `flags-info`'s `set-flags`
list).

The decisions are scoped to the **target interface** the adapter
wraps. Each adapter wraps one target interface, which may contain
one function or many — `wasi:http/handler` has just `handle`,
while `my:service/math` has `add`, `sub`, `mul`, `div`. Decision 1
runs per kind K across the whole interface; Decision 2 runs per
individual interface function.

### The axis: does the K-entry count scale with a list length?

A list's *length* is always dynamic — intrinsic to `list<T>`. But
that doesn't mean every list affects side-table sizing: only lists
whose elements contain a K value do.

- `list<u32>`, `list<string>`, `list<tuple<u32, u32>>` — elements
  have no nominal kinds, so they produce **zero** K-entries
  regardless of length. They don't affect any kind's policy.
- `list<some-flags-type>` (K = `flags`), `list<record { perms: flags }>`
  (K = `flags`, nested in a record) — produce one K-entry per
  element. The K-entry count grows with the list length.

Everything else (records, variants, handles in non-list positions)
produces a build-time-known number of K-entries. So the question
splicer answers for each K is: **does any list in the interface
produce K-entries?** Both policies handle mixed static + dynamic
fields the same way — that's not the axis. The axis is whether
the *number of K-entries* is fixed at build time or varies per
call.

### Decision 1: per kind, scoped to the interface

The first decision is **per kind K, decided once across the
entire target interface**. Two outcomes:

- **Count is always static → static segment.** No function in the
  interface has a list whose elements contain a K value. The
  K-entry count for every (fn, param | result) is build-time-
  known. Splicer pre-allocates a wasm data segment per (fn, param
  | result), writes static fields into the segment bytes directly,
  and reserves slots for the dynamic fields the wrapper patches
  per call. The field-tree's `K-infos` slice is baked with both
  `(ptr, len)`.
- **Count could be dynamic → per-call buffer.** Some function in
  the interface has a list whose elements contain a K value,
  making that function's K-count len-dependent. A data segment
  can't be sized for it, so splicer moves **all** of the
  interface's functions to the per-call buffer policy for K —
  every function calls `cabi_realloc` per call to allocate the buffer,
  then writes every field of every entry (static fields from
  `i32.const`s emitted into the wrapper body, dynamic fields from
  wasm locals).

The "all functions" promotion is the uniform-code-shape tradeoff:
splicer would rather pay the per-call cost uniformly across the
interface than maintain two code paths for the same kind within
one adapter. A function that doesn't itself have a list-with-K
can still end up under the per-call buffer policy because some
sibling function in the same interface forced it.

### Decision 2: per interface function, only under the per-call buffer policy

If Decision 1 picked per-call buffers for K, a second decision fires **per
interface function** — but it's a narrow optimization, not a
partial escape back to static-segment territory. Every function
in the interface still calls `cabi_realloc` per call, still
allocates a fresh buffer, and still writes every field (including
the static ones, from `i32.const`s) into that buffer per call —
that's all fixed by Decision 1. What Decision 2 controls is just
the buffer's **size argument**: when a particular function has
no list-with-K of its own, the size is build-time-known, and
splicer can skip the size-accumulation pre-pass that walks lists
and computes `static_count + Σ_lists len * K_entries_per_elem`.
Decision 2 saves *only* that pre-pass — not the allocation, not
the static-field writes.

Which of the two per-call sub-cases applies depends on the
specific interface function's own use of `list<...>`:

- **Interface function's K-count is static → buffer size fixed at
  build time.** This function has no list-element occurrences of
  K. Buffer size is `static_count * sizeof(K-entry)`;
  `K-infos.len` is baked, only `.ptr` is patched per call.
- **Interface function's K-count is dynamic → buffer size
  computed per call.** At least one list element in this
  function's plan contains a K value. The pre-pass accumulates
  `static_count + Σ_lists len * K_entries_per_elem` into the
  `cabi_realloc` size; both `.ptr` and `.len` are patched per
  call. Per-list base is captured before bumping; per-iteration
  `list_elem_K_base = base + j * K_entries_per_elem` resolves the
  absolute slot for each element-plan cell.

The three outcomes side by side (the "size-accumulation
pre-pass" in the last row is the walk-lists-and-sum step that
computes `static_count + Σ_lists len * K_entries_per_elem` to
feed `cabi_realloc`):

|                                       | Static segment (Decision 1) | Per-call, static count (D1 + D2) | Per-call, dynamic count (D1 + D2) |
|---------------------------------------|-----------------------------|----------------------------------|-----------------------------------|
| `cabi_realloc` per call               | no                          | yes (const size)                 | yes (computed size)               |
| Static fields baked in data segment   | yes                         | no — written per call            | no — written per call             |
| Dynamic fields patched per call       | yes (pre-reserved slots)    | yes (fresh buffer)               | yes (fresh buffer)                |
| Size-accumulation pre-pass per call   | n/a                         | no (Decision 2 skips)            | yes                               |

**Worked example.** A target interface with two functions:
`foo(p: my-flags)` (`flags` once, no list) and
`bar(ps: list<my-flags>)` (`flags` × `len(ps)`).

- **Decision 1**: `bar` exists in the interface, so `flags` is
  promoted to per-call for the whole interface. Both `foo` and
  `bar` now use `cabi_realloc`'d buffers for flags-info.
- **Decision 2**: `foo`'s buffer is `1 * sizeof(flags-info)`,
  fixed at build time (foo has no list-with-flags itself).
  `bar`'s buffer is `1 * sizeof(flags-info) * len(ps)`,
  accumulated at execution time from the list length.

`foo` uses per-call buffers *because of `bar`*; Decision 2
just lets `foo` skip the size-accumulation pre-pass. Note that
`foo` still pays the `cabi_realloc` cost and still writes its
flags-info entry's `type-name` from an `i32.const` per call —
neither of which would happen if `flags` had stayed in the
static-segment policy (where the type-name would already be
baked into the data segment and no allocation would be needed).

### Cost

The per-kind promotion to per-call buffers costs ~7 wasm
instructions + one extra `cabi_realloc` per (fn, param | result)
of kind K, plus per-cell write overhead from fields that used to
be statically baked. Paid uniformly even when an interface
function's K-count is static. The tradeoff favors uniform code
shape over the few instructions saved by a hybrid path.
