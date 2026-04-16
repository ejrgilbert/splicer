# Generating a Proxy Component #

Right now, if someone wants to splice middleware on some function signature, the middleware has to import and export
that exact function signature. This means there needs to be one middleware provided per unique function signature it
runs on! This becomes unmanageable for any real-world application where middleware must be placed on every function
call (e.g. a pluggable OpenTelemetry middleware).

To help alleviate this developer burden, the next step in this project is to generate "proxy components" from middleware
that needs to be adapted to fit on some function signature. The following constraints must hold true for a middleware
to be eligible for generating a proxy component wrapper:
1. The component can only import _from the host_
2. TODO: Fill in constraints as more are discovered

Some resources that could be helpful here:
- https://github.com/chenyan2002/proxy-component/tree/main/src
- https://github.com/bytecodealliance/wasm-tools/tree/main/crates/wit-dylib
    - [Example that generates the lift](https://github.com/bytecodealliance/wasm-tools/blob/main/crates/wit-dylib/src/bindgen.rs#L768)

# Use cases
I see the following requirements for middleware capabilities where a middleware can do none or all of these things.

TODO: It's possible there are more cases, if so make sure to note them here!

Timing
1. Runs _before_
2. Runs _after_
3. Runs _before_ AND _after_

Block
1. Can _conditionally_ block the downstream call from being invoked

Data flow (requires type-aware value access -- see Middleware Tiers below)
1. **Inspect** the data being _passed to_ the downstream function
2. **Modify** the data being _passed to_ the downstream function (must run _before_ the invocation)
3. **Inspect** the data being _returned from_ the downstream function (must run _after_ the invocation)
4. **Modify** the data being _returned from_ the downstream function (must run _after_ the invocation)

# Approaches
I have thought of the following approaches, but am open to more ideas (especially if they are cleaner to implement):
1. Rust macros (that the middleware writer invokes)
2. Rust meta-programming (similar to what is done here: `/Users/evgilber/git/research/proxy-component`)
3. Wasm component creation (like from the bottom up using `wirm` library)

The best approach depends on the middleware tier (see Middleware Tiers below). For Tier 1 and Tier 2
adapter generation, use `wasm_encoder` to build the component binary directly — the adapter is
pure dispatch glue with no value construction from scratch, and `wasm_encoder`'s
`ReencodeComponent` trait handles the fiddly index-translation work. For one-per-sig middleware,
use the [`proxy-component`] approach (`syn`/`quote` Rust codegen)! Attempting to do type-aware value
generation in raw Wasm bytecode would be orders of magnitude more complex with no benefit.

# Middleware Tiers

Not all middleware needs the same level of access to function arguments and return values. Two tiers are
proposed, each with its own middleware WIT interface. The generated proxy component knows which tier to
use based on which interface the middleware exports.

## Tier 1: Type-Erased Middleware

The middleware only needs timing and/or blocking behavior. It never touches argument or return values.
The proxy handles all type plumbing; the middleware just receives the function name.

WIT interface:
```wit
interface type-erased-middleware {
    before-call: func(name: string);
    should-block-call: func(name: string) -> bool;  // true = block downstream
    after-call: func(name: string);
}
```

Generated proxy shape:
```
export handle(req: request) -> response:
    middleware.before-call("handle")
    proceed = middleware.should-block-call("handle")
    if proceed:
        result = downstream.handle(req)
        middleware.after-call("handle")
        return result
    else:
        // block case constraints apply (see above)
```

The block case has one wrinkle: if `should-block-call` returns `false`, you need to return something from the exported function
without calling downstream. This is fine for:
- `void functions`: just return
- `result<T, E>` return types: return an Err (very common in WASI, e.g. wasi:http/handler)

But for functions returning plain values (not wrapped in result), there's no sensible "blocked" value to synthesize.
Worth documenting as a constraint on which interfaces support the block use case.

**Suitable for**: OpenTelemetry tracing, logging, rate limiting, auth (allow/deny only).

## Tier 2: Value-Aware Middleware

The middleware needs access to serialized argument and return values (e.g. for caching or inspection).
Arguments and results are passed as WAVE-encoded strings. The proxy handles canonical ABI
lifting/lowering; the middleware works entirely with strings.

WIT interface:
```wit
interface value-aware-middleware {
    // return some(wave-encoded-result) to short-circuit and skip downstream entirely
    before-call: async func(name: string, args: string) -> option<string>
    // return some(wave-encoded-result) to replace the downstream result
    after-call: async func(name: string, result: string) -> option<string>
}
```

Generated proxy shape:
```
export handle(req: request) -> response:
    wave_args = wave_encode(req)
    cached = middleware.before-call("handle", wave_args)
    if cached is some:
        return wave_decode(cached)
    result = downstream.handle(req)
    wave_result = wave_encode(result)
    override = middleware.after-call("handle", wave_result)
    return wave_decode(override) if override is some else result
```

**Suitable for**: memoizers, circuit breakers, result caching, mutation-based fuzzers.

## The "One-Per-Signature" Case

Some middleware genuinely cannot be expressed generically over serialized values because they must
**fabricate structurally valid new values from scratch**. This requires knowing the full type structure
at code-generation time, not just at runtime.

Known one-per-sig cases:
- **Type-generating fuzzer**: must construct valid values of every parameter type from raw random
  bytes. Mutation-based fuzzers (start from a real value, perturb the WAVE string) fit in Tier 2.
- **Mock/stub generator**: must return a valid fake value of the return type. (Replay from a
  recorded trace is Tier 2 since the WAVE bytes already exist; mocks that synthesize responses
  from scratch are one-per-sig.)
- **Property-based test harness**: must generate and shrink typed counterexamples; shrinking
  requires constructing smaller valid values, not just mutating existing ones.
- **Argument defaulting/enrichment**: filling in missing or zero fields requires knowing which
  fields are optional vs. required and what sensible defaults look like per type.

### Why Rust codegen ([`proxy-component`] approach) is the right tool here

The tempting alternative is to generate the Wasm component directly using `wirm`. However, that
would require implementing, in raw Wasm bytecode:
- Canonical ABI lowering/lifting per WIT type
- Recursive valid-value construction per WIT type (records, variants, lists, options, resources...)
- Random value generation over that construction

This is an enormous amount of work and very hard to get right.

The [`proxy-component`] project demonstrates a much leaner path: generate a small Rust file using `syn`/`quote`,
then compile it with `cargo`. This works because `wit-bindgen` already derives `Arbitrary` on every generated type,
so the entire type-correct random value construction reduces to:

```rust
let mut u = Unstructured::new(&random_bytes);
let value: SomeWitType = u.arbitrary().unwrap();
```

The actual codegen in [`proxy-component`] (`generate_fuzz_func`) is only ~120 lines of `quote!` macros.
The hard type-specific work is fully delegated to `wit-bindgen` + the `arbitrary` crate, neither
of which needs to be re-implemented.

### The Implementation Split

| Middleware tier      | Generation approach                                          | Rationale                                                                    |
|----------------------|--------------------------------------------------------------|------------------------------------------------------------------------------|
| Tier 1 (type-erased) | `wasm_encoder`                                               | Pure dispatch, no value construction; direct binary construction is simplest |
| Tier 2 (value-aware) | `wasm_encoder`                                               | Dispatch + WAVE encode/decode; still no value construction from scratch      |
| One-per-sig          | Rust codegen via `syn`/`quote` + `wit-bindgen` + `arbitrary` | `arbitrary` derive handles all type complexity for free; codegen stays small |

For natively-provided one-per-sig middleware (fuzzer, mock, property harness), splicer generates
the complete component. There is no separate "strategy" component. The algorithm lives in splicer's
Rust code generator, and `wirm` is not involved. The cost is an external `cargo build` step, but
since these are code-generation artifacts (not runtime operations), that is acceptable.

[`proxy-component`]: https://github.com/chenyan2002/proxy-component/tree/main

# Future Work

## Per-function interposition filter (config-level allow-list)

Today a tier-1 adapter wraps **every** exported function of the target
interface with the same middleware. The middleware can filter at
runtime via the `name` param, but the hook round-trip still fires on
every call — including the ones the middleware immediately no-ops.
That's fine for single-function interfaces (`wasi:http/handler`) but
gets expensive and awkward as interfaces grow.

Proposal: an optional `funcs: [...]` include-list per injection in the
splice config. When present, the adapter emits a dispatch wrapper only
for the listed functions; the rest become direct
`alias export <handler_inst> "<func_name>"` — zero runtime cost for
excluded funcs, zero coupling between the middleware and specific
target names.

```yaml
rules:
  - before:
      interface: my:service/math
    inject:
      - name: metrics-mdl
        path: ./metrics.wasm
        funcs: [add, div]   # only wrap these; sub/mul pass through
```

**Impact is localized:**

- `SpliceRule`/`Injection` grows an `Option<Vec<String>>`.
- `extract_adapter_funcs` partitions the interface's functions into
  `(wrapped, passthrough)` using the filter. The passthrough list just
  needs the name + signature enough for `alias export`.
- `build_adapter_bytes` emits dispatch wrappers for `wrapped` (same as
  today) and direct aliases for `passthrough`; both groups end up
  under the same target-interface export instance.
- `validate_contract` gets a new check: names in `funcs` must exist in
  the target interface, reported next to the existing "available
  interfaces" diagnostic.

**Nothing changes** in the closure walker, the canonical-ABI
machinery, or the memory module — this is purely a phase-1 dispatch
decision.

**Hold off until:** there's a concrete multi-function target where
the runtime-hook-per-excluded-call overhead is a real pain, or a user
hits the "my middleware shouldn't need to know the function names of
every target it attaches to" decoupling problem. Until then the
include-list is a solution looking for a problem.

**Open design questions for when we revisit:**

- Exclude-list form (`except_funcs: [...]`) as a convenience for the
  common case of "wrap everything except these"? Keep to a single
  form for v1.
- Glob / regex patterns? Probably not — function names are
  well-defined at config time and a bounded list is unambiguous.
- Interaction with tier-2 / tier-3 (where filtering also affects
  whether we need to lift/lower payloads) — spec this then.

## Audit follow-ups (tier-1 cleanup stack)

Pulled from a structural code audit of `src/adapter/`. These are
concrete items the module would benefit from, ranked by leverage.

### Correctness (latent bugs, fix before they bite)

All of these are real issues that have never surfaced because the
test suite exercises wasi:http/handler-shaped interfaces and closely
related primitive shapes. They'd all bite the first time someone
points splicer at a different interface shape.

- [x] **Exhaustive matches on `ValueType`; silent-resource `bail!`.**
  _Landed._ Two compile-time / immediate-error wins that don't add
  maintenance burden for the correctness items below:

  **(a) Exhaustive matches on `ValueType`.** Three sites had
  `_ => false` or `_ => build_sequential_layout(...)` wildcard arms
  that would silently swallow a new `ValueType` variant if cviz ever
  grew one: `type_has_strings` and `type_has_resources` at
  `src/adapter/ty.rs:112` / `:154`, and `FlatLayout::new` at
  `src/adapter/ty.rs:343`. All three are now explicit per-variant
  matches — a new cviz variant will force an explicit decision at
  compile time.

  **(b) Silent `Primitive(U32)` resource fallback → `bail!`.**
  `encode_comp_cv` at `src/adapter/encoders.rs:313` used to fall back
  to `Primitive(U32)` when a resource's `ValueTypeId` wasn't
  registered in `comp_own_by_vid`, silently losing the `own<T>`
  typing. Now it bails with `"internal error: resource {id:?} not
  registered in comp_own_by_vid — the handler-import phase must
  declare every resource before encode_comp_cv references it"`. All
  existing tests pass, confirming the fallback was unreachable today
  — but the error now surfaces immediately if registration ever
  drifts out of sync with the encoder.

  **What was considered and dropped:** an upfront
  `validate_iface_supported` pre-pass. Each correctness item below
  is going to add its own proper handling (or explicit `bail!`) at
  the fix site; a pre-pass duplicates those errors and becomes stale
  work — every accepted shape has to be un-rejected in two places.
  The exhaustive matches above give the same compile-time safety
  without the maintenance shadow.

### Remaining silent-fallback audit (handle at the fix site)

The fail-closed pass above covered `ValueType` matches and the
resource registration. Several other silent fallbacks remain in the
filter / reencoder / wac — they don't affect correctness under
today's test fixtures, but an upstream change (wasmparser / wirm
adding a new enum variant) could silently drop tracked items. Fix
each at its site, not via a pre-pass:

- `src/adapter/filter/section_filter.rs:253` — `ItemKind → ItemSpace::Other`
- `src/adapter/filter/section_filter.rs:340` — `Space → None` in `lookup_loc`
- `src/adapter/filter/raw_sections_reencoder.rs:414` — export kind
  count bump (`Func`/`Module`/`Component`/`Value` ignored; comment
  flags this as "room to grow")
- `src/adapter/filter/raw_sections_reencoder.rs:671, 675` —
  `ComponentExternalKind` / `ComponentOuterAliasKind` →
  `AliasSpaceKind::Other`
- `src/adapter/filter/raw_sections_reencoder.rs:434, 444, 462, 470` —
  `self.type_map.get(&ty).copied().unwrap_or(ty)` falls back to the
  *original* index if the lookup misses. The comment says downstream
  wasm validation will catch misuse, but a stale-but-valid index
  could still type-check and silently reference the wrong thing. Fix
  by changing `unwrap_or(ty)` to
  `.ok_or_else(|| anyhow!("type {ty} expected in filtered map but
  missing — closure walker bug"))`.
- `src/wac.rs:192` — `InternedId → None` fallback when looking up
  export types (minor).
- `src/adapter/dispatch.rs:464` — `core_results[0] → void_i32_ty`
  fallback. Currently unreachable (flat_types_for only produces
  I32/I64/F32/F64) but worth making exhaustive for future-proofing.

- [x] **`FlatLayout::new` mis-computes memory offsets for
  discriminated and subword-containing types.** _Largely landed._
  Rewrote `FlatLayout::new` to walk the `ValueType` structure
  directly using `canonical_size_and_align` — every slot's
  `byte_offset` now matches what `canon lower` writes.
  `FlatSlot`'s `(val_type, load_byte_size)` pair was replaced with a
  single `FlatSlotShape` enum (6 variants, one per valid load
  instruction: `U8`/`U16`/`I32`/`I64`/`F32`/`F64`), making the
  "valid combos only" invariant type-level and dropping the
  unreachable-arm panic from `emit_load_slot`. Subword loads
  (`i32.load8_u`/`i32.load16_u`) now emit at canonical payload
  offsets (1 for `option<u8>`, 2 for `option<u16>`, etc.), and
  discriminator loads match the variant's case-count-derived width.
  New tests: `option<u8>` / `option<u16>` / `result<u8, u8>` as
  async results — all pass. Deferred sub-items:
  - **Heterogeneous variant arms** (e.g. `variant { u8, u64 }`)
    use the longest-flat arm's layout at the payload offset; reads
    for shorter arms pick up the canonical bytes plus
    canon-lower's (zero-init) padding, which canon lift truncates
    back on the receiving side. Spec-non-compliant if memory is
    ever reused in a non-zero-init pattern. Proper fix needs
    runtime dispatch on the discriminator — bigger change in
    `emit_task_return_loads`.
  - **Record / Tuple / Variant / Enum as top-level results** now
    lay out correctly, but tests for those shapes need a richer
    consumer-WAT template (compound types used in import instance
    types need to be pre-exported with `(eq N)`). Coverage
    deferred to a template rewrite. (`src/adapter/ty.rs`
  lines 343–401.) The actual bug scope is wider than the original
  sketch; in-scope fix needs real implementation of each case, not
  stubs/panics. Concrete shapes broken today:

  1. **Multi-byte discriminants**: `Variant` with >256 cases has a
     u16 discriminant (2 bytes), >65_536 has u32 (4 bytes).
     `build_sequential_layout` treats the disc slot as full i32
     regardless, so a stale byte 2–3 from prior memory could
     corrupt the widened disc value. Also the sequential fallback
     doesn't call `i32.load16_u` / `i32.load8_u` for narrower discs.
  2. **Subword payload alignment**: `Option<u8>` / `Option<u16>`,
     `Result<u8, u8>`, `Variant { case u8 }` — the canonical ABI
     stores the payload at `align_to(disc_size, canonical_align(T))`
     (offset 1 for `Option<u8>`, offset 2 for `Option<u16>`), but
     `append_sequential` aligns every I32 slot to 4 bytes regardless
     of the original type's canonical alignment. So we load the
     payload from the wrong offset (4 instead of 1/2), reading
     adjacent garbage.
  3. **Subword value loads**: even at the right offset, an I32 slot
     representing a u8 needs `i32.load8_u` (1 byte → widened), not
     `i32.load` (reads 4 bytes, high 3 are adjacent memory).
  4. **Heterogeneous variant arms (joined-flat)**: `Variant { case
     u8, case u64 }` — at flat position 1, the joined type is I64.
     Canon lower writes either 1 byte (u8 case) or 8 bytes (u64
     case) depending on which arm is active. Loading as i64 from
     the common offset reads wrong bytes for the u8 arm. Correct
     handling needs runtime dispatch on the disc to load the active
     arm's canonical layout and widen to joined flat positions —
     genuinely complex code-emit, not a layout math tweak.
  5. **Subword fields in records / tuples**: `Record { a: bool, b:
     bool }` — canonical layout puts `b` at offset 1; flat-widened
     puts it at offset 4. Same class as (2).

  **Why it doesn't bite in practice today**: the bump-allocator
  memory is zero-init on first allocation, so subword reads get
  `u8_value | (0 << 8) | (0 << 16) | (0 << 24)` = correct widened
  i32. Fragile to any allocator reuse pattern and spec-non-compliant,
  but currently passes tests by accident.

  **Fix scope**: real implementations of each case, per explicit
  user ask ("don't just panic"). The fix needs:
  - `FlatSlot` to track canonical byte size (not just flat val type)
    so loads use the right width (`i32.load8_u` / `i32.load16_u` /
    `i32.load` / `i64.load`).
  - Layout walk that uses canonical-ABI alignment throughout, threaded
    from the original `ValueType`, not `val_type_byte_size`.
  - Discriminated-type handling for `Result` / `Option` / `Variant`
    / `Enum` via a unified `build_discriminated_layout`.
  - For heterogeneous variant arms, the dispatch code in
    `emit_task_return_loads` needs a runtime branch on disc to load
    each arm's canonical layout and widen. This is the biggest
    piece — likely a new emit helper, not just a layout builder.
  - Subword-field handling in records/tuples: the sequential layout
    builder must use canonical alignment, not val-type alignment.
  Add tests across all five shape categories, each with a
  non-zero-init memory assertion (fill memory with `0xFF` before
  the test and verify the widened value still comes back correctly)
  so the bug actually surfaces when broken.

- [ ] **`FixedSizeList(T, N)` silently encoded as dynamic `list<T>`;
  element count `N` discarded.** (`src/adapter/encoders.rs` lines
  246–251 in `InstTypeCtx::encode_cv` and lines 447–461 in
  `encode_comp_cv`.) Both sites call `defined_type().list(inner_cv)`,
  dropping `N`. wasm-encoder has `defined_type().fixed_size_list(cv,
  elements)` — it's simply not being used. The generated adapter's
  interface type claims `list<T>` where the real interface has
  `list<T, N>`, producing a type mismatch at composition time against
  any real provider/consumer that uses fixed-size lists. **Fix**:
  match on `ValueType::FixedSizeList(inner, n)` and call
  `fixed_size_list(inner_cv, n)` at both sites.

- [ ] **`flat_types_for(FixedSizeList)` returns `[I32, I32]`;
  canonical ABI says `N × flat(T)` inlined.** (`src/adapter/ty.rs`
  line 55.) `FixedSizeList(T, N)` is treated the same as dynamic
  `List(T)` — a `(ptr, len)` pair on the wire. Canonical ABI
  flattens `list<T, N>` to `N` repetitions of `flat(T)` inlined;
  `canonical_align` has the same bug (line 171 returns 4 instead of
  `align(T)`). Any sync-complex or async result of type `list<T, N>`
  would mis-size its buffer and mis-offset its memory reads.
  **Fix**: in `flat_types_for`, emit `flat_types_for(inner)` repeated
  `n` times; in `canonical_align`, return `canonical_align(inner)`.

- [x] **`needs_realloc` misses `list<T>` types entirely.** _Landed._
  Added `type_has_lists` in `src/adapter/ty.rs` (exhaustive match,
  covers both `List(_)` and `FixedSizeList(..)`), `has_lists: bool`
  on `AdapterFunc`, and a trio of `canon_needs_memory()` /
  `canon_needs_realloc()` / `canon_needs_utf8()` helpers that
  centralize the per-function canon-option decisions. Updated all
  three canon sites in `src/adapter/component.rs` (handler canon
  lower, `task.return`, canon lift for exports) to use those
  helpers. Tightened the rules: `has_resources` no longer forces
  memory or realloc (bare `own<T>` is an `i32` handle with no
  memory access; resource-in-compound cases are caught by
  `result_is_complex`, and async-result cases by `is_async &&
  has_result`). `type_has_resources` and the `has_resources` bool
  on `AdapterFunc` are now removed — they were only feeding the
  over-conservative rules. Also fixed a latent WAT-template bug in
  the consumer synth split where inline compound types (e.g.
  `(list u32)` in a func param) shifted the numeric type
  indices — switched to named types (`$fn_<name>`). New tests
  cover `list<u32>` as param (sync), as result (sync canon-lift
  path), and as async param (task.return path).

  - [ ] **Flat params and flat results >16 silently declare the wrong
    core type.** (`src/adapter/func.rs` lines 188–197;
    `src/adapter/dispatch.rs` lines 317–333 wrapper types, 340–391
    handler + task.return types.) `core_params.extend(flat_types_for(id))`
    and `types.ty().function(core_results.iter().copied(), [])`
    produce verbatim flat-types signatures. Canonical ABI rule:
    when total flat params/results > MAX_FLAT_PARAMS (16), the core
    signature collapses to `(i32)` pointer form with the arguments
    marshaled in memory. Our wrapper / handler / task.return type
    declarations would mismatch what canon lift/lower produces at the
    component level, failing at link time. Latent for any function
    with >16 flat params or any async result whose flat form exceeds
    16 values (e.g. a record with 20 u32 fields). **Fix**: in
    `extract_func_sig`, if the accumulated flat length exceeds 16,
    collapse to `[I32]` and record a flag (`params_are_ptr: bool`,
    `results_are_ptr: bool`). Each emitter in dispatch.rs checks the
    flag and emits pointer-form signatures; memory layout needs a
    corresponding buffer reservation for the spilled args.

- [x] **Silent fallback to `U32` for unregistered resources.**
  _Landed with the fail-closed pass above._ The
  `encode_comp_cv` fallback at `src/adapter/encoders.rs:313` now
  bails with `"internal error: resource {id:?} not registered in
  comp_own_by_vid — the handler-import phase must declare every
  resource before encode_comp_cv references it"`. All existing tests
  pass, confirming the fallback was unreachable today.

### To do

- [x] **Split `emit_imports_from_consumer_split` in two.** _Landed._
  Renamed the top-level dispatcher to `emit_imports_from_split`
  (the old name was a misnomer after the split handled both
  variants). The dispatcher now just copies the raw sections, seeds
  the index allocator, and calls
  `emit_imports_consumer_split` (when `target_interface` is in
  `split.import_names`) or `emit_imports_provider_split` (otherwise).
  Each strategy owns its own `ImportsOutcome` construction, so the
  consumer path (re-export handler types via `InstanceExport`) and
  provider path (reuse preamble-aliased types via `alias outer`) can
  be read in isolation. No behavior change; 116 tests pass.
- [x] **Centralize hook / env-slot name constants.** _Landed._ Two
  sources of truth now:
  - **`build.rs`** extracts function names from
    `wit/tier1/world.wit` and emits both `TIER1_{IFACE}_FNS` (WIT
    hyphenated names like `"before-call"`) and
    `TIER1_{IFACE}_ENV_SLOTS` (underscored mirrors like
    `"before_call"` for use in env core-instance slots). Accessible
    via `crate::contract::...`.
  - **`src/adapter/names.rs`** holds splicer-internal env-slot
    names that aren't derived from WIT: `ENV_INSTANCE` (`"env"`,
    the module arg name for the dispatch module), `ENV_MEMORY`,
    `ENV_REALLOC`, the async-builtin names
    (`ENV_WAITABLE_NEW` etc.), and the indexed-name helpers
    (`env_handler_fn(i)`, `env_task_return_fn(i)`).
  Both `component::emit_hook_inst_types` /
  `component::emit_dispatch_phase` and `dispatch::build_dispatch_module`
  now reference the same constants — a rename only has to happen in
  one place. Doc comments on `component::emit_hook_inst_types` and
  on the `hook_ty` / `block_ty` declarations in
  `dispatch::build_dispatch_module` point at `wit/tier1/world.wit`
  as the source of truth for the hook *signature shape* — while the
  names come from WIT automatically via `build.rs`, the canon-
  lowered flat shapes (`(i32, i32) → i32` etc.) are still spelled
  out in Rust and would need updating if the WIT signatures change.
- [~] **~~Pass hook flags into `compute_memory_layout`.~~** _Dropped
  after attempting._ The audit claim (`has_async_machinery`
  "re-derives" from `funcs`) was misread — there's only ONE site
  computing it (inside `compute_memory_layout`). No duplication to
  eliminate. Attempted refactors (hoisting to caller, extracting to
  `needs_async_machinery()` helper) turned one readable inline line
  into a rename or a call site with the same meaning — pure noise.
  The original inline definition reads fine and stays as-is.

### Upstream (wirm / cviz)

- [x] **wirm's concretization had silent `_ => ConcreteValType::Resource`
  fallbacks that defeated splicer's fail-closed boundary.** _Landed
  in wirm._ All six sites in
  `~/git/research/compilers/wirm/src/ir/component/concrete.rs`
  (`concretize_from_resolved`,
  `concretize_comp_type_to_val`,
  `resolve_type_from_import_instance`) and the
  `InstanceExport`-as-val-type match on lines 578–597 now panic with
  `"invalid component: …"` messages describing the specific
  invariant that failed, instead of fabricating a bogus `Resource`.
  Framing is component-model-spec-level, not WIT-specific — the val
  types list comes straight from the spec. 102 wirm tests pass, 116
  splicer tests pass; the fallbacks were unreachable from today's
  test fixtures, which confirms the silent-fallback was pure dead
  weight (and a bug waiting to happen).

### To investigate

- [x] **Extract `build_env_exports` from `emit_dispatch_phase`.**
  _Landed._ The 47-line conditional-push block that built the env
  instance's exports is now a dedicated
  `fn build_env_exports(canon_lower, funcs, mem_core_mem) -> Vec<...>`.
  `emit_dispatch_phase` collapses its env-construction to a one-line
  call. The contract "these names MUST match the dispatch module's
  imports, and the `Option<u32>`s on each side must agree on which
  slots exist" is now documented once at the extracted function and
  referenced by both sides. No behavior change; 116 tests pass.