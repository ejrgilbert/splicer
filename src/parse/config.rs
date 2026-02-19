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
    }
}
