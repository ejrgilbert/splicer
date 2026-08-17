//! Behavioral smoke test for the `otel-payload-metrics` builtin.
//!
//! Instantiates the embedded tier-2 component in wasmtime with a fake
//! `wasi:otel/metrics` host that captures `export(resource-metrics)`
//! calls, drives `splicer:tier2/before#on-call` (one string arg) and
//! `after#on-return` (a `result-err` payload) for a synthetic call-id,
//! and asserts the captured payload metrics carry the expected values
//! and `code.namespace` / `code.function` / `splicer.payload.direction`
//! attributes. `buffer` defaults to 1, so every payload flushes its own
//! delta window immediately: one export per direction.
//!
//! Requires `make build-builtins` to have populated
//! `assets/builtins/otel-payload-metrics.wasm`, or `SPLICER_BUILTINS_DIR`
//! pointing at a directory containing it.

use std::collections::HashMap;

use anyhow::Result;
use wasmtime::component::{Linker, Val};

mod common;
use common::{
    add_builtin_config_stub, assert_json_attr, capture_export, captured, drive_tier2_cycle,
    expect_list, expect_record, f64_metric_number, field, first_data_point, exp_histogram_record,
    kv_map, metric, otel_metrics_by_name, sum_record, u64_metric_number, Captures, Host, TARGET_FN,
    TARGET_IFACE,
};

const OTEL_METRICS: &str = "wasi:otel/metrics@0.2.0-rc.2";
const SCOPE: &str = "splicer:otel-payload-metrics";

fn setup(linker: &mut Linker<Host<Captures>>) -> Result<()> {
    add_builtin_config_stub(linker)?;
    let mut otel = linker.instance(OTEL_METRICS)?;
    capture_export(&mut otel, "export")
}

// ── field-tree `Val` builders ─────────────────────────────────────────

fn cell(case: &str, payload: Option<Val>) -> Val {
    Val::Variant(case.into(), payload.map(Box::new))
}

/// A `field-tree` with the given cells and root; all side tables empty.
fn tree(cells: Vec<Val>, root: u32) -> Val {
    Val::Record(vec![
        ("cells".into(), Val::List(cells)),
        ("record-infos".into(), Val::List(vec![])),
        ("flags-infos".into(), Val::List(vec![])),
        ("enum-infos".into(), Val::List(vec![])),
        ("variant-infos".into(), Val::List(vec![])),
        ("handle-infos".into(), Val::List(vec![])),
        ("root".into(), Val::U32(root)),
    ])
}

fn field_val(name: &str, tree_val: Val) -> Val {
    Val::Record(vec![
        ("name".into(), Val::String(name.into())),
        ("tree".into(), tree_val),
    ])
}

// ── direction-keyed lookup ────────────────────────────────────────────

fn direction_of(dp: &[(String, Val)]) -> String {
    kv_map(expect_list(field(dp, "attributes")))
        .get("splicer.payload.direction")
        .cloned()
        .expect("direction attr")
        .trim_matches('"')
        .to_string()
}

/// Metrics for the export whose `payload.size` data point is `dir`.
fn metrics_for(exports: &[Val], dir: &str) -> HashMap<String, Val> {
    for rm in exports {
        let metrics = otel_metrics_by_name(rm, SCOPE);
        let dp = first_data_point(exp_histogram_record(metric(&metrics, "payload.size")));
        if direction_of(dp) == dir {
            return metrics;
        }
    }
    panic!("no export for direction {dir}");
}

fn assert_base_attrs(dp: &[(String, Val)], dir: &str) {
    let a = kv_map(expect_list(field(dp, "attributes")));
    assert_json_attr(&a, "code.namespace", TARGET_IFACE);
    assert_json_attr(&a, "code.function", TARGET_FN);
    assert_json_attr(&a, "splicer.payload.direction", dir);
}

/// The `sum` of a named single-data-point histogram metric.
fn hist_sum(metrics: &HashMap<String, Val>, name: &str) -> f64 {
    let dp = first_data_point(exp_histogram_record(metric(metrics, name)));
    f64_metric_number(field(dp, "sum"))
}

#[test]
fn payload_metrics_exports_size_shape_and_error_rate() -> Result<()> {
    let bytes = common::read_builtin("otel-payload-metrics");

    // arg: a single `text("hello")` (5 bytes, 1 node).
    let args = vec![field_val(
        "req",
        tree(vec![cell("text", Some(Val::String("hello".into())))], 0),
    )];
    // result: a `result-err` payload (1 node, one err).
    let result = Some(tree(vec![cell("result-err", Some(Val::Option(None)))], 0));

    let capture = drive_tier2_cycle::<Captures, _>(&bytes, args, result, setup)?;
    let cap = capture.lock().unwrap();
    let exports = captured(&cap, "export");
    assert_eq!(exports.len(), 2, "one export per direction (buffer=1)");

    // ── arg direction: size + structure of "hello" ──
    let arg = metrics_for(exports, "arg");
    assert_eq!(arg.len(), 7, "6 registry metrics + the type histogram");
    // canonical-ABI estimate: 8-byte string descriptor + 5 content bytes.
    assert_eq!(hist_sum(&arg, "payload.size"), 13.0, "\"hello\" = 8 + 5 bytes");
    assert_eq!(hist_sum(&arg, "payload.node.count"), 1.0, "one node");
    assert_eq!(hist_sum(&arg, "payload.depth"), 1.0, "depth 1");
    assert_base_attrs(
        first_data_point(exp_histogram_record(metric(&arg, "payload.size"))),
        "arg",
    );

    // type histogram carries a `text` kind data point.
    let kinds: HashMap<String, u64> = expect_list(field(
        sum_record(metric(&arg, "payload.cell.count")),
        "data-points",
    ))
    .iter()
    .map(|dp| {
        let r = expect_record(dp);
        let kind = kv_map(expect_list(field(r, "attributes")))
            .get("splicer.cell.kind")
            .cloned()
            .unwrap()
            .trim_matches('"')
            .to_string();
        (kind, u64_metric_number(field(r, "value")))
    })
    .collect();
    assert_eq!(kinds.get("text"), Some(&1), "one text cell; got {kinds:?}");

    // ── result direction: error count ──
    let result_metrics = metrics_for(exports, "result");
    let err_dp = first_data_point(sum_record(metric(&result_metrics, "payload.result.error")));
    assert_eq!(
        u64_metric_number(field(err_dp, "value")),
        1,
        "one result-err observed"
    );
    assert_base_attrs(err_dp, "result");

    Ok(())
}
