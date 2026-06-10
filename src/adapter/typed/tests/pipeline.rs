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
        splicer_tool_sdk_version: crate::test_consts::SDK_TEST_VERSION,
        bridged_sync_target: false,
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
    assert!(
        lib.contains("define_strategy_singleton!"),
        "missing strategy singleton invocation:\n{lib}"
    );
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
fn chaos_err_composes_against_resource_bearing_interface() {
    // Smoke check for L3c: a `Result<resource, E: Default>`-returning
    // interface, tier-4 virtualize, the chaos-err strategy. The
    // wrapper crate must:
    //   - dispatch through VirtualizeStrategy on the `Result<...>`
    //     return type (whose Ok arm contains a `WrapperBucket`);
    //   - not pull in any of the cells-decoding APIs
    //     (cells_to_typed, cells_to_typed_with_resources, etc.);
    //   - parameterize args by `<'a>` for the borrow argument.
    //
    // The strategy's `R: HasArbitraryErr` bound is satisfied by
    // `Result<WrapperBucket, String>` via
    // `impl<T, E: Arbitrary> HasArbitraryErr for Result<T, E>` in
    // the SDK. The Ok arm's type (`WrapperBucket`) doesn't need
    // `Arbitrary` because the Ok arm is never instantiated.
    let wit = r#"
        package test:store@0.1.0;
        interface store {
            resource bucket {
                constructor(name: string);
                size: async func() -> result<u64, string>;
            }
            open: async func(name: string) -> result<bucket, string>;
            compare: async func(a: borrow<bucket>, b: borrow<bucket>) -> result<bool, string>;
        }
        world w { export store; }
    "#;
    let crate_out = generate_wrapper_crate(&GenerateWrapperInput {
        target_wit: wit,
        world_name: Some("w"),
        interface_qualified_name: "test:store/store@0.1.0",
        behavior: Behavior::Virtualize,
        strategy_crate_name: "chaos-err",
        strategy_crate_path: "/abs/path/to/chaos-err",
        strategy_type: "ChaosErr",
        splicer_tool_sdk_version: crate::test_consts::SDK_TEST_VERSION,
        bridged_sync_target: false,
    })
    .unwrap();
    let lib = &crate_out.lib_rs;

    // tier-4 dispatch only.
    assert!(
        lib.contains("VirtualizeStrategy"),
        "chaos-err wrapper should dispatch through VirtualizeStrategy:\n{lib}"
    );
    assert!(
        !lib.contains("TransformStrategy"),
        "chaos-err wrapper must not dispatch through TransformStrategy:\n{lib}"
    );

    // No cells-decoding helpers in the build path. ChaosErr returns
    // `Err(E::default())`; it never touches the recorded-trace path.
    // The codegen DOES emit a `WitTypedWithResources` impl on
    // `WrapperBucket` as a forward-looking convenience for replay-
    // style strategies; that's fine. What we care about here is the
    // call surface (no helper invocations).
    assert!(
        !lib.contains("::cells_to_typed("),
        "chaos-err must not invoke cells_to_typed:\n{lib}"
    );
    assert!(
        !lib.contains("::cells_to_typed_with_resources("),
        "chaos-err must not invoke cells_to_typed_with_resources:\n{lib}"
    );

    // Borrow-argument lifetime threads through `compare`.
    assert!(
        lib.contains("StoreCompareArgs<'a>"),
        "expected lifetime-parameterized StoreCompareArgs:\n{lib}"
    );
    assert!(
        lib.contains("StoreCompareArgs<'_>"),
        "expected StoreCompareArgs<'_> at dispatch site:\n{lib}"
    );

    // Wrapper newtype is tier-4 shaped (inner is MockedResource).
    assert!(
        lib.contains("pub struct WrapperBucket(pub ::splicer_tool_sdk::MockedResource)"),
        "expected tier-4 WrapperBucket(MockedResource):\n{lib}"
    );
}

#[test]
fn replayer_composes_against_resource_bearing_interface() {
    // The Replayer's `R: WitTypedWithResources` bound is satisfied
    // by `Result<WrapperBucket, String>`: WrapperBucket gets its
    // WTWR impl from the codegen-emitted SDK macro.
    let wit = r#"
        package test:store@0.1.0;
        interface store {
            resource bucket {
                constructor(name: string);
                size: async func() -> result<u64, string>;
            }
            open: async func(name: string) -> result<bucket, string>;
        }
        world w { export store; }
    "#;
    let crate_out = generate_wrapper_crate(&GenerateWrapperInput {
        target_wit: wit,
        world_name: Some("w"),
        interface_qualified_name: "test:store/store@0.1.0",
        behavior: Behavior::Virtualize,
        strategy_crate_name: "replayer",
        strategy_crate_path: "/abs/path/to/replayer",
        strategy_type: "Replayer",
        splicer_tool_sdk_version: crate::test_consts::SDK_TEST_VERSION,
        bridged_sync_target: false,
    })
    .unwrap();
    let lib = &crate_out.lib_rs;

    assert!(
        lib.contains("VirtualizeStrategy"),
        "replayer wrapper should dispatch through VirtualizeStrategy:\n{lib}"
    );
    assert!(
        !lib.contains("TransformStrategy"),
        "replayer wrapper must not dispatch through TransformStrategy:\n{lib}"
    );
    assert!(
        lib.contains("define_strategy_singleton!(replayer::Replayer)"),
        "expected strategy singleton bound to replayer::Replayer:\n{lib}"
    );
    assert!(
        lib.contains("impl_wit_typed_with_resources_for_wrapper!"),
        "expected wrapper-newtype WTWR macro invocation:\n{lib}"
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
