//! Integration tests that run the component-interposition test suite.
//!
//! These tests require the `tests/component-interposition` git submodule to be
//! initialized, plus `splicer`, `wac`, `cviz-cli`, and `wasm-tools` on PATH.
//! Fixtures aren't checked in — the first test run invokes `./run.sh build`
//! once via a sync::Once so subsequent configs reuse the built `.comp.wasm`s
//! with `--skip-build`.
//!
//! Run with:
//!   cargo test --test integration -- --ignored            # all configs
//!   cargo test --test integration -- --ignored single     # just --single
//!
//! `#[ignore]`'d by default so `cargo test` stays fast.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};

const SUBMODULE_PATH: &str = "tests/component-interposition";

// Configs share intermediate output dirs (compositions/, generated-wac/,
// splits/) in the submodule, so concurrent invocations race. Serialize them.
static TEST_LOCK: Mutex<()> = Mutex::new(());
// One-time fixture build per test-session. The `Once` closure must not
// panic (it'd poison the primitive and cascade "Once poisoned" panics
// across every test), so we capture success/failure in a separate
// atomic and let the caller surface a clear message pointing at the
// failing test's own stdout.
static FIXTURES_BUILT: Once = Once::new();
static FIXTURES_BUILD_OK: AtomicBool = AtomicBool::new(false);

fn submodule_ready() -> bool {
    let run_sh = Path::new(SUBMODULE_PATH).join("run.sh");
    if !run_sh.exists() {
        eprintln!("Submodule not initialized. Run:\n  git submodule update --init");
        return false;
    }
    true
}

fn ensure_fixtures_built() {
    FIXTURES_BUILT.call_once(|| {
        // NB: this closure must not panic — panicking poisons the
        // Once and every subsequent test panics with the unhelpful
        // "Once instance has previously been poisoned" message
        // instead of pointing at the real build failure. Record the
        // outcome and let the caller surface a clear message.
        let status = match Command::new("./run.sh")
            .arg("build")
            .current_dir(SUBMODULE_PATH)
            .status()
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ensure_fixtures_built: spawn ./run.sh build: {e}");
                return;
            }
        };
        if !status.success() {
            eprintln!(
                "ensure_fixtures_built: ./run.sh build failed with exit code {:?}",
                status.code()
            );
            return;
        }
        // Build the tier-1/2 builtin wasms from this branch's source so
        // splicer doesn't fall back to OCI (which would test whatever's
        // currently published, not this branch).
        let status = match Command::new("make").arg("build-builtins").status() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ensure_fixtures_built: spawn make build-builtins: {e}");
                return;
            }
        };
        if !status.success() {
            eprintln!(
                "ensure_fixtures_built: make build-builtins failed with exit code {:?}",
                status.code()
            );
            return;
        }
        FIXTURES_BUILD_OK.store(true, Ordering::Relaxed);
    });
    assert!(
        FIXTURES_BUILD_OK.load(Ordering::Relaxed),
        "fixture build failed — see the first FAILED integration_* test's \
         output for the actual build error"
    );
}

fn run_config(opt: &str) {
    // Recover from poisoned lock so one panicking test doesn't cascade.
    let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if !submodule_ready() {
        panic!("component-interposition submodule not ready");
    }
    ensure_fixtures_built();
    let builtins_dir = std::env::current_dir()
        .expect("cwd")
        .join("assets")
        .join("builtins");
    let status = Command::new("./run.sh")
        .arg("all")
        .arg(format!("--{}", opt))
        .arg("--skip-build")
        .env("SPLICER_BUILTINS_DIR", &builtins_dir)
        .current_dir(SUBMODULE_PATH)
        .status()
        .unwrap_or_else(|e| panic!("failed to run ./run.sh all --{}: {}", opt, e));
    assert!(
        status.success(),
        "run.sh all --{} failed with exit code {:?}",
        opt,
        status.code()
    );
}

#[test]
#[ignore]
fn integration_single() {
    run_config("single");
}

#[test]
#[ignore]
fn integration_multiple() {
    run_config("multiple");
}

#[test]
#[ignore]
fn integration_chain() {
    run_config("chain");
}

#[test]
#[ignore]
fn integration_chain1() {
    run_config("chain1");
}

#[test]
#[ignore]
fn integration_chain_n() {
    run_config("chainN");
}

#[test]
#[ignore]
fn integration_nested() {
    run_config("nested");
}

#[test]
#[ignore]
fn integration_inner_nested1() {
    run_config("inner-nested1");
}

#[test]
#[ignore]
fn integration_inner_nested_n() {
    run_config("inner-nestedN");
}

#[test]
#[ignore]
fn integration_pre_nested1() {
    run_config("pre-nested1");
}

#[test]
#[ignore]
fn integration_pre_nested_n() {
    run_config("pre-nestedN");
}

#[test]
#[ignore]
fn integration_inner_pre_nested1() {
    run_config("inner+pre-nested1");
}

#[test]
#[ignore]
fn integration_inner_pre_nested_n() {
    run_config("inner+pre-nestedN");
}

#[test]
#[ignore]
fn integration_fanin() {
    run_config("fanin");
}

#[test]
#[ignore]
fn integration_fanin1() {
    run_config("fanin1");
}

#[test]
#[ignore]
fn integration_fanin_n() {
    run_config("faninN");
}

#[test]
#[ignore]
fn integration_fanin_all1() {
    run_config("fanin-all1");
}

#[test]
#[ignore]
fn integration_fanin_all_n() {
    run_config("fanin-allN");
}

#[test]
#[ignore]
fn integration_fanin_filter() {
    run_config("fanin-filter");
}

#[test]
#[ignore]
fn integration_block1() {
    run_config("block1");
}

#[test]
#[ignore]
fn integration_block_n() {
    run_config("blockN");
}

#[test]
#[ignore]
fn integration_nonblock1() {
    run_config("nonblock1");
}

#[test]
#[ignore]
fn integration_nonblock_n() {
    run_config("nonblockN");
}

#[test]
#[ignore]
fn integration_tier1_all() {
    run_config("tier1-all");
}

#[test]
#[ignore]
fn integration_tier2() {
    run_config("tier2");
}

#[test]
#[ignore]
fn integration_tier2_all() {
    run_config("tier2-all");
}

#[test]
#[ignore]
fn integration_builtin_hello_tier1() {
    run_config("builtin-hello-tier1");
}

#[test]
#[ignore]
fn integration_builtin_hello_tier2() {
    run_config("builtin-hello-tier2");
}

#[test]
#[ignore]
fn integration_builtin_hello_tier3() {
    run_config("builtin-hello-tier3");
}

#[test]
#[ignore]
fn integration_builtin_hello_tier4() {
    run_config("builtin-hello-tier4");
}

#[test]
#[ignore]
fn integration_builtin_otel() {
    run_config("builtin-otel");
}
