//! Read a strategy crate's behavior declaration from its
//! `manifest.toml`. The `[builtin]` block declares
//! `behavior = "forward"` or `"virtualize"` alongside the
//! description and config keys; the schema lives in
//! `builtin_manifest::Manifest` and we delegate parsing there.

use std::fmt;
use std::path::Path;

/// What the strategy does to the wrapped target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// Strategy forwards each call to the wrapped target. Wrapper
    /// imports the target's interface.
    Forward,
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
    /// `[builtin]` had no `behavior` key. tier-1/2 manifests legitimately
    /// omit it; this is how callers detect a non-tier-3/4 builtin.
    FieldMissing,
    /// `behavior` value was something other than `"forward"` or
    /// `"virtualize"`.
    UnknownValue(String),
}

impl fmt::Display for BehaviorReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "failed to read strategy manifest.toml: {e}"),
            Self::Toml(e) => write!(f, "failed to parse strategy manifest.toml: {e}"),
            Self::FieldMissing => write!(
                f,
                "[builtin] has no `behavior` key; set it to \"forward\" or \"virtualize\""
            ),
            Self::UnknownValue(v) => write!(
                f,
                "[builtin] behavior = {v:?}; expected \"forward\" or \"virtualize\""
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
    let raw = manifest
        .builtin
        .behavior
        .ok_or(BehaviorReadError::FieldMissing)?;
    match raw.as_str() {
        "forward" => Ok(Behavior::Forward),
        "virtualize" => Ok(Behavior::Virtualize),
        _ => Err(BehaviorReadError::UnknownValue(raw)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_forward() {
        let toml = r#"
            [builtin]
            description = "hello-tier3 smoke"
            behavior = "forward"
        "#;
        assert_eq!(read_behavior_from_str(toml).unwrap(), Behavior::Forward);
    }

    #[test]
    fn reads_virtualize() {
        let toml = r#"
            [builtin]
            description = "hello-tier4 smoke"
            behavior = "virtualize"
        "#;
        assert_eq!(read_behavior_from_str(toml).unwrap(), Behavior::Virtualize);
    }

    #[test]
    fn missing_behavior_field_errors() {
        // tier-1/2 manifests legitimately have no behavior field;
        // callers use FieldMissing to detect that case.
        let toml = r#"
            [builtin]
            description = "tier-1 builtin"
        "#;
        assert!(matches!(
            read_behavior_from_str(toml),
            Err(BehaviorReadError::FieldMissing)
        ));
    }

    #[test]
    fn rejects_unknown_value() {
        let toml = r#"
            [builtin]
            description = "..."
            behavior = "mock"
        "#;
        match read_behavior_from_str(toml) {
            Err(BehaviorReadError::UnknownValue(v)) => assert_eq!(v, "mock"),
            other => panic!("expected UnknownValue, got {other:?}"),
        }
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
