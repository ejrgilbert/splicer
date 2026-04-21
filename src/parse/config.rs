use anyhow::bail;
use serde::Deserialize;
use std::collections::HashMap;

/// Parse a YAML splice configuration string into a list of validated
/// [`SpliceRule`]s ready to pass to [`crate::lowlevel::generate_wac`].
pub fn parse_yaml(yaml_str: &str) -> anyhow::Result<Vec<SpliceRule>> {
    let config: ConfigFile = serde_yaml::from_str(yaml_str)?;
    config.validate()?;
    Ok(config.into_splice_rules())
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

/// Extra information stored on an [`Injection`] when it has been resolved as a
/// tier-1 adapter by `add_to_inject_plan`.  Not present in the YAML config.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AdapterInjectionInfo {
    /// Path to the generated adapter `.wasm` file.
    pub adapter_path: String,
    /// Tier-1 interfaces the middleware exports (e.g. `"splicer:tier1/before"`).
    pub tier1_interfaces: Vec<String>,
}

/// A middleware to inject at a splice point. Constructed from the YAML
/// config `inject` list or programmatically via [`Injection::from_path`]
/// / [`Injection::from_name`].
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Injection {
    /// The middleware's logical name (used as the WAC variable).
    pub name: String,
    /// Path to the middleware `.wasm` file on disk. `None` when the
    /// middleware is referenced by name only (contract checks will
    /// produce a warning instead of a definitive result).
    pub path: Option<String>,
    /// Populated at runtime by `add_to_inject_plan` when this injection
    /// is resolved as a tier-1 adapter. Not part of the YAML config and
    /// not user-settable — use the `generated_adapters` field on
    /// [`crate::api::SpliceOutput`] for the structured view of which
    /// adapters splicer wrote.
    #[serde(skip)]
    pub(crate) adapter_info: Option<AdapterInjectionInfo>,
}

impl Injection {
    /// Construct an [`Injection`] for a middleware that should be
    /// loaded from a `.wasm` file at `path`.
    pub fn from_path(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: Some(path.into()),
            adapter_info: None,
        }
    }

    /// Construct an [`Injection`] referencing a middleware by name
    /// only — useful for the limited subset of contract checks that
    /// can run without loading the middleware bytes.
    pub fn from_name(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: None,
            adapter_info: None,
        }
    }
}

/// A validated splice rule, normalized from the YAML config.
#[derive(Debug)]
pub enum SpliceRule {
    /// Inject middleware before a provider on an interface edge.
    Before {
        /// The interface to match (e.g. `"wasi:http/handler@0.3.0"`).
        interface: String,
        /// Optional provider name to scope the match.
        provider_name: Option<String>,
        /// Optional alias for the matched provider in the generated WAC.
        provider_alias: Option<String>,
        /// Middleware to inject (in order).
        inject: Vec<Injection>,
    },
    /// Inject middleware between two specific components on an interface edge.
    Between {
        /// The interface to match.
        interface: String,
        /// Name of the inner (provider-side) component.
        inner_name: String,
        /// Optional alias for the inner component.
        inner_alias: Option<String>,
        /// Name of the outer (consumer-side) component.
        outer_name: String,
        /// Optional alias for the outer component.
        outer_alias: Option<String>,
        /// Middleware to inject (in order).
        inject: Vec<Injection>,
    },
}

impl ConfigFile {
    /// Validate the parsed configuration, returning a descriptive error for any problem.
    ///
    /// Checks (in order):
    /// 1. Supported version number.
    /// 2. Each rule specifies exactly one strategy (`before` XOR `between`).
    /// 3. Each rule's `inject` list is non-empty.
    /// 4. Each injection name is non-empty.
    /// 5. Each injection `path`, when present, is non-empty.
    /// 6. Interface names are non-empty.
    /// 7. `before` provider `name`, when present, is non-empty.
    /// 8. `between` `inner` and `outer` must name different instances.
    /// 9. Injection names are globally unique across all rules (required because
    ///    each name becomes a WAC instance identifier and `--dep` argument key).
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.version != 1 {
            bail!(
                "unsupported config version {}: only version 1 is supported",
                self.version
            );
        }

        // name → first rule index (1-based) for duplicate detection
        let mut seen_names: HashMap<&str, usize> = HashMap::new();

        for (i, rule) in self.rules.iter().enumerate() {
            let rule_num = i + 1;

            // Strategy must be exactly one of before/between.
            match (&rule.before, &rule.between) {
                (Some(_), Some(_)) => {
                    bail!("rule {rule_num}: a rule may specify 'before' or 'between', not both")
                }
                (None, None) => {
                    bail!("rule {rule_num}: a rule must specify either 'before' or 'between'")
                }
                _ => {}
            }

            // Interface name must be non-empty.
            let interface = if let Some(b) = &rule.before {
                &b.interface
            } else if let Some(bw) = &rule.between {
                &bw.interface
            } else {
                unreachable!()
            };
            if interface.is_empty() {
                bail!("rule {rule_num}: 'interface' must not be empty");
            }

            // before-specific checks.
            if let Some(before) = &rule.before {
                if let Some(prov) = &before.provider {
                    if prov.name.as_deref() == Some("") {
                        bail!(
                            "rule {rule_num}: provider 'name' must not be empty if specified \
                             (omit the key to leave it unset)"
                        );
                    }
                }
            }

            // between-specific checks.
            if let Some(between) = &rule.between {
                if between.inner.name == between.outer.name {
                    bail!(
                        "rule {rule_num} (between): 'inner' and 'outer' must name different \
                         instances, but both are '{}'",
                        between.inner.name
                    );
                }
            }

            // inject list must be non-empty.
            if rule.inject.is_empty() {
                bail!("rule {rule_num}: 'inject' list must contain at least one entry");
            }

            for (j, inj) in rule.inject.iter().enumerate() {
                let inj_num = j + 1;

                if inj.name.is_empty() {
                    bail!("rule {rule_num}, injection {inj_num}: injection name must not be empty");
                }

                if inj.path.as_deref() == Some("") {
                    bail!(
                        "rule {rule_num}, injection '{}': 'path' must not be empty if specified \
                         (omit the key to leave it unset)",
                        inj.name
                    );
                }

                // Global uniqueness: injection names are used as WAC identifiers.
                if let Some(first_rule) = seen_names.get(inj.name.as_str()) {
                    bail!(
                        "injection name '{}' is used in rule {rule_num} but was already declared \
                         in rule {first_rule}; each injection must have a globally unique name",
                        inj.name
                    );
                }
                seen_names.insert(&inj.name, rule_num);
            }
        }

        Ok(())
    }

    /// Convert validated YAML rules into normalized [`SpliceRule`]s.
    ///
    /// Assumes [`ConfigFile::validate`] has already been called.
    pub fn into_splice_rules(self) -> Vec<SpliceRule> {
        self.rules
            .into_iter()
            .map(
                |YamlRule {
                     before,
                     between,
                     inject,
                 }| {
                    if let Some(YamlStrategyBefore {
                        interface,
                        provider,
                    }) = before
                    {
                        SpliceRule::Before {
                            interface,
                            provider_name: provider.as_ref().and_then(|p| p.name.clone()),
                            provider_alias: provider.and_then(|p| p.alias),
                            inject,
                        }
                    } else if let Some(YamlStrategyBetween {
                        interface,
                        inner,
                        outer,
                    }) = between
                    {
                        SpliceRule::Between {
                            interface,
                            inner_name: inner.name,
                            inner_alias: inner.alias,
                            outer_name: outer.name,
                            outer_alias: outer.alias,
                            inject,
                        }
                    } else {
                        unreachable!("validate() guarantees exactly one strategy per rule")
                    }
                },
            )
            .collect()
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
    fn parse_unknown_version() {
        let yaml = r#"
version: 99
rules: []
"#;
        let err = parse_yaml(yaml).unwrap_err().to_string();
        assert!(
            err.contains("unsupported config version"),
            "unexpected error: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Validation error cases
    // -----------------------------------------------------------------------

    fn assert_err(yaml: &str, expected_fragment: &str) {
        let err = parse_yaml(yaml).unwrap_err().to_string();
        assert!(
            err.contains(expected_fragment),
            "expected error containing {expected_fragment:?}, got: {err}"
        );
    }

    #[test]
    fn validate_both_before_and_between() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    between:
      interface: wasi:http/handler
      inner:
        name: a
      outer:
        name: b
    inject:
      - name: mw
"#,
            "'before' or 'between', not both",
        );
    }

    #[test]
    fn validate_neither_before_nor_between() {
        assert_err(
            r#"
version: 1
rules:
  - inject:
      - name: mw
"#,
            "either 'before' or 'between'",
        );
    }

    #[test]
    fn validate_empty_inject_list() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject: []
"#,
            "'inject' list must contain at least one entry",
        );
    }

    #[test]
    fn validate_empty_injection_name() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - name: ""
"#,
            "injection name must not be empty",
        );
    }

    #[test]
    fn validate_empty_injection_path() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - name: mw
        path: ""
"#,
            "'path' must not be empty if specified",
        );
    }

    #[test]
    fn validate_empty_interface_name() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: ""
    inject:
      - name: mw
"#,
            "'interface' must not be empty",
        );
    }

    #[test]
    fn validate_empty_before_provider_name() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
      provider:
        name: ""
    inject:
      - name: mw
"#,
            "provider 'name' must not be empty if specified",
        );
    }

    #[test]
    fn validate_between_same_inner_outer() {
        assert_err(
            r#"
version: 1
rules:
  - between:
      interface: wasi:http/handler
      inner:
        name: srv
      outer:
        name: srv
    inject:
      - name: mw
"#,
            "'inner' and 'outer' must name different instances",
        );
    }

    #[test]
    fn validate_duplicate_injection_name_across_rules() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - name: mw-a
  - before:
      interface: wasi:logging/log
    inject:
      - name: mw-a
"#,
            "injection name 'mw-a' is used in rule 2 but was already declared in rule 1",
        );
    }

    #[test]
    fn validate_duplicate_injection_name_within_rule() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - name: mw-a
      - name: mw-a
"#,
            "injection name 'mw-a' is used in rule 1 but was already declared in rule 1",
        );
    }
}
