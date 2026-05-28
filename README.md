# `splicer` 🔍✂️🪡

**Plan and generate middleware splice operations for WebAssembly component composition graphs.**

`splicer` reads:

* A **component binary** as `.wasm` (the composition or a single component)
* A **splice configuration** (YAML)

It produces a new composed `.wasm` (or the underlying plan pieces) that injects middleware components according to declarative rules.

`splicer` is **interface-agnostic** — it operates on any WIT interface edge in a component graph.
The same splice rules apply whether the targeted interface is `wasi:http/handler`, `wasi:cli/run`, `my:app/orders`, or a custom in-house contract.

---

# Why splicer?

When building component-based systems, middleware insertion often requires:

* Rewriting instantiation chains
* Re-threading handler references
* Maintaining correct edge ordering
* Traversing nested provider chains

`splicer` automates that planning step.

Instead of manually restructuring component wiring, you define declarative rules on:

* _which interface_ to target,
* _what middleware_ to wire in, and
* _where to invoke_ the middleware.

`splicer` either produces the newly composed `.wasm` directly, or (with `--plan`) emits the underlying plan pieces (generated `wac` + split sub-components) for inspection or manual composition.

`splicer` operates directly on the component **binary** (no source code, and no original `wac` even when the input is itself a composition). The graph is **discovered** from the binary alone.

---

# Adapter Components

Most middleware doesn't need to match the exact type signature of the interface
it's being placed on. A logging middleware that prints "before" and "after"
around every call works the same whether the target interface is
`my:service/adder`, `wasi:cli/run`, or `wasi:http/handler`; it only needs
the function name.

Splicer generates **adapter components** that bridge between a generic
middleware WIT interface and the specific target interface. The middleware author
writes against a simple contract; splicer handles all the type plumbing at
composition time.

## Middleware Tiers

| Tier       | Data access         | Calls downstream? | Capability                                                                              | Contract                                                    | Status        |
|------------|---------------------|-------------------|-----------------------------------------------------------------------------------------|-------------------------------------------------------------|---------------|
| **Tier 1** | none (call-id only) | yes _(skippable)_ | **Hooks**: middleware sees the call identity but _not types or data_                    | [`wit/tier1/world.wit`](wit/tier1/world.wit)                | **Supported** |
| **Tier 2** | read-only           | yes               | **Observe**: middleware sees the typed values flowing through; _cannot modify_          | [`wit/tier2/world.wit`](wit/tier2/world.wit)                | **Supported** |
| **Tier 3** | read + write        | yes               | **Transform**: middleware sees AND modifies the values; _downstream is still called_    | [`splicer_tool_sdk::TransformStrategy`](splicer-tool-sdk/)  | **Supported** |
| **Tier 4** | read + write        | no                | **Virtualize**: middleware _replaces the downstream_ entirely (mocks, virts, replayers) | [`splicer_tool_sdk::VirtualizeStrategy`](splicer-tool-sdk/) | **Supported** |

Each tier strictly adds one capability. Middleware written for a lower tier
works unchanged when higher tiers become available.

"Skippable" (tier 1) vs "no" (tier 4) are different things. With
`should-block` the adapter still **generates** the downstream call and
asks the middleware at runtime whether to invoke it. It's a per-call gate.
With virtualization the downstream call **is not in the adapter at
all**; it cannot be reached, regardless of runtime state.

Tier-1 and tier-2 middleware are **components**: your wasm exports one
or more of the interfaces defined in the relevant tier WIT world (e.g.
[`wit/tier1/world.wit`](wit/tier1/world.wit)). Tier-2 hooks receive
arguments and results lifted into a structural `field-tree` (defined in
[`wit/common/world.wit`](wit/common/world.wit)), so observation middleware
can inspect typed values without depending on the target interface's
concrete types.

When `splicer splice` detects that a middleware exports these interfaces (instead
of the target interface directly), it automatically generates an adapter
component and wires it into the composition.

Tier-3 and tier-4 middleware are **Rust strategy crates** implementing
[`TransformStrategy`](splicer-tool-sdk/) or
[`VirtualizeStrategy`](splicer-tool-sdk/) from
[`splicer-tool-sdk`](splicer-tool-sdk/). Splicer codegens a per-target
wrapper at splice-time; the wrapper is the adapter. See
[tier-3](docs/tiers/tier-3.md) / [tier-4](docs/tiers/tier-4.md).

Tier-3/4 currently ships only via builtins. User-form tier-3/4 (point
splicer at your own strategy crate) is planned.

For Rust authors, [`splicer-tool-sdk`](splicer-tool-sdk/) ships
ready-made building blocks for middleware and downstream tools. Common
operations on lifted typed values live in one place, so your middleware
and any consuming tools (decoders, replay drivers, fixture sanitizers)
get them for free instead of each crate re-implementing them.

For the full guide — including how to write a middleware, how adapter
detection works, and what the generated adapter does internally — see
[docs/adapter-components.md](docs/adapter-components.md).

---

# Builtins

Splicer ships middleware as builtins, referenced by name in a splice
config:

```yaml
inject:
  - builtin: hello-tier1
```

Tier-1/2 builtins are pre-built wasm fetched on demand from
`ghcr.io/ejrgilbert/splicer/builtins/*`. Tier-3/4 builtins are Rust
strategy crates embedded in splicer's binary; the wrapper is codegen'd
and compiled per-target at splice-time (requires `cargo` and
`wasm32-wasip1` on PATH).

| Name                                               | Tier | Description                                                                      |
|----------------------------------------------------|------|----------------------------------------------------------------------------------|
| [`hello-tier1`](builtins/hello-tier1/)             | 1    | `println!`s every wrapped call. Verifies splice rules fire.                      |
| [`hello-tier2`](builtins/hello-tier2/)             | 2    | `println!`s every wrapped call with lifted arg + result values.                  |
| [`hello-tier3`](builtins/hello-tier3/)             | 3    | Pass-through transform; `println!`s before/after each wrapped call.              |
| [`hello-tier4`](builtins/hello-tier4/)             | 4    | Returns `R::default()` instead of forwarding. Requires `R: Default`.             |
| [`otel-bare-spans`](builtins/otel-bare-spans/)     | 1    | Emits a `wasi:otel` span per call (timing + call-id attrs, no payload).          |
| [`otel-bare-metrics`](builtins/otel-bare-metrics/) | 1    | Emits `wasi:otel` count + duration-histogram metrics (no payload).               |
| [`otel-bare-logs`](builtins/otel-bare-logs/)       | 1    | Emits a structured `wasi:otel` log per call (configurable severity, no payload). |

See [docs/splice-config.md](docs/splice-config.md#inject-entry-shapes)
for the full `builtin:` schema (short + long forms, the `config:` block,
and the local-override → cache → OCI resolution order).

The CLI includes helpful information on builtins. Run: `splicer builtin` to view. 

---

# Build + Install

From [crates.io](https://crates.io/crates/splicer):

```bash
cargo install splicer
```

Or as a library dependency:

```toml
[dependencies]
splicer = "2"
```

The library entry point is `splicer::splice(SpliceRequest) -> Bundle`;
`examples/wac_compose.rs` is a runnable end-to-end demo.

From source (for development):

```bash
cargo build --release
# binary at target/release/splicer
```

Builtin source crates live under [`builtins/`](builtins/). You don't
need to build them to use splicer, they're pulled from
`ghcr.io/ejrgilbert/splicer/builtins/*` on demand. To rebuild local
artifacts (for iterating on a builtin without re-publishing), run:

```bash
make build-builtins
```

Builds land in `assets/builtins/`; point `SPLICER_BUILTINS_DIR` at that
directory to short-circuit the OCI pull.

To kick the tires, `cargo run --example demo` runs a self-contained
demo; for a fuller walkthrough see the external
[`component-interposition`](https://github.com/ejrgilbert/component-interposition)
repo.

---

# Configuration Format

Splicing behavior is defined in a YAML configuration file:

```yaml
version: 1
rules:
  - before:
      interface: my:app/orders
      provider:
        name: validate
    inject:
      - builtin: hello-tier1
```

`interface` and the node-name fields (`provider`/`inner`/`outer`) accept
glob patterns like `wasi:*` and lists like `["wasi:*", "my:*"]`; the
node-name fields are optional (omitted matches any).

See [`docs/splice-config.md`](docs/splice-config.md) for the full
specification.

`splicer` is a multi-subcommand CLI; run `splicer --help` to see
what's available.

---

# Testing

Unit tests cover the adapter generator, WAC emitter, and composition
planner: `cargo test --lib`.

End-to-end coverage lives in `tests/fuzz_and_run.rs`. It scaffolds
provider/consumer/middleware crates, drives them through the full
splicer pipeline (compose + splice, `before` and `between`), and
invokes the result under `wasmtime`. Two entry points (both
`#[ignore]`'d — they build real crates):

- `test_canned` — a hardcoded catalog of value-type shapes crossed
  with async modes and split-kind pipelines. Deterministic; the
  regression canary.
- `test_fuzz` — `arbitrary`-driven random shapes, reproducible via
  `SPLICER_FUZZ_SEED` (replay any failing iter by re-running with
  its seed and `SPLICER_FUZZ_ITERS=1`).

```bash
cargo test --test fuzz_and_run -- --ignored --nocapture test_canned
cargo test --test fuzz_and_run -- --ignored --nocapture test_fuzz
```

Env knobs:

| var                   | default      | effect                                                  |
|-----------------------|--------------|---------------------------------------------------------|
| `SPLICER_FUZZ_SEED`   | `0xDEADBEEF` | base RNG seed; each iter's shape uses `seed + iter_idx` |
| `SPLICER_FUZZ_ITERS`  | 30           | iterations per async mode (sync + async both run)       |
| `SPLICER_FUZZ_DEPTH`  | 4            | max recursion depth for compound shapes                 |
| `SPLICER_KEEP_TMPDIR` | unset        | preserve the tempdir for post-mortem inspection         |
