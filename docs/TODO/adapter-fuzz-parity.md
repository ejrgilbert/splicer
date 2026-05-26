# Adapter fuzz parity + test-layout refactor

Goal: bring tier-2 and typed up to tier-1's fuzz coverage, and
standardize the test-layout convention across all three adapters
while we're in there.

The integration-test side is already done — `tests/integration.rs`
already invokes `--builtin-hello-tier3` and `--builtin-hello-tier4`
via the existing component-interposition harness (see
`integration_builtin_hello_tier{3,4}` at the bottom of
`tests/integration.rs`). No work needed there.

## Motivation

`src/adapter/tier1/tests/fuzz.rs` has demonstrably paid off — the
first ~440 lines of that file are hand-written regression tests
for bugs the fuzz loop originally surfaced
(`test_adapter_async_5_u32_params_validates`,
`test_adapter_async_mixed_primitives_indirect_params_validates`,
…). The same value proposition applies to tier-2 (canonical-ABI
memory layout, sidetables — more intricate than tier-1) and to
typed (tier-3/4 codegen, currently only covered by hand-enumerated
matrix tests). The comment block in
`src/adapter/tier1/tests/fuzz.rs:34-46` even calls out that the
indirect-params fix applies to both tiers — so we already know
failure modes overlap.

## Current state (snapshot)

The three adapters use inconsistent test layouts:

| Adapter | Layout |
|---|---|
| `tier1/` | `mod.rs` → `mod tests;` → `tests.rs` (1382 lines, declares `mod fuzz;`) → `tests/fuzz.rs`. Plus inline `mod tests` in `emit.rs:1240`. |
| `tier2/` | Inline `mod tests { … }` in `mod.rs:441` (~1700 lines through EOF in a 2144-line file), `cells.rs:668`, `layout.rs:668`. `lift/mod.rs:39` already uses a separate submodule. Has `tier2/test_utils.rs` (16 lines, shared helpers). |
| `typed/` | Top-level `matrix_tests.rs` and `bindgen_contract_tests.rs` as siblings of `mod.rs`. Plus inline `mod tests { … }` at `typed/mod.rs:150`. |

Tier-1 is the most structured. Tier-2 and typed are each ad-hoc
in different ways.

Inside `tier1/tests/fuzz.rs`, the fuzz body uses `use super::*`
to pull in tier-1-specific test helpers from `tier1/tests.rs`:
`make_iface`, `sig`, `gen_adapter`, `SplitKind`, `synth_split`.
Tier-2 and typed will each need their own analogous helpers — the
helpers are tier-specific, not shareable.

What *is* shareable: the LCG (`fuzz_seeded_bytes`), the
env-knob parser (`SPLICER_FUZZ_SEED` / `SPLICER_FUZZ_ITERS`),
and the failure formatter (`MAX_FAILURES_SHOWN` truncation). All
in `tier1/tests/fuzz.rs:23-32, 447-460`. ~50 LOC total.

## Target state

Each adapter follows the tier-1 pattern:

```
src/adapter/
  fuzz_common.rs              # NEW: cfg-test shared helpers
  tier1/
    mod.rs                    # unchanged
    tests.rs                  # unchanged
    tests/
      fuzz.rs                 # unchanged (switch to fuzz_common imports)
  tier2/
    mod.rs                    # remove inline mod tests, add `mod tests;`
    tests.rs                  # NEW: extracted hand-written tests
    tests/
      fuzz.rs                 # NEW
  typed/
    mod.rs                    # remove inline mod tests, add `mod tests;`
    tests.rs                  # NEW
    tests/
      matrix.rs               # MOVED from typed/matrix_tests.rs
      bindgen_contract.rs     # MOVED from typed/bindgen_contract_tests.rs
      fuzz.rs                 # NEW
```

Privacy stays correct because each `tests/fuzz.rs` remains a
descendant of its adapter module, keeping `use super::*` access
to the adapter's internals and tier-local test helpers.

## Steps

Recommended commit boundaries — each step is a discrete reviewable
change. Tier-1 is untouched in steps 1–2.

### Step 1: layout refactor (no behavior change)

1a. **tier2**: lift the three inline `mod tests { … }` blocks.
- `tier2/cells.rs:668..EOF` → `tier2/tests/cells.rs`
- `tier2/layout.rs:668..EOF` → `tier2/tests/layout.rs`
- `tier2/mod.rs:441..EOF` → `tier2/tests/mod_root.rs` (or pick a
  better name based on what's there — it's ~1700 lines, may
  warrant splitting into multiple submodules)
- Add `tier2/tests.rs` declaring the submodules
- Replace each inline `#[cfg(test)] mod tests { … }` with
  `#[cfg(test)] mod tests;` at the parent
- `tier2/lift/mod.rs` already uses a separate submodule — leave
  alone
- `tier2/blob.rs:229` — check whether this is a `mod tests` or a
  single `#[cfg(test)]` item; act accordingly

1b. **typed**:
- `typed/matrix_tests.rs` → `typed/tests/matrix.rs`
- `typed/bindgen_contract_tests.rs` → `typed/tests/bindgen_contract.rs`
- Lift `typed/mod.rs:150`'s inline `mod tests` into
  `typed/tests/mod_root.rs` (or similar)
- Add `typed/tests.rs` declaring the submodules
- Update `typed/mod.rs` to drop the inline `mod tests` and the
  `mod matrix_tests; mod bindgen_contract_tests;` declarations,
  replacing with `#[cfg(test)] mod tests;`

Verify: `cargo test --lib` runs the same set of tests as before
(count + names). No new tests, no removed tests.

### Step 2: extract shared fuzz helpers

Create `src/adapter/fuzz_common.rs` (cfg-test-gated) containing:
- `fuzz_seeded_bytes(seed: u64, len: usize) -> Vec<u8>` (LCG)
- Constants: `DEFAULT_FUZZ_SEED`, `DEFAULT_FUZZ_ITERS`,
  `FUZZ_BYTES_PER_ITER`, `MAX_FAILURES_SHOWN`
- Env-knob parser (`SPLICER_FUZZ_ITERS`, `SPLICER_FUZZ_SEED`)
- A failure-printing helper if there's a clean signature for one

Wire `tier1/tests/fuzz.rs` to import from it instead of defining
its own copies. This is the canary that the shared module works
before tier-2 and typed start consuming it.

### Step 3: tier-2 fuzz harness

New `src/adapter/tier2/tests/fuzz.rs`. Mirror tier-1's structure:
- Top of file: hand-written regression tests for shapes the fuzz
  has surfaced (empty to start; will grow as bugs are found)
- `fn fuzz_value_type(u, arena, depth, need_export) -> ValueTypeId`
  — recursive `ValueType` tree generator (steal tier-1's)
- `fn fuzz_is_expected_bail(msg: &str) -> bool` — tier-2-specific
  expected-bail messages (different from tier-1's; need to read
  through tier-2's bail sites to enumerate)
- `#[test] fn fuzz_structural_shapes` — same loop shape as tier-1,
  calls `generate_tier2_adapter` (or whatever the tier-2 entry
  point is — confirm by reading `tier2/mod.rs`), validates output
  with `wasmparser::Validator`

Open question: does tier-2's adapter generator take the same
`(name, interface_qualified_name, hooks, …)` shape as tier-1's?
Spot-check `generate_tier2_adapter`'s signature first; the helper
that wraps the call probably needs to differ.

### Step 4: typed (tier-3/4) fuzz harness

New `src/adapter/typed/tests/fuzz.rs`. The tricky bit: typed's
codegen output is **Rust source code + a Cargo.toml**, not a wasm
component. So the cheap validation is `syn::parse_file(&lib_rs)`
not `wasmparser::Validator::validate_all(&bytes)`.

Shape of the harness:
- Random `ValueType`-equivalent IR via the typed IR types
  (`src/adapter/typed/ir.rs` — read this first; it explicitly
  rejects resources/handles/futures/streams, so the generator
  needs to stay within value-typed WIT)
- For each iteration, call `generate_wrapper_crate(input)` twice:
  once with `Behavior::Transform`, once with
  `Behavior::Virtualize`
- Assert each returned `WrapperCrate.lib_rs` parses via `syn`
- Assert basic structural properties on the output:
  - Transform output contains the target call site
  - Virtualize output does not import the target interface
    (can grep, or parse the WIT string in the input)

**Async-only constraint:** see
`/Users/evgilber/.claude/projects/-Users-evgilber-git-cosmonic-splicer3/memory/project_sync_wrapping_roadmap.md`
and the comment at `src/adapter/typed/emit_method.rs:13`. Today
the typed codegen unconditionally emits `async fn` Guest method
bodies. Sync WIT funcs produce a wrapper that fails to compile
(E0053). The fuzzer should only generate async-mode functions
until sync-bridging lands; otherwise it'll generate inputs that
the codegen can't legally accept.

**Heavy variant (defer):** a build-and-run typed fuzzer that
actually invokes `cargo build` + composes with a synthesized
strategy crate belongs in `tests/fuzz_and_run.rs`, not in
`typed/tests/fuzz.rs`. The latter is the *cheap* layer — codegen
+ syn parse, runs by default in `cargo test --lib`. The heavy
variant is a separate, larger project (needs strategy-crate
synthesis infrastructure that doesn't exist yet).

## Notes / gotchas

- **Tier-2 has ~1700 lines of inline tests in `mod.rs`**. The
  lift is mechanical but big. Worth eyeballing whether the chunk
  splits naturally into multiple `tier2/tests/<area>.rs` files
  rather than dumping into one `tier2/tests/mod_root.rs`. The
  inline block likely has logical sections (lift/, layout/,
  cells/) that suggest split lines.

- **`tier2/test_utils.rs`** (16 lines) is already cross-test
  shared. Doesn't need to move — `tier2/tests/*.rs` can `use
  crate::adapter::tier2::test_utils::*`. But check whether the
  file makes more sense renamed/moved to `tier2/tests/util.rs`
  for symmetry. Cosmetic.

- **Tier-1 fuzz "expected bails"** (`tier1/tests/fuzz.rs:465-471`)
  is a tier-1-specific allowlist of bail messages the harness
  treats as not-a-failure. Tier-2 and typed will each need their
  own. *Do not* try to share that list — the bail-message strings
  are codegen-specific.

- **Replay convention.** Keep tier-1's env-knob names
  (`SPLICER_FUZZ_SEED`, `SPLICER_FUZZ_ITERS`) for all three
  fuzzers. Same convention everywhere → muscle memory works for
  whoever's debugging.

- **Iteration count.** Tier-1 uses 200 iters / 256 bytes / depth
  2. Tier-2 will likely want similar; typed might want a smaller
  depth (typed IR has more constructor variants — record fields,
  variant arms, multi-interface worlds — that explode combinatorially
  faster).

## Out of scope (do not bundle into this work)

- Component-interposition tier-3/4 tests beyond
  `builtin-hello-tier{3,4}` (custom `transform_mdl` /
  `virtualize_mdl` middleware crates, stacking case, etc.). The
  existing builtins coverage is enough for now.
- The heavy build-and-run typed fuzzer in `tests/fuzz_and_run.rs`.
  Separate project — needs strategy-crate synthesis.
- Sync WIT support in typed codegen. Tracked separately; see
  the sync-wrapping roadmap memo.
