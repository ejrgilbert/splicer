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
use wasmtime::component::Linker;

mod common;
use common::{
    add_builtin_config_stub, assert_call_attrs, capture_call, captured, drive_call_cycle,
    empty_span_context, expect_list, expect_option, expect_optional_string, expect_optional_u8,
    expect_record, expect_string, field, stub_returning, stub_void, Captures, Host,
};

const OTEL_LOGS: &str = "wasi:otel/logs@0.2.0-rc.2";
const OTEL_TRACING: &str = "wasi:otel/tracing@0.2.0-rc.2";

fn setup(linker: &mut Linker<Host<Captures>>) -> Result<()> {
    add_builtin_config_stub(linker)?;
    let mut logs = linker.instance(OTEL_LOGS)?;
    capture_call(&mut logs, "on-emit")?;

    // The builtin's WIT only calls `outer-span-context`, but the tracing
    // instance has other functions on it; provide trivial stubs so
    // instantiation doesn't fail on missing imports it never calls.
    let mut tracing = linker.instance(OTEL_TRACING)?;
    stub_returning(&mut tracing, "outer-span-context", empty_span_context())?;
    stub_void(&mut tracing, "on-start")?;
    stub_void(&mut tracing, "on-end")
}

#[test]
fn otel_bare_logs_emits_structured_record() -> Result<()> {
    let bytes = common::read_builtin("otel-bare-logs");
    let capture = drive_call_cycle::<Captures, _>(&bytes, setup)?;
    let cap = capture.lock().unwrap();
    let logs = captured(&cap, "on-emit");

    assert_eq!(logs.len(), 1, "exactly one on-emit call expected");

    let record = expect_record(&logs[0]);

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
    let attrs = expect_option(field(record, "attributes")).expect("attributes present");
    assert_call_attrs(expect_list(attrs));

    // Observed-timestamp present (we don't pin a value — clock state).
    assert!(
        expect_option(field(record, "observed-timestamp")).is_some(),
        "observed-timestamp is set"
    );

    // No host parent ⇒ trace-correlation fields unset.
    assert!(
        expect_option(field(record, "trace-id")).is_none(),
        "trace-id is none when no parent span"
    );
    assert!(
        expect_option(field(record, "span-id")).is_none(),
        "span-id is none when no parent span"
    );
    assert!(
        expect_option(field(record, "trace-flags")).is_none(),
        "trace-flags is none when no parent span"
    );

    // Instrumentation scope identifies the source builtin.
    let scope = expect_record(expect_option(field(record, "instrumentation-scope")).expect("scope"));
    assert_eq!(
        expect_string(field(scope, "name")),
        "splicer:otel-bare-logs",
        "scope name identifies the source"
    );

    Ok(())
}
