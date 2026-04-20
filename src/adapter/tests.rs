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
use std::collections::{BTreeMap, HashMap};

/// Helper: validate that bytes form a valid component-model binary.
fn validate_component(bytes: &[u8]) {
    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    if let Err(e) = validator.validate_all(bytes) {
        let dbg_path = std::env::temp_dir().join("splicer_failing_adapter.wasm");
        let _ = std::fs::write(&dbg_path, bytes);
        panic!(
            "generated adapter should be a valid component: {e}\n\
             (raw bytes written to {}, use `wasm-tools print` to inspect)",
            dbg_path.display(),
        );
    }
}

/// Map a primitive or String `ValueType` to its WAT spelling, for use
/// when rendering function parameter / result types in the synth split.
/// Panics on anything non-primitive — the synth split generator
/// expects the resource-handler case to go through the hardcoded WAT
/// template in [`synth_split`].
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
        ValueType::List(inner) => format!("(list {})", wat_type(*inner, arena)),
        ValueType::FixedSizeList(inner, n) => {
            format!("(list {} {n})", wat_type(*inner, arena))
        }
        ValueType::Option(inner) => format!("(option {})", wat_type(*inner, arena)),
        ValueType::Result { ok, err } => {
            let ok_str = ok.map(|id| wat_type(id, arena));
            let err_str = err.map(|id| wat_type(id, arena));
            match (ok_str, err_str) {
                (Some(o), Some(e)) => format!("(result {o} (error {e}))"),
                (Some(o), None) => format!("(result {o})"),
                (None, Some(e)) => format!("(result (error {e}))"),
                (None, None) => "(result)".into(),
            }
        }
        ValueType::Tuple(ids) => {
            let inner: Vec<String> = ids.iter().map(|id| wat_type(*id, arena)).collect();
            format!("(tuple {})", inner.join(" "))
        }
        ValueType::Record(fields) => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(name, id)| format!(r#"(field "{name}" {})"#, wat_type(*id, arena)))
                .collect();
            format!("(record {})", inner.join(" "))
        }
        ValueType::Variant(cases) => {
            let inner: Vec<String> = cases
                .iter()
                .map(|(name, opt_id)| match opt_id {
                    Some(id) => format!(r#"(case "{name}" {})"#, wat_type(*id, arena)),
                    None => format!(r#"(case "{name}")"#),
                })
                .collect();
            format!("(variant {})", inner.join(" "))
        }
        ValueType::Enum(tags) => {
            let inner: Vec<String> = tags.iter().map(|t| format!(r#""{t}""#)).collect();
            format!("(enum {})", inner.join(" "))
        }
        ValueType::Flags(names) => {
            let inner: Vec<String> = names.iter().map(|n| format!(r#""{n}""#)).collect();
            format!("(flags {})", inner.join(" "))
        }
        other => panic!(
            "wat_type: synth split helper only supports primitive + string + \
             list + compound (option/result/tuple/record/variant/enum/flags) \
             types, got {other:?}. For resources, use a dedicated WAT template."
        ),
    }
}

/// Which shape of split to synthesize. The two code paths through
/// [`super::component::emit_imports_from_split`] — `handler_in_split`
/// true vs false — are exercised by whether the target interface
/// appears as an import or an export in the split, so we test both.
#[derive(Clone, Copy)]
enum SplitKind {
    /// The split imports the target interface (middleware-style).
    /// Exercises the `handler_in_split = true` path: the adapter
    /// copies the handler import + types verbatim and aliases its
    /// exports.
    Consumer,
    /// The split exports the target interface (outermost-provider
    /// style). Exercises the `handler_in_split = false` path: the
    /// closure walker finds the target as an export, the adapter
    /// builds a fresh handler import type on top of the preamble's
    /// aliased types.
    Provider,
}

/// Build a minimal split component for the given `kind`. Written as
/// WAT and parsed with `wat::parse_str` — easier to audit than
/// wasm_encoder for a fixture whose shape we want to match the
/// convention a real production split uses.
fn synth_split(
    target: &str,
    iface: &InterfaceType,
    arena: &TypeArena,
    kind: SplitKind,
) -> tempfile::NamedTempFile {
    let iface_inst = match iface {
        InterfaceType::Instance(i) => i,
        _ => panic!("synth_split: bare function interfaces not supported"),
    };

    // Route to the HTTP-handler-shape template only when the
    // interface has RESOURCE type exports (which need the special
    // types-instance import + alias-outer preamble). Non-resource
    // compounds (records, enums, variants in type_exports) are
    // handled by the generic template via collect_compound_decls.
    let has_resources = iface_inst
        .type_exports
        .values()
        .any(|&vid| matches!(arena.lookup_val(vid), ValueType::Resource(_)));
    let wat = match (kind, has_resources) {
        (SplitKind::Consumer, false) => wat_consumer_primitive_only(target, iface_inst, arena),
        (SplitKind::Consumer, true) => wat_consumer_http_handler_shape(target),
        (SplitKind::Provider, false) => wat_provider_primitive_only(target, iface_inst, arena),
        (SplitKind::Provider, true) => wat_provider_http_handler_shape(target),
    };

    let bytes = wat::parse_str(&wat).unwrap_or_else(|e| {
        panic!("synth split WAT failed to parse: {e}\n\n--- WAT ---\n{wat}\n--- end ---")
    });
    let mut tmp = tempfile::NamedTempFile::new().expect("make tempfile");
    std::io::Write::write_all(&mut tmp, &bytes).expect("write synth split");
    tmp
}

fn wat_consumer_primitive_only(
    target: &str,
    iface: &InstanceInterface,
    arena: &TypeArena,
) -> String {
    // Named-type refs ($fn_{name}) instead of numeric indices, so
    // inline compound types like `(list u32)` — which the WAT
    // parser allocates their own type slots for — don't shift the
    // numbering and break later references.
    //
    // Compound types (record / variant / enum / flags) used as
    // param or result types would fail the component-model
    // validator's rule that import instance types can't reference
    // anonymous compounds. Pre-declare each such type as a named
    // type, export with `(eq $name)`, and reference by $name from
    // the function signatures.

    // Pre-declare compound types from type_exports. The adapter
    // aliases these by NAME from the handler instance, so export
    // names must match the type_exports keys. Each compound gets
    // TWO type-space slots: the inline declaration and the (eq N)
    // export. Resources are skipped — the HTTP handler template
    // handles those with the special alias-outer preamble.
    let mut compounds: HashMap<ValueTypeId, u32> = HashMap::new();
    let mut body = String::new();
    let mut type_idx: u32 = 0;
    for (export_name, &vid) in &iface.type_exports {
        if matches!(
            arena.lookup_val(vid),
            ValueType::Resource(_) | ValueType::AsyncHandle
        ) {
            continue;
        }
        let body_str = wat_compound_body(vid, arena, &compounds);
        body.push_str(&format!("      (type (;{type_idx};) {body_str})\n"));
        let decl_idx = type_idx;
        type_idx += 1;
        body.push_str(&format!(
            "      (export (;{type_idx};) \"{export_name}\" (type (eq {decl_idx})))\n"
        ));
        compounds.insert(vid, type_idx);
        type_idx += 1;
    }

    let mut export_lines = String::new();
    for (name, sig) in &iface.functions {
        let params: Vec<String> = sig
            .param_names
            .iter()
            .zip(sig.params.iter())
            .map(|(pname, &pid)| {
                format!(
                    r#"(param "{pname}" {})"#,
                    wat_type_ctx(pid, arena, &compounds)
                )
            })
            .collect();
        let result = match sig.results.first() {
            Some(&rid) => format!(" (result {})", wat_type_ctx(rid, arena, &compounds)),
            None => String::new(),
        };
        let async_kw = if sig.is_async { "async " } else { "" };
        let func_ty_id = format!("$fn_{}", name.replace('-', "_"));
        body.push_str(&format!(
            "      (type {func_ty_id} (func {async_kw}{}{result}))\n",
            params.join(" "),
        ));
        export_lines.push_str(&format!(
            "      (export \"{name}\" (func (type {func_ty_id})))\n"
        ));
    }
    body.push_str(&export_lines);

    format!(
        "(component\n  (type $iface (instance\n{body}  ))\n  (import \"{target}\" (instance (type $iface)))\n)\n"
    )
}

/// Render the body of a compound type (what goes inside
/// `(type (;N;) ...)`), with inner compound references resolved to
/// their pre-declared export type indices via `compounds`.
fn wat_compound_body(
    id: ValueTypeId,
    arena: &TypeArena,
    compounds: &HashMap<ValueTypeId, u32>,
) -> String {
    match arena.lookup_val(id) {
        ValueType::Record(fields) => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(name, fid)| {
                    format!(
                        r#"(field "{name}" {})"#,
                        wat_type_ctx(*fid, arena, compounds)
                    )
                })
                .collect();
            format!("(record {})", inner.join(" "))
        }
        ValueType::Variant(cases) => {
            let inner: Vec<String> = cases
                .iter()
                .map(|(name, opt)| match opt {
                    Some(cid) => {
                        format!(
                            r#"(case "{name}" {})"#,
                            wat_type_ctx(*cid, arena, compounds)
                        )
                    }
                    None => format!(r#"(case "{name}")"#),
                })
                .collect();
            format!("(variant {})", inner.join(" "))
        }
        ValueType::Enum(tags) => {
            let items: Vec<String> = tags.iter().map(|t| format!(r#""{t}""#)).collect();
            format!("(enum {})", items.join(" "))
        }
        ValueType::Flags(names) => {
            let items: Vec<String> = names.iter().map(|n| format!(r#""{n}""#)).collect();
            format!("(flags {})", items.join(" "))
        }
        other => panic!(
            "wat_compound_body: {other:?} should not be in the compounds pre-declaration list"
        ),
    }
}

/// Like [`wat_type`] but substitutes a numeric type-index reference
/// for any ValueTypeId found in `compounds` (pre-declared types)
/// — the index is the EXPORT's slot (`(eq N)`), not the inline
/// declaration, because component-model validation rejects
/// anonymous compounds in import instance types.
fn wat_type_ctx(
    id: ValueTypeId,
    arena: &TypeArena,
    compounds: &HashMap<ValueTypeId, u32>,
) -> String {
    if let Some(idx) = compounds.get(&id) {
        return idx.to_string();
    }
    match arena.lookup_val(id) {
        // Containers — recurse into inner with context so nested
        // compound refs also get substituted.
        ValueType::List(inner) => format!("(list {})", wat_type_ctx(*inner, arena, compounds)),
        ValueType::FixedSizeList(inner, n) => {
            format!("(list {} {n})", wat_type_ctx(*inner, arena, compounds))
        }
        ValueType::Option(inner) => {
            format!("(option {})", wat_type_ctx(*inner, arena, compounds))
        }
        ValueType::Result { ok, err } => {
            let ok_str = ok.map(|id| wat_type_ctx(id, arena, compounds));
            let err_str = err.map(|id| wat_type_ctx(id, arena, compounds));
            match (ok_str, err_str) {
                (Some(o), Some(e)) => format!("(result {o} (error {e}))"),
                (Some(o), None) => format!("(result {o})"),
                (None, Some(e)) => format!("(result (error {e}))"),
                (None, None) => "(result)".into(),
            }
        }
        ValueType::Tuple(ids) => {
            let inner: Vec<String> = ids
                .iter()
                .map(|id| wat_type_ctx(*id, arena, compounds))
                .collect();
            format!("(tuple {})", inner.join(" "))
        }
        // Everything else defers to `wat_type` (primitives +
        // compound-as-inline fallback, though compounds should have
        // been caught by the `compounds.get` short-circuit above).
        _ => wat_type(id, arena),
    }
}

/// WAT for the wasi:http/handler-shape fixture used by
/// `test_adapter_resource_handler`. Mirrors `service_a.comp.wasm`'s
/// real handler import structure: a types instance with request /
/// response resources + the error-code variant, then a handler
/// instance type that `alias outer`s each and re-exports via `eq`.
fn wat_consumer_http_handler_shape(target: &str) -> String {
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

/// WAT for a provider split that **exports** the target interface with
/// a trivial primitive signature. Exercises the `handler_in_split = false`
/// path in [`super::component::emit_imports_from_split`].
///
/// Restricted to a single sync function of shape
/// `(param s32 s32) (result s32)` — richer provider shapes are covered
/// by the integration tests under `tests/component-interposition`. The
/// body is a minimal `i32.add` core module; the implementation only has
/// to type-check for the split to parse and the closure walker to find
/// the target as an export.
fn wat_provider_primitive_only(
    target: &str,
    iface: &InstanceInterface,
    arena: &TypeArena,
) -> String {
    assert_eq!(
        iface.functions.len(),
        1,
        "provider primitive helper: single-function ifaces only"
    );
    let (name, sig) = iface.functions.iter().next().unwrap();
    assert!(!sig.is_async, "provider primitive helper: sync funcs only");
    assert_eq!(
        sig.params.len(),
        2,
        "provider primitive helper: 2-param funcs only"
    );
    assert!(
        sig.params
            .iter()
            .all(|&p| matches!(arena.lookup_val(p), ValueType::S32)),
        "provider primitive helper: s32 params only"
    );
    assert_eq!(sig.results.len(), 1);
    assert!(
        matches!(arena.lookup_val(sig.results[0]), ValueType::S32),
        "provider primitive helper: s32 result only"
    );

    let pa = &sig.param_names[0];
    let pb = &sig.param_names[1];
    format!(
        r#"(component
  (core module (;0;)
    (func (export "{name}") (param i32 i32) (result i32)
      local.get 0
      local.get 1
      i32.add))
  (core instance (;0;) (instantiate 0))
  (alias core export 0 "{name}" (core func (;0;)))
  (type (;0;) (func (param "{pa}" s32) (param "{pb}" s32) (result s32)))
  (func (;0;) (type 0) (canon lift (core func 0)))
  (instance (;0;) (export "{name}" (func 0)))
  (export "{target}" (instance 0))
)
"#
    )
}

/// WAT for a provider split that **exports** an HTTP-handler-shape
/// interface. Mirrors [`wat_consumer_http_handler_shape`]'s preamble
/// (types instance with request/response resources + the error-code
/// variant, aliased at component scope, and a handler instance type
/// that references them via `alias outer`) but flips the handler from
/// import to export.
///
/// To avoid hand-rolling an async canon lift + realloc for
/// `result<own<response>, error-code>`, the provider re-exports an
/// imported instance from a neighbor interface. The adapter's closure
/// walker seeds BFS from the export and pulls the same preamble a real
/// provider component would carry. The neighbor import has a
/// non-target name, so `handler_in_split` stays false and the
/// `emit_imports_from_split` provider branch runs.
fn wat_provider_http_handler_shape(target: &str) -> String {
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
  (import "impl:test/handler" (instance (;1;) (type 4)))
  (export "{target}" (instance 1))
)
"#
    )
}

/// Helper: generate an adapter and return the raw bytes.
fn gen_adapter(
    target: &str,
    hooks: &[&str],
    iface: &InterfaceType,
    arena: &TypeArena,
    kind: SplitKind,
) -> Vec<u8> {
    let tmp = tempfile::tempdir().unwrap();
    let hook_strings: Vec<String> = hooks.iter().map(|s| s.to_string()).collect();
    let split = synth_split(target, iface, arena, kind);
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
        SplitKind::Consumer,
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
        SplitKind::Consumer,
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
        SplitKind::Consumer,
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
        SplitKind::Consumer,
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
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Build an HTTP-handler-shape interface matching the WAT emitted by
/// [`wat_consumer_http_handler_shape`] / [`wat_provider_http_handler_shape`]:
/// `request` and `response` resources, an `error-code` variant (with a
/// DNS-error-payload record case), and an async
/// `handle(request) -> result<response, error-code>`. Shared by the
/// consumer- and provider-side resource-handler tests so both exercise
/// the exact same type graph.
fn build_http_handler_iface(arena: &mut TypeArena) -> InterfaceType {
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
    InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([("handle".to_string(), func)]),
        type_exports: BTreeMap::from([
            ("request".to_string(), request),
            ("response".to_string(), response),
            ("error-code".to_string(), error_code),
        ]),
    })
}

// ── Tier 1: async with resource types (HTTP handler pattern) ─────────

#[test]
fn test_adapter_resource_handler() {
    let mut arena = TypeArena::default();
    let iface = build_http_handler_iface(&mut arena);
    let bytes = gen_adapter(
        "wasi:http/handler@0.3.0-rc-2026-01-06",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

// ── Tier 1: list<T> (needs_realloc detection) ────────────────────────

/// A function with a `list<u32>` parameter requires canon lower to
/// allocate memory for the marshaled list contents, so the emitted
/// memory module must include `realloc`. This verifies that the
/// `needs_realloc` predicate detects lists (not just strings and
/// resources) and that the resulting component validates end-to-end.
#[test]
fn test_adapter_list_param_sync() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let list_u32 = arena.intern_val(ValueType::List(u32_id));
    let iface = make_iface(vec![(
        "sum",
        sig(false, &["xs"], vec![list_u32], vec![u32_id]),
    )]);
    let bytes = gen_adapter(
        "test:pkg/summer@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// List-as-result path: the canon lift (export side) needs the
/// `Memory` and `Realloc` options to lift `list<u32>` results too.
#[test]
fn test_adapter_list_result_sync() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let list_u32 = arena.intern_val(ValueType::List(u32_id));
    let iface = make_iface(vec![(
        "range",
        sig(false, &["n"], vec![u32_id], vec![list_u32]),
    )]);
    let bytes = gen_adapter(
        "test:pkg/ranger@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Fixed-size-list param — `list<T, N>` must flatten to `N × flat(T)`
/// inlined on the wasm stack (not to a `(ptr, len)` pair like
/// dynamic `list<T>`). This drives both component-level type encoders
/// (`InstTypeCtx::encode_cv` and `encode_comp_cv`) and the
/// `WitBridge::flat_types` flattening the adapter uses for
/// canon-lift/lower.
#[test]
fn test_adapter_fixed_size_list_param_sync() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let fsl = arena.intern_val(ValueType::FixedSizeList(u32_id, 4));
    let iface = make_iface(vec![("take", sig(false, &["buf"], vec![fsl], vec![]))]);
    let bytes = gen_adapter(
        "test:pkg/taker@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Async path with a list parameter — exercises the list-detection
/// through the async canon-lower and task.return emission, not just
/// the sync canon-lift path.
#[test]
fn test_adapter_list_param_async() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let list_u32 = arena.intern_val(ValueType::List(u32_id));
    let iface = make_iface(vec![(
        "process",
        sig(true, &["xs"], vec![list_u32], vec![]),
    )]);
    let bytes = gen_adapter(
        "test:pkg/processor@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

// ── Tier 1: subword-aligned shapes (canonical-ABI correctness) ───────
//
// These shapes test that the adapter's lift-from-memory path handles
// non-4-byte payload offsets: `Option<u8>` / `Option<u16>` /
// `Result<u8, u8>` / records with subword fields all have
// canonical-ABI payload offsets that don't match simple
// ValType-byte-width offsets. The tests validate that generation
// succeeds and the binary structurally validates — actual runtime
// correctness needs execution (covered by the __testme integration
// suite in tests/component-interposition/).

#[test]
fn test_adapter_option_u8_async_result() {
    let mut arena = TypeArena::default();
    let u8_id = arena.intern_val(ValueType::U8);
    let opt_u8 = arena.intern_val(ValueType::Option(u8_id));
    let iface = make_iface(vec![("get", sig(true, &[], vec![], vec![opt_u8]))]);
    let bytes = gen_adapter(
        "test:pkg/get@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

#[test]
fn test_adapter_option_u16_async_result() {
    let mut arena = TypeArena::default();
    let u16_id = arena.intern_val(ValueType::U16);
    let opt_u16 = arena.intern_val(ValueType::Option(u16_id));
    let iface = make_iface(vec![("get", sig(true, &[], vec![], vec![opt_u16]))]);
    let bytes = gen_adapter(
        "test:pkg/get@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

#[test]
fn test_adapter_result_u8_u8_async_result() {
    let mut arena = TypeArena::default();
    let u8_id = arena.intern_val(ValueType::U8);
    let result = arena.intern_val(ValueType::Result {
        ok: Some(u8_id),
        err: Some(u8_id),
    });
    let iface = make_iface(vec![("get", sig(true, &[], vec![], vec![result]))]);
    let bytes = gen_adapter(
        "test:pkg/get@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Record with subword fields as an async result — exercises the
/// consumer-WAT template's compound-type pre-declaration and the
/// adapter's `comp_aliased_types` re-export flow. Mirrors
/// real-WIT shape by putting the record in `type_exports` (that's
/// how cviz surfaces named compounds from compiled WIT).
#[test]
fn test_adapter_record_with_subword_fields_async_result() {
    let mut arena = TypeArena::default();
    let bool_id = arena.intern_val(ValueType::Bool);
    let u32_id = arena.intern_val(ValueType::U32);
    let u16_id = arena.intern_val(ValueType::U16);
    let record = arena.intern_val(ValueType::Record(vec![
        ("flag".into(), bool_id),
        ("count".into(), u32_id),
        ("tag".into(), u16_id),
    ]));
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([("get".to_string(), sig(true, &[], vec![], vec![record]))]),
        type_exports: BTreeMap::from([("my-record".to_string(), record)]),
    });
    let bytes = gen_adapter(
        "test:pkg/get@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Enum as an async result — same pattern: enum is named in
/// `type_exports`, matching how real WIT compiles.
#[test]
fn test_adapter_enum_async_result() {
    let mut arena = TypeArena::default();
    let en = arena.intern_val(ValueType::Enum(vec![
        "red".into(),
        "green".into(),
        "blue".into(),
    ]));
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([("get".to_string(), sig(true, &[], vec![], vec![en]))]),
        type_exports: BTreeMap::from([("color".to_string(), en)]),
    });
    let bytes = gen_adapter(
        "test:pkg/get@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

// ── Tier 1: shape-matrix coverage ────────────────────────────────────
//
// Targeted tests for shapes called out in
// docs/TODO/test-with-real-compositions.md that aren't exercised by
// the wasi:http / primitives / subword coverage above. Each uses an
// async result so the value flows through
// `build_task_return_loads` → `lift_from_memory` — the code path most
// exposed to canonical-ABI subtleties.

/// Record result with every alignment boundary represented: 1-byte,
/// 4-byte, 2-byte, 8-byte fields in an order that forces each field
/// to pad up to its natural alignment. Pins the inter-field padding
/// and subword-offset math.
#[test]
fn test_adapter_mixed_alignment_record_async_result() {
    let mut arena = TypeArena::default();
    let u8_id = arena.intern_val(ValueType::U8);
    let u32_id = arena.intern_val(ValueType::U32);
    let u16_id = arena.intern_val(ValueType::U16);
    let u64_id = arena.intern_val(ValueType::U64);
    let record = arena.intern_val(ValueType::Record(vec![
        ("a".into(), u8_id),
        ("b".into(), u32_id),
        ("c".into(), u16_id),
        ("d".into(), u64_id),
    ]));
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([("get".to_string(), sig(true, &[], vec![], vec![record]))]),
        type_exports: BTreeMap::from([("mixed".to_string(), record)]),
    });
    let bytes = gen_adapter(
        "test:pkg/mixed@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Variant with numerically heterogeneous arms: `u8` / `u64` / `f64`
/// flatten to `[i32]` / `[i64]` / `[f64]`, joining to `[i64]` at the
/// payload position. Exercises `cast(I32, I64)` and `cast(F64, I64)`
/// in the same variant dispatch — cells of the bitcast table that
/// wasi:http's error variant doesn't happen to hit.
#[test]
fn test_adapter_heterogeneous_numeric_variant_async_result() {
    let mut arena = TypeArena::default();
    let u8_id = arena.intern_val(ValueType::U8);
    let u64_id = arena.intern_val(ValueType::U64);
    let f64_id = arena.intern_val(ValueType::F64);
    let v = arena.intern_val(ValueType::Variant(vec![
        ("x".into(), Some(u8_id)),
        ("y".into(), Some(u64_id)),
        ("z".into(), Some(f64_id)),
    ]));
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([("get".to_string(), sig(true, &[], vec![], vec![v]))]),
        type_exports: BTreeMap::from([("mixed-v".to_string(), v)]),
    });
    let bytes = gen_adapter(
        "test:pkg/mixed-v@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// A function with 17 flat params exceeds the canonical-ABI cap
/// (`MAX_FLAT_PARAMS = 16`). Splicer currently bails at generation
/// time with a clear error rather than emit invalid core-wasm; this
/// test pins that contract so a future change that silently routes
/// around the check (instead of implementing the retptr form) fails
/// loud.
#[test]
fn test_adapter_too_many_flat_params_fails_cleanly() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let param_names: Vec<String> = (0..17).map(|i| format!("p{i}")).collect();
    let iface = make_iface(vec![(
        "many",
        FuncSignature {
            is_async: false,
            param_names,
            params: vec![u32_id; 17],
            results: vec![],
        },
    )]);
    let tmp = tempfile::tempdir().unwrap();
    let split = synth_split("test:pkg/many@1.0.0", &iface, &arena, SplitKind::Consumer);
    let err = generate_tier1_adapter(
        "test-mdl",
        "test:pkg/many@1.0.0",
        &[],
        Some(&iface),
        tmp.path().to_str().unwrap(),
        split.path().to_str().unwrap(),
        &arena,
    )
    .expect_err("generation should fail when a function's flat arity exceeds MAX_FLAT_PARAMS");
    let msg = err.to_string();
    assert!(
        msg.contains("flat") || msg.contains("16"),
        "error should mention the flat-param limit, got: {msg}"
    );
}

/// Build, generate, and validate an async-result adapter whose result
/// is a `flags` with `n` labels. Shared helper for the width-boundary
/// tests below.
fn gen_flags_adapter(n: usize) {
    let mut arena = TypeArena::default();
    let names: Vec<String> = (0..n).map(|i| format!("f{i}")).collect();
    let flags = arena.intern_val(ValueType::Flags(names));
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([("get".to_string(), sig(true, &[], vec![], vec![flags]))]),
        type_exports: BTreeMap::from([("fs".to_string(), flags)]),
    });
    let bytes = gen_adapter(
        "test:pkg/fs@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

// Flags storage widths per the canonical ABI:
//   ≤ 8  labels → 1 byte    (i32.load8_u)
//   ≤ 16 labels → 2 bytes   (i32.load16_u)
//   ≤ 32 labels → 4 bytes   (single i32.load)
// Each width is a different load shape in the Bindgen emit.
//
// The Component Model binary format caps `flags` at 32 members; a
// 33-label flags type fails component-level validation at the type
// section ("cannot have more than 32 flags"). wit-parser and the
// canonical-ABI spec describe a multi-word encoding for 33+ flags
// (FlagsRepr::U32(n)), but that encoding isn't reachable through the
// component type system, so it can't be exercised here and isn't
// tested below.

#[test]
fn test_adapter_flags_1_label_async_result() {
    gen_flags_adapter(1);
}
#[test]
fn test_adapter_flags_8_labels_async_result() {
    gen_flags_adapter(8);
}
#[test]
fn test_adapter_flags_16_labels_async_result() {
    gen_flags_adapter(16);
}
#[test]
fn test_adapter_flags_32_labels_async_result() {
    gen_flags_adapter(32);
}

/// Variant with 300 cases — past the 256-case boundary, so the
/// discriminant widens from `u8` to `u16` per the canonical ABI.
/// The dispatch emits `i32.load16_u` for the disc instead of the
/// `i32.load8_u` used by smaller variants. Pins that path.
#[test]
fn test_adapter_variant_over_256_cases_async_result() {
    let mut arena = TypeArena::default();
    let cases: Vec<(String, Option<ValueTypeId>)> =
        (0..300).map(|i| (format!("c{i:03}"), None)).collect();
    let v = arena.intern_val(ValueType::Variant(cases));
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([("get".to_string(), sig(true, &[], vec![], vec![v]))]),
        type_exports: BTreeMap::from([("big-v".to_string(), v)]),
    });
    let bytes = gen_adapter(
        "test:pkg/big-v@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
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
        SplitKind::Consumer,
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
        SplitKind::Consumer,
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
        SplitKind::Consumer,
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
        SplitKind::Consumer,
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
    let bytes = gen_adapter(
        "test:pkg/adder@1.0.0",
        &[],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

// ── Tier 1: provider split (handler exported, not imported) ──────────

/// Exercises the `!handler_in_split` branch of
/// [`super::component::emit_imports_from_split`] with an
/// empty preamble: the split *exports* the target interface instead of
/// importing it, so the adapter builds a fresh handler import type
/// rather than copying the one from the split's raw sections. For a
/// primitive-only interface the preamble is empty, so `outer_aliased`
/// stays empty and the adapter's import type contains only the handler
/// func signature.
#[test]
fn test_adapter_provider_split_primitive() {
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
        SplitKind::Provider,
    );
    validate_component(&bytes);
}

/// Exercises the `!handler_in_split` branch with a populated preamble:
/// the provider split exports an HTTP-handler-shape interface whose
/// resource types (`request`, `response`) and compound type
/// (`error-code` variant) are aliased at component scope. The adapter
/// must route those aliased types through `outer_aliased` into the
/// fresh handler import type's body via `alias outer`, and reuse the
/// preamble's component-scope indices for the dispatch phase rather
/// than aliasing them off the handler instance (which wouldn't work —
/// the resources came from the preamble, not from the handler
/// instance's SubResource exports).
#[test]
fn test_adapter_provider_split_resource_handler() {
    let mut arena = TypeArena::default();
    let iface = build_http_handler_iface(&mut arena);
    let bytes = gen_adapter(
        "wasi:http/handler@0.3.0-rc-2026-01-06",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Provider,
    );
    validate_component(&bytes);
}
