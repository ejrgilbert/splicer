//! End-to-end CLI tests for the `splicer` binary.
//!
//! These tests invoke the built binary directly via
//! [`env!("CARGO_BIN_EXE_splicer")`] (cargo wires this up
//! automatically for integration tests), so they exercise the real
//! `clap` parsing and the same `main()` flow users hit. Each test
//! works in its own tempdir to keep the assertions about "what's in
//! cwd after the run?" hermetic.
//!
//! Most coverage uses the `compose` subcommand because two
//! WAT-compiled components are the cheapest way to drive a
//! deterministic happy path. A separate failure-path test exercises
//! `splice` against a known-bad submodule fixture (gracefully skipped
//! when the submodule isn't checked out).

use std::path::{Path, PathBuf};
use std::process::Command;

// ── Fixture helpers ────────────────────────────────────────────────────────

/// Two trivial component WATs whose import/export surfaces match
/// (provider exports `my:providers/a@0.1.0`, consumer imports it).
/// `host:env/dep@0.1.0` stays as an unresolved host import — that's
/// fine for compose; it falls to the host runtime to satisfy.
const WAT_PROVIDER: &str = r#"(component
    (import "host:env/dep@0.1.0" (instance $dep
        (export "get" (func (result u32)))
    ))
    (alias export $dep "get" (func $f))
    (instance $out (export "get" (func $f)))
    (export "my:providers/a@0.1.0" (instance $out))
)"#;

const WAT_CONSUMER: &str = r#"(component
    (import "my:providers/a@0.1.0" (instance $a
        (export "get" (func (result u32)))
    ))
    (alias export $a "get" (func $f))
    (instance $out (export "get" (func $f)))
    (export "my:consumer/app@0.1.0" (instance $out))
)"#;

/// Compile both WATs to wasm, write them next to each other in `dir`,
/// and return their paths (`provider.wasm`, `consumer.wasm`).
fn write_compose_components(dir: &Path) -> (PathBuf, PathBuf) {
    let provider = dir.join("provider.wasm");
    let consumer = dir.join("consumer.wasm");
    std::fs::write(
        &provider,
        wat::parse_str(WAT_PROVIDER).expect("compile provider"),
    )
    .expect("write provider");
    std::fs::write(
        &consumer,
        wat::parse_str(WAT_CONSUMER).expect("compile consumer"),
    )
    .expect("write consumer");
    (provider, consumer)
}

/// Build a `Command` for the splicer binary with `dir` as its working
/// directory — every test uses its own tempdir so cwd-side-effects
/// stay isolated.
fn splicer_in(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_splicer"));
    cmd.current_dir(dir);
    cmd
}

fn assert_valid_wasm(bytes: &[u8]) {
    let mut v = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    v.validate_all(bytes)
        .expect("composed bytes should validate");
}

// ── Compose subcommand ─────────────────────────────────────────────────────

/// Default mode writes only `composed.wasm` and produces nothing on
/// stdout. The bytes must validate as a real wasm component.
#[test]
fn compose_default_writes_only_composed_wasm() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = write_compose_components(dir.path());

    let out = splicer_in(dir.path())
        .arg("compose")
        .arg(&a)
        .arg(&b)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "splicer compose failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "default mode should be silent on stdout, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let composed = dir.path().join("composed.wasm");
    assert!(composed.exists(), "composed.wasm should exist");
    assert!(
        !dir.path().join("output.wac").exists(),
        "output.wac should not be written in default mode"
    );

    let bytes = std::fs::read(&composed).unwrap();
    assert_valid_wasm(&bytes);
}

/// `--plan` writes the WAC + prints the `wac compose ...` shell
/// command on stdout, and does NOT produce a composed wasm.
#[test]
fn compose_plan_writes_wac_only_and_prints_command() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = write_compose_components(dir.path());

    let out = splicer_in(dir.path())
        .arg("compose")
        .arg(&a)
        .arg(&b)
        .arg("--plan")
        .output()
        .unwrap();
    assert!(out.status.success());

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("wac compose "),
        "stdout should start with `wac compose ...` repro command, got: {stdout}"
    );
    assert!(stdout.contains("--dep"), "stdout should list deps");

    assert!(
        dir.path().join("output.wac").exists(),
        "--plan should persist output.wac"
    );
    assert!(
        !dir.path().join("composed.wasm").exists(),
        "--plan should skip in-process compose"
    );
}

/// Bare `--emit-wac` persists the WAC at the default path *and* still
/// writes the composed wasm.
#[test]
fn compose_emit_wac_bare_persists_both() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = write_compose_components(dir.path());

    let out = splicer_in(dir.path())
        .arg("compose")
        .arg(&a)
        .arg(&b)
        .arg("--emit-wac")
        .output()
        .unwrap();
    assert!(out.status.success());

    assert!(dir.path().join("composed.wasm").exists());
    assert!(dir.path().join("output.wac").exists());
}

/// `--emit-wac <path>` writes the WAC at the user-supplied path.
#[test]
fn compose_emit_wac_custom_path() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = write_compose_components(dir.path());

    let out = splicer_in(dir.path())
        .arg("compose")
        .arg(&a)
        .arg(&b)
        .arg("--emit-wac")
        .arg("custom.wac")
        .output()
        .unwrap();
    assert!(out.status.success());

    assert!(dir.path().join("custom.wac").exists());
    assert!(!dir.path().join("output.wac").exists());
    assert!(dir.path().join("composed.wasm").exists());
}

/// `-o <path>` redirects the composed bytes.
#[test]
fn compose_output_path_override() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = write_compose_components(dir.path());

    let out = splicer_in(dir.path())
        .arg("compose")
        .arg(&a)
        .arg(&b)
        .arg("-o")
        .arg("myapp.wasm")
        .output()
        .unwrap();
    assert!(out.status.success());

    assert!(dir.path().join("myapp.wasm").exists());
    assert!(!dir.path().join("composed.wasm").exists());
}

// ── Splice subcommand: failure path ────────────────────────────────────────
//
// We don't ship a hand-rolled splice-able fixture in the repo (a
// composition wasm with inter-component connections preserved at the
// component-graph level requires `wac`/`wasm-tools` build steps the
// CLI tests don't carry). The submodule's `chain1` fixture is the
// next-best deterministic source — splicer parses it fine but
// `wac-graph::encode` rejects it with an "already defined" error,
// which is exactly the in-process compose failure the failure UX is
// designed for. Tests below skip gracefully when the submodule isn't
// checked out.
//
// We don't have a hand-rolled splice fixture that succeeds end-to-end;
// the simplest deterministic failure source is the chain1 fixture
// from the component-interposition submodule, which currently surfaces
// an "already defined" error inside `wac-graph`. That's exactly the
// scenario the failure UX is designed for, so we use it to verify the
// error message contents.

const CHAIN1_YAML_REL: &str = "tests/component-interposition/splicer-rules/chain1.yaml";
const CHAIN1_WASM_REL: &str = "tests/component-interposition/compositions/chain1.wasm";
const PRINTER_MDL_REL: &str = "tests/component-interposition/fixtures/printer_mdl.comp.wasm";

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn chain1_fixture_present() -> bool {
    let m = manifest_dir();
    m.join(CHAIN1_YAML_REL).exists()
        && m.join(CHAIN1_WASM_REL).exists()
        && m.join(PRINTER_MDL_REL).exists()
}

/// Copy chain1.yaml, chain1.wasm, and the printer_mdl fixture into
/// `dir` so a `splicer splice` invocation in that cwd can resolve the
/// YAML's `./fixtures/printer_mdl.comp.wasm` reference.
fn stage_chain1_fixture(dir: &Path) {
    let m = manifest_dir();
    std::fs::copy(m.join(CHAIN1_YAML_REL), dir.join("chain1.yaml")).unwrap();
    std::fs::copy(m.join(CHAIN1_WASM_REL), dir.join("chain1.wasm")).unwrap();
    std::fs::create_dir_all(dir.join("fixtures")).unwrap();
    std::fs::copy(
        m.join(PRINTER_MDL_REL),
        dir.join("fixtures/printer_mdl.comp.wasm"),
    )
    .unwrap();
}

/// When in-process compose fails, the error message must:
/// - report that compose failed,
/// - name the path where the WAC was preserved,
/// - name the path where the splits were preserved,
/// - include a `wac compose ...` repro command the user can run.
#[test]
fn splice_compose_failure_preserves_wac_and_prints_repro() {
    if !chain1_fixture_present() {
        eprintln!("skipping: chain1 fixture not checked out");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    stage_chain1_fixture(dir.path());

    let out = splicer_in(dir.path())
        .arg("splice")
        .arg("chain1.yaml")
        .arg("chain1.wasm")
        .output()
        .unwrap();
    assert!(!out.status.success(), "chain1 should fail at compose time");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("in-process compose failed"),
        "error should label the failure: {stderr}"
    );
    assert!(
        stderr.contains("WAC preserved at:"),
        "error should name the persisted WAC path: {stderr}"
    );
    assert!(
        stderr.contains("Splits preserved at:"),
        "error should name the persisted splits path: {stderr}"
    );
    assert!(
        stderr.contains("Reproduce standalone with:"),
        "error should include a repro section header: {stderr}"
    );
    assert!(
        stderr.contains("wac compose "),
        "repro command should be the literal `wac compose ...`: {stderr}"
    );
}

/// In failure mode (default flags), nothing is left in cwd — the
/// preserved WAC + splits go to leaked tempdirs whose paths appear in
/// the error message.
#[test]
fn splice_failure_does_not_pollute_cwd() {
    if !chain1_fixture_present() {
        eprintln!("skipping: chain1 fixture not checked out");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    stage_chain1_fixture(dir.path());

    let _ = splicer_in(dir.path())
        .arg("splice")
        .arg("chain1.yaml")
        .arg("chain1.wasm")
        .output()
        .unwrap();

    assert!(!dir.path().join("output.wac").exists());
    assert!(!dir.path().join("composed.wasm").exists());
    assert!(!dir.path().join("splits").exists());
}

/// `--plan` against the failing fixture should still succeed: it
/// never runs `to_wasm()`, so the underlying compose error is not
/// hit. We get `output.wac` + `splits/` and a printed shell command.
#[test]
fn splice_plan_works_even_when_compose_would_fail() {
    if !chain1_fixture_present() {
        eprintln!("skipping: chain1 fixture not checked out");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    stage_chain1_fixture(dir.path());

    let out = splicer_in(dir.path())
        .arg("splice")
        .arg("chain1.yaml")
        .arg("chain1.wasm")
        .arg("--plan")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "--plan skips compose, so it should succeed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("wac compose "));
    assert!(dir.path().join("output.wac").exists());
    assert!(dir.path().join("splits").exists());
    assert!(!dir.path().join("composed.wasm").exists());
}
