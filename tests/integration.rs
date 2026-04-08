//! Integration tests that run the component-interposition test suite.
//!
//! These tests require the `tests/component-interposition` git submodule to be
//! initialized and its `fixtures/` directory to contain pre-built `.comp.wasm`
//! files.  They also require `splicer`, `wac`, `cviz-cli`, and `wasm-tools`
//! to be installed.
//!
//! Run with:
//!   cargo test --test integration            # all configs
//!   cargo test --test integration -- single  # just --single
//!
//! These tests are `#[ignore]` by default so `cargo test` stays fast.
//! Run ignored tests with:
//!   cargo test --test integration -- --ignored

use std::path::Path;
use std::process::Command;

const SUBMODULE_PATH: &str = "tests/component-interposition";

fn submodule_ready() -> bool {
    let run_sh = Path::new(SUBMODULE_PATH).join("run.sh");
    let fixtures = Path::new(SUBMODULE_PATH).join("fixtures");
    if !run_sh.exists() {
        eprintln!(
            "Submodule not initialized. Run:\n  git submodule update --init"
        );
        return false;
    }
    if !fixtures.exists() || !fixtures.is_dir() {
        eprintln!(
            "Fixtures not found at {}/fixtures/.\n\
             Build components in the submodule first, or check that pre-built\n\
             fixtures are committed.",
            SUBMODULE_PATH
        );
        return false;
    }
    true
}

fn run_config(opt: &str) {
    if !submodule_ready() {
        panic!("component-interposition submodule not ready");
    }
    let status = Command::new("./run.sh")
        .arg("all")
        .arg(format!("--{}", opt))
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
