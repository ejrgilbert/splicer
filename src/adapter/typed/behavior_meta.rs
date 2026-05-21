//! Read a strategy crate's `Behavior` (forward vs virtualize) from
//! its `manifest.toml`. Reads the manifest's required `tier` field
//! via `builtin_manifest::Manifest` and maps tier-3 → forward,
//! tier-4 → virtualize. Tier-1/2 manifests are rejected as not
//! tier-3/4 builtins.

use std::fmt;
use std::path::Path;

use builtin_manifest::Tier;

/// What the strategy does to the wrapped target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// Strategy transforms the call — receives typed args + result
    /// and may mutate either before forwarding to the wrapped target.
    /// Wrapper imports the target's interface.
    Transform,
    /// Strategy replaces the wrapped target. Wrapper does not import
    /// the target's interface.
    Virtualize,
}

/// Failure modes for [`read_behavior`] / [`read_behavior_from_str`].
#[derive(Debug)]
pub enum BehaviorReadError {
    /// Failed to read the strategy's `manifest.toml`.
    Io(std::io::Error),
    /// Failed to parse the file as a `builtin_manifest::Manifest`.
    Toml(toml::de::Error),
    /// Manifest's `tier` was 1 or 2 — not a tier-3/4 builtin.
    NotTyped(Tier),
}

impl fmt::Display for BehaviorReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "failed to read strategy manifest.toml: {e}"),
            Self::Toml(e) => write!(f, "failed to parse strategy manifest.toml: {e}"),
            Self::NotTyped(t) => write!(
                f,
                "manifest declares tier-{}; expected tier-3 or tier-4",
                u8::from(*t)
            ),
        }
    }
}

impl std::error::Error for BehaviorReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Toml(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for BehaviorReadError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<toml::de::Error> for BehaviorReadError {
    fn from(e: toml::de::Error) -> Self {
        Self::Toml(e)
    }
}

/// Read [`Behavior`] from a strategy crate's directory.
pub fn read_behavior(crate_dir: &Path) -> Result<Behavior, BehaviorReadError> {
    let text = std::fs::read_to_string(crate_dir.join("manifest.toml"))?;
    read_behavior_from_str(&text)
}

/// Same as [`read_behavior`] but takes the file text directly.
pub fn read_behavior_from_str(toml_text: &str) -> Result<Behavior, BehaviorReadError> {
    let manifest: builtin_manifest::Manifest = toml::from_str(toml_text)?;
    match manifest.builtin.tier {
        Tier::Tier3 => Ok(Behavior::Transform),
        Tier::Tier4 => Ok(Behavior::Virtualize),
        other => Err(BehaviorReadError::NotTyped(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_forward_from_tier3_manifest() {
        let toml = r#"
            [builtin]
            description = "hello-tier3 smoke"
            tier = 3
        "#;
        assert_eq!(read_behavior_from_str(toml).unwrap(), Behavior::Transform);
    }

    #[test]
    fn reads_virtualize_from_tier4_manifest() {
        let toml = r#"
            [builtin]
            description = "hello-tier4 smoke"
            tier = 4
        "#;
        assert_eq!(read_behavior_from_str(toml).unwrap(), Behavior::Virtualize);
    }

    #[test]
    fn tier1_manifest_is_not_typed() {
        let toml = r#"
            [builtin]
            description = "tier-1 builtin"
            tier = 1
        "#;
        assert!(matches!(
            read_behavior_from_str(toml),
            Err(BehaviorReadError::NotTyped(Tier::Tier1))
        ));
    }

    #[test]
    fn tier2_manifest_is_not_typed() {
        let toml = r#"
            [builtin]
            description = "tier-2 builtin"
            tier = 2
        "#;
        assert!(matches!(
            read_behavior_from_str(toml),
            Err(BehaviorReadError::NotTyped(Tier::Tier2))
        ));
    }

    #[test]
    fn rejects_out_of_range_tier() {
        let toml = r#"
            [builtin]
            description = "..."
            tier = 99
        "#;
        assert!(matches!(
            read_behavior_from_str(toml),
            Err(BehaviorReadError::Toml(_))
        ));
    }

    #[test]
    fn missing_tier_field_errors() {
        let toml = r#"
            [builtin]
            description = "old-style manifest"
        "#;
        assert!(matches!(
            read_behavior_from_str(toml),
            Err(BehaviorReadError::Toml(_))
        ));
    }

    #[test]
    fn surfaces_toml_parse_errors() {
        let toml = "not valid = [[[";
        assert!(matches!(
            read_behavior_from_str(toml),
            Err(BehaviorReadError::Toml(_))
        ));
    }
}
