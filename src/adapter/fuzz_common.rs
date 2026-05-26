//! Shared primitives for the per-tier structural fuzz harnesses.
//!
//! Each tier owns its own random-shape generator (the IR shape and
//! bail-message vocabulary differ); the LCG byte source, env-knob
//! parser, iteration driver, and failure formatter are uniform so
//! `SPLICER_FUZZ_SEED` / `SPLICER_FUZZ_ITERS` mean the same thing in
//! every fuzz target.

/// Pinned default seed — overrideable with `SPLICER_FUZZ_SEED`.
pub(super) const DEFAULT_FUZZ_SEED: u64 = 0xDEAD_BEEF;
/// Default iteration count for structural fuzz loops.
pub(super) const DEFAULT_FUZZ_ITERS: usize = 200;
/// Random bytes drawn per fuzz iteration.
pub(super) const FUZZ_BYTES_PER_ITER: usize = 256;
/// Max failures echoed into the test output before truncating.
pub(super) const MAX_FAILURES_SHOWN: usize = 20;

/// Per-iteration outcome reported by the harness closure.
pub(super) enum FuzzOutcome {
    /// Adapter generated and validated.
    Passed,
    /// Adapter bailed with a known-in-scope error (a generator
    /// shape outside the current support envelope).
    ExpectedBail,
}

/// Read `SPLICER_FUZZ_ITERS` + `SPLICER_FUZZ_SEED`, defaulting to the
/// pinned constants. Echoes the resolved values so a CI log makes
/// replay obvious.
pub(super) fn fuzz_knobs(label: &str) -> (usize, u64) {
    let iters: usize = std::env::var("SPLICER_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_FUZZ_ITERS);
    let base_seed: u64 = std::env::var("SPLICER_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_FUZZ_SEED);
    eprintln!("{label}: iters={iters} base_seed={base_seed}");
    (iters, base_seed)
}

/// Deterministic LCG byte source — replayable iteration data without
/// pulling `rand` in just for the fuzz harness.
pub(super) fn fuzz_seeded_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 32) as u8
        })
        .collect()
}

/// Run a structural fuzz loop. The closure receives one
/// LCG-seeded byte slice per iteration and decides whether the
/// generated shape passed, hit an expected bail, or failed. Panics
/// inside the closure are caught and recorded as failures so one
/// bad shape doesn't mask the rest.
pub(super) fn run_structural_fuzz(
    label: &str,
    iter_fn: impl Fn(&[u8]) -> Result<FuzzOutcome, String>,
) {
    let (iters, base_seed) = fuzz_knobs(label);
    let mut passed = 0usize;
    let mut expected_bails = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for i in 0..iters {
        let iter_seed = base_seed.wrapping_add(i as u64);
        let bytes = fuzz_seeded_bytes(iter_seed, FUZZ_BYTES_PER_ITER);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| iter_fn(&bytes)));
        match outcome {
            Ok(Ok(FuzzOutcome::Passed)) => passed += 1,
            Ok(Ok(FuzzOutcome::ExpectedBail)) => expected_bails += 1,
            Ok(Err(msg)) => failures.push(format!("iter {i} seed {iter_seed}: {msg}")),
            Err(_) => failures.push(format!("iter {i} seed {iter_seed}: PANIC")),
        }
    }
    eprintln!(
        "{label}: passed={passed} expected_bails={expected_bails} failures={}",
        failures.len()
    );
    if !failures.is_empty() {
        report_failures_and_panic(&failures);
    }
}

/// Print pending failures then panic with a replay-hint message.
/// Called by [`run_structural_fuzz`] when failures are non-empty.
fn report_failures_and_panic(failures: &[String]) -> ! {
    for f in failures.iter().take(MAX_FAILURES_SHOWN) {
        eprintln!("  {f}");
    }
    if failures.len() > MAX_FAILURES_SHOWN {
        eprintln!("  ... and {} more", failures.len() - MAX_FAILURES_SHOWN);
    }
    panic!(
        "{} structural fuzz iterations failed -- replay a single case with \
         SPLICER_FUZZ_SEED=<iter_seed_from_output> SPLICER_FUZZ_ITERS=1",
        failures.len()
    );
}
