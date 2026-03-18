# Splicer Demo Plan

## Goal

A self-contained `cargo run --example demo` that shows:

1. **Basic splice** — apply `before` / `between` rules to a composition graph and print the
   generated WAC to the terminal.
2. **Type-compatibility checking** — demonstrate all three contract outcomes: `Warn` (no path),
   `Ok` (fingerprints match), and `Error` (fingerprints incompatible).

Modelled after `cviz`'s `cargo run --example demo`: no external tools required, everything
runs at `cargo run` time.

---

## Approach

### Composition graph side

Use the JSON graph format (same fixture data as the tests).  A helper
`add_chain_fingerprint` stamps a stable fake fingerprint onto the chain so the type-checking
path is exercised.

### Middleware side (type checking)

Use `validate_contract` directly with a **pre-populated export cache** (`checked_middlewares`).
This avoids needing real WASM files for the demo while clearly illustrating the API behaviour.
A follow-up phase (Phase 3) can upgrade to real WAT-compiled components.

---

## Scenarios

### Phase 1 — Basic splice (no type info)

| # | Graph | Rule | Expected WAC |
|---|-------|------|--------------|
| 1a | One-service (srv-b) | `before` srv-b with `mw-a` | mw-a wraps srv-b |
| 1b | Short chain (srv-b → srv) | `before` srv-b with `mw-a` | mw-a inserted before srv-b |
| 1c | Long chain (srv-c → srv-b → srv) | `between` srv-b / srv with `mw-a`, `mw-b` | both mws between srv-b and srv |

Print header, generated WAC, and a one-line summary for each scenario.

### Phase 2 — Type-compatibility checking

All scenarios use a fingerprinted short chain (one non-host import on
`wasi:logging/log@0.1.0`).  The pre-populated middleware cache stands in for
real WASM discovery.

| # | Middleware cache | Injection path | Expected outcome |
|---|-----------------|----------------|-----------------|
| 2a | No entry (empty cache, no path) | `None` | `Warn` — cannot validate |
| 2b | Entry with matching fingerprint | `None` (cache hit) | `Ok` — types compatible |
| 2c | Entry with mismatching fingerprint | `None` (cache hit) | `Error` — incompatible types |

Print a summary line per scenario showing the `ContractResult` variant and
message.  The `Error` case is caught and printed without panicking so the demo
continues.

### Phase 3 — Full pipeline with real WAT (future)

- Add `wat` as a `dev-dependency`.
- Author two minimal WAT middleware components (compatible and incompatible
  signatures for `wasi:logging/log@0.1.0`).
- Write them to temp files; pass the paths as `Injection.path`.
- Call `generate_wac` end-to-end — exercises the full `discover_middleware_exports` path.
- Store WAT sources under `demo/wat/`.

---

## Files to create / change

- [x] `src/lib.rs` — re-exports `contract`, `parse`, `split`, `wac` so the example can access them
- [x] `examples/demo.rs` — demo entry point + inline `#[test]` functions
- [x] `Cargo.toml` — `[lib]` target + `[[example]]` entry (name = `"demo"`)
- [ ] `demo/` directory — (Phase 3) WAT sources

## Running the demo tests

The example's `#[test]` functions are picked up by:

```
cargo test --all-targets        # runs all tests including the example
cargo test --example demo       # runs only the demo tests
cargo run --example demo        # runs the demo interactively
```

CI should use `--all-targets` (or add a separate `--example demo` step) to ensure the
demo is not silently broken.

---

## Checklist

### Phase 1 — Basic splice
- [x] Add `[lib]` + `[[example]]` blocks to `Cargo.toml`
- [x] Create `src/lib.rs` with public module re-exports
- [x] Create `examples/demo.rs` with `header` / `subheader` helpers (mirror cviz style)
- [x] Inline log-chain JSON graph fixtures as `const` strings
- [x] Inline YAML rule configs per scenario
- [x] Scenario functions return WAC string (pure, no side effects)
- [x] `main()` prints scenarios 1a, 1b, 1c
- [x] `#[test]` for each Phase 1 scenario

### Phase 2 — Type-compatibility checking
- [x] `show_contract_result` printer
- [x] Scenario functions return `Vec<ContractResult>` (pure, no side effects)
- [x] Scenario 2a: Warn (no path, empty cache)
- [x] Scenario 2b: Ok (matching fingerprint in cache)
- [x] Scenario 2c: Error (mismatching fingerprint in cache)
- [x] `#[test]` for each Phase 2 scenario

### Phase 3 — Full pipeline with WAT (future)
- [ ] Add `wat = "1"` to `[dev-dependencies]`
- [ ] Write `demo/wat/log-middleware-compatible.wat`
- [ ] Write `demo/wat/log-middleware-incompatible.wat`
- [ ] `run_type_check_full` helper: compile WAT → temp file → `generate_wac` → print diagnostics
- [ ] Scenario 3a: compatible WAT middleware → Ok  (exercises full `discover_middleware_exports` path)
- [ ] Scenario 3b: incompatible WAT middleware → Error
