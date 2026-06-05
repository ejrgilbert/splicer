# Tier-3/4 strategy crates can't read builtin config

Tier-1 and tier-2 builtins ARE components: each imports
`splicer:builtin-config/get@0.1.0` via wit-bindgen, has a `build.rs`
that codegens `mod config` from the manifest's `[[key]]` blocks, and
exposes typed accessors (`config::greeting()`) at runtime. At splice
time, `src/config_provider.rs` builds a per-edge provider component
with YAML values baked in, and the composer wires it to the builtin's
import.

Tier-3/4 strategy crates are NOT components. They're Rust libraries
linked into the codegen'd wrapper crate; the wrapper IS the component.
The strategy crate doesn't have its own wit-bindgen, doesn't import
the substrate, and has no way to surface config to its `handle` body.

Effect: every existing tier-3/4 manifest has vestigial `[[key]]`
blocks (hello-tier3 `greeting`, hello-tier4 `greeting`, chaos-err
nothing, replayer's `SPLICER_REPLAY_TRACE` env var as a workaround).
None of them actually read what the user sets in YAML.

## The constraint

Three structural facts:

- **The wrapper component is what imports things.** Splicer's config
  provider gets composed against components that import
  `splicer:builtin-config/get`. The wrapper crate is the splice-time
  component; the strategy crate is one of its compile-time
  dependencies.
- **wit-bindgen-generated code is per-crate.** If both the wrapper
  and the strategy declared the import via wit-bindgen, they'd emit
  parallel Rust types and possibly clash at link time. The clean
  story is: one crate (the wrapper) owns the wit-bindgen, the other
  (the strategy) consumes via an abstract API.
- **The strategy's `Default::default()` is when state initializes.**
  The `define_strategy_singleton!` macro lazily constructs the
  strategy on first use. Config reading has to happen at or before
  that point.

## Where splicer hits this

| File | Role |
|---|---|
| `wit/builtin-config/world.wit` | The substrate WIT (interface `get`, provider world). |
| `src/config_provider.rs` | Builds the per-edge provider wasm with YAML values baked in. |
| `src/adapter/typed/target_wit.rs` | Constructs the wrapper world WIT. **Doesn't currently add the substrate import.** |
| `src/adapter/typed/assemble.rs` | Assembles the wrapper crate's `lib.rs`. **No `_wrapper_config_get` shim emitted.** |
| `splicer-tool-sdk/src/strategy.rs` | Hosts `define_strategy_singleton!`. **No config-init plumbing.** |
| `splicer-tool-sdk/src/lib.rs` | Public SDK surface. **No `config` module.** |
| `builtins/{hello-tier3,hello-tier4,chaos-err,replayer}/manifest.toml` | All have `[[key]]` blocks but none are wired through. |

`src/config_provider.rs:imports_substrate` already inspects builtin
bytes for `splicer:builtin-config/get` and gates the YAML-error
"non-consumer with non-empty `config:`" check on it. Once the wrapper
declares the import, the wrapper bytes will trip this check and the
existing config-validation path will fire automatically. Most of the
splicer-side machinery is already in place.

## Design

### Wrapper imports the substrate

`target_wit_for_codegen` (`target_wit.rs:42`) constructs the wrapper
world. Add an `import splicer:builtin-config/get@0.1.0;` line
unconditionally — every tier-3/4 wrapper claims the substrate, even
if the strategy doesn't use any config keys (it costs the wrapper
nothing and lets `imports_substrate` find it for the provider
composition).

### Wrapper codegen emits a shim + SDK init

In `assemble.rs`, alongside the `define_strategy_singleton!`
invocation, emit:

```rust
fn _wrapper_config_get(key: &str) -> Option<String> {
    bindings::splicer::builtin_config::get::get(key)
}
```

Modify `define_strategy_singleton!` (in the SDK) to take an extra
arg — the config-get function — and install it into a SDK-level
`OnceLock<fn(&str) -> Option<String>>` before constructing the
strategy:

```rust
::splicer_tool_sdk::define_strategy_singleton!(
    my_strategy::MyStrategy,
    _wrapper_config_get,
);
```

The macro expands to:

```rust
static STRATEGY: OnceLock<MyStrategy> = OnceLock::new();
fn strategy() -> &'static MyStrategy {
    STRATEGY.get_or_init(|| {
        ::splicer_tool_sdk::config::__init(_wrapper_config_get);
        <MyStrategy as Default>::default()
    })
}
```

### SDK `config` module

```rust
// splicer-tool-sdk/src/config.rs
static CONFIG_GET: OnceLock<fn(&str) -> Option<String>> = OnceLock::new();

#[doc(hidden)]
pub fn __init(f: fn(&str) -> Option<String>) {
    let _ = CONFIG_GET.set(f); // idempotent; second-init wins is fine
}

pub fn get(key: &str) -> Option<String> {
    CONFIG_GET.get().and_then(|f| f(key))
}
```

Strategies call `splicer_tool_sdk::config::get("trace_path")` at
runtime. The function pointer indirection means the strategy crate
never imports wit-bindgen and never sees the substrate's WIT types
directly. Init order is enforced by `define_strategy_singleton!`
populating the pointer before constructing the strategy.

### Typed helpers (later, optional)

```rust
pub fn get_u32(key: &str) -> Option<u32> {
    config::get(key).and_then(|s| s.parse().ok())
}
pub fn get_bool(key: &str) -> Option<bool> { /* … */ }
pub fn get_path(key: &str) -> Option<PathBuf> { /* … */ }
```

The values that come through `get` are WAVE-encoded strings (per
`config_provider.rs`'s validate path); a fully-typed API would decode
WAVE per the manifest's declared type. Phase 2.

## Implementation plan

### Phase 1 — wrapper imports + SDK plumbing (~3–4h)

1. `target_wit.rs`: append `import splicer:builtin-config/get@0.1.0;`
   to the wrapper world WIT. Update unit tests.
2. `assemble.rs`: emit the `_wrapper_config_get` shim before the
   strategy singleton macro.
3. SDK: new `config` module with `__init` + `get`. Re-export `get`.
4. `define_strategy_singleton!`: take the shim as a macro arg,
   install it before constructing.
5. Update existing builtins to pass the shim (matrix change touches
   every tier-3/4 strategy crate's expected wrapper output).
6. Pipeline tests: verify the wrapper imports the substrate.

### Phase 2 — strategy-facing usage (~1–2h)

1. Convert `replayer`'s trace-path knob from env var to
   `splicer_tool_sdk::config::get("trace_path")`. Update manifest
   description to document the key.
2. Wire hello-tier3 / hello-tier4 to actually read their `greeting`
   keys (currently hard-coded).
3. Chaos-err: add `seed: u64` config knob for reproducible chaos.

### Phase 3 — typed helpers (later)

WAVE-decode helpers on top of `config::get`. Defer until a strategy
actually wants typed config values.

## Open design questions

- **Default values from manifest.** The provider currently bakes
  whatever the YAML sets. Manifest `default = ...` doesn't propagate
  to runtime if the YAML omits the key — the SDK's `get` returns
  `None`. Should the SDK helper layer apply manifest defaults via a
  build.rs codegen for tier-3/4 (mirroring tier-1/2's `mod config`)?
  Or do strategies handle defaults explicitly (`get(...).unwrap_or(...)`)?
- **Per-strategy build.rs.** Tier-1/2 builtins have a `build.rs`
  invoking `builtin_protocol::build_helper::codegen`. To get
  manifest-aware typed accessors on tier-3/4 strategies, they'd need
  similar build.rs scaffolding. Worth the user-facing complexity, or
  is "raw `get`" + manual parse enough?
- **`__init` race.** If two tier-3/4 wrappers are linked into the
  same wasm component (multiple strategies, multiple resources), the
  shim has the same `_wrapper_config_get` ident. OnceLock makes the
  init idempotent, but the shims would shadow each other at link
  time. Likely needs per-strategy ident mangling.
- **Strategy unit tests.** A strategy's tests can't run with a real
  substrate. SDK should expose a test-only `__set_for_test` so unit
  tests can install a mock config-get function.

## What this enables

- Replayer graduates from `SPLICER_REPLAY_TRACE` env var to a YAML
  `config: { trace_path: "..." }` knob.
- Chaos-err gets a `seed` knob for reproducible chaos in CI.
- Future strategies declare typed config in their manifest; users
  configure via YAML; splicer validates at splice time; strategies
  read at runtime. Same story as tier-1/2 today.

## Non-goals

- Removing or changing the existing tier-1/2 substrate. The provider
  composition path stays as-is; this is purely about teaching the
  tier-3/4 wrapper to claim the substrate as a consumer.

## References

- `wit/builtin-config/world.wit` — substrate definition.
- `src/config_provider.rs` — provider builder + validation.
- `src/adapter/typed/{target_wit,assemble}.rs` — wrapper crate
  generation.
- `splicer-tool-sdk/src/strategy.rs` — singleton macro.
- `builtins/hello-tier1/{src/lib.rs,build.rs,manifest.toml}` — the
  tier-1 reference impl of the substrate consumer pattern.
- `docs/TODO/bound-mismatch-skip-and-warn.md` — composes with this
  (config-time knobs reduce the need for compile-time fallback).
