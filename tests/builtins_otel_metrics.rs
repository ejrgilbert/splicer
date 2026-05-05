//! Behavioral smoke test for the `otel-metrics` builtin.
//!
//! Instantiates the embedded component in wasmtime with a fake
//! `wasi:otel/metrics` host that captures `export(resource-metrics)`
//! calls, drives `splicer:tier1/before#on-call` and
//! `after#on-return` for a synthetic call-id, and asserts the captured
//! payload carries the expected `component.call.count` (`u64-sum`,
//! delta, monotonic, value=1) and `component.call.duration`
//! (`f64-histogram`, delta, single sample) metrics with
//! `code.namespace` / `code.function` attributes.
//!
//! Requires `make build-builtins` to have populated
//! `assets/builtins/otel-metrics.wasm` (embedded below via
//! `include_bytes!`).

use std::collections::HashMap;

use anyhow::Result;
use wasmtime::component::Val;

mod common;
use common::{
    assert_call_attrs, drive_call_cycle, expect_bool, expect_enum, expect_list, expect_record,
    expect_string, expect_u64, expect_variant, field, Host,
};

const OTEL_METRICS: &str = "wasi:otel/metrics@0.2.0-rc.2";

#[derive(Default)]
struct Capture {
    exports: Vec<Val>,
}

fn add_otel_metrics_to_linker(
    linker: &mut wasmtime::component::Linker<Host<Capture>>,
) -> Result<()> {
    let mut otel = linker.instance(OTEL_METRICS)?;

    otel.func_new("export", |store, _ty, params, results| {
        store
            .data()
            .capture
            .lock()
            .unwrap()
            .exports
            .push(params[0].clone());
        // `result<_, error>` — ok side has no payload (the `_`).
        results[0] = Val::Result(Ok(None));
        Ok(())
    })?;

    Ok(())
}

#[test]
fn otel_metrics_exports_count_and_duration() -> Result<()> {
    let bytes = include_bytes!("../assets/builtins/otel-metrics.wasm");
    let capture = drive_call_cycle::<Capture, _>(bytes, add_otel_metrics_to_linker)?;
    let cap = capture.lock().unwrap();

    assert_eq!(cap.exports.len(), 1, "exactly one export call expected");

    let resource_metrics = expect_record(&cap.exports[0]);
    let scope_metrics_list = expect_list(field(resource_metrics, "scope-metrics"));
    assert_eq!(scope_metrics_list.len(), 1, "exactly one scope-metrics entry");

    let scope_metrics = expect_record(&scope_metrics_list[0]);
    let scope = expect_record(field(scope_metrics, "scope"));
    assert_eq!(
        expect_string(field(scope, "name")),
        "splicer:otel-metrics",
        "instrumentation scope name identifies the source"
    );

    let metrics_list = expect_list(field(scope_metrics, "metrics"));
    assert_eq!(metrics_list.len(), 2, "two metrics: count + duration");

    let mut by_name: HashMap<&str, &[(String, Val)]> = HashMap::new();
    for m in metrics_list {
        let r = expect_record(m);
        by_name.insert(expect_string(field(r, "name")), r);
    }

    let count_metric = by_name
        .get("component.call.count")
        .expect("component.call.count metric present");
    let duration_metric = by_name
        .get("component.call.duration")
        .expect("component.call.duration metric present");

    // Count: u64-sum, monotonic, delta-temporality, single data point of value 1.
    let (case, payload) = expect_variant(field(count_metric, "data"));
    assert_eq!(case, "u64-sum", "count uses u64-sum aggregation");
    let sum = expect_record(payload.expect("u64-sum carries a sum payload"));
    assert!(
        expect_bool(field(sum, "is-monotonic")),
        "count Sum is monotonic"
    );
    assert_eq!(
        expect_enum(field(sum, "temporality")),
        "delta",
        "count uses delta temporality"
    );
    let count_dps = expect_list(field(sum, "data-points"));
    assert_eq!(count_dps.len(), 1, "one count data point");
    let count_dp = expect_record(&count_dps[0]);
    let (case, payload) = expect_variant(field(count_dp, "value"));
    assert_eq!(case, "u64", "count value is u64");
    let v = match payload {
        Some(Val::U64(n)) => *n,
        other => panic!("expected u64 metric-number payload, got {other:?}"),
    };
    assert_eq!(v, 1, "count value is 1 per call");
    assert_call_attrs(expect_list(field(count_dp, "attributes")));

    // Duration: f64-histogram, delta-temporality, single sample.
    let (case, payload) = expect_variant(field(duration_metric, "data"));
    assert_eq!(case, "f64-histogram", "duration uses f64-histogram");
    let hist = expect_record(payload.expect("f64-histogram carries a histogram payload"));
    assert_eq!(
        expect_enum(field(hist, "temporality")),
        "delta",
        "duration uses delta temporality"
    );
    let hist_dps = expect_list(field(hist, "data-points"));
    assert_eq!(hist_dps.len(), 1, "one histogram data point");
    let hist_dp = expect_record(&hist_dps[0]);
    assert_eq!(
        expect_u64(field(hist_dp, "count")),
        1,
        "histogram count is 1"
    );

    let bounds = expect_list(field(hist_dp, "bounds"));
    assert_eq!(
        bounds.len(),
        14,
        "14 explicit bucket boundaries (OTel HTTP default)"
    );
    let bucket_counts = expect_list(field(hist_dp, "bucket-counts"));
    assert_eq!(
        bucket_counts.len(),
        15,
        "bucket-counts is bounds.len() + 1 (overflow)"
    );
    let total_count: u64 = bucket_counts.iter().map(expect_u64).sum();
    assert_eq!(total_count, 1, "exactly one sample distributed across buckets");

    // sum / min / max all come from the same single sample, so they
    // must agree. Don't pin a specific value (clock jitter), but they
    // can't be negative.
    let sum_v = expect_f64_metric_number(field(hist_dp, "sum"));
    let min_v = expect_optional_f64_metric_number(field(hist_dp, "min"))
        .expect("min present for non-empty histogram");
    let max_v = expect_optional_f64_metric_number(field(hist_dp, "max"))
        .expect("max present for non-empty histogram");
    assert!(sum_v >= 0.0, "duration sum is non-negative");
    assert_eq!(min_v, sum_v, "single-sample histogram: min == sum");
    assert_eq!(max_v, sum_v, "single-sample histogram: max == sum");

    assert_call_attrs(expect_list(field(hist_dp, "attributes")));

    Ok(())
}

/// Unwrap a `metric-number` variant that's expected to be the `f64`
/// case. Panics on other cases or non-variants.
fn expect_f64_metric_number(v: &Val) -> f64 {
    let (case, payload) = expect_variant(v);
    assert_eq!(case, "f64", "expected f64 metric-number, got {case}");
    match payload {
        Some(Val::Float64(f)) => *f,
        other => panic!("expected f64 payload, got {other:?}"),
    }
}

/// Same as `expect_f64_metric_number` but for `option<metric-number>`.
fn expect_optional_f64_metric_number(v: &Val) -> Option<f64> {
    if let Val::Option(inner) = v {
        inner.as_deref().map(expect_f64_metric_number)
    } else {
        panic!("expected option, got {v:?}")
    }
}
