//! Shared scaffolding for integration tests that drive a tier-1
//! builtin in wasmtime against a synthetic call-id.
//!
//! Each builtin smoke-test (`builtins_otel_bare_spans.rs`,
//! `builtins_otel_bare_metrics.rs`, …) supplies its own `Capture` type and
//! linker-side fake host implementation; everything else (engine
//! config, instantiation, `on-call` → `on-return` drive cycle, `Val`
//! extractors) lives here so the per-test files stay focused on the
//! assertions.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::runtime::Runtime;
use wasmtime::component::{Component, Func, Instance, Linker, LinkerInstance, ResourceTable, Val};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

pub const SPLICER_BEFORE: &str = "splicer:tier1/before@0.5.0";
pub const SPLICER_AFTER: &str = "splicer:tier1/after@0.5.0";
pub const SPLICER_TIER2_BEFORE: &str = "splicer:tier2/before@0.3.0";
pub const SPLICER_TIER2_AFTER: &str = "splicer:tier2/after@0.3.0";
pub const SPLICER_BUILTIN_CONFIG_GET: &str = "splicer:builtin-config/get@0.1.0";

pub const TARGET_IFACE: &str = "wasi:http/handler@0.3.0";
pub const TARGET_FN: &str = "handle";

/// Generic capture: host imports push their first argument into a
/// per-function bucket. One shared shape covers every builtin's fake
/// host (metrics `export`, tracing `on-start`/`on-end`, logs `on-emit`,
/// …); tests read a bucket back with [`captured`]. Keyed by the imported
/// function's name.
pub type Captures = HashMap<String, Vec<Val>>;

pub struct Host<C: Send + 'static> {
    pub wasi: WasiCtx,
    pub table: ResourceTable,
    pub capture: Arc<Mutex<C>>,
}

impl<C: Send + 'static> WasiView for Host<C> {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

pub fn call_id_val(iface: &str, func: &str) -> Val {
    Val::Record(vec![
        ("interface-name".into(), Val::String(iface.into())),
        ("function-name".into(), Val::String(func.into())),
        ("id".into(), Val::U64(0)),
    ])
}

/// Empty `span-context` — all-zero ids, no flags, no state. Returned
/// by fake `outer-span-context` host fns so the builtin sees "no host
/// parent" and either mints a fresh trace-id (tracing) or leaves
/// trace-correlation fields unset on emitted records (logs).
pub fn empty_span_context() -> Val {
    Val::Record(vec![
        ("trace-id".into(), Val::String(String::new())),
        ("span-id".into(), Val::String(String::new())),
        ("trace-flags".into(), Val::Flags(vec![])),
        ("is-remote".into(), Val::Bool(false)),
        ("trace-state".into(), Val::List(vec![])),
    ])
}

/// Build the engine, instantiate the embedded component with
/// `setup`-installed fake host imports, and return the runtime plus the
/// store, instance, and capture handle. Both `drive_*` helpers share
/// this; they differ only in which exports they invoke.
fn setup_instance<C, F>(
    bytes: &[u8],
    setup: F,
) -> Result<(Runtime, Store<Host<C>>, Instance, Arc<Mutex<C>>)>
where
    C: Default + Send + 'static,
    F: FnOnce(&mut Linker<Host<C>>) -> Result<()>,
{
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_binary(&engine, bytes)?;

    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    setup(&mut linker)?;

    let capture = Arc::new(Mutex::new(C::default()));
    let stdout = MemoryOutputPipe::new(64 * 1024);
    let host = Host {
        wasi: WasiCtxBuilder::new().stdout(stdout).build(),
        table: ResourceTable::new(),
        capture: capture.clone(),
    };
    let mut store = Store::new(&engine, host);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let instance = rt.block_on(linker.instantiate_async(&mut store, &component))?;
    Ok((rt, store, instance, capture))
}

/// Resolve an exported hook `iface#func` on the instance, if present.
fn export_func<C: Send + 'static>(
    store: &mut Store<Host<C>>,
    instance: &Instance,
    iface: &str,
    func: &str,
) -> Option<Func> {
    let iface_idx = instance.get_export_index(&mut *store, None, iface)?;
    let func_idx = instance.get_export_index(&mut *store, Some(&iface_idx), func)?;
    instance.get_func(&mut *store, func_idx)
}

/// Drive a single tier-1 `on-call` → `on-return` cycle against a
/// synthetic call-id targeting `TARGET_IFACE` / `TARGET_FN`.
///
/// `setup` installs the fake host imports the builtin needs (see the
/// `capture_*` / `stub_*` helpers). Tier-1 builtins export any non-empty
/// subset of {before, after, gate}; whichever `on-call`/`on-return` this
/// one exports is driven. Returns the capture handle for inspection.
pub fn drive_call_cycle<C, F>(bytes: &[u8], setup: F) -> Result<Arc<Mutex<C>>>
where
    C: Default + Send + 'static,
    F: FnOnce(&mut Linker<Host<C>>) -> Result<()>,
{
    let (rt, mut store, instance, capture) = setup_instance(bytes, setup)?;
    rt.block_on(async {
        let cid = call_id_val(TARGET_IFACE, TARGET_FN);
        if let Some(f) = export_func(&mut store, &instance, SPLICER_BEFORE, "on-call") {
            f.call_async(&mut store, std::slice::from_ref(&cid), &mut [])
                .await?;
        }
        if let Some(f) = export_func(&mut store, &instance, SPLICER_AFTER, "on-return") {
            f.call_async(&mut store, &[cid], &mut []).await?;
        }
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(capture)
}

/// Drive a single tier-2 `on-call(call-id, args)` → `on-return(call-id,
/// result)` cycle against a synthetic call-id. `args` are the lifted
/// argument fields; `result` is the lifted return `field-tree` (or `None`
/// for a void return). Panics if the builtin doesn't export both hooks.
pub fn drive_tier2_cycle<C, F>(
    bytes: &[u8],
    args: Vec<Val>,
    result: Option<Val>,
    setup: F,
) -> Result<Arc<Mutex<C>>>
where
    C: Default + Send + 'static,
    F: FnOnce(&mut Linker<Host<C>>) -> Result<()>,
{
    let (rt, mut store, instance, capture) = setup_instance(bytes, setup)?;
    rt.block_on(async {
        let cid = call_id_val(TARGET_IFACE, TARGET_FN);
        let on_call = export_func(&mut store, &instance, SPLICER_TIER2_BEFORE, "on-call")
            .expect("tier2 before#on-call export");
        let on_return = export_func(&mut store, &instance, SPLICER_TIER2_AFTER, "on-return")
            .expect("tier2 after#on-return export");
        on_call
            .call_async(&mut store, &[cid.clone(), Val::List(args)], &mut [])
            .await?;
        on_return
            .call_async(&mut store, &[cid, Val::Option(result.map(Box::new))], &mut [])
            .await?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(capture)
}

/// Register a no-op stub for `splicer:builtin-config/get`: every key
/// returns `none`, so the builtin falls back to its hardcoded defaults
/// across the test cycle. Tests that want non-default config should
/// register their own `get` function instead.
pub fn add_builtin_config_stub<C: Send + 'static>(linker: &mut Linker<Host<C>>) -> Result<()> {
    let mut iface = linker.instance(SPLICER_BUILTIN_CONFIG_GET)?;
    iface.func_new("get", |_store, _ty, _params, results| {
        results[0] = Val::Option(None);
        Ok(())
    })?;
    Ok(())
}

/// Install a fake host `func` that records its first argument into the
/// `Captures` bucket named `func` and returns nothing. For void-returning
/// signal sinks (`on-start`, `on-end`, `on-emit`).
pub fn capture_call(inst: &mut LinkerInstance<'_, Host<Captures>>, func: &str) -> Result<()> {
    let name = func.to_string();
    inst.func_new(func, move |store, _ty, params, _results| {
        push_capture(store.data(), &name, &params[0]);
        Ok(())
    })?;
    Ok(())
}

/// Like [`capture_call`] but for a `func` returning `result<_, error>`
/// (the metrics `export` sink): records the arg, returns `ok`.
pub fn capture_export(inst: &mut LinkerInstance<'_, Host<Captures>>, func: &str) -> Result<()> {
    let name = func.to_string();
    inst.func_new(func, move |store, _ty, params, results| {
        push_capture(store.data(), &name, &params[0]);
        results[0] = Val::Result(Ok(None));
        Ok(())
    })?;
    Ok(())
}

/// Install a fake host `func` that ignores its args and returns a fixed
/// value (e.g. `outer-span-context` → an empty parent context).
pub fn stub_returning(
    inst: &mut LinkerInstance<'_, Host<Captures>>,
    func: &str,
    val: Val,
) -> Result<()> {
    inst.func_new(func, move |_store, _ty, _params, results| {
        results[0] = val.clone();
        Ok(())
    })?;
    Ok(())
}

/// Install a no-op void `func` — a stub for imports the builtin never
/// calls but that must exist for instantiation to succeed.
pub fn stub_void(inst: &mut LinkerInstance<'_, Host<Captures>>, func: &str) -> Result<()> {
    inst.func_new(func, |_store, _ty, _params, _results| Ok(()))?;
    Ok(())
}

fn push_capture(host: &Host<Captures>, func: &str, arg: &Val) {
    host.capture
        .lock()
        .unwrap()
        .entry(func.to_string())
        .or_default()
        .push(arg.clone());
}

/// The values captured for host function `func` (empty if never called).
pub fn captured<'a>(cap: &'a Captures, func: &str) -> &'a [Val] {
    cap.get(func).map(Vec::as_slice).unwrap_or_default()
}

/// A `wasi:otel/types.key-value` list as a `key -> value` map. Values are
/// the raw JSON-encoded `AnyValue` strings (e.g. a plain string arrives as
/// `"\"...\""`); compare with [`assert_json_attr`].
pub fn kv_map(attrs: &[Val]) -> HashMap<String, String> {
    attrs
        .iter()
        .map(|kv| {
            let r = expect_record(kv);
            (
                expect_string(field(r, "key")).to_string(),
                expect_string(field(r, "value")).to_string(),
            )
        })
        .collect()
}

/// Assert `attrs[key]` equals `expected` once JSON-string-encoded (the
/// `AnyValue` wire form the builtins emit for plain-string attributes).
pub fn assert_json_attr(attrs: &HashMap<String, String>, key: &str, expected: &str) {
    assert_eq!(
        attrs.get(key).map(String::as_str),
        Some(format!("\"{expected}\"").as_str()),
        "{key} JSON-encoded; got {attrs:?}"
    );
}

/// Assert an attribute list carries `code.namespace` / `code.function`
/// entries matching `TARGET_IFACE` / `TARGET_FN`.
pub fn assert_call_attrs(attrs: &[Val]) {
    let m = kv_map(attrs);
    assert_json_attr(&m, "code.namespace", TARGET_IFACE);
    assert_json_attr(&m, "code.function", TARGET_FN);
}

// ─── Val extractors ────────────────────────────────────────────────

pub fn field<'a>(record: &'a [(String, Val)], name: &str) -> &'a Val {
    record
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("field {name:?} not found in record {record:?}"))
}

pub fn expect_record(v: &Val) -> &[(String, Val)] {
    if let Val::Record(fields) = v {
        fields
    } else {
        panic!("expected record, got {v:?}")
    }
}
pub fn expect_string(v: &Val) -> &str {
    if let Val::String(s) = v {
        s
    } else {
        panic!("expected string, got {v:?}")
    }
}
pub fn expect_u64(v: &Val) -> u64 {
    if let Val::U64(n) = v {
        *n
    } else {
        panic!("expected u64, got {v:?}")
    }
}
pub fn expect_u32(v: &Val) -> u32 {
    if let Val::U32(n) = v {
        *n
    } else {
        panic!("expected u32, got {v:?}")
    }
}
pub fn expect_bool(v: &Val) -> bool {
    if let Val::Bool(b) = v {
        *b
    } else {
        panic!("expected bool, got {v:?}")
    }
}
pub fn expect_list(v: &Val) -> &[Val] {
    if let Val::List(items) = v {
        items
    } else {
        panic!("expected list, got {v:?}")
    }
}
pub fn expect_variant(v: &Val) -> (&str, Option<&Val>) {
    if let Val::Variant(case, payload) = v {
        (case.as_str(), payload.as_deref())
    } else {
        panic!("expected variant, got {v:?}")
    }
}
pub fn expect_enum(v: &Val) -> &str {
    if let Val::Enum(case) = v {
        case.as_str()
    } else {
        panic!("expected enum, got {v:?}")
    }
}

pub fn expect_option(v: &Val) -> Option<&Val> {
    if let Val::Option(inner) = v {
        inner.as_deref()
    } else {
        panic!("expected option, got {v:?}")
    }
}
pub fn expect_optional_string(v: &Val) -> Option<&str> {
    expect_option(v).map(expect_string)
}
pub fn expect_optional_u8(v: &Val) -> Option<u8> {
    expect_option(v).map(|inner| {
        if let Val::U8(n) = inner {
            *n
        } else {
            panic!("expected u8, got {inner:?}")
        }
    })
}

// ─── OTel metric navigation ─────────────────────────────────────────

/// From a captured `resource-metrics` export, assert its single scope's
/// name is `scope_name` and return its metrics as a `name -> metric` map.
pub fn otel_metrics_by_name(resource_metrics: &Val, scope_name: &str) -> HashMap<String, Val> {
    let rm = expect_record(resource_metrics);
    let scope_metrics = expect_list(field(rm, "scope-metrics"));
    assert_eq!(scope_metrics.len(), 1, "exactly one scope-metrics entry");
    let sm = expect_record(&scope_metrics[0]);
    let scope = expect_record(field(sm, "scope"));
    assert_eq!(
        expect_string(field(scope, "name")),
        scope_name,
        "scope name identifies the source"
    );
    expect_list(field(sm, "metrics"))
        .iter()
        .map(|m| (expect_string(field(expect_record(m), "name")).to_string(), m.clone()))
        .collect()
}

pub fn metric<'a>(metrics: &'a HashMap<String, Val>, name: &str) -> &'a Val {
    metrics
        .get(name)
        .unwrap_or_else(|| panic!("metric {name:?} present; have {:?}", metrics.keys()))
}

/// The `u64-sum` aggregation record of a metric (`data-points`,
/// `is-monotonic`, `temporality`, …).
pub fn sum_record(metric: &Val) -> &[(String, Val)] {
    let (case, payload) = expect_variant(field(expect_record(metric), "data"));
    assert_eq!(case, "u64-sum", "expected u64-sum data, got {case}");
    expect_record(payload.expect("u64-sum payload"))
}

/// The `f64-histogram` aggregation record of a metric.
pub fn histogram_record(metric: &Val) -> &[(String, Val)] {
    let (case, payload) = expect_variant(field(expect_record(metric), "data"));
    assert_eq!(case, "f64-histogram", "expected f64-histogram data, got {case}");
    expect_record(payload.expect("f64-histogram payload"))
}

/// The `f64-exponential-histogram` aggregation record of a metric.
pub fn exp_histogram_record(metric: &Val) -> &[(String, Val)] {
    let (case, payload) = expect_variant(field(expect_record(metric), "data"));
    assert_eq!(
        case, "f64-exponential-histogram",
        "expected f64-exponential-histogram data, got {case}"
    );
    expect_record(payload.expect("f64-exponential-histogram payload"))
}

/// First data point of an aggregation record (from [`sum_record`] /
/// [`histogram_record`]).
pub fn first_data_point(agg: &[(String, Val)]) -> &[(String, Val)] {
    let dps = expect_list(field(agg, "data-points"));
    assert!(!dps.is_empty(), "aggregation has at least one data point");
    expect_record(&dps[0])
}

/// Unwrap a `metric-number` variant expected to be the `f64` case.
pub fn f64_metric_number(v: &Val) -> f64 {
    let (case, payload) = expect_variant(v);
    assert_eq!(case, "f64", "expected f64 metric-number, got {case}");
    match payload {
        Some(Val::Float64(f)) => *f,
        other => panic!("expected f64 payload, got {other:?}"),
    }
}

/// Same as [`f64_metric_number`] but for `option<metric-number>`.
pub fn opt_f64_metric_number(v: &Val) -> Option<f64> {
    expect_option(v).map(f64_metric_number)
}

/// Unwrap a `metric-number` variant expected to be the `u64` case.
pub fn u64_metric_number(v: &Val) -> u64 {
    let (case, payload) = expect_variant(v);
    assert_eq!(case, "u64", "expected u64 metric-number, got {case}");
    match payload {
        Some(Val::U64(n)) => *n,
        other => panic!("expected u64 payload, got {other:?}"),
    }
}

/// Read a built builtin's bytes from disk. Honors `SPLICER_BUILTINS_DIR`
/// (the same env var splicer's runtime fetch reads); otherwise looks in
/// `<crate-root>/assets/builtins/`. Panics with a `make build-builtins`
/// hint if the file is missing — these tests instantiate real
/// components and have no useful behavior to fall back on.
pub fn read_builtin(name: &str) -> Vec<u8> {
    let dir = std::env::var_os("SPLICER_BUILTINS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/builtins"));
    let path = dir.join(format!("{name}.wasm"));
    std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "couldn't read {}: {e}\n\
             run `make build-builtins`, or set SPLICER_BUILTINS_DIR=<dir>",
            path.display()
        )
    })
}
