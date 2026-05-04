# Adapter lift codegen

How the tier-2 adapter generator turns a WIT type into wasm that
populates a `field-tree` of cells at runtime. Companion to
[`tier-2.md`](./tier-2.md), which describes the wire format; this
doc is for contributors editing `src/adapter/tier2/`.

> **Scope.** Tier-2 today. Tier-1 hooks pass raw canonical-ABI
> bytes through and don't lift; tier-3/-4 are aspirational. When
> tier-3 lands, extend this doc with its model.

---

## The pipeline

For one target function the adapter walks four phases:

```
WIT param/result types
       ↓ (1) classify
  LiftPlan (per param + per result)
       ↓ (2) layout
  static memory plan (cells slabs, side tables, fields blob)
       ↓ (3) section emit
  wasm types/imports/funcs/exports/data
       ↓ (4) wrapper body
  per-fn wrapper that lifts → calls hook → forwards → calls hook
```

Each phase has one home:

| phase                | file                               | key types/fns                                                                                                |
|----------------------|------------------------------------|--------------------------------------------------------------------------------------------------------------|
| Classify             | `tier2/lift.rs`                    | `LiftPlan`, `CellOp`, `LiftPlanBuilder`, `classify_func_params`, `classify_result_lift`                      |
| Side-table layout    | `tier2/lift.rs`                    | `register_side_table_strings`, `build_side_table_blob`, `SideTableBlob`, `SideTableSpec`                     |
| Static-memory layout | `tier2/emit.rs`                    | `lay_out_static_memory`, `StaticDataPlan`, `FieldSideTables`, `build_fields_blob`, `build_after_params_blob` |
| Section + body emit  | `tier2/emit.rs` + `tier2/cells.rs` | `emit_type_section`, `emit_imports_and_funcs`, `emit_wrapper_function`, `CellLayout::emit_*`                 |

Schema layouts (`field`, `field-tree`, `cell`, side-table records)
are derived once via `wit_parser::SizeAlign` + `RecordLayout` /
`CellLayout` in `compute_schema` and threaded through the rest as
`SchemaLayouts`. Field offsets are looked up by name (`offset_of("…")`)
at use sites — no hardcoded offsets in adapter codegen.

---

## Library leverage and what we considered

The principle: **use library leaf-operations where they fit, hand-roll
the driver pattern.** Specifically:

### What we use from libraries

| library | what we use | where |
|---|---|---|
| `wit_parser::SizeAlign` | byte sizes, alignments, record/tuple field offsets | `compute_schema`, `RecordLayout`, retptr scratch sizing |
| `wit_parser::Resolve::push_flat` | canonical-ABI flat-slot count + types per WIT type | (planned) flat-slot allocation for retptr-loaded result locals |
| `wit_bindgen_core::abi::lift_from_memory` | walks a WIT type from a memory address, emits wasm bytecode that pushes flat values onto the stack | async `task.return` flat loads (today); will drive retptr-loaded compound result lifts (records-as-result and beyond) |
| `wit_bindgen_core::abi::lower_to_memory` | inverse: walks a WIT type, emits wasm that writes a typed value to memory | (anticipated) tier-3's modify-and-write-back path |

These are **leaf** operations: "given an address, produce flat" or
"given typed value, produce memory." Self-contained, well-defined I/O,
no hidden assumptions about caller context.

### What we evaluated and rejected: `wit_bindgen_core::abi::call(...)` as wrapper driver

`call(resolve, variant, lift_lower, func, bindgen, async_)` is the
library's full lift+CallInterface+lower driver. We considered using
it as the wrapper-body emitter — implement `Bindgen` for our cell-
tree target, let the library walk every WIT type. Concluded
**model mismatch**, not effort:

1. **The library expects a language binding.** `CallInterface`'s
   contract is "invoke typed user code with typed args, produce a
   typed result." Our wrapper is **flat-to-flat passthrough that
   produces a cell tree as a side artifact** — the cell tree is
   observation, not the value forwarded to the handler. We'd be
   hijacking `CallInterface` to do something orthogonal.

2. **Operand dual-tracking.** Each typed operand would need to carry
   BOTH the cell index (for the side artifact) AND the original flat-
   slot wasm locals (for forwarding to the handler). Language
   bindings carry one representation per operand; we'd carry the
   cost of typed lift while not actually using the typed values for
   the call.

3. **Side-table info has to leak out of the impl.** Language bindings
   emit code, period. Our impl produces wasm body AND per-(fn, param)
   side-table contributions (record-info entries, enum-info, etc.).
   The `Bindgen` trait isn't designed for that secondary output
   channel.

4. **Two-pass requirement.** Static-memory layout needs cell counts
   before code emit. `call(...)` is one pass — we'd run it twice
   (discovery + emit) or cache an entire emit run.

The hand-rolled `LiftPlan` / `CellOp` is structurally better for our
use case: declarative, walkable by the side-table builders, tracks
flat-locals alongside cells, one pass.

### When to revisit

Reconsider when:
- **Tier-3's modify-and-write-back** lands. `lower_to_memory` already
  applies; the rest of tier-3 may map more naturally to `call(...)` if
  we end up doing a lift→modify→lower flow that matches the library's
  shape.
- The cumulative LoC of hand-rolled compound kinds (variant + list +
  tuple + option + result) crosses ~600 LoC. Migration would still
  cost ~700+ LoC but the LoC win starts paying off there.

For variant specifically: hand-rolling needs canonical-ABI variant
payload joining via `wit_parser::abi::join`. That math is in the
library and we can use it without buying the full `call(...)` driver.

---

## Phase 1: classify → `LiftPlan`

The output of classification, per (function, param) and (function,
result), is a **`LiftPlan`** — a flat `Vec<CellOp>` describing every
cell the lift needs to write, in allocation order, with `cells[0]`
as the root.

```rust
struct LiftPlan { cells: Vec<CellOp>, flat_slot_count: u32 }

enum CellOp {
    Bool { local: u32 },
    IntegerSignExt { local: u32 },
    IntegerZeroExt { local: u32 },
    Integer64 { local: u32 },
    FloatingF32 { local: u32 },
    FloatingF64 { local: u32 },
    Text { ptr: u32, len: u32 },
    Bytes { ptr: u32, len: u32 },
    EnumCase { local: u32, info: NamedListInfo },
    RecordOf { type_name: String, fields: Vec<(String, u32)> },
    // … one per supported WIT type ctor; new kinds add a variant ↑
}
```

`local` / `ptr` / `len` are **absolute wasm-local indices** baked in at
plan-build time. The plan-builder accepts a `local_base` (the absolute
index of the plan's first flat slot) and increments from there as cells
consume slots.

### Why a flat plan instead of a nested IR

The plan is the **single source of truth** for three otherwise easily-
desynced facts:

1. **Cell count.** `plan.cells.len()` is what we allocate slab space
   for. A field-tree with `cells.len = 5` requires 5 cells in the
   slab, full stop.
2. **Cell indices in side-table entries.** `RecordOf::fields` carries
   `(field-name, child-cell-idx)` where `child-cell-idx` is just the
   `Vec`-position of where that child got pushed during plan-building.
   The same `Vec<CellOp>` is what codegen iterates over at emit time,
   so the index a side-table entry references is *literally* the same
   slot the codegen writes to.
3. **Flat-slot consumption.** Each cell that names a `local` pins one
   wasm flat slot. Total slots = `plan.flat_slot_count` (computed by
   the builder as `next_local - local_base` at `into_plan` time) — no
   per-`LiftKind` `slot_count()` table to keep in sync with the plan-
   builder.

Compare to the alternative we replaced: `(LiftKind, first_local,
SideTableInfo)` triples per param, with side-table indices and cell
allocation tracked across separate functions. Adding records would
have required four new tracking concerns (parent/child cell offsets,
field-name lookup, side-table index allocation, slab sizing) all
agreeing. With `LiftPlan` they can't disagree because they read the
same vector.

### Building a plan

`LiftPlanBuilder` allocates cells parent-first via mutual recursion:

```rust
fn push(&mut self, ty: &Type, resolve: &Resolve) -> u32 {
    let root_idx = self.cells.len() as u32;
    match LiftKind::classify(ty, resolve) {
        // primitives: push one CellOp consuming next_local..next_local+N
        LiftKind::Bool => { /* push Bool { local: self.bump_local() } */ }
        // …

        // compounds: push parent first, recurse on children, backfill parent
        LiftKind::Record => {
            self.cells.push(CellOp::RecordOf { type_name, fields: vec![] });
            let parent = root_idx as usize;
            let mut fields = vec![];
            for f in record_fields(ty, resolve) {
                let child_idx = self.push(&f.ty, resolve);  // ← appends after parent
                fields.push((f.name.clone(), child_idx));
            }
            if let CellOp::RecordOf { fields: f, .. } = &mut self.cells[parent] {
                *f = fields;
            }
        }
        // …
    }
    root_idx
}
```

The recursion order is **parent-before-children**: when the recursion
unwinds, every child's cells live at indices ≥ parent_idx + 1, and
the parent's `fields` list captures those indices. Self-evident
correctness (vs. trying to predict child indices ahead of time).

### Absolute local indices baked at build time

Cells store **absolute** wasm-local indices, not plan-local offsets.
The plan-builder's caller provides `local_base` — the absolute index
of the plan's first flat slot — and the builder increments from there
as it allocates cells:

- **Params**: `local_base = slot_cursor`, the cumulative flat-slot
  count of preceding params. Known at classify time because wasm
  function params occupy locals `0..param_flat_count`.
- **Compound results**: synth locals are allocated at emit time via
  `FunctionIndices::alloc_local`. The classify-time
  `CompoundResult::plan` is built with placeholder `local_base = 0`
  (only its cell structure is consumed — by the side-table builders).
  The emit phase rebuilds a fresh plan with `local_base =
  synth_locals[0]` and stores it on `ResultEmitPlan::Compound::plan`;
  that's the plan `emit_lift_plan` walks.

Why absolute indices on cells, not a plan-local offset + base
resolved at emit time: the contiguity invariant ("synth locals occupy
`synth_locals[0..N]`") becomes a build-time fact baked into each
cell, not a runtime contract `emit_lift_plan` has to honor by reading
`alloc_wrapper_locals` carefully.

---

## Phase 2: side-table layout

Nominal cells (`enum-case`, `record-of`, `flags-set`, …) carry a
`u32` *side-table index*; the actual metadata (type-names, case-
names, child cell indices) lives in per-kind side tables on the
`field-tree`.

Every kind shares the same lifecycle:

1. **Walk all plans** for cells of this kind, dedup-register their
   strings into the shared `name_blob`.
2. **Lay out one entry record per "thing"** (per case, per record-
   instance, per flag-bit, etc.) in a contiguous data segment.
3. **Hand back per-(fn, param) and per-(fn, result) `BlobSlice`
   pointers** that the field-tree blobs patch in.

`register_side_table_strings` + `build_side_table_blob` are generic
over an extractor closure (`Fn(&CellOp) -> Option<&KindInfo>`). New
kinds plug in by:

- adding their info struct (e.g., `NamedListInfo` for enum, eventually
  a record-info-shaped one),
- adding a thin extractor closure (`|op| match op { CellOp::EnumCase {
  info, ..} => Some(info), _ => None }`),
- providing a `SideTableSpec { entry_layout, item_name_field }`,
- adding a slot to `FieldSideTables` (the per-tree side-table-
  pointers struct in `emit.rs`).

### Discriminant index conventions

For some kinds the side-table index is *runtime-dynamic*; for others
it's *adapter-build-time-static*. This affects whether the cell's
payload is sourced from a wasm local or an `i32.const`:

| kind | side-table index | source |
|---|---|---|
| `enum-case` | the runtime disc — N entries laid out per type, one per case, in disc order | `Local(disc)` |
| `record-of` | allocated by the side-table builder — one entry per *plan cell* of kind `RecordOf` | `ConstI32(idx)` |
| `flags-set` | one entry per *runtime value* (set-bits-by-name); per-call cabi_realloc | TBD when flags lands |
| `variant-case` | the runtime disc — entries laid out per type, one per case | `Local(disc)` (probably) |
| `resource-handle` etc. | one entry per *handle id* observed at runtime | TBD when handles land |

`cells.rs::PayloadSource::{Local, ConstI32}` discriminates these at
the byte-emission level — the lift-codegen layer chooses the source
based on the `CellOp`'s static-vs-dynamic semantics.

---

## Phase 3: static-memory layout

`lay_out_static_memory` reserves data-segment regions for everything
the wrapper body references at runtime. Order matters (some blobs
hold pointers into others):

1. **`name_blob` strings.** Param names + side-table strings get
   appended first, then placed as a single data segment. Anything
   that needs a string offset reads from this blob.
2. **Cells slabs.** One contiguous slab per param's `LiftPlan`, sized
   to `plan.cells.len() * cell_size`. Each `ParamLift::cells_offset`
   gets set to the param's slab base. Optional 1-cell scratch for
   the result lift, when on-return is wired.
3. **Side-table data segments.** Each kind's blob (e.g., `enum-info`
   entries) gets placed; the relative offsets stored in the `BlobSlice`
   pointers handed back from `build_side_table_blob` get translated
   to absolute via `BlobSlice::translate`.
4. **Fields blob (data).** One pre-built `field` record per (fn, param),
   with `cells.ptr` pointing at the param's slab and per-kind side-
   table pointers patched in. Built by `build_fields_blob` walking
   `param_side_tables: Vec<Vec<FieldSideTables>>`.
5. **On-return params blob (data).** One pre-built `func-params` record
   per fn, with `result: option<field-tree>` either `some` (function
   has a liftable result) or `none`. Built by `build_after_params_blob`.
6. **Scratch slots.** Event-record slot for canon-async waits, on-call
   indirect-params buffer, per-fn retptr scratch.

The whole region is one contiguous `0`-based data segment laid out by
`StaticLayout`; `bump_start = layout.end().next_multiple_of(cell_align)`
is where `cabi_realloc`'s arena begins after our static data.

### `FieldSideTables`

The "what side-table pointers does this one field-tree carry" struct.
One field per supported kind:

```rust
struct FieldSideTables {
    enum_infos: BlobSlice,
    // record_infos: BlobSlice,     // when records land
    // flags_infos: BlobSlice,
    // variant_infos: BlobSlice,
    // handle_infos: BlobSlice,
}

impl FieldSideTables {
    fn write_to_tree(&self, blob: &mut [u8], tree: &RecordWriter) {
        tree.write_slice(blob, TREE_ENUM_INFOS, self.enum_infos);
        // tree.write_slice(blob, TREE_RECORD_INFOS, self.record_infos);
        // …
    }
}
```

The patch sites in `write_field_record` and `build_after_params_blob`
both go through `FieldSideTables::write_to_tree`, so adding a new
kind is one field on the struct + one line in the writer.

---

## Phase 4: wrapper-body emit

For each target function, `emit_wrapper_function` builds one wrapper
that:

```text
// ── Phase 1 of body: on-call ──
for (cell_idx, cell_op) in plan.cells:
    addr = param.cells_offset + cell_idx * cell_size
    emit cell_op at addr (writes one 16-byte cell)
populate hook-params buffer (call-id strings, args ptr/len)
canon-lower-async-call $on_call, wait

// ── Phase 2: forward to handler ──
emit_handler_call (bridges callee-returns ↔ caller-allocates retptr)
if async: wait for handler subtask

// ── Phase 3: on-return ──
if result_lift is Some and after-hook wired:
    emit lift for the result cell at result_cells_offset
canon-lower-async-call $on_return, wait

// ── Phase 4: tail ──
async: task.return (with retptr or flat result)
sync: emit_wrapper_return
```

The cell-write loop is the heart of the lift codegen:

```rust
for (cell_idx, cell_op) in plan.cells.iter().enumerate() {
    let cell_addr = param.cells_offset + cell_idx as u32 * cell_layout.size;
    f.instructions().i32_const(cell_addr as i32);
    f.instructions().local_set(lcl.addr);
    emit_cell_op(f, cell_layout, cell_op, lcl);
}
```

`emit_cell_op` is a switch on `CellOp` variants that delegates to
`CellLayout::emit_{bool,integer,floating,text,bytes,enum_case,
record_of,…}` (one helper per cell variant in `cells.rs`). The
helpers are ABI-correct by construction: they read field offsets
from the schema-derived `CellLayout` and use the `PayloadPart`
abstraction to enforce store-width / alignment.

### `WrapperLocals`

Pre-allocated up-front so every emit phase references the same set:

| local | purpose |
|---|---|
| `addr` | scratch for the cell write address |
| `st`, `ws` | canon-async packed-status + waitable-set handle |
| `ptr_scratch`, `len_scratch` | retptr-loaded `(ptr, len)` for Text/Bytes results |
| `ext64` | i64 widening source for IntegerSignExt/ZeroExt |
| `ext_f64` | f64 promoted source for FloatingF32 |
| `result` | direct-return value when export sig has one flat result |
| `tr_addr` | i32 addr local driving `lift_from_memory` for async task.return flat loads |

`alloc_wrapper_locals` allocates them in deterministic order; the
resulting indices are referenced through the `WrapperLocals` struct
(not raw u32s) at every emit site.

---

## Adding a new WIT type ctor: a checklist

When adding tier-2 support for a new WIT type ctor (say, `flags`):

1. **`LiftKind::classify`** already maps the WIT type ctor to a
   `LiftKind` variant. Confirm it's right.
2. **`LiftPlanBuilder::push`** — add a case that allocates the right
   `CellOp`(s) and recurses on children if any.
3. **`CellOp`** — add a variant carrying the side-table info needed.
4. **Side-table builder** — if the kind has a static side-table:
   - Add an extractor closure `Fn(&CellOp) -> Option<&KindInfo>`
   - Add a `SideTableSpec` (or a new dedicated builder if the kind's
     entry shape differs from `NamedListInfo`)
   - Add a thin facade in `lift.rs` (e.g., `register_flags_strings`,
     `build_flags_info_blob`).
5. **`SchemaLayouts`** — add the kind's entry-record `RecordLayout`,
   loaded in `compute_schema`.
6. **`FieldSideTables`** — add a `<kind>_infos: BlobSlice` field and
   one line in `write_to_tree`.
7. **`lay_out_static_memory`** — register strings, build blob, place,
   translate, fold into `param_side_tables` / `result_side_tables`.
8. **`cells.rs`** — implement `CellLayout::emit_<kind>` (replace the
   `todo!()` stub with the real `emit_cell` call).
9. **Lift-codegen dispatch** — add the `CellOp` variant arm in
   `emit_cell_op` (the per-cell switch in `lift.rs::emit_lift_param`).
10. **Test** — add a shape to `tests/fuzz_and_run.rs::canned_shapes`
    and run the canned tier-2 sweep.

If the new kind has runtime-allocated side-table entries (like
`flags-info { set-flags: list<string> }`), step 4's "static side-
table builder" doesn't apply — instead, the codegen at step 9 emits
`cabi_realloc` calls to allocate the entry per-call.

---

## Per-kind roadmap

Status of each WIT type ctor as a tier-2 lift source:

| kind                             | param | result          | LoC est. (delta)    | mechanism notes                                                                                                                                     |
|----------------------------------|-------|-----------------|---------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------|
| primitives (bool/int/float)      | ✓     | ✓               | —                   | shipped                                                                                                                                             |
| string / bytes (`list<u8>`)      | ✓     | ✓ (retptr-pair) | —                   | shipped                                                                                                                                             |
| enum                             | ✓     | ✓               | —                   | shipped — first nominal-cell side table                                                                                                             |
| **record**                       | **✓** | **✓**           | —                   | shipped — first compound result; uses `lift_from_memory` over retptr scratch + per-result synthetic locals.                                         |
| flags                            | ✗     | ✗               | ~150 each direction | per-call `cabi_realloc` for `set-flags: list<string>`; first kind with runtime-allocated side-table entries. Limit ≤32 flags in v1 to drop ~40 LoC. |
| option                           | ✗     | ✗               | ~80 each direction  | runtime disc + recursive payload lift                                                                                                               |
| result\<T,E\>                    | ✗     | ✗               | ~80 each direction  | same as option but with two payload arms                                                                                                            |
| char                             | ✗     | ✗               | ~80 each direction  | utf-8 encode at runtime via `cabi_realloc`; emits as `cell::text`                                                                                   |
| **variant**                      | ✗     | ✗               | ~150 each direction | hardest hand-roll: canonical-ABI payload joining via `wit_parser::abi::join`. Use the library's `join` math but keep the dispatch hand-rolled.      |
| list (non-u8)                    | ✗     | ✗               | ~150 each direction | runtime loop over elements + `cabi_realloc` for cell-index array                                                                                    |
| tuple                            | ✗     | ✗               | ~100 each direction | similar to record but anonymous + heterogeneous                                                                                                     |
| handles (resource/stream/future) | ✗     | ✗               | ~200 cumulative     | new `handle-info` correlation table; per-handle u64 id assignment                                                                                   |
| error-context                    | ✗     | ✗               | TBD                 | design TBD                                                                                                                                          |

### Recommended order

1. **flags** — first runtime-allocated side-table machinery.
2. **option / result\<T,E\>** — share the disc + recursive-payload
   pattern; ship together.
3. **char** — leaf utf-8 encoder.
4. **variant** — hardest hand-roll; pull in `wit_parser::abi::join`.
5. **list / tuple** — runtime loops + index-array allocation.
6. **handles** — correlation-id table.

The result-side `LiftPlan` shape that record-as-result validated
generalizes to all future compound results: the wrapper-body emitter
allocates per-result synthetic locals, runs `lift_from_memory` to
push canonical-ABI flat values from retptr scratch onto the wasm
stack, captures them via `local.set` in reverse order, then walks
the plan exactly like a param.

### "Add a kind" recipe

The checklist in [Adding a new WIT type ctor](#adding-a-new-wit-type-ctor-a-checklist) is the canonical reference. Highlights for new contributors:

- **`LiftKind::classify`** already maps every WIT type ctor — start by
  confirming the right variant is returned.
- **Plan-builder arm in `LiftPlanBuilder::push`** is the main work.
  For compound types, follow the `push_record` pattern: reserve the
  parent cell first, recurse on children, backfill parent's child
  cell indices.
- **Side-table machinery** is generic over an extractor closure for
  per-case kinds (enum-info shape) via `register_side_table_strings` +
  `build_side_table_blob`. Per-instance kinds (record-info,
  flags-info) build their own `*_info_blob` fn alongside but the
  pattern (entries blob + tuples/strings sub-blob + per-(fn, param)
  ranges + cell-idx assignments) is documented in `RecordInfoBlobs`.
- **`FieldSideTables` slot** + one `write_to_tree` line per kind.
- **`cells.rs::emit_<kind>`** — replace the `todo!()` stub with the
  real emit. For nominal cells with adapter-build-time-known indices
  (record-of, flags-set), use `PayloadSource::ConstI32`. For runtime-
  disc indices (enum-case), use `PayloadSource::Local`.
- **Test fixture in `canned_shapes`** + an arm in
  `predict_tier2_arg_inner` matching `fmt_cell`'s output exactly.
- **`fmt_cell` in `MIDDLEWARE_TIER2_LIB_RS`** to render the new cell
  variant in the middleware's trace output.

---

## What this doc does **not** cover

- **Tier-1 codegen** (`src/adapter/tier1/`) — different model entirely,
  no lift; passes raw canonical-ABI bytes through.
- **The `field-tree` wire format** for middleware authors — see
  [`tier-2.md`](./tier-2.md).
- **Tier-3+ models** — not yet implemented.
