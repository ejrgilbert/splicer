//! Read the strategy crate's behavior declaration from its
//! `Cargo.toml`. Strategy authors declare what their middleware does
//! to the wrapped target via:
//!
//! ```toml
//! [package.metadata.splicer]
//! behavior = "forward"     # or "virtualize"
//! ```
//!
//! - `forward`: the strategy passes each call to the wrapped target,
//!   optionally transforming args before or the result after. The
//!   generated wrapper imports the target's interface.
//! - `virtualize`: the strategy replaces the wrapped target, producing
//!   results from internal state without invoking it. The generated
//!   wrapper does NOT import the target.
//!
//! The host reads this declaration at codegen time to decide which
//! wrapper shape to emit. The strategy's `impl ForwardStrategy` or
//! `impl VirtualizeStrategy` block (in the strategy crate's source)
//! is what backs that declaration at compile time; the metadata is
//! the host-readable mirror.

use std::fmt;
use std::path::Path;

use serde::Deserialize;

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
    /// Failed to read the strategy's Cargo.toml.
    Io(std::io::Error),
    /// Failed to parse the file as TOML.
    Toml(toml::de::Error),
    /// `[package.metadata.splicer]` section is missing.
    SectionMissing,
    /// Section exists but has no `behavior` key.
    FieldMissing,
    /// `behavior` value was something other than "forward" or
    /// "virtualize".
    UnknownValue(String),
}

impl fmt::Display for BehaviorReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "failed to read strategy Cargo.toml: {e}"),
            Self::Toml(e) => write!(f, "failed to parse strategy Cargo.toml: {e}"),
            Self::SectionMissing => write!(
                f,
                "strategy Cargo.toml is missing [package.metadata.splicer]; \
                 add `behavior = \"forward\"` or `behavior = \"virtualize\"`"
            ),
            Self::FieldMissing => write!(
                f,
                "[package.metadata.splicer] has no `behavior` key; \
                 set it to \"forward\" or \"virtualize\""
            ),
            Self::UnknownValue(v) => write!(
                f,
                "[package.metadata.splicer] behavior = {v:?}; \
                 expected \"forward\" or \"virtualize\""
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

/// Read [`Behavior`] from the strategy crate's `Cargo.toml`.
pub fn read_behavior(cargo_toml: &Path) -> Result<Behavior, BehaviorReadError> {
    let text = std::fs::read_to_string(cargo_toml)?;
    read_behavior_from_str(&text)
}

/// Same as [`read_behavior`] but takes the file text directly.
pub fn read_behavior_from_str(toml_text: &str) -> Result<Behavior, BehaviorReadError> {
    let parsed: CargoToml = toml::from_str(toml_text)?;
    let splicer = parsed
        .package
        .and_then(|p| p.metadata)
        .and_then(|m| m.splicer)
        .ok_or(BehaviorReadError::SectionMissing)?;
    let raw = splicer.behavior.ok_or(BehaviorReadError::FieldMissing)?;
    match raw.as_str() {
        "forward" => Ok(Behavior::Forward),
        "virtualize" => Ok(Behavior::Virtualize),
        _ => Err(BehaviorReadError::UnknownValue(raw)),
    }
}

#[derive(Deserialize)]
struct CargoToml {
    package: Option<Package>,
}

#[derive(Deserialize)]
struct Package {
    metadata: Option<Metadata>,
}

#[derive(Deserialize)]
struct Metadata {
    splicer: Option<SplicerMeta>,
}

#[derive(Deserialize)]
struct SplicerMeta {
    behavior: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_forward() {
        let toml = r#"
            [package]
            name = "my-strategy"
            version = "0.1.0"
            edition = "2021"

            [package.metadata.splicer]
            behavior = "forward"
        "#;
        assert_eq!(read_behavior_from_str(toml).unwrap(), Behavior::Forward);
    }

    #[test]
    fn reads_virtualize() {
        let toml = r#"
            [package]
            name = "my-replay"
            version = "0.1.0"
            edition = "2021"

            [package.metadata.splicer]
            behavior = "virtualize"
        "#;
        assert_eq!(read_behavior_from_str(toml).unwrap(), Behavior::Virtualize);
    }

    #[test]
    fn coexists_with_other_metadata_sections() {
        // Cargo metadata for several tools at once shouldn't confuse us.
        let toml = r#"
            [package]
            name = "my-strategy"
            version = "0.1.0"
            edition = "2021"

            [package.metadata.docs.rs]
            all-features = true

            [package.metadata.splicer]
            behavior = "forward"

            [package.metadata.release]
            sign-commit = true
        "#;
        assert_eq!(read_behavior_from_str(toml).unwrap(), Behavior::Forward);
    }

    #[test]
    fn errors_when_section_missing() {
        let toml = r#"
            [package]
            name = "my-strategy"
            version = "0.1.0"
            edition = "2021"
        "#;
        assert!(matches!(
            read_behavior_from_str(toml),
            Err(BehaviorReadError::SectionMissing)
        ));
    }

    #[test]
    fn errors_when_field_missing() {
        let toml = r#"
            [package]
            name = "my-strategy"
            version = "0.1.0"
            edition = "2021"

            [package.metadata.splicer]
            other-key = "something"
        "#;
        assert!(matches!(
            read_behavior_from_str(toml),
            Err(BehaviorReadError::FieldMissing)
        ));
    }

    #[test]
    fn errors_on_unknown_value() {
        let toml = r#"
            [package]
            name = "my-strategy"
            version = "0.1.0"
            edition = "2021"

            [package.metadata.splicer]
            behavior = "mock"
        "#;
        match read_behavior_from_str(toml) {
            Err(BehaviorReadError::UnknownValue(v)) => assert_eq!(v, "mock"),
            other => panic!("expected UnknownValue, got {other:?}"),
        }
    }

    #[test]
    fn surfaces_toml_parse_errors() {
        let toml = "this is not valid toml = [[[[";
        assert!(matches!(
            read_behavior_from_str(toml),
            Err(BehaviorReadError::Toml(_))
        ));
    }
}
