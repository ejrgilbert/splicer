//! Shared schema + build-time codegen for splicer builtin manifests.
//!
//! Each builtin ships a `manifest.toml` declaring the keys it accepts
//! from the `splicer:builtin-config` substrate. At build time the
//! builtin's `build.rs` calls [`build_helper::codegen`], which:
//!
//! 1. Parses + validates the TOML against [`Manifest`].
//! 2. Emits a `#[link_section]`-attributed static that lands the TOML
//!    bytes verbatim in a wasm custom section named
//!    [`MANIFEST_SECTION_NAME`]. Splicer locates it with `wasmparser`,
//!    no byte-scan or magic sentinel needed.
//! 3. Emits a `mod config` with one typed accessor per declared key.
//!    Accessors call `crate::bindings::splicer::builtin_config::get::get`
//!    (the substrate import every consumer already binds) and fall
//!    back to the manifest-declared default if the substrate returns
//!    `None` or the value fails to parse. Hardcoding defaults at the
//!    Rust call site is no longer possible — `manifest.toml` is the
//!    single source of truth.

use serde::{Deserialize, Serialize};

pub mod build_helper;

/// Common prefix for the wasm custom section that carries each
/// builtin's `manifest.toml`. The full section name is
/// `{MANIFEST_SECTION_PREFIX}{builtin-name}` — e.g.
/// `splicer-builtin-manifest/hello-tier1`. Per-builtin naming lets a
/// composed pipeline carry many manifests without ambiguity: the
/// extractor returns `Vec<(name, Manifest)>` keyed by the suffix, and
/// splicer's splice-time check requires the section name to match the
/// builtin's YAML-declared name (catches OCI misrouting too).
pub const MANIFEST_SECTION_PREFIX: &str = "splicer-builtin-manifest/";

/// Format the full section name for `builtin_name`. Used by both the
/// build-time codegen and the splicer-side extractor.
pub fn section_name_for(builtin_name: &str) -> String {
    format!("{MANIFEST_SECTION_PREFIX}{builtin_name}")
}

/// Parsed `manifest.toml` for a builtin.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    /// Builtin-level metadata. Rendered by `splicer builtin list` and
    /// `splicer builtin <name>`.
    pub builtin: BuiltinMeta,
    /// Declared config keys, in source order. Empty for builtins that
    /// don't import the substrate but ship a manifest anyway (rare).
    #[serde(rename = "key", default)]
    pub keys: Vec<ConfigKey>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BuiltinMeta {
    /// One-line description shown by `splicer builtin list`.
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigKey {
    /// Key as it appears in the splice-config YAML's `config:` map.
    pub name: String,
    /// Declared scalar type. Splicer rejects YAML values that don't
    /// match this at splice time; the codegen'd accessor parses the
    /// substrate value to the same type at runtime, falling back to
    /// [`ConfigKey::default`] on parse failure.
    #[serde(rename = "type")]
    pub kind: KeyType,
    /// Default the codegen'd accessor returns when the user didn't
    /// set the key in YAML (substrate returns `None`) or when the
    /// stringified value fails to re-parse at runtime.
    pub default: toml::Value,
    /// Per-key documentation rendered by `splicer builtin <name>`.
    pub doc: String,
    /// Optional enum-style constraint. Currently only valid on
    /// `type = "string"` keys: when non-empty, splice-time validation
    /// rejects any YAML value not in this list. The runtime accessor
    /// still returns `&'static str`; the constraint just guarantees
    /// the substrate-served value parses cleanly.
    #[serde(default)]
    pub values: Vec<String>,
    /// Compare YAML values against [`ConfigKey::values`] with ASCII
    /// case folding. Convenience for OTel-style level names where
    /// `INFO`, `Info`, and `info` should all be accepted.
    #[serde(default)]
    pub case_insensitive: bool,
}

impl ConfigKey {
    /// Human-readable rendering of the default for CLI output. Strings
    /// keep their quotes; floats always include a decimal point so the
    /// type (`f64`) and value visually agree even for whole numbers
    /// (`10` vs. `10.0`); numbers/bools stringify. Lifted to a method
    /// so callers don't have to depend on `toml` directly.
    pub fn default_display(&self) -> String {
        match &self.default {
            toml::Value::String(s) => format!("\"{s}\""),
            toml::Value::Integer(n) => n.to_string(),
            toml::Value::Float(f) => {
                let s = f.to_string();
                if s.contains(['.', 'e', 'E', 'n']) {
                    s
                } else {
                    format!("{s}.0")
                }
            }
            toml::Value::Boolean(b) => b.to_string(),
            other => other.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyType {
    String,
    U32,
    F64,
    Bool,
}

impl KeyType {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyType::String => "string",
            KeyType::U32 => "u32",
            KeyType::F64 => "f64",
            KeyType::Bool => "bool",
        }
    }
}

#[derive(Debug)]
pub enum ManifestError {
    Parse(String),
    InvalidDefault {
        key: String,
        declared: KeyType,
        actual: String,
    },
    U32Range(String),
    DuplicateKey(String),
    /// `values = [...]` was set on a non-string key.
    ValuesOnNonString(String),
    /// Declared `default` isn't a member of the `values` enum.
    DefaultNotInValues {
        key: String,
        default: String,
        values: Vec<String>,
    },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "manifest.toml parse error: {e}"),
            Self::InvalidDefault {
                key,
                declared,
                actual,
            } => write!(
                f,
                "key '{key}': declared type '{}' does not match default's TOML type '{actual}'",
                declared.as_str(),
            ),
            Self::U32Range(key) => write!(
                f,
                "key '{key}': declared type 'u32' but default is out of range \
                 (must be a non-negative integer ≤ {})",
                u32::MAX
            ),
            Self::DuplicateKey(key) => write!(f, "duplicate config key declared: '{key}'"),
            Self::ValuesOnNonString(key) => write!(
                f,
                "key '{key}': `values = [...]` is only supported on type = \"string\""
            ),
            Self::DefaultNotInValues {
                key,
                default,
                values,
            } => write!(
                f,
                "key '{key}': default \"{default}\" is not in declared values [{}]",
                values.join(", "),
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

impl Manifest {
    /// Parse TOML source and validate that each `default` matches the
    /// declared `type`. The same routine runs in builtin `build.rs`
    /// (catches errors at builtin-build time) and in splicer's
    /// extractor (catches drift if a builtin was built against an
    /// older manifest crate).
    pub fn from_toml(src: &str) -> Result<Self, ManifestError> {
        let m: Manifest = toml::from_str(src).map_err(|e| ManifestError::Parse(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        let mut seen = std::collections::BTreeSet::new();
        for k in &self.keys {
            if !seen.insert(k.name.clone()) {
                return Err(ManifestError::DuplicateKey(k.name.clone()));
            }
            match (k.kind, &k.default) {
                (KeyType::String, toml::Value::String(_)) => {}
                (KeyType::Bool, toml::Value::Boolean(_)) => {}
                (KeyType::F64, toml::Value::Float(_)) => {}
                // TOML integers fit into f64 too — useful so `default = 10`
                // works for an f64 key without surprising the author.
                (KeyType::F64, toml::Value::Integer(_)) => {}
                (KeyType::U32, toml::Value::Integer(n)) => {
                    if *n < 0 || *n > u32::MAX as i64 {
                        return Err(ManifestError::U32Range(k.name.clone()));
                    }
                }
                (declared, actual) => {
                    return Err(ManifestError::InvalidDefault {
                        key: k.name.clone(),
                        declared,
                        actual: toml_kind(actual).to_string(),
                    });
                }
            }
            // Enum-style `values = [...]` is only valid on string keys,
            // and the declared default has to be one of the listed values
            // (case-folded if `case_insensitive`).
            if !k.values.is_empty() {
                if k.kind != KeyType::String {
                    return Err(ManifestError::ValuesOnNonString(k.name.clone()));
                }
                let toml::Value::String(default) = &k.default else {
                    unreachable!("validated as string above");
                };
                if !matches_value(default, &k.values, k.case_insensitive) {
                    return Err(ManifestError::DefaultNotInValues {
                        key: k.name.clone(),
                        default: default.clone(),
                        values: k.values.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate a single YAML-derived string value against this
    /// manifest's declared type for `key`. Returns `Ok(None)` if the
    /// key isn't declared (caller decides whether unknown keys are
    /// an error); returns `Err` if the value won't parse.
    ///
    /// Used by splicer at splice time: a user's `buffer: "ten"` against
    /// a `u32`-typed key fails here, not at runtime.
    pub fn validate_value(&self, key: &str, value: &str) -> Result<Option<()>, String> {
        let Some(decl) = self.keys.iter().find(|k| k.name == key) else {
            return Ok(None);
        };
        let type_ok = match decl.kind {
            KeyType::String => true,
            KeyType::Bool => value.parse::<bool>().is_ok(),
            KeyType::U32 => value.parse::<u32>().is_ok(),
            KeyType::F64 => value.parse::<f64>().is_ok(),
        };
        if !type_ok {
            return Err(format!(
                "value '{value}' for key '{key}' does not parse as {}",
                decl.kind.as_str(),
            ));
        }
        if !decl.values.is_empty()
            && !matches_value(value, &decl.values, decl.case_insensitive)
        {
            return Err(format!(
                "value '{value}' for key '{key}' is not in declared values [{}]",
                decl.values.join(", "),
            ));
        }
        Ok(Some(()))
    }
}

/// Extract every embedded manifest from a wasm module or component.
/// Walks nested modules transparently (Rust's `#[link_section]` emits
/// at the core-module level; `wasmparser::Parser::parse_all` surfaces
/// any nested module's customs in one pass), returning one entry per
/// section whose name starts with [`MANIFEST_SECTION_PREFIX`]. The
/// key is the suffix — i.e. the builtin's canonical name.
///
/// A composed pipeline can carry many manifests; this function returns
/// all of them. Splice-time validation in splicer reads only the one
/// matching the YAML-declared builtin name. Returns `Err` for non-
/// UTF-8 payloads or TOML parse errors; duplicates of the same
/// builtin name are also rejected (build mismatch).
pub fn extract_all(wasm_bytes: &[u8]) -> Result<Vec<(String, Manifest)>, String> {
    use wasmparser::{Parser, Payload};
    let mut found: Vec<(String, Manifest)> = Vec::new();
    for payload in Parser::new(0).parse_all(wasm_bytes) {
        let payload = payload.map_err(|e| format!("wasm parse error: {e}"))?;
        let Payload::CustomSection(reader) = payload else {
            continue;
        };
        let Some(name) = reader.name().strip_prefix(MANIFEST_SECTION_PREFIX) else {
            continue;
        };
        if name.is_empty() {
            return Err(format!(
                "custom section '{prefix}' has empty builtin-name suffix",
                prefix = MANIFEST_SECTION_PREFIX,
            ));
        }
        if found.iter().any(|(n, _)| n == name) {
            return Err(format!(
                "manifest section for builtin '{name}' appears more than once",
            ));
        }
        let toml_src = std::str::from_utf8(reader.data()).map_err(|e| {
            format!("manifest section for '{name}' is not valid UTF-8: {e}")
        })?;
        let manifest = Manifest::from_toml(toml_src)
            .map_err(|e| format!("manifest for '{name}' failed to parse: {e}"))?;
        found.push((name.to_string(), manifest));
    }
    Ok(found)
}

/// Convenience wrapper: extract the manifest section matching
/// `builtin_name` exactly. Returns `Ok(None)` if no manifest is
/// present at all; returns `Err` if a manifest exists but is named
/// for a different builtin (catches OCI misrouting / build-tag
/// mismatches) or if any section is malformed.
pub fn extract_for_builtin(
    wasm_bytes: &[u8],
    builtin_name: &str,
) -> Result<Option<Manifest>, String> {
    let all = extract_all(wasm_bytes)?;
    if all.is_empty() {
        return Ok(None);
    }
    if let Some((_, m)) = all.iter().find(|(n, _)| n == builtin_name) {
        return Ok(Some(m.clone()));
    }
    let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
    Err(format!(
        "expected manifest for builtin '{builtin_name}', but the wasm carries \
         manifest(s) for: [{}]",
        names.join(", "),
    ))
}

fn matches_value(needle: &str, haystack: &[String], case_insensitive: bool) -> bool {
    if case_insensitive {
        haystack
            .iter()
            .any(|v| v.eq_ignore_ascii_case(needle))
    } else {
        haystack.iter().any(|v| v == needle)
    }
}

fn toml_kind(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_manifest() {
        let src = r#"
            [builtin]
            description = "test builtin"

            [[key]]
            name = "greeting"
            type = "string"
            default = "hi"
            doc = "Greeting prefix."
        "#;
        let m = Manifest::from_toml(src).unwrap();
        assert_eq!(m.builtin.description, "test builtin");
        assert_eq!(m.keys.len(), 1);
        assert_eq!(m.keys[0].name, "greeting");
        assert_eq!(m.keys[0].kind, KeyType::String);
    }

    #[test]
    fn rejects_mismatched_default() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "buffer"
            type = "u32"
            default = "ten"
            doc = ""
        "#;
        let err = Manifest::from_toml(src).unwrap_err();
        assert!(matches!(err, ManifestError::InvalidDefault { .. }), "{err}");
    }

    #[test]
    fn accepts_integer_default_for_f64() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "delay"
            type = "f64"
            default = 10
            doc = ""
        "#;
        Manifest::from_toml(src).unwrap();
    }

    #[test]
    fn rejects_out_of_range_u32() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "buffer"
            type = "u32"
            default = -1
            doc = ""
        "#;
        let err = Manifest::from_toml(src).unwrap_err();
        assert!(matches!(err, ManifestError::U32Range(_)), "{err}");
    }

    #[test]
    fn rejects_duplicate_keys() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "buffer"
            type = "u32"
            default = 1
            doc = ""

            [[key]]
            name = "buffer"
            type = "u32"
            default = 2
            doc = ""
        "#;
        let err = Manifest::from_toml(src).unwrap_err();
        assert!(matches!(err, ManifestError::DuplicateKey(_)), "{err}");
    }

    /// Hand-encode a minimal wasm module with a single custom section.
    /// Avoids dragging in `wasm-encoder` as a dev-dep just for tests.
    fn module_with_custom_section(name: &str, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\0asm");
        out.extend_from_slice(&1u32.to_le_bytes());
        // Custom section: section id = 0, then LEB128 size, then payload
        // (LEB128 name length + name + data).
        let mut payload = Vec::new();
        write_leb128(&mut payload, name.len() as u64);
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(data);
        out.push(0);
        write_leb128(&mut out, payload.len() as u64);
        out.extend_from_slice(&payload);
        out
    }

    fn write_leb128(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    #[test]
    fn extract_all_round_trip() {
        let toml = r#"
            [builtin]
            description = "round-trip"

            [[key]]
            name = "buffer"
            type = "u32"
            default = 1
            doc = ""
        "#;
        let wasm = module_with_custom_section(&section_name_for("hello-tier1"), toml.as_bytes());
        let all = extract_all(&wasm).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "hello-tier1");
        assert_eq!(all[0].1.builtin.description, "round-trip");
    }

    #[test]
    fn extract_for_builtin_returns_none_when_no_section() {
        let wasm = module_with_custom_section("unrelated", b"");
        assert!(extract_for_builtin(&wasm, "anything").unwrap().is_none());
    }

    #[test]
    fn extract_for_builtin_rejects_wrong_name() {
        let toml = r#"[builtin]
            description = "x""#;
        let wasm = module_with_custom_section(&section_name_for("hello-tier1"), toml.as_bytes());
        let err = extract_for_builtin(&wasm, "otel-bare-metrics").unwrap_err();
        assert!(err.contains("expected manifest for builtin"), "{err}");
        assert!(err.contains("hello-tier1"), "{err}");
    }

    #[test]
    fn values_constraint_rejects_unknown_value() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "severity"
            type = "string"
            values = ["INFO", "WARN", "ERROR"]
            default = "INFO"
            doc = ""
        "#;
        let m = Manifest::from_toml(src).unwrap();
        assert!(m.validate_value("severity", "INFO").unwrap().is_some());
        let err = m.validate_value("severity", "TRACE").unwrap_err();
        assert!(err.contains("not in declared values"), "{err}");
    }

    #[test]
    fn values_constraint_case_insensitive() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "severity"
            type = "string"
            values = ["INFO", "WARN"]
            case_insensitive = true
            default = "INFO"
            doc = ""
        "#;
        let m = Manifest::from_toml(src).unwrap();
        // Mixed-case accepted under case_insensitive.
        assert!(m.validate_value("severity", "info").unwrap().is_some());
        assert!(m.validate_value("severity", "Warn").unwrap().is_some());
    }

    #[test]
    fn values_on_non_string_rejected_at_parse() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "buffer"
            type = "u32"
            values = ["1", "2", "3"]
            default = 1
            doc = ""
        "#;
        let err = Manifest::from_toml(src).unwrap_err();
        assert!(matches!(err, ManifestError::ValuesOnNonString(_)), "{err}");
    }

    #[test]
    fn default_not_in_values_rejected_at_parse() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "severity"
            type = "string"
            values = ["INFO", "WARN"]
            default = "TRACE"
            doc = ""
        "#;
        let err = Manifest::from_toml(src).unwrap_err();
        assert!(
            matches!(err, ManifestError::DefaultNotInValues { .. }),
            "{err}"
        );
    }

    #[test]
    fn validate_value_rejects_typo() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "buffer"
            type = "u32"
            default = 1
            doc = ""
        "#;
        let m = Manifest::from_toml(src).unwrap();
        assert!(m.validate_value("buffer", "10").unwrap().is_some());
        let err = m.validate_value("buffer", "ten").unwrap_err();
        assert!(err.contains("u32"), "{err}");
        assert!(m.validate_value("unknown", "x").unwrap().is_none());
    }
}
