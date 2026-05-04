//! Behavioral smoke test for the `otel-bare-spans` builtin.
//!
//! Instantiates the embedded component in wasmtime with a fake
//! `wasi:otel/tracing` host that captures `on-start` / `on-end`
//! events, drives `splicer:tier1/before#on-call` and
//! `after#on-return` for a synthetic call-id, and asserts the
//! captured span pair has consistent ids, ordered timestamps, and
//! the expected `code.namespace` / `code.function` attributes.
//!
//! Requires `make build-builtins` to have populated
//! `assets/builtins/otel-bare-spans.wasm` (embedded below via
//! `include_bytes!`).

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use wasmtime::component::{Component, Linker, ResourceTable, Val};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

const OTEL_TRACING: &str = "wasi:otel/tracing@0.2.0-rc.2";
const SPLICER_BEFORE: &str = "splicer:tier1/before@0.2.0";
const SPLICER_AFTER: &str = "splicer:tier1/after@0.2.0";

const TARGET_IFACE: &str = "wasi:http/handler@0.3.0";
const TARGET_FN: &str = "handle";

#[derive(Default)]
struct Capture {
    starts: Vec<Val>,
    ends: Vec<Val>,
}

struct Host {
    wasi: WasiCtx,
    table: ResourceTable,
    capture: Arc<Mutex<Capture>>,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Empty `span-context` — all-zero ids, no flags, no state. Returned
/// by the fake `outer-span-context` so the component sees "no host
/// parent" and mints a fresh trace-id rather than inheriting.
fn empty_span_context() -> Val {
    Val::Record(vec![
        ("trace-id".into(), Val::String(String::new())),
        ("span-id".into(), Val::String(String::new())),
        ("trace-flags".into(), Val::Flags(vec![])),
        ("is-remote".into(), Val::Bool(false)),
        ("trace-state".into(), Val::List(vec![])),
    ])
}

fn add_otel_tracing_to_linker(linker: &mut Linker<Host>) -> Result<()> {
    let mut otel = linker.instance(OTEL_TRACING)?;

    otel.func_new("on-start", |store, _ty, params, _results| {
        store
            .data()
            .capture
            .lock()
            .unwrap()
            .starts
            .push(params[0].clone());
        Ok(())
    })?;

    otel.func_new("on-end", |store, _ty, params, _results| {
        store
            .data()
            .capture
            .lock()
            .unwrap()
            .ends
            .push(params[0].clone());
        Ok(())
    })?;

    otel.func_new("outer-span-context", |_store, _ty, _params, results| {
        results[0] = empty_span_context();
        Ok(())
    })?;

    Ok(())
}

fn call_id_val(iface: &str, func: &str) -> Val {
    Val::Record(vec![
        ("interface-name".into(), Val::String(iface.into())),
        ("function-name".into(), Val::String(func.into())),
    ])
}

#[test]
fn otel_bare_spans_emits_consistent_start_and_end() -> Result<()> {
    let bytes = include_bytes!("../assets/builtins/otel-bare-spans.wasm");

    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_binary(&engine, bytes)?;

    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    add_otel_tracing_to_linker(&mut linker)?;

    let capture = Arc::new(Mutex::new(Capture::default()));
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

    rt.block_on(async {
        let instance = linker.instantiate_async(&mut store, &component).await?;

        let before_idx = instance
            .get_export_index(&mut store, None, SPLICER_BEFORE)
            .context("missing before export")?;
        let on_call_idx = instance
            .get_export_index(&mut store, Some(&before_idx), "on-call")
            .context("missing on-call export")?;
        let on_call = instance
            .get_func(&mut store, on_call_idx)
            .context("on-call is not a func")?;

        let after_idx = instance
            .get_export_index(&mut store, None, SPLICER_AFTER)
            .context("missing after export")?;
        let on_return_idx = instance
            .get_export_index(&mut store, Some(&after_idx), "on-return")
            .context("missing on-return export")?;
        let on_return = instance
            .get_func(&mut store, on_return_idx)
            .context("on-return is not a func")?;

        let cid = call_id_val(TARGET_IFACE, TARGET_FN);

        let mut results: Vec<Val> = vec![];
        on_call
            .call_async(&mut store, &[cid.clone()], &mut results)
            .await?;

        let mut results: Vec<Val> = vec![];
        on_return
            .call_async(&mut store, &[cid], &mut results)
            .await?;

        Ok::<_, anyhow::Error>(())
    })?;

    let cap = capture.lock().unwrap();
    assert_eq!(cap.starts.len(), 1, "exactly one on-start call expected");
    assert_eq!(cap.ends.len(), 1, "exactly one on-end call expected");

    let start_ctx = expect_record(&cap.starts[0]);
    let end_span = expect_record(&cap.ends[0]);

    let started_trace = expect_string(field(start_ctx, "trace-id"));
    let started_span_id = expect_string(field(start_ctx, "span-id"));
    assert_eq!(
        started_trace.len(),
        32,
        "trace-id is 16 bytes hex (32 chars), got {started_trace:?}"
    );
    assert_eq!(
        started_span_id.len(),
        16,
        "span-id is 8 bytes hex (16 chars), got {started_span_id:?}"
    );
    assert!(
        started_trace.chars().all(|c| c.is_ascii_hexdigit()),
        "trace-id is hex"
    );
    assert!(
        started_span_id.chars().all(|c| c.is_ascii_hexdigit()),
        "span-id is hex"
    );

    let end_ctx = expect_record(field(end_span, "span-context"));
    assert_eq!(
        expect_string(field(end_ctx, "trace-id")),
        started_trace,
        "trace-id matches between start and end"
    );
    assert_eq!(
        expect_string(field(end_ctx, "span-id")),
        started_span_id,
        "span-id matches between start and end"
    );

    let start_dt = expect_record(field(end_span, "start-time"));
    let end_dt = expect_record(field(end_span, "end-time"));
    let s = (
        expect_u64(field(start_dt, "seconds")),
        expect_u32(field(start_dt, "nanoseconds")),
    );
    let e = (
        expect_u64(field(end_dt, "seconds")),
        expect_u32(field(end_dt, "nanoseconds")),
    );
    assert!(e >= s, "end_time {e:?} >= start_time {s:?}");

    let attrs = expect_list(field(end_span, "attributes"));
    let attr_map: std::collections::HashMap<String, String> = attrs
        .iter()
        .map(|kv| {
            let r = expect_record(kv);
            (
                expect_string(field(r, "key")).to_string(),
                expect_string(field(r, "value")).to_string(),
            )
        })
        .collect();
    assert_eq!(
        attr_map.get("code.namespace").map(String::as_str),
        Some(format!("\"{TARGET_IFACE}\"").as_str()),
        "code.namespace JSON-encoded; got {attr_map:?}"
    );
    assert_eq!(
        attr_map.get("code.function").map(String::as_str),
        Some(format!("\"{TARGET_FN}\"").as_str()),
        "code.function JSON-encoded; got {attr_map:?}"
    );

    Ok(())
}

fn field<'a>(record: &'a [(String, Val)], name: &str) -> &'a Val {
    record
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("field {name:?} not found in record {record:?}"))
}

fn expect_record(v: &Val) -> &[(String, Val)] {
    if let Val::Record(fields) = v {
        fields
    } else {
        panic!("expected record, got {v:?}")
    }
}
fn expect_string(v: &Val) -> &str {
    if let Val::String(s) = v {
        s
    } else {
        panic!("expected string, got {v:?}")
    }
}
fn expect_u64(v: &Val) -> u64 {
    if let Val::U64(n) = v {
        *n
    } else {
        panic!("expected u64, got {v:?}")
    }
}
fn expect_u32(v: &Val) -> u32 {
    if let Val::U32(n) = v {
        *n
    } else {
        panic!("expected u32, got {v:?}")
    }
}
fn expect_list(v: &Val) -> &[Val] {
    if let Val::List(items) = v {
        items
    } else {
        panic!("expected list, got {v:?}")
    }
}
