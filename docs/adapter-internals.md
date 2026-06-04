# Adapter generation — architecture

Design and rationale for the code that produces splicer's adapter
components. Companion doc:
[`adapter-components.md`](./adapter-components.md) is the user-facing
explainer; this file is for contributors working on the generators
themselves. Tier-by-tier user docs live in
[`docs/tiers/`](./tiers/), and tier-2 lift internals live in
[`docs/tiers/lift-codegen.md`](./tiers/lift-codegen.md).

The implementation lives under `src/adapter/`. Module names, file
paths, and per-kind dispatch tables intentionally aren't enumerated
here — they rot every refactor and are faster to read at the source.

## Mission

Given

- a target interface (fully-qualified, e.g. `wasi:http/handler@0.3.0`),
- a split `.wasm` whose embedded WIT defines that interface,
- the set of `splicer:tier{1,2}/*` interfaces the middleware exports,

emit a WebAssembly Component binary that:

- Re-exports the target interface unchanged (drop-in replacement for
  the upstream caller).
- Imports the target interface from a handler-providing component.
- Imports the middleware's hooks (`before` / `after` / `gate` for
  tier-1 or tier-2).
- For each function in the target interface, wraps it with the
  hooks' before/after/gate phases, handling the canonical-ABI
  lift/lower and async machinery transparently.

There are two generators with the same outer shape but different
dispatch-module bodies (see [Tier-1 vs Tier-2](#tier-1-vs-tier-2)).

## Design thesis

**Splicer does not implement the Component Model's canonical ABI.
It consumes one.** The generator's job is to:

1. Know what *shape* the adapter should have (which hooks fire,
   which handler gets called, how the phases sequence).
2. Emit the wasm to *drive* a canonical-ABI implementation someone
   else owns.

The canonical-ABI authority lives in two upstream crates:

- [`wit-parser`] — type model, `SizeAlign` for canonical-ABI layout
  (size / align / field offsets / variant payload offsets),
  `Resolve::wasm_signature` / `Resolve::push_flat` for flattening,
  `Resolve::wasm_import_name` / `wasm_export_name` for canonical
  mangling.
- [`wit-bindgen-core::abi`] — instruction-level codegen via the
  `Bindgen` trait. Walks a type, emits an abstract instruction
  stream (`I32Load { offset }`, `VariantLift { … }`,
  `RecordLift { … }`, etc.).

Splicer implements `Bindgen` against `wasm_encoder::Instruction`.
Every canonical-ABI decision — walk order, offsets, discriminant
widths, joined flat shapes, widening rules — comes from upstream.
The implementation is a transcriber: abstract instruction → concrete
wasm opcode.

The outer Component is *also* not splicer's job: both generators
hand a single core module to `wit_component::ComponentEncoder`,
which synthesizes the surrounding component from the metadata
embedded into the module. Splicer owns the inner core module and
the WIT world that declares its imports/exports — nothing else.

[`wit-parser`]: https://docs.rs/wit-parser
[`wit-bindgen-core::abi`]: https://docs.rs/wit-bindgen-core

## Shared-nothing components and the canon-ABI trampoline

The component model is a **shared-nothing** model: each component
has its own isolated linear memory, and no component can
dereference a pointer that belongs to a different component. That
constraint is what motivates the canonical ABI in the first
place — components need a defined protocol for shipping typed
values across boundaries when they can't share memory addresses.

For each value crossing a component boundary, the canonical ABI
splits the work three ways:

- **Lower** writes a typed value into the *sender's* linear
  memory and produces a flat representation (a sequence of
  primitive wasm values; pointers reference the sender's memory).
- The **boundary trampoline** — provided by the host runtime
  (e.g., wasmtime), not by any component — reads the flat
  representation, copies any pointer-referenced bytes from the
  sender's memory into the *receiver's* memory via the receiver's
  `cabi_realloc`, and rewrites the pointers in the flat
  representation to reference the receiver's memory.
- **Lift** reads the typed value from the *receiver's* linear
  memory using the rewritten flat representation.

Lower and lift each operate within a single component's memory;
the cross-component copy is the trampoline's job. **Splicer never
authors a cross-component memory write** — the adapter's wasm
only ever touches its own linear memory. For a wrapper sitting
between caller and handler:

- When the caller invokes the wrapper export, the trampoline has
  already copied pointer-referenced args into the wrapper's
  memory; the wrapper sees flat args whose pointers reference its
  own memory.
- When the wrapper calls the handler import, the trampoline at
  *that* boundary copies pointer-referenced data from the
  wrapper's memory into the handler's memory; the handler
  receives flat args whose pointers reference its own memory.
- Symmetric copying happens for results.

That's why tier-1 can be a true "passthrough" wrapper at the wasm
level (the body just `local.get`s and `call`s) even though every
call involves real byte-copying between components — the copying
happens *outside* the wrapper's own wasm, in the trampoline.
Tier-2 additionally walks its own copy of the args via
`lift_from_memory` to populate the field-tree, but still never
touches any other component's memory.

## Layering

Two layers, with a clean responsibility split:

- **`abi/` — spec-consuming.** Encodes knowledge *of* the canonical
  ABI by importing `wit-parser` and `wit-bindgen-core`. Touches
  `wasm-encoder` only for individual `Instruction` opcodes inside
  the `Bindgen` impl, plus shared section / `cabi_realloc` /
  hook-import / call-id helpers. Nothing in `abi/` knows about
  "the adapter's shape" — it's generic infrastructure both tiers
  consume.

- **Per-tier modules — wasm-emitting.** Each owns the shape of its
  dispatch core module: which hooks fire, how the phases sequence,
  what gets written to scratch memory, how results are returned.
  They share `abi/` for the standard-export / hook-import /
  `cabi_realloc` plumbing every dispatch module needs.

Cross-cutting infrastructure (split decoding, target-interface
lookup, `DispatchIndices`, `LocalsBuilder`, `MemoryLayoutBuilder`)
sits at the root of `src/adapter/`.

## Pipeline: split bytes → emitted Component

Both tiers follow the same outer shape:

1. **Discover the target's WIT.** Decode the input split's
   `component-type` custom section into a `wit_parser::Resolve`.
   This is how splicer learns the target interface — its function
   list, type definitions, resource shapes, async-ness — without
   any out-of-band schema. Everything downstream (which functions
   to wrap, which types to lift, what to re-export, what canonical
   names to use for imports/exports) is driven by this WIT.
2. **Synthesize the adapter world.** Push the `splicer:common` /
   `splicer:tier*` WIT packages plus a generated adapter-world
   WIT into the `Resolve`, then select that world. The world
   declares the imports (target interface re-imported from a
   handler component, plus the middleware's hook interfaces) and
   the exports (the target interface, re-exposed to the caller)
   the dispatch core module must satisfy.
3. **Build dispatch module.** Emit the wasm core module that
   implements that world — the tier-specific part (see below).
4. **Encode Component.** Hand the core module to
   `wit_component::ComponentEncoder` via
   `embed_component_metadata` + `.module(...).encode()`; the
   library synthesizes the surrounding Component shape from
   metadata embedded in the module.

The synthesized adapter-world WIT is what makes the generator
**specification-driven**: every import/export name in the
dispatch core module comes from `Resolve::wasm_import_name` /
`wasm_export_name` queries against this world, not from string
concatenation. A WIT-level mangling change in upstream silently
propagates to the emitted module.

The two tiers diverge at the dispatch-module build:

- **Tier-1** is a single-pass emitter: per-function loop produces
  sigs + name offsets + retptr offsets, then sections are emitted
  in fixed order.
- **Tier-2** runs three explicit phases: **classify**
  (`FuncClassified` per function with lift recipes, no offsets
  yet), **layout** (reserve data + scratch + per-call buffers,
  pre-build the blobs that embed cross-blob pointers), **emit**
  (write the wasm sections and each wrapper body). The
  classify→layout type-state hinge guarantees no offset is
  back-filled into a placeholder.

## Tier-1 vs tier-2

The user-facing distinction is in [`docs/tiers/`](./tiers/). Inside
the generators it shows up as **what gets written to the dispatch
module's scratch memory**.

**Tier-1 dispatch shape — passthrough wrapper.** The wrapper export
and the handler import share the same flat sig (same
`Resolve::wasm_signature` call against the same function). The body
`local.get`s every param and `call`s the handler — no lift, no
lower, no copy through memory for the payload. Static memory holds
only call-identity bookkeeping (interface + function names, call-id
buffer), retptr scratch per function whose result needs one, and
hook plumbing (canon-async event slot, should-call retptr). Hooks
observe call-id metadata only — they never see param or result
values.

**Tier-2 dispatch shape — lifting wrapper.** Each param and result
is lifted into the `field-tree` representation defined in
[`wit/common/world.wit`](../wit/common/world.wit). Hooks see
`on-call(call-id, args: list<field>)` and
`on-return(call-id, result: option<field-tree>)`; the wrapper
still forwards canonical-ABI flat values through to the handler
unchanged (observation, not transformation). The data flow — cell
shapes, side-table storage policy, plan invariants, list-element
widening — is documented in [`docs/tiers/lift-codegen.md`](./tiers/lift-codegen.md).
The cross-cutting fact relevant to this doc is that tier-2's
compound-result lift goes through our `Bindgen` impl (see
[Where the Bindgen fires](#where-the-bindgen-fires)); the
`on-call` and handler phases produce flat-value traffic identical
to tier-1's.

## Where the Bindgen fires

`WasmEncoderBindgen` is splicer's `wit_bindgen_core::abi::Bindgen`
implementation. It's invoked everywhere we need to **lift a
canonical-ABI value out of linear memory onto the wasm value stack
in joined-flat form**:

- Tier-1 async wrappers whose flat-fitting result was returned via
  retptr — we lift from the retptr scratch to satisfy `task.return`'s
  flat-value contract.
- Tier-2 compound result lifts — load multi-slot retptr buffers into
  per-slot synth locals so the result-cells can read each slot
  independently.
- Tier-2 async `task.return` tail — the same flat-load as tier-1's
  async path, but happens after the on-return hook has observed the
  lifted result.

In every case the caller stashes the source pointer into an
`addr_local`, constructs a `WasmEncoderBindgen` with `&mut locals`
(so any scratch the bindgen needs lands in the wrapper's local index
space), calls `lift_from_memory(resolve, &mut bindgen, (), &ty)`,
and flushes the resulting `Vec<Instruction>` into the function body.
All canonical-ABI heavy lifting — walking the type, picking load
widths, computing offsets, dispatching variant arms, widening arm
flats to the joined flat, unrolling fixed-size-list iteration —
happens inside upstream's `read_from_memory` via our `Bindgen::emit`.

## `WasmEncoderBindgen` — invariants

The detailed treatment is in the module header. The invariants that
constrain how the impl is allowed to evolve:

- **`Operand = ()`.** The wasm value stack is the source of truth.
  The generator's internal operand stack tracks counts, not
  identities. Emit arms pop / push placeholders to match each
  `Instruction` variant's declared arity.
- **Address handling by local.** The base address lives in an
  `addr_local` (or a per-iteration `iter_addr_local` for fixed-size
  lists); every load funnels through `emit_load`, which emits
  `local.get $addr; <load> offset=N`. The generator's abstract
  address operand can be cloned freely because we never pop a wasm
  value for it — each load re-reads from the local.
- **Block-capture IR.** Block-pushing emits redirect into a buffer;
  block-finishing stashes the buffer for the variant /
  fixed-size-list lift to consume. Variant emits splice captured
  arm bodies inside a `block ... br_table ... end` structure;
  fixed-size-list emits replay the single element-read body N times
  with the address local advanced by `elem_size` each iteration.
- **Local allocation is shared with the outer function.** The
  `Bindgen` borrows `&mut LocalsBuilder` from the caller, so every
  local it allocates lands in the same contiguous local-index space
  as the dispatch module's own locals. The caller calls
  `locals.freeze()` once when constructing the `Function`.

## Heterogeneous variants and the joined flat representation

Variant / option / result arms can have different flat shapes:

- `result<u8, u64>` — ok arm flats to `[i32]`, err arm flats to
  `[i64]`. Joined payload: `[i64]`. Ok arm's load must be widened
  via `i64.extend_i32_u`.
- `result<string, u64>` — ok flats to `[Pointer, Length]`, err flats
  to `[I64]`. Joined: `[PointerOrI64, Length]`. Ok arm's Pointer at
  position 0 is i32 at the wasm level; PointerOrI64 is i64.
  Widening: `i64.extend_i32_u`.

The widening table lives in the `abi/` borrow from upstream (see
below). **Key subtlety on wasm32**: `Pointer` and `Length` collapse
to `i32` but `PointerOrI64` collapses to `i64`, so the four
cross-boundary casts (`PToP64`, `LToI64`, `P64ToP`, `I64ToL`) need
`i64.extend_i32_u` / `i32.wrap_i64` — not no-ops. Unit-tested
alongside the bindgen impl.

## `abi/compat.rs` — a temporary upstream borrow

`wit-bindgen-core`'s `cast` / `flat_types` helpers and the
`MAX_FLAT_PARAMS` constant aren't part of its public API. Splicer's
variant widening needs them, so they're copied verbatim into a
small `abi/compat.rs`. Visibility-flip PR tracked at
<https://github.com/bytecodealliance/wit-bindgen/pull/1597>; when it
merges, delete the file and import from upstream directly. Mark on
the calendar: every wit-bindgen-core upgrade should re-check that
the copies still match.

## Index spaces

Two running-index allocators thread through every emit:

- **`DispatchIndices`** — per dispatch module's type + function
  indices.
- **`LocalsBuilder`** — per emitted wasm function's locals.

`LocalsBuilder` is the cross-cutting one: both the wrapper-body
emitter and the `Bindgen` allocate into the *same* instance, so
their locals share an index space. The caller constructs it,
pre-allocates anything it knows about, threads `&mut LocalsBuilder`
into `Bindgen::new`, and after the bindgen finishes calls
`locals.freeze()` to feed `Function::new_with_locals_types`.

The outer Component's index spaces (component types / instances /
canon lifts / canon lowers) are owned by `wit-component`, not
splicer — a consequence of the "one core module → ComponentEncoder"
shape.

## How canonical-ABI evolution affects the code

Three failure modes, in order of frequency:

1. **New `TypeDefKind` upstream.** Most type-walking goes through
   `wit-parser` / `wit-bindgen-core` directly, so new kinds are
   absorbed transparently. The risk surface is tier-2's classify
   pass and the cell-emit dispatch — both have non-exhaustive
   matches over `TypeDefKind` and bail with a clear error on
   unsupported kinds. Adding support = one new arm + a `Cell`
   variant if the shape demands one.
2. **New `Instruction` variant in `wit-bindgen-core::abi`.** The
   `Bindgen` emit's match over `AbiInst` is **not** exhaustive —
   the fallback is `unimplemented!()`. Won't catch at compile
   time; the first run that exercises it panics with a clear
   message. Add a new arm.
3. **Bitcast table expansion.** The copy in `abi/compat.rs` has a
   non-exhaustive `cast` match ending in `unreachable!()` for
   bitcast pairs the canonical ABI doesn't allow. If upstream adds
   a new `WasmType` or changes the allowed join pairs, the copy
   must update. One reason to prefer upstream's version once
   public.

None of these are silent failures; all three trip on first
execution rather than emitting subtly-wrong wasm.

## Testing

Three layers, all expected to pass for any non-trivial change:

- **Unit tests** alongside the code they exercise. Emit-level
  assertions ("loading a u32 emits one `i32.load`", "heterogeneous
  variant emits one `i64.extend_i32_u`", "this `Cell` variant emits
  this exact byte sequence"). Catch bitcast / widening /
  cell-encoding regressions at `cargo test` time.
- **Adapter-shape integration tests** — run the full generator for
  various interface shapes, then validate the emitted binary with
  `wasmparser`. Catches structural bugs but not execution-time behavior.
- **End-to-end composition** in `tests/component-interposition/`.
  Run `./run.sh __testme` to build every configuration (single
  middleware / chain / fan-in / nested / …), compose with real
  handler components, and execute through a wasmtime runner. The
  gold standard for "does the adapter actually work?" —
  execution-time bugs (unaligned retptrs, missing borrow-drops,
  cell-layout drift) surface here even when the unit +
  binary-validation layers pass.

For tier-2 specifically, there's also a canned wasmtime sweep
(`cargo test --test fuzz_and_run test_tier2_canned -- --ignored`)
exercising one representative shape per `Cell` kind.

## References

- [`CanonicalABI.md`](https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md) — the spec.
- [`definitions.py`](https://github.com/WebAssembly/component-model/blob/main/design/mvp/canonical-abi/definitions.py) — precise reference semantics.
- [`docs/tiers/lift-codegen.md`](./tiers/lift-codegen.md) — tier-2 lift design (data flow, plan invariants, side-table storage policy).
- [`docs/tiers/tier-1.md`](./tiers/tier-1.md), [`tier-2.md`](./tiers/tier-2.md), [`tier-3.md`](./tiers/tier-3.md), [`tier-4.md`](./tiers/tier-4.md) — per-tier user-facing semantics.
