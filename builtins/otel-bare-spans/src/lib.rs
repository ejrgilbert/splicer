//! Builtin: wrap every call in a "bare" `wasi:otel` span — timing
//! plus call-name only, no payload-derived attributes.
//!
//! `on-call` mints a span-context (inheriting the host's outer
//! trace-id when one is active), notifies the host via
//! `tracing::on-start`, and pushes a pending entry. `on-return` pops
//! the entry and emits `tracing::on-end` with the captured timestamps.

mod bindings {
    wit_bindgen::generate!({
        world: "otel-bare-spans-mdl",
        async: true,
        generate_all,
    });
}

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use bindings::exports::splicer::tier1::after::Guest as AfterGuest;
use bindings::exports::splicer::tier1::before::Guest as BeforeGuest;
use bindings::splicer::common::types::CallId;
use bindings::wasi::clocks::wall_clock::{now, Datetime};
use bindings::wasi::otel::tracing::{
    on_end, on_start, outer_span_context, SpanContext, SpanData, SpanKind, Status, TraceFlags,
};
use bindings::wasi::otel::types::{InstrumentationScope, KeyValue};
use bindings::wasi::random::random::get_random_bytes;

/// OTel-spec ID byte widths. Wire encoding is lowercase hex, so the
/// rendered string is 2× these (32 chars for trace-id, 16 for span-id).
const TRACE_ID_BYTE_LEN: u64 = 16;
const SPAN_ID_BYTE_LEN: u64 = 8;

/// In-flight span recorded at `on-call`, drained at `on-return`.
struct Pending {
    context: SpanContext,
    parent_span_id: String,
    start_time: Datetime,
}

/// Stack per `(interface, function)` so concurrent or recursive
/// invocations of the same name don't clobber each other's contexts.
///
/// TODO(call-id correlation): switch the key to `call.id` once the
/// per-invocation `u64` field on `splicer:common/types::call-id` lands
/// from `feature/tier2-adapter`. Today's name-only key still
/// mis-pairs concurrent invocations of the same function — `on-return`
/// can pop a sibling invocation's pending span. The stack just keeps
/// the failure mode bounded (LIFO swap) instead of unbounded growth.
fn pending() -> &'static Mutex<HashMap<(String, String), Vec<Pending>>> {
    static M: OnceLock<Mutex<HashMap<(String, String), Vec<Pending>>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(call: &CallId) -> (String, String) {
    (call.interface_name.clone(), call.function_name.clone())
}

/// OTel encodes "no parent" as the all-zero id. Treat empty strings
/// the same way for resilience against hosts that report them.
fn empty_id(s: &str) -> bool {
    s.is_empty() || s.bytes().all(|b| b == b'0')
}

/// Draw `byte_len` random bytes from `wasi:random` and render them
/// as a lowercase hex string. Used for minting fresh OTel trace-ids
/// and span-ids, which the spec defines as raw byte widths but the
/// wire / `wasi:otel` types carry as hex.
async fn random_hex(byte_len: u64) -> String {
    get_random_bytes(byte_len)
        .await
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
        name: "splicer:otel-bare-spans".into(),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        schema_url: None,
        attributes: vec![],
    }
}

pub struct OtelBareSpans;

impl BeforeGuest for OtelBareSpans {
    async fn on_call(call: CallId) {
        let parent = outer_span_context().await;
        let trace_id = if empty_id(&parent.trace_id) {
            random_hex(TRACE_ID_BYTE_LEN).await
        } else {
            parent.trace_id.clone()
        };
        let parent_span_id = if empty_id(&parent.span_id) {
            String::new()
        } else {
            parent.span_id.clone()
        };
        let context = SpanContext {
            trace_id,
            span_id: random_hex(SPAN_ID_BYTE_LEN).await,
            trace_flags: TraceFlags::SAMPLED,
            is_remote: false,
            trace_state: vec![],
        };
        let start_time = now().await;
        on_start(context.clone()).await;
        pending()
            .lock()
            .unwrap()
            .entry(key(&call))
            .or_default()
            .push(Pending {
                context,
                parent_span_id,
                start_time,
            });
    }
}

impl AfterGuest for OtelBareSpans {
    async fn on_return(call: CallId) {
        let popped = pending()
            .lock()
            .unwrap()
            .get_mut(&key(&call))
            .and_then(|v| v.pop());
        let Some(p) = popped else {
            return;
        };
        let span = SpanData {
            span_context: p.context,
            parent_span_id: p.parent_span_id,
            span_kind: SpanKind::Internal,
            name: format!("{}::{}", call.interface_name, call.function_name),
            start_time: p.start_time,
            end_time: now().await,
            attributes: vec![
                kv("code.namespace", &call.interface_name),
                kv("code.function", &call.function_name),
            ],
            events: vec![],
            links: vec![],
            status: Status::Ok,
            instrumentation_scope: scope(),
            dropped_attributes: 0,
            dropped_events: 0,
            dropped_links: 0,
        };
        on_end(span).await;
    }
}

bindings::export!(OtelBareSpans with_types_in bindings);
