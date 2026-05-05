//! Behavioral smoke test for the `otel-bare-logs` builtin.
//!
//! Instantiates the embedded component in wasmtime with a fake
//! `wasi:otel/logs` host that captures `on-emit` calls and a fake
//! `wasi:otel/tracing.outer-span-context` returning an empty parent
//! (no active host span). Drives `splicer:tier1/after#on-return` for
//! a synthetic call-id and asserts the captured log record carries
//! the expected severity, event-name, body, attributes, and that
//! trace-correlation fields are absent (no parent ⇒ `none`).
//!
//! Requires `make build-builtins` to have populated
//! `assets/builtins/otel-bare-logs.wasm`, or `SPLICER_BUILTINS_DIR`
//! pointing at a directory containing it.

use anyhow::Result;
use wasmtime::component::Val;

mod common;
use common::{
    assert_call_attrs, drive_call_cycle, expect_list, expect_record, expect_string, field, Host,
};

const OTEL_LOGS: &str = "wasi:otel/logs@0.2.0-rc.2";
const OTEL_TRACING: &str = "wasi:otel/tracing@0.2.0-rc.2";

#[derive(Default)]
struct Capture {
    logs: Vec<Val>,
}

/// Empty `span-context` — all-zero ids, no flags, no state. Returned
/// by the fake `outer-span-context` so the builtin sees "no host
/// parent" and leaves trace-correlation fields unset on the emitted
/// log record.
fn empty_span_context() -> Val {
    Val::Record(vec![
        ("trace-id".into(), Val::String(String::new())),
        ("span-id".into(), Val::String(String::new())),
        ("trace-flags".into(), Val::Flags(vec![])),
        ("is-remote".into(), Val::Bool(false)),
        ("trace-state".into(), Val::List(vec![])),
    ])
}

fn add_otel_logs_to_linker(linker: &mut wasmtime::component::Linker<Host<Capture>>) -> Result<()> {
    let mut logs = linker.instance(OTEL_LOGS)?;
    logs.func_new("on-emit", |store, _ty, params, _results| {
        store
            .data()
            .capture
            .lock()
            .unwrap()
            .logs
            .push(params[0].clone());
        Ok(())
    })?;

    let mut tracing = linker.instance(OTEL_TRACING)?;
    tracing.func_new("outer-span-context", |_store, _ty, _params, results| {
        results[0] = empty_span_context();
        Ok(())
    })?;
    // The builtin's WIT only calls `outer-span-context`, but the
    // tracing instance has other functions on it; provide trivial
    // stubs so instantiation doesn't fail on missing imports the
    // builtin never calls.
    tracing.func_new("on-start", |_store, _ty, _params, _results| Ok(()))?;
    tracing.func_new("on-end", |_store, _ty, _params, _results| Ok(()))?;

    Ok(())
}

#[test]
fn otel_bare_logs_emits_structured_record() -> Result<()> {
    let bytes = common::read_builtin("otel-bare-logs");
    let capture = drive_call_cycle::<Capture, _>(&bytes, add_otel_logs_to_linker)?;
    let cap = capture.lock().unwrap();

    assert_eq!(cap.logs.len(), 1, "exactly one on-emit call expected");

    let record = expect_record(&cap.logs[0]);

    // Severity: INFO / 9.
    assert_eq!(
        expect_optional_string(field(record, "severity-text")),
        Some("INFO"),
        "severity-text is INFO"
    );
    assert_eq!(
        expect_optional_u8(field(record, "severity-number")),
        Some(9),
        "severity-number is 9 (INFO)"
    );

    // Event name + body.
    assert_eq!(
        expect_optional_string(field(record, "event-name")),
        Some("call.invoked"),
        "event-name identifies the event class"
    );
    let expected_body = format!("\"{}::{}\"", common::TARGET_IFACE, common::TARGET_FN);
    assert_eq!(
        expect_optional_string(field(record, "body")),
        Some(expected_body.as_str()),
        "body is JSON-encoded interface::function"
    );

    // Attributes: code.namespace / code.function, JSON-encoded.
    let attrs_opt = expect_option_field(field(record, "attributes"));
    let attrs = expect_list(attrs_opt.expect("attributes present"));
    assert_call_attrs(attrs);

    // Observed-timestamp present (we don't pin a value — clock state).
    assert!(
        expect_option_field(field(record, "observed-timestamp")).is_some(),
        "observed-timestamp is set"
    );

    // No host parent ⇒ trace-correlation fields unset.
    assert!(
        expect_option_field(field(record, "trace-id")).is_none(),
        "trace-id is none when no parent span"
    );
    assert!(
        expect_option_field(field(record, "span-id")).is_none(),
        "span-id is none when no parent span"
    );
    assert!(
        expect_option_field(field(record, "trace-flags")).is_none(),
        "trace-flags is none when no parent span"
    );

    // Instrumentation scope identifies the source builtin.
    let scope_opt = expect_option_field(field(record, "instrumentation-scope"));
    let scope = expect_record(scope_opt.expect("instrumentation-scope present"));
    assert_eq!(
        expect_string(field(scope, "name")),
        "splicer:otel-bare-logs",
        "scope name identifies the source"
    );

    Ok(())
}

fn expect_option_field(v: &Val) -> Option<&Val> {
    if let Val::Option(inner) = v {
        inner.as_deref()
    } else {
        panic!("expected option, got {v:?}")
    }
}

fn expect_optional_string(v: &Val) -> Option<&str> {
    expect_option_field(v).map(expect_string)
}

fn expect_optional_u8(v: &Val) -> Option<u8> {
    expect_option_field(v).map(|inner| {
        if let Val::U8(n) = inner {
            *n
        } else {
            panic!("expected u8, got {inner:?}")
        }
    })
}
