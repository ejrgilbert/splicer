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
//! `assets/builtins/otel-bare-spans.wasm`, or `SPLICER_BUILTINS_DIR`
//! pointing at a directory containing it.

use anyhow::Result;
use wasmtime::component::Val;

mod common;
use common::{
    assert_call_attrs, drive_call_cycle, empty_span_context, expect_list, expect_record,
    expect_string, expect_u32, expect_u64, field, Host,
};

const OTEL_TRACING: &str = "wasi:otel/tracing@0.2.0-rc.2";

#[derive(Default)]
struct Capture {
    starts: Vec<Val>,
    ends: Vec<Val>,
}

fn add_otel_tracing_to_linker(
    linker: &mut wasmtime::component::Linker<Host<Capture>>,
) -> Result<()> {
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

#[test]
fn otel_bare_spans_emits_consistent_start_and_end() -> Result<()> {
    let bytes = common::read_builtin("otel-bare-spans");
    let capture = drive_call_cycle::<Capture, _>(&bytes, add_otel_tracing_to_linker)?;
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

    assert_call_attrs(expect_list(field(end_span, "attributes")));

    Ok(())
}
