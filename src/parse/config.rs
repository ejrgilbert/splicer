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
    inject: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct YamlStrategyBefore {
    interface: String,
    provider_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct YamlStrategyBetween {
    inner: String,
    outer: String,
    interface: String,
}

/// --- Normalized rule type for Rust usage ---
#[derive(Debug)]
pub enum SpliceRule {
    Before {
        interface: String,
        provider_name: Option<String>,
        inject: Vec<String>,
    },
    Between {
        interface: String,
        inner: String,
        outer: String,
        inject: Vec<String>,
    },
}

impl ConfigFile {
    /// Convert YAML parsed rules into normalized [SpliceRule]
    pub fn to_splice_rules(&self) -> Vec<SpliceRule> {
        self.rules.iter().map(| YamlRule { before, between, inject } | {
            if before.is_some() && between.is_some() {
                panic!("insert error here (should have one or the other, not BOTH!)");
            }

            if let Some(YamlStrategyBefore {interface, provider_name}) = before {
                SpliceRule::Before {
                    interface: interface.clone(),
                    provider_name: provider_name.clone(),
                    inject: inject.clone(),
                }
            } else if let Some(YamlStrategyBetween {interface, inner, outer }) = between {
                SpliceRule::Between {
                    interface: interface.clone(),
                    inner: inner.clone(),
                    outer: outer.clone(),
                    inject: inject.clone(),
                }
            } else {
                panic!("insert error here (should have one or the other, not neither!)");
            }
        }).collect()
    }
}