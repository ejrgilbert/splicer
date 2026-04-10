//! In-process validation tests for the tier-1 adapter generator.
//!
//! Each test builds a synthetic [`InterfaceType`] via the cviz model
//! types, runs [`generate_tier1_adapter`] end-to-end, and validates
//! the resulting bytes via `wasmparser::Validator`. They cover the
//! per-shape combinations the tier-1 generator has to produce: sync
//! primitives, async-void with strings, async with resource types
//! (the wasi:http/handler shape), multiple functions per interface,
//! before-only / after-only / blocking, and no-hooks.

use super::*;
use cviz::model::{
    FuncSignature, InstanceInterface, InterfaceType, TypeArena, ValueType, ValueTypeId,
};
use std::collections::BTreeMap;

/// Helper: validate that bytes form a valid component-model binary.
fn validate_component(bytes: &[u8]) {
    let mut validator =
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    validator
        .validate_all(bytes)
        .expect("generated adapter should be a valid component");
}

/// Helper: generate an adapter and return the raw bytes.
fn gen_adapter(
    target: &str,
    hooks: &[&str],
    iface: &InterfaceType,
    arena: &TypeArena,
) -> Vec<u8> {
    let tmp = tempfile::tempdir().unwrap();
    let hook_strings: Vec<String> = hooks.iter().map(|s| s.to_string()).collect();
    let path = generate_tier1_adapter(
        "test-mdl",
        None,
        target,
        &hook_strings,
        Some(iface),
        tmp.path().to_str().unwrap(),
        None, // no consumer split in unit tests
        arena,
    )
    .expect("adapter generation should succeed");
    std::fs::read(&path).expect("should read generated adapter file")
}

fn make_iface(funcs: Vec<(&str, FuncSignature)>) -> InterfaceType {
    InterfaceType::Instance(InstanceInterface {
        functions: funcs
            .into_iter()
            .map(|(n, s)| (n.to_string(), s))
            .collect(),
        type_exports: BTreeMap::new(),
    })
}

fn sig(
    is_async: bool,
    names: &[&str],
    params: Vec<ValueTypeId>,
    results: Vec<ValueTypeId>,
) -> FuncSignature {
    FuncSignature {
        is_async,
        param_names: names.iter().map(|s| s.to_string()).collect(),
        params,
        results,
    }
}

// ── Tier 1: sync primitives ──────────────────────────────────────────

#[test]
fn test_adapter_sync_primitives() {
    let mut arena = TypeArena::default();
    let s32 = arena.intern_val(ValueType::S32);
    let iface = make_iface(vec![("add", sig(false, &["a", "b"], vec![s32, s32], vec![s32]))]);
    let bytes = gen_adapter(
        "test:pkg/adder@1.0.0",
        &["splicer:adapter/before", "splicer:adapter/after"],
        &iface,
        &arena,
    );
    validate_component(&bytes);
}

// ── Tier 1: async void with string param ─────────────────────────────

#[test]
fn test_adapter_async_void_string() {
    let mut arena = TypeArena::default();
    let string = arena.intern_val(ValueType::String);
    let iface = make_iface(vec![("print", sig(true, &["msg"], vec![string], vec![]))]);
    let bytes = gen_adapter(
        "test:pkg/printer@1.0.0",
        &["splicer:adapter/before", "splicer:adapter/after"],
        &iface,
        &arena,
    );
    validate_component(&bytes);
}

// ── Tier 1: async with resource types (HTTP handler pattern) ─────────

#[test]
fn test_adapter_resource_handler() {
    let mut arena = TypeArena::default();

    // Build the error-code variant (simplified)
    let string_id = arena.intern_val(ValueType::String);
    let opt_string = arena.intern_val(ValueType::Option(string_id));
    let u16_id = arena.intern_val(ValueType::U16);
    let opt_u16 = arena.intern_val(ValueType::Option(u16_id));
    let dns_error_payload = arena.intern_val(ValueType::Record(vec![
        ("rcode".into(), opt_string),
        ("info-code".into(), opt_u16),
    ]));
    let error_code = arena.intern_val(ValueType::Variant(vec![
        ("DNS-timeout".into(), None),
        ("DNS-error".into(), Some(dns_error_payload)),
        ("connection-refused".into(), None),
        ("internal-error".into(), Some(opt_string)),
    ]));

    let request = arena.intern_val(ValueType::Resource("request".into()));
    let response = arena.intern_val(ValueType::Resource("response".into()));
    let result_ty = arena.intern_val(ValueType::Result {
        ok: Some(response),
        err: Some(error_code),
    });

    let func = sig(true, &["request"], vec![request], vec![result_ty]);
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([("handle".to_string(), func)]),
        type_exports: BTreeMap::from([
            ("request".to_string(), request),
            ("response".to_string(), response),
            ("error-code".to_string(), error_code),
        ]),
    });

    let bytes = gen_adapter(
        "wasi:http/handler@0.3.0-rc-2026-01-06",
        &["splicer:adapter/before", "splicer:adapter/after"],
        &iface,
        &arena,
    );
    validate_component(&bytes);
}

// ── Tier 1: multiple functions ───────────────────────────────────────

#[test]
fn test_adapter_multi_func() {
    let mut arena = TypeArena::default();
    let s32 = arena.intern_val(ValueType::S32);
    let string = arena.intern_val(ValueType::String);
    let iface = make_iface(vec![
        ("add", sig(false, &["a", "b"], vec![s32, s32], vec![s32])),
        ("print", sig(true, &["msg"], vec![string], vec![])),
        ("get-value", sig(false, &[], vec![], vec![s32])),
    ]);
    let bytes = gen_adapter(
        "test:pkg/mixed@1.0.0",
        &["splicer:adapter/before", "splicer:adapter/after"],
        &iface,
        &arena,
    );
    validate_component(&bytes);
}

// ── Tier 1: before hook only ─────────────────────────────────────────

#[test]
fn test_adapter_before_only() {
    let mut arena = TypeArena::default();
    let s32 = arena.intern_val(ValueType::S32);
    let iface = make_iface(vec![("get", sig(false, &[], vec![], vec![s32]))]);
    let bytes = gen_adapter(
        "test:pkg/getter@1.0.0",
        &["splicer:adapter/before"],
        &iface,
        &arena,
    );
    validate_component(&bytes);
}

// ── Tier 1: after hook only ──────────────────────────────────────────

#[test]
fn test_adapter_after_only() {
    let mut arena = TypeArena::default();
    let s32 = arena.intern_val(ValueType::S32);
    let iface = make_iface(vec![("get", sig(true, &[], vec![], vec![s32]))]);
    let bytes = gen_adapter(
        "test:pkg/getter@1.0.0",
        &["splicer:adapter/after"],
        &iface,
        &arena,
    );
    validate_component(&bytes);
}

// ── Tier 1: blocking hook (void async only) ──────────────────────────

#[test]
fn test_adapter_blocking() {
    let mut arena = TypeArena::default();
    let string = arena.intern_val(ValueType::String);
    let iface = make_iface(vec![("fire", sig(true, &["msg"], vec![string], vec![]))]);
    let bytes = gen_adapter(
        "test:pkg/fire@1.0.0",
        &[
            "splicer:adapter/before",
            "splicer:adapter/blocking",
            "splicer:adapter/after",
        ],
        &iface,
        &arena,
    );
    validate_component(&bytes);
}

// ── Tier 1: no hooks at all ──────────────────────────────────────────

#[test]
fn test_adapter_no_hooks() {
    let mut arena = TypeArena::default();
    let s32 = arena.intern_val(ValueType::S32);
    let iface = make_iface(vec![("add", sig(false, &["a", "b"], vec![s32, s32], vec![s32]))]);
    let bytes = gen_adapter(
        "test:pkg/adder@1.0.0",
        &[],
        &iface,
        &arena,
    );
    validate_component(&bytes);
}
