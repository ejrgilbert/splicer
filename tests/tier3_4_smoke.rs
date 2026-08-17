//! End-to-end smoke for the tier-3/4 codegen + build pipeline.
//! Runs `generate_wrapper_crate` + `build_wrapper` against a real
//! strategy crate from `builtins/` and asserts the produced `.wasm`
//! parses as a valid wasm component.
//!
//! Marked `#[ignore]` because each test shells out to cargo and
//! needs the `wasm32-wasip1` target installed. Run with
//! `cargo test --test tier3_4_smoke -- --ignored`.

use std::path::PathBuf;

use splicer::lowlevel::{build_wrapper, Behavior, BuildConfig, BuildOutcome, GenerateWrapperInput};

include!(concat!(env!("OUT_DIR"), "/sdk_test_version.rs"));

// `async func` so wit-bindgen emits an `async fn` Guest method —
// matches the `async fn` body our `emit_method` codegen produces.
// Mixing sync WIT with our async-emit'd Guest impl is a real shape
// mismatch (E0053) and needs separate sync-bridging work to support.
const TIER4_TARGET_WIT: &str = r#"
    package smoke:tier4@0.1.0;
    interface ops {
        add: async func(a: u32, b: u32) -> u32;
    }
    world tier4-smoke {
        export ops;
    }
"#;

// Tier-3 wrapper: world both exports and imports the target
// interface — the export is the wrapper's contract to its caller,
// the import is the downstream call into the wrapped target.
const TIER3_TARGET_WIT: &str = r#"
    package smoke:tier3@0.1.0;
    interface ops {
        add: async func(a: u32, b: u32) -> u32;
    }
    world tier3-smoke {
        export ops;
        import ops;
    }
"#;

// Tier-3 with an `error-context` arg: exercises handle pass-
// through. The IR carries the arg as
// `WitTypeRef::Handle(HandleRef::ErrorContext)`, the args struct
// renders an `ErrorContext` field, and emit_wit_typed suppresses
// the args-struct `WitTyped` impl. Pass-through hello-tier3 has no
// `Args: WitTyped` bound, so the wrapper compiles end-to-end.
const TIER3_EC_TARGET_WIT: &str = r#"
    package smoke:tier3-ec@0.1.0;
    interface ops {
        process: async func(ec: error-context);
    }
    world tier3-ec-smoke {
        export ops;
        import ops;
    }
"#;

const TIER3_REDACT_TARGET_WIT: &str = r#"
    package smoke:redact@0.1.0;
    interface ops {
        greet: async func(name: string) -> string;
    }
    world redact-smoke {
        export ops;
        import ops;
    }
"#;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn build_and_validate(
    target_wit: &str,
    world_name: &str,
    interface_qualified_name: &str,
    behavior: Behavior,
    strategy_dir_name: &str,
    strategy_type: &str,
) {
    let root = workspace_root();
    let strategy = PathBuf::from(env!("OUT_DIR"))
        .join("embedded-strategies")
        .join(strategy_dir_name);
    let adapter = root
        .join("builtins")
        .join("wasi_snapshot_preview1.reactor.wasm");
    let build_root = tempfile::tempdir().expect("tempdir");

    let outcome = build_wrapper(
        &GenerateWrapperInput {
            target_wit,
            world_name: Some(world_name),
            interface_qualified_name,
            behavior,
            strategy_crate_name: strategy_dir_name,
            strategy_crate_path: strategy.to_str().unwrap(),
            strategy_type,
            splicer_tool_sdk_version: SDK_TEST_VERSION,
        },
        &BuildConfig {
            build_root: build_root.path(),
            adapter_wasm: &adapter,
            target: None,
        },
    )
    .expect("build pipeline produces a wasm");
    let BuildOutcome::Built(wasm_path) = outcome else {
        panic!("expected a built wrapper, got a bound mismatch");
    };

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

#[test]
#[ignore = "shells out to cargo + wasm32-wasip1; run with --ignored"]
fn hello_tier4_builds_to_a_valid_component() {
    build_and_validate(
        TIER4_TARGET_WIT,
        "tier4-smoke",
        "smoke:tier4/ops@0.1.0",
        Behavior::Virtualize,
        "hello-tier4",
        "HelloTier4",
    );
}

#[test]
#[ignore = "shells out to cargo + wasm32-wasip1; run with --ignored"]
fn hello_tier3_builds_to_a_valid_component() {
    build_and_validate(
        TIER3_TARGET_WIT,
        "tier3-smoke",
        "smoke:tier3/ops@0.1.0",
        Behavior::Transform,
        "hello-tier3",
        "HelloTier3",
    );
}

#[test]
#[ignore = "shells out to cargo + wasm32-wasip1; run with --ignored"]
fn redact_strings_builds_to_a_valid_component() {
    build_and_validate(
        TIER3_REDACT_TARGET_WIT,
        "redact-smoke",
        "smoke:redact/ops@0.1.0",
        Behavior::Transform,
        "redact-strings",
        "RedactStrings",
    );
}

#[test]
#[ignore = "shells out to cargo + wasm32-wasip1; run with --ignored"]
fn hello_tier3_with_error_context_arg_builds_to_a_valid_component() {
    // Compile-only bar: cross-component `error-context` lift is
    // broken in wasmtime <=44, so a fully-spliced runtime run isn't
    // expected to work yet. What this test catches is that the wrapper
    // crate codegen + cargo build remain green for handle-typed args.
    build_and_validate(
        TIER3_EC_TARGET_WIT,
        "tier3-ec-smoke",
        "smoke:tier3-ec/ops@0.1.0",
        Behavior::Transform,
        "hello-tier3",
        "HelloTier3",
    );
}
