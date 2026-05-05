//! Builtin: emit `wasi:otel` metrics around every wrapped call.
//!
//! `on-call` records the start time. `on-return` ships a fresh
//! delta-temporality `resource-metrics` payload with one data point
//! per metric (count + duration), then drains the pending entry.
//! No in-component aggregation: a production OTel SDK would batch
//! many measurements into one export, but this builtin doesn't yet.
//! The downstream collector re-aggregates the deltas.
//!
//! Cheap to implement; trades host-call frequency for simplicity.
//! Aggregation with a traffic-driven flush is a planned follow-up
//! gated on a builtin-config substrate (`buffer` + per-call flush
//! threshold).

// TODO(aggregation): once the splice-time `splicer:builtin-config`
// substrate lands (post-tier2), wire this builtin to read config at
// init and aggregate measurements rather than exporting per-call.
//
// Config keys, parsed at init via `splicer:builtin-config/get` into
// a `OnceLock<Config>` (string-typed at the WIT boundary; parsed
// here):
//
//   * `buffer`              u32, default 1. Accumulate N measurements
//                           per (iface, fn) before flushing. `1` is
//                           the current per-call behavior.
//   * `flush_after_seconds` f64, default 10.0. Staleness flush
//                           trigger; ignored when `buffer == 1`.
//
// State: per-(iface, fn) accumulator for the `Sum` count + the
// `Histogram` (bucket counts, sum, min, max). Allocated lazily on
// first observation for that attribute set; resides alongside the
// existing `pending()` map but with persistent (not drained-per-call)
// semantics.
//
// Flush trigger checked on every `on-return`: flush a given
// attribute set when its buffered count >= `buffer` OR wall-clock
// since its last flush > `flush_after_seconds`, whichever first.
// Bounded data loss at process exit — tier-1 has no shutdown hook,
// so the unflushed tail (<= one flush window) is dropped.
//
// Until the substrate lands, this builtin runs in always-flush mode
// (effectively `buffer = 1`).

mod bindings {
    wit_bindgen::generate!({
        world: "otel-metrics-mdl",
        async: true,
        generate_all,
    });
}

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use bindings::exports::splicer::tier1::after::Guest as AfterGuest;
use bindings::exports::splicer::tier1::before::Guest as BeforeGuest;
use bindings::splicer::common::types::CallId;
use bindings::wasi::clocks::wall_clock::{now, Datetime};
use bindings::wasi::otel::metrics::{
    export, Histogram, HistogramDataPoint, Metric, MetricData, MetricNumber, ResourceMetrics,
    ScopeMetrics, Sum, SumDataPoint, Temporality,
};
use bindings::wasi::otel::types::{InstrumentationScope, KeyValue, Resource};

/// In-flight call recorded at `on-call`, drained at `on-return`.
struct Pending {
    start_time: Datetime,
}

/// Stack per `(interface, function)` so concurrent or recursive
/// invocations of the same name don't clobber each other's start
/// times.
///
/// TODO(call-id correlation): switch the key to `call.id` once the
/// per-invocation `u64` field on `splicer:common/types::call-id`
/// lands. Today's name-only key still mis-pairs concurrent
/// invocations of the same function — `on-return` can pop a
/// sibling's start time. The stack just keeps the failure mode
/// bounded (LIFO swap) instead of unbounded growth.
fn pending() -> &'static Mutex<HashMap<(String, String), Vec<Pending>>> {
    static M: OnceLock<Mutex<HashMap<(String, String), Vec<Pending>>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(call: &CallId) -> (String, String) {
    (call.interface_name.clone(), call.function_name.clone())
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
        name: "splicer:otel-metrics".into(),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        schema_url: None,
        attributes: vec![],
    }
}

/// Default explicit bucket boundaries for request-duration
/// histograms, in seconds. Taken from the OpenTelemetry HTTP
/// semantic-conventions advice for `http.server.request.duration`
/// — the de facto default reused by language SDKs (Java, Go, Python,
/// .NET) for any RPC-style call-duration histogram. Reasonable for
/// "WIT-component call duration" too; not HTTP-specific.
///
/// Source: OpenTelemetry semantic conventions, "HTTP metrics" — see
/// https://opentelemetry.io/docs/specs/semconv/http/http-metrics/
const HISTOGRAM_BOUNDS_S: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// Index `bucket_counts` by where `value` falls in `bounds`. The
/// returned vec has `bounds.len() + 1` slots — the trailing slot is
/// the +Inf overflow bucket — and exactly one slot is set to 1.
fn single_sample_buckets(value: f64, bounds: &[f64]) -> Vec<u64> {
    let mut counts = vec![0u64; bounds.len() + 1];
    let idx = bounds
        .iter()
        .position(|b| value <= *b)
        .unwrap_or(bounds.len());
    counts[idx] = 1;
    counts
}

/// Difference between two `wasi:clocks` datetimes, in seconds.
/// Saturates to zero if `end < start` (clock skew, NTP step, etc.) so
/// downstream histogram bucketing never sees a negative sample.
fn duration_seconds(start: &Datetime, end: &Datetime) -> f64 {
    let to_dur = |d: &Datetime| Duration::new(d.seconds, d.nanoseconds);
    to_dur(end).saturating_sub(to_dur(start)).as_secs_f64()
}

fn build_resource_metrics(
    call: &CallId,
    start: Datetime,
    end: Datetime,
    duration_s: f64,
) -> ResourceMetrics {
    let attributes = vec![
        kv("code.namespace", &call.interface_name),
        kv("code.function", &call.function_name),
    ];

    let count_metric = Metric {
        name: "component.call.count".into(),
        description: "Number of wrapped calls observed.".into(),
        unit: "{call}".into(),
        data: MetricData::U64Sum(Sum {
            data_points: vec![SumDataPoint {
                attributes: attributes.clone(),
                value: MetricNumber::U64(1),
                exemplars: vec![],
            }],
            start_time: start,
            time: end,
            temporality: Temporality::Delta,
            is_monotonic: true,
        }),
    };

    let bounds = HISTOGRAM_BOUNDS_S.to_vec();
    let bucket_counts = single_sample_buckets(duration_s, HISTOGRAM_BOUNDS_S);
    let duration_metric = Metric {
        name: "component.call.duration".into(),
        description: "Duration of wrapped calls.".into(),
        unit: "s".into(),
        data: MetricData::F64Histogram(Histogram {
            data_points: vec![HistogramDataPoint {
                attributes,
                count: 1,
                bounds,
                bucket_counts,
                min: Some(MetricNumber::F64(duration_s)),
                max: Some(MetricNumber::F64(duration_s)),
                sum: MetricNumber::F64(duration_s),
                exemplars: vec![],
            }],
            start_time: start,
            time: end,
            temporality: Temporality::Delta,
        }),
    };

    ResourceMetrics {
        resource: Resource {
            attributes: vec![],
            schema_url: None,
        },
        scope_metrics: vec![ScopeMetrics {
            scope: scope(),
            metrics: vec![count_metric, duration_metric],
        }],
    }
}

pub struct OtelMetrics;

impl BeforeGuest for OtelMetrics {
    async fn on_call(call: CallId) {
        let start_time = now().await;
        pending()
            .lock()
            .unwrap()
            .entry(key(&call))
            .or_default()
            .push(Pending { start_time });
    }
}

impl AfterGuest for OtelMetrics {
    async fn on_return(call: CallId) {
        let popped = pending()
            .lock()
            .unwrap()
            .get_mut(&key(&call))
            .and_then(|v| v.pop());
        let Some(p) = popped else {
            return;
        };
        let end_time = now().await;
        let duration_s = duration_seconds(&p.start_time, &end_time);
        let payload = build_resource_metrics(&call, p.start_time, end_time, duration_s);
        // The host's `export` returns a `result<_, error>` — best effort
        // here; nothing to do at the call site if the host can't ship.
        let _ = export(payload).await;
    }
}

bindings::export!(OtelMetrics with_types_in bindings);
