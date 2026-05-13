//! Builtin: emit `wasi:otel` metrics around wrapped calls.
//!
//! `on-call` records the start time. `on-return` folds the duration
//! into a per-(iface, fn) delta-window accumulator and ships a
//! `resource-metrics` payload (count + duration histogram) via
//! `wasi:otel/metrics.export` once the window closes: either `buffer`
//! samples have been seen or `flush_after_seconds` of wall-clock have
//! elapsed since the window opened. Staleness is checked on every
//! `on-return`, so a window that goes quiet sits unflushed until the
//! next call — tier-1 has no shutdown hook, and the unflushed tail
//! (<= one window per `(iface, fn)`) is dropped at process exit.
//!
//! Config keys are read once at first observation via
//! `splicer:builtin-config/get` and cached for the instance's
//! lifetime. Unset keys + parse failures fall back to defaults
//! silently (tier-1 has no logging surface).

mod bindings {
    // Per-export async filter (NOT `async: true`). Every import is
    // sync-WIT and MUST lower as plain `canon lower` (no async); see
    // `docs/TODO/sync-wit-suspend-limit.md` and hello-tier1 for the
    // rationale (sync-WIT-rooted task cannot block on canon-async wait).
    wit_bindgen::generate!({
        world: "otel-bare-metrics-mdl",
        async: [
            "export:splicer:tier1/before@0.3.0#on-call",
            "export:splicer:tier1/after@0.3.0#on-return",
        ],
        generate_all,
    });
}

// Codegenned from manifest.toml: manifest custom section + typed
// accessors in `mod config`. Defaults live only in `manifest.toml`.
include!(concat!(env!("OUT_DIR"), "/builtin_config_codegen.rs"));

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

/// Delta-window accumulator for a single OTel attribute set —
/// one `(interface, function)` pair. Each `on-return` against that
/// attribute set folds its measurement into the same `Agg`; the
/// `Agg` is drained and rebuilt on the next sample when a flush
/// closes the window (count threshold or wall-clock staleness).
///
/// Distinct from [`Pending`] (one-per-invocation, keyed by
/// `call.id`): `Pending` tracks the in-flight start time of a
/// single call, `Agg` aggregates *across many calls* sharing the
/// same emitted-metric attributes.
struct Agg {
    /// Number of samples folded into this window. Emitted as the
    /// Sum data point's value and as the Histogram's `count`.
    count: u64,
    /// Sum of duration samples, in seconds. Emitted as the
    /// Histogram's `sum`.
    duration_sum: f64,
    /// Minimum duration sample, in seconds. Emitted
    /// as the Histogram's `min`.
    duration_min: f64,
    /// Maximum duration sample, in seconds. Emitted as the
    /// Histogram's `max`.
    duration_max: f64,
    /// OTel explicit-bucket histogram counts, in the order the
    /// bounds appear in [`HISTOGRAM_BOUNDS_S`]. `bucket_counts[i]`
    /// is the number of samples that landed in bucket `i` — i.e.
    /// where the sample is ≤ `HISTOGRAM_BOUNDS_S[i]` and (for
    /// `i > 0`) greater than `HISTOGRAM_BOUNDS_S[i - 1]`. The
    /// trailing slot (index `HISTOGRAM_BOUNDS_S.len()`) is the
    /// `+Inf` overflow: samples larger than the last bound. Total
    /// length is therefore `HISTOGRAM_BOUNDS_S.len() + 1`, and
    /// the slot indices sum to `count`. See [`bucket_index`] for
    /// how each sample is placed.
    bucket_counts: Vec<u64>,
    /// Wall-clock time of the first sample's `on-call` for this
    /// window. Becomes the `start_time` of both the Sum and the
    /// Histogram data points; the staleness check uses
    /// `now - window_start` to decide when to flush.
    window_start: Datetime,
}

/// Parsed config, materialized once on first observation. Pulls
/// values via the codegen'd typed accessors, which read the manifest-
/// declared defaults when the user didn't set anything in YAML.
/// `buffer` is clamped to `>= 1` so `buffer = 0` doesn't lock the
/// window open forever.
struct Config {
    buffer: u32,
    flush_after_seconds: f64,
}

fn cached_config() -> &'static Config {
    static C: OnceLock<Config> = OnceLock::new();
    C.get_or_init(|| Config {
        buffer: config::buffer().max(1),
        flush_after_seconds: config::flush_after_seconds(),
    })
}

/// `(interface, function)` is the metric attribute set; one
/// accumulator per attribute set.
type CallKey = (String, String);

/// In-flight calls keyed by `call.id`. The host stamps each
/// invocation with a monotonic-per-instance id, so `on-return` pops
/// the matching `on-call` exactly even under concurrent or recursive
/// invocations of the same function.
fn pending() -> &'static Mutex<HashMap<u64, Pending>> {
    static M: OnceLock<Mutex<HashMap<u64, Pending>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Open delta windows, one per `(interface, function)` attribute
/// set. Each `Agg` accumulates many invocations sharing that
/// attribute set; entries are removed on flush and lazily
/// recreated by the next sample.
fn aggregators() -> &'static Mutex<HashMap<CallKey, Agg>> {
    static M: OnceLock<Mutex<HashMap<CallKey, Agg>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(call: &CallId) -> CallKey {
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
        name: "splicer:otel-bare-metrics".into(),
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

fn bucket_index(value: f64, bounds: &[f64]) -> usize {
    bounds
        .iter()
        .position(|b| value <= *b)
        .unwrap_or(bounds.len())
}

/// Difference between two `wasi:clocks` datetimes, in seconds.
/// Saturates to zero if `end < start` (clock skew, NTP step, etc.) so
/// downstream histogram bucketing never sees a negative sample.
fn duration_seconds(start: &Datetime, end: &Datetime) -> f64 {
    let to_dur = |d: &Datetime| Duration::new(d.seconds, d.nanoseconds);
    to_dur(end).saturating_sub(to_dur(start)).as_secs_f64()
}

fn build_resource_metrics(call: &CallId, agg: &Agg, end: Datetime) -> ResourceMetrics {
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
                value: MetricNumber::U64(agg.count),
                exemplars: vec![],
            }],
            start_time: agg.window_start,
            time: end,
            temporality: Temporality::Delta,
            is_monotonic: true,
        }),
    };

    let duration_metric = Metric {
        name: "component.call.duration".into(),
        description: "Duration of wrapped calls.".into(),
        unit: "s".into(),
        data: MetricData::F64Histogram(Histogram {
            data_points: vec![HistogramDataPoint {
                attributes,
                count: agg.count,
                bounds: HISTOGRAM_BOUNDS_S.to_vec(),
                bucket_counts: agg.bucket_counts.clone(),
                min: Some(MetricNumber::F64(agg.duration_min)),
                max: Some(MetricNumber::F64(agg.duration_max)),
                sum: MetricNumber::F64(agg.duration_sum),
                exemplars: vec![],
            }],
            start_time: agg.window_start,
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

pub struct OtelBareMetrics;

impl BeforeGuest for OtelBareMetrics {
    async fn on_call(call: CallId) {
        let start_time = now();
        pending()
            .lock()
            .unwrap()
            .insert(call.id, Pending { start_time });
    }
}

impl AfterGuest for OtelBareMetrics {
    async fn on_return(call: CallId) {
        let popped = pending().lock().unwrap().remove(&call.id);
        let Some(p) = popped else {
            return;
        };
        let end_time = now();
        let duration_s = duration_seconds(&p.start_time, &end_time);
        let cfg = cached_config();

        // Accumulate into the per-(iface, fn) window; capture whether
        // this measurement closes the window so we can flush after
        // dropping the lock.
        let flushed = {
            let mut map = aggregators().lock().unwrap();
            let k = key(&call);
            let agg = map.entry(k.clone()).or_insert_with(|| Agg {
                count: 0,
                duration_sum: 0.0,
                duration_min: f64::INFINITY,
                duration_max: f64::NEG_INFINITY,
                bucket_counts: vec![0u64; HISTOGRAM_BOUNDS_S.len() + 1],
                window_start: p.start_time,
            });
            agg.count += 1;
            agg.duration_sum += duration_s;
            if duration_s < agg.duration_min {
                agg.duration_min = duration_s;
            }
            if duration_s > agg.duration_max {
                agg.duration_max = duration_s;
            }
            agg.bucket_counts[bucket_index(duration_s, HISTOGRAM_BOUNDS_S)] += 1;

            let elapsed = duration_seconds(&agg.window_start, &end_time);
            let should_flush =
                u64::from(cfg.buffer) <= agg.count || elapsed >= cfg.flush_after_seconds;
            should_flush.then(|| map.remove(&k).expect("entry just inserted"))
        };

        if let Some(agg) = flushed {
            let payload = build_resource_metrics(&call, &agg, end_time);
            // The host's `export` returns a `result<_, error>` — best effort
            // here; nothing to do at the call site if the host can't ship.
            let _ = export(&payload);
        }
    }
}

bindings::export!(OtelBareMetrics with_types_in bindings);
