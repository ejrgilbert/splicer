# Adapter generation — architecture

Low-level map of the code that produces splicer's adapter components.
Companion doc: [`adapter-components.md`](./adapter-components.md) is the
user-facing explainer; this file is for contributors working on the
generators themselves. Tier-by-tier user docs live in
[`docs/tiers/`](./tiers/).

## Mission

Given

- a target interface (fully-qualified, e.g. `wasi:http/handler@0.3.0`),
- a split `.wasm` whose embedded WIT defines that interface,
- the set of `splicer:tier{1,2}/*` interfaces the middleware exports,

emit a WebAssembly Component binary that:

- Re-exports the target interface unchanged (drop-in replacement for
  the upstream caller).
- Imports the target interface from a handler-providing component.
- Imports the middleware's hooks (`before` / `after` / `blocking` for
  tier-1; `before` / `after` for tier-2).
- For each function in the target interface, wraps it with the hooks'
  before/after/blocking phases, handling the canonical-ABI lift/lower
  and async machinery transparently.

There are two generators with the same outer shape but different
dispatch-module bodies — see [Tier-1 vs Tier-2](#tier-1-vs-tier-2).

## Design thesis

**Splicer does not implement the Component Model's canonical ABI.** It
consumes one. The adapter generator's job is to:

1. Know what *shape* the adapter should have (which hooks fire, which
   handler gets called, how the phases sequence).
2. Emit the wasm to *drive* a canonical-ABI implementation someone else
   owns.

The canonical-ABI authority lives in two upstream crates:

- [`wit-parser`] — type model, `SizeAlign` for canonical-ABI layout
  (size / align / field offsets / variant payload offsets),
  `Resolve::wasm_signature` / `Resolve::push_flat` for flattening,
  `Resolve::wasm_import_name` / `wasm_export_name` for canonical mangling.
- [`wit-bindgen-core::abi`] — instruction-level codegen via the
  `Bindgen` trait. Walks a type, emits an abstract instruction stream
  (`I32Load { offset }`, `VariantLift { … }`, `RecordLift { … }`,
  `FixedLengthListLiftFromMemory { … }`, etc.).

Splicer implements `Bindgen` against `wasm_encoder::Instruction`. Every
canonical-ABI decision — walk order, offsets, discriminant widths,
joined flat shapes, widening rules — comes from upstream. Splicer's
implementation is a transcriber: abstract `Instruction` → concrete wasm
opcode.

The outer Component is *also* not splicer's job: both generators hand
a single core module to `wit_component::ComponentEncoder`, which
synthesizes the surrounding component from the metadata embedded into
the module. Splicer owns the inner core module and the WIT world that
declares its imports/exports — nothing else.

[`wit-parser`]: https://docs.rs/wit-parser
[`wit-bindgen-core::abi`]: https://docs.rs/wit-bindgen-core

## Module layout

```
src/adapter/
├── abi/                  — canonical-ABI infrastructure (cross-tier)
│   ├── bindgen.rs        — WasmEncoderBindgen (Bindgen impl)
│   ├── canon_async.rs    — wit-component async intrinsics + wait-loop emit
│   ├── compat.rs         — verbatim cast / flat_types (pending PR #1597)
│   └── emit.rs           — wasm-encoder emit helpers shared across tiers
│                           (memory/globals, cabi_realloc, hook-import lookup,
│                            call-id record helpers, borrow-drop emit)
├── tier1/
│   ├── emit.rs           — build_adapter: WIT push + dispatch core module
│   └── tests.rs          — adapter-shape integration tests
├── tier2/
│   ├── mod.rs            — build_tier2_adapter entry
│   ├── blob.rs           — typed data-segment packing helpers
│   ├── cells.rs          — emit one canonical-ABI `cell` variant case
│   ├── layout.rs         — static-memory layout phase
│   ├── lift/             — type classification + lift codegen
│   │   ├── plan.rs       — LiftPlan / Cell allocator
│   │   ├── classify.rs   — per-(param|result) lift recipes
│   │   ├── sidetable/    — per-field-tree side tables (enum/record/flags/...)
│   │   └── emit.rs       — wasm emit for one cell per lifted value
│   ├── schema.rs         — splicer:common/types layouts + hook-import lookup
│   ├── section_emit.rs   — type / import / export / code / data section emit
│   └── wrapper_body.rs   — per-wrapper body generation
├── indices.rs            — DispatchIndices / LocalsBuilder
├── mem_layout.rs         — MemoryLayoutBuilder (static-memory allocator)
├── resolve.rs            — split decode + target-interface lookup
└── mod.rs                — generate_tier{1,2}_adapter entry points
```

### Layer responsibilities

**`abi/` — spec-consuming.** Encodes knowledge *of* the canonical ABI
by importing `wit-parser` and `wit-bindgen-core`. Touches `wasm-encoder`
only for individual `Instruction` opcodes (inside `Bindgen`) plus the
shared section-builders in `abi/emit.rs`. Nothing in `abi/` knows
about "the adapter's shape" — it's generic infrastructure both tiers
consume.

**`tier1/` and `tier2/` — wasm-emitting.** Each owns the shape of its
dispatch core module: which hooks fire, how the phases sequence, what
gets written to scratch memory, how results are returned. They share
`abi/emit.rs` for the standard-export / hook-import / cabi_realloc
plumbing every dispatch module needs regardless of tier.

**Cross-cutting (root).** `resolve.rs` decodes the split's embedded
WIT and looks up the target interface by name. `indices.rs` holds two
running-index allocators — `DispatchIndices` (per dispatch module's
type / function namespaces) and `LocalsBuilder` (per emitted wasm
function's locals). `mem_layout.rs` is a single-cursor allocator that
hands out byte offsets for the dispatch module's static memory (name
blobs, retptr scratch, hook event slots, call-id buffer, …) in
declaration order; the cursor's final value becomes the bump-allocator
start.

## Pipeline: split bytes → emitted wasm

Both tiers follow the same outer shape:

```
split bytes
   │
   │ resolve::decode_input_resolve
   ▼
wit_parser::Resolve  (Resolve owns the target interface's types,
   │                  imported from the split's component-type custom section)
   │
   │ Resolve::push_str("splicer-common.wit", …)
   │ Resolve::push_str("splicer-tier{1,2}.wit", …)
   │ Resolve::push_str("splicer-adapter.wit", synthesized world WIT)
   │ Resolve::select_world(…) → WorldId
   ▼
WorldId  (the adapter's world: re-exports the target interface,
   │      imports the handler interface and the active hooks)
   │
   │ build_dispatch_module — the tier-specific part
   ▼
core wasm Module bytes (raw .wasm)
   │
   │ wit_component::embed_component_metadata
   │ wit_component::ComponentEncoder::default()
   │     .module(&core_module).encode()
   ▼
adapter Component bytes
```

The synthesized adapter-world WIT is what makes the generator
"specification-driven": every import/export name in the dispatch core
module comes from `Resolve::wasm_import_name` / `wasm_export_name`
queries against this world, not from string concatenation. A WIT-level
mangling change in upstream silently propagates to the emitted module.

The two tiers diverge at `build_dispatch_module`:

- **tier-1** (`tier1/emit.rs::build_dispatch_module`) — single-pass
  emitter. Per-function loop produces a `FuncDispatch` carrying sigs
  + name offsets + retptr offsets, then sections are emitted in fixed
  order (types → imports → memory/globals → exports → code → data).
- **tier-2** (`tier2/mod.rs::build_tier2_adapter` →
  `build_dispatch_module`) — preflight (`schema::compute_schema`
  computes the canonical-ABI layouts of `splicer:common/types`
  typedefs and resolves the hook imports), then three explicit
  phases: classify (`build_per_func_classified` produces a
  `FuncClassified` per function — sigs, mangled names, lift recipes
  for each param/result, no static-memory offsets yet), lay out
  (`layout::lay_out_static_memory` reserves data + scratch slabs,
  pre-builds blobs that embed cross-blob pointers, and consumes the
  classify list to produce an immutable `FuncDispatch` list), emit
  (`section_emit` writes the wasm sections;
  `wrapper_body::emit_wrapper_function` writes each wrapper body).
  The classify→layout type-state hinge guarantees no offset is
  back-filled into a placeholder after the fact.

## Tier-1 vs tier-2

The user-facing distinction is in [`docs/tiers/`](./tiers/). Inside
the generators it shows up as: **what gets written to the dispatch
module's scratch memory**.

**Tier-1 dispatch shape — passthrough wrapper.** The wrapper export's
flat sig and the handler import's flat sig come from the same
`Resolve::wasm_signature` call against the same function. The body
just `local.get`s every wrapper param and `call`s the handler — no
lift, no lower, no copy through memory for the *payload*. The static
memory holds only:

- the iface name + each function's name (pointers handed to hooks),
- a shared call-id record buffer (lowered before each hook call),
- the bump-allocator save slot,
- a retptr scratch allocation per function whose result needs one,
- the canon-async event slot (`waitable-set.wait` writes here),
- the `should-block` retptr slot (a single i32 bool).

The wrapper body sequences five phases — bump-save → before → blocking
→ handler call → after → borrow-drops + bump-restore → return. See
`tier1/emit.rs::emit_wrapper_body`. Async-stackful adds a
`task.return` tail and the wait-loop scratch (subtask + waitable-set
handles). Hooks observe call-id metadata only — they never see param
or result values.

**Tier-2 dispatch shape — lifting wrapper.** Each param and result is
lifted into the `field-tree` representation defined in
[`wit/common/world.wit`](../wit/common/world.wit) — a flat array of
`cell`s with side tables for nominal-typed cases (record / flags /
enum / variant / handle infos) plus a `root` index. Hooks see
`on-call(call-id, args: list<field>)` and
`on-return(call-id, result: option<field-tree>)`; one `field` per
target-fn param, one `field-tree` for the result.

That representation lives across two regions of the dispatch
module's static memory:

- **Data segments (built at adapter-build time, never written to at
  runtime):** the name interner blob (interface name + every fn name +
  every record / enum / flags / variant case name + every record-field
  name); the per-fn `list<field>` blob (one `field` record per param,
  each holding a name pointer + a pre-wired `field-tree` whose
  `cells` slice points at the param's cells slab and whose
  `*-infos` lists point at the per-tree side-table blobs); the
  per-tree side-table blobs themselves (`enum-infos`, `record-infos`,
  `flags-infos`, `variant-infos`, `handle-infos`); the on-return
  params blob (one record per fn, with `iface` + `fn` names and the
  result `field-tree`'s pointers all pre-baked).
- **Scratch (reserved for runtime writes):** the per-param cells
  slabs (one cell per `LiftPlan::Cell`, written each call by the
  lift codegen); the per-fn result cells slab; per-`Cell::Flags`
  scratch (the bit-walk emits `set-flags.ptr` here); per-`Cell::Char`
  scratch (utf-8 encoder output); the on-call indirect-params
  buffer (`record { call: call-id, args: list<field> }` — the wrapper
  patches `call.id` and the args slice ptr/len each call, leaving
  the field records themselves untouched); the per-fn retptr scratch
  (when the canonical ABI uses caller-allocates); the canon-async
  event slot; the bump-allocator reset slot.

The on-return params blob is the same shape — every part except
`call.id` is pre-baked, so the wrapper just patches the i64 id field
before calling the after hook.

The wrapper body shape is four-phase
(`tier2/wrapper_body.rs::emit_wrapper_function`):

1. **on-call** (only if before hook wired) — for each param, walk
   its `LiftPlan` and emit one cell into the param's cells slab;
   then patch the on-call indirect-params buffer's `call-id` + args
   slice; call the before hook; await.
2. **handler call** — pure passthrough, same shape as tier-1
   (`emit_handler_call` is shared infrastructure).
3. **on-return** (only if after hook wired) — lift the result into
   the per-fn result cells slab (Direct: one cell from the flat
   return value; Compound: load multi-slot retptr through the
   bindgen-built `lift_from_memory` sequence into synthetic locals,
   then walk the result `LiftPlan`); patch `call.id` in the
   on-return params blob; call the after hook; await.
4. **tail** — sync funcs return the direct value or static retptr
   (same as tier-1); async funcs run `task.return` (void / retptr
   passthrough / flat-load via `lift_from_memory`, same three
   shapes tier-1 has).

Lifting is the unique tier-2 work; the rest is the same plumbing
tier-1 has, accessed through `abi/emit.rs`.

## Where the Bindgen actually fires

`WasmEncoderBindgen` is splicer's `wit_bindgen_core::abi::Bindgen`
implementation. It's invoked in three call-sites today, all of them
running `lift_from_memory(resolve, &mut bindgen, (), &ty)` to load a
canonical-ABI value out of linear memory and onto the wasm value stack
in joined-flat form:

| Call site | Why |
|---|---|
| `tier1/emit.rs::emit_async_wrapper_body` | Async function with a flat-fitting result: handler returns via retptr, but `task.return` for that result takes flat values. We lift from the retptr scratch to satisfy the ABI mismatch. |
| `tier2/lift/emit.rs` (compound result lift) | The result type's flat representation has multiple slots; lifted into per-slot synth locals so the result-cells can read each slot independently. |
| `tier2/lift/emit.rs` (async task.return tail) | Same `task.return` flat-load as tier-1's async path, but happens after the on-return hook has observed the lifted result. |

In every case the caller does the same dance:

1. Allocate an i32 local via `LocalsBuilder` to hold the source
   buffer's base address.
2. Stash the source pointer with `i32.const X; local.set $addr_local`.
3. Construct `WasmEncoderBindgen::new(sizes, addr_local, &mut locals)`
   so any scratch locals the bindgen needs (variant disc, fixed-list
   iter address, payload widening stash) land in the same function-local
   index space.
4. Call `lift_from_memory(resolve, &mut bindgen, (), &ty)`.
5. `bindgen.into_instructions()` — a `Vec<wasm_encoder::Instruction>`
   that, when flushed into the function body, leaves the joined flat
   representation of the value on the wasm stack.

All canonical-ABI heavy lifting — walking the type, picking load
widths, computing offsets, dispatching variant arms, widening arm
flats to the joined flat, unrolling fixed-size-list iteration —
happens inside upstream's `read_from_memory` via our `Bindgen::emit`.

## `WasmEncoderBindgen` — design notes

The full treatment is in `src/adapter/abi/bindgen.rs`'s module header.
Key invariants:

- **`Operand = ()`**. The wasm value stack is the source of truth.
  The generator's internal operand stack tracks *counts*, not
  identities. Splicer's emit arms pop/push placeholders to match each
  `Instruction` variant's declared arity.

- **Address handling by local.** The base address lives in
  `addr_local` (or a per-iteration `iter_addr_local` for fixed-size
  lists); every load emit funnels through `emit_load`, which emits
  `local.get $addr; <load> offset=N`. The generator's abstract
  address operand can be cloned freely because we never pop a wasm
  value for it — each load re-reads from the local.

- **Block-capture IR.** `push_block` / `finish_block` redirect emits
  into an `ActiveBlock` buffer; `finish_block` stashes the buffer in
  `completed_blocks` for the variant / fixed-size-list lift to
  consume. Variant emits splice captured arm bodies inside a
  `block ... br_table ... end` structure; fixed-size-list emits
  replay the single element-read body N times with the address local
  advanced by `elem_size` each iteration.

- **Local allocation is shared with the outer function.** The
  `Bindgen` borrows `&mut LocalsBuilder` from the caller, so every
  local it allocates lands in the *same* contiguous local-index space
  as the dispatch module's own locals (subtask / waitable-set / call-id
  / synth result slots). The caller calls `locals.freeze()` once when
  constructing the `Function`.

## Heterogeneous variants and joined flat

Variant / option / result arms can have different flat shapes. E.g.:

- `result<u8, u64>` — ok arm flats to `[i32]`, err arm flats to
  `[i64]`. Joined payload: `[i64]`. Ok arm's load must be widened via
  `i64.extend_i32_u`.

- `result<string, u64>` — ok flats to `[Pointer, Length]`, err flats
  to `[I64]`. Joined payload: `[PointerOrI64, Length]`. Ok arm's
  Pointer at position 0 is i32 at the wasm level; PointerOrI64 is
  i64. Widening: `i64.extend_i32_u`.

The widening table lives in `abi/compat.rs::cast` (verbatim copy of
`wit-bindgen-core`'s private `fn cast`). The `Bindgen`'s
`emit_bitcast` maps each `Bitcast` variant to its wasm opcode. **Key
subtlety on wasm32**: `Pointer` and `Length` collapse to `i32` but
`PointerOrI64` collapses to `i64`, so the four cross-boundary casts
(`PToP64`, `LToI64`, `P64ToP`, `I64ToL`) need `i64.extend_i32_u` /
`i32.wrap_i64` — not no-ops. Tested in `abi/bindgen.rs`'s unit tests.

## What the dispatch modules still own

The `abi/` layer is generic. Each tier's emit module owns its
*shape* — the part nobody else generates:

**Tier-1.**
- Per-function wrapper body sequencing five phases: before,
  blocking, handler call, after, return.
- Async wait-loop emission (`waitable-set.wait` blocks) for hook
  subtasks and async handler calls.
- `task.return` wiring per async function: flat (lift-from-retptr +
  call), retptr-passthrough (single buffer pointer), or void.
- Call-id record lowering into a shared static buffer before each
  hook call.
- Borrow-drop emission so resource borrows don't outlive the wrapper.

**Tier-2.**
- All of the above, plus:
- `LiftPlan` construction (`tier2/lift/plan.rs`): WIT type → flat
  list of `Cell`s, allocation order with children-before-parents
  indexing; `root` records the parent's index.
- Schema-driven static-memory layout (`tier2/schema.rs` +
  `tier2/layout.rs`): canonical-ABI sizes/offsets of `field`,
  `field-tree`, and the on-call / on-return params records come
  from `wit-parser`; layout pre-builds the data segments that
  embed cross-blob pointers (fields buf → cells slab; field-tree
  → side-table blobs) so the wrapper body never has to assemble
  them at runtime.
- Cell-emit codegen (`tier2/cells.rs` + `tier2/lift/emit.rs`): one
  wasm sequence per `Cell` variant, writing into the cells slab
  at the offset layout reserved.
- Side-table population (`tier2/lift/sidetable/`): precomputed
  per-tree blobs (enum / record / flags / variant / handle infos)
  that hooks read for nominal info; flags + handle entries get
  per-call runtime fills patched in at lift time.
- Result lifting: Direct (single flat slot consumed from the
  handler's flat return) vs Compound (multi-slot loaded out of
  retptr scratch via `lift_from_memory` into synthetic locals,
  then walked by the result `LiftPlan`).

## `abi/compat.rs` — a temporary borrow

`wit-bindgen-core`'s private helpers `cast(WasmType, WasmType) ->
Bitcast` and `flat_types(&Resolve, &Type, Option<usize>) ->
Option<Vec<WasmType>>` aren't part of its public API. Splicer's
variant widening needs both. `abi/compat.rs` contains verbatim copies
of those two functions plus the `MAX_FLAT_PARAMS` constant.

Visibility-flip PR filed upstream:
<https://github.com/bytecodealliance/wit-bindgen/pull/1597>. When it
merges, delete `abi/compat.rs` and change the few callers to import
`wit_bindgen_core::abi::{cast, flat_types}` directly. The functions
reference only already-public types (`WasmType`, `Bitcast`, `Resolve`,
`Type`), so the flip is semantically trivial.

## Index spaces

| Struct | Namespace | Scope |
|---|---|---|
| `DispatchIndices` | dispatch core module's type + function indices | one per dispatch module |
| `LocalsBuilder` | wasm function locals | one per emitted wasm function |

`LocalsBuilder` is the cross-cutting one: both the wrapper-body
emitter (for subtask / waitable-set / call-id locals) and the
`Bindgen` (for iter address, disc, payload stash locals) allocate
into the same instance. The caller constructs it, pre-allocates
anything it knows about, then threads `&mut LocalsBuilder` into
`Bindgen::new`. When the bindgen is done, the caller calls
`locals.freeze()` and feeds the result into
`Function::new_with_locals_types`.

The outer Component's index spaces (component types / instances /
canon lifts / canon lowers) are owned by `wit-component`, not splicer
— another consequence of the "one core module → ComponentEncoder"
shape. There is no `ComponentIndices` struct in the current code.

## How canonical-ABI evolution affects the code

Three failure modes, in order of frequency:

1. **New `TypeDefKind` upstream.** Most type-walking goes through
   `wit-parser` / `wit-bindgen-core` directly, so new `TypeDefKind`s
   are absorbed transparently. The risk surface is tier-2's classify
   pass (`tier2/lift/plan.rs`) and the cell-emit table — both have
   non-exhaustive matches over `TypeDefKind` and bail with a clear
   error on unsupported kinds. Adding support = one new arm + a
   `Cell` variant if the shape demands one.

2. **New `Instruction` variant in `wit-bindgen-core::abi`.**
   `WasmEncoderBindgen::emit`'s match over `AbiInst` is NOT
   exhaustive — the fallback is `unimplemented!()`. When upstream
   adds a new instruction (say, a new async bookkeeping op), we
   won't notice at compile time, but the first run that exercises it
   panics with a clear message. Add a new emit arm.

3. **Bitcast table expansion.** `abi/compat.rs::cast` has a
   non-exhaustive match (it ends with `unreachable!()` for
   bitcast pairs the canonical ABI doesn't allow). If upstream adds a
   new `WasmType` variant or changes the allowed join pairs, we'd
   need to update the copied table. This is one of the reasons to
   prefer upstream's version once `cast` is made public.

None of these are silent.

## Testing

Three layers of test coverage:

- **Unit tests** alongside the code they exercise (`abi/bindgen.rs`,
  `tier2/lift/tests.rs`, the per-section emit modules). Emit-level
  assertions: "loading a u32 emits one `i32.load`", "heterogeneous
  variant emits one `i64.extend_i32_u`", "this `Cell` variant emits
  this exact byte sequence". Catch bitcast / widening / cell-encoding
  regressions at `cargo test` time.

- **Adapter-shape integration tests** in `src/adapter/tier1/tests.rs`,
  `tier1/tests/fuzz.rs`, and `tier2/*/tests.rs`. Run the full
  generator for various interface shapes, then validate the emitted
  binary with `wasmparser`. Catches structural bugs but not runtime
  behavior.

- **End-to-end composition** in `tests/component-interposition/`. Run
  `./run.sh __testme` to build every configuration (single middleware
  / chain / fan-in / nested / …), compose with real handler
  components, and execute the result through a wasmtime runner. The
  gold standard for "does the adapter actually work?" — runtime
  bugs (unaligned retptrs, missing borrow-drops, cell-layout drift)
  surface here even when the unit + binary-validation layers pass.

Any non-trivial change should clear all three.

## References

- [`CanonicalABI.md`](https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md)
  — the spec.
- [`definitions.py`](https://github.com/WebAssembly/component-model/blob/main/design/mvp/canonical-abi/definitions.py)
  — precise reference semantics.
- [`docs/tiers/lift-codegen.md`](./tiers/lift-codegen.md) — tier-2
  lift design (data flow, plan invariants, why the plan exists).
- [`docs/tiers/tier-1.md`](./tiers/tier-1.md),
  [`tier-2.md`](./tiers/tier-2.md), …, [`tier-4.md`](./tiers/tier-4.md)
  — per-tier user-facing semantics.
