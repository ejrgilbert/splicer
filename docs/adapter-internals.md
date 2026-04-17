# Adapter generation — architecture

Low-level map of the code that produces a tier-1 adapter component.
Companion doc: [`adapter-components.md`](./adapter-components.md) is the
user-facing explainer; this file is for contributors working on the
generator itself.

## Mission

Given

- the target interface (cviz `InterfaceType::Instance`),
- a split `.wasm` that imports or exports it,
- the set of `splicer:tier1/*` interfaces the middleware exports,

emit a WebAssembly Component binary that:

- Re-exports the target interface unchanged (drop-in replacement for
  the upstream caller).
- Imports the target interface from a handler-providing component.
- Imports the middleware's tier-1 hooks (`before`, `after`, `blocking`).
- For each function in the target interface, wraps it with the hooks'
  before/after/blocking phases, handling the canonical-ABI lift/lower
  and async machinery transparently.

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
  `Resolve::push_flat` for flattening.
- [`wit-bindgen-core::abi`] — instruction-level codegen via the
  `Bindgen` trait. Walks a type, emits an abstract instruction stream
  (`I32Load { offset }`, `VariantLift { … }`, `RecordLift { … }`,
  `FixedLengthListLiftFromMemory { … }`, etc.).

Splicer implements `Bindgen` against `wasm-encoder::Instruction`. Every
canonical-ABI decision — walk order, offsets, discriminant widths,
joined flat shapes, widening rules — comes from upstream. Splicer's
implementation is a transcriber: abstract `Instruction` → concrete wasm
opcode.

When upstream adds a new canonical-ABI feature, splicer picks it up
via `cargo update` with at most one new `emit` match arm. When the
upstream types grow a new variant splicer doesn't handle, Rust's
exhaustive-match rule fails at compile time — **loud at build, silent
never.**

[`wit-parser`]: https://docs.rs/wit-parser
[`wit-bindgen-core::abi`]: https://docs.rs/wit-bindgen-core

## Module layout

```
src/adapter/
├── abi/                  — canonical-ABI abstraction
│   ├── bindgen.rs        — WasmEncoderBindgen (Bindgen impl)
│   ├── bridge.rs         — WitBridge (cviz → wit-parser translator)
│   └── compat.rs         — verbatim cast / flat_types (pending [PR #1597])
├── build/                — wasm binary emission
│   ├── component.rs      — build_adapter_bytes (outer Component, 13 phases)
│   ├── dispatch.rs       — inner dispatch core module
│   ├── encoders.rs       — component-level type-section encoders
│   ├── mem_layout.rs     — MemoryLayoutBuilder (scratch-memory allocator)
│   └── ty.rs             — prim_cv, val_type_byte_size, align_to_val
├── filter/               — closure-based split dep walker + raw-sections re-encoder
├── func.rs               — AdapterFunc value object
├── indices.rs            — ComponentIndices / DispatchIndices / FunctionIndices
├── names.rs              — stable import/export name strings
├── tests.rs              — integration tests
└── mod.rs                — generate_tier1_adapter entry
```

Two layers (`abi/`, `build/`) plus three cross-cutting root files
(`func.rs`, `indices.rs`, `names.rs`) and a `filter/` module that
stands on its own.

### Layer responsibilities

**`abi/` — spec-consuming.** Encodes knowledge *of* the canonical ABI
by importing `wit-parser` and `wit-bindgen-core`. Never touches
`wasm-encoder` section builders directly except for individual
`Instruction` opcodes inside the `Bindgen` impl. Nothing in `abi/`
knows about "the adapter's shape" — it's generic lift-from-memory
machinery.

**`build/` — wasm-emitting.** Knows about the adapter's shape (13
phases, hook sequencing, nested core modules, name conventions) and
uses `wasm-encoder` sections to assemble the final binary. Consumes
`abi/` for the lift-from-memory instruction bytes at the one spot
that needs them (`task.return` load).

**Cross-cutting (root).** `AdapterFunc` is a per-function value object
produced by `func.rs::extract_adapter_funcs`. `indices.rs` holds three
running-index allocators (one per namespace: outer component, dispatch
core module, wasm function locals). `names.rs` centralizes string
constants. Used by both layers.

## Type-flow: cviz arena → emitted wasm

```
cviz::TypeArena                        src/adapter/mod.rs entry
      │
      │ (splicer's internal type model, populated from the split)
      ▼
WitBridge::from_cviz(&arena)          abi/bridge.rs
      │
      │ Walks every ValueTypeId in the arena, allocates a wit_parser
      │ TypeDef per compound type, records a HashMap<ValueTypeId, Type>.
      │ Types insert children-first so Resolve::types stays topologically
      │ ordered (SizeAlign::fill requires this).
      ▼
wit_parser::Resolve + SizeAlign       (owned by WitBridge)
      │
      │ Every canonical-ABI query now goes through this. WitBridge
      │ exposes `size_bytes`, `flat_types`, `has_strings`, `has_lists`
      │ wrappers so splicer consumers don't import wit-parser directly.
      ▼
AdapterFunc list                      func.rs::extract_adapter_funcs
      │
      │ Per-function resolution: param/result type ids, core-wasm flat
      │ signature (via push_flat), result buffer size, has-strings /
      │ has-lists predicates. Also allocates the initial bytes of the
      │ dispatch module's scratch memory (function-name blob +
      │ per-function result buffers).
      ▼
build_adapter_bytes                   build/component.rs
      │
      │ The 13 phases assemble the outer Component: type / import /
      │ alias sections, handler instance type, canon lift/lower,
      │ embed mem module + dispatch module, wire instances, export.
      ▼
build_dispatch_module                 build/dispatch.rs
      │
      │ Emits the inner core-wasm module: per-function wrapper bodies
      │ with hook phases + async wait-loops + task.return. For async
      │ funcs with a result, pre-runs WasmEncoderBindgen on the result
      │ type to get the instruction sequence that task.return needs.
      ▼
Final .wasm bytes
```

### Where the Bindgen actually fires

Just one spot: `build/dispatch.rs::build_task_return_loads`. For each
async function whose result is non-void, we:

1. Allocate an i32 local via `FunctionIndices::alloc_local` to hold
   the result buffer's base address.
2. Emit `I32Const(result_ptr); LocalSet(addr_local)` to stash it.
3. Construct a `WasmEncoderBindgen` over the `&bridge.sizes` and
   `&mut indices` (so any locals the bindgen needs for variant
   dispatch / fixed-size-list iteration are allocated into the same
   function-local space).
4. Call `wit_bindgen_core::abi::lift_from_memory(&bridge.resolve,
   &mut bindgen, (), &result_type)`.
5. `bindgen.into_instructions()` — a `Vec<wasm_encoder::Instruction>`
   that, when flushed into the function body, leaves the joined flat
   representation of the result on the wasm value stack, ready for
   the `task.return` call that follows.

All of the canonical-ABI heavy lifting — walking the type, picking
load widths, computing offsets, dispatching variant arms, widening
arm flats to the joined flat, unrolling fixed-size-list iteration —
happens inside upstream's `read_from_memory` via our `Bindgen::emit`.

## `WasmEncoderBindgen` — design notes

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
  `Bindgen` borrows `&mut FunctionIndices` from the caller, so every
  local it allocates (disc locals for variants, payload locals for
  widening stash, iter address locals for fixed-size lists) lands in
  the *same* contiguous local-index space as the dispatch module's
  own locals (subtask / waitable-set). The caller calls
  `indices.into_locals()` once when constructing the `Function`.

See the module docstring in `src/adapter/abi/bindgen.rs` for the full
treatment, including the block-capture rationale and the fixed-size
vs dynamic list table.

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
`i32.wrap_i64` — not no-ops. Tested by
`lift_result_string_u64_widens_pointer_to_pointer_or_i64` and
`lift_result_list_u64_widens_pointer_to_pointer_or_i64` in
`abi/bindgen.rs`.

## Dispatch module — what splicer still owns

The `abi/` layer is generic lift-from-memory. The *shape* of the
adapter — which is splicer's unique value-add — lives in
`build/dispatch.rs`:

- Per-function wrapper body, sequencing five phases: before,
  blocking, handler call, after, return.
- Async wait-loop emission (`waitable-set.wait` blocks) for hook
  subtasks and async handler calls.
- `task.return` wiring: custom wasm function types when the result
  flattens to multiple values, shared `void → ()` / `(i32) → ()`
  types for common cases, per-func import aliases.
- Name-blob data segment + function-name hook invocations.
- Nested core module 0 (memory provider, optionally with a bump
  realloc) whose exports `mem` / `realloc` are aliased out and used
  as canon-lift/lower options.

None of this is canonical-ABI logic — it's adapter policy and
component-model plumbing. Splicer owns it because it's the *shape*
nobody else generates.

## `abi/compat.rs` — a temporary borrow

`wit-bindgen-core`'s private helpers `cast(WasmType, WasmType) ->
Bitcast` and `flat_types(&Resolve, &Type, Option<usize>) ->
Option<Vec<WasmType>>` aren't part of its public API. Splicer's
variant widening needs both. `abi/compat.rs` contains verbatim copies
of those two functions (plus the `MAX_FLAT_PARAMS` constant).

Visibility-flip PR filed upstream:
<https://github.com/bytecodealliance/wit-bindgen/pull/1597>. When it
merges, delete `abi/compat.rs` and change `abi/bindgen.rs` to import
`wit_bindgen_core::abi::{cast, flat_types}` directly. The two
functions reference only already-public types (`WasmType`, `Bitcast`,
`Resolve`, `Type`), so the flip is semantically trivial.

## Supported WIT types for async results

Everything except Map. Specifically:

- All primitives (bool, s8..s64, u8..u64, f32, f64, char, string,
  error-context)
- Records, tuples at any nesting
- Enums, flags (any case/flag count)
- Resources (Own / Borrow)
- Futures, Streams (as i32 handles)
- Dynamic lists, strings
- Variants, options, results — including heterogeneous arms with
  `Pointer`/`Length` ↔ `PointerOrI64` widening
- Fixed-size lists — unrolled N-element reads with per-iteration
  address advancement
- Any nesting of the above

**`Map<K, V>`**: `wit-bindgen-core`'s `read_from_memory` currently
has `TypeDefKind::Map(..) => todo!()`. An async result type
containing a Map would panic at lift time. Workaround: WIT authors
can use `list<tuple<K, V>>` instead. Fixing this requires a
`wit-bindgen-core` patch.

The sync function path and function parameter handling flow through
component-model `canon lift` / `canon lower` opcodes, which are the
runtime's responsibility. Splicer never emits instruction-level
lift/lower for those — it just declares the lift/lower operations
via `CanonicalFunctionSection`, and the runtime does the rest. So
those paths have always handled whatever types the component model
supports.

## How canonical-ABI evolution affects the code

Three failure modes, in order of frequency:

1. **New `TypeDefKind` upstream.** `WitBridge::translate` has an
   exhaustive match over cviz's `ValueType`, so cviz evolution forces
   a compile error. The wit-parser side uses upstream-provided
   `push_flat` / `SizeAlign` behavior, which absorbs most new type
   kinds without our code changing. If upstream adds a
   `TypeDefKind` splicer genuinely can't express (because cviz
   doesn't have a matching `ValueType`), the bridge needs a new
   translation arm.

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

## Index spaces

Three separate counter allocators, one per namespace:

| Struct | Namespace | Scope |
|---|---|---|
| `ComponentIndices` | outer Component types / instances / funcs / core instances / core funcs | one per adapter component |
| `DispatchIndices` | dispatch core module types / funcs | one per dispatch module |
| `FunctionIndices` | wasm function locals | one per emitted wasm function |

Keeping them separate makes the "different index spaces" explicit —
the dispatch core module's type table has no relationship to the
outer component's, and a function's locals are disjoint from both.

`FunctionIndices` is the cross-cutting one: both the dispatch module
(for subtask / waitable-set locals) and the Bindgen (for iter
address, disc, and payload stash locals) allocate into the same
instance. The caller constructs it, pre-allocates anything it knows
about, then threads a `&mut FunctionIndices` into `Bindgen::new`.
When the bindgen is dropped, the caller calls `into_locals()` and
feeds the result into `Function::new_with_locals_types`.

## Testing

Three layers of test coverage:

- **Unit tests in `abi/bindgen.rs`** (~11 tests): emit-level
  assertions — "loading a u32 emits one `i32.load`", "heterogeneous
  variant emits one `i64.extend_i32_u`", "option's None arm pads
  with `i32.const 0`". These catch bitcast / widening regressions at
  `cargo test` time.

- **In-process adapter validation in `src/adapter/tests.rs`** (~60
  tests): run the full adapter generator for various interface
  shapes, then validate the emitted binary with wasmparser. Catches
  structural bugs but not runtime behavior.

- **End-to-end composition in `tests/component-interposition/`**: run
  `./run.sh __testme` to build every configuration (single middleware
  / chain / fan-in / nested / …), compose with real handler
  components, and execute the result through a wasmtime runner. This
  is the gold standard for "does the adapter actually work."

Any non-trivial change should clear all three. The bitcast widening
bug we fixed in Stage 2 was invisible to the first two — it only
surfaced when `__testme` tried to compose the real `wasi:http`
error-code variant.

## References

- [`CanonicalABI.md`](https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md)
  — the spec.
- [`definitions.py`](https://github.com/WebAssembly/component-model/blob/main/design/mvp/canonical-abi/definitions.py)
  — precise reference semantics.
- `docs/TODO/investigate-canon-abi.md` — the decision / migration
  doc that drove the Bindgen adoption.
- `docs/adapter-comp-planning.md` — broader planning notes on the
  tier-1 adapter.
