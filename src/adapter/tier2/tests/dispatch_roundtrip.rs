//! Dispatch-module roundtrip tests: build a synthetic WAT split,
//! run it through [`build_tier2_adapter`], and validate the resulting
//! component bytes parse + structurally validate.
//!
//! Each test pins one input shape (sync primitives, async indirect
//! params, option / variant / result / list / handle / etc.) so a
//! per-shape regression has a single named owner.

use super::super::build_tier2_adapter;

/// Build a tier-2 adapter for a target with a single sync
/// primitive function (`add(x: u32, y: u32) -> u32`-style), run
/// it through ComponentEncoder, and validate the resulting
/// component bytes round-trip through wasmparser. Confirms the
/// dispatch module — including the on-call invocation + wait
/// loop — produces structurally valid wasm.
#[test]
fn dispatch_module_roundtrips_through_component_encoder() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "add") (param i32 i32) (result i32)
                    local.get 0
                    local.get 1
                    i32.add
                )
            )
            (core instance $i (instantiate $m))
            (alias core export $i "add" (core func $add))
            (type $add-ty (func (param "x" u32) (param "y" u32) (result u32)))
            (func $add-lifted (type $add-ty) (canon lift (core func $add)))
            (instance $api-inst (export "add" (func $add-lifted)))
            (export "my:math/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:math/api@1.0.0" (instance $api "my:math/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:math/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `Cell::TupleOf`: plan-builder + tuple-
/// indices side-table + emit-phase `(ptr, len)` const writes,
/// validated through ComponentEncoder.
#[test]
fn dispatch_module_with_tuple_param_roundtrips() {
    // Flat `tuple<u32, s32>` param + void return; no canonical
    // option `memory` needed for the WAT's lift.
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "consume") (param i32 i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (type $consume-ty (func (param "t" (tuple u32 s32))))
            (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
            (instance $api-inst (export "consume" (func $consume-lifted)))
            (export "my:tup/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:tup/api@1.0.0" (instance $api "my:tup/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:tup/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for tuple param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `Cell::Option` as a param: branching emit
/// dispatch (option-some / option-none) plus the canonical-ABI
/// `[disc, ...flat(T)]` slot ordering. `option<u32>` keeps the
/// canon-lift options minimal (no realloc / memory required).
#[test]
fn dispatch_module_with_option_param_roundtrips() {
    // option<u32> flat = [i32 disc, i32 value].
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "consume") (param i32 i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (type $consume-ty (func (param "o" (option u32))))
            (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
            (instance $api-inst (export "consume" (func $consume-lifted)))
            (export "my:opt/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:opt/api@1.0.0" (instance $api "my:opt/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:opt/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for option param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// Single-flat-slot compound result (`tuple<u32>`) — comes back
/// flat (not via retptr). Compound emit reads from `lcl.result`
/// instead of memory; the regression guard pins both the build
/// and validate paths through the no-retptr Compound branch.
#[test]
fn dispatch_module_with_single_slot_tuple_result_lifts_from_flat() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "one-val") (param i32) (result i32)
                    local.get 0
                )
            )
            (core instance $i (instantiate $m))
            (alias core export $i "one-val" (core func $one))
            (type $one-ty (func (param "x" u32) (result (tuple u32))))
            (func $one-lifted (type $one-ty) (canon lift (core func $one)))
            (instance $api-inst (export "one-val" (func $one-lifted)))
            (export "my:tup1/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:tup1/api@1.0.0" (instance $api "my:tup1/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "my:tup1/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("single-slot tuple result must build via no-retptr Compound, not panic");
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted adapter component should validate");
}

/// End-to-end test for `tuple<...>` as a compound result —
/// drives `is_compound_result(Tuple) → Compound → lift_from_memory`.
/// Result flattens to 2 slots → retptr; canon lift's `memory` +
/// `post-return` options materialize it via the callee-allocates
/// pattern.
#[test]
fn dispatch_module_with_tuple_result_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (memory (export "memory") 1)
                (func (export "two-vals") (param i32) (result i32)
                    i32.const 0x1000
                    local.get 0
                    i32.store
                    i32.const 0x1000
                    i32.const -1
                    i32.store offset=4
                    i32.const 0x1000
                )
                (func (export "cabi_post_two-vals") (param i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "two-vals" (core func $two))
            (alias core export $i "cabi_post_two-vals" (core func $two_post))
            (alias core export $i "memory" (core memory $mem))
            (type $two-ty (func (param "x" u32) (result (tuple u32 s32))))
            (func $two-lifted (type $two-ty)
                (canon lift (core func $two) (memory $mem)
                    (post-return (func $two_post))))
            (instance $api-inst (export "two-vals" (func $two-lifted)))
            (export "my:tup-ret/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:tup-ret/api@1.0.0" (instance $api "my:tup-ret/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:tup-ret/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for tuple result");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `option<T>` as a compound result. Drives
/// `is_compound_result(Option) → Compound → lift_from_memory` and
/// the if/else branching emit at the parent Option cell. Result
/// flattens to 2 slots → retptr; canon lift's `memory` +
/// `post-return` materialize it via the callee-allocates pattern.
#[test]
fn dispatch_module_with_option_result_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (memory (export "memory") 1)
                (func (export "maybe-val") (param i32) (result i32)
                    i32.const 0x1000
                    i32.const 1
                    i32.store
                    i32.const 0x1000
                    local.get 0
                    i32.store offset=4
                    i32.const 0x1000
                )
                (func (export "cabi_post_maybe-val") (param i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "maybe-val" (core func $maybe))
            (alias core export $i "cabi_post_maybe-val" (core func $maybe_post))
            (alias core export $i "memory" (core memory $mem))
            (type $maybe-ty (func (param "x" u32) (result (option u32))))
            (func $maybe-lifted (type $maybe-ty)
                (canon lift (core func $maybe) (memory $mem)
                    (post-return (func $maybe_post))))
            (instance $api-inst (export "maybe-val" (func $maybe-lifted)))
            (export "my:opt-ret/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:opt-ret/api@1.0.0" (instance $api "my:opt-ret/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:opt-ret/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for option result");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `Cell::Flags` as a param. Nominal types
/// (flags / enum / record) must be `(export … (type …))`'d from
/// the api instance — otherwise wit-component's decode rejects
/// the inner instance with `instance not valid to be used as
/// export`. Anonymous types (option / result / tuple) sidestep
/// the rule.
#[test]
fn dispatch_module_with_flags_param_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "consume") (param i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (type $perms (flags "read" "write" "exec"))
            (export $perms-export "fperms" (type $perms))
            (type $consume-ty (func (param "p" $perms-export)))
            (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
            (instance $api-inst
                (export "fperms" (type $perms-export))
                (export "consume" (func $consume-lifted)))
            (export "my:fl/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:fl/api@1.0.0" (instance $api "my:fl/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:fl/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for flags param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// `flags` param **and** `flags` result on the same fn — pins the
/// shared `WrapperLocals.flags_info_base` getting reset across
/// the param-side and result-side plans (mirrors the handle-side
/// `dispatch_module_with_handle_param_and_handle_result_roundtrips`).
#[test]
fn dispatch_module_with_flags_param_and_flags_result_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "thru") (param i32) (result i32) local.get 0)
            )
            (core instance $i (instantiate $m))
            (alias core export $i "thru" (core func $thru))
            (type $perms (flags "read" "write" "exec"))
            (export $perms-export "fperms" (type $perms))
            (type $thru-ty (func (param "p" $perms-export) (result $perms-export)))
            (func $thru-lifted (type $thru-ty) (canon lift (core func $thru)))
            (instance $api-inst
                (export "fperms" (type $perms-export))
                (export "thru" (func $thru-lifted)))
            (export "my:flio/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:flio/api@1.0.0" (instance $api "my:flio/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "my:flio/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect(
        "tier-2 adapter generation should succeed for flags param + flags result on the same fn",
    );
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `Cell::Char` as a param. Drives the
/// utf-8 encoder + per-cell scratch reservation + cell::text emit.
/// `char` flattens to a single i32 (the code point).
#[test]
fn dispatch_module_with_char_param_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "consume") (param i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (type $consume-ty (func (param "c" char)))
            (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
            (instance $api-inst (export "consume" (func $consume-lifted)))
            (export "my:ch/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:ch/api@1.0.0" (instance $api "my:ch/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:ch/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for char param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `list<char>` as a param. Drives the
/// per-list utf-8 scratch realloc + per-iteration scratch-addr
/// staging + the `Prestaged` `CellSideData::Char` branch in
/// `emit_single_slot_cell`. `list<char>` flattens to a `(ptr,
/// len)` pair on the wire — the lift needs canonical-option
/// `memory` to read the list payload.
#[test]
fn dispatch_module_with_char_list_param_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (memory (export "memory") 1)
                (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
                    i32.const 0x4000)
                (func (export "consume") (param i32 i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (alias core export $i "memory" (core memory $mem))
            (alias core export $i "cabi_realloc" (core func $realloc))
            (type $consume-ty (func (param "xs" (list char))))
            (func $consume-lifted (type $consume-ty)
                (canon lift (core func $consume) (memory $mem) (realloc (func $realloc))))
            (instance $api-inst (export "consume" (func $consume-lifted)))
            (export "my:lc/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:lc/api@1.0.0" (instance $api "my:lc/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:lc/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for list<char> param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `Cell::Variant` as a param. Drives the
/// N-way disc dispatch + per-arm case-name + payload writes,
/// plus the per-cell variant-info side-table entry placement.
/// Same nominal-type WAT shape as the flags tests.
#[test]
fn dispatch_module_with_variant_param_roundtrips() {
    // variant shape { circle, sq(u32), tri(u32) } flattens to
    // [i32 disc, i32 (joined u32/u32)] = 2 i32 params.
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "consume") (param i32 i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (type $shape (variant (case "circle") (case "sq" u32) (case "tri" u32)))
            (export $shape-export "shape" (type $shape))
            (type $consume-ty (func (param "s" $shape-export)))
            (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
            (instance $api-inst
                (export "shape" (type $shape-export))
                (export "consume" (func $consume-lifted)))
            (export "my:vt/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:vt/api@1.0.0" (instance $api "my:vt/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:vt/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for variant param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// `require_no_inline_resources` rejects inline-resource
/// interfaces with a clear factored-types pointer.
#[test]
fn dispatch_module_with_inline_resource_bails() {
    let wat = r#"(component
        (component $inner
            (core module $m (func (export "consume") (param i32)))
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (type $r (resource (rep i32)))
            (export $r-export "my-res" (type $r))
            (type $own-r (own $r-export))
            (type $consume-ty (func (param "h" $own-r)))
            (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
            (instance $api-inst
                (export "my-res" (type $r-export))
                (export "consume" (func $consume-lifted)))
            (export "my:rh/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:rh/api@1.0.0" (instance $api "my:rh/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let err = build_tier2_adapter(
        "my:rh/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect_err("inline-resource interface must bail");
    let msg = err.to_string();
    assert!(
        msg.contains("declares resource `my-res` inline"),
        "bail should call out the inline resource; got: {msg}",
    );
    assert!(
        msg.contains("factored-types pattern"),
        "bail should point at the factored-types fix; got: {msg}",
    );
}

/// End-to-end test for `Cell::Handle` as a param using the
/// factored-types pattern (resource in `my:rh/types`, the api
/// `use`s it). WAT shape mirrors what `wasm-tools component new`
/// emits from a real factored WIT — two shim sub-components plus
/// the alias chain that pins resource type identity across both
/// exported instances.
#[test]
fn dispatch_module_with_resource_handle_param_roundtrips() {
    let wat = r#"(component
  (core module $main
(func (export "my:rh/api@1.0.0#consume-own") (param i32))
(func (export "my:rh/api@1.0.0#consume-borrow") (param i32))
(func (export "my:rh/types@1.0.0#[resource-drop]my-res") (param i32))
(memory (export "memory") 1)
(func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) i32.const 0)
  )
  (type $my-res (resource (rep i32)))
  (core instance $main (instantiate $main))
  (alias core export $main "memory" (core memory $memory))
  (component $types-shim
(import "import-type-my-res" (type $r (sub resource)))
(export "my-res" (type $r))
  )
  (instance $types-inst (instantiate $types-shim
(with "import-type-my-res" (type $my-res))
  ))
  (export $types-export "my:rh/types@1.0.0" (instance $types-inst))
  (type $own-r (own $my-res))
  (type $consume-own-ty (func (param "h" $own-r)))
  (alias core export $main "my:rh/api@1.0.0#consume-own" (core func $consume-own-core))
  (alias core export $main "cabi_realloc" (core func $cabi_realloc))
  (func $consume-own (type $consume-own-ty) (canon lift (core func $consume-own-core)))
  (type $borrow-r (borrow $my-res))
  (type $consume-borrow-ty (func (param "h" $borrow-r)))
  (alias core export $main "my:rh/api@1.0.0#consume-borrow" (core func $consume-borrow-core))
  (func $consume-borrow (type $consume-borrow-ty) (canon lift (core func $consume-borrow-core)))
  (alias export $types-export "my-res" (type $r-aliased))
  (component $api-shim
(import "import-type-my-res" (type $r (sub resource)))
(import "import-type-my-res0" (type $r0 (eq 0)))
(type $own-r0 (own 1))
(type $f-own (func (param "h" $own-r0)))
(import "import-func-consume-own" (func $consume-own (type $f-own)))
(type $borrow-r0 (borrow 1))
(type $f-borrow (func (param "h" $borrow-r0)))
(import "import-func-consume-borrow" (func $consume-borrow (type $f-borrow)))
(export $r-export "my-res" (type $r))
(type $own-out (own $r-export))
(type $f-own-out (func (param "h" $own-out)))
(export "consume-own" (func $consume-own) (func (type $f-own-out)))
(type $borrow-out (borrow $r-export))
(type $f-borrow-out (func (param "h" $borrow-out)))
(export "consume-borrow" (func $consume-borrow) (func (type $f-borrow-out)))
  )
  (instance $api-inst (instantiate $api-shim
(with "import-func-consume-own" (func $consume-own))
(with "import-func-consume-borrow" (func $consume-borrow))
(with "import-type-my-res" (type $r-aliased))
(with "import-type-my-res0" (type $my-res))
  ))
  (export "my:rh/api@1.0.0" (instance $api-inst))
)"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "my:rh/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for factored-types resource handle param");
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `Cell::Handle` as a Direct result, using
/// the factored-types pattern (resource in `my:rhret/types`, the
/// api `use`s it). Sync `own<R>` returns flat as i32 (no retptr).
#[test]
fn dispatch_module_with_resource_handle_result_roundtrips() {
    let wat = r#"(component
  (core module $main
(func (export "my:rhret/api@1.0.0#make") (result i32) i32.const 0)
(func (export "my:rhret/types@1.0.0#[resource-drop]my-res") (param i32))
(memory (export "memory") 1)
(func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) i32.const 0)
  )
  (type $my-res (resource (rep i32)))
  (core instance $main (instantiate $main))
  (alias core export $main "memory" (core memory $memory))
  (component $types-shim
(import "import-type-my-res" (type $r (sub resource)))
(export "my-res" (type $r))
  )
  (instance $types-inst (instantiate $types-shim
(with "import-type-my-res" (type $my-res))
  ))
  (export $types-export "my:rhret/types@1.0.0" (instance $types-inst))
  (type $own-r (own $my-res))
  (type $make-ty (func (result $own-r)))
  (alias core export $main "my:rhret/api@1.0.0#make" (core func $make-core))
  (alias core export $main "cabi_realloc" (core func $cabi_realloc))
  (func $make (type $make-ty) (canon lift (core func $make-core)))
  (alias export $types-export "my-res" (type $r-aliased))
  (component $api-shim
(import "import-type-my-res" (type $r (sub resource)))
(import "import-type-my-res0" (type $r0 (eq 0)))
(type $own-in (own 1))
(type $f-in (func (result $own-in)))
(import "import-func-make" (func $make (type $f-in)))
(export $r-export "my-res" (type $r))
(type $own-out (own $r-export))
(type $f-out (func (result $own-out)))
(export "make" (func $make) (func (type $f-out)))
  )
  (instance $api-inst (instantiate $api-shim
(with "import-func-make" (func $make))
(with "import-type-my-res" (type $r-aliased))
(with "import-type-my-res0" (type $my-res))
  ))
  (export "my:rhret/api@1.0.0" (instance $api-inst))
)"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "my:rhret/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for factored-types resource handle result");
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// `own<R>` param **and** `own<R>` result on the same fn — pins the
/// shared `WrapperLocals.handle_info_base` getting reset across the
/// param-side and result-side plans. Two per-call buffers; the
/// param-side write must not survive into the result-side read.
#[test]
fn dispatch_module_with_handle_param_and_handle_result_roundtrips() {
    let wat = r#"(component
  (core module $main
(func (export "my:rhio/api@1.0.0#thru") (param i32) (result i32) local.get 0)
(func (export "my:rhio/types@1.0.0#[resource-drop]my-res") (param i32))
(memory (export "memory") 1)
(func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) i32.const 0)
  )
  (type $my-res (resource (rep i32)))
  (core instance $main (instantiate $main))
  (alias core export $main "memory" (core memory $memory))
  (component $types-shim
(import "import-type-my-res" (type $r (sub resource)))
(export "my-res" (type $r))
  )
  (instance $types-inst (instantiate $types-shim
(with "import-type-my-res" (type $my-res))
  ))
  (export $types-export "my:rhio/types@1.0.0" (instance $types-inst))
  (type $own-r (own $my-res))
  (type $thru-ty (func (param "h" $own-r) (result $own-r)))
  (alias core export $main "my:rhio/api@1.0.0#thru" (core func $thru-core))
  (alias core export $main "cabi_realloc" (core func $cabi_realloc))
  (func $thru (type $thru-ty) (canon lift (core func $thru-core)))
  (alias export $types-export "my-res" (type $r-aliased))
  (component $api-shim
(import "import-type-my-res" (type $r (sub resource)))
(import "import-type-my-res0" (type $r0 (eq 0)))
(type $own-r0 (own 1))
(type $f-thru (func (param "h" $own-r0) (result $own-r0)))
(import "import-func-thru" (func $thru (type $f-thru)))
(export $r-export "my-res" (type $r))
(type $own-out (own $r-export))
(type $f-thru-out (func (param "h" $own-out) (result $own-out)))
(export "thru" (func $thru) (func (type $f-thru-out)))
  )
  (instance $api-inst (instantiate $api-shim
(with "import-func-thru" (func $thru))
(with "import-type-my-res" (type $r-aliased))
(with "import-type-my-res0" (type $my-res))
  ))
  (export "my:rhio/api@1.0.0" (instance $api-inst))
)"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "my:rhio/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect(
        "tier-2 adapter generation should succeed for handle param + handle result on the same fn",
    );
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `Cell::Char` as a Direct result. Drives
/// `is_supported_direct_result(Char) → Direct` + the per-result
/// scratch reservation + utf-8 encoder + cell::text emit. Sync
/// returns char as a flat i32 (no retptr).
#[test]
fn dispatch_module_with_char_result_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "make") (result i32) i32.const 0x4E2D)
            )
            (core instance $i (instantiate $m))
            (alias core export $i "make" (core func $make))
            (type $make-ty (func (result char)))
            (func $make-lifted (type $make-ty) (canon lift (core func $make)))
            (instance $api-inst (export "make" (func $make-lifted)))
            (export "my:chret/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:chret/api@1.0.0" (instance $api "my:chret/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:chret/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for char result");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `error-context` as a param. Same single-
/// i32 canonical-ABI shape as a resource handle, but as a
/// primitive — no resource declaration / factored-types ceremony.
/// Drives `Type::ErrorContext → Cell::Handle { kind: ErrorContext }`
/// → `cell::error-context-handle`.
#[test]
fn dispatch_module_with_error_context_param_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "consume") (param i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (type $consume-ty (func (param "e" error-context)))
            (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
            (instance $api-inst (export "consume" (func $consume-lifted)))
            (export "my:ec/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:ec/api@1.0.0" (instance $api "my:ec/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:ec/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for error-context param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `error-context` as a Direct result. Sync
/// single-i32 return → no retptr; routes through
/// `is_supported_direct_result(ErrorContext) = true`.
#[test]
fn dispatch_module_with_error_context_result_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "make") (result i32) i32.const 0)
            )
            (core instance $i (instantiate $m))
            (alias core export $i "make" (core func $make))
            (type $make-ty (func (result error-context)))
            (func $make-lifted (type $make-ty) (canon lift (core func $make)))
            (instance $api-inst (export "make" (func $make-lifted)))
            (export "my:ecret/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:ecret/api@1.0.0" (instance $api "my:ecret/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:ecret/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for error-context result");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `Cell::Variant` as a Compound result.
/// Drives `is_compound_result(Variant) → Compound → lift_from_memory` + the
/// N-way disc dispatch on the result side. `shape { circle,
/// sq(u32), tri(u32) }` joined-flat = [i32 disc, i32 (joined u32/u32)] → 2 slots → retptr.
#[test]
fn dispatch_module_with_variant_result_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (memory (export "memory") 1)
                (func (export "make") (result i32)
                    i32.const 0x1000
                    i32.const 2
                    i32.store
                    i32.const 0x1000
                    i32.const 42
                    i32.store offset=4
                    i32.const 0x1000
                )
                (func (export "cabi_post_make") (param i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "make" (core func $make))
            (alias core export $i "cabi_post_make" (core func $make_post))
            (alias core export $i "memory" (core memory $mem))
            (type $shape (variant (case "circle") (case "sq" u32) (case "tri" u32)))
            (export $shape-export "shape" (type $shape))
            (type $make-ty (func (result $shape-export)))
            (func $make-lifted (type $make-ty)
                (canon lift (core func $make) (memory $mem)
                    (post-return (func $make_post))))
            (instance $api-inst
                (export "shape" (type $shape-export))
                (export "make" (func $make-lifted)))
            (export "my:vtret/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:vtret/api@1.0.0" (instance $api "my:vtret/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:vtret/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for variant result");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// Drives `is_compound_result(List) → Compound → lift_from_memory` + `Cell::ListOf`
/// element-loop emit on the result side. `list<u32>` flattens to (ptr, len) → 2 slots → retptr.
/// Pre-allocated u32 array at 0x2000 + (ptr, len) header at 0x1000.
#[test]
fn dispatch_module_with_list_result_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (memory (export "memory") 1)
                (data (i32.const 0x2000) "\0a\00\00\00\14\00\00\00\1e\00\00\00")
                (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
                    i32.const 0x4000)
                (func (export "make") (result i32)
                    i32.const 0x1000
                    i32.const 0x2000
                    i32.store
                    i32.const 0x1000
                    i32.const 3
                    i32.store offset=4
                    i32.const 0x1000
                )
                (func (export "cabi_post_make") (param i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "make" (core func $make))
            (alias core export $i "cabi_post_make" (core func $make_post))
            (alias core export $i "memory" (core memory $mem))
            (alias core export $i "cabi_realloc" (core func $realloc))
            (type $make-ty (func (result (list u32))))
            (func $make-lifted (type $make-ty)
                (canon lift (core func $make) (memory $mem) (realloc (func $realloc))
                    (post-return (func $make_post))))
            (instance $api-inst (export "make" (func $make-lifted)))
            (export "my:listret/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:listret/api@1.0.0" (instance $api "my:listret/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:listret/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for list<u32> result");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// `list<option<u32>>` param — multi-cell element. Exercises the
/// per-iteration `elem_cell_base = start_i + j*elem_count` stage + runtime-computed
/// Option child index in `cell::option-some(idx)` payload.
/// Canned-sweep covers runtime value-correctness; this is a build-and-validate fast check.
#[test]
fn dispatch_module_with_option_list_param_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (memory (export "memory") 1)
                (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
                    i32.const 0x4000)
                (func (export "consume") (param i32 i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (alias core export $i "memory" (core memory $mem))
            (alias core export $i "cabi_realloc" (core func $realloc))
            (type $consume-ty (func (param "xs" (list (option u32)))))
            (func $consume-lifted (type $consume-ty)
                (canon lift (core func $consume) (memory $mem) (realloc (func $realloc))))
            (instance $api-inst (export "consume" (func $consume-lifted)))
            (export "my:lo/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:lo/api@1.0.0" (instance $api "my:lo/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:lo/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for list<option<u32>> param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// `list<result<u32, string>>` param — exercises both result arms'
/// runtime child indices on the same shared `list_elem_child_idx`
/// staging local.
#[test]
fn dispatch_module_with_result_list_param_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (memory (export "memory") 1)
                (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
                    i32.const 0x4000)
                (func (export "consume") (param i32 i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (alias core export $i "memory" (core memory $mem))
            (alias core export $i "cabi_realloc" (core func $realloc))
            (type $consume-ty (func (param "xs" (list (result u32 (error string))))))
            (func $consume-lifted (type $consume-ty)
                (canon lift (core func $consume) (memory $mem) (realloc (func $realloc))))
            (instance $api-inst (export "consume" (func $consume-lifted)))
            (export "my:lr/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:lr/api@1.0.0" (instance $api "my:lr/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:lr/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for list<result<u32, string>> param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// `list<tuple<u32, string>>` param — multi-cell tuple element.
/// Exercises the per-list tuple-indices `cabi_realloc`'d buffer,
/// the per-iteration slot staging, and the `PerIteration`
/// `TupleIdxSource` branch in `emit_tuple_of_cell`.
#[test]
fn dispatch_module_with_tuple_list_param_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (memory (export "memory") 1)
                (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
                    i32.const 0x4000)
                (func (export "consume") (param i32 i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (alias core export $i "memory" (core memory $mem))
            (alias core export $i "cabi_realloc" (core func $realloc))
            (type $consume-ty (func (param "xs" (list (tuple u32 string)))))
            (func $consume-lifted (type $consume-ty)
                (canon lift (core func $consume) (memory $mem) (realloc (func $realloc))))
            (instance $api-inst (export "consume" (func $consume-lifted)))
            (export "my:lt/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:lt/api@1.0.0" (instance $api "my:lt/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:lt/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for list<tuple<u32, string>> param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// `list<char>` result — same Compound/retptr path as
/// `list<u32>`, but the per-iteration utf-8 scratch + Prestaged
/// `CharScratch` ride along on the result side too. The canned
/// sweep in `tests/fuzz_and_run.rs` runtime-checks the encoded
/// bytes; this is a build-and-validate fast-feedback test.
#[test]
fn dispatch_module_with_char_list_result_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (memory (export "memory") 1)
                (data (i32.const 0x2000) "\78\00\00\00")
                (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
                    i32.const 0x4000)
                (func (export "make") (result i32)
                    i32.const 0x1000
                    i32.const 0x2000
                    i32.store
                    i32.const 0x1000
                    i32.const 1
                    i32.store offset=4
                    i32.const 0x1000
                )
                (func (export "cabi_post_make") (param i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "make" (core func $make))
            (alias core export $i "cabi_post_make" (core func $make_post))
            (alias core export $i "memory" (core memory $mem))
            (alias core export $i "cabi_realloc" (core func $realloc))
            (type $make-ty (func (result (list char))))
            (func $make-lifted (type $make-ty)
                (canon lift (core func $make) (memory $mem) (realloc (func $realloc))
                    (post-return (func $make_post))))
            (instance $api-inst (export "make" (func $make-lifted)))
            (export "my:lcret/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:lcret/api@1.0.0" (instance $api "my:lcret/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:lcret/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for list<char> result");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// Single-flat-slot variant (`variant { only }` → just disc, no
/// payloads) — comes back flat, not retptr. Routes through the
/// no-retptr Compound branch (variant is in `is_compound_result`),
/// reading the disc from `lcl.result`. Pins the build + validate
/// of this single-cell flat-Compound path.
#[test]
fn dispatch_module_with_single_slot_variant_result_lifts_from_flat() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "noop") (result i32) i32.const 0)
            )
            (core instance $i (instantiate $m))
            (alias core export $i "noop" (core func $noop))
            (type $only (variant (case "only")))
            (export $only-export "only" (type $only))
            (type $noop-ty (func (result $only-export)))
            (func $noop-lifted (type $noop-ty) (canon lift (core func $noop)))
            (instance $api-inst
                (export "only" (type $only-export))
                (export "noop" (func $noop-lifted)))
            (export "my:vt1/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:vt1/api@1.0.0" (instance $api "my:vt1/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "my:vt1/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("single-slot variant must build via no-retptr Compound, not panic");
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted adapter component should validate");
}

/// End-to-end test for `Cell::Flags` as a Direct result. Drives
/// the bit-walk reading from `lcl.result` (the i32 the export sig
/// returns) plus the per-result-direct flags-info entry the layout
/// phase appends. Same nominal-type WAT shape as
/// `dispatch_module_with_flags_param_roundtrips`.
#[test]
fn dispatch_module_with_flags_result_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "produce") (result i32) i32.const 5)
            )
            (core instance $i (instantiate $m))
            (alias core export $i "produce" (core func $produce))
            (type $perms (flags "read" "write" "exec"))
            (export $perms-export "fperms" (type $perms))
            (type $produce-ty (func (result $perms-export)))
            (func $produce-lifted (type $produce-ty) (canon lift (core func $produce)))
            (instance $api-inst
                (export "fperms" (type $perms-export))
                (export "produce" (func $produce-lifted)))
            (export "my:flret/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:flret/api@1.0.0" (instance $api "my:flret/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:flret/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for flags result");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// End-to-end test for `Cell::Result` as a param: branching emit
/// (result-ok / result-err with option<u32> payload) and the
/// canonical-ABI joined-flat slot sharing across both arms.
/// `result<u32, u32>` keeps the canon-lift options minimal (no
/// realloc / memory required) — both arms share the joined slot,
/// no widening needed.
#[test]
fn dispatch_module_with_result_param_roundtrips() {
    // result<u32, u32> flat = [i32 disc, i32 (joined u32/u32)].
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "consume") (param i32 i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (type $consume-ty (func (param "r" (result u32 (error u32)))))
            (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
            (instance $api-inst (export "consume" (func $consume-lifted)))
            (export "my:res/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:res/api@1.0.0" (instance $api "my:res/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:res/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for result param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// `result<u32, u64>` as a param exercises joined-flat widening:
/// joined = [i32 disc, i64 (max width)]. The wrapper's flat
/// fn-param at slot 1 is i64; the ok arm's `Cell::IntegerZeroExt`
/// (u32) reads slot 1 expecting i32 — emit must bitcast i64→i32
/// before the i64.extend, otherwise wasm validation rejects the
/// i32-typed instruction taking an i64 stack value.
#[test]
fn dispatch_module_with_widening_result_param_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "consume") (param i32 i64))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (type $consume-ty (func (param "r" (result u32 (error u64)))))
            (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
            (instance $api-inst (export "consume" (func $consume-lifted)))
            (export "my:res/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:res/api@1.0.0" (instance $api "my:res/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:res/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation must succeed for widening result param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted adapter must validate (joined-flat bitcast on ok-arm read)");
}

/// `variant { a(u32), b(u64), c(f64) }` mixed-width param —
/// joined slot 1 = i64. Arms `a` (i32) and `c` (f64) widen, arm
/// `b` matches. Pins that the bitcast emitter handles each arm's
/// distinct cast independently and that f64-arm leaves use the
/// f64 scratch path through `pin_leaf_flat`.
#[test]
fn dispatch_module_with_widening_variant_param_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "consume") (param i32 i64))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "consume" (core func $consume))
            (type $tri (variant (case "a" u32) (case "b" u64) (case "c" f64)))
            (export $tri-export "tri" (type $tri))
            (type $consume-ty (func (param "v" $tri-export)))
            (func $consume-lifted (type $consume-ty) (canon lift (core func $consume)))
            (instance $api-inst
                (export "tri" (type $tri-export))
                (export "consume" (func $consume-lifted)))
            (export "my:vt/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:vt/api@1.0.0" (instance $api "my:vt/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:vt/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation must succeed for mixed-width variant param");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted adapter must validate (per-arm bitcasts on a/c arms)");
}

/// End-to-end test for `result<T, E>` as a compound result.
/// Drives `is_compound_result(Result) → Compound → lift_from_memory`
/// and the if/else branching emit at the parent Result cell.
/// `result<u32, u32>` flattens to 2 slots → retptr; canon lift's
/// `memory` + `post-return` materialize it via the
/// callee-allocates pattern.
#[test]
fn dispatch_module_with_result_result_roundtrips() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (memory (export "memory") 1)
                (func (export "either") (param i32) (result i32)
                    i32.const 0x1000
                    i32.const 0
                    i32.store
                    i32.const 0x1000
                    local.get 0
                    i32.store offset=4
                    i32.const 0x1000
                )
                (func (export "cabi_post_either") (param i32))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "either" (core func $either))
            (alias core export $i "cabi_post_either" (core func $either_post))
            (alias core export $i "memory" (core memory $mem))
            (type $either-ty (func (param "x" u32) (result (result u32 (error u32)))))
            (func $either-lifted (type $either-ty)
                (canon lift (core func $either) (memory $mem)
                    (post-return (func $either_post))))
            (instance $api-inst (export "either" (func $either-lifted)))
            (export "my:res-ret/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:res-ret/api@1.0.0" (instance $api "my:res-ret/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "my:res-ret/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for result result");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// Single-flat-slot compound result (`result<_, _>`) — flat is
/// just the i32 disc, comes back direct (not retptr). Routes
/// through the no-retptr Compound branch (reading the disc from
/// `lcl.result`); the after-hook sees `result-err(none)` or
/// `result-ok(none)`.
#[test]
fn dispatch_module_with_single_slot_result_result_lifts_from_flat() {
    let wat = r#"(component
        (component $inner
            (core module $m
                (func (export "noop") (result i32)
                    i32.const 0
                )
            )
            (core instance $i (instantiate $m))
            (alias core export $i "noop" (core func $noop))
            (type $noop-ty (func (result (result))))
            (func $noop-lifted (type $noop-ty) (canon lift (core func $noop)))
            (instance $api-inst (export "noop" (func $noop-lifted)))
            (export "my:res1/api@1.0.0" (instance $api-inst))
        )
        (instance $api (instantiate $inner))
        (export "my:res1/api@1.0.0" (instance $api "my:res1/api@1.0.0"))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "my:res1/api@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("single-slot result must build via no-retptr Compound, not panic");
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted adapter component should validate");
}

// ── Tier 2: async indirect-params (lower_to_memory) ──────────────
//
// Parallel to tier-1's coverage in
// `src/adapter/tier1/tests/fuzz.rs`. Tier-2 already lowers the
// *hook* params record by hand (`emit_populate_hook_params`) — these
// tests pin that the *target-fn* params record, sized + offset by
// wit-parser, also lowers correctly when async canon-lower switches
// to `indirect_params = true`. Hooks-wired so both lowerings appear
// in the same wrapper body.

/// Five `u32` params overflow `MAX_FLAT_ASYNC_PARAMS` → indirect-params.
/// Smallest shape that forces tier-2 to lower the target-fn params
/// record alongside its existing hook-params lowering.
#[test]
fn async_5_u32_params_validates() {
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (func async
                (param "a" u32) (param "b" u32) (param "c" u32)
                (param "d" u32) (param "e" u32) (result u32)))
            (export "many" (func (type 0)))
        ))
        (import "test:pkg/many@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "test:pkg/many@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for 5×u32 async params");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// Mixed primitive widths in indirect-params position — exercises
/// `i32.store` / `i64.store` / `f32.store` / `f64.store` /
/// `i32.store8` plus the canonical-ABI inter-field padding the
/// params record imposes (alignments 4 → 8 → 4 → 8 → 1 → 4).
#[test]
fn async_mixed_primitives_indirect_params_validates() {
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (func async
                (param "a" u32) (param "b" u64) (param "c" f32)
                (param "d" f64) (param "e" bool) (param "f" char)
                (result u32)))
            (export "mixed" (func (type 0)))
        ))
        (import "test:pkg/mixed-async@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "test:pkg/mixed-async@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for mixed-primitive async params");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// Aggregate params (record + tuple + enum + flags) in indirect-
/// params position. The WIT-level interface declares each as a
/// named type the function references; the adapter must walk
/// each aggregate via the corresponding `*Lower` arm and emit
/// the inner stores at canonical record offsets.
#[test]
fn async_aggregates_indirect_params_validates() {
    // 5×u32 record alone hits indirect-params (5 > 4 = MAX_FLAT_ASYNC_PARAMS).
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (record
                (field "a" u32) (field "b" u32) (field "c" u32)
                (field "d" u32) (field "e" u32)))
            (export "rec5" (type (eq 0)))
            (type (;2;) (tuple u32 u64 f32 f64 bool))
            (type (;3;) (enum "red" "green" "blue"))
            (export "color" (type (eq 3)))
            (type (;5;) (flags "read" "write" "exec"))
            (export "perms" (type (eq 5)))
            (type (;7;) (func async
                (param "r" 1) (param "t" 2) (param "c" 4) (param "f" 6)
                (result u32)))
            (export "many" (func (type 7)))
        ))
        (import "test:pkg/agg-async@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "test:pkg/agg-async@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for aggregate async params");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// String / list / option / result params in indirect-params
/// position. Fixed-list is omitted (tier-2's hook-side lift
/// codegen doesn't yet handle it; tier-1 covers it separately).
/// Result arms share i32 flat → no joined-flat widening on hook
/// lift, sidestepping a separate pre-existing limit.
#[test]
fn async_dispatch_shapes_indirect_params_validates() {
    // string(2) + list(2) + option<u32>(2) + result<u32,u32>(2) +
    // 2×u32 = 10 flat → indirect.
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (list u32))
            (type (;1;) (option u32))
            (type (;2;) (result u32 (error u32)))
            (type (;3;) (func async
                (param "s" string)
                (param "l" 0)
                (param "o" 1)
                (param "r" 2)
                (param "a" u32)
                (param "b" u32)
                (result u32)))
            (export "many" (func (type 3)))
        ))
        (import "test:pkg/disp-async@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");

    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "test:pkg/disp-async@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter generation should succeed for dispatch shapes");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

// ── Tier 2: sync indirect-params (symmetric pointer-form flip) ──
//
// Sync funcs whose params overflow `MAX_FLAT_PARAMS` (16) flip the
// export sig to `(i32) -> ...` (host writes the params record into
// our memory, hands us the pointer). With hooks wired, tier-2 must
// materialize each param's flat representation into synth locals
// via `build_lift_params_from_memory` — otherwise the hook-record
// lift would index nonexistent flat wrapper locals. Covers the
// function-boundary sync half of `docs/TODO/canonical-abi-gaps.md`.

/// 17×u32 sync + before/after hooks. Smallest shape that forces
/// `export_sig.indirect_params = true` and exercises the new
/// lift-from-memory path. Without it the hook record-build at
/// phase 1 emits `local.get` on indices 1..16 — invalid wasm.
#[test]
fn sync_17_u32_params_with_hooks_validates() {
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (func
                (param "a" u32) (param "b" u32) (param "c" u32)
                (param "d" u32) (param "e" u32) (param "f" u32)
                (param "g" u32) (param "h" u32) (param "i" u32)
                (param "j" u32) (param "k" u32) (param "l" u32)
                (param "m" u32) (param "n" u32) (param "o" u32)
                (param "p" u32) (param "q" u32)))
            (export "wide" (func (type 0)))
        ))
        (import "test:pkg/wide@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "test:pkg/wide@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter must succeed for 17×u32 sync params with hooks");
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// Mixed primitive widths overflowing 16 + hooks. Exercises
/// per-param `lift_from_memory` across i32/i64/f32/f64/i8(bool)/
/// i32(char) flat widths inside synth-local materialization.
#[test]
fn sync_mixed_primitives_indirect_params_with_hooks_validates() {
    // 9×u64 + u32 + f32 + f64 + bool + char = 19 flat slots.
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (func
                (param "a" u64) (param "b" u64) (param "c" u64)
                (param "d" u64) (param "e" u64) (param "f" u64)
                (param "g" u64) (param "h" u64) (param "i" u64)
                (param "j" u32) (param "k" f32) (param "l" f64)
                (param "m" bool) (param "n" char)))
            (export "mixed" (func (type 0)))
        ))
        (import "test:pkg/sync-mixed@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "test:pkg/sync-mixed@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter must succeed for mixed-primitive sync indirect params");
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// Wide aggregate params (record + tuple + enum + flags) overflowing
/// 16 + hooks. Drives each aggregate's bindgen walk inside the
/// per-param lift-from-memory sequence, then through `emit_lift_plan`
/// reading from per-param synth locals.
#[test]
fn sync_wide_aggregates_indirect_params_with_hooks_validates() {
    // 5×u32 record + 5-element tuple + enum + flags + 6×u32 ≥ 17 flat.
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (record
                (field "a" u32) (field "b" u32) (field "c" u32)
                (field "d" u32) (field "e" u32)))
            (export "rec5" (type (eq 0)))
            (type (;2;) (tuple u32 u64 f32 f64 bool))
            (type (;3;) (enum "red" "green" "blue"))
            (export "color" (type (eq 3)))
            (type (;5;) (flags "read" "write" "exec"))
            (export "perms" (type (eq 5)))
            (type (;7;) (func
                (param "r" 1) (param "t" 2) (param "c" 4) (param "f" 6)
                (param "u0" u32) (param "u1" u32) (param "u2" u32)
                (param "u3" u32) (param "u4" u32) (param "u5" u32)))
            (export "many" (func (type 7)))
        ))
        (import "test:pkg/sync-agg@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "test:pkg/sync-agg@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter must succeed for wide aggregate sync indirect params");
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// 9 strings (18 flat slots) + hooks. Each string lifts as a
/// `(ptr, len)` pair from the host's params record, then the hook
/// record-build runs `cabi_realloc` + memcpy through `emit_lift_plan`.
#[test]
fn sync_string_list_indirect_params_with_hooks_validates() {
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (func
                (param "s0" string) (param "s1" string) (param "s2" string)
                (param "s3" string) (param "s4" string) (param "s5" string)
                (param "s6" string) (param "s7" string) (param "s8" string)))
            (export "many" (func (type 0)))
        ))
        (import "test:pkg/sync-strs@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "test:pkg/sync-strs@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter must succeed for 9-string sync indirect params");
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// 17×u32 *async* + hooks. Async-stackful export caps at 16-flat,
/// so 17 params flips BOTH `import_sig.indirect_params` AND
/// `export_sig.indirect_params` (the symmetric corner — wrapper has
/// only `local 0`, handler also wants a pointer). The same
/// passthrough as sync >16, but the export sig is
/// `GuestExportAsyncStackful`. Pins that the symmetric-indirect
/// plumbing covers async too, not just sync.
#[test]
fn async_17_u32_params_symmetric_indirect_validates() {
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (func async
                (param "a" u32) (param "b" u32) (param "c" u32)
                (param "d" u32) (param "e" u32) (param "f" u32)
                (param "g" u32) (param "h" u32) (param "i" u32)
                (param "j" u32) (param "k" u32) (param "l" u32)
                (param "m" u32) (param "n" u32) (param "o" u32)
                (param "p" u32) (param "q" u32)))
            (export "wide" (func (type 0)))
        ))
        (import "test:pkg/async-sym@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "test:pkg/async-sym@1.0.0",
        true,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter must succeed for 17×u32 async symmetric-indirect");
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// 17×u32 sync + after-hook only. Phase 1 (before-hook param lift)
/// is skipped, so the new `params_lift_seqs` machinery doesn't fire;
/// pins that the gate-flip alone works through the handler-call
/// passthrough plus phase 3's result-side lift.
#[test]
fn sync_17_u32_params_after_only_validates() {
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (func
                (param "a" u32) (param "b" u32) (param "c" u32)
                (param "d" u32) (param "e" u32) (param "f" u32)
                (param "g" u32) (param "h" u32) (param "i" u32)
                (param "j" u32) (param "k" u32) (param "l" u32)
                (param "m" u32) (param "n" u32) (param "o" u32)
                (param "p" u32) (param "q" u32)))
            (export "wide" (func (type 0)))
        ))
        (import "test:pkg/wide-after@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let bytes = build_tier2_adapter(
        "test:pkg/wide-after@1.0.0",
        false,
        true,
        false,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 adapter must succeed for 17×u32 sync params with after-hook only");
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

// ── Tier-2 `gate` hook (`should-call`) ───────────────────────────────

/// `gate` alone (no before/after) on a void async fn with a string
/// param: the lift code that previously only ran under `before` must
/// now fire for `gate`-only, populate the shared args buffer, and
/// branch on the bool result. Validates structurally.
#[test]
fn dispatch_module_with_gate_only_void_async_roundtrips() {
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (func async (param "msg" string)))
            (export "fire" (func (type 0)))
        ))
        (import "test:pkg/gate-only@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "test:pkg/gate-only@1.0.0",
        false, // has_before
        false, // has_after
        true,  // has_gate
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 gate-only adapter generation should succeed");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// `gate` + `before` + `after` together. Exercises the shared args
/// buffer (both `before` and `gate` reuse the same `{ call, args }`
/// indirect-params slot) and threads the bool branch through the
/// wrapper body.
#[test]
fn dispatch_module_with_gate_and_full_hooks_roundtrips() {
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (func async (param "msg" string)))
            (export "fire" (func (type 0)))
        ))
        (import "test:pkg/gate-all@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let bytes = build_tier2_adapter(
        "test:pkg/gate-all@1.0.0",
        true, // has_before
        true, // has_after
        true, // has_gate
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect("tier-2 gate+before+after adapter generation should succeed");

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&bytes)
        .expect("emitted tier-2 adapter component should validate");
}

/// `gate` on a non-void target is rejected upstream — the adapter
/// can't synthesize a return value when the call is skipped (same
/// constraint tier-1 enforces).
#[test]
fn dispatch_module_with_gate_and_nonvoid_bails() {
    let wat = r#"(component
        (type (;0;) (instance
            (type (;0;) (func async (param "x" u32) (result u32)))
            (export "compute" (func (type 0)))
        ))
        (import "test:pkg/gate-nonvoid@1.0.0" (instance (type 0)))
    )"#;
    let split_bytes = wat::parse_str(wat).expect("WAT must parse");
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");

    let err = build_tier2_adapter(
        "test:pkg/gate-nonvoid@1.0.0",
        false,
        false,
        true,
        &split_bytes,
        common_wit,
        tier2_wit,
    )
    .expect_err("gate on a non-void fn must bail");
    let msg = err.to_string();
    assert!(
        msg.contains("`gate`"),
        "bail should call out the gate hook; got: {msg}",
    );
    assert!(
        msg.contains("void-returning"),
        "bail should explain the void-only constraint; got: {msg}",
    );
}
