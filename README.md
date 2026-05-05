# `splicer` 🔍✂️🪡

**Plan and generate middleware splice operations for WebAssembly component composition graphs.**

`splicer` reads:

* A **composition graph** (JSON)
* A **splice configuration** (YAML)

It produces a modified plan that injects middleware components according to declarative rules.

This tool is designed to work with component-based systems such as WASI HTTP services, but is interface-agnostic and can splice across any interface edge in a component graph.

---

# Why splicer?

When building component-based systems, middleware insertion often requires:

* Rewriting instantiation chains
* Re-threading handler references
* Maintaining correct edge ordering
* Traversing nested provider chains

`splicer` automates that planning step.

Instead of manually restructuring component wiring, you define:

* What interface to target
* Where to inject middleware
* What middleware components to insert

And `splicer` generates the modified composition plan.

A demo of `splicer` can be run using: `cargo run --example demo`

A more in-depth usage of `splicer` is done in the external [`component-interposition`](https://github.com/ejrgilbert/component-interposition) repo.

---

# Adapter Components

Most middleware doesn't need to match the exact type signature of the interface
it's being placed on. A logging middleware that prints "before" and "after"
around every call works the same whether the target interface is
`wasi:http/handler` or `my:service/adder`, it only needs the function name.

Splicer generates **adapter components** that bridge between a generic
middleware WIT interface and the specific target interface. The middleware author
writes against a simple contract; splicer handles all the type plumbing at
composition time.

### Middleware Tiers

| Tier       | Capability                                                                                                              | WIT                                          | Status        |
|------------|-------------------------------------------------------------------------------------------------------------------------|----------------------------------------------|---------------|
| **Tier 1** | Hook (name only) — `on-call`, `on-return`, `should-block`: middleware sees the call identity but not types or data      | [`wit/tier1/world.wit`](wit/tier1/world.wit) | **Supported** |
| **Tier 2** | Observe — middleware sees the typed values flowing through (lifted into a structural attribute tree); cannot modify     | `wit/tier2/world.wit` (planned)              | Planned       |
| **Tier 3** | Transform — middleware sees AND modifies the values; downstream is still called                                         | `wit/tier3/world.wit` (planned)              | Planned       |
| **Tier 4** | Virtualize — middleware replaces the downstream entirely (mocks, virts, replayers)                                      | `wit/tier4/world.wit` (planned)              | Planned       |

Each tier strictly adds one capability. Middleware written for a lower tier
works unchanged when higher tiers become available.

To write a tier-1 middleware, your component exports one or more of the
interfaces defined in [`wit/tier1/world.wit`](wit/tier1/world.wit).

When `splicer splice` detects that a middleware exports these interfaces (instead
of the target interface directly), it automatically generates an adapter
component and wires it into the composition.

For the full guide — including how to write a tier-1 middleware, how adapter
detection works, and what the generated adapter does internally — see
[docs/adapter-components.md](docs/adapter-components.md).

### Builtin Middleware

Splicer ships pre-built middleware components embedded in the binary.
Reference one from a splice config without supplying a path:

```yaml
inject:
  - builtin: hello-tier1
```

| Name              | Tier | Description                                                                  |
|-------------------|------|------------------------------------------------------------------------------|
| `hello-tier1`     | 1    | `println!`s every wrapped call. Verifies splice rules fire.                  |
| `otel-bare-spans` | 1    | Emits a `wasi:otel` span per call (timing + call-id attrs).                  |
| `otel-metrics`    | 1    | Emits `wasi:otel` count + duration-histogram metrics per call.               |
| `otel-logs`       | 1    | Emits a structured `wasi:otel` log record per call (severity `INFO`).        |

Source crates live under [`builtins/`](builtins/); rebuild artifacts
with `make build-builtins`.

See [docs/splice-config.md](docs/splice-config.md#inject-entry-shapes)
for the full `builtin:` schema (short + long forms).

---

# Installation

From source:

```bash
cargo build --release
```

Binary will be located at:

```
target/release/splicer
```

---

# Usage

```bash
splicer <JSON_GRAPH> <SPLICE_CFG> [--output <FILE>]
```

### Arguments

| Argument     | Description                                  |
| ------------ | -------------------------------------------- |
| `JSON_GRAPH` | Path to the composition graph in JSON format |
| `SPLICE_CFG` | Path to the splice configuration YAML file   |
| `--output`   | Optional output file (defaults to stdout)    |

---

# Configuration Format

Splicing behavior is defined in a YAML configuration file.

See full specification:

```
docs/splice-config.md
```

---

# Example Configuration

```yaml
version: 1

rules:
  - before:
      interface: wasi:http/handler
      provider:
        name: auth
    inject:
        - middleware-a
        - middleware-b

  - between:
      interface: wasi:http/handler
      inner:
        name: auth
      outer:
        name: handler
    inject:
        - tracing
```

---

# Splice Semantics

`splicer` operates on interface edges in the graph.

If no matches are found, the generated `wac` will produce an identity component (roundtrips to same component).

Two matching modes are supported:

## 1. Single-Target Injection

Inject middleware for a given interface, optionally scoped to a specific provider.

```yaml
before:
  interface: wasi:http/handler
  provider:
    name: auth
```

If `provider.name` is omitted, all providers of that interface are matched.

---

## 2. Between Injection

Inject middleware between two specific components connected via an interface edge.

```yaml
between:
   interface: wasi:http/handler
   inner:
     name: auth
   outer:
     name: handler
```

This replaces:

```
handler → auth
```

With:

```
handler → middleware → auth
```

Middleware chains are traversed in reverse order during injection to preserve declared ordering.

---

# Rule Ordering

Rules are applied in file order.

Later rules operate on the graph after earlier modifications.

This allows intentional stacking:

```
auth → logging → metrics → handler
```

---

# Validation

The configuration will fail if:

* `version` is missing or unsupported
* Required fields are absent
* Middleware list is empty

---

# Testing

In-process unit tests live under `src/` and exercise the adapter
generator, WAC emitter, and composition planner directly:

```bash
cargo test --lib
```

## End-to-end fuzz + run harness

`tests/fuzz_and_run.rs` scaffolds provider, consumer, and middleware
crates in a tempdir, drives them through the full splicer pipeline
(compose + splice for both `between` and `before` rules), and invokes
the result under `wasmtime` to check the composition actually executes.

Two entry points, both `#[ignore]`'d (they build real crates — slow):

* `test_canned` — a hardcoded catalog of 22 value-type shapes * 2
  async modes * 2 split-kind pipelines = 88 combos. Same shapes every
  run. Quick-to-bisect canary for regressions in a known shape.

* `test_fuzz` — `arbitrary`-driven random shapes. Reproducible via
  `SPLICER_FUZZ_SEED` so any failure can be replayed. Each iter
  prints `[i/N]` progress (requires `--nocapture`).

```bash
# Canned catalog — 88 combos, ~2 min
cargo test --test fuzz_and_run -- --ignored --nocapture test_canned

# Fuzz at PR CI config (25 iters × 2 modes × depth 5, ~2 min)
SPLICER_FUZZ_ITERS=25 SPLICER_FUZZ_DEPTH=5 \
  cargo test --test fuzz_and_run -- --ignored --nocapture test_fuzz

# Replay a single failing iteration
SPLICER_FUZZ_SEED=<seed_from_output> SPLICER_FUZZ_ITERS=1 \
  cargo test --test fuzz_and_run -- --ignored --nocapture test_fuzz
```

Env knobs:

| var                   | default      | effect                                                  |
|-----------------------|--------------|---------------------------------------------------------|
| `SPLICER_FUZZ_SEED`   | `0xDEADBEEF` | base RNG seed; each iter's shape uses `seed + iter_idx` |
| `SPLICER_FUZZ_ITERS`  | 30           | iterations per async mode (sync + async both run)       |
| `SPLICER_FUZZ_DEPTH`  | 4            | max recursion depth for compound shapes                 |
| `SPLICER_KEEP_TMPDIR` | unset        | preserve the tempdir for post-mortem inspection         |

---

# Project Structure

```
splicer/
├── src/
├── docs/
│   └── splice-config.md
├── README.md
```

---

# Design Principles

* **Declarative configuration**
* **Deterministic ordering**
* **Interface-driven matching**
* **Graph-aware edge replacement**
* **Middleware-agnostic**

`splicer` does not assume HTTP semantics — it operates on generic interface edges.

---

# Future Evolution

The configuration format is versioned:

```yaml
version: 1
```

Breaking changes will increment the version number.
