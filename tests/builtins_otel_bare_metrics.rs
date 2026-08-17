//! Behavioral smoke test for the `otel-bare-metrics` builtin.
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
//! `assets/builtins/otel-bare-metrics.wasm`, or `SPLICER_BUILTINS_DIR`
//! pointing at a directory containing it.

use anyhow::Result;
use wasmtime::component::Linker;

mod common;
use common::{
    add_builtin_config_stub, assert_call_attrs, capture_export, captured, drive_call_cycle,
    expect_bool, expect_enum, expect_list, expect_u64, f64_metric_number, field, first_data_point,
    histogram_record, metric, opt_f64_metric_number, otel_metrics_by_name, sum_record,
    u64_metric_number, Captures, Host,
};

const OTEL_METRICS: &str = "wasi:otel/metrics@0.2.0-rc.2";
const SCOPE: &str = "splicer:otel-bare-metrics";

fn setup(linker: &mut Linker<Host<Captures>>) -> Result<()> {
    add_builtin_config_stub(linker)?;
    let mut otel = linker.instance(OTEL_METRICS)?;
    capture_export(&mut otel, "export")
}

#[test]
fn otel_bare_metrics_exports_count_and_duration() -> Result<()> {
    let bytes = common::read_builtin("otel-bare-metrics");
    let capture = drive_call_cycle::<Captures, _>(&bytes, setup)?;
    let cap = capture.lock().unwrap();
    let exports = captured(&cap, "export");
    assert_eq!(exports.len(), 1, "exactly one export call expected");

    let metrics = otel_metrics_by_name(&exports[0], SCOPE);
    assert_eq!(metrics.len(), 2, "two metrics: count + duration");

    // Count: u64-sum, monotonic, delta-temporality, single data point of value 1.
    let count = sum_record(metric(&metrics, "component.call.count"));
    assert!(
        expect_bool(field(count, "is-monotonic")),
        "count Sum is monotonic"
    );
    assert_eq!(
        expect_enum(field(count, "temporality")),
        "delta",
        "count uses delta temporality"
    );
    let count_dp = first_data_point(count);
    assert_eq!(
        u64_metric_number(field(count_dp, "value")),
        1,
        "count value is 1 per call"
    );
    assert_call_attrs(expect_list(field(count_dp, "attributes")));

    // Duration: f64-histogram, delta-temporality, single sample.
    let hist = histogram_record(metric(&metrics, "component.call.duration"));
    assert_eq!(
        expect_enum(field(hist, "temporality")),
        "delta",
        "duration uses delta temporality"
    );
    let hist_dp = first_data_point(hist);
    assert_eq!(expect_u64(field(hist_dp, "count")), 1, "histogram count is 1");

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
    let total: u64 = bucket_counts.iter().map(expect_u64).sum();
    assert_eq!(total, 1, "exactly one sample distributed across buckets");

    // sum / min / max all come from the same single sample, so they must
    // agree. Don't pin a value (clock jitter), but they can't be negative.
    let sum_v = f64_metric_number(field(hist_dp, "sum"));
    let min_v = opt_f64_metric_number(field(hist_dp, "min")).expect("min present");
    let max_v = opt_f64_metric_number(field(hist_dp, "max")).expect("max present");
    assert!(sum_v >= 0.0, "duration sum is non-negative");
    assert_eq!(min_v, sum_v, "single-sample histogram: min == sum");
    assert_eq!(max_v, sum_v, "single-sample histogram: max == sum");
    assert_call_attrs(expect_list(field(hist_dp, "attributes")));

    Ok(())
}
