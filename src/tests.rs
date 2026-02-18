use crate::{parse, wac};
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

fn run_all(yaml: &str, exp: HashMap<String, String>) -> anyhow::Result<()> {
    let cfg = parse::config::parse_yaml(yaml)?;

    let mut graphs = HashMap::new();
    let all_json = testcases::get_all_json();
    for (name, json) in all_json {
        graphs.insert(name.clone(), json::parse_json_str(&json)?);
    }

    for (name, graph) in graphs.iter() {
        let wac = wac::generate_wac(graph, &cfg);
        let exp_wac = exp.get(name).unwrap_or_else(|| {
            panic!("Test setup incorrect, should be able to find expected result for name '{name}'")
        });

        assert_eq!(wac.trim(), exp_wac.trim(),
            "Failed on test '{name}', for the following config:{yaml}\nGot the following result:{wac}"
        );
    }
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
            - middleware-a
            - middleware-b
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
        - middleware-a
        - middleware-b
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
            - middleware-a
            - middleware-b
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
            - middleware-a
            - middleware-b
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
            - middleware-a
            - middleware-b
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
            - middleware-a
            - middleware-b

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
            - middleware-a
            - middleware-b
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
            - middleware-a
          - before:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              provider:
                name: srv-b
            inject:
            - middleware-b
            - middleware-c
          - between:
              interface: wasi:http/handler@0.3.0-rc-2026-01-06
              inner:
                name: srv-c
              outer:
                name: srv-b
            inject:
            - middleware-d
            - middleware-e
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
            - middleware-a
            - middleware-b
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
            - middleware-a
            - middleware-b
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
            - middleware-a
            - middleware-b
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
            - middleware-a
            - middleware-b
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
}
