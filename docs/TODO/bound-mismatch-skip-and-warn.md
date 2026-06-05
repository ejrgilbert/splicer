# Treat strategy trait-bound failures as skip-and-warn, not hard fail

The tier-3/4 codegen pipeline currently bails any time `cargo build`
fails on a generated wrapper crate. When the failure mode is
**"strategy's trait bound doesn't fit the wrapped interface's WIT
shape"** (E0277), that's not a bug — it's a *config-time fact* about
which (rule × interface) pairs the strategy can interpose. Today the
user sees a hard splice failure with a hint pointing at
`trait_bound_hint`; the right behavior is to skip that specific match,
warn, and let the rest of the composition proceed.

This is also the right architectural answer to "SDK trait bounds keep
multiplying and YAML type predicates can't stay in lock step": once
bound failures degrade gracefully, type predicates become a true
**optimization** (skip the expensive cargo invocation on shapes you
can pre-prove won't fit), not a correctness requirement.

## The constraint

The compile-time bound check is the ground truth. The wrapper crate
either compiles or it doesn't; the trait bound on the strategy is
what enforces "this strategy fits this interface." YAML type
predicates exist to *forecast* that result so users get config-time
errors instead of build-time errors. Two consequences:

- A type predicate that mirrors a specific bound (`is-has-arbitrary-err`)
  carries no correctness value — the bound enforces itself. Such
  predicates are pure ergonomics.
- A *missing* predicate for a given bound means failures arrive at
  cargo build time instead of YAML parse time. That's a worse error
  UX, not a correctness problem.

So the SDK ↔ YAML coupling everyone is worried about is about *when*
errors fire, not *whether* they fire. The skip-and-warn change makes
that coupling fully optional.

## Where splicer hits this

`src/adapter/typed/build.rs:build_wrapper` returns `Result<PathBuf>`.
On cargo failure it bails:

```rust
if !out.status.success() {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let hint = trait_bound_hint(&stderr);
    bail!(
        "cargo build failed (exit code {:?}){hint}:\n{stderr}",
        out.status.code(),
    );
}
```

`trait_bound_hint` (`build.rs:150`) is a loose substring match on
`"E0277" || "trait bound"`. It only adds a hint string today; under
skip-and-warn it would drive a behavioral branch, so its precision
matters more.

The orchestration chain:

```
wac.rs:materialize_tier3_4_inline      ←─ iterates injections, would aggregate skips
  → strategies.rs:materialize_tier3_4   ←─ public entry per (rule × interface)
    → strategies.rs:materialize_from_prepared
      → strategies.rs:run_codegen_build
        → build.rs:build_wrapper        ←─ cargo failure originates here
```

Five layers; the return type change has to propagate through all of
them, plus an aggregation surface (probably on `SpliceCtx` or the
return of `materialize_tier3_4_inline`).

## Design

### Categorize the cargo failure

```rust
pub enum BuildOutcome {
    /// Wrapper compiled and produced a component wasm at this path.
    Built(PathBuf),
    /// Cargo failed with a trait-bound mismatch — the strategy's
    /// bound (`R: HasArbitraryErr`, `Args: WitTyped`, …) is not
    /// satisfied by the wrapped interface's WIT shape. Carries the
    /// inferred bound name (from manifest description or stderr
    /// parsing) so the orchestrator can surface a precise warning.
    BoundMismatch {
        strategy: String,
        interface: String,
        required_bound: Option<String>,
        stderr: String,
    },
    // any other cargo failure remains an `Err`, not a `BoundMismatch`.
}
```

Internal-failure cases (manifest missing, cargo not on PATH, link-time
error, dep resolution failure, segfault, etc.) stay as `Err`. Only
genuine bound mismatches get the soft outcome.

### Tighten the heuristic

`trait_bound_hint`'s current loose match is fine for "add a hint
string"; it's not precise enough to drive skip-vs-fail. Tighten to
specifically recognize the rustc trait-bound error shape:

```
error[E0277]: the trait bound `T: Trait` is not satisfied
 --> src/lib.rs:N:M
```

The structured part (`E0277` + `the trait bound` + `is not satisfied`)
appears in a stable line shape. Parse the bound text out of it; that's
the `required_bound` field. If parsing fails, fall back to "bound
mismatch detected" without a specific name.

A failure that mentions "trait bound" in a *secondary* diagnostic
(e.g., the strategy crate itself has an unrelated E0277) should NOT
be classified as `BoundMismatch`; the bound check shows up at the
wrapper crate's emission site, not deep inside the strategy crate.
Either constrain to errors whose `--> src/lib.rs:` path is the
wrapper crate, or require the bound be one the manifest declares.

### Manifest declares the bound

```toml
[builtin]
description = "..."
tier = 4
required_bound = "R: HasArbitraryErr"
```

The orchestrator uses this both to:
- Make the warning message precise (`required_bound` is in the
  manifest, so the user sees the SDK trait name verbatim).
- Constrain the heuristic (only categorize as `BoundMismatch` if the
  rustc error names a trait that appears in `required_bound`).

This closes the false-positive door: a strategy whose `required_bound`
field is empty just hard-fails on E0277 like today. Opt-in graceful
degradation.

### Orchestrator aggregates skips

`materialize_tier3_4_inline` already loops over injections. Each
`BoundMismatch` outcome accumulates into a `Vec<SkipRecord>` on
`SpliceCtx` (or threaded through the return). The `Built` outcomes
get stamped onto injections as today.

End-of-run summary (in main / lib top-level), example:

```
Spliced 12 rules across 47 interface matches.
Skipped 8 matches (strategy bound did not fit):
  chaos-err on wasi:http/handler@0.3.0 — needs `R: HasArbitraryErr`
  replayer on wasi:io/streams@0.2.0    — needs `R: WitTypedWithResources`
  ...
Run with `--strict` to fail on skips.
```

### `--strict` mode

CLI flag that promotes any `BoundMismatch` back to a hard failure.
For CI gates where partial composition is unacceptable.

Default: lenient (skip + warn). Rationale: the most common iterative
workflow is "splice broadly with a glob; see what fits"; surprise
failures on partial fits punish exploration. CI users opt into
strict.

## Implementation plan

### Pass 1 — internal refactor, no external behavior change (~1–2h)

1. Define `BuildOutcome` and `SkipRecord` in `build.rs`.
2. Tighten `trait_bound_hint` into a structured parser returning
   `Option<BoundMismatchSignature>`. Add unit tests over canned
   rustc-stderr fixtures (one true positive per common bound, one
   false positive that mentions "trait bound" but isn't, one that
   names a trait not in `required_bound`).
3. Internally, `build_wrapper` builds a `BuildOutcome` but still
   returns `Result<PathBuf>` to callers. `BoundMismatch` collapses
   to an `Err` for now, preserving today's behavior.

After Pass 1: nothing observable changes, but the categorization
machinery + heuristic are tested and ready to use.

### Pass 2 — propagate + user-facing (~2–4h + design)

1. Add `required_bound` to the manifest schema. Update existing
   builtin manifests (chaos-err, replayer).
2. Flip `build_wrapper` return type to `Result<BuildOutcome>`.
3. Propagate through `run_codegen_build` →
   `materialize_from_prepared` → `materialize_tier3_4` →
   `materialize_tier3_4_inline`.
4. Aggregate `BoundMismatch`es into a skip list (probably on
   `SpliceCtx` or returned from the inline orchestrator).
5. Print the end-of-run summary.
6. Add `--strict` CLI flag.
7. End-to-end test: a fixture strategy whose bound doesn't fit a
   fixture interface; verify the splice succeeds with a skip rather
   than failing.

## Open design questions for Pass 2

These shouldn't be decided unilaterally — they shape the user
experience:

- **`--strict` default.** Lenient (skip + warn) for iterative use,
  strict for CI? Or strict by default and `--allow-skips` to opt
  out? Affects the muscle memory of every user.
- **Exit code semantics.** `0` on skip-and-warn vs `0` only on
  zero-skip vs non-zero with a distinct code for "skipped only" vs
  "real failure"? Different downstream tooling integrations want
  different answers.
- **Summary format.** Plain text (above) is fine for humans. CI
  tooling probably wants `--summary-format=json` for machine parse.
- **Multi-match rule semantics.** A YAML rule with a glob may match
  10 interfaces. If 3 skip + 7 succeed, is that a "partially-applied
  rule" worth distinguishing in the summary? Or do we summarize
  purely at the (rule × interface) cell level? Affects how users
  reason about rules that don't fully apply.

## What this enables

- **SDK bound additions become additive.** Adding `HasArbitraryErr`,
  `IntoResult`, future structural traits doesn't require any YAML
  vocabulary change. Strategies declaring the new bound just work;
  interfaces that don't satisfy it skip with a precise warning.
- **YAML type predicates remain genuinely useful as an
  optimization.** Users with strict latency budgets pre-filter via
  predicates to avoid the cargo invocation. Users who don't care
  about latency don't bother — and they're not punished for it.
- **Glob ergonomics improve.** `interface: "wasi:*"` is now a
  reasonable rule: it applies wherever the strategy fits, skips
  cleanly where it doesn't.

## Non-goals

- **Removing or deprecating existing predicates.** `concrete` and
  `defaultable` stay; they're structural (whole-tree) properties
  that cover many bounds at once. The change is about not feeling
  pressure to add a new predicate per bound.
- **Auto-discovering bounds from the strategy crate.** The manifest's
  `required_bound` is human-authored; we don't try to parse the
  strategy's Rust source to extract its bound. (Could be a future
  enhancement; not needed for the architectural change to land.)
- **Strict arg-matching at the wrapper boundary.** Out of scope; see
  [tier3-tier4-builtins.md](tier3-tier4-builtins.md) for the broader
  arg-matching discussion.

## References

- `src/adapter/typed/build.rs` — `build_wrapper`, `trait_bound_hint`.
- `src/strategies.rs` — `run_codegen_build`,
  `materialize_from_prepared`, `materialize_tier3_4`.
- `src/wac.rs:materialize_tier3_4_inline` — the orchestration loop.
- Existing predicates: `src/select.rs:ValueProperty`,
  `src/preview.rs` — vocabulary today.
- Cross-cutting context on the SDK-bound family:
  [tier3-tier4-builtins.md](tier3-tier4-builtins.md).
