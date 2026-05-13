//! Builtin: emit a structured `wasi:otel/logs` record on every
//! wrapped call. Each record carries the call's (interface,
//! function) as attributes, a configurable severity (default
//! `INFO`), event-name `call.invoked`, an `observed-timestamp`,
//! and trace-correlation fields populated from the host's
//! `outer-span-context` when one is active. No payload-derived
//! content.
//!
//! Audience: shops with a structured-logging backend (Loki, ELK,
//! Splunk) but no tracing pipeline — they want call-event records
//! flowing through the format their existing tooling consumes,
//! independent of whether they also collect spans.
//!
//! Config keys are read once at first observation via
//! `splicer:builtin-config/get` and cached for the instance's
//! lifetime. Unrecognized values fall back to defaults silently
//! (tier-1 has no logging surface).

mod bindings {
    // Per-export async filter (NOT `async: true`). Every import is
    // sync-WIT and MUST lower as plain `canon lower` (no async); see
    // `docs/TODO/sync-wit-suspend-limit.md` and hello-tier1 for the
    // rationale (sync-WIT-rooted task cannot block on canon-async wait).
    wit_bindgen::generate!({
        world: "otel-bare-logs-mdl",
        async: [
            "export:splicer:tier1/after@0.3.0#on-return",
        ],
        generate_all,
    });
}

use std::sync::OnceLock;

use bindings::exports::splicer::tier1::after::Guest as AfterGuest;
use bindings::splicer::builtin_config::get::get as get_config;
use bindings::splicer::common::types::CallId;
use bindings::wasi::clocks::wall_clock::now;
use bindings::wasi::otel::logs::{on_emit, LogRecord};
use bindings::wasi::otel::tracing::outer_span_context;
use bindings::wasi::otel::types::{InstrumentationScope, KeyValue};

/// Event name applied to every emitted record. Lets consumers filter
/// "splicer call events" without parsing the body.
const EVENT_NAME: &str = "call.invoked";

/// OTel severity, parsed from config. The `text` field becomes the
/// record's `severity-text`; `number` is the spec's base number for
/// that level (per
/// https://opentelemetry.io/docs/specs/otel/logs/data-model/#field-severitynumber).
struct Severity {
    text: &'static str,
    number: u8,
}

/// OTel SeverityNumber base values, one per spec-defined level. The
/// spec allocates 4 numbers per level for finer-grained variants
/// (`INFO2`, `INFO3`, …); the substrate's string config keeps things
/// to the named base values only.
const SEVERITY_TRACE: Severity = Severity { text: "TRACE", number: 1 };
const SEVERITY_DEBUG: Severity = Severity { text: "DEBUG", number: 5 };
const SEVERITY_INFO: Severity = Severity { text: "INFO", number: 9 };
const SEVERITY_WARN: Severity = Severity { text: "WARN", number: 13 };
const SEVERITY_ERROR: Severity = Severity { text: "ERROR", number: 17 };
const SEVERITY_FATAL: Severity = Severity { text: "FATAL", number: 21 };

/// Parsed config, materialized once on first observation.
struct Config {
    severity: Severity,
}

fn config() -> &'static Config {
    static C: OnceLock<Config> = OnceLock::new();
    if let Some(c) = C.get() {
        return c;
    }
    let severity = match get_config("severity") {
        Some(s) => parse_severity(&s).unwrap_or(SEVERITY_INFO),
        None => SEVERITY_INFO,
    };
    C.get_or_init(|| Config { severity })
}

/// Case-insensitive match against the OTel spec-defined level names.
/// `None` on unknown input — caller falls back to the default.
fn parse_severity(s: &str) -> Option<Severity> {
    match s.trim().to_ascii_uppercase().as_str() {
        "TRACE" => Some(SEVERITY_TRACE),
        "DEBUG" => Some(SEVERITY_DEBUG),
        "INFO" => Some(SEVERITY_INFO),
        "WARN" | "WARNING" => Some(SEVERITY_WARN),
        "ERROR" => Some(SEVERITY_ERROR),
        "FATAL" => Some(SEVERITY_FATAL),
        _ => None,
    }
}

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
        name: "splicer:otel-bare-logs".into(),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        schema_url: None,
        attributes: vec![],
    }
}

pub struct OtelBareLogs;

impl AfterGuest for OtelBareLogs {
    async fn on_return(call: CallId) {
        let parent = outer_span_context();
        let (trace_id, span_id, trace_flags) = if empty_id(&parent.trace_id) {
            (None, None, None)
        } else {
            (
                Some(parent.trace_id),
                Some(parent.span_id),
                Some(parent.trace_flags),
            )
        };

        let cfg = config();
        let body = format!("{}::{}", call.interface_name, call.function_name);
        let record = LogRecord {
            timestamp: None,
            observed_timestamp: Some(now()),
            severity_text: Some(cfg.severity.text.into()),
            severity_number: Some(cfg.severity.number),
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
        on_emit(&record);
    }
}

bindings::export!(OtelBareLogs with_types_in bindings);
