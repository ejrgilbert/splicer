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
use cviz::parse::component::parse_component;
use std::collections::{BTreeMap, HashMap, HashSet};

mod fuzz;

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
    /// Consumer-style split where the target interface's compound
    /// types come from a *sibling* types-instance import and are
    /// referenced inside the target-interface type via `alias outer`
    /// (the shape real WITs produce for `use other:pkg/types.{X}`).
    /// Value-type only — mirrors the nebula `orders` composition
    /// shape that surfaced the cross-interface aliasing bug.
    ConsumerSiblingTypes,
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
        (SplitKind::ConsumerSiblingTypes, _) => wat_consumer_cross_interface_value_types(target),
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
    // Import instance types have a specific rule: any type that is
    // exported (`(export "X" (type (eq N)))`) creates an ADDITIONAL
    // type slot at N+1, and later types that reference the exported
    // type must point at the EXPORT slot, not the raw declaration.
    // See wasi:http's shape: its variant's `(case "DNS-error" 5)`
    // references the exported record at slot 5, not the raw record at
    // slot 4. Non-exported compounds can still be referenced by their
    // raw declaration slot.
    //
    // This helper tracks an "effective" index per compound:
    //   - exported compound → its export slot
    //   - non-exported compound → its raw declaration slot
    //
    // Previous implementations (named-type-based, or naive numeric)
    // didn't distinguish exported from non-exported and so miscalc-
    // ulated references to exported sub-compounds, producing
    // `instance not valid to be used as import` validation errors.
    let mut order: Vec<ValueTypeId> = Vec::new();
    let mut visited: HashSet<ValueTypeId> = HashSet::new();

    for sig in iface.functions.values() {
        for &pid in &sig.params {
            collect_compound_order(pid, arena, &mut visited, &mut order);
        }
        for &rid in &sig.results {
            collect_compound_order(rid, arena, &mut visited, &mut order);
        }
    }
    for &vid in iface.type_exports.values() {
        collect_compound_order(vid, arena, &mut visited, &mut order);
    }

    // Multiple `type_exports` keys can point at the same ValueTypeId
    // (real WIT allows `type foo = u32; type bar = u32;` as two
    // exports of the same underlying type — and interning in
    // `TypeArena` collapses them further). Each name gets its own
    // export; the first export's slot becomes the compound's
    // "effective" reference index.
    let mut exports_by_id: HashMap<ValueTypeId, Vec<String>> = HashMap::new();
    for (export_name, &vid) in &iface.type_exports {
        if matches!(
            arena.lookup_val(vid),
            ValueType::Resource(_) | ValueType::AsyncHandle
        ) {
            continue;
        }
        exports_by_id
            .entry(vid)
            .or_default()
            .push(export_name.clone());
    }

    let mut effective: HashMap<ValueTypeId, u32> = HashMap::new();
    let mut next_slot: u32 = 0;
    let mut body = String::new();

    for &id in &order {
        let decl_body = wat_compound_decl_body(id, arena, &effective);
        body.push_str(&format!("      (type (;{next_slot};) {decl_body})\n"));
        let decl_slot = next_slot;
        next_slot += 1;
        effective.insert(id, decl_slot);

        if let Some(names) = exports_by_id.get(&id) {
            for (i, export_name) in names.iter().enumerate() {
                body.push_str(&format!(
                    "      (export (;{next_slot};) \"{export_name}\" (type (eq {decl_slot})))\n"
                ));
                if i == 0 {
                    effective.insert(id, next_slot);
                }
                next_slot += 1;
            }
        }
    }

    let mut export_lines = String::new();
    for (fname, sig) in &iface.functions {
        let params: Vec<String> = sig
            .param_names
            .iter()
            .zip(sig.params.iter())
            .map(|(pn, &pid)| format!(r#"(param "{pn}" {})"#, wat_ref(pid, arena, &effective)))
            .collect();
        let result = match sig.results.first() {
            Some(&rid) => format!(" (result {})", wat_ref(rid, arena, &effective)),
            None => String::new(),
        };
        let async_kw = if sig.is_async { "async " } else { "" };
        let func_slot = next_slot;
        body.push_str(&format!(
            "      (type (;{func_slot};) (func {async_kw}{}{result}))\n",
            params.join(" "),
        ));
        next_slot += 1;
        export_lines.push_str(&format!(
            "      (export \"{fname}\" (func (type {func_slot})))\n"
        ));
    }
    body.push_str(&export_lines);

    format!(
        "(component\n  (type $iface (instance\n{body}  ))\n  (import \"{target}\" (instance (type $iface)))\n)\n"
    )
}

/// Post-order collection: every compound reachable from `id`, children
/// before parents. Primitives, resources, and async handles are
/// skipped — resources flow through a different WAT template.
fn collect_compound_order(
    id: ValueTypeId,
    arena: &TypeArena,
    visited: &mut HashSet<ValueTypeId>,
    order: &mut Vec<ValueTypeId>,
) {
    if !visited.insert(id) {
        return;
    }
    match arena.lookup_val(id) {
        ValueType::Bool
        | ValueType::S8
        | ValueType::U8
        | ValueType::S16
        | ValueType::U16
        | ValueType::S32
        | ValueType::U32
        | ValueType::S64
        | ValueType::U64
        | ValueType::F32
        | ValueType::F64
        | ValueType::Char
        | ValueType::String
        | ValueType::ErrorContext
        | ValueType::Resource(_)
        | ValueType::AsyncHandle
        | ValueType::Map(_, _) => return,
        ValueType::List(inner) | ValueType::Option(inner) | ValueType::FixedSizeList(inner, _) => {
            collect_compound_order(*inner, arena, visited, order);
        }
        ValueType::Result { ok, err } => {
            if let Some(o) = ok {
                collect_compound_order(*o, arena, visited, order);
            }
            if let Some(e) = err {
                collect_compound_order(*e, arena, visited, order);
            }
        }
        ValueType::Tuple(ids) => {
            for cid in ids.clone() {
                collect_compound_order(cid, arena, visited, order);
            }
        }
        ValueType::Record(fields) => {
            for (_, fid) in fields.clone() {
                collect_compound_order(fid, arena, visited, order);
            }
        }
        ValueType::Variant(cases) => {
            for (_, opt) in cases.clone() {
                if let Some(cid) = opt {
                    collect_compound_order(cid, arena, visited, order);
                }
            }
        }
        ValueType::Enum(_) | ValueType::Flags(_) => {}
    }
    order.push(id);
}

/// Primitive → WAT spelling; compound → its effective type-space
/// slot (export slot if exported, raw decl slot otherwise).
fn wat_ref(id: ValueTypeId, arena: &TypeArena, effective: &HashMap<ValueTypeId, u32>) -> String {
    match arena.lookup_val(id) {
        ValueType::Bool => "bool".into(),
        ValueType::S8 => "s8".into(),
        ValueType::U8 => "u8".into(),
        ValueType::S16 => "s16".into(),
        ValueType::U16 => "u16".into(),
        ValueType::S32 => "s32".into(),
        ValueType::U32 => "u32".into(),
        ValueType::S64 => "s64".into(),
        ValueType::U64 => "u64".into(),
        ValueType::F32 => "f32".into(),
        ValueType::F64 => "f64".into(),
        ValueType::Char => "char".into(),
        ValueType::String => "string".into(),
        _ => effective
            .get(&id)
            .map(|n| n.to_string())
            .unwrap_or_else(|| panic!("wat_ref: no effective slot for {id:?}")),
    }
}

/// Render the body of a compound type declaration (what goes inside
/// `(type (;N;) …)`). Sub-compound references go through
/// [`wat_ref`], which picks the correct effective slot.
fn wat_compound_decl_body(
    id: ValueTypeId,
    arena: &TypeArena,
    effective: &HashMap<ValueTypeId, u32>,
) -> String {
    match arena.lookup_val(id) {
        ValueType::List(inner) => format!("(list {})", wat_ref(*inner, arena, effective)),
        ValueType::FixedSizeList(inner, n) => {
            format!("(list {} {n})", wat_ref(*inner, arena, effective))
        }
        ValueType::Option(inner) => format!("(option {})", wat_ref(*inner, arena, effective)),
        ValueType::Result { ok, err } => {
            let ok_s = ok.map(|id| wat_ref(id, arena, effective));
            let err_s = err.map(|id| wat_ref(id, arena, effective));
            match (ok_s, err_s) {
                (Some(o), Some(e)) => format!("(result {o} (error {e}))"),
                (Some(o), None) => format!("(result {o})"),
                (None, Some(e)) => format!("(result (error {e}))"),
                (None, None) => "(result)".into(),
            }
        }
        ValueType::Tuple(ids) => {
            let inner: Vec<String> = ids
                .iter()
                .map(|id| wat_ref(*id, arena, effective))
                .collect();
            format!("(tuple {})", inner.join(" "))
        }
        ValueType::Record(fields) => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(n, fid)| format!(r#"(field "{n}" {})"#, wat_ref(*fid, arena, effective)))
                .collect();
            format!("(record {})", inner.join(" "))
        }
        ValueType::Variant(cases) => {
            let inner: Vec<String> = cases
                .iter()
                .map(|(n, opt)| match opt {
                    Some(cid) => format!(r#"(case "{n}" {})"#, wat_ref(*cid, arena, effective)),
                    None => format!(r#"(case "{n}")"#),
                })
                .collect();
            format!("(variant {})", inner.join(" "))
        }
        ValueType::Enum(tags) => {
            let items: Vec<String> = tags.iter().map(|t| format!(r#""{t}""#)).collect();
            format!("(enum {})", items.join(" "))
        }
        ValueType::Flags(labels) => {
            let items: Vec<String> = labels.iter().map(|n| format!(r#""{n}""#)).collect();
            format!("(flags {})", items.join(" "))
        }
        other => panic!("wat_compound_decl_body: {other:?} is not a declarable compound"),
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

/// WAT for a consumer split whose target interface references compound
/// types defined in a sibling types-instance import, rather than
/// declaring them inline. Mirrors the split shape produced for WIT
/// that uses `use other:pkg/types.{X}` (e.g. the nebula `orders`
/// interface pulling `order`/`quote` from `nebula:core/types`).
///
/// The handler instance type does NOT re-export these shared types as
/// direct `(export "name" (type (eq N)))` members — it only
/// `alias outer`s them from the types instance to reference in its
/// function signatures. That's what distinguishes this fixture from
/// the HTTP-handler shape: `error-code` there IS re-exported on the
/// handler instance, so `Alias::InstanceExport` succeeds downstream.
/// Here the shared types live only at component scope + inside the
/// types instance, which exposes the cross-interface aliasing bug.
///
/// Hardcoded to the nebula `orders` shape (records + enums + list);
/// adding more fixtures would be straightforward by parameterizing
/// the type decls.
fn wat_consumer_cross_interface_value_types(target: &str) -> String {
    format!(
        r#"(component
  (type (;0;) (instance
    (type (record (field "sku" string) (field "quantity" u32) (field "unit-price" f64)))
    (export "item" (type (eq 0)))
    (type (list 1))
    (type (enum "BE" "US" "UK" "JP" "CA" "AU"))
    (export "country" (type (eq 3)))
    (type (record (field "order-id" string) (field "items" 2) (field "destination" 4)))
    (export "order" (type (eq 5)))
    (type (enum "EUR" "USD" "GBP" "JPY" "CAD" "AUD"))
    (export "currency" (type (eq 7)))
    (type (record (field "order-id" string) (field "subtotal" f64) (field "tax" f64) (field "total" f64) (field "currency" 8)))
    (export "quote" (type (eq 9)))
  ))
  (import "synth:test/types" (instance (;0;) (type 0)))
  (alias export 0 "order" (type (;1;)))
  (alias export 0 "quote" (type (;2;)))
  (type (;3;) (instance
    (alias outer 1 1 (type (;0;)))
    (export "order" (type (eq 0)))
    (alias outer 1 2 (type (;2;)))
    (export "quote" (type (eq 2)))
    (type (;4;) (func (param "order" 0) (result 2)))
    (export "create-order" (func (type 4)))
  ))
  (import "{target}" (instance (;1;) (type 3)))
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

// Multi-function sync-primitives interface — exercises the new emit
// path's multi-handler dispatch (separate per-func wrapper + per-func
// name in the data segment + per-func handler import).
#[test]
fn test_adapter_sync_multi_func_primitives() {
    let mut arena = TypeArena::default();
    let s32 = arena.intern_val(ValueType::S32);
    let u32_ = arena.intern_val(ValueType::U32);
    let iface = make_iface(vec![
        ("add", sig(false, &["a", "b"], vec![s32, s32], vec![s32])),
        ("count", sig(false, &[], vec![], vec![u32_])),
        ("noop", sig(false, &["x"], vec![s32], vec![])),
    ]);
    let bytes = gen_adapter(
        "test:pkg/multi@1.0.0",
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

// ── Tier 1: real-world multi-function + named-types shape ──────────
//
// Regression fixture for the nebula demo composition
// (~/git/research/wasm/nebula/demo). That composition's
// `nebula:service/orders` interface exports three functions — two with
// record params/results, one void — and all the compound types live
// in a sibling `nebula:core/types` interface that both sides import.
// A user reported that splicer's generated adapter dropped two of the
// three functions and failed component validation with
// "instance not valid to be used as export". This test pins the
// shape so a future regression fails loud.

/// Build the nebula `orders`-interface shape: three freestanding
/// functions plus named type exports for `order`, `quote`, `item`,
/// `country`, `currency` — the way a real WIT with
/// `use nebula:core/types.{order, quote}` flows into cviz's
/// `InstanceInterface`.
fn build_nebula_orders_iface(arena: &mut TypeArena) -> InterfaceType {
    let string_id = arena.intern_val(ValueType::String);
    let u32_id = arena.intern_val(ValueType::U32);
    let f64_id = arena.intern_val(ValueType::F64);

    let item = arena.intern_val(ValueType::Record(vec![
        ("sku".into(), string_id),
        ("quantity".into(), u32_id),
        ("unit-price".into(), f64_id),
    ]));
    let country = arena.intern_val(ValueType::Enum(vec![
        "BE".into(),
        "US".into(),
        "UK".into(),
        "JP".into(),
        "CA".into(),
        "AU".into(),
    ]));
    let currency = arena.intern_val(ValueType::Enum(vec![
        "EUR".into(),
        "USD".into(),
        "GBP".into(),
        "JPY".into(),
        "CAD".into(),
        "AUD".into(),
    ]));
    let list_item = arena.intern_val(ValueType::List(item));
    let order = arena.intern_val(ValueType::Record(vec![
        ("order-id".into(), string_id),
        ("items".into(), list_item),
        ("destination".into(), country),
    ]));
    let quote = arena.intern_val(ValueType::Record(vec![
        ("order-id".into(), string_id),
        ("subtotal".into(), f64_id),
        ("tax".into(), f64_id),
        ("total".into(), f64_id),
        ("currency".into(), currency),
    ]));
    let opt_order = arena.intern_val(ValueType::Option(order));

    let create_order = sig(false, &["order"], vec![order], vec![quote]);
    let read_order = sig(false, &["order-id"], vec![string_id], vec![opt_order]);
    let delete_order = sig(false, &["order-id"], vec![string_id], vec![]);

    InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([
            ("create-order".to_string(), create_order),
            ("read-order".to_string(), read_order),
            ("delete-order".to_string(), delete_order),
        ]),
        type_exports: BTreeMap::from([
            ("item".to_string(), item),
            ("country".to_string(), country),
            ("currency".to_string(), currency),
            ("order".to_string(), order),
            ("quote".to_string(), quote),
        ]),
    })
}

#[test]
fn test_adapter_nebula_orders_shape() {
    let mut arena = TypeArena::default();
    let iface = build_nebula_orders_iface(&mut arena);
    let bytes = gen_adapter(
        "nebula:service/orders",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);

    // Pin that the generated component actually re-exports all three
    // interface functions — the original bug report had the adapter
    // exporting only `create-order`. Scan the raw bytes for each
    // name: component exports carry function names as UTF-8 in the
    // component-level type/export sections.
    for name in ["create-order", "read-order", "delete-order"] {
        let needle = name.as_bytes();
        let found = bytes.windows(needle.len()).any(|w| w == needle);
        assert!(
            found,
            "generated adapter should reference `{name}` but the binary \
             doesn't contain it — splicer may have dropped interface \
             functions other than the first one"
        );
    }
}

/// End-to-end regression for the nebula `orders` composition bug.
/// Parses a nebula-shaped composition WAT via cviz, extracts the
/// `nebula:service/orders` import's `InterfaceType`, and feeds it
/// into `generate_tier1_adapter`. The test fails end-to-end if any
/// layer in the stack regresses:
///
/// - wirm's `concretize_from_resolved_to_val` stops following
///   `Alias::Outer` / `Alias::InstanceExport` chains → cviz returns
///   empty `type_exports` → splicer redeclares types locally →
///   `validate_component` trips on
///   `instance not valid to be used as export`.
/// - cviz's `concrete_to_interface_type` drops types on the floor.
/// - splicer's `emit_imports_consumer_split` stops aliasing types
///   off the handler instance when cviz DOES supply them.
///
/// The WAT shape mirrors wit-component's output for a
/// `use other:pkg/types.{X}` pattern: an inner component imports
/// both a sibling types instance (`nebula:core/types`) and the
/// target service (`nebula:service/orders`) whose instance type
/// `alias outer`s into the types-instance's exports.
#[test]
fn test_adapter_cross_interface_value_types() {
    let wat = r#"(component
        (component $inner
            (import "nebula:core/types" (instance $types
                (type (record (field "sku" string) (field "quantity" u32) (field "unit-price" f64)))
                (export "item" (type (eq 0)))
                (type (list 1))
                (type (enum "BE" "US" "UK" "JP" "CA" "AU"))
                (export "country" (type (eq 3)))
                (type (record (field "order-id" string) (field "items" 2) (field "destination" 4)))
                (export "order" (type (eq 5)))
                (type (enum "EUR" "USD" "GBP" "JPY" "CAD" "AUD"))
                (export "currency" (type (eq 7)))
                (type (record (field "order-id" string) (field "subtotal" f64) (field "tax" f64) (field "total" f64) (field "currency" 8)))
                (export "quote" (type (eq 9)))
            ))
            (alias export $types "order" (type $order))
            (alias export $types "quote" (type $quote))
            (import "nebula:service/orders" (instance $svc
                (alias outer 1 $order (type (;0;)))
                (export "order" (type (eq 0)))
                (alias outer 1 $quote (type (;2;)))
                (export "quote" (type (eq 2)))
                (type (;4;) (func (param "order" 0) (result 2)))
                (export "create-order" (func (type 4)))
            ))
            (alias export $svc "create-order" (func $f))
            (instance $out (export "create-order" (func $f)))
            (export "nebula:service/orders" (instance $out))
        )
        (import "nebula:core/types" (instance $host-types
            (type (record (field "sku" string) (field "quantity" u32) (field "unit-price" f64)))
            (export "item" (type (eq 0)))
            (type (list 1))
            (type (enum "BE" "US" "UK" "JP" "CA" "AU"))
            (export "country" (type (eq 3)))
            (type (record (field "order-id" string) (field "items" 2) (field "destination" 4)))
            (export "order" (type (eq 5)))
            (type (enum "EUR" "USD" "GBP" "JPY" "CAD" "AUD"))
            (export "currency" (type (eq 7)))
            (type (record (field "order-id" string) (field "subtotal" f64) (field "tax" f64) (field "total" f64) (field "currency" 8)))
            (export "quote" (type (eq 9)))
        ))
        (alias export $host-types "order" (type $outer-order))
        (alias export $host-types "quote" (type $outer-quote))
        (import "nebula:service/orders" (instance $host-svc
            (alias outer 1 $outer-order (type (;0;)))
            (export "order" (type (eq 0)))
            (alias outer 1 $outer-quote (type (;2;)))
            (export "quote" (type (eq 2)))
            (type (;4;) (func (param "order" 0) (result 2)))
            (export "create-order" (func (type 4)))
        ))
        (instance $inst (instantiate $inner
            (with "nebula:core/types" (instance $host-types))
            (with "nebula:service/orders" (instance $host-svc))
        ))
        (alias export $inst "nebula:service/orders" (instance $out))
        (export "nebula:service/orders" (instance $out))
    )"#;

    let comp_bytes = wat::parse_str(wat).expect("composition WAT parses");
    let graph = parse_component(&comp_bytes).expect("parse_component succeeds");
    let svc_conn = graph
        .nodes
        .values()
        .flat_map(|n| n.imports.iter())
        .find(|c| c.interface_name == "nebula:service/orders")
        .expect("inner component should import nebula:service/orders");
    let iface = svc_conn
        .interface_type
        .as_ref()
        .expect("interface_type populated")
        .clone();

    // Sanity: the whole point of the wirm fix is that cviz now
    // populates these. Assert before running the adapter gen so a
    // regression in wirm/cviz gives a clearer error than the downstream
    // `instance not valid to be used as export`.
    if let InterfaceType::Instance(inst) = &iface {
        assert!(
            inst.type_exports.contains_key("order") && inst.type_exports.contains_key("quote"),
            "cviz should populate order+quote in type_exports; got {:?}",
            inst.type_exports.keys().collect::<Vec<_>>()
        );
    }

    let bytes = gen_adapter(
        "nebula:service/orders",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &graph.arena,
        SplitKind::ConsumerSiblingTypes,
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
// Blocked on https://github.com/bytecodealliance/wasm-tools/issues/2506
// — wit-parser panics when decoding a component that imports + re-exports
// a resource-bearing instance, which is exactly the provider-split shape
// for a resource-bearing target. Re-enable once upstream fixes it.
#[test]
#[ignore]
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
