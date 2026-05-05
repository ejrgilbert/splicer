//! Builtin: emit a structured `wasi:otel/logs` record on every
//! wrapped call. Each record carries the call's (interface,
//! function) as attributes, severity `INFO`, event-name
//! `call.invoked`, an `observed-timestamp`, and trace-correlation
//! fields populated from the host's `outer-span-context` when one
//! is active. No payload-derived content.
//!
//! Audience: shops with a structured-logging backend (Loki, ELK,
//! Splunk) but no tracing pipeline — they want call-event records
//! flowing through the format their existing tooling consumes,
//! independent of whether they also collect spans.

mod bindings {
    wit_bindgen::generate!({
        world: "otel-logs-mdl",
        async: true,
        generate_all,
    });
}

use bindings::exports::splicer::tier1::after::Guest as AfterGuest;
use bindings::splicer::common::types::CallId;
use bindings::wasi::clocks::wall_clock::now;
use bindings::wasi::otel::logs::{on_emit, LogRecord};
use bindings::wasi::otel::tracing::outer_span_context;
use bindings::wasi::otel::types::{InstrumentationScope, KeyValue};

/// OTel severity-number for `INFO` per the spec's severity-number
/// table (https://opentelemetry.io/docs/specs/otel/logs/data-model/#field-severitynumber).
const SEVERITY_INFO: u8 = 9;

/// Event name applied to every emitted record. Lets consumers filter
/// "splicer call events" without parsing the body.
const EVENT_NAME: &str = "call.invoked";

/// OTel encodes "no parent" as the all-zero id. Treat empty strings
/// the same way for resilience against hosts that report them.
fn empty_id(s: &str) -> bool {
    s.is_empty() || s.bytes().all(|b| b == b'0')
}

/// `wasi:otel/types.value` is a JSON-encoded `AnyValue`. Wrap a plain
/// string as a JSON string literal.
fn encode_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn kv(k: &str, v: &str) -> KeyValue {
    KeyValue {
        key: k.to_string(),
        value: encode_json_string(v),
    }
}

fn scope() -> InstrumentationScope {
    InstrumentationScope {
        name: "splicer:otel-logs".into(),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        schema_url: None,
        attributes: vec![],
    }
}

pub struct OtelLogs;

impl AfterGuest for OtelLogs {
    async fn on_return(call: CallId) {
        let parent = outer_span_context().await;
        let (trace_id, span_id, trace_flags) = if empty_id(&parent.trace_id) {
            (None, None, None)
        } else {
            (
                Some(parent.trace_id),
                Some(parent.span_id),
                Some(parent.trace_flags),
            )
        };

        let body = format!("{}::{}", call.interface_name, call.function_name);
        let record = LogRecord {
            timestamp: None,
            observed_timestamp: Some(now().await),
            severity_text: Some("INFO".into()),
            severity_number: Some(SEVERITY_INFO),
            body: Some(encode_json_string(&body)),
            attributes: Some(vec![
                kv("code.namespace", &call.interface_name),
                kv("code.function", &call.function_name),
            ]),
            event_name: Some(EVENT_NAME.into()),
            resource: None,
            instrumentation_scope: Some(scope()),
            trace_id,
            span_id,
            trace_flags,
        };
        on_emit(record).await;
    }
}

bindings::export!(OtelLogs with_types_in bindings);
