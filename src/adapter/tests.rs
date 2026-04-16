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
    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    validator
        .validate_all(bytes)
        .expect("generated adapter should be a valid component");
}

/// Map a primitive or String `ValueType` to its WAT spelling, for use
/// when rendering function parameter / result types in the synth split.
/// Panics on anything non-primitive — the synth split generator
/// expects the resource-handler case to go through the hardcoded WAT
/// template in [`synth_consumer_split`].
fn wat_type(id: ValueTypeId, arena: &TypeArena) -> String {
    match arena.lookup_val(id) {
        ValueType::S32 => "s32".into(),
        ValueType::U32 => "u32".into(),
        ValueType::S64 => "s64".into(),
        ValueType::U64 => "u64".into(),
        ValueType::S8 => "s8".into(),
        ValueType::U8 => "u8".into(),
        ValueType::S16 => "s16".into(),
        ValueType::U16 => "u16".into(),
        ValueType::F32 => "f32".into(),
        ValueType::F64 => "f64".into(),
        ValueType::Bool => "bool".into(),
        ValueType::Char => "char".into(),
        ValueType::String => "string".into(),
        other => panic!(
            "wat_type: synth split helper only supports primitive + string types, \
             got {other:?}. For richer shapes, add a dedicated WAT template."
        ),
    }
}

/// Build a minimal consumer-split component that imports `target` as an
/// instance. Written as WAT and parsed with `wat::parse_str` — easier
/// to audit than wasm_encoder for a fixture whose shape we want to
/// match the convention a real production consumer split uses.
///
/// Two shapes supported:
///
/// - **Primitives-only** (no `type_exports`): every function's params
///   and result are primitives/strings, so the instance type body is a
///   straight `(func …)` + `(export "name" (func (type …)))` listing.
/// - **wasi:http/handler-shape** (the only non-primitive shape used by
///   the tests): hardcoded WAT that mirrors `service_a.comp.wasm`'s
///   handler import — a separate `wasi:http/types`-style instance
///   import provides request / response / error-code, the handler
///   instance type aliases-outer them and re-exports via `(type (eq))`.
fn synth_consumer_split(
    target: &str,
    iface: &InterfaceType,
    arena: &TypeArena,
) -> tempfile::NamedTempFile {
    let iface_inst = match iface {
        InterfaceType::Instance(i) => i,
        _ => panic!("synth_consumer_split: bare function interfaces not supported"),
    };

    let wat = if iface_inst.type_exports.is_empty() {
        wat_primitive_only(target, iface_inst, arena)
    } else {
        wat_http_handler_shape(target)
    };

    let bytes = wat::parse_str(&wat).unwrap_or_else(|e| {
        panic!("synth split WAT failed to parse: {e}\n\n--- WAT ---\n{wat}\n--- end ---")
    });
    let mut tmp = tempfile::NamedTempFile::new().expect("make tempfile");
    std::io::Write::write_all(&mut tmp, &bytes).expect("write synth split");
    tmp
}

fn wat_primitive_only(target: &str, iface: &InstanceInterface, arena: &TypeArena) -> String {
    let mut body = String::new();
    let mut func_type_for: Vec<(String, u32)> = Vec::new();

    for (type_idx, (name, sig)) in iface.functions.iter().enumerate() {
        let type_idx = type_idx as u32;
        let params: Vec<String> = sig
            .param_names
            .iter()
            .zip(sig.params.iter())
            .map(|(pname, &pid)| format!(r#"(param "{pname}" {})"#, wat_type(pid, arena)))
            .collect();
        let result = match sig.results.first() {
            Some(&rid) => format!(" (result {})", wat_type(rid, arena)),
            None => String::new(),
        };
        let async_kw = if sig.is_async { "async " } else { "" };
        body.push_str(&format!(
            "      (type (;{type_idx};) (func {async_kw}{}{result}))\n",
            params.join(" "),
        ));
        func_type_for.push((name.clone(), type_idx));
    }
    for (name, fty) in &func_type_for {
        body.push_str(&format!("      (export \"{name}\" (func (type {fty})))\n"));
    }

    format!(
        "(component\n  (type (;0;) (instance\n{body}  ))\n  (import \"{target}\" (instance (type 0)))\n)\n"
    )
}

/// WAT for the wasi:http/handler-shape fixture used by
/// `test_adapter_resource_handler`. Mirrors `service_a.comp.wasm`'s
/// real handler import structure: a types instance with request /
/// response resources + the error-code variant, then a handler
/// instance type that `alias outer`s each and re-exports via `eq`.
fn wat_http_handler_shape(target: &str) -> String {
    // Note: when the types instance is used as an IMPORT, each compound
    // type referenced by a variant case payload must be surfaced as an
    // `(export "name" (type (eq N)))` first — the component-model
    // validator rejects variants whose cases carry *anonymous* record
    // payloads in an instance type used at the import boundary. We
    // mirror the convention the real WIT-standard HTTP bindings use
    // (see `fixtures/service_a.comp.wasm`): the record is exported as
    // `DNS-error-payload`, and the variant's `DNS-error` case then
    // references that export's index.
    format!(
        r#"(component
  (type (;0;) (instance
    (export "request" (type (sub resource)))
    (export "response" (type (sub resource)))
    (type (option string))
    (type (option u16))
    (type (record (field "rcode" 2) (field "info-code" 3)))
    (export "DNS-error-payload" (type (eq 4)))
    (type (variant
      (case "DNS-timeout")
      (case "DNS-error" 5)
      (case "connection-refused")
      (case "internal-error" 2)))
    (export "error-code" (type (eq 6)))
  ))
  (import "synth:test/types" (instance (;0;) (type 0)))
  (alias export 0 "request" (type (;1;)))
  (alias export 0 "response" (type (;2;)))
  (alias export 0 "error-code" (type (;3;)))
  (type (;4;) (instance
    (alias outer 1 1 (type (;0;)))
    (export "request" (type (eq 0)))
    (alias outer 1 2 (type (;2;)))
    (export "response" (type (eq 2)))
    (alias outer 1 3 (type (;4;)))
    (export "error-code" (type (eq 4)))
    (type (;6;) (own 1))
    (type (;7;) (own 3))
    (type (;8;) (result 7 (error 5)))
    (type (;9;) (func async (param "request" 6) (result 8)))
    (export "handle" (func (type 9)))
  ))
  (import "{target}" (instance (;1;) (type 4)))
)
"#
    )
}

/// Helper: generate an adapter and return the raw bytes.
fn gen_adapter(target: &str, hooks: &[&str], iface: &InterfaceType, arena: &TypeArena) -> Vec<u8> {
    let tmp = tempfile::tempdir().unwrap();
    let hook_strings: Vec<String> = hooks.iter().map(|s| s.to_string()).collect();
    let split = synth_consumer_split(target, iface, arena);
    let split_path = split.path().to_str().expect("tempfile path utf-8");
    let path = generate_tier1_adapter(
        "test-mdl",
        target,
        &hook_strings,
        Some(iface),
        tmp.path().to_str().unwrap(),
        split_path,
        arena,
    )
    .expect("adapter generation should succeed");
    std::fs::read(&path).expect("should read generated adapter file")
}

fn make_iface(funcs: Vec<(&str, FuncSignature)>) -> InterfaceType {
    InterfaceType::Instance(InstanceInterface {
        functions: funcs.into_iter().map(|(n, s)| (n.to_string(), s)).collect(),
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
    let iface = make_iface(vec![(
        "add",
        sig(false, &["a", "b"], vec![s32, s32], vec![s32]),
    )]);
    let bytes = gen_adapter(
        "test:pkg/adder@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
    );
    validate_component(&bytes);
}

// ── Tier 1: sync string return (retptr pattern) ─────────────────────

#[test]
fn test_adapter_sync_string_return() {
    let mut arena = TypeArena::default();
    let string = arena.intern_val(ValueType::String);
    let iface = make_iface(vec![("get-msg", sig(false, &[], vec![], vec![string]))]);
    let bytes = gen_adapter(
        "test:pkg/messenger@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
    );
    validate_component(&bytes);
}

// ── Tier 1: sync string param + string return ───────────────────────

#[test]
fn test_adapter_sync_string_roundtrip() {
    let mut arena = TypeArena::default();
    let string = arena.intern_val(ValueType::String);
    let iface = make_iface(vec![(
        "echo",
        sig(false, &["input"], vec![string], vec![string]),
    )]);
    let bytes = gen_adapter(
        "test:pkg/echo@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
    );
    validate_component(&bytes);
}

// ── Tier 1: async string return ──────────────────────────────────────

#[test]
fn test_adapter_async_string_return() {
    let mut arena = TypeArena::default();
    let string = arena.intern_val(ValueType::String);
    let iface = make_iface(vec![("get-msg", sig(true, &[], vec![], vec![string]))]);
    let bytes = gen_adapter(
        "test:pkg/messenger@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
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
        &["splicer:tier1/before", "splicer:tier1/after"],
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
        &["splicer:tier1/before", "splicer:tier1/after"],
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
        &["splicer:tier1/before", "splicer:tier1/after"],
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
        &["splicer:tier1/before"],
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
        &["splicer:tier1/after"],
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
            "splicer:tier1/before",
            "splicer:tier1/blocking",
            "splicer:tier1/after",
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
    let iface = make_iface(vec![(
        "add",
        sig(false, &["a", "b"], vec![s32, s32], vec![s32]),
    )]);
    let bytes = gen_adapter("test:pkg/adder@1.0.0", &[], &iface, &arena);
    validate_component(&bytes);
}
