//! Builtin: derive read-only metrics about the payloads crossing a
//! wrapped boundary and export them via `wasi:otel/metrics`.
//!
//! Tier-2 hands each hook the *lifted* payload values (`Vec<Field>` on
//! `on-call`, `option<field-tree>` on `on-return`). For every payload
//! this builtin walks the value tree once into a [`PayloadStats`], folds
//! each registered [`MetricDesc`]'s sample into a per-(interface,
//! function, direction) delta window, and ships the window via
//! `wasi:otel/metrics.export` once it closes (either `buffer` samples
//! have been folded or `flush_after_seconds` of wall-clock have elapsed).
//! Only statistics are kept — payload content is never persisted.
//!
//! Adding a metric is a two-line change: add a field to [`PayloadStats`]
//! (populated in [`walk`]) and a row to [`METRICS`]. The aggregation and
//! export loop is generic over the registry. The per-kind type histogram
//! (`payload.cell.count`) is the one non-scalar metric and is built by a
//! dedicated branch beside the registry loop.

mod bindings {
    splicer_tool_sdk::wit_bindgen!({
        world: "otel-payload-metrics-mdl",
        generate_all,
    });
}

// Codegenned from manifest.toml: manifest custom section + typed
// accessors in `mod config`. Defaults live only in `manifest.toml`.
include!(concat!(env!("OUT_DIR"), "/builtin_config_codegen.rs"));

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use bindings::exports::splicer::tier2::after::Guest as AfterGuest;
use bindings::exports::splicer::tier2::before::Guest as BeforeGuest;
use bindings::wasi::clocks::wall_clock::{now, Datetime};
use bindings::wasi::otel::metrics::{
    export, ExponentialBucket, ExponentialHistogram, ExponentialHistogramDataPoint, Metric,
    MetricData, MetricNumber, ResourceMetrics, ScopeMetrics, Sum, SumDataPoint, Temporality,
};
use bindings::wasi::otel::types::{InstrumentationScope, KeyValue, Resource};
use splicer_tool_sdk::{CallId, Cell, Field, FieldTree};

// ── Payload statistics ────────────────────────────────────────────────

/// Per-payload statistics. One field per derived quantity; extend both
/// this struct and [`METRICS`] to add a metric.
#[derive(Default)]
struct PayloadStats {
    /// Estimated canonical-ABI byte size (see `FieldTree::size_est`).
    size_bytes: u64,
    /// Number of value nodes reachable from the root.
    node_count: u64,
    /// Maximum nesting depth (root is depth 1).
    depth: u64,
    /// Largest `list`/`tuple` element count anywhere in the payload.
    max_collection_len: u64,
    /// Per-`Cell`-kind tally, keyed by `Cell::kind_name`.
    kind_counts: BTreeMap<&'static str, u64>,
    /// `result-ok` cells observed.
    result_ok: u64,
    /// `result-err` cells observed.
    result_err: u64,
}

/// Walk a lifted value tree once, from the root, accumulating every
/// [`PayloadStats`] quantity. Iterative with a visited guard so shared or
/// malformed indices can't loop or double-count.
fn walk(tree: &FieldTree) -> PayloadStats {
    let mut s = PayloadStats {
        size_bytes: tree.size_est(),
        ..PayloadStats::default()
    };
    if tree.cells.is_empty() {
        return s;
    }
    let mut visited = vec![false; tree.cells.len()];
    let mut stack = vec![(tree.root, 1u64)];
    while let Some((idx, depth)) = stack.pop() {
        let ui = idx as usize;
        if ui >= tree.cells.len() || visited[ui] {
            continue;
        }
        visited[ui] = true;
        let cell = &tree.cells[ui];

        s.node_count += 1;
        if depth > s.depth {
            s.depth = depth;
        }
        *s.kind_counts.entry(cell.kind_name()).or_default() += 1;

        match cell {
            Cell::ListOf(ix) | Cell::TupleOf(ix) => {
                let len = ix.len() as u64;
                if len > s.max_collection_len {
                    s.max_collection_len = len;
                }
            }
            Cell::ResultOk(_) => s.result_ok += 1,
            Cell::ResultErr(_) => s.result_err += 1,
            _ => {}
        }

        for child in tree.child_indices(cell) {
            stack.push((child, depth + 1));
        }
    }
    s
}

// ── Metric registry ────────────────────────────────────────

/// OTel instrument shape for a registry entry.
enum Instrument {
    /// Base-2 exponential histogram over the per-payload sample
    Histogram,
    /// Monotonic delta sum of the per-payload sample.
    Sum,
}

/// A scalar metric derived from a payload
struct MetricDesc {
    name: &'static str,
    description: &'static str,
    unit: &'static str,
    instrument: Instrument,
    sample: fn(&PayloadStats) -> f64,
}

/// The metric registry. Add a row (plus a [`PayloadStats`] field) to
/// grow the emitted metric set.
static METRICS: &[MetricDesc] = &[
    MetricDesc {
        name: "payload.size",
        description: "Estimated serialized byte size of the payload.",
        unit: "By",
        instrument: Instrument::Histogram,
        sample: |s| s.size_bytes as f64,
    },
    MetricDesc {
        name: "payload.node.count",
        description: "Number of value nodes in the payload.",
        unit: "{node}",
        instrument: Instrument::Histogram,
        sample: |s| s.node_count as f64,
    },
    MetricDesc {
        name: "payload.depth",
        description: "Maximum nesting depth of the payload.",
        unit: "{node}",
        instrument: Instrument::Histogram,
        sample: |s| s.depth as f64,
    },
    MetricDesc {
        name: "payload.collection.max_length",
        description: "Largest list/tuple element count in the payload.",
        unit: "{element}",
        instrument: Instrument::Histogram,
        sample: |s| s.max_collection_len as f64,
    },
    MetricDesc {
        name: "payload.result.ok",
        description: "Count of result-ok values observed.",
        unit: "{result}",
        instrument: Instrument::Sum,
        sample: |s| s.result_ok as f64,
    },
    MetricDesc {
        name: "payload.result.error",
        description: "Count of result-err values observed.",
        unit: "{result}",
        instrument: Instrument::Sum,
        sample: |s| s.result_err as f64,
    },
];

/// Per-metric accumulator. Histograms use count/sum/min/max plus the
/// exponential-bucket map; sums use `total`.
#[derive(Default)]
struct MetricAcc {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
    /// Exponential-histogram positive buckets
    buckets: HashMap<i32, u64>,
    zero_count: u64,
    /// Running total for `Sum` instruments.
    total: f64,
}

impl MetricAcc {
    fn new() -> Self {
        MetricAcc {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            ..Default::default()
        }
    }

    fn fold(&mut self, desc: &MetricDesc, v: f64) {
        match desc.instrument {
            Instrument::Histogram => {
                self.count += 1;
                self.sum += v;
                if v < self.min {
                    self.min = v;
                }
                if v > self.max {
                    self.max = v;
                }
                if v > 0.0 {
                    *self.buckets.entry(bucket_index(v)).or_default() += 1;
                } else {
                    self.zero_count += 1;
                }
            }
            Instrument::Sum => self.total += v,
        }
    }
}

// ── Delta-window aggregation ──────────────────────────────────────────

struct Window {
    window_start: Datetime,
    /// Payloads folded into this window (flush threshold + denominator).
    sample_count: u64,
    /// One accumulator per [`METRICS`] entry, aligned by index.
    accs: Vec<MetricAcc>,
    /// Per-kind totals for the `payload.cell.count` type histogram.
    kind_counts: BTreeMap<&'static str, u64>,
}

/// `(interface, function, direction)`
type WinKey = (String, String, &'static str);

fn windows() -> &'static Mutex<HashMap<WinKey, Window>> {
    static M: OnceLock<Mutex<HashMap<WinKey, Window>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Walk one payload, fold it into its window, and flush + export if the
/// window closes.
fn observe(call: &CallId, direction: &'static str, tree: &FieldTree) {
    let stats = walk(tree);
    // `buffer` clamped to `>= 1` so `buffer = 0` can't hold the window
    // open forever.
    let buffer = config::buffer().max(1);
    let flush_after_seconds = config::flush_after_seconds();
    let now_dt = now();

    let flushed = {
        let mut map = windows().lock().unwrap();
        let k = (
            call.interface_name.clone(),
            call.function_name.clone(),
            direction,
        );
        let w = map.entry(k.clone()).or_insert_with(|| Window {
            window_start: now_dt,
            sample_count: 0,
            accs: METRICS.iter().map(|_| MetricAcc::new()).collect(),
            kind_counts: BTreeMap::new(),
        });
        w.sample_count += 1;
        for (i, desc) in METRICS.iter().enumerate() {
            w.accs[i].fold(desc, (desc.sample)(&stats));
        }
        for (&kind, &count) in &stats.kind_counts {
            *w.kind_counts.entry(kind).or_default() += count;
        }

        let elapsed = duration_seconds(&w.window_start, &now_dt);
        let should_flush = u64::from(buffer) <= w.sample_count || elapsed >= flush_after_seconds;
        should_flush.then(|| map.remove(&k).expect("entry just inserted"))
    };

    if let Some(w) = flushed {
        let payload = build_resource_metrics(call, direction, &w, now_dt);
        // Best-effort if error occurs during emit
        let _ = export(&payload);
    }
}

// ── OTel payload construction ─────────────────────────────────────────

/// Exponential-histogram resolution
const SCALE: i8 = 2;

/// OTel exponential-histogram bucket index for a positive value at [`SCALE`]
fn bucket_index(value: f64) -> i32 {
    let scale_factor = 2.0_f64.powi(SCALE as i32);
    (value.log2() * scale_factor).ceil() as i32 - 1
}

/// Pack a sparse `index -> count` map into an OTel `exponential-bucket`:
/// `offset` is the lowest occupied index, `counts` the contiguous run up
/// to the highest. Empty when no positive samples were recorded.
fn exponential_bucket(buckets: &HashMap<i32, u64>) -> ExponentialBucket {
    let (Some(&min_idx), Some(&max_idx)) = (buckets.keys().min(), buckets.keys().max()) else {
        return ExponentialBucket {
            offset: 0,
            counts: vec![],
        };
    };
    let counts = (min_idx..=max_idx)
        .map(|i| buckets.get(&i).copied().unwrap_or(0))
        .collect();
    ExponentialBucket {
        offset: min_idx,
        counts,
    }
}

fn duration_seconds(start: &Datetime, end: &Datetime) -> f64 {
    let to_dur = |d: &Datetime| Duration::new(d.seconds, d.nanoseconds);
    to_dur(end).saturating_sub(to_dur(start)).as_secs_f64()
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
        name: "splicer:otel-payload-metrics".into(),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        schema_url: None,
        attributes: vec![],
    }
}

/// Build one OTel `Metric` from a registry entry and its accumulator.
fn build_metric(
    desc: &MetricDesc,
    acc: &MetricAcc,
    base: &[KeyValue],
    start: Datetime,
    end: Datetime,
) -> Metric {
    let data = match desc.instrument {
        Instrument::Histogram => MetricData::F64ExponentialHistogram(ExponentialHistogram {
            data_points: vec![ExponentialHistogramDataPoint {
                attributes: base.to_vec(),
                count: acc.count,
                min: Some(MetricNumber::F64(acc.min)),
                max: Some(MetricNumber::F64(acc.max)),
                sum: MetricNumber::F64(acc.sum),
                scale: SCALE,
                zero_count: acc.zero_count,
                positive_bucket: exponential_bucket(&acc.buckets),
                negative_bucket: ExponentialBucket {
                    offset: 0,
                    counts: vec![],
                },
                zero_threshold: 0.0,
                exemplars: vec![],
            }],
            start_time: start,
            time: end,
            temporality: Temporality::Delta,
        }),
        Instrument::Sum => MetricData::U64Sum(Sum {
            data_points: vec![SumDataPoint {
                attributes: base.to_vec(),
                value: MetricNumber::U64(acc.total as u64),
                exemplars: vec![],
            }],
            start_time: start,
            time: end,
            temporality: Temporality::Delta,
            is_monotonic: true,
        }),
    };
    Metric {
        name: desc.name.into(),
        description: desc.description.into(),
        unit: desc.unit.into(),
        data,
    }
}

fn build_kind_metric(
    kind_counts: &BTreeMap<&'static str, u64>,
    base: &[KeyValue],
    start: Datetime,
    end: Datetime,
) -> Metric {
    let mut data_points = Vec::new();
    for (&kind, &count) in kind_counts {
        let mut attributes = base.to_vec();
        attributes.push(kv("splicer.cell.kind", kind));
        data_points.push(SumDataPoint {
            attributes,
            value: MetricNumber::U64(count),
            exemplars: vec![],
        });
    }
    Metric {
        name: "payload.cell.count".into(),
        description: "Count of value cells by kind.".into(),
        unit: "{cell}".into(),
        data: MetricData::U64Sum(Sum {
            data_points,
            start_time: start,
            time: end,
            temporality: Temporality::Delta,
            is_monotonic: true,
        }),
    }
}

fn build_resource_metrics(
    call: &CallId,
    direction: &str,
    w: &Window,
    end: Datetime,
) -> ResourceMetrics {
    let base = vec![
        kv("code.namespace", &call.interface_name),
        kv("code.function", &call.function_name),
        kv("splicer.payload.direction", direction),
    ];

    let mut metrics: Vec<Metric> = METRICS
        .iter()
        .enumerate()
        .map(|(i, desc)| build_metric(desc, &w.accs[i], &base, w.window_start, end))
        .collect();
    metrics.push(build_kind_metric(&w.kind_counts, &base, w.window_start, end));

    ResourceMetrics {
        resource: Resource {
            attributes: vec![],
            schema_url: None,
        },
        scope_metrics: vec![ScopeMetrics {
            scope: scope(),
            metrics,
        }],
    }
}

// ── Tier-2 hooks ──────────────────────────────────────────────────────

pub struct OtelPayloadMetrics;

impl BeforeGuest for OtelPayloadMetrics {
    fn on_call(call: CallId, args: Vec<Field>) {
        if matches!(
            config::payloads(),
            config::Payloads::Args | config::Payloads::Both
        ) {
            for arg in &args {
                observe(&call, "arg", &arg.tree);
            }
        }
    }
}

impl AfterGuest for OtelPayloadMetrics {
    fn on_return(call: CallId, result: Option<FieldTree>) {
        if matches!(
            config::payloads(),
            config::Payloads::Result | config::Payloads::Both
        ) {
            if let Some(tree) = &result {
                observe(&call, "result", tree);
            }
        }
    }
}

bindings::export!(OtelPayloadMetrics with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;
    use splicer_tool_sdk::RecordInfo;

    fn empty_tree(cells: Vec<Cell>, root: u32) -> FieldTree {
        FieldTree {
            cells,
            record_infos: vec![],
            flags_infos: vec![],
            enum_infos: vec![],
            variant_infos: vec![],
            handle_infos: vec![],
            root,
        }
    }

    #[test]
    fn scalar_payload() {
        // A single integer at the root.
        let stats = walk(&empty_tree(vec![Cell::Integer(42)], 0));
        assert_eq!(stats.node_count, 1);
        assert_eq!(stats.depth, 1);
        assert_eq!(stats.size_bytes, 8);
        assert_eq!(stats.kind_counts.get("integer").copied(), Some(1));
        assert_eq!(stats.max_collection_len, 0);
    }

    #[test]
    fn record_with_string_list_and_err() {
        // record { name: "abc", tags: list["x","y"], status: result-err }
        // cells: 0 record, 1 text "abc", 2 list[3,4], 3 text "x",
        //        4 text "y", 5 result-err(none)
        let mut tree = empty_tree(
            vec![
                Cell::RecordOf(0),
                Cell::Text("abc".into()),
                Cell::ListOf(vec![3, 4]),
                Cell::Text("x".into()),
                Cell::Text("y".into()),
                Cell::ResultErr(None),
            ],
            0,
        );
        tree.record_infos = vec![RecordInfo {
            type_name: "rec".into(),
            fields: vec![
                ("name".into(), 1),
                ("tags".into(), 2),
                ("status".into(), 5),
            ],
        }];

        let stats = walk(&tree);
        assert_eq!(stats.node_count, 6);
        // root(1) → list(2) → elems(3) is the deepest path.
        assert_eq!(stats.depth, 3);
        assert_eq!(stats.max_collection_len, 2);
        assert_eq!(stats.result_err, 1);
        assert_eq!(stats.result_ok, 0);
        assert_eq!(stats.kind_counts.get("text").copied(), Some(3));
        assert_eq!(stats.kind_counts.get("list").copied(), Some(1));
        // Canonical-ABI estimate (see FieldTree::size_est): record 0
        // + text(8+3) + list header 8 + text(8+1) + text(8+1) + err disc 1.
        assert_eq!(stats.size_bytes, 38);
    }

    #[test]
    fn cyclic_indices_terminate() {
        // Malformed: a list pointing back at itself must not loop.
        let stats = walk(&empty_tree(vec![Cell::ListOf(vec![0])], 0));
        assert_eq!(stats.node_count, 1);
    }

    #[test]
    fn registry_and_accumulators_stay_aligned() {
        // Every registry entry gets an accumulator; folding a sample
        // must not panic for any instrument shape.
        let stats = walk(&empty_tree(vec![Cell::Bool(true)], 0));
        let mut accs: Vec<MetricAcc> = METRICS.iter().map(|_| MetricAcc::new()).collect();
        assert_eq!(accs.len(), METRICS.len());
        for (i, desc) in METRICS.iter().enumerate() {
            accs[i].fold(desc, (desc.sample)(&stats));
        }
    }

    #[test]
    fn bucket_index_maps_values_to_exponential_buckets() {
        // At SCALE=2, base = 2^0.25. Bucket i covers (base^i, base^(i+1)];
        // a value equal to a power of two lands in the bucket just below
        // its exponent's boundary, and buckets increase with the value.
        assert_eq!(bucket_index(1.0), -1, "1 == base^0 sits in bucket -1");
        assert!(
            bucket_index(2.0) < bucket_index(1000.0),
            "buckets are monotonic in the value"
        );
        // 2 == base^4 (2^(4*0.25) = 2^1); as an upper boundary it lands in
        // bucket 3, i.e. ceil(log2(2)*4) - 1 = 3.
        assert_eq!(bucket_index(2.0), 3);
    }

    #[test]
    fn exponential_bucket_packs_sparse_indices_contiguously() {
        let mut buckets = HashMap::new();
        buckets.insert(3, 2u64);
        buckets.insert(5, 1u64);
        let packed = exponential_bucket(&buckets);
        assert_eq!(packed.offset, 3, "offset is the lowest index");
        assert_eq!(packed.counts, vec![2, 0, 1], "gap at index 4 is zero-filled");
        assert_eq!(exponential_bucket(&HashMap::new()).counts, Vec::<u64>::new());
    }
}
