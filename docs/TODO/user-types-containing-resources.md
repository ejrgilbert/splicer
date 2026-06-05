# User-declared types containing resources are unsupported

User WIT can declare records / variants whose fields hold resources:

```wit
record envelope { id: u64, b: bucket }
variant outcome { miss, hit(bucket) }
```

These are legal WIT and wit-bindgen handles them, but splicer's
codegen currently short-circuits on any user type whose field tree
mentions a handle. Neither a `WitTyped` impl nor a
`WitTypedWithResources` impl is emitted, so a strategy with
`R: WitTypedWithResources` won't satisfy its bound for
`Result<Envelope, _>`, and the wrapper fails to compile.

## The constraint

Three structural facts:

- **`WitTyped` routes through wasm-wave**, which has no resource
  representation. Records / variants with resource fields can't go
  through `WitTyped` — that's why `emit_wit_typed.rs:emit_one`
  short-circuits via `f.ty.contains_handle()` for records.
- **`WitTypedWithResources` walks cells directly** (no wave bridge),
  so it CAN handle resource leaves. But the codegen needs to emit a
  cell walker that knows how to dispatch per-field, including
  recursion into wrapper newtypes for the resource fields.
- **The Rust shape of the user type already has the resource as a
  field.** wit-bindgen emits `struct Envelope { id: u64, b: Bucket }`
  where `Bucket` is the export-side `Resource<WrapperBucket>`. For
  tier-4 synthesis, the strategy needs to construct that. But the
  strategy works with `WrapperBucket` (not `Resource<WrapperBucket>`)
  to stay decoupled from wit-bindgen's resource lifecycle.

That third point is the load-bearing one: just like for return-
position resources we generate an intermediate type
(`Result<WrapperBucket, E>` instead of `Result<Bucket, E>`), we'd
need an intermediate USER TYPE
(`WrapperEnvelope { id: u64, b: WrapperBucket }` instead of
`Envelope { id: u64, b: Bucket }`). That's a new layer of codegen.

## Where splicer hits this

`src/adapter/typed/emit_wit_typed.rs:emit_one` (around line 50):

```rust
NamedKind::Record { fields } => {
    if fields.iter().any(|f| f.ty.contains_handle()) {
        return None;
    }
    emit_record(t, fields)
}
```

Returns `None` → no impl emitted for the type. Same logic exists
implicitly for variants (today no handle-bearing variants are
covered) and for any user type whose field tree mentions a resource
at any depth.

Downstream consequences:

- `Result<Envelope, _>` doesn't satisfy `R: WitTypedWithResources`
  because `Envelope: WitTypedWithResources` is missing.
- `Result<Envelope, _>` doesn't satisfy `R: WitTyped` either
  (`Envelope` contains a handle).
- Strategy bounds fail; wrapper crate fails to compile.

`src/adapter/typed/emit_method.rs:build_resource_wrap` recursively
rewrites resource positions for return types, but it operates on
`WitTypeRef` IR — it sees a `Named(Envelope)` and treats it as
opaque. The wrap doesn't reach inside user-declared types.

## Design

### Two pieces of codegen

1. **Per-user-type WTWR impl** that walks cells directly. For each
   record/variant/enum/flags that contains a resource, emit a
   `WitTypedWithResources` impl that walks the cells per-field,
   dispatching to each field's WTWR impl. Resource fields recurse
   through the wrapper newtype's WTWR impl (which calls
   `MockedResource::from_handle_cell`).

2. **Intermediate user-type for the strategy boundary.** For
   tier-4 synthesis, the strategy can't construct a
   `Resource<WrapperBucket>` directly. Either:
   - **(a)** Emit a parallel `WrapperEnvelope` user type with
     resource fields swapped for wrapper newtypes, and extend
     `build_resource_wrap` to recurse through user types into
     this intermediate form.
   - **(b)** Restrict to tier-2 / replay-style consumers where
     the strategy decodes cells into the wrapper-newtype form
     directly. The wrapper code converts to the export-side form
     at the boundary.

(a) is the symmetric extension of how we handle compound returns.
(b) is the "Replay can do this; full tier-4 synthesis of these
types can't" cut.

### Per-type WTWR walker

For `record envelope { id: u64, b: bucket }`:

```rust
impl WitTypedWithResources for Envelope {
    fn from_cells(tree: &FieldTree, root: u32) -> Result<Self, BridgeError> {
        let side_idx = match tree.cells.get(root as usize) {
            Some(Cell::RecordOf(i)) => *i,
            _ => return Err(BridgeError::Unsupported("expected RecordOf for Envelope")),
        };
        let info = tree.record_infos.get(side_idx as usize).ok_or(
            BridgeError::SideTableOutOfBounds { /* ... */ },
        )?;
        let mut id = None;
        let mut b = None;
        for (name, cell_idx) in &info.fields {
            match name.as_str() {
                "id" => id = Some(<u64 as WitTypedWithResources>::from_cells(tree, *cell_idx)?),
                "b" => b = Some(<WrapperBucket as WitTypedWithResources>::from_cells(tree, *cell_idx)?),
                _ => {}
            }
        }
        Ok(Envelope {
            id: id.ok_or(BridgeError::MissingField { name: "id".into() })?,
            b: bindings::exports::iface::Bucket::new(
                b.ok_or(BridgeError::MissingField { name: "b".into() })?
            ),
        })
    }
}
```

Note the final field: the WTWR walker decodes a `WrapperBucket` but
the user's `Envelope` field type is `Bucket` (Resource<WrapperBucket>),
so the walker calls `Bucket::new(WrapperBucket)` to convert.

That works for replay-style consumers: the strategy returns
`Envelope`; the wrapper hands it to the outer caller; wit-bindgen
takes it from there.

This is design (b) above — the strategy returns the user's actual
type, and the WTWR walker handles the conversion. Cleaner than (a)
because no parallel "WrapperEnvelope" types need to exist; the
strategy author sees the same types they'd see from wit-bindgen.

### Same shape for variants / enums / flags

Variants need to walk `Cell::VariantCase` + `variant_infos[idx]`,
match on `case_name`, recurse into the payload (per-case payload
type). For a variant with a resource-carrying case
(`variant outcome { miss, hit(bucket) }`):

```rust
impl WitTypedWithResources for Outcome {
    fn from_cells(tree, root) -> Result<Self, BridgeError> {
        let side_idx = match tree.cells.get(root as usize) {
            Some(Cell::VariantCase(i)) => *i,
            _ => return Err(BridgeError::Unsupported("expected VariantCase")),
        };
        let info = &tree.variant_infos[side_idx as usize];
        match (info.case_name.as_str(), info.payload) {
            ("miss", None) => Ok(Outcome::Miss),
            ("hit", Some(p)) => {
                let b = <WrapperBucket as WitTypedWithResources>::from_cells(tree, p)?;
                Ok(Outcome::Hit(bindings::exports::iface::Bucket::new(b)))
            }
            _ => Err(BridgeError::UnknownCase { type_kind: ..., case: info.case_name.clone() }),
        }
    }
}
```

Enums and flags have no payload, so they can stay value-typed (their
existing `WitTyped`-via-wave path is fine; `impl_wit_typed_with_resources_via_wave!`
already handles them).

### The `WitTyped` story stays "no impl"

User types containing resources still don't get a `WitTyped` impl
(wasm-wave can't represent the resource leaf). The
`impl_wit_typed_with_resources_via_wave!` macro can't fire for them
either (it depends on `WitTyped`). Only the recursive cell walker
fits — and that's the new piece of codegen.

## Implementation plan

### Phase 1 — handle-bearing records (~3–4h)

1. Refactor `emit_one` in `emit_wit_typed.rs` to no longer short-
   circuit on `contains_handle`. Instead, branch:
   - No handles in tree: existing dual emit (`WitTyped` +
     `impl_wit_typed_with_resources_via_wave!`).
   - Has handles: emit ONLY a per-type WTWR walker via a new
     emitter function. No WitTyped impl.
2. Build `emit_record_wtwr_walker(t, fields)`: walks `Cell::RecordOf`,
   recurses into fields, calls `Bucket::new(...)` at resource leaves.
3. Per-field rendering: each field's WTWR call uses the field's
   `WitTypeRef::to_tokens()`. Resource fields render as
   `bindings::exports::iface::Bucket`, but the WTWR call needs the
   `WrapperBucket` ident — add a helper for "rust type at this
   WitTypeRef" vs "WTWR-decodable type" (the latter substitutes
   `WrapperR` for `own<R>`).
4. Matrix tests: record with single resource field, record with
   value + resource fields, record with nested compound resource
   field.

### Phase 2 — handle-bearing variants (~2–3h)

1. Similar walker for variants: `Cell::VariantCase` + per-case
   payload recursion.
2. Per-case payload: same Bucket::new wrapping at resource leaves.
3. Matrix tests: variant with unit + resource cases.

### Phase 3 — error-arm + return integration (~1h)

1. Verify that `Result<HandleBearingRecord, E>` flows through
   `build_resource_wrap` correctly. The record's WTWR is now
   available; the wrap should just recurse via
   `Named(HandleBearingRecord)::to_tokens` for the type and trust
   the type's own WTWR impl for decoding.
2. End-to-end pipeline test: replayer over an interface returning
   `result<envelope, string>` decodes correctly.

## Open design questions

- **Closure over wit-bindgen's resource lifecycle.** Calling
  `Bucket::new(WrapperBucket)` inside `from_cells` materializes a
  `Resource<WrapperBucket>`. That handle is owned by the returned
  value (the user-type's resource field). When the user-type drops,
  the resource drops too. Need to verify this composes correctly
  with wit-bindgen's resource table semantics. Unit-test level
  won't catch issues; runtime smoke needed.
- **Identity of the "Rust type" vs "WTWR-decodable type."** Record
  fields render as `Bucket` (Resource<WrapperBucket>) in the user
  struct but the walker calls WTWR on `WrapperBucket`. The codegen
  needs a clean way to map between them. Reuse
  `build_resource_wrap`'s intermediate-type tracking? Or a separate
  `Bucket → WrapperBucket` substitution at field-render time?
- **Handle-bearing records inside compound returns.** Once
  `Envelope: WitTypedWithResources` exists, does
  `build_resource_wrap` know to NOT rewrite `Named(Envelope)` to
  some intermediate (it shouldn't — Envelope IS the user-facing
  type)? The contains_resource_own check currently doesn't see
  inside `Named` references; it would need to be either type-aware
  or stay conservative.

## Non-goals

- **`WitTyped` for handle-bearing types.** wasm-wave has no resource
  representation; there's no way to bridge. Strategies that need
  these types use the WTWR path; strategies that want the wave path
  are restricted to value-typed types.
- **Mutating resource fields in place.** TypedVisit's mutation
  story (used by redact-strings et al.) treats resource leaves as
  opaque-skip. Records with resource fields walk through the
  non-resource fields normally; resource fields are left alone.

## References

- `src/adapter/typed/emit_wit_typed.rs:emit_one` — current short-
  circuit.
- `splicer-tool-sdk/src/bridge_resources.rs` — WTWR trait + macros.
- `splicer-tool-sdk/src/bridge.rs:decode_record` — pattern for the
  walker (used for `WitTyped` over records).
- `docs/TODO/tier3-tier4-substrate.md` — broader coverage gap
  context.
