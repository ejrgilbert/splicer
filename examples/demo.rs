//! Demo: splicer rule application and type-compatibility checking.
//!
//! Run with:
//!   cargo run --example demo
//!
//! Tests (also exercised by `cargo test --all-targets`):
//!   cargo test --example demo
//!
//! ## Phase 1 — Basic splice
//! Shows WAC generation for `before` and `between` rules on a simple
//! log-interface chain (no type information required).
//!
//! ## Phase 2 — Type-compatibility checking
//! Demonstrates all three `ContractResult` outcomes:
//!   Warn  — middleware has no path; type safety unconfirmed but injection proceeds
//!   Ok    — fingerprints match; injection confirmed safe
//!   Error — fingerprints differ; types are structurally incompatible
//!
//! ## Phase 3 — Full pipeline with real WAT
//! Compiles WAT middleware sources from `demo/wat/`, writes them to temp files,
//! and calls `validate_contract` via the real `discover_middleware_exports` path:
//!   Ok    — compatible WAT middleware (same `log` signature as chain)
//!   Error — incompatible WAT middleware (different `log` signature)

use cviz::model::ExportInfo;
use cviz::parse::component::parse_component;
use cviz::parse::json::parse_json_str;
use splicer::contract::{validate_contract, ContractResult};
use splicer::parse::config::{parse_yaml, Injection};
use splicer::wac::generate_wac;
use std::collections::{BTreeMap, HashMap};

// ─── JSON graph fixtures ──────────────────────────────────────────────────
// Two-node log chain:  log-provider  →  app
const JSON_LOG_SHORT: &str = r#"
{
  "version": 1,
  "nodes": [
    { "id": 11, "name": "log-provider", "component_index": 0, "component_num": 0, "imports": [] },
    {
      "id": 13, "name": "app", "component_index": 1, "component_num": 1,
      "imports": [
        { "interface": "wasi:logging/log@0.1.0", "short": "log",
          "source_instance": 11, "is_host_import": false }
      ]
    }
  ],
  "exports": [{ "interface": "wasi:logging/log@0.1.0", "source_instance": 13 }]
}
"#;

// Three-node log chain:  log-provider-inner  →  log-provider  →  app
const JSON_LOG_LONG: &str = r#"
{
  "version": 1,
  "nodes": [
    { "id": 11, "name": "log-provider-inner", "component_index": 0, "component_num": 0, "imports": [] },
    {
      "id": 12, "name": "log-provider", "component_index": 1, "component_num": 1,
      "imports": [
        { "interface": "wasi:logging/log@0.1.0", "short": "log",
          "source_instance": 11, "is_host_import": false }
      ]
    },
    {
      "id": 13, "name": "app", "component_index": 2, "component_num": 2,
      "imports": [
        { "interface": "wasi:logging/log@0.1.0", "short": "log",
          "source_instance": 12, "is_host_import": false }
      ]
    }
  ],
  "exports": [{ "interface": "wasi:logging/log@0.1.0", "source_instance": 13 }]
}
"#;

const LOG_IFACE: &str = "wasi:logging/log@0.1.0";

// ─── Phase 1 scenario functions ───────────────────────────────────────────

/// 1a: inject `mw-a` before `log-provider` in a two-node chain.
pub fn scenario_1a_before_on_short_chain() -> String {
    let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:logging/log@0.1.0
      provider:
        name: log-provider
    inject:
      - name: mw-a
"#;
    let cfg = parse_yaml(yaml).unwrap();
    let graph = parse_json_str(JSON_LOG_SHORT).unwrap();
    generate_wac(
        HashMap::new(),
        "placeholder",
        &graph,
        &cfg,
        None,
        "example:composition",
    )
    .unwrap()
    .wac
}

/// 1b: inject `mw-a` before `log-provider-inner` (the deepest node) in a three-node chain.
pub fn scenario_1b_before_on_long_chain() -> String {
    let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:logging/log@0.1.0
      provider:
        name: log-provider-inner
    inject:
      - name: mw-a
"#;
    let cfg = parse_yaml(yaml).unwrap();
    let graph = parse_json_str(JSON_LOG_LONG).unwrap();
    generate_wac(
        HashMap::new(),
        "placeholder",
        &graph,
        &cfg,
        None,
        "example:composition",
    )
    .unwrap()
    .wac
}

/// 1c: inject `mw-a` and `mw-b` between `log-provider-inner` and `log-provider`.
pub fn scenario_1c_between_on_long_chain() -> String {
    let yaml = r#"
version: 1
rules:
  - between:
      interface: wasi:logging/log@0.1.0
      inner:
        name: log-provider-inner
      outer:
        name: log-provider
    inject:
      - name: mw-a
      - name: mw-b
"#;
    let cfg = parse_yaml(yaml).unwrap();
    let graph = parse_json_str(JSON_LOG_LONG).unwrap();
    generate_wac(
        HashMap::new(),
        "placeholder",
        &graph,
        &cfg,
        None,
        "example:composition",
    )
    .unwrap()
    .wac
}

// ─── Phase 2 helpers ─────────────────────────────────────────────────────

const CHAIN_FP: &str = "sha256-abc123-fake-fingerprint";

fn injection(name: &str) -> Injection {
    Injection {
        name: name.to_string(),
        path: None,
    }
}

fn cache_with_fp(mw: &str, fp: &str) -> HashMap<String, BTreeMap<String, ExportInfo>> {
    let mut exports = BTreeMap::new();
    exports.insert(
        LOG_IFACE.to_string(),
        ExportInfo {
            source_instance: 0,
            fingerprint: Some(fp.to_string()),
            ty: None,
        },
    );
    let mut cache = HashMap::new();
    cache.insert(mw.to_string(), exports);
    cache
}

// ─── Phase 2 scenario functions ───────────────────────────────────────────

/// 2a: Warn — middleware has no path; discovery returns empty; cannot validate.
pub fn scenario_2a_warn_no_path() -> Vec<ContractResult> {
    let mut cache = HashMap::new();
    validate_contract(
        &[injection("mw")],
        LOG_IFACE,
        &Some(CHAIN_FP.to_string()),
        &mut cache,
    )
}

/// 2b: Ok — middleware exports the interface with a matching fingerprint.
pub fn scenario_2b_ok_compatible() -> Vec<ContractResult> {
    let mut cache = cache_with_fp("mw", CHAIN_FP);
    validate_contract(
        &[injection("mw")],
        LOG_IFACE,
        &Some(CHAIN_FP.to_string()),
        &mut cache,
    )
}

/// 2c: Error — middleware exports the interface but with an incompatible fingerprint.
pub fn scenario_2c_error_incompatible() -> Vec<ContractResult> {
    let mut cache = cache_with_fp("mw", "sha256-zzz999-different-type");
    validate_contract(
        &[injection("mw")],
        LOG_IFACE,
        &Some(CHAIN_FP.to_string()),
        &mut cache,
    )
}

// ─── Phase 3 helpers ─────────────────────────────────────────────────────

// Chain WAT used solely to derive the fingerprint for wasi:logging/log@0.1.0.
// The interface exports a single `log(level: u32, message: string)` function.
const CHAIN_WAT: &str = r#"(component
    (import "wasi:logging/log@0.1.0" (instance $host
        (export "log" (func (param "level" u32) (param "message" string)))
    ))
    (alias export $host "log" (func $f))
    (instance $out (export "log" (func $f)))
    (export "wasi:logging/log@0.1.0" (instance $out))
)"#;

/// Derive the fingerprint for `wasi:logging/log@0.1.0` from the chain WAT.
fn chain_log_fingerprint() -> Option<String> {
    let bytes = wat::parse_str(CHAIN_WAT).expect("compile chain WAT");
    let graph = parse_component(&bytes).expect("parse chain component");
    graph
        .component_exports
        .get(LOG_IFACE)
        .and_then(|e| e.fingerprint.clone())
}

/// Compile `mw_wat`, write the WASM bytes to a temp file, then run
/// `validate_contract` — exercising the real `discover_middleware_exports` path.
fn run_type_check_full(mw_wat: &str, temp_name: &str) -> Vec<ContractResult> {
    let chain_fp = chain_log_fingerprint();

    let mw_bytes = wat::parse_str(mw_wat).expect("compile middleware WAT");
    let tmp_path = std::env::temp_dir().join(temp_name);
    std::fs::write(&tmp_path, &mw_bytes).expect("write temp wasm");

    let inj = Injection {
        name: "mw".to_string(),
        path: Some(tmp_path.to_str().unwrap().to_string()),
    };

    let mut cache = HashMap::new();
    validate_contract(&[inj], LOG_IFACE, &chain_fp, &mut cache)
}

// ─── Phase 3 scenario functions ───────────────────────────────────────────

/// 3a: compatible WAT middleware (same signature as chain) → Ok.
pub fn scenario_3a_ok_compatible_wat() -> Vec<ContractResult> {
    let mw_wat = include_str!("../demo/wat/log-middleware-compatible.wat");
    run_type_check_full(mw_wat, "splicer-demo-mw-compatible.wasm")
}

/// 3b: incompatible WAT middleware (different `log` signature) → Error.
pub fn scenario_3b_error_incompatible_wat() -> Vec<ContractResult> {
    let mw_wat = include_str!("../demo/wat/log-middleware-incompatible.wat");
    run_type_check_full(mw_wat, "splicer-demo-mw-incompatible.wasm")
}

// ─── Printing helpers ─────────────────────────────────────────────────────

fn header(title: &str) {
    let bar = "═".repeat(64);
    println!("\n{bar}");
    println!("  {title}");
    println!("{bar}");
}

fn subheader(label: &str) {
    println!("\n  ── {label} ──");
}

fn show_contract_result(result: &ContractResult) {
    match result {
        ContractResult::Ok => println!("  ✔  Ok — types are compatible, injection confirmed safe"),
        ContractResult::Warn(msg) => println!("  ⚠  Warn — {msg}"),
        ContractResult::Error(msg) => println!("  ✘  Error — {msg}"),
        ContractResult::Tier1Compatible(ifaces) => {
            println!("  ↪  Tier1Compatible — middleware is type-erased; proxy component will be generated (hooks: {ifaces:?})")
        }
    }
}

// ─── main ─────────────────────────────────────────────────────────────────

fn main() {
    // ── Phase 1 ──────────────────────────────────────────────────────────
    header("Phase 1 — Basic Splice  (no type checking)");

    subheader("1a · before rule on a two-node log chain");
    println!("{}", scenario_1a_before_on_short_chain());

    subheader("1b · before rule on a three-node log chain  (targets innermost provider)");
    println!("{}", scenario_1b_before_on_long_chain());

    subheader("1c · between rule on a three-node log chain  (two middlewares)");
    println!("{}", scenario_1c_between_on_long_chain());

    // ── Phase 2 ──────────────────────────────────────────────────────────
    header("Phase 2 — Type-Compatibility Checking");
    println!();
    println!("  Chain fingerprint: {CHAIN_FP}");

    subheader("2a · Warn — middleware has no path; cannot validate type safety");
    for r in scenario_2a_warn_no_path() {
        show_contract_result(&r);
    }

    subheader("2b · Ok — middleware exports the interface with matching fingerprint");
    for r in scenario_2b_ok_compatible() {
        show_contract_result(&r);
    }

    subheader("2c · Error — middleware exports the interface with a DIFFERENT fingerprint");
    for r in scenario_2c_error_incompatible() {
        show_contract_result(&r);
    }

    // ── Phase 3 ──────────────────────────────────────────────────────────
    header("Phase 3 — Full Pipeline with Real WAT");
    println!();
    println!("  Interface : {LOG_IFACE}");
    println!("  Chain WAT : log(level: u32, message: string)");

    subheader("3a · Ok — compatible WAT middleware  (same log signature)");
    for r in scenario_3a_ok_compatible_wat() {
        show_contract_result(&r);
    }

    subheader("3b · Error — incompatible WAT middleware  (level: string vs u32)");
    for r in scenario_3b_error_incompatible_wat() {
        show_contract_result(&r);
    }

    let bar = "═".repeat(64);
    println!("\n{bar}");
}

// ─── Tests ────────────────────────────────────────────────────────────────
// Run with: cargo test --example demo   (or cargo test --all-targets)

#[test]
fn test_1a_middleware_wraps_log_provider() {
    let wac = scenario_1a_before_on_short_chain();
    assert!(
        wac.contains("let mw-a = new my:mw-a {"),
        "mw-a should be instantiated"
    );
    assert!(
        wac.contains(&format!(r#""{LOG_IFACE}": log-provider["{LOG_IFACE}"]"#)),
        "mw-a should be wired from log-provider"
    );
    assert!(
        wac.contains(&format!(r#""{LOG_IFACE}": mw-a["{LOG_IFACE}"]"#)),
        "app should receive log through mw-a"
    );
}

#[test]
fn test_1b_middleware_inserted_before_innermost() {
    let wac = scenario_1b_before_on_long_chain();
    assert!(
        wac.contains("let mw-a = new my:mw-a {"),
        "mw-a should be instantiated"
    );
    assert!(
        wac.contains(&format!(
            r#""{LOG_IFACE}": log-provider-inner["{LOG_IFACE}"]"#
        )),
        "mw-a should be wired from log-provider-inner"
    );
    // log-provider wired through mw-a, not directly from log-provider-inner
    assert!(
        wac.contains(&format!(r#""{LOG_IFACE}": mw-a["{LOG_IFACE}"]"#)),
        "log-provider should receive log through mw-a"
    );
}

#[test]
fn test_1c_two_middlewares_between_nodes() {
    let wac = scenario_1c_between_on_long_chain();
    assert!(
        wac.contains("let mw-a = new my:mw-a {"),
        "mw-a should be instantiated"
    );
    assert!(
        wac.contains("let mw-b = new my:mw-b {"),
        "mw-b should be instantiated"
    );
    // Both middlewares wired on the log interface
    let mw_a_line = format!(r#""{LOG_IFACE}": mw-a["{LOG_IFACE}"]"#);
    let mw_b_line = format!(r#""{LOG_IFACE}": mw-b["{LOG_IFACE}"]"#);
    assert!(wac.contains(&mw_a_line), "mw-a should be in the chain");
    assert!(wac.contains(&mw_b_line), "mw-b should be in the chain");
    // log-provider-inner is still present as the chain root
    assert!(wac.contains("let log-provider-inner = new my:log-provider-inner {"));
}

#[test]
fn test_2a_produces_warn() {
    let results = scenario_2a_warn_no_path();
    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], ContractResult::Warn(_)),
        "expected Warn, got {:?}",
        results[0]
    );
}

#[test]
fn test_2b_produces_ok() {
    let results = scenario_2b_ok_compatible();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0], ContractResult::Ok);
}

#[test]
fn test_2c_produces_error() {
    let results = scenario_2c_error_incompatible();
    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], ContractResult::Error(_)),
        "expected Error, got {:?}",
        results[0]
    );
}

#[test]
fn test_3a_compatible_wat_produces_ok() {
    let results = scenario_3a_ok_compatible_wat();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0],
        ContractResult::Ok,
        "compatible WAT middleware should produce Ok"
    );
}

#[test]
fn test_3b_incompatible_wat_produces_error() {
    let results = scenario_3b_error_incompatible_wat();
    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], ContractResult::Error(_)),
        "incompatible WAT middleware should produce Error, got {:?}",
        results[0]
    );
}
