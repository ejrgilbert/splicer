# Tier-3/4 static methods on resources are partly broken

Static methods on a WIT resource (e.g.
`[static] bucket.list-names: func() -> list<string>`,
`[static] bucket.anonymous: func() -> bucket`) work in some
tier × sync/async × return-shape combinations and fail or punt in
others. The two real holes:

- **Tier-3 sync static returning a resource.** Codegen emits a body
  whose type doesn't match the trait signature: the trait expects
  `Resource<WrapperBucket>` (export-side), the body emits the
  import-side `Bucket`. Wrapper crate won't compile.
- **Tier-4 statics (any return).** Codegen emits a deliberate
  `::core::compile_error!("tier-4 static methods on resources are
  not yet supported")`. No design yet for sync-or-async strategy
  dispatch from a static method body.

## What works today

| Tier | Sync/Async | Returns | Status |
|---|---|---|---|
| 3 | async | value-typed | ✓ |
| 3 | async | resource | ✓ (matrix: `matrix_resource_static_factory_returns_resource`) |
| 3 | sync | value-typed | ✓ |
| 3 | sync | resource | **✗ type mismatch** |
| 4 | any | any | **✗ `compile_error!`** |

## The constraint

Static methods on resources are unusual in real WIT (typical
factory patterns put the factory at interface level), but they're
legal canonical-ABI shapes. Three structural facts:

- **Tier-3 sync methods bypass strategy dispatch.** The L2 sync-
  method limitation: sync wrapper methods can't `.await` an async
  `TransformStrategy::handle`, so the codegen emits the import call
  directly with no strategy interposition. For value-typed returns
  that's fine; for resource returns the import call returns the
  import-side `Bucket`, which doesn't match the trait's export-side
  `Resource<WrapperBucket>`.
- **Tier-4 has no import side.** A tier-4 wrapper world doesn't
  import the target interface, so a tier-4 static body has nothing
  to forward to. The only way a strategy can supply a return is
  through dispatch, which is async — sync tier-4 statics can't
  dispatch, async tier-4 statics could.
- **Static methods can be sync or async per WIT.** Unlike
  constructors (always sync per spec), static methods carry the
  same sync/async dimension as any other function. Codegen needs
  to handle both.

## Where splicer hits this

`src/adapter/typed/emit_method.rs`, in the
`GuestTraitKind::Resource(_), ExportFnKind::Static` branch
(currently around line 412):

```rust
Behavior::Transform => {
    // tier-3: dispatch through the import-side type surface
    let import_resource = build_import_resource_path(resource_pascal, guest_module_path);
    build_static_call(&import_resource, method_ident, fields)
}
Behavior::Virtualize => {
    let msg = format!(
        "tier-4 static methods on resources are not yet supported \
         (encountered `{}::{}`)",
        resource_pascal, method_ident,
    );
    quote!(::core::compile_error!(#msg))
}
```

This produces `target_call`. Downstream of it, the body construction
diverges by sync/async:

```rust
let body = if !is_async {
    quote! { let args = #args_construct; #target_call }  // sync: raw call, no wrap
} else {
    // async: dispatch + final_wrap
};
```

For tier-3 sync static returning a resource, `target_call` is
`bindings::iface::Bucket::anonymous()` (returning import-side
`Bucket`). `resource_wrap` IS computed (the return contains
`own<R>`) but only `final_wrap` is applied in the async branch; the
sync branch ignores it. The body emits the raw import call where the
trait sig expects `Resource<WrapperBucket>`.

For tier-4 static (any sync/async), `target_call` is the
`compile_error!` macro invocation. That propagates through the body
unchanged.

## Design

### Tier-3 sync-static-returns-resource fix

Apply the wrap in the sync body path when one is computed:

```rust
let body = if !is_async {
    let call = match &resource_wrap {
        Some(rw) => &rw.forward_expr,
        None => &target_call,
    };
    quote! { let args = #args_construct; #call }
} else {
    // async: unchanged
};
```

This makes sync resource methods (including statics) symmetric with
async ones. The same fix covers the
tier-3-sync-method-returning-resource case more generally — sync
resource methods that return a resource have the same shape.

Matrix tests:
- `[static] bucket.anonymous: func() -> bucket` (sync, returning own).
- `bucket.clone: func() -> bucket` (sync method returning own).
- Both wrap to `WrapperBucket(bindings::iface::Bucket::anonymous())`
  and `Bucket::new(intermediate)` analogues at the boundary.

### Tier-4 static method dispatch

Four sub-cases:

| Static is | Returns | Strategy |
|---|---|---|
| async | value-typed | Dispatch through `VirtualizeStrategy<EmptyArgs, R>`. Same pattern as interface-level async fn. |
| async | resource | Same + apply wrap. Strategy R = `WrapperBucket`; final_wrap calls `Bucket::new`. |
| sync | value-typed | No await, no dispatch. Options: (a) `R::default()` if available, (b) `compile_error!`, (c) require manifest annotation. **Open question — see below.** |
| sync | resource | `mint_mock_resource!` (same as constructors). |

For the async cases, dispatch is straightforward — static methods
have no `&self`, so the closure is `||` for tier-3 and
`VirtualizeStrategy::handle(s, call, args)` for tier-4. Args struct
is whatever the static's params are (empty for `anonymous()`).

For sync tier-4 statics returning value-typed: there's no way to
synthesize a non-default value without a strategy. Two paths:

- **`compile_error!` with a clear "use async statics for tier-4
  interposition" message.** Honest about the limitation.
- **`R::default()` if `R: Default` can be proven structurally.** Same
  heuristic as `hello-tier4` but at the codegen site. Surprising —
  the user wouldn't expect a default value from a static call.

`compile_error!` is the safer default. Statics that need tier-4
dispatch should be async; that's a WIT-side guideline, not a
splicer limitation.

## Implementation plan

### Phase 1 — tier-3 sync wrap fix (~1h)

1. Modify the sync body construction in `emit_method.rs` to apply
   `resource_wrap.forward_expr` when present.
2. Matrix rows for: sync static returning resource, sync method
   returning resource.
3. Document that tier-3 sync RESOURCE METHODS now work (was a
   doc'd "L2 sync limitation"; this lifts the limitation for the
   resource-return subset).

### Phase 2 — tier-4 static method dispatch (~2–3h)

1. In the `(Resource, Static)` arm of `target_call` construction,
   branch on `behavior × is_async × return-contains-resource`:
   - tier-4 async value-typed: dispatch via `VirtualizeStrategy`.
   - tier-4 async resource: dispatch + wrap.
   - tier-4 sync resource: `mint_mock_resource!`.
   - tier-4 sync value-typed: keep `compile_error!`, refine message.
2. Update `matrix_tier4_static_method_fails_fast_with_compile_error`
   to cover the now-supported cases (verify they no longer emit
   `compile_error!`) and the still-unsupported sync-value-typed
   case.
3. Add `Default` for chaos-err / replayer pipeline tests that may
   now exercise tier-4 statics.

## Open design questions

- **Sync tier-4 statics returning value-typed: `compile_error!` or
  default?** Compile-error is honest; default is convenient for the
  hello-tier4-style stub strategies. Lean toward compile-error.
- **Wrap-in-sync naming.** The sync body currently just uses
  `target_call` directly. After this change it uses `forward_expr`
  when a wrap is computed. The naming `target_call` vs `forward_expr`
  in the sync branch is slightly off — worth renaming to make the
  symmetry obvious.
- **Tier-3 sync method bypassing strategy.** This is a related but
  broader limitation: tier-3 sync methods bypass strategy dispatch
  entirely (the L2 sync limitation). After this fix, sync RESOURCE
  methods that return a resource will compile, but they still won't
  interpose — they just forward to the import. Worth documenting
  alongside the fix.

## Non-goals

- **Lifting the L2 sync-method limitation for value-typed methods.**
  That would require either a sync version of `TransformStrategy`
  or routing sync methods through `block_on`-style adapters. Out of
  scope; the resource-return fix is a structural codegen change,
  not a strategy-trait change.

## References

- `src/adapter/typed/emit_method.rs:407-420` — current static-method
  branch.
- `src/adapter/typed/emit_method.rs:486` — sync body construction
  that bypasses the wrap.
- `src/adapter/typed/tests/matrix.rs:matrix_resource_static_factory_returns_resource`
  — works today (tier-3 async).
- `src/adapter/typed/tests/matrix.rs:matrix_tier4_static_method_fails_fast_with_compile_error`
  — current tier-4 fail-fast.
- `docs/TODO/tier3-tier4-builtins.md` — broader tier-3/4 substrate
  context.
