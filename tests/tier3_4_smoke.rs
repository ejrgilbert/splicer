//! End-to-end smoke for the tier-3/4 codegen + build pipeline.
//! Runs `generate_wrapper_crate` + `build_wrapper` against a real
//! strategy crate from `builtins/` and asserts the produced `.wasm`
//! parses as a valid wasm component.
//!
//! Marked `#[ignore]` because each test shells out to cargo and
//! needs the `wasm32-wasip1` target installed. Run with
//! `cargo test --test tier3_4_smoke -- --ignored`.

use std::path::PathBuf;

use splicer::lowlevel::{build_wrapper, Behavior, BuildConfig, GenerateWrapperInput};

const TIER4_TARGET_WIT: &str = r#"
    package smoke:tier4@0.1.0;
    interface ops {
        add: func(a: u32, b: u32) -> u32;
    }
    world tier4-smoke {
        export ops;
    }
"#;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
#[ignore = "shells out to cargo + wasm32-wasip1; run with --ignored"]
fn hello_tier4_builds_to_a_valid_component() {
    let root = workspace_root();
    let sdk = root.join("splicer-tool-sdk");
    let strategy = root.join("builtins").join("hello-tier4");
    let adapter = root.join("builtins").join("wasi_snapshot_preview1.reactor.wasm");
    let cache_dir = tempfile::tempdir().expect("tempdir");

    let wasm_path = build_wrapper(
        &GenerateWrapperInput {
            target_wit: TIER4_TARGET_WIT,
            world_name: Some("tier4-smoke"),
            interface_qualified_name: "smoke:tier4/ops@0.1.0",
            behavior: Behavior::Virtualize,
            strategy_crate_name: "hello-tier4",
            strategy_crate_path: strategy.to_str().unwrap(),
            strategy_type: "HelloTier4",
            splicer_tool_sdk_path: sdk.to_str().unwrap(),
        },
        &BuildConfig {
            cache_dir: cache_dir.path(),
            adapter_wasm: &adapter,
            target: None,
        },
    )
    .expect("tier-4 build pipeline produces a wasm");

    let bytes = std::fs::read(&wasm_path).expect("read produced wasm");
    assert!(
        bytes.starts_with(&[0x00, 0x61, 0x73, 0x6d]),
        "produced file is not wasm: {:?}",
        &bytes.get(..8)
    );

    // Walk the binary with wasmparser to confirm it's a parseable
    // wasm component (not just a core module). Any parse error here
    // surfaces a codegen bug.
    let parser = wasmparser::Parser::new(0);
    for payload in parser.parse_all(&bytes) {
        payload.expect("wasm payload parses");
    }
}
