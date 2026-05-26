//! End-to-end pipeline contract: both behaviors of
//! [`generate_wrapper_crate`] produce a complete `WrapperCrate`,
//! and crate-name derivation distinguishes inputs that sanitize
//! identically.

use super::super::{
    generate_wrapper_crate, make_wrapper_crate_name, Behavior, GenerateWrapperInput,
};

const TINY_WIT: &str = r#"
    package test:pkg@0.1.0;
    interface ops {
        add: async func(a: u32, b: u32) -> u32;
    }
    world w { export ops; }
"#;

fn input<'a>(behavior: Behavior) -> GenerateWrapperInput<'a> {
    GenerateWrapperInput {
        target_wit: TINY_WIT,
        world_name: Some("w"),
        interface_qualified_name: "test:pkg/ops@0.1.0",
        behavior,
        strategy_crate_name: "my-strategy",
        strategy_crate_path: "/abs/path/to/my-strategy",
        strategy_type: "MyStrategy",
        splicer_tool_sdk_path: "/abs/path/to/splicer-tool-sdk",
    }
}

#[test]
fn transform_pipeline_produces_a_complete_wrapper_crate() {
    let crate_out = generate_wrapper_crate(&input(Behavior::Transform)).unwrap();
    // Crate name reflects both inputs and is a valid Cargo ident.
    assert!(
        crate_out.crate_name.starts_with("splicer_wrapper_"),
        "crate name: {}",
        crate_out.crate_name
    );
    assert!(crate_out.crate_name.contains("my_strategy"));

    // lib.rs has the pipeline's signature pieces.
    let lib = &crate_out.lib_rs;
    assert!(lib.contains("mod bindings"), "missing bindings:\n{lib}");
    assert!(
        lib.contains("TransformStrategy"),
        "transform pipeline should use TransformStrategy:\n{lib}"
    );
    assert!(lib.contains("OnceLock"), "missing OnceLock storage:\n{lib}");
    assert!(
        lib.contains("bindings::export!"),
        "missing component-export hookup:\n{lib}"
    );

    // Cargo.toml is parseable TOML and lists the strategy as a dep.
    let parsed: toml::Value = toml::from_str(&crate_out.cargo_toml).expect("cargo.toml parses");
    assert!(parsed["dependencies"].get("my-strategy").is_some());
}

#[test]
fn virtualize_pipeline_swaps_strategy_trait() {
    let crate_out = generate_wrapper_crate(&input(Behavior::Virtualize)).unwrap();
    assert!(
        crate_out.lib_rs.contains("VirtualizeStrategy"),
        "virtualize pipeline should use VirtualizeStrategy:\n{}",
        crate_out.lib_rs
    );
    assert!(
        !crate_out.lib_rs.contains("TransformStrategy"),
        "virtualize pipeline should not import TransformStrategy:\n{}",
        crate_out.lib_rs
    );
}

#[test]
fn wrapper_crate_name_distinguishes_inputs_that_sanitize_identically() {
    // Both inputs sanitize to "wasi_http_handler_0_3_0" — the hash
    // suffix is what keeps the two crate names distinct.
    let a = make_wrapper_crate_name("wasi:http/handler@0.3.0", "s");
    let b = make_wrapper_crate_name("wasi-http-handler-0-3-0", "s");
    assert_ne!(a, b);
}
