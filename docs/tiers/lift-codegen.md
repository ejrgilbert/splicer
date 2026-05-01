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
| Classify             | `tier2/lift.rs`                    | `LiftPlan`, `CellOp`, `LocalRef`, `LiftPlanBuilder`, `classify_func_params`, `classify_result_lift`          |
| Side-table layout    | `tier2/lift.rs`                    | `register_side_table_strings`, `build_side_table_blob`, `SideTableBlob`, `SideTableSpec`                     |
| Static-memory layout | `tier2/emit.rs`                    | `lay_out_static_memory`, `StaticDataPlan`, `FieldSideTables`, `build_fields_blob`, `build_after_params_blob` |
| Section + body emit  | `tier2/emit.rs` + `tier2/cells.rs` | `emit_type_section`, `emit_imports_and_funcs`, `emit_wrapper_function`, `CellLayout::emit_*`                 |

Schema layouts (`field`, `field-tree`, `cell`, side-table records)
are derived once via `wit_parser::SizeAlign` + `RecordLayout` /
`CellLayout` in `compute_schema` and threaded through the rest as
`SchemaLayouts`. Field offsets are looked up by name (`offset_of("…")`)
at use sites — no hardcoded offsets in adapter codegen.

---

## Phase 1: classify → `LiftPlan`

The output of classification, per (function, param) and (function,
result), is a **`LiftPlan`** — a flat `Vec<CellOp>` describing every
cell the lift needs to write, in allocation order, with `cells[0]`
as the root.

```rust
struct LiftPlan { cells: Vec<CellOp> }

enum CellOp {
    Bool { local: LocalRef },
    IntegerSignExt { local: LocalRef },
    IntegerZeroExt { local: LocalRef },
    Integer64 { local: LocalRef },
    FloatingF32 { local: LocalRef },
    FloatingF64 { local: LocalRef },
    Text { ptr: LocalRef, len: LocalRef },
    Bytes { ptr: LocalRef, len: LocalRef },
    EnumCase { local: LocalRef, info: NamedListInfo },
    RecordOf { type_name: String, fields: Vec<(String, u32)> },
    // … one per supported WIT type ctor; new kinds add a variant ↑
}

enum LocalRef {
    /// nth flat-slot wasm local of the function's params.
    Param(u32),
    /// `lcl.ptr_scratch` — set up by the wrapper before the lift.
    PtrScratch,
    LenScratch,
    /// `lcl.result` — direct primitive return captured into a local.
    Result,
}
```

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
3. **Flat-slot consumption.** Each `LocalRef::Param(n)` allocated by
   the builder pins one wasm flat slot. Total slots = `next_local -
   first_local` after the build — no per-`LiftKind` `slot_count()`
   table to keep in sync with the plan-builder.

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
        LiftKind::Bool => { /* push Bool { local: Param(self.bump_local()) } */ }
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

### `LocalRef::resolve`

The plan is built without knowing about `WrapperLocals` (which is
allocated per-wrapper at emit time). At emit time, we resolve each
`LocalRef` to a concrete wasm local index:

```rust
impl LocalRef {
    fn resolve(&self, lcl: &WrapperLocals) -> u32 {
        match self {
            LocalRef::Param(i) => *i,
            LocalRef::PtrScratch => lcl.ptr_scratch,
            LocalRef::LenScratch => lcl.len_scratch,
            LocalRef::Result => lcl.result.expect("…"),
        }
    }
}
```

Why a tagged enum vs. raw `u32`: `lcl.ptr_scratch` and
`lcl.result` are wrapper-body scratch locals; `Param(n)` is a flat-
slot local from the function signature. They live in disjoint local
ranges and serve disjoint roles. Using one type forced you to know
which kind of local an index referred to by convention. The tagged
enum makes "scratch local" and "param flat slot" non-confusable at
the type level.

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

## What this doc does **not** cover

- **Tier-1 codegen** (`src/adapter/tier1/`) — different model entirely,
  no lift; passes raw canonical-ABI bytes through.
- **The `field-tree` wire format** for middleware authors — see
  [`tier-2.md`](./tier-2.md).
- **Tier-3+ models** — not yet implemented.
- **Result-side compound lifts** (records as return values, etc.) —
  defer; needs a memory-load-prefix variant of the lift codegen
  because canonical-ABI returns compounds via retptr (whole struct
  in memory), not flat locals.
