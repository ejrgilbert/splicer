//! WIT shape matrix for the typed-codegen pipeline.
//!
//! One fixture per shape; each runs through
//! [`super::generate_wrapper_crate`]. The pipeline parses the
//! emitted lib.rs internally, so a successful return means the
//! output is syntactically well-formed Rust — not that it
//! type-checks. Compile coverage lives in the smoke tests.
//!
//! Fixtures use `async func`: the emitted Guest methods are `async
//! fn`, so a sync WIT here would syntactic-pass but fail to compile
//! end-to-end. Extend the matrix with sync fixtures once sync
//! wrapping lands.

use super::super::{generate_wrapper_crate, Behavior, GenerateWrapperInput, WrapperCrate};

fn generate_for_wit(
    wit: &str,
    world: &str,
    iface: &str,
    func: &str,
    behavior: Behavior,
) -> WrapperCrate {
    let wrapper = generate_wrapper_crate(&GenerateWrapperInput {
        target_wit: wit,
        world_name: Some(world),
        interface_qualified_name: iface,
        behavior,
        strategy_crate_name: "matrix-strategy",
        strategy_crate_path: "/abs/path/to/matrix-strategy",
        strategy_type: "MatrixStrategy",
        splicer_tool_sdk_version: crate::test_consts::SDK_TEST_VERSION,
    })
    .expect("generation succeeds");

    let should_not_import = matches!(behavior, Behavior::Virtualize);
    if should_not_import {
        assert!(
            !wrapper.lib_rs.contains(func),
            "virtualize emission must not call the target import '{func}':\n{}",
            wrapper.lib_rs,
        );
    } else {
        assert!(
            wrapper.lib_rs.contains(func),
            "non-virt emission must call the target import '{func}':\n{}",
            wrapper.lib_rs,
        );
    }

    wrapper
}

#[test]
fn matrix_primitives_as_args_and_return() {
    generate_for_wit(
        r#"
            package matrix:prim@0.1.0;
            interface ops {
                a-bool: async func(v: bool) -> bool;
                a-u8: async func(v: u8) -> u8;
                a-u16: async func(v: u16) -> u16;
                a-u32: async func(v: u32) -> u32;
                a-u64: async func(v: u64) -> u64;
                a-s8: async func(v: s8) -> s8;
                a-s16: async func(v: s16) -> s16;
                a-s32: async func(v: s32) -> s32;
                a-s64: async func(v: s64) -> s64;
                a-f32: async func(v: f32) -> f32;
                a-f64: async func(v: f64) -> f64;
                a-char: async func(v: char) -> char;
                a-string: async func(v: string) -> string;
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:prim/ops@0.1.0",
        "bindings::matrix::prim::ops::a_char",
        Behavior::Transform,
    );
}

#[test]
fn matrix_lists() {
    generate_for_wit(
        r#"
            package matrix:lists@0.1.0;
            interface ops {
                bytes: async func(v: list<u8>) -> list<u8>;
                strings: async func(v: list<string>) -> list<string>;
                nested: async func(v: list<list<u32>>) -> list<list<u32>>;
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:lists/ops@0.1.0",
        "bindings::matrix::lists::ops::bytes",
        Behavior::Transform,
    );
}

#[test]
fn matrix_options() {
    generate_for_wit(
        r#"
            package matrix:opt@0.1.0;
            interface ops {
                one: async func(v: option<string>) -> option<string>;
                nested: async func(v: option<option<u32>>) -> option<option<u32>>;
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:opt/ops@0.1.0",
        "bindings::matrix::opt::ops::nested",
        Behavior::Transform,
    );
}

#[test]
fn matrix_results_all_arm_shapes() {
    generate_for_wit(
        r#"
            package matrix:res@0.1.0;
            interface ops {
                both: async func() -> result<u32, string>;
                ok-only: async func() -> result<u32>;
                err-only: async func() -> result<_, string>;
                neither: async func() -> result;
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:res/ops@0.1.0",
        "bindings::matrix::res::ops::neither",
        Behavior::Transform,
    );
}

#[test]
fn matrix_tuples() {
    generate_for_wit(
        r#"
            package matrix:tup@0.1.0;
            interface ops {
                one: async func(v: tuple<u32>) -> tuple<u32>;
                two: async func(v: tuple<u32, string>) -> tuple<u32, string>;
                three: async func(v: tuple<u32, string, bool>) -> tuple<u32, string, bool>;
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:tup/ops@0.1.0",
        "bindings::matrix::tup::ops::two",
        Behavior::Transform,
    );
}

#[test]
fn matrix_records_with_and_without_dashed_fields() {
    let out = generate_for_wit(
        r#"
            package matrix:rec@0.1.0;
            interface ops {
                record undashed {
                    x: u32,
                    y: u32,
                }
                record dashed {
                    pet-name: string,
                    age-years: u32,
                }
                place: async func(u: undashed, d: dashed);
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:rec/ops@0.1.0",
        "bindings::matrix::rec::ops::place",
        Behavior::Transform,
    );
    // Field-key strings stay kebab; Rust idents are snake.
    assert!(out.lib_rs.contains("\"pet-name\""));
    assert!(out.lib_rs.contains("self.pet_name"));
}

#[test]
fn matrix_enum_and_variant() {
    let out = generate_for_wit(
        r#"
            package matrix:enums@0.1.0;
            interface ops {
                enum color { red, green, blue }
                variant outcome {
                    miss,
                    hit(u32),
                    report(string),
                }
                paint: async func(c: color);
                fetch: async func() -> outcome;
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:enums/ops@0.1.0",
        "bindings::matrix::enums::ops::paint",
        Behavior::Transform,
    );
    assert!(out.lib_rs.contains("Type::enum_ty"));
    assert!(out.lib_rs.contains("Type::variant"));
}

#[test]
fn matrix_flags_single_multiple_kebab() {
    let out = generate_for_wit(
        r#"
            package matrix:flagz@0.1.0;
            interface ops {
                flags lone { only }
                flags many { read, write, exec }
                flags dashy { read-only, write-x }
                use-them: async func(a: lone, b: many, c: dashy);
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:flagz/ops@0.1.0",
        "bindings::matrix::flagz::ops::use_them",
        Behavior::Transform,
    );
    // Spot-check the flag member references end up SHOUTING_SNAKE.
    assert!(out.lib_rs.contains("READ_ONLY"));
    assert!(out.lib_rs.contains("WRITE_X"));
}

#[test]
fn matrix_type_alias_primitive_and_record() {
    // `type x = T;` is transparent in the IR: x doesn't get its
    // own WitTyped impl; uses of x in field positions resolve to
    // T's impl. This test confirms generation still produces a
    // valid wrapper.
    generate_for_wit(
        r#"
            package matrix:alias@0.1.0;
            interface ops {
                record point { x: u32, y: u32 }
                type %id = u32;
                type origin = point;
                fetch: async func(i: %id) -> origin;
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:alias/ops@0.1.0",
        "bindings::matrix::alias::ops::fetch",
        Behavior::Transform,
    );
}

#[test]
fn matrix_zero_arg_function() {
    // Exercises the sentinel-record args path end-to-end: the args
    // record for `noop` has no fields, so its WitTyped impl must emit
    // `Type::record(<sentinel>)` rather than `Type::tuple([])` (which
    // panics inside wasm-wave). Unit-tested in emit_wit_typed; here we
    // confirm the full generation pipeline still produces parseable
    // Rust for the zero-arg shape.
    generate_for_wit(
        r#"
            package matrix:zero@0.1.0;
            interface ops {
                noop: async func();
                hello: async func() -> string;
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:zero/ops@0.1.0",
        "bindings::matrix::zero::ops::noop",
        Behavior::Transform,
    );
}

#[test]
fn matrix_virtualize_behavior() {
    // Behavior::Virtualize is structurally distinct from Transform:
    // method bodies dispatch through VirtualizeStrategy and never
    // build the downstream closure that forwards to the wrapped
    // target. Unit tests cover the emitter; this fixture confirms the
    // full pipeline assembles into parseable Rust under the virtualize
    // branch.
    let out = generate_for_wit(
        r#"
            package matrix:virt@0.1.0;
            interface ops {
                lookup: async func(k: string) -> option<u32>;
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:virt/ops@0.1.0",
        "bindings::matrix::virt::ops::lookup",
        Behavior::Virtualize,
    );
    assert!(
        out.lib_rs.contains("VirtualizeStrategy"),
        "virtualize emission missing VirtualizeStrategy:\n{}",
        out.lib_rs,
    );
    assert!(
        !out.lib_rs.contains("TransformStrategy"),
        "virtualize emission must not import TransformStrategy:\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_error_context_arg_pass_through() {
    // `error-context` in arg position lifts through the IR as
    // `WitTypeRef::Handle(HandleRef::ErrorContext)`, renders to the
    // wit-bindgen async-support `ErrorContext` Rust type, and
    // suppresses the args-struct `WitTyped` impl (handles aren't
    // `WitTyped`). Pass-through Transform strategies still work
    // because their dispatch doesn't bound `Args: WitTyped`.
    let out = generate_for_wit(
        r#"
            package matrix:ec@0.1.0;
            interface ops {
                process: async func(ec: error-context);
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:ec/ops@0.1.0",
        "bindings::matrix::ec::ops::process",
        Behavior::Transform,
    );
    assert!(
        out.lib_rs.contains("ErrorContext"),
        "expected ErrorContext-typed field in args struct:\n{}",
        out.lib_rs,
    );
    assert!(
        out.lib_rs.contains("pub struct OpsProcessArgs"),
        "expected OpsProcessArgs struct decl:\n{}",
        out.lib_rs,
    );
    assert!(
        !out.lib_rs.contains("WitTyped for OpsProcessArgs"),
        "args struct with handle-typed field must NOT get a WitTyped impl:\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_multiple_exported_interfaces() {
    // Two exported interfaces in one world. Exercises the per-Guest
    // loop in `emit_guest` and the multi-impl assembly in
    // `assemble_lib_rs`. NOTE: `interface_qualified_name` is a single
    // string applied to every emitted CallId, so multi-interface
    // wrappers carry the wrong CallId.interface_name today; this test
    // covers the structural codegen path, not CallId correctness.
    let out = generate_for_wit(
        r#"
            package matrix:multi@0.1.0;
            interface ops-a {
                ping: async func() -> u32;
            }
            interface ops-b {
                pong: async func() -> string;
            }
            world w {
                export ops-a;
                export ops-b;
            }
        "#,
        "w",
        "matrix:multi/ops-a@0.1.0",
        "bindings::matrix::multi::ops_a::ping",
        Behavior::Transform,
    );
    // Both Guest impls should land in the assembled lib.rs, under
    // their per-interface module paths.
    assert!(
        out.lib_rs.contains("ops_a::Guest for Wrapper"),
        "missing ops_a Guest impl:\n{}",
        out.lib_rs,
    );
    assert!(
        out.lib_rs.contains("ops_b::Guest for Wrapper"),
        "missing ops_b Guest impl:\n{}",
        out.lib_rs,
    );
    // Per-method args structs should be namespaced by interface.
    assert!(out.lib_rs.contains("OpsAPingArgs"));
    assert!(out.lib_rs.contains("OpsBPongArgs"));
}
