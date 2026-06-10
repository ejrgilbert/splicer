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
        bridged_sync_target: false,
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
fn matrix_resource_pass_through() {
    // Resource exercise: method calls on a resource (`&self` is the
    // implicit borrow) dispatch through a per-resource GuestBucket
    // impl on a wrapper newtype. Interface-level fns returning a
    // resource (`open`) round-trip through the wrapper newtype as
    // the strategy R, then re-wrap to the export-side resource at
    // the boundary. `open-maybe -> result<bucket, string>` exercises
    // the Result<resource, E> compound shape.
    let out = generate_for_wit(
        r#"
            package matrix:res-bucket@0.1.0;
            interface store {
                resource bucket {
                    constructor(name: string);
                    get: async func(key: string) -> option<list<u8>>;
                    put: async func(key: string, val: list<u8>);
                }
                open: async func(name: string) -> bucket;
                open-maybe: async func(name: string) -> result<bucket, string>;
            }
            world w { export store; }
        "#,
        "w",
        "matrix:res-bucket/store@0.1.0",
        "bindings::matrix::res_bucket::store::open",
        Behavior::Transform,
    );
    // prettyplease wraps long expressions across lines; collapse to
    // single-line form before substring-matching the codegen shape.
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    // Wrapper newtype is emitted as `WrapperBucket(pub <import-path>::Bucket)`.
    assert!(
        oneline.contains("pub struct WrapperBucket")
            && oneline.contains("bindings::matrix::res_bucket::store::Bucket"),
        "expected WrapperBucket newtype over import-side Bucket:\n{}",
        out.lib_rs,
    );
    // Interface-level Guest impl declares `type Bucket = WrapperBucket;`.
    assert!(
        oneline.contains("type Bucket = WrapperBucket"),
        "expected `type Bucket = WrapperBucket` assoc type in Guest impl:\n{}",
        out.lib_rs,
    );
    // Per-resource GuestBucket impl is emitted on the wrapper newtype.
    assert!(
        oneline.contains("GuestBucket for WrapperBucket"),
        "expected per-resource GuestBucket impl on WrapperBucket:\n{}",
        out.lib_rs,
    );
    // Per-resource args structs use the resource name as prefix.
    for ident in ["BucketGetArgs", "BucketPutArgs", "BucketNewArgs"] {
        assert!(
            oneline.contains(ident),
            "expected resource-prefixed args struct `{ident}`:\n{}",
            out.lib_rs,
        );
    }
    // The closure body for `open` constructs the intermediate via
    // `WrapperBucket(<import>::open(...))` and the final wrap calls
    // `<export>::Bucket::new(intermediate)`.
    assert!(
        oneline.contains("WrapperBucket( bindings::matrix::res_bucket::store::open")
            || oneline.contains("WrapperBucket(bindings::matrix::res_bucket::store::open"),
        "expected closure to wrap import-side open() into WrapperBucket:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("bindings::exports::matrix::res_bucket::store::Bucket::new(intermediate)"),
        "expected outer wrap to re-wrap intermediate into export-side Bucket:\n{}",
        out.lib_rs,
    );
    // result<bucket, _>: match Ok→WrapperBucket(x) forward,
    // match Ok→Bucket::new(x) reverse, Err passes through.
    assert!(
        oneline.contains("WrapperBucket(x)"),
        "expected Ok-arm forward to wrap inner value into WrapperBucket(x):\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("bindings::exports::matrix::res_bucket::store::Bucket::new(x)"),
        "expected Ok-arm reverse to call Bucket::new(x):\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("Result::Err(x) => ::core::result::Result::Err(x)"),
        "expected Err-arm to pass through unchanged:\n{}",
        out.lib_rs,
    );
    // Per-resource methods use the captured `&self`, not an args
    // field.
    assert!(
        oneline.contains("self.0.get(args.key)"),
        "expected GuestBucket::get closure body to call self.0.get(args.key):\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_resource_virtualize() {
    // Tier-4 (Virtualize) over a resource-bearing interface: the
    // wrapper newtype's inner field is `MockedResource` (no import
    // side exists), per-resource async methods dispatch via
    // `VirtualizeStrategy`, the sync constructor synthesizes a
    // stand-in `MockedResource` (no strategy dispatch, since
    // constructors are sync per the WIT spec), and the codegen emits
    // a `WitTypedWithResources` impl that decodes a recorded
    // `Cell::ResourceHandle` into the wrapper newtype.
    //
    // Exercises the four return shapes a tier-4 strategy may
    // synthesize: bare `bucket`, `option<bucket>`, `result<bucket, _>`,
    // `list<bucket>`.
    let out = generate_for_wit(
        r#"
            package matrix:res-virt@0.1.0;
            interface store {
                resource bucket {
                    constructor(name: string);
                    get: async func(key: string) -> option<list<u8>>;
                }
                open: async func(name: string) -> bucket;
                open-maybe: async func(name: string) -> result<bucket, string>;
                open-opt: async func(name: string) -> option<bucket>;
                open-many: async func(prefix: string) -> list<bucket>;
            }
            world w { export store; }
        "#,
        "w",
        "matrix:res-virt/store@0.1.0",
        // Behavior::Virtualize does NOT import the target, so the
        // generate_for_wit helper asserts the import path is absent.
        "bindings::matrix::res_virt::store::open",
        Behavior::Virtualize,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    // The wrapper newtype is `WrapperBucket(pub MockedResource)` — no
    // bindings::iface::Bucket inner anywhere.
    assert!(
        oneline.contains("pub struct WrapperBucket")
            && oneline.contains("::splicer_tool_sdk::MockedResource"),
        "expected WrapperBucket(pub MockedResource):\n{}",
        out.lib_rs,
    );
    assert!(
        !oneline.contains("WrapperBucket(pub bindings"),
        "tier-4 wrapper newtype must not reference import-side bindings:\n{}",
        out.lib_rs,
    );

    // The impl is wired through the SDK's
    // `impl_wit_typed_with_resources_for_wrapper!` macro: the body
    // lives in the SDK, the codegen just emits the one-line
    // invocation per WIT resource.
    assert!(
        oneline.contains("impl_wit_typed_with_resources_for_wrapper!(WrapperBucket, \"bucket\")"),
        "expected the SDK macro invocation per-WIT-resource:\n{}",
        out.lib_rs,
    );

    // VirtualizeStrategy dispatch — no TransformStrategy anywhere.
    assert!(
        oneline.contains("VirtualizeStrategy"),
        "expected VirtualizeStrategy dispatch:\n{}",
        out.lib_rs,
    );
    assert!(
        !oneline.contains("TransformStrategy"),
        "tier-4 emission must not reference TransformStrategy:\n{}",
        out.lib_rs,
    );

    // The sync constructor goes through the SDK macro that owns the
    // counter + Cow shape. Codegen just emits the invocation.
    assert!(
        oneline.contains("mint_mock_resource!(WrapperBucket, \"bucket\")"),
        "expected sync constructor to delegate to mint_mock_resource! macro:\n{}",
        out.lib_rs,
    );

    // Resource-returning interface fns get the same intermediate-
    // wrapping treatment regardless of tier: strategy returns the
    // wrapper newtype, codegen does `Bucket::new(intermediate)`.
    assert!(
        oneline.contains("bindings::exports::matrix::res_virt::store::Bucket::new(intermediate)"),
        "expected outer wrap to re-wrap intermediate into export-side Bucket:\n{}",
        out.lib_rs,
    );
    // Four return shapes through the recursive wrap: bare, result,
    // option, list.
    let bucket_new = "bindings::exports::matrix::res_virt::store::Bucket::new(x)";
    assert!(
        oneline.contains(bucket_new),
        "expected reverse transform to call Bucket::new(x) at resource leaves:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains(&format!(".map(|x| {bucket_new})")),
        "expected option<bucket> + list<bucket> reverse to use .map(|x| Bucket::new(x)):\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains(".into_iter()") && oneline.contains(".collect::<::std::vec::Vec<_>>()"),
        "expected list<bucket> reverse to use .into_iter().map().collect::<Vec<_>>():\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_borrow_args_thread_lifetime() {
    // `borrow<R>` lowers to wit-bindgen's `BucketBorrow<'_>` companion
    // struct. The wrapper crate top-level args struct must (1) render
    // the field type as the absolute `bindings::iface::BucketBorrow<'a>`,
    // (2) parameterize itself by `<'a>` so the field's lifetime has a
    // binding, and (3) instantiate as `Args<'_>` at the strategy
    // dispatch site so the closure / virtualize call infers the live
    // lifetime. Exercises both interface-level (`compare-buckets`)
    // and per-resource (`copy-from`) borrow positions.
    let out = generate_for_wit(
        r#"
            package matrix:res-borrow@0.1.0;
            interface store {
                resource bucket {
                    constructor(name: string);
                    copy-from: async func(src: borrow<bucket>);
                }
                compare-buckets: async func(a: borrow<bucket>, b: borrow<bucket>) -> bool;
            }
            world w { export store; }
        "#,
        "w",
        "matrix:res-borrow/store@0.1.0",
        "bindings::matrix::res_borrow::store::compare_buckets",
        Behavior::Transform,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    // Both args structs declare `<'a>` since they each carry a
    // BucketBorrow field. The freestanding `compare-buckets`
    // synthesizes `StoreCompareBucketsArgs<'a>`; the per-resource
    // `copy-from` synthesizes `BucketCopyFromArgs<'a>`.
    for sig in [
        "pub struct StoreCompareBucketsArgs<'a>",
        "pub struct BucketCopyFromArgs<'a>",
    ] {
        assert!(
            oneline.contains(sig),
            "expected lifetime-parameterized args decl `{sig}`:\n{}",
            out.lib_rs,
        );
    }

    // Field type renders to the absolute BucketBorrow path with the
    // hardcoded `'a` from the IR. Exported resources nest under
    // `exports::`, mirroring wit-bindgen's module shape.
    assert!(
        oneline.contains("bindings::exports::matrix::res_borrow::store::BucketBorrow<'a>"),
        "expected absolute BucketBorrow<'a> in field types:\n{}",
        out.lib_rs,
    );

    // Strategy dispatch instantiates the args type as
    // `Args<'_>`: the placeholder lets Rust infer the binding from
    // the trait method's borrow inputs at the call site.
    assert!(
        oneline.contains("TransformStrategy< StoreCompareBucketsArgs<'_>"),
        "expected dispatch to instantiate StoreCompareBucketsArgs<'_>:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("TransformStrategy< BucketCopyFromArgs<'_>"),
        "expected dispatch to instantiate BucketCopyFromArgs<'_>:\n{}",
        out.lib_rs,
    );

    // Args-struct WitTyped impls are suppressed because the fields
    // contain a Handle. The existing `contains_handle` rule covers
    // this; verify the impl didn't sneak in.
    assert!(
        !oneline.contains("WitTyped for StoreCompareBucketsArgs"),
        "args struct with borrow field must NOT get a WitTyped impl:\n{}",
        out.lib_rs,
    );
    assert!(
        !oneline.contains("WitTyped for BucketCopyFromArgs"),
        "args struct with borrow field must NOT get a WitTyped impl:\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_tier4_static_method_fails_fast_with_compile_error() {
    // tier-4 has no import side, so a static method body has nowhere
    // to dispatch (and no fixture / design exercises strategy
    // dispatch from a static yet). The codegen must emit a
    // `compile_error!` into the wrapper crate so the build error
    // points at the unsupported shape, rather than producing a call
    // to a nonexistent import that surfaces as a misleading
    // unresolved-name error.
    let out = generate_for_wit(
        r#"
            package matrix:s4@0.1.0;
            interface store {
                resource bucket {
                    constructor();
                    info: static func() -> string;
                }
            }
            world w { export store; }
        "#,
        "w",
        "matrix:s4/store@0.1.0",
        // No target import path to check; just ensure the helper's
        // post-condition (no import for Virtualize) holds.
        "bindings::matrix::s4::store::info",
        Behavior::Virtualize,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        oneline.contains("compile_error")
            && oneline.contains("tier-4 static methods on resources are not yet supported"),
        "expected a compile_error! at the tier-4 static method site:\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_own_resource_arg_emits_structural_codegen() {
    // Structural pin only. Whether the closure-body call typechecks
    // depends on wit-bindgen's re-exported-resource handling and
    // needs a cargo-check integration test to verify.
    let out = generate_for_wit(
        r#"
            package matrix:res-own-arg@0.1.0;
            interface store {
                resource bucket {
                    constructor(name: string);
                }
                push: async func(b: bucket);
            }
            world w { export store; }
        "#,
        "w",
        "matrix:res-own-arg/store@0.1.0",
        "bindings::matrix::res_own_arg::store::push",
        Behavior::Transform,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    // Args struct field type renders as the export-side Bucket path.
    assert!(
        oneline.contains("pub struct StorePushArgs"),
        "expected args struct for push:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("pub b: bindings::exports::matrix::res_own_arg::store::Bucket"),
        "expected own<bucket> arg field to render as export-side Bucket path:\n{}",
        out.lib_rs,
    );

    // Closure body forwards `args.b` to the import-side push call.
    assert!(
        oneline.contains("bindings::matrix::res_own_arg::store::push(args.b).await"),
        "expected closure body to call import-side push(args.b):\n{}",
        out.lib_rs,
    );

    // No WitTyped impl on the args struct (it carries a handle).
    assert!(
        !oneline.contains("WitTyped for StorePushArgs"),
        "args struct with own<R> arg must not get a WitTyped impl:\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_zero_arg_constructor_tier4() {
    // Zero-arg sentinel macro + tier-4 mint macro composing in the
    // same body.
    let out = generate_for_wit(
        r#"
            package matrix:res-zero-ctor@0.1.0;
            interface store {
                resource bucket {
                    constructor();
                    poke: async func();
                }
            }
            world w { export store; }
        "#,
        "w",
        "matrix:res-zero-ctor/store@0.1.0",
        "bindings::matrix::res_zero_ctor::store::Bucket::new",
        Behavior::Virtualize,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        oneline.contains("pub struct BucketNewArgs {}"),
        "expected zero-arg args struct decl:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("impl_wit_typed_for_zero_arg_args!(BucketNewArgs)"),
        "expected zero-arg sentinel macro invocation:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("let args = BucketNewArgs {}"),
        "expected empty args literal in constructor body:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("mint_mock_resource!(WrapperBucket, \"bucket\")"),
        "expected tier-4 mint_mock_resource invocation in constructor:\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_multi_resource_with_cross_resource_borrow() {
    // Two resources + a method on one taking a borrow of the other:
    // pins per-resource codegen isolation and cross-resource ident
    // rewrites in AbsolutizeResources.
    let out = generate_for_wit(
        r#"
            package matrix:res-multi@0.1.0;
            interface store {
                resource conn {
                    constructor(url: string);
                    ping: async func();
                }
                resource bucket {
                    constructor(name: string);
                    attach-conn: async func(c: borrow<conn>);
                }
                open-bucket: async func(name: string) -> bucket;
                open-conn: async func(url: string) -> conn;
            }
            world w { export store; }
        "#,
        "w",
        "matrix:res-multi/store@0.1.0",
        "bindings::matrix::res_multi::store::open_bucket",
        Behavior::Transform,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        oneline.contains("pub struct WrapperBucket") && oneline.contains("pub struct WrapperConn"),
        "expected per-resource wrapper newtypes for both resources:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("GuestBucket for WrapperBucket")
            && oneline.contains("GuestConn for WrapperConn"),
        "expected per-resource GuestX impls for both resources:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("type Bucket = WrapperBucket")
            && oneline.contains("type Conn = WrapperConn"),
        "expected interface Guest impl to declare both assoc types:\n{}",
        out.lib_rs,
    );

    // Cross-resource borrow: `bucket.attach-conn` carries
    // `ConnBorrow<'a>` (not `BucketBorrow<'a>`).
    assert!(
        oneline.contains("pub struct BucketAttachConnArgs<'a>"),
        "expected BucketAttachConnArgs to be parameterized by 'a:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("bindings::exports::matrix::res_multi::store::ConnBorrow<'a>"),
        "expected the borrow field to render the cross-resource Conn path:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("TransformStrategy< BucketAttachConnArgs<'_>")
            || oneline.contains("TransformStrategy<BucketAttachConnArgs<'_>"),
        "expected dispatch to instantiate the lifetime-parameterized args:\n{}",
        out.lib_rs,
    );

    for ident in [
        "BucketNewArgs",
        "BucketAttachConnArgs",
        "ConnNewArgs",
        "ConnPingArgs",
        "StoreOpenBucketArgs",
        "StoreOpenConnArgs",
    ] {
        assert!(
            oneline.contains(ident),
            "expected args ident `{ident}`:\n{}",
            out.lib_rs,
        );
    }
}

#[test]
fn matrix_value_typed_user_types_get_dual_impls() {
    // Value-typed user records / enums / variants / flags get both
    // WitTyped and WitTypedWithResources. The latter goes through
    // the SDK's via-wave macro, which delegates to cells_to_typed.
    let out = generate_for_wit(
        r#"
            package matrix:dual@0.1.0;
            interface ops {
                record point { x: u32, y: u32 }
                enum color { red, green, blue }
                variant outcome { miss, hit(u32) }
                flags perms { read, write, exec-x }
                pack: async func(p: point, c: color, o: outcome, perms: perms) -> u32;
            }
            world w { export ops; }
        "#,
        "w",
        "matrix:dual/ops@0.1.0",
        "bindings::matrix::dual::ops::pack",
        Behavior::Transform,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    // Every value-typed user type gets the WTWR macro invocation
    // alongside its WitTyped impl.
    for ty_path in [
        "bindings::exports::matrix::dual::ops::Point",
        "bindings::exports::matrix::dual::ops::Color",
        "bindings::exports::matrix::dual::ops::Outcome",
        "bindings::exports::matrix::dual::ops::Perms",
    ] {
        assert!(
            oneline.contains(&format!("WitTyped for {ty_path}")),
            "expected WitTyped impl for {ty_path}:\n{}",
            out.lib_rs,
        );
        // The macro invocation gets line-wrapped by prettyplease when
        // the type path is long; check the two pieces separately.
        assert!(
            oneline.contains("impl_wit_typed_with_resources_via_wave!")
                && oneline.contains(ty_path),
            "expected WTWR via-wave macro to reference {ty_path}:\n{}",
            out.lib_rs,
        );
    }
}

#[test]
fn matrix_handle_bearing_args_skip_both_witty_and_wtwr() {
    // Args structs with a borrow field already skip the WitTyped
    // impl (`contains_handle` short-circuits). The WTWR via-wave
    // macro must skip too — it routes through cells_to_typed which
    // requires WitTyped.
    let out = generate_for_wit(
        r#"
            package matrix:dual-skip@0.1.0;
            interface store {
                resource bucket {
                    constructor(name: string);
                    copy-from: async func(src: borrow<bucket>);
                }
            }
            world w { export store; }
        "#,
        "w",
        "matrix:dual-skip/store@0.1.0",
        "self.0.copy_from(args.src)",
        Behavior::Transform,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        !oneline.contains("WitTyped for BucketCopyFromArgs"),
        "handle-bearing args must not get a WitTyped impl:\n{}",
        out.lib_rs,
    );
    assert!(
        !oneline.contains("impl_wit_typed_with_resources_via_wave!(BucketCopyFromArgs"),
        "handle-bearing args must not get a WTWR via-wave impl:\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_resource_method_returns_resource() {
    // Resource methods returning a resource: the wrap fires from
    // the per-resource trait kind, not just Interface.
    let out = generate_for_wit(
        r#"
            package matrix:res-clone@0.1.0;
            interface store {
                resource bucket {
                    constructor(name: string);
                    clone: async func() -> bucket;
                }
            }
            world w { export store; }
        "#,
        "w",
        "matrix:res-clone/store@0.1.0",
        "self.0.clone()",
        Behavior::Transform,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    // Strategy R is the wrapper newtype, not Resource<WrapperBucket>.
    assert!(
        oneline.contains("TransformStrategy< BucketCloneArgs, WrapperBucket")
            || oneline.contains("TransformStrategy<BucketCloneArgs, WrapperBucket"),
        "expected method dispatch to use WrapperBucket as R:\n{}",
        out.lib_rs,
    );
    // Closure body wraps the import-side `self.0.clone().await` into
    // the wrapper newtype.
    assert!(
        oneline.contains("WrapperBucket(self.0.clone().await)"),
        "expected closure body to wrap self.0.clone().await into WrapperBucket:\n{}",
        out.lib_rs,
    );
    // Final wrap calls Bucket::new on the strategy-returned intermediate.
    assert!(
        oneline.contains("bindings::exports::matrix::res_clone::store::Bucket::new(intermediate)"),
        "expected final wrap to call Bucket::new(intermediate):\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_resource_static_factory_returns_resource() {
    // `[static] bucket.anonymous -> bucket`: wrap fires from the
    // static-method body too, not just methods.
    let out = generate_for_wit(
        r#"
            package matrix:res-stat@0.1.0;
            interface store {
                resource bucket {
                    constructor(name: string);
                    anonymous: static async func() -> bucket;
                }
            }
            world w { export store; }
        "#,
        "w",
        "matrix:res-stat/store@0.1.0",
        "bindings::matrix::res_stat::store::Bucket::anonymous",
        Behavior::Transform,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        oneline.contains("TransformStrategy< BucketAnonymousArgs, WrapperBucket")
            || oneline.contains("TransformStrategy<BucketAnonymousArgs, WrapperBucket"),
        "expected static dispatch to use WrapperBucket as R:\n{}",
        out.lib_rs,
    );
    // prettyplease can wrap the WrapperBucket(...) call across lines
    // when the inner call is long; assert on the two pieces.
    assert!(
        oneline.contains("WrapperBucket(")
            && oneline.contains("bindings::matrix::res_stat::store::Bucket::anonymous().await"),
        "expected static closure body to wrap import-side factory call:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("bindings::exports::matrix::res_stat::store::Bucket::new(intermediate)"),
        "expected final wrap to call Bucket::new(intermediate):\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_resource_in_tuple_return() {
    // Mixed value + resource elements; wrap destructures and
    // rebuilds.
    let out = generate_for_wit(
        r#"
            package matrix:res-tuple@0.1.0;
            interface store {
                resource bucket {
                    constructor(name: string);
                    poke: async func();
                }
                open-tagged: async func(name: string) -> tuple<u32, bucket>;
            }
            world w { export store; }
        "#,
        "w",
        "matrix:res-tuple/store@0.1.0",
        "bindings::matrix::res_tuple::store::open_tagged",
        Behavior::Transform,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        oneline.contains("(u32, WrapperBucket)") || oneline.contains("(u32 , WrapperBucket)"),
        "expected intermediate type `(u32, WrapperBucket)`:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("let __t = ") && oneline.contains("WrapperBucket(__t.1)"),
        "expected forward transform to bind tuple and wrap element 1:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("bindings::exports::matrix::res_tuple::store::Bucket::new(__t.1)"),
        "expected reverse transform to call Bucket::new(__t.1):\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_resource_in_nested_compound_return() {
    // `result<option<bucket>, string>` composes `match` (Result)
    // with `.map(|x| …)` (Option) at the same call site.
    let out = generate_for_wit(
        r#"
            package matrix:res-nested@0.1.0;
            interface store {
                resource bucket {
                    constructor(name: string);
                    poke: async func();
                }
                open-maybe-opt: async func(name: string) -> result<option<bucket>, string>;
            }
            world w { export store; }
        "#,
        "w",
        "matrix:res-nested/store@0.1.0",
        "bindings::matrix::res_nested::store::open_maybe_opt",
        Behavior::Transform,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        oneline.contains("Option<WrapperBucket>"),
        "expected `Option<WrapperBucket>` in nested intermediate type:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains(".map(|x| WrapperBucket(x))"),
        "expected forward to map inner Option with WrapperBucket(x):\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains(".map(|x| bindings::exports::matrix::res_nested::store::Bucket::new"),
        "expected reverse to map inner Option with Bucket::new at the leaf:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("Result::Err(x) => ::core::result::Result::Err(x)"),
        "expected Err arm to pass through unchanged:\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_resource_named_borrow_is_not_misrewritten() {
    // Regression: an `XxxBorrow`-named resource in own position must
    // resolve to `bindings::iface::XxxBorrow` (its own type), not
    // `bindings::iface::Xxx` via the suffix-strip path. Exact-match
    // takes precedence; suffix-strip is the fallback.
    let out = generate_for_wit(
        r#"
            package matrix:nb@0.1.0;
            interface store {
                resource foo-borrow {
                    constructor();
                    poke: async func();
                }
                lend: async func() -> foo-borrow;
            }
            world w { export store; }
        "#,
        "w",
        "matrix:nb/store@0.1.0",
        "bindings::matrix::nb::store::lend",
        Behavior::Transform,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");
    // The wrapper newtype references the resource's actual ident, not
    // a stripped-suffix variant.
    assert!(
        oneline.contains("bindings::matrix::nb::store::FooBorrow"),
        "expected the literal `FooBorrow` resource path in tier-3 newtype:\n{}",
        out.lib_rs,
    );
    assert!(
        !oneline.contains("bindings::matrix::nb::store::Foo)")
            && !oneline.contains("bindings::matrix::nb::store::Foo>"),
        "must NOT rewrite `FooBorrow` (own resource) as `Foo` via suffix-strip:\n{}",
        out.lib_rs,
    );
}

#[test]
fn matrix_resource_virtualize_with_borrow() {
    // Combined: tier-4 virtualize + borrow arg. Exercises the
    // interaction between `Args<'a>` and `VirtualizeStrategy` (which
    // may .await between accepting args and producing R). The wrapper
    // crate must still type-check.
    let out = generate_for_wit(
        r#"
            package matrix:res-virt-borrow@0.1.0;
            interface store {
                resource bucket {
                    constructor(name: string);
                    copy-from: async func(src: borrow<bucket>);
                }
                compare-buckets: async func(a: borrow<bucket>, b: borrow<bucket>) -> bool;
            }
            world w { export store; }
        "#,
        "w",
        "matrix:res-virt-borrow/store@0.1.0",
        "bindings::matrix::res_virt_borrow::store::compare_buckets",
        Behavior::Virtualize,
    );
    let oneline: String = out.lib_rs.split_whitespace().collect::<Vec<_>>().join(" ");

    // tier-4: inner is MockedResource, dispatch is VirtualizeStrategy,
    // borrow args carry through the same lifetime threading.
    assert!(
        oneline.contains("VirtualizeStrategy< StoreCompareBucketsArgs<'_>"),
        "expected VirtualizeStrategy dispatch with lifetime-parameterized args:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("VirtualizeStrategy< BucketCopyFromArgs<'_>"),
        "expected per-resource VirtualizeStrategy dispatch with lifetime args:\n{}",
        out.lib_rs,
    );
    assert!(
        oneline.contains("pub struct WrapperBucket(pub ::splicer_tool_sdk::MockedResource)"),
        "expected tier-4 WrapperBucket with MockedResource inner:\n{}",
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
