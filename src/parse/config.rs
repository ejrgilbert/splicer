use serde::Deserialize;

pub fn parse_yaml(yaml_str: &str) -> anyhow::Result<Vec<SpliceRule>> {
    let config: ConfigFile = serde_yaml::from_str(yaml_str)?;

    // i'm only able to parse this config version!
    assert_eq!(config.version, 1);
    Ok(config.to_splice_rules())
}

/// --- YAML config structures ---
#[derive(Debug, Deserialize)]
pub struct ConfigFile {
    pub version: u32,
    pub rules: Vec<YamlRule>,
}

#[derive(Debug, Deserialize)]
pub struct YamlRule {
    before: Option<YamlStrategyBefore>,
    between: Option<YamlStrategyBetween>,
    inject: Vec<Injection>,
}

#[derive(Debug, Deserialize)]
pub struct YamlStrategyBefore {
    interface: String,
    provider: Option<YamlProviderOpt>,
}

#[derive(Debug, Deserialize)]
pub struct YamlStrategyBetween {
    inner: YamlProviderReq,
    outer: YamlProviderReq,
    interface: String,
}

#[derive(Debug, Deserialize)]
pub struct YamlProviderReq {
    // The name of the instance to match on in the component
    // e.g.: `(instance $srv-b ...` --> "srv-b"
    // OR  : `(instance $wasi:http/handler@0.3.0-rc-2026-01-06-shim-instance ...` --> "wasi:http/handler@0.3.0-rc-2026-01-06-shim-instance"
    name: String,
    // Alias the matched provider to this name in the generated wac
    alias: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct YamlProviderOpt {
    // The name of the instance to match on in the component
    name: Option<String>,
    // Alias the matched provider to this name in the generated wac
    alias: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Injection {
    pub name: String,
    pub path: Option<String>,
}

/// --- Normalized rule type for Rust usage ---
#[derive(Debug)]
pub enum SpliceRule {
    Before {
        interface: String,
        provider_name: Option<String>,
        provider_alias: Option<String>,
        inject: Vec<Injection>,
    },
    Between {
        interface: String,
        inner_name: String,
        inner_alias: Option<String>,
        outer_name: String,
        outer_alias: Option<String>,
        inject: Vec<Injection>,
    },
}

impl ConfigFile {
    /// Convert YAML parsed rules into normalized [SpliceRule]
    pub fn to_splice_rules(&self) -> Vec<SpliceRule> {
        self.rules
            .iter()
            .map(
                |YamlRule {
                     before,
                     between,
                     inject,
                 }| {
                    if before.is_some() && between.is_some() {
                        panic!("insert error here (should have one or the other, not BOTH!)");
                    }

                    if let Some(YamlStrategyBefore {
                        interface,
                        provider,
                    }) = before
                    {
                        SpliceRule::Before {
                            interface: interface.clone(),
                            provider_name: if let Some(prov) = provider {
                                prov.name.clone()
                            } else {
                                None
                            },
                            provider_alias: if let Some(prov) = provider {
                                prov.alias.clone()
                            } else {
                                None
                            },
                            inject: (*inject).clone(),
                        }
                    } else if let Some(YamlStrategyBetween {
                        interface,
                        inner,
                        outer,
                    }) = between
                    {
                        SpliceRule::Between {
                            interface: interface.clone(),
                            inner_name: inner.name.clone(),
                            inner_alias: inner.alias.clone(),
                            outer_name: outer.name.clone(),
                            outer_alias: outer.alias.clone(),
                            inject: (*inject).clone(),
                        }
                    } else {
                        panic!("insert error here (should have one or the other, not neither!)");
                    }
                },
            )
            .collect()

        // TODO: Ensure this is valid -- all `Injection` must have a unique name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_before_rule() {
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
      provider:
        name: srv-b
    inject:
      - name: middleware-a
"#;
        let rules = parse_yaml(yaml).unwrap();
        assert_eq!(rules.len(), 1);
        let SpliceRule::Before {
            interface,
            provider_name,
            provider_alias,
            inject,
        } = &rules[0]
        else {
            panic!("expected Before rule");
        };
        assert_eq!(interface, "wasi:http/handler@0.3.0");
        assert_eq!(provider_name.as_deref(), Some("srv-b"));
        assert!(provider_alias.is_none());
        assert_eq!(inject.len(), 1);
        assert_eq!(inject[0].name, "middleware-a");
        assert!(inject[0].path.is_none());
    }

    #[test]
    fn parse_before_rule_no_provider() {
        // `provider` is optional — omitting it means inject before every instance.
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
    inject:
      - name: mw
"#;
        let rules = parse_yaml(yaml).unwrap();
        assert_eq!(rules.len(), 1);
        let SpliceRule::Before {
            provider_name,
            provider_alias,
            ..
        } = &rules[0]
        else {
            panic!("expected Before rule");
        };
        assert!(provider_name.is_none());
        assert!(provider_alias.is_none());
    }

    #[test]
    fn parse_between_rule() {
        let yaml = r#"
version: 1
rules:
  - between:
      interface: wasi:http/handler@0.3.0
      inner:
        name: srv-b
        alias: renamed-b
      outer:
        name: srv
    inject:
      - name: mw-a
      - name: mw-b
        path: /tmp/mw-b.wasm
"#;
        let rules = parse_yaml(yaml).unwrap();
        assert_eq!(rules.len(), 1);
        let SpliceRule::Between {
            interface,
            inner_name,
            inner_alias,
            outer_name,
            outer_alias,
            inject,
        } = &rules[0]
        else {
            panic!("expected Between rule");
        };
        assert_eq!(interface, "wasi:http/handler@0.3.0");
        assert_eq!(inner_name, "srv-b");
        assert_eq!(inner_alias.as_deref(), Some("renamed-b"));
        assert_eq!(outer_name, "srv");
        assert!(outer_alias.is_none());
        assert_eq!(inject.len(), 2);
        assert_eq!(inject[1].path.as_deref(), Some("/tmp/mw-b.wasm"));
    }

    #[test]
    fn parse_multi_rule() {
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
    inject:
      - name: first
  - between:
      interface: wasi:http/handler@0.3.0
      inner:
        name: srv-b
      outer:
        name: srv
    inject:
      - name: second
"#;
        let rules = parse_yaml(yaml).unwrap();
        assert_eq!(rules.len(), 2);
        assert!(matches!(rules[0], SpliceRule::Before { .. }));
        assert!(matches!(rules[1], SpliceRule::Between { .. }));
        // Order is preserved
        let SpliceRule::Before { inject: inj0, .. } = &rules[0] else {
            unreachable!()
        };
        let SpliceRule::Between { inject: inj1, .. } = &rules[1] else {
            unreachable!()
        };
        assert_eq!(inj0[0].name, "first");
        assert_eq!(inj1[0].name, "second");
    }

    #[test]
    fn parse_missing_interface() {
        // `interface` is required inside `before`; omitting it is a parse error.
        let yaml = r#"
version: 1
rules:
  - before:
      provider:
        name: srv-b
    inject:
      - name: mw
"#;
        let result = parse_yaml(yaml);
        assert!(
            result.is_err(),
            "expected parse error for missing interface field"
        );
    }

    #[test]
    #[should_panic]
    fn parse_unknown_version() {
        let yaml = r#"
version: 99
rules: []
"#;
        // version check is an assert_eq! inside parse_yaml — panics on mismatch
        let _ = parse_yaml(yaml);
    }
}
