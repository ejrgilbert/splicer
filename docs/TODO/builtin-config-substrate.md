# Builtin config substrate — `splicer:builtin-config`

> **Status:** landed. The substrate ships as designed below, with one
> deviation: the body talks about builtins (including the provider
> template) being **embedded in the splicer binary**, but builtins now
> ship as OCI artifacts under
> `ghcr.io/ejrgilbert/splicer/builtins/<name>:<version>` and are
> resolved at splice time via local override → on-disk cache → OCI
> pull (see [`src/builtins.rs`](../../src/builtins.rs) for the
> resolver). The patching mechanism is the same; only the bytes'
> origin changed.
>
> User-facing YAML reference lives in
> [`docs/splice-config.md`](../splice-config.md); the worked example
> is `--hello-builtin-config` in
> `tests/component-interposition/run.sh`. The first non-trivial
> consumer — the `otel-bare-metrics` aggregation rework — has now
> landed: see
> [`builtins/otel-bare-metrics/README.md`](../../builtins/otel-bare-metrics/README.md)
> for the `buffer` + `flush_after_seconds` keys it now reads. The rest
> of this file is preserved as the design rationale — particularly the
> "Why string-based (not typed records)" and "Path 1 vs path 2 vs path
> 3" tradeoffs — so future contributors can reason about boundary
> cases without re-litigating them.

## Motivation

Most planned tier-1 builtins beyond `hello-tier1` and `otel-bare-spans`
need user-facing config:

- `otel-bare-metrics` aggregation: `buffer` size, `flush_after_seconds`
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
`otel-bare-spans`, current `otel-bare-metrics`) don't import it and are
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
      - builtin: otel-bare-metrics
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

1. ✅ **WIT** — `wit/builtin-config/world.wit` defines
   `splicer:builtin-config@0.1.0` (single `get` interface). Consuming
   builtins point at it from their `wkg.toml`; see
   [`builtins/hello-tier1/wkg.toml`](../../builtins/hello-tier1/wkg.toml)
   for the override pattern.

2. ✅ **Provider template** — [`builtins/config-provider/`](../../builtins/config-provider/)
   Rust crate. Exports `splicer:builtin-config/get`. Reads its KV
   table from a fixed-capacity `static` buffer (`SPLICER_CONFIG_BLOB`)
   in linear memory rather than a custom section; the buffer is
   prefixed with a magic sentinel so the splicer-side patcher can
   locate it by byte-scan after `wasm-tools component new`. Built
   via `make build-builtins`.

3. ✅ **Splice-time patcher** — [`src/config_provider.rs`](../../src/config_provider.rs)
   exposes `build_provider(values) -> Vec<u8>` (loads the template
   via the same OCI/cache/override resolver as user-facing builtins,
   then in-place rewrites the magic-anchored window). Shared
   wire-format constants + codec live in
   [`builtins/config-provider/src/wire_format.rs`](../../builtins/config-provider/src/wire_format.rs),
   `mod wire_format`-included from both crates so the patcher and
   the runtime parser can't drift.

4. ✅ **YAML schema** — `parse::config::Injection` carries
   `builtin_config: BTreeMap<String, String>`; the YAML's
   `builtin.config:` map gets stringified at parse time (scalars
   only; lists, maps, nulls, and tags are rejected with a clear
   error).

5. ✅ **Composition wiring** — `ensure_provider_for` in
   `src/config_provider.rs` decodes each materialized builtin's WIT
   imports; if `splicer:builtin-config` is among them, it patches +
   writes the provider to `<splits>/builtins/<inject-id>-config.wasm`
   and stamps the path on the injection. `create_tier1_mdl` in
   `src/wac.rs` emits the provider's WAC instance and wires its
   `get` export into the consuming builtin's import. As a misconfig
   guard, setting `config:` on a non-consumer builtin errors at
   splice time rather than silently dropping the values.

6. ✅ **Tests** — the patcher's wire-format invariants
   (`round_trip_empty`, `round_trip_multi_key`, `overflow_rejects`,
   missing/duplicate magic, etc.) and the substrate-wiring path
   (`ensure_provider_for_*`) live in
   [`src/config_provider.rs`](../../src/config_provider.rs)'s test
   module. Two `#[ignore]`d regression tests run against the actual
   built provider when invoked with
   `SPLICER_BUILTINS_DIR=assets/builtins`:
   `built_provider_has_unique_magic` (catches the rustc/lld
   duplicate-MAGIC emission bug from earlier iterations) and
   `hello_tier1_smoke` (confirms a real shipped builtin trips
   `imports_substrate` and produces a patched provider end-to-end).
   The component-interposition submodule's
   `--hello-builtin-config` mode stacks two `hello-tier1` instances
   (one defaulted, one configured) and diffs stdout against
   `expected-output/hello-builtin-config.txt`, which is the
   load-bearing end-to-end check.

7. ✅ **First consumer**: `otel-bare-metrics` aggregation rework — the
   builtin now imports `splicer:builtin-config/get` and reads
   `buffer` (u32, default 1) + `flush_after_seconds` (f64, default
   10.0) at first observation, accumulating count + histogram
   (sum/min/max/buckets) per `(iface, fn)` and flushing when either
   threshold trips. With the default `buffer = 1` the behavior is
   identical to the prior always-flush mode. See
   [`builtins/otel-bare-metrics/README.md`](../../builtins/otel-bare-metrics/README.md).

## Sequencing

Substrate landed off main with the tier-2 work already in place, so
there were no merge-conflict surprises versus the original sequencing
plan. The `otel-bare-metrics` aggregation rework followed as the
first non-trivial consumer (item 7).

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
  `otel-bare-metrics.buffer` to avoid two co-injected builtins fighting over
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

## Future enhancements

### Discoverable config schemas

What landed: each builtin documents its supported keys + defaults in
`builtins/<name>/README.md` (e.g.
[`builtins/hello-tier1/README.md`](../../builtins/hello-tier1/README.md)).
Splice-config docs point users at the per-builtin README. Splicer
itself doesn't type-check YAML values against the schema; an unknown
key passes parse time and is ignored at runtime.

The gap: discovery is human-only ("read the README") and validation is
runtime-only ("the builtin's init parse will reject malformed values").
A user with a typo in a key name (or value type) doesn't find out
until they actually run the spliced wasm.

Two plausible follow-ups in increasing scope:

1. **CLI introspection.** A `splicer help builtin <name>` (or
   `splicer describe-builtin <name>`) subcommand that prints the
   README — same source of truth, lower friction. Pure additive,
   no schema format needed.

2. **Structured manifest + splice-time validation.** Each builtin
   ships a `config.toml` (or sibling WIT custom section)
   declaring `{ key, type, default, doc }` per config entry.
   Splicer reads it at YAML parse time and:
   - Rejects unknown keys with a clear error (catches typos).
   - Type-checks scalars against the declared type
     (`buffer: "ten"` errors before runtime).
   - Backs `splicer describe-builtin <name>`.

   The "Defaults / missing-key semantics" section above deliberately
   punted on this for v0.1; revisit once a few real-world consumers
   exist and we know whether the typo-protection is worth the schema
   maintenance cost. The substrate's design doesn't change either
   way — only the YAML pre-validation does.
