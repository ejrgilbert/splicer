# Builtin config substrate — `splicer:builtin-config`

## Motivation

Most planned tier-1 builtins beyond `hello-tier1` and `otel-bare-spans`
need user-facing config:

- `otel-metrics` aggregation: `buffer` size, `flush_after_seconds`
- `rate-limit`: rate, burst, scope
- `deny-list`: list of `(interface, function)` rules
- `chaos`: failure probability, latency injection range

Without a shared substrate, every builtin reinvents config — environment
variables, hardcoded `env!`-baked constants, or its own one-off WIT
interface. That fragments the operator's mental model (config lives in
N places) and makes splicer's YAML splice-config schema increasingly
incomplete: the user writes `inject: { builtin: rate-limit }` and then
has to discover, separately, that they also need to set
`SPLICER_RATE_LIMIT_RPS=...` somewhere else at deploy time.

Goal: one substrate that lets a user write configuration **next to the
inject rule** in splice-config YAML, sealed into the spliced component
at splice time so it travels with the artifact.

## Decision

**Splice-time-sealed config via a codegenned provider component
exporting a string-based custom WIT interface (`splicer:builtin-config`).**

- Splicer reads `inject: { config: { ... } }` from YAML
- Splicer produces (per-inject-site) a tiny "config provider" component
  that exports `splicer:builtin-config/get` with the configured values
- Splicer composes the provider with the builtin: builtin imports
  `splicer:builtin-config`, the provider satisfies that import
- Builtin reads its config at init via `OnceLock`, parses string values
  to typed config (u32, f64, etc.), runs.

The substrate is **only** built and wired when the builtin imports
`splicer:builtin-config`. Existing builtins (`hello-tier1`,
`otel-bare-spans`, current `otel-metrics`) don't import it and are
unaffected.

## Design space considered

| Path                    | WIT interface          | Codegen needed? | Config in YAML? |
|-------------------------|------------------------|-----------------|-----------------|
| 1 (chosen)              | `splicer:builtin-config` | yes             | yes (sealed)    |
| 2                       | `wasi:config/runtime`  | yes             | yes (sealed)    |
| 3                       | `wasi:config/runtime`  | no              | no (host-provided) |
| 4 (degenerate — skip)   | `splicer:builtin-config` | no            | no — no host implements `splicer:`           |

Path 3 (host-provided `wasi:config/runtime`) was the cheapest to
implement (no splicer plumbing) but pushes config out of YAML and into
host deploy-time config — splitting the operator's source of truth.
Rejected.

Path 2 was viable but adds a moving WASI proposal to splicer's
dependency surface (we already pin `wasi:otel@0.2.0-rc.2` and accept
that risk for that one) and creates conflict surface — if the wrapped
service also imports `wasi:config/runtime` the codegenned provider
would by default intercept its imports too. Manageable with careful
WAC composition, but extra plumbing for no concrete benefit at this
stage.

Path 1 keeps the design under our control. Migration to path 2 if
`wasi:config/runtime` stabilizes nicely later is a WIT name change, not
a redesign — the shape is identical.

## Why string-based (not typed records)

A typed-record interface (`get-config: func() -> record { buffer: u32,
flush-after-seconds: f64, ... }`) would be more ergonomic inside the
builtin (no parse, type-safe), but it forces one of two choices:

- **One central WIT** with one record per builtin → editing the
  shared interface every time a builtin is added.
- **Per-builtin custom WIT** with general codegen → splicer has to
  produce a wasm component exporting an arbitrary WIT shape.

Strings sidestep both. The codegen template is a single component
parameterized only by a `(key, value)` table; every builtin uses the
same provider shape. Per-builtin string parsing at init is unavoidable
anyway because YAML scalars arrive as strings.

The cost: a few microseconds of init-time parsing per builtin. Steady-
state hot-path cost is identical to a typed interface (config is
cached in `OnceLock` after init).

## WIT shape

```wit
package splicer:builtin-config@0.1.0;

interface get {
    /// Look up a config value by key. Returns `none` if the key was
    /// not set in the splice-config; the caller falls back to its
    /// own default.
    get: func(key: string) -> option<string>;
}

world provider {
    export get;
}
```

Mirrors the shape of `wasi:config/runtime` so a future migration is a
package rename.

## Architecture

### Provider template

`builtins/config-provider/` — a Rust crate built like other builtins
via `make build-builtins`. Exports `splicer:builtin-config/get`,
reads its key-value table from a known custom data section
(`__splicer_config_table` or similar). The template's `get`
implementation parses the table at init into a `HashMap<String,
String>` and looks up keys.

Pre-built template lives at `assets/builtins/config-provider.wasm`,
embedded in the splicer binary the same way other builtins are.

### Splice-time patcher

New module `src/config_provider.rs`:

```rust
pub fn build_provider(values: &BTreeMap<String, String>) -> Result<Vec<u8>>;
```

- Loads template bytes from the embedded provider
- Serializes `values` to bytes (simple framed key-value format —
  length-prefixed strings; no need for serde)
- Walks the wasm module, locates the data section by name, replaces
  its contents with the serialized table
- Returns the patched component bytes

This avoids hand-rolling wasm-encoder generation per splice. The
template is built once at splicer build time; per-splice work is one
data-section swap.

### Composition wiring

When the splice pipeline is materializing a builtin that imports
`splicer:builtin-config`:

1. Read the inject rule's `config:` block (if any; empty otherwise)
2. Call `build_provider(values)` → get the patched provider bytes
3. Write the provider to the splits dir alongside the builtin and the
   tier-1 adapter (e.g., `<splits>/builtins/<inject-id>-config.wasm`)
4. Add the provider to the wac compose graph
5. Wire the provider's `splicer:builtin-config/get` export to the
   builtin's same-named import

Each inject site gets its own provider component (configs for two
injections of the same builtin don't collide).

## YAML shape

```yaml
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
      provider: { name: my-service }
    inject:
      - builtin: otel-metrics
        config:
          buffer: 100
          flush_after_seconds: 10.0
```

YAML scalars (numbers, booleans) are stringified at parse time before
being handed to the provider. Lists and maps are not supported by the
substrate directly — a builtin that wants list-shaped config (e.g.,
`deny-list`'s rule list) encodes its own format inside a single string
value (newline-separated, JSON, etc.) and parses on its end.

### Parser changes

`parse::config::Injection` gets:

```rust
#[serde(default)]
config: BTreeMap<String, serde_yaml::Value>,
```

Stringified at parse time (not deserialize time, so we preserve the
original scalar type for error messages).

## Implementation pieces

1. **WIT**: `wit/builtin-config/world.wit` defining
   `splicer:builtin-config@0.1.0`. Add to all consuming builtins'
   `wkg.toml` overrides like `splicer:tier1` / `splicer:common`.

2. **Provider template**: `builtins/config-provider/` Rust crate. Exports
   `splicer:builtin-config/get`, reads its KV table from a known custom
   data section. Built via `make build-builtins`.

3. **Splice-time patcher**: `src/config_provider.rs` with
   `build_provider(values) -> Vec<u8>`.

4. **YAML schema**: extend `parse::config::Injection` with optional
   `config:` map; plumb into the splice pipeline.

5. **Composition wiring**: detect `splicer:builtin-config` in the
   builtin's imports, generate + write + wac-wire the provider.

6. **Tests**:
    - Unit test for the patcher (right key returns right value, missing
      key returns `none`, malformed table fails cleanly)
    - Integration test for an end-to-end splice with a config-consuming
      builtin

7. **First consumer**: `otel-metrics` aggregation rework — see the
   `TODO(aggregation)` block in `builtins/otel-metrics/src/lib.rs`.

## Sequencing

**Hold until tier2 lands.** The substrate's composition wiring (item 5)
sits in the same adapter/composition pipeline that tier2 codegen is
actively touching. Building in parallel guarantees merge conflict;
branching off the tier2 branch trades that for repeated rebases against
a moving target. Cleanest path: tier2 → main, then config substrate
as a fresh branch off main.

In the meantime, `otel-metrics` runs in always-flush (`buffer = 1`)
mode with hardcoded defaults. The `TODO(aggregation)` block in its
`lib.rs` lists the config keys and semantics so the rework is mechanical
once the substrate is in place.

## Defaults / missing-key semantics

- Builtin reads each config key at init via the imported `get` function
- If `get(key)` returns `none`, builtin falls back to its hardcoded
  default
- If the splice-config has no `config:` block at all for a given inject
  rule, `build_provider` is still called with an empty `BTreeMap` —
  every `get` returns `none` — and the builtin's defaults apply
  uniformly. (This means we always wire a provider when the builtin
  imports `splicer:builtin-config`; there's no branching on
  "config-present-or-not" in the pipeline.)
- Schema validation (typed parse, range checks) lives in the **builtin**,
  not in splicer. Splicer does not know what keys a given builtin
  accepts. Trade-off: invalid config surfaces as a runtime init
  error from the builtin, not a YAML parse error from splicer.
  Acceptable for now; reconsider if multiple builtins share enough
  config shape to warrant a schema declaration.

## Future migration path to wasi:config/runtime

If the WASI proposal stabilizes and we want to switch:

1. Bump consuming builtins' WIT imports from `splicer:builtin-config`
   to `wasi:config/runtime`
2. Bump the provider template's WIT export the same way
3. The patcher and pipeline are unchanged — they operate on bytes and
   compose graphs, not on the interface name

The user-facing YAML and the architecture are identical. This is a
WIT-level rename, not a redesign.

## Open questions

- **Per-builtin keyspace**: do we namespace keys (e.g.,
  `otel-metrics.buffer` to avoid two co-injected builtins fighting over
  the same key name) or assume single-config-per-inject-site is enough?
  Current design: per-inject-site provider, so co-injected builtins
  each get their own provider — no collision, no namespacing needed.
  Document this clearly so a builtin author doesn't try to share keys.

- **Reserved keys**: any meta keys (e.g., `_builtin_version` for
  compat checks)? Probably none for v0.1; revisit if version skew
  becomes a real problem.

- **Multi-instance configs**: confirmed independent — see above.

- **Config evolution within a builtin**: if a builtin renames a config
  key in v0.2, splicer has no way to migrate the user's YAML
  automatically. Builtins should treat keys as part of their public
  API and add new keys before removing old ones.
