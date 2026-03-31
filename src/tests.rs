use crate::{parse, wac};
use cviz::model::CompositionGraph;
use cviz::parse::json;
use std::collections::HashMap;

#[test]
fn before_on_all() -> anyhow::Result<()> {
    run_all(testcases::yaml_before(), testcases::yaml_before_all_exp())
}
#[test]
fn before_noprov_on_all() -> anyhow::Result<()> {
    run_all(
        testcases::yaml_before_noprov(),
        testcases::yaml_before_noprov_all_exp(),
    )
}

#[test]
fn before_long_on_all() -> anyhow::Result<()> {
    run_all(
        testcases::yaml_before_long(),
        testcases::yaml_before_long_all_exp(),
    )
}

#[test]
fn before_nomatch_on_all() -> anyhow::Result<()> {
    run_all(
        testcases::yaml_before_nomatch(),
        testcases::yaml_before_nomatch_all_exp(),
    )
}

#[test]
fn splice_on_all() -> anyhow::Result<()> {
    run_all(testcases::yaml_splice(), testcases::yaml_splice_all_exp())
}

#[test]
fn splice_long_on_all() -> anyhow::Result<()> {
    run_all(
        testcases::yaml_splice_long(),
        testcases::yaml_splice_long_all_exp(),
    )
}

#[test]
fn splice_nomatch_on_all() -> anyhow::Result<()> {
    run_all(
        testcases::yaml_splice_nomatch(),
        testcases::yaml_splice_nomatch_all_exp(),
    )
}

#[test]
fn multi_rule_on_all() -> anyhow::Result<()> {
    run_all(
        testcases::yaml_multi_rule(),
        testcases::yaml_multi_rule_all_exp(),
    )
}

#[test]
fn alias_in_before() -> anyhow::Result<()> {
    run_all(
        testcases::yaml_alias_in_before(),
        testcases::yaml_alias_in_before_all_exp(),
    )
}

#[test]
fn alias_in_between_inner() -> anyhow::Result<()> {
    run_all(
        testcases::alias_in_between_inner(),
        testcases::alias_in_between_inner_all_exp(),
    )
}

#[test]
fn alias_in_between_outer() -> anyhow::Result<()> {
    run_all(
        testcases::alias_in_between_outer(),
        testcases::alias_in_between_outer_all_exp(),
    )
}

#[test]
fn alias_in_between_inner_and_outer() -> anyhow::Result<()> {
    run_all(
        testcases::alias_in_between_inner_and_outer(),
        testcases::alias_in_between_inner_and_outer_all_exp(),
    )
}

// --- Graph edge-case tests (priority 7 from test-plan.md) ---

#[test]
fn between_rule_on_http_does_not_affect_log_chain() -> anyhow::Result<()> {
    // A `between` rule scoped to wasi:http/handler injects between http-provider and app.
    // The wasi:logging/log chain (log-provider → app) must be untouched.
    let yaml = r#"
version: 1
rules:
  - between:
      interface: wasi:http/handler@0.3.0-rc-2026-01-06
      inner:
        name: http-provider
      outer:
        name: app
    inject:
      - name: http-middleware
"#;
    let cfg = parse::config::parse_yaml(yaml)?;
    let graph = json::parse_json_str(testcases::json_multi_interface_node())?;
    let (wac, _, _) = wac::generate_wac(
        HashMap::new(),
        "placeholder",
        &graph,
        &cfg,
        None,
        "example:composition",
    );

    // http-middleware is injected for the http chain
    assert!(
        wac.contains("let http-middleware = new my:http-middleware {"),
        "expected http-middleware instantiation"
    );
    assert!(wac.contains(r#""wasi:http/handler@0.3.0-rc-2026-01-06": http-provider["wasi:http/handler@0.3.0-rc-2026-01-06"]"#),
        "http-middleware should be wired from http-provider");
    // log-provider is instantiated independently (not wrapped by any middleware)
    assert!(
        wac.contains("let log-provider = new my:log-provider {"),
        "log-provider should be instantiated directly"
    );
    // no log-middleware or any wrapping of the log interface
    assert!(
        !wac.contains("log-middleware"),
        "log chain must not have middleware injected"
    );
    Ok(())
}

#[test]
fn before_on_http_does_not_affect_log_interface() -> anyhow::Result<()> {
    // A `before` rule on wasi:http/handler injects http-middleware before http-provider.
    // The wasi:logging/log chain must be untouched — log-provider is instantiated
    // directly and the log interface is satisfied via WAC's `...` spread in app.
    let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0-rc-2026-01-06
      provider:
        name: http-provider
    inject:
      - name: http-middleware
"#;
    let cfg = parse::config::parse_yaml(yaml)?;
    let graph = json::parse_json_str(testcases::json_multi_interface_node())?;
    let (wac, _, _) = wac::generate_wac(
        HashMap::new(),
        "placeholder",
        &graph,
        &cfg,
        None,
        "example:composition",
    );

    // http-middleware inserted before http-provider
    assert!(
        wac.contains("let http-middleware = new my:http-middleware {"),
        "expected http-middleware instantiation"
    );
    // app's explicit http wiring goes through http-middleware
    assert!(wac.contains(r#""wasi:http/handler@0.3.0-rc-2026-01-06": http-middleware["wasi:http/handler@0.3.0-rc-2026-01-06"]"#),
        "app should receive http through http-middleware");
    // log-provider is instantiated independently — the log interface is satisfied
    // via WAC's `...` spread (no explicit binding needed in the app block)
    assert!(
        wac.contains("let log-provider = new my:log-provider {"),
        "log-provider should be instantiated directly"
    );
    // http-middleware must not appear in any log-related wiring
    assert!(
        !wac.contains(r#"wasi:logging/log@0.1.0": http-middleware"#),
        "http-middleware must not be wired into the log interface"
    );
    Ok(())
}

// --- Non-http interface chain tests ---
// All existing tests use wasi:http/handler.  These verify that rules dispatch
// correctly on a completely different interface (wasi:logging/log).

#[test]
fn before_on_log_chain() -> anyhow::Result<()> {
    let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:logging/log@0.1.0
      provider:
        name: log-provider
    inject:
      - name: log-middleware
"#;
    let cfg = parse::config::parse_yaml(yaml)?;
    let graph = json::parse_json_str(testcases::json_log_short_chain())?;
    let (wac, _, _) = wac::generate_wac(
        HashMap::new(),
        "placeholder",
        &graph,
        &cfg,
        None,
        "example:composition",
    );

    let expected = r#"
package example:composition;

let log-provider = new my:log-provider {
    ...
};

let log-middleware = new my:log-middleware {
    "wasi:logging/log@0.1.0": log-provider["wasi:logging/log@0.1.0"], ...
};

let app = new my:app {
    "wasi:logging/log@0.1.0": log-middleware["wasi:logging/log@0.1.0"],
    ...
};

export app["wasi:logging/log@0.1.0"];
"#;
    assert_eq!(wac.trim(), expected.trim(), "unexpected WAC output:\n{wac}");
    Ok(())
}

#[test]
fn between_on_log_chain() -> anyhow::Result<()> {
    let yaml = r#"
version: 1
rules:
  - between:
      interface: wasi:logging/log@0.1.0
      inner:
        name: log-provider-inner
      outer:
        name: log-provider
    inject:
      - name: log-middleware
"#;
    let cfg = parse::config::parse_yaml(yaml)?;
    let graph = json::parse_json_str(testcases::json_log_long_chain())?;
    let (wac, _, _) = wac::generate_wac(
        HashMap::new(),
        "placeholder",
        &graph,
        &cfg,
        None,
        "example:composition",
    );

    let expected = r#"
package example:composition;

let log-provider-inner = new my:log-provider-inner {
    ...
};

let log-middleware = new my:log-middleware {
    "wasi:logging/log@0.1.0": log-provider-inner["wasi:logging/log@0.1.0"], ...
};

let log-provider = new my:log-provider {
    "wasi:logging/log@0.1.0": log-middleware["wasi:logging/log@0.1.0"],
    ...
};

let app = new my:app {
    "wasi:logging/log@0.1.0": log-provider["wasi:logging/log@0.1.0"],
    ...
};

export app["wasi:logging/log@0.1.0"];
"#;
    assert_eq!(wac.trim(), expected.trim(), "unexpected WAC output:\n{wac}");
    Ok(())
}

#[test]
fn http_rule_does_not_inject_into_log_chain() -> anyhow::Result<()> {
    // A rule targeting wasi:http/handler must produce no effect on a pure log graph.
    let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0-rc-2026-01-06
    inject:
      - name: http-middleware
"#;
    let cfg = parse::config::parse_yaml(yaml)?;
    let graph = json::parse_json_str(testcases::json_log_short_chain())?;
    let (wac, _, _) = wac::generate_wac(
        HashMap::new(),
        "placeholder",
        &graph,
        &cfg,
        None,
        "example:composition",
    );

    assert!(
        !wac.contains("http-middleware"),
        "http rule must not affect a log-only graph"
    );
    assert!(
        wac.contains("let log-provider = new my:log-provider {"),
        "log-provider should still be instantiated"
    );
    assert!(
        wac.contains(r#"export app["wasi:logging/log@0.1.0"];"#),
        "log export must still be present"
    );
    Ok(())
}

// --- Typed-graph tests (priority 4 from test-plan.md) ---
// These use the same YAML configs and expected WAC outputs as the untyped
// variants, but the parsed graphs are post-processed to carry a fake
// fingerprint on the `wasi:http/handler` chain.  The middleware has no
// path (path: None), so `validate_contract` emits a Warn and proceeds;
// the generated WAC must be identical to the untyped result.

#[test]
fn before_on_all_typed() -> anyhow::Result<()> {
    run_all_typed(testcases::yaml_before(), testcases::yaml_before_all_exp())
}

#[test]
fn before_noprov_on_all_typed() -> anyhow::Result<()> {
    run_all_typed(
        testcases::yaml_before_noprov(),
        testcases::yaml_before_noprov_all_exp(),
    )
}

#[test]
fn splice_on_all_typed() -> anyhow::Result<()> {
    run_all_typed(testcases::yaml_splice(), testcases::yaml_splice_all_exp())
}

// --- P8: validate → generate integration test ---

#[test]
fn warn_is_non_blocking_and_wac_still_generated() -> anyhow::Result<()> {
    // Typed graph + middleware with path: None.
    // validate_contract produces one Warn per middleware (can't verify without a path),
    // but the plan still proceeds and the WAC is emitted.
    let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:logging/log@0.1.0
      provider:
        name: log-provider
    inject:
      - name: log-middleware
      - name: log-middleware-2
"#;
    let cfg = parse::config::parse_yaml(yaml)?;
    let mut graph = json::parse_json_str(testcases::json_log_short_chain())?;
    add_chain_fingerprint(&mut graph, "wasi:logging/log@0.1.0", "fake-fp-xyz");

    let (wac, _, diagnostics) = wac::generate_wac(
        HashMap::new(),
        "placeholder",
        &graph,
        &cfg,
        None,
        "example:composition",
    );

    // WAC is still generated — the Warn is advisory only
    assert!(
        wac.contains("let log-middleware = new my:log-middleware {"),
        "WAC must be generated even when contract validation warns"
    );
    assert!(
        wac.contains("let log-middleware-2 = new my:log-middleware-2 {"),
        "second middleware must also be injected"
    );

    // One Warn per middleware injection (2 middlewares, no paths → 2 Warns)
    let warns: Vec<_> = diagnostics
        .iter()
        .filter(|d| matches!(d, crate::contract::ContractResult::Warn(_)))
        .collect();
    assert_eq!(
        warns.len(),
        2,
        "expected one Warn per middleware without a path, got: {diagnostics:?}"
    );

    // No errors or unexpected Ok results
    assert!(
        diagnostics
            .iter()
            .all(|d| matches!(d, crate::contract::ContractResult::Warn(_))),
        "all diagnostics should be Warn, got: {diagnostics:?}"
    );

    Ok(())
}

fn run_all(yaml: &str, exp: HashMap<String, String>) -> anyhow::Result<()> {
    let cfg = parse::config::parse_yaml(yaml)?;

    let mut graphs = HashMap::new();
    let all_json = testcases::get_all_json();
    for (name, json) in all_json {
        graphs.insert(name.clone(), json::parse_json_str(&json)?);
    }

    for (name, graph) in graphs.iter() {
        let (wac, _, _) = wac::generate_wac(
            HashMap::new(),
            "placeholder",
            graph,
            &cfg,
            None,
            "example:composition",
        );
        let exp_wac = exp.get(name).unwrap_or_else(|| {
            panic!("Test setup incorrect, should be able to find expected result for name '{name}'")
        });

        assert_eq!(wac.trim(), exp_wac.trim(),
            "Failed on test '{name}', for the following config:{yaml}\nGot the following result:{wac}"
        );
    }
    Ok(())
}

/// Set a stable fake fingerprint on all non-host imports and exports whose
/// interface name matches `interface_name`.  This simulates type-annotated
/// graphs without needing real WASM parsing.
fn add_chain_fingerprint(graph: &mut CompositionGraph, interface_name: &str, fingerprint: &str) {
    for node in graph.nodes.values_mut() {
        for conn in node.imports.iter_mut() {
            if !conn.is_host_import && conn.interface_name == interface_name {
                conn.fingerprint = Some(fingerprint.to_string());
            }
        }
    }
    if let Some(export) = graph.component_exports.get_mut(interface_name) {
        export.fingerprint = Some(fingerprint.to_string());
    }
}

fn run_all_typed(yaml: &str, exp: HashMap<String, String>) -> anyhow::Result<()> {
    let cfg = parse::config::parse_yaml(yaml)?;
    const IFACE: &str = "wasi:http/handler@0.3.0-rc-2026-01-06";
    const FP: &str = "fake-fingerprint-abc123";

    let mut graphs = HashMap::new();
    let all_json = testcases::get_all_json();
    for (name, json_str) in all_json {
        let mut graph = json::parse_json_str(&json_str)?;
        add_chain_fingerprint(&mut graph, IFACE, FP);
        graphs.insert(name.clone(), graph);
    }

    for (name, graph) in graphs.iter() {
        let (wac, _, _) = wac::generate_wac(
            HashMap::new(),
            "placeholder",
            graph,
            &cfg,
            None,
            "example:composition",
        );
        let exp_wac = exp.get(name).unwrap_or_else(|| {
            panic!("Test setup incorrect, should be able to find expected result for name '{name}'")
        });

        assert_eq!(
            wac.trim(),
            exp_wac.trim(),
            "Failed on test '{name}', for the following config:{yaml}\nGot the following result:{wac}"
        );
    }
    Ok(())
}

// ── Shim roundtrip test ───────────────────────────────────────────────────────
// Exercises the full splice pipeline on a real composed Wasm binary that
// contains an internal shim sub-component.  Unlike the JSON-fixture tests
// above, this one writes actual bytes to disk so split_out_composition can
// identify shim nodes via its heuristic, then verifies that generate_wac
// omits those spurious shim-sourced graph-level exports.

#[test]
fn shim_exports_not_in_splice_roundtrip_wac() -> anyhow::Result<()> {
    use crate::split::split_out_composition;
    use cviz::parse::component::parse_component;

    // A composed binary whose root component exports two things:
    //   - "my:service/handler@0.1.0" from $svc-inst  ← legitimate
    //   - "my:shim/iface@0.1.0"     from $shim-inst  ← spurious (simulates
    //     what wac compose produces when an inner component's shim becomes
    //     visible as a peer-level node after flattening)
    //
    // $shim has no core module → split.rs marks it as a shim.
    // $service has a core module → split.rs treats it as a real component.
    let wat = r#"(component
        (component $shim
            (import "host:env/dep@0.1.0" (instance $dep
                (export "get" (func (result u32)))
            ))
            (export "my:shim/iface@0.1.0" (instance $dep))
        )
        (component $service
            (import "my:shim/iface@0.1.0" (instance $iface
                (export "get" (func (result u32)))
            ))
            (core module $m
                (func (export "run") (result i32) i32.const 42)
            )
            (core instance $mi (instantiate $m))
            (alias export $iface "get" (func $get))
            (instance $h-out (export "run" (func $get)))
            (export "my:service/handler@0.1.0" (instance $h-out))
        )
        (import "host:env/dep@0.1.0" (instance $host-dep
            (export "get" (func (result u32)))
        ))
        (instance $shim-inst (instantiate $shim
            (with "host:env/dep@0.1.0" (instance $host-dep))
        ))
        (instance $svc-inst (instantiate $service
            (with "my:shim/iface@0.1.0" (instance $shim-inst "my:shim/iface@0.1.0"))
        ))
        (export "my:service/handler@0.1.0" (instance $svc-inst "my:service/handler@0.1.0"))
        (export "my:shim/iface@0.1.0" (instance $shim-inst "my:shim/iface@0.1.0"))
    )"#;

    let bytes = wat::parse_str(wat).expect("failed to parse WAT");

    // Write the composed binary and a splits dir to a deterministic temp location.
    let tmp = std::env::temp_dir().join("splicer_shim_roundtrip");
    std::fs::create_dir_all(&tmp)?;
    let wasm_path = tmp.join("composed.wasm");
    let splits_dir = tmp.join("splits");
    std::fs::write(&wasm_path, &bytes)?;

    let (splits_path, shim_comps) =
        split_out_composition(&wasm_path, &Some(splits_dir.to_string_lossy().into_owned()))?;

    // shim_comps must be non-empty — if it's empty the filter is a no-op and
    // the test would pass vacuously.
    assert!(
        !shim_comps.is_empty(),
        "expected split.rs to detect at least one shim component; \
         check the WAT — the shim sub-component must have no core module"
    );

    let graph = parse_component(&bytes).expect("failed to parse composed binary");

    let (wac, _, _) = wac::generate_wac(shim_comps, &splits_path, &graph, &[], None, "test:pkg");

    // The service's interface MUST appear as a graph-level export statement.
    let has_service_export = wac
        .lines()
        .any(|l| l.trim().starts_with("export") && l.contains("my:service/handler@0.1.0"));
    assert!(
        has_service_export,
        "expected 'export …[\"my:service/handler@0.1.0\"];' in WAC:\n{wac}"
    );

    // The shim's interface MUST NOT appear as a graph-level export statement.
    // Before the fix this produced: export shim-iface["my:shim/iface@0.1.0"];
    let has_shim_export = wac
        .lines()
        .any(|l| l.trim().starts_with("export") && l.contains("my:shim/iface@0.1.0"));
    assert!(
        !has_shim_export,
        "shim interface must not appear as a WAC-level export:\n{wac}"
    );

    std::fs::remove_dir_all(&tmp).ok();
    Ok(())
}

mod testcases {
    use std::collections::HashMap;

    const ONE: &str = "one";
    const SHORT: &str = "short";
    const LONG: &str = "long";

    fn json_one_service() -> &'static str {
        // service-b.json
        r#"
        {
          "version": 1,
          "nodes": [
            {
              "id": 12,
              "name": "srv-b",
              "component_index": 0,
              "component_num": 0,
              "imports": [
                {
                  "interface": "import-func-handle",
                  "short": "import-func-handle",
                  "source_instance": 16,
                  "is_host_import": true
                },
                {
                  "interface": "import-type-request",
                  "short": "import-type-request",
                  "source_instance": 37,
                  "is_host_import": true
                },
                {
                  "interface": "import-type-response",
                  "short": "import-type-response",
                  "source_instance": 38,
                  "is_host_import": true
                },
                {
                  "interface": "import-type-DNS-error-payload",
                  "short": "import-type-DNS-error-payload",
                  "source_instance": 39,
                  "is_host_import": true
                },
                {
                  "interface": "import-type-TLS-alert-received-payload",
                  "short": "import-type-TLS-alert-received-payload",
                  "source_instance": 40,
                  "is_host_import": true
                },
                {
                  "interface": "import-type-field-size-payload",
                  "short": "import-type-field-size-payload",
                  "source_instance": 41,
                  "is_host_import": true
                },
                {
                  "interface": "import-type-error-code",
                  "short": "import-type-error-code",
                  "source_instance": 42,
                  "is_host_import": true
                },
                {
                  "interface": "import-type-request0",
                  "short": "import-type-request0",
                  "source_instance": 23,
                  "is_host_import": true
                },
                {
                  "interface": "import-type-response0",
                  "short": "import-type-response0",
                  "source_instance": 24,
                  "is_host_import": true
                },
                {
                  "interface": "import-type-error-code0",
                  "short": "import-type-error-code0",
                  "source_instance": 25,
                  "is_host_import": true
                }
              ]
            }
          ],
          "exports": [
            {
              "interface": "wasi:http/handler@0.3.0-rc-2026-01-06",
              "source_instance": 12
            }
          ]
        }
        "#
    }
    fn json_short_chain() -> &'static str {
        // short-chain.json
        r#"
        {
          "version": 1,
          "nodes": [
            {
              "id": 11,
              "name": "srv-b",
              "component_index": 0,
              "component_num": 0,
              "imports": [
                {
                  "interface": "wasi:http/types@0.3.0-rc-2026-01-06",
                  "short": "types",
                  "source_instance": 0,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/environment@0.2.6",
                  "short": "environment",
                  "source_instance": 1,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/exit@0.2.6",
                  "short": "exit",
                  "source_instance": 2,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:io/error@0.2.6",
                  "short": "error",
                  "source_instance": 3,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:io/streams@0.2.6",
                  "short": "streams",
                  "source_instance": 4,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stdin@0.2.6",
                  "short": "stdin",
                  "source_instance": 5,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stdout@0.2.6",
                  "short": "stdout",
                  "source_instance": 6,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stderr@0.2.6",
                  "short": "stderr",
                  "source_instance": 7,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:clocks/wall-clock@0.2.6",
                  "short": "wall-clock",
                  "source_instance": 8,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:filesystem/types@0.2.6",
                  "short": "types",
                  "source_instance": 9,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:filesystem/preopens@0.2.6",
                  "short": "preopens",
                  "source_instance": 10,
                  "is_host_import": true
                }
              ]
            },
            {
              "id": 13,
              "name": "srv",
              "component_index": 1,
              "component_num": 1,
              "imports": [
                {
                  "interface": "wasi:http/handler@0.3.0-rc-2026-01-06",
                  "short": "handler",
                  "source_instance": 11,
                  "is_host_import": false
                },
                {
                  "interface": "wasi:http/types@0.3.0-rc-2026-01-06",
                  "short": "types",
                  "source_instance": 0,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/environment@0.2.6",
                  "short": "environment",
                  "source_instance": 1,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/exit@0.2.6",
                  "short": "exit",
                  "source_instance": 2,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:io/error@0.2.6",
                  "short": "error",
                  "source_instance": 3,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:io/streams@0.2.6",
                  "short": "streams",
                  "source_instance": 4,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stdin@0.2.6",
                  "short": "stdin",
                  "source_instance": 5,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stdout@0.2.6",
                  "short": "stdout",
                  "source_instance": 6,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stderr@0.2.6",
                  "short": "stderr",
                  "source_instance": 7,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:clocks/wall-clock@0.2.6",
                  "short": "wall-clock",
                  "source_instance": 8,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:filesystem/types@0.2.6",
                  "short": "types",
                  "source_instance": 9,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:filesystem/preopens@0.2.6",
                  "short": "preopens",
                  "source_instance": 10,
                  "is_host_import": true
                }
              ]
            }
          ],
          "exports": [
            {
              "interface": "wasi:http/handler@0.3.0-rc-2026-01-06",
              "source_instance": 13
            }
          ]
        }
        "#
    }
    fn json_long_chain() -> &'static str {
        // long-chain.json
        r#"
        {
          "version": 1,
          "nodes": [
            {
              "id": 11,
              "name": "srv-c",
              "component_index": 0,
              "component_num": 0,
              "imports": [
                {
                  "interface": "wasi:http/types@0.3.0-rc-2026-01-06",
                  "short": "types",
                  "source_instance": 0,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/environment@0.2.6",
                  "short": "environment",
                  "source_instance": 1,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/exit@0.2.6",
                  "short": "exit",
                  "source_instance": 2,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:io/error@0.2.6",
                  "short": "error",
                  "source_instance": 3,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:io/streams@0.2.6",
                  "short": "streams",
                  "source_instance": 4,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stdin@0.2.6",
                  "short": "stdin",
                  "source_instance": 5,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stdout@0.2.6",
                  "short": "stdout",
                  "source_instance": 6,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stderr@0.2.6",
                  "short": "stderr",
                  "source_instance": 7,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:clocks/wall-clock@0.2.6",
                  "short": "wall-clock",
                  "source_instance": 8,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:filesystem/types@0.2.6",
                  "short": "types",
                  "source_instance": 9,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:filesystem/preopens@0.2.6",
                  "short": "preopens",
                  "source_instance": 10,
                  "is_host_import": true
                }
              ]
            },
            {
              "id": 12,
              "name": "srv-b",
              "component_index": 1,
              "component_num": 1,
              "imports": [
                {
                  "interface": "wasi:http/handler@0.3.0-rc-2026-01-06",
                  "short": "handler",
                  "source_instance": 11,
                  "is_host_import": false
                },
                {
                  "interface": "wasi:http/types@0.3.0-rc-2026-01-06",
                  "short": "types",
                  "source_instance": 0,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/environment@0.2.6",
                  "short": "environment",
                  "source_instance": 1,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/exit@0.2.6",
                  "short": "exit",
                  "source_instance": 2,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:io/error@0.2.6",
                  "short": "error",
                  "source_instance": 3,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:io/streams@0.2.6",
                  "short": "streams",
                  "source_instance": 4,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stdin@0.2.6",
                  "short": "stdin",
                  "source_instance": 5,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stdout@0.2.6",
                  "short": "stdout",
                  "source_instance": 6,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stderr@0.2.6",
                  "short": "stderr",
                  "source_instance": 7,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:clocks/wall-clock@0.2.6",
                  "short": "wall-clock",
                  "source_instance": 8,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:filesystem/types@0.2.6",
                  "short": "types",
                  "source_instance": 9,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:filesystem/preopens@0.2.6",
                  "short": "preopens",
                  "source_instance": 10,
                  "is_host_import": true
                }
              ]
            },
            {
              "id": 13,
              "name": "srv",
              "component_index": 2,
              "component_num": 2,
              "imports": [
                {
                  "interface": "wasi:http/handler@0.3.0-rc-2026-01-06",
                  "short": "handler",
                  "source_instance": 12,
                  "is_host_import": false
                },
                {
                  "interface": "wasi:http/types@0.3.0-rc-2026-01-06",
                  "short": "types",
                  "source_instance": 0,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/environment@0.2.6",
                  "short": "environment",
                  "source_instance": 1,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/exit@0.2.6",
                  "short": "exit",
                  "source_instance": 2,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:io/error@0.2.6",
                  "short": "error",
                  "source_instance": 3,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:io/streams@0.2.6",
                  "short": "streams",
                  "source_instance": 4,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stdin@0.2.6",
                  "short": "stdin",
                  "source_instance": 5,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stdout@0.2.6",
                  "short": "stdout",
                  "source_instance": 6,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:cli/stderr@0.2.6",
                  "short": "stderr",
                  "source_instance": 7,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:clocks/wall-clock@0.2.6",
                  "short": "wall-clock",
                  "source_instance": 8,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:filesystem/types@0.2.6",
                  "short": "types",
                  "source_instance": 9,
                  "is_host_import": true
                },
                {
                  "interface": "wasi:filesystem/preopens@0.2.6",
                  "short": "preopens",
                  "source_instance": 10,
                  "is_host_import": true
                }
              ]
            }
          ],
          "exports": [
            {
              "interface": "wasi:http/handler@0.3.0-rc-2026-01-06",
              "source_instance": 13
            }
          ]
        }
        "#
    }
    pub fn get_all_json() -> HashMap<String, String> {
        HashMap::from_iter(vec![
            (ONE.to_string(), json_one_service().to_string()),
            (SHORT.to_string(), json_short_chain().to_string()),
            (LONG.to_string(), json_long_chain().to_string()),
        ])
    }
    fn wac_one_identity() -> &'static str {
        r#"
package example:composition;

let srv-b = new my:srv-b {
    ...
};

export srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn wac_short_identity() -> &'static str {
        r#"
package example:composition;

let srv-b = new my:srv-b {
    ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export srv["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn wac_long_identity() -> &'static str {
        r#"
package example:composition;

let srv-c = new my:srv-c {
    ...
};

let srv-b = new my:srv-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-c["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export srv["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn wac_all_identities() -> HashMap<String, String> {
        HashMap::from_iter(vec![
            (ONE.to_string(), wac_one_identity().to_string()),
            (SHORT.to_string(), wac_short_identity().to_string()),
            (LONG.to_string(), wac_long_identity().to_string()),
        ])
    }
    pub fn yaml_before() -> &'static str {
        // before.yaml
        r#"
        version: 1

        rules:
          - before:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              provider:
                name: srv-b
            inject:
            - name: middleware-a
            - name: middleware-b
        "#
    }
    fn yaml_before_on_one_exp() -> &'static str {
        r#"
package example:composition;

let srv-b = new my:srv-b {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

export middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn yaml_before_on_short_exp() -> &'static str {
        r#"
package example:composition;

let srv-b = new my:srv-b {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export srv["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn yaml_before_on_long_exp() -> &'static str {
        r#"
package example:composition;

let srv-c = new my:srv-c {
    ...
};

let srv-b = new my:srv-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-c["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export srv["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    pub fn yaml_before_all_exp() -> HashMap<String, String> {
        HashMap::from_iter(vec![
            (ONE.to_string(), yaml_before_on_one_exp().to_string()),
            (SHORT.to_string(), yaml_before_on_short_exp().to_string()),
            (LONG.to_string(), yaml_before_on_long_exp().to_string()),
        ])
    }
    pub fn yaml_before_noprov() -> &'static str {
        // before-noprov.yaml
        r#"
    version: 1

    rules:
      - before:
          interface: wasi:http/handler@0.3.0-rc-2026-01-06
        inject:
        - name: middleware-a
        - name: middleware-b
    "#
    }
    fn yaml_before_noprov_on_one_exp() -> &'static str {
        r#"
package example:composition;

let srv-b = new my:srv-b {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

export middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn yaml_before_noprov_on_short_exp() -> &'static str {
        r#"
package example:composition;

let srv-b = new my:srv-b {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

export middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn yaml_before_noprov_on_long_exp() -> &'static str {
        r#"
package example:composition;

let srv-c = new my:srv-c {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-c["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv-b = new my:srv-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

export middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    pub fn yaml_before_noprov_all_exp() -> HashMap<String, String> {
        HashMap::from_iter(vec![
            (ONE.to_string(), yaml_before_noprov_on_one_exp().to_string()),
            (
                SHORT.to_string(),
                yaml_before_noprov_on_short_exp().to_string(),
            ),
            (
                LONG.to_string(),
                yaml_before_noprov_on_long_exp().to_string(),
            ),
        ])
    }
    pub fn yaml_before_long() -> &'static str {
        // before-long.yaml
        r#"
        version: 1

        rules:
          - before:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              provider:
                name: srv-c
            inject:
            - name: middleware-a
            - name: middleware-b
        "#
    }
    fn yaml_before_long_on_one_exp() -> &'static str {
        wac_one_identity()
    }
    fn yaml_before_long_on_short_exp() -> &'static str {
        wac_short_identity()
    }
    fn yaml_before_long_on_long_exp() -> &'static str {
        r#"
package example:composition;

let srv-c = new my:srv-c {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-c["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv-b = new my:srv-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export srv["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    pub fn yaml_before_long_all_exp() -> HashMap<String, String> {
        HashMap::from_iter(vec![
            (ONE.to_string(), yaml_before_long_on_one_exp().to_string()),
            (
                SHORT.to_string(),
                yaml_before_long_on_short_exp().to_string(),
            ),
            (LONG.to_string(), yaml_before_long_on_long_exp().to_string()),
        ])
    }
    pub fn yaml_before_nomatch() -> &'static str {
        // before-nomatch.yaml
        r#"
        version: 1

        rules:
          - before:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              provider:
                name: srv-NA
            inject:
            - name: middleware-a
            - name: middleware-b
        "#
    }
    pub fn yaml_before_nomatch_all_exp() -> HashMap<String, String> {
        wac_all_identities()
    }
    pub fn yaml_splice() -> &'static str {
        // splice.yaml
        r#"
        version: 1

        rules:
          - between:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              inner:
                name: srv-b
              outer:
                name: srv
            inject:
            - name: middleware-a
            - name: middleware-b
        "#
    }
    fn yaml_splice_on_one_exp() -> &'static str {
        wac_one_identity()
    }
    fn yaml_splice_on_short_exp() -> &'static str {
        r#"
package example:composition;

let srv-b = new my:srv-b {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export srv["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn yaml_splice_on_long_exp() -> &'static str {
        r#"
package example:composition;

let srv-c = new my:srv-c {
    ...
};

let srv-b = new my:srv-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-c["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export srv["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    pub fn yaml_splice_all_exp() -> HashMap<String, String> {
        HashMap::from_iter(vec![
            (ONE.to_string(), yaml_splice_on_one_exp().to_string()),
            (SHORT.to_string(), yaml_splice_on_short_exp().to_string()),
            (LONG.to_string(), yaml_splice_on_long_exp().to_string()),
        ])
    }
    pub fn yaml_splice_long() -> &'static str {
        // splice-long.yaml
        r#"
        version: 1

        rules:
          - between:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              inner:
                name: srv-c
              outer:
                name: srv-b
            inject:
            - name: middleware-a
            - name: middleware-b

        "#
    }
    fn yaml_splice_long_on_one_exp() -> &'static str {
        wac_one_identity()
    }
    fn yaml_splice_long_on_short_exp() -> &'static str {
        wac_short_identity()
    }
    fn yaml_splice_long_on_long_exp() -> &'static str {
        r#"
package example:composition;

let srv-c = new my:srv-c {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-c["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv-b = new my:srv-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export srv["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    pub fn yaml_splice_long_all_exp() -> HashMap<String, String> {
        HashMap::from_iter(vec![
            (ONE.to_string(), yaml_splice_long_on_one_exp().to_string()),
            (
                SHORT.to_string(),
                yaml_splice_long_on_short_exp().to_string(),
            ),
            (LONG.to_string(), yaml_splice_long_on_long_exp().to_string()),
        ])
    }
    pub fn yaml_splice_nomatch() -> &'static str {
        // splice-nomatch.yaml
        r#"
        version: 1

        rules:
          - between:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              inner:
                name: srv-NA
              outer:
                name: srv
            inject:
            - name: middleware-a
            - name: middleware-b
        "#
    }
    pub fn yaml_splice_nomatch_all_exp() -> HashMap<String, String> {
        wac_all_identities()
    }
    pub fn yaml_multi_rule() -> &'static str {
        r#"
        version: 1

        rules:
          - before:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
            inject:
            - name: middleware-a
          - before:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              provider:
                name: srv-b
            inject:
            - name: middleware-b
            - name: middleware-c
          - between:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              inner:
                name: srv-c
              outer:
                name: srv-b
            inject:
            - name: middleware-d
            - name: middleware-e
        "#
    }
    fn yaml_multi_rule_on_one_exp() -> &'static str {
        r#"
package example:composition;

let srv-b = new my:srv-b {
    ...
};

let middleware-c = new my:middleware-c {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-c["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

export middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn yaml_multi_rule_on_short_exp() -> &'static str {
        r#"
package example:composition;

let srv-b = new my:srv-b {
    ...
};

let middleware-c = new my:middleware-c {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-c["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

export middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn yaml_multi_rule_on_long_exp() -> &'static str {
        r#"
package example:composition;

let srv-c = new my:srv-c {
    ...
};

let middleware-e = new my:middleware-e {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-c["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-d = new my:middleware-d {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-e["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-d["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv-b = new my:srv-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let middleware-c = new my:middleware-c {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-c["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

export middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    pub fn yaml_multi_rule_all_exp() -> HashMap<String, String> {
        HashMap::from_iter(vec![
            (ONE.to_string(), yaml_multi_rule_on_one_exp().to_string()),
            (
                SHORT.to_string(),
                yaml_multi_rule_on_short_exp().to_string(),
            ),
            (LONG.to_string(), yaml_multi_rule_on_long_exp().to_string()),
        ])
    }
    pub fn yaml_alias_in_before() -> &'static str {
        // before.yaml
        r#"
        version: 1

        rules:
          - before:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              provider:
                name: srv-b
                alias: other-name
            inject:
            - name: middleware-a
            - name: middleware-b
        "#
    }
    fn yaml_alias_in_before_on_one_exp() -> &'static str {
        r#"
package example:composition;

let other-name = new my:other-name {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": other-name["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

export middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn yaml_alias_in_before_on_short_exp() -> &'static str {
        r#"
package example:composition;

let other-name = new my:other-name {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": other-name["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export srv["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn yaml_alias_in_before_on_long_exp() -> &'static str {
        r#"
package example:composition;

let srv-c = new my:srv-c {
    ...
};

let other-name = new my:other-name {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-c["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": other-name["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export srv["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    pub fn yaml_alias_in_before_all_exp() -> HashMap<String, String> {
        HashMap::from_iter(vec![
            (
                ONE.to_string(),
                yaml_alias_in_before_on_one_exp().to_string(),
            ),
            (
                SHORT.to_string(),
                yaml_alias_in_before_on_short_exp().to_string(),
            ),
            (
                LONG.to_string(),
                yaml_alias_in_before_on_long_exp().to_string(),
            ),
        ])
    }
    pub fn alias_in_between_inner() -> &'static str {
        r#"
        version: 1

        rules:
          - between:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              inner:
                name: srv-b
                alias: other-b
              outer:
                name: srv
            inject:
            - name: middleware-a
            - name: middleware-b
        "#
    }
    fn alias_in_between_inner_on_one_exp() -> &'static str {
        wac_one_identity()
    }
    fn alias_in_between_inner_on_short_exp() -> &'static str {
        r#"
package example:composition;

let other-b = new my:other-b {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": other-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export srv["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn alias_in_between_inner_on_long_exp() -> &'static str {
        r#"
package example:composition;

let srv-c = new my:srv-c {
    ...
};

let other-b = new my:other-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-c["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": other-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let srv = new my:srv {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export srv["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    pub fn alias_in_between_inner_all_exp() -> HashMap<String, String> {
        HashMap::from_iter(vec![
            (
                ONE.to_string(),
                alias_in_between_inner_on_one_exp().to_string(),
            ),
            (
                SHORT.to_string(),
                alias_in_between_inner_on_short_exp().to_string(),
            ),
            (
                LONG.to_string(),
                alias_in_between_inner_on_long_exp().to_string(),
            ),
        ])
    }
    pub fn alias_in_between_outer() -> &'static str {
        r#"
        version: 1

        rules:
          - between:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              inner:
                name: srv-b
              outer:
                name: srv
                alias: other
            inject:
            - name: middleware-a
            - name: middleware-b
        "#
    }
    fn alias_in_between_outer_on_one_exp() -> &'static str {
        wac_one_identity()
    }
    fn alias_in_between_outer_on_short_exp() -> &'static str {
        r#"
package example:composition;

let srv-b = new my:srv-b {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let other = new my:other {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export other["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn alias_in_between_outer_on_long_exp() -> &'static str {
        r#"
package example:composition;

let srv-c = new my:srv-c {
    ...
};

let srv-b = new my:srv-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-c["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let other = new my:other {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export other["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    pub fn alias_in_between_outer_all_exp() -> HashMap<String, String> {
        HashMap::from_iter(vec![
            (
                ONE.to_string(),
                alias_in_between_outer_on_one_exp().to_string(),
            ),
            (
                SHORT.to_string(),
                alias_in_between_outer_on_short_exp().to_string(),
            ),
            (
                LONG.to_string(),
                alias_in_between_outer_on_long_exp().to_string(),
            ),
        ])
    }
    pub fn alias_in_between_inner_and_outer() -> &'static str {
        r#"
        version: 1

        rules:
          - between:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              inner:
                name: srv-b
                alias: other-b
              outer:
                name: srv
                alias: other
            inject:
            - name: middleware-a
            - name: middleware-b
        "#
    }
    fn alias_in_between_inner_and_outer_on_one_exp() -> &'static str {
        wac_one_identity()
    }
    fn alias_in_between_inner_and_outer_on_short_exp() -> &'static str {
        r#"
package example:composition;

let other-b = new my:other-b {
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": other-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let other = new my:other {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export other["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    fn alias_in_between_inner_and_outer_on_long_exp() -> &'static str {
        r#"
package example:composition;

let srv-c = new my:srv-c {
    ...
};

let other-b = new my:other-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": srv-c["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

let middleware-b = new my:middleware-b {
    "wasi:http/handler@0.3.0-rc-2026-01-06": other-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let middleware-a = new my:middleware-a {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-b["wasi:http/handler@0.3.0-rc-2026-01-06"], ...
};

let other = new my:other {
    "wasi:http/handler@0.3.0-rc-2026-01-06": middleware-a["wasi:http/handler@0.3.0-rc-2026-01-06"],
    ...
};

export other["wasi:http/handler@0.3.0-rc-2026-01-06"];
        "#
    }
    pub fn alias_in_between_inner_and_outer_all_exp() -> HashMap<String, String> {
        HashMap::from_iter(vec![
            (
                ONE.to_string(),
                alias_in_between_inner_and_outer_on_one_exp().to_string(),
            ),
            (
                SHORT.to_string(),
                alias_in_between_inner_and_outer_on_short_exp().to_string(),
            ),
            (
                LONG.to_string(),
                alias_in_between_inner_and_outer_on_long_exp().to_string(),
            ),
        ])
    }

    // --- Edge-case fixtures (P7) ---

    /// A two-node chain using `wasi:logging/log@0.1.0` instead of `wasi:http/handler`.
    /// log-provider exports the log interface; app consumes it.
    pub fn json_log_short_chain() -> &'static str {
        r#"
        {
          "version": 1,
          "nodes": [
            {
              "id": 11,
              "name": "log-provider",
              "component_index": 0,
              "component_num": 0,
              "imports": []
            },
            {
              "id": 13,
              "name": "app",
              "component_index": 1,
              "component_num": 1,
              "imports": [
                {
                  "interface": "wasi:logging/log@0.1.0",
                  "short": "log",
                  "source_instance": 11,
                  "is_host_import": false
                }
              ]
            }
          ],
          "exports": [
            {
              "interface": "wasi:logging/log@0.1.0",
              "source_instance": 13
            }
          ]
        }
        "#
    }

    /// A three-node chain using `wasi:logging/log@0.1.0`:
    /// log-provider-inner → log-provider → app
    pub fn json_log_long_chain() -> &'static str {
        r#"
        {
          "version": 1,
          "nodes": [
            {
              "id": 11,
              "name": "log-provider-inner",
              "component_index": 0,
              "component_num": 0,
              "imports": []
            },
            {
              "id": 12,
              "name": "log-provider",
              "component_index": 1,
              "component_num": 1,
              "imports": [
                {
                  "interface": "wasi:logging/log@0.1.0",
                  "short": "log",
                  "source_instance": 11,
                  "is_host_import": false
                }
              ]
            },
            {
              "id": 13,
              "name": "app",
              "component_index": 2,
              "component_num": 2,
              "imports": [
                {
                  "interface": "wasi:logging/log@0.1.0",
                  "short": "log",
                  "source_instance": 12,
                  "is_host_import": false
                }
              ]
            }
          ],
          "exports": [
            {
              "interface": "wasi:logging/log@0.1.0",
              "source_instance": 13
            }
          ]
        }
        "#
    }

    /// A single consumer (app) that imports two different interfaces from two
    /// different providers.  Both exports are surfaced.
    pub fn json_multi_interface_node() -> &'static str {
        r#"
        {
          "version": 1,
          "nodes": [
            {
              "id": 201,
              "name": "http-provider",
              "component_index": 0,
              "component_num": 0,
              "imports": []
            },
            {
              "id": 202,
              "name": "log-provider",
              "component_index": 1,
              "component_num": 1,
              "imports": []
            },
            {
              "id": 203,
              "name": "app",
              "component_index": 2,
              "component_num": 2,
              "imports": [
                {
                  "interface": "wasi:http/handler@0.3.0-rc-2026-01-06",
                  "short": "handler",
                  "source_instance": 201,
                  "is_host_import": false
                },
                {
                  "interface": "wasi:logging/log@0.1.0",
                  "short": "log",
                  "source_instance": 202,
                  "is_host_import": false
                }
              ]
            }
          ],
          "exports": [
            {
              "interface": "wasi:http/handler@0.3.0-rc-2026-01-06",
              "source_instance": 203
            },
            {
              "interface": "wasi:logging/log@0.1.0",
              "source_instance": 203
            }
          ]
        }
        "#
    }
}
