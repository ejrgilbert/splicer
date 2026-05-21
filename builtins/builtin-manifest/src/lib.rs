//! Shared schema + build-time codegen for splicer builtin manifests.
//!
//! Each builtin ships a `manifest.toml` declaring the keys it accepts
//! from the `splicer:builtin-config` substrate. At build time the
//! builtin's `build.rs` calls [`build_helper::codegen`], which:
//!
//! 1. Parses + validates the TOML against [`Manifest`].
//! 2. Emits a `#[link_section]`-attributed static that lands the TOML
//!    bytes verbatim in a wasm custom section named for the builtin.
//!    Splicer locates it with `wasmparser`.
//! 3. Emits a `mod config` with one typed accessor per declared key.
//!    Accessors call `crate::bindings::splicer::builtin_config::get::get`
//!    (the substrate import every consumer already binds) and parse
//!    the returned WAVE text into the declared type with `wasm-wave`,
//!    falling back to the manifest-declared default when the user
//!    didn't set the key in YAML. Hardcoding defaults at the Rust
//!    call site is no longer possible — `manifest.toml` is the single
//!    source of truth.
//!
//! Values on the wire are WAVE text (see the `wasm-wave` crate): the
//! standard textual encoding for component-model values. Primitives
//! stringify identically to what YAML scalars produce
//! (`42`, `"hi"`, `true`), so the on-wire representation didn't change
//! for the scalar case; compounds (`list<u32>`, `enum { a, b }`,
//! records, tuples) now have a canonical encoding rather than each
//! builtin inventing its own.

use serde::{Deserialize, Serialize};

pub mod build_helper;
pub mod typ;

pub use typ::{parse_wit_type, ParseError, TypeAst};

/// Common prefix for the wasm custom section that carries each
/// builtin's `manifest.toml`. The full section name is
/// `{MANIFEST_SECTION_PREFIX}{builtin-name}` — e.g.
/// `splicer-builtin-manifest/hello-tier1`. Per-builtin naming lets a
/// composed pipeline carry many manifests without ambiguity.
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
    /// Which splicer tier this builtin participates in. Required;
    /// tier-3 implies forward (calls pass through to the wrapped
    /// target), tier-4 implies virtualize (the strategy replaces
    /// the target).
    pub tier: Tier,
}

/// Splicer's tier classification. Manifest reads `tier = 1` through
/// `tier = 4`; `try_from`/`into` `u8` keeps the on-disk form numeric
/// while enforcing the 1..=4 range at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum Tier {
    /// Name-only hooks (before/after, no payload). Wasm builtin.
    Tier1,
    /// Typed observation hooks (lifted args/result as cells). Wasm builtin.
    Tier2,
    /// Forward strategy — passes through to the wrapped target,
    /// optionally transforming. Source crate, splicer-built per target.
    Tier3,
    /// Virtualize strategy — replaces the wrapped target. Source
    /// crate, splicer-built per target.
    Tier4,
}

impl TryFrom<u8> for Tier {
    type Error = String;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            1 => Ok(Self::Tier1),
            2 => Ok(Self::Tier2),
            3 => Ok(Self::Tier3),
            4 => Ok(Self::Tier4),
            _ => Err(format!("invalid tier {v}; expected 1, 2, 3, or 4")),
        }
    }
}

impl From<Tier> for u8 {
    fn from(t: Tier) -> u8 {
        match t {
            Tier::Tier1 => 1,
            Tier::Tier2 => 2,
            Tier::Tier3 => 3,
            Tier::Tier4 => 4,
        }
    }
}

impl Tier {
    /// Human-readable single-word label suitable for `splicer
    /// builtin` output: `"name-only"`, `"observe"`, `"forward"`,
    /// `"virtualize"`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tier1 => "name-only",
            Self::Tier2 => "observe",
            Self::Tier3 => "forward",
            Self::Tier4 => "virtualize",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigKey {
    /// Key as it appears in the splice-config YAML's `config:` map.
    pub name: String,
    /// WIT type expression as text. Parsed by [`parse_wit_type`] into
    /// a [`TypeAst`] (mirroring `wasm_wave::value::Type`'s vocabulary
    /// — primitives + `list<T>`, `option<T>`, `tuple<...>`, and
    /// `enum { ... }`). Splicer rejects YAML values that don't match
    /// at splice time; the codegen'd accessor parses the substrate's
    /// WAVE text into the same type at runtime.
    ///
    /// `option<T>` semantics: TOML/YAML have no null we accept, so
    /// `none` is signaled by *omitting* the key in YAML — the
    /// codegen'd accessor falls back to the manifest default in that
    /// case. Setting the key to any value emits `some(...)`.
    #[serde(rename = "type")]
    pub wit_type: String,
    /// Default in TOML-native form (number, string, bool, array, table).
    /// Converted to canonical WAVE text via the declared type at
    /// validate time. Stored as TOML rather than a pre-stringified
    /// WAVE blob so authors get TOML's natural scalar/array shape.
    pub default: toml::Value,
    /// Per-key documentation rendered by `splicer builtin <name>`.
    pub doc: String,
    /// Enum-parse policy. When set on an `enum { ... }`-typed key,
    /// splice-time YAML matching accepts any case-folded form and
    /// canonicalizes to the manifest-declared case before encoding
    /// as WAVE. No effect on non-enum types.
    #[serde(default)]
    pub case_insensitive: bool,
}

impl ConfigKey {
    /// Parse [`ConfigKey::wit_type`]. Called by both validation
    /// (host-side) and codegen (build-time).
    pub fn parsed_type(&self) -> Result<TypeAst, ParseError> {
        parse_wit_type(&self.wit_type)
    }

    /// Human-readable rendering of the default, suitable for CLI
    /// output. Mirrors WAVE's text encoding: strings quoted,
    /// floats always include `.0`, enum cases bare.
    pub fn default_display(&self) -> String {
        default_to_wave_loose(&self.default)
    }
}

#[derive(Debug)]
pub enum ManifestError {
    Parse(String),
    /// `type` couldn't be parsed.
    BadType {
        key: String,
        message: String,
    },
    /// `default` value didn't fit the declared type.
    BadDefault {
        key: String,
        message: String,
    },
    DuplicateKey(String),
    /// `case_insensitive = true` on a non-enum type.
    CaseInsensitiveOnNonEnum(String),
    /// Key name isn't a valid kebab/snake-case identifier — would
    /// break Rust codegen and/or YAML round-tripping.
    BadKeyName(String),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "manifest.toml parse error: {e}"),
            Self::BadType { key, message } => {
                write!(f, "key '{key}': bad type expression: {message}")
            }
            Self::BadDefault { key, message } => {
                write!(
                    f,
                    "key '{key}': default does not fit declared type: {message}"
                )
            }
            Self::DuplicateKey(key) => write!(f, "duplicate config key declared: '{key}'"),
            Self::CaseInsensitiveOnNonEnum(key) => write!(
                f,
                "key '{key}': `case_insensitive` is only meaningful on `enum {{ ... }}` keys"
            ),
            Self::BadKeyName(key) => write!(
                f,
                "key {key:?} is not a valid identifier — names must start with \
                 a letter or `_` and contain only ASCII letters, digits, `_`, and `-`"
            ),
        }
    }
}

/// True iff `name` is safe to use as a YAML key, a TOML key, and a
/// Rust identifier (with `-` → `_` substitution). Disallows
/// whitespace, quotes, control characters, and other punctuation
/// that would break codegen or YAML round-tripping.
fn is_valid_key_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl std::error::Error for ManifestError {}

impl Manifest {
    /// Parse TOML source and validate each key's type + default. Same
    /// routine runs in builtin `build.rs` (catches errors at build
    /// time) and in splicer's extractor.
    pub fn from_toml(src: &str) -> Result<Self, ManifestError> {
        let m: Manifest = toml::from_str(src).map_err(|e| ManifestError::Parse(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        let mut seen = std::collections::BTreeSet::new();
        for k in &self.keys {
            if !is_valid_key_name(&k.name) {
                return Err(ManifestError::BadKeyName(k.name.clone()));
            }
            if !seen.insert(k.name.clone()) {
                return Err(ManifestError::DuplicateKey(k.name.clone()));
            }
            let ty = k.parsed_type().map_err(|e| ManifestError::BadType {
                key: k.name.clone(),
                message: e.to_string(),
            })?;
            // Re-encode the default through the declared type. If it
            // doesn't fit, we bail. This same routine produces the
            // WAVE text the substrate carries at runtime.
            encode_toml_as_wave(&k.default, &ty, k.case_insensitive).map_err(|e| {
                ManifestError::BadDefault {
                    key: k.name.clone(),
                    message: e,
                }
            })?;
            if k.case_insensitive && !matches!(ty, TypeAst::Enum { .. }) {
                return Err(ManifestError::CaseInsensitiveOnNonEnum(k.name.clone()));
            }
        }
        Ok(())
    }

    /// Validate a YAML-derived value (already round-tripped through
    /// `toml::Value` by splicer's parser) against `key`'s declared
    /// type. Returns the canonical WAVE text on success — splicer
    /// stores that string in the substrate. `Ok(None)` if `key` isn't
    /// declared; caller decides whether unknown keys are an error.
    pub fn validate_value(&self, key: &str, value: &toml::Value) -> Result<Option<String>, String> {
        let Some(decl) = self.keys.iter().find(|k| k.name == key) else {
            return Ok(None);
        };
        let ty = decl
            .parsed_type()
            .map_err(|e| format!("internal: key '{key}' has invalid type at validate time: {e}"))?;
        encode_toml_as_wave(value, &ty, decl.case_insensitive)
            .map(Some)
            .map_err(|e| format!("value for key '{key}': {e}"))
    }
}

/// Encode a TOML scalar as canonical WAVE text without consulting a
/// declared type. Quoting + float-normalization match what
/// [`encode_toml_as_wave`] produces for the same value against an
/// inferable type, so the two paths produce identical bytes for any
/// value either of them accepts. Used by splicer's manifest-less
/// middleware fallback and by [`default_to_wave_loose`] for display.
///
/// Returns `Err` on values with no scalar-WAVE encoding (arrays,
/// tables, datetimes) — splicer's lenient-path callers surface this
/// as a clear "compound config needs a manifest" error.
pub fn loose_scalar_to_wave(v: &toml::Value) -> Result<String, String> {
    match v {
        toml::Value::String(s) => Ok(format!("\"{}\"", wave_escape_string(s))),
        toml::Value::Integer(n) => Ok(n.to_string()),
        toml::Value::Float(f) => Ok(encode_float(*f)),
        toml::Value::Boolean(b) => Ok(b.to_string()),
        toml::Value::Array(_) | toml::Value::Table(_) => {
            Err("compound value has no scalar WAVE encoding".into())
        }
        toml::Value::Datetime(_) => Err("datetime is not a WAVE-supported type".into()),
    }
}

/// Loose pretty-printer for a TOML scalar/array/table without a
/// declared type. Used only for CLI default rendering — formal
/// type-aware encoding goes through [`encode_toml_as_wave`].
fn default_to_wave_loose(v: &toml::Value) -> String {
    if let Ok(s) = loose_scalar_to_wave(v) {
        return s;
    }
    match v {
        toml::Value::Array(xs) => {
            let parts: Vec<String> = xs.iter().map(default_to_wave_loose).collect();
            format!("[{}]", parts.join(", "))
        }
        toml::Value::Table(t) => {
            let parts: Vec<String> = t
                .iter()
                .map(|(k, v)| format!("{k}: {}", default_to_wave_loose(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        toml::Value::Datetime(d) => d.to_string(),
        // Scalars were handled above; this branch is unreachable.
        _ => unreachable!("scalar handled by loose_scalar_to_wave"),
    }
}

/// Convert a TOML value to canonical WAVE text against `ty`. Recursive
/// so list/tuple/option/enum all work. Numerics and bools land
/// unquoted; strings + enum cases follow WAVE's quoting / bare-ident
/// rules. Splicer's YAML-side helper round-trips `serde_yaml::Value`
/// through `toml::Value` first and reuses this for free.
pub fn encode_toml_as_wave(
    value: &toml::Value,
    ty: &TypeAst,
    case_insensitive: bool,
) -> Result<String, String> {
    match (ty, value) {
        (TypeAst::Bool, toml::Value::Boolean(b)) => Ok(b.to_string()),

        (TypeAst::U8, toml::Value::Integer(n)) => encode_uint(*n, u8::MAX as i64),
        (TypeAst::U16, toml::Value::Integer(n)) => encode_uint(*n, u16::MAX as i64),
        (TypeAst::U32, toml::Value::Integer(n)) => encode_uint(*n, u32::MAX as i64),
        (TypeAst::U64, toml::Value::Integer(n)) => {
            if *n < 0 {
                Err(format!("value {n} is negative, expected u64"))
            } else {
                Ok(n.to_string())
            }
        }

        (TypeAst::S8, toml::Value::Integer(n)) => encode_sint(*n, i8::MIN as i64, i8::MAX as i64),
        (TypeAst::S16, toml::Value::Integer(n)) => {
            encode_sint(*n, i16::MIN as i64, i16::MAX as i64)
        }
        (TypeAst::S32, toml::Value::Integer(n)) => {
            encode_sint(*n, i32::MIN as i64, i32::MAX as i64)
        }
        (TypeAst::S64, toml::Value::Integer(n)) => Ok(n.to_string()),

        (TypeAst::F32, toml::Value::Float(f)) => encode_f32(*f),
        (TypeAst::F32, toml::Value::Integer(n)) => encode_f32(*n as f64),
        (TypeAst::F64, toml::Value::Float(f)) => Ok(encode_float(*f)),
        (TypeAst::F64, toml::Value::Integer(n)) => Ok(encode_float(*n as f64)),

        (TypeAst::Char, toml::Value::String(s)) => {
            let mut it = s.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => Ok(format!("'{}'", c)),
                _ => Err(format!("expected single character, got {s:?}")),
            }
        }
        (TypeAst::String, toml::Value::String(s)) => Ok(format!("\"{}\"", wave_escape_string(s))),

        (TypeAst::Enum { cases }, toml::Value::String(s)) => {
            let matched = if case_insensitive {
                cases.iter().find(|c| c.eq_ignore_ascii_case(s))
            } else {
                cases.iter().find(|c| c.as_str() == s.as_str())
            };
            match matched {
                Some(canonical) => Ok(canonical.clone()),
                None => Err(format!(
                    "{s:?} is not in declared enum cases [{}]",
                    cases.join(", "),
                )),
            }
        }

        (TypeAst::List(elem), toml::Value::Array(xs)) => {
            let parts: Result<Vec<_>, _> = xs
                .iter()
                .map(|x| encode_toml_as_wave(x, elem, case_insensitive))
                .collect();
            Ok(format!("[{}]", parts?.join(", ")))
        }

        (TypeAst::Tuple(elems), toml::Value::Array(xs)) => {
            if xs.len() != elems.len() {
                return Err(format!(
                    "tuple arity mismatch: declared {}, got {}",
                    elems.len(),
                    xs.len()
                ));
            }
            let parts: Result<Vec<_>, _> = xs
                .iter()
                .zip(elems)
                .map(|(v, t)| encode_toml_as_wave(v, t, case_insensitive))
                .collect();
            Ok(format!("({})", parts?.join(", ")))
        }

        (TypeAst::Option(inner), v) => {
            // `none` is TOML-modeled as omitted — when present, treat
            // any non-null value as `some(...)`.
            Ok(format!(
                "some({})",
                encode_toml_as_wave(v, inner, case_insensitive)?
            ))
        }

        (declared, actual) => Err(format!(
            "type mismatch: declared {}, got TOML {:?}",
            declared.display(),
            toml_type_str(actual),
        )),
    }
}

/// `f32`-narrowed encoder: reject values whose magnitude wouldn't
/// survive the `f64 → f32` cast (saturate to ±inf). NaN/inf pass
/// through unchanged so authors can declare them deliberately.
fn encode_f32(f: f64) -> Result<String, String> {
    if !f.is_finite() {
        return Ok(encode_float(f));
    }
    let narrowed = f as f32;
    if !narrowed.is_finite() {
        return Err(format!(
            "value {f} is out of f32 range (would saturate to {})",
            if narrowed.is_sign_negative() {
                "-inf"
            } else {
                "inf"
            },
        ));
    }
    Ok(encode_float(f))
}

fn encode_uint(n: i64, max: i64) -> Result<String, String> {
    if n < 0 || n > max {
        Err(format!("value {n} out of range (0..={max})"))
    } else {
        Ok(n.to_string())
    }
}

fn encode_sint(n: i64, min: i64, max: i64) -> Result<String, String> {
    if n < min || n > max {
        Err(format!("value {n} out of range ({min}..={max})"))
    } else {
        Ok(n.to_string())
    }
}

/// Format a `f64` as canonical WAVE float text: whole-number values
/// gain a trailing `.0` so the type/value visually agree (`10.0`, not
/// `10`); scientific notation passes through unchanged; NaN/inf
/// produce the lowercase WAVE forms (`nan`, `inf`, `-inf`).
///
/// Rust's `f64::Display` emits `"NaN"` (capital N), which round-trips
/// fine through `f64::FromStr` but is not the WAVE spec form. Emit
/// the canonical lowercase so both ends see the same bytes.
pub fn encode_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f.is_sign_negative() { "-inf" } else { "inf" }.into();
    }
    let s = f.to_string();
    if s.contains(['.', 'e', 'E']) {
        s
    } else {
        format!("{s}.0")
    }
}

/// Escape a string body for embedding inside WAVE's `"..."` literal:
/// `"`, `\`, common control characters, and any other sub-`0x20` byte
/// gets the `\xNN` / `\u{NN}` treatment. The returned string does NOT
/// include the surrounding double quotes — callers add those.
pub fn wave_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn toml_type_str(v: &toml::Value) -> &'static str {
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

/// Combined result of [`scan_substrate_component`]: manifest sections
/// keyed by builtin name, plus whether the component imports any
/// interface in the `splicer:builtin-config` package. Computed in a
/// single `wasmparser` pass so splicer doesn't decode the wasm twice
/// to answer "does this consume the substrate?" and "what's its
/// manifest?".
#[derive(Debug, Default)]
pub struct ScanResult {
    pub manifests: Vec<(String, Manifest)>,
    pub imports_substrate: bool,
}

/// Walk `wasm_bytes` once, collecting every manifest custom section
/// and noting whether any component-import name lives under the
/// `splicer:builtin-config` package. Returns `Err` on framing /
/// manifest parse errors so the caller can render a clear diagnostic.
pub fn scan_substrate_component(wasm_bytes: &[u8]) -> Result<ScanResult, String> {
    use wasmparser::{Parser, Payload};
    let mut result = ScanResult::default();
    for payload in Parser::new(0).parse_all(wasm_bytes) {
        let payload = payload.map_err(|e| format!("wasm parse error: {e}"))?;
        match payload {
            Payload::CustomSection(reader) => {
                let Some(name) = reader.name().strip_prefix(MANIFEST_SECTION_PREFIX) else {
                    continue;
                };
                if name.is_empty() {
                    return Err(format!(
                        "custom section '{prefix}' has empty builtin-name suffix",
                        prefix = MANIFEST_SECTION_PREFIX,
                    ));
                }
                if result.manifests.iter().any(|(n, _)| n == name) {
                    return Err(format!(
                        "manifest section for builtin '{name}' appears more than once",
                    ));
                }
                let toml_src = std::str::from_utf8(reader.data()).map_err(|e| {
                    format!("manifest section for '{name}' is not valid UTF-8: {e}")
                })?;
                let manifest = Manifest::from_toml(toml_src)
                    .map_err(|e| format!("manifest for '{name}' failed to parse: {e}"))?;
                result.manifests.push((name.to_string(), manifest));
            }
            Payload::ComponentImportSection(reader) => {
                if result.imports_substrate {
                    continue;
                }
                for import in reader {
                    let import = import.map_err(|e| format!("wasm import parse error: {e}"))?;
                    if import.name.0.starts_with(SUBSTRATE_IMPORT_PREFIX) {
                        result.imports_substrate = true;
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(result)
}

/// Package prefix for splicer's config substrate. Any component
/// import whose name starts with this is taken as evidence the
/// component consumes the substrate.
const SUBSTRATE_IMPORT_PREFIX: &str = "splicer:builtin-config/";

/// Extract every embedded manifest from a wasm module or component.
/// Walks nested modules transparently — Rust's `#[link_section]`
/// emits at the core-module level; `wasmparser::Parser::parse_all`
/// surfaces any nested module's customs in one pass. Returns one
/// entry per section whose name starts with [`MANIFEST_SECTION_PREFIX`];
/// the key is the suffix (the builtin's canonical name).
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
        let toml_src = std::str::from_utf8(reader.data())
            .map_err(|e| format!("manifest section for '{name}' is not valid UTF-8: {e}"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_primitive_keys() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "buffer"
            type = "u32"
            default = 100
            doc = ""

            [[key]]
            name = "ratio"
            type = "f64"
            default = 1.5
            doc = ""

            [[key]]
            name = "label"
            type = "string"
            default = "hi"
            doc = ""

            [[key]]
            name = "on"
            type = "bool"
            default = true
            doc = ""
        "#;
        let m = Manifest::from_toml(src).unwrap();
        assert_eq!(m.keys.len(), 4);
        assert_eq!(m.keys[0].parsed_type().unwrap(), TypeAst::U32);
    }

    #[test]
    fn enum_type_with_case_insensitive() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "severity"
            type = "enum { trace, debug, info, warn, error }"
            case_insensitive = true
            default = "INFO"
            doc = ""
        "#;
        let m = Manifest::from_toml(src).unwrap();
        // Default canonicalises to declared case.
        let wave = m
            .validate_value("severity", &toml::Value::String("WARN".into()))
            .unwrap()
            .unwrap();
        assert_eq!(wave, "warn");
        let err = m
            .validate_value("severity", &toml::Value::String("loud".into()))
            .unwrap_err();
        assert!(err.contains("not in declared enum cases"), "{err}");
    }

    #[test]
    fn rejects_case_insensitive_on_non_enum() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "buffer"
            type = "u32"
            case_insensitive = true
            default = 1
            doc = ""
        "#;
        let err = Manifest::from_toml(src).unwrap_err();
        assert!(
            matches!(err, ManifestError::CaseInsensitiveOnNonEnum(_)),
            "{err}"
        );
    }

    #[test]
    fn list_of_primitives_encodes() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "denylist"
            type = "list<string>"
            default = ["a", "b"]
            doc = ""
        "#;
        let m = Manifest::from_toml(src).unwrap();
        let wave = m
            .validate_value(
                "denylist",
                &toml::Value::Array(vec![
                    toml::Value::String("1.2.3.4/32".into()),
                    toml::Value::String("5.6.7.8/32".into()),
                ]),
            )
            .unwrap()
            .unwrap();
        assert_eq!(wave, r#"["1.2.3.4/32", "5.6.7.8/32"]"#);
    }

    #[test]
    fn option_of_u32_validate_value() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "limit"
            type = "option<u32>"
            default = 1
            doc = ""
        "#;
        let m = Manifest::from_toml(src).unwrap();
        // A user-set integer round-trips through validation as
        // `some(...)`.
        let wave = m
            .validate_value("limit", &toml::Value::Integer(100))
            .unwrap()
            .unwrap();
        assert_eq!(wave, "some(100)");
        // A string value where a u32 is required still rejects under
        // the option wrapper.
        let err = m
            .validate_value("limit", &toml::Value::String("nope".into()))
            .unwrap_err();
        assert!(err.contains("type mismatch"), "{err}");
    }

    #[test]
    fn rejects_oob_u32() {
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
        let err = m
            .validate_value("buffer", &toml::Value::Integer(-1))
            .unwrap_err();
        assert!(err.contains("out of range"), "{err}");
    }

    #[test]
    fn encode_float_emits_canonical_nan_and_inf() {
        // Rust's f64::Display gives "NaN" (capital N), which would
        // break the runtime decoder's `parse::<f64>()` after the
        // ".0" normalization step. We canonicalise to WAVE form.
        assert_eq!(encode_float(f64::NAN), "nan");
        assert_eq!(encode_float(f64::INFINITY), "inf");
        assert_eq!(encode_float(f64::NEG_INFINITY), "-inf");
        // Finite values keep their stringified form, padded with `.0`
        // when whole.
        assert_eq!(encode_float(10.0), "10.0");
        assert_eq!(encode_float(1.5), "1.5");
    }

    #[test]
    fn f32_overflow_rejected_at_validate_time() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "ratio"
            type = "f32"
            default = 1.0
            doc = ""
        "#;
        let m = Manifest::from_toml(src).unwrap();
        // A value that's representable as f64 but saturates to inf
        // when narrowed to f32 must be rejected at splice time.
        let err = m
            .validate_value("ratio", &toml::Value::Float(1.0e40))
            .unwrap_err();
        assert!(err.contains("out of f32 range"), "{err}");
    }

    #[test]
    fn rejects_invalid_key_names() {
        let src = r#"
            [builtin]
            description = "x"

            [[key]]
            name = "has space"
            type = "u32"
            default = 1
            doc = ""
        "#;
        let err = Manifest::from_toml(src).unwrap_err();
        assert!(matches!(err, ManifestError::BadKeyName(_)), "{err}");

        // Newlines in TOML keys are technically allowed in quoted form
        // and would break Rust codegen.
        let src = "\
            [builtin]\n\
            description = \"x\"\n\
            \n\
            [[key]]\n\
            name = \"bad\\nnewline\"\n\
            type = \"u32\"\n\
            default = 1\n\
            doc = \"\"\n\
        ";
        let err = Manifest::from_toml(src).unwrap_err();
        assert!(matches!(err, ManifestError::BadKeyName(_)), "{err}");
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

    fn module_with_custom_section(name: &str, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\0asm");
        out.extend_from_slice(&1u32.to_le_bytes());
        let mut payload = Vec::new();
        write_leb128(&mut payload, name.len() as u64);
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(data);
        out.push(0);
        write_leb128(&mut out, payload.len() as u64);
        out.extend_from_slice(&payload);
        out
    }

    /// Spec-conformance tripwire: every WAVE string our encoder emits
    /// must round-trip cleanly through `wasm_wave::from_str` against
    /// the same type. Catches drift between our hand-rolled encoder
    /// and the canonical parser, and (by extension) between the
    /// codegen's hand-rolled runtime decoder and the spec.
    #[test]
    fn encoder_round_trips_through_wasm_wave() {
        use wasm_wave::value::{Type as WaveType, Value};
        use wasm_wave::wasm::{WasmTypeKind, WasmValue};

        let cases: &[(&str, toml::Value, WaveType)] = &[
            (
                "u32",
                toml::Value::Integer(42),
                WaveType::simple(WasmTypeKind::U32).unwrap(),
            ),
            (
                "u64",
                toml::Value::Integer(99),
                WaveType::simple(WasmTypeKind::U64).unwrap(),
            ),
            (
                "s32",
                toml::Value::Integer(-7),
                WaveType::simple(WasmTypeKind::S32).unwrap(),
            ),
            (
                "f64",
                toml::Value::Float(1.5),
                WaveType::simple(WasmTypeKind::F64).unwrap(),
            ),
            (
                "f64-int",
                toml::Value::Integer(10),
                WaveType::simple(WasmTypeKind::F64).unwrap(),
            ),
            (
                "bool",
                toml::Value::Boolean(true),
                WaveType::simple(WasmTypeKind::Bool).unwrap(),
            ),
            (
                "string",
                toml::Value::String("hello, \"world\"\n".into()),
                WaveType::simple(WasmTypeKind::String).unwrap(),
            ),
        ];
        for (label, val, ty) in cases {
            let wave = encode_toml_as_wave(val, &lookalike_typeast(ty), false)
                .unwrap_or_else(|e| panic!("{label}: encode: {e}"));
            let parsed: Value = wasm_wave::from_str(ty, &wave)
                .unwrap_or_else(|e| panic!("{label}: wasm-wave parse of {wave:?}: {e}"));
            // Spot-check the typed value matches what we encoded.
            match (val, parsed.kind()) {
                (toml::Value::Boolean(b), WasmTypeKind::Bool) => {
                    assert_eq!(*b, parsed.unwrap_bool())
                }
                (toml::Value::Integer(n), WasmTypeKind::U32) => {
                    assert_eq!(*n as u32, parsed.unwrap_u32())
                }
                (toml::Value::Integer(n), WasmTypeKind::U64) => {
                    assert_eq!(*n as u64, parsed.unwrap_u64())
                }
                (toml::Value::Integer(n), WasmTypeKind::S32) => {
                    assert_eq!(*n as i32, parsed.unwrap_s32())
                }
                (toml::Value::Float(f), WasmTypeKind::F64) => assert_eq!(*f, parsed.unwrap_f64()),
                (toml::Value::Integer(n), WasmTypeKind::F64) => {
                    assert_eq!(*n as f64, parsed.unwrap_f64())
                }
                (toml::Value::String(s), WasmTypeKind::String) => {
                    assert_eq!(s.as_str(), &*parsed.unwrap_string())
                }
                (other, kind) => panic!("{label}: unexpected pairing {other:?} vs {kind:?}"),
            }
        }

        // Enums separately — the wasm-wave Type has to know the cases.
        let enum_ty = WaveType::enum_ty(["trace", "debug", "info"]).unwrap();
        let wave = encode_toml_as_wave(
            &toml::Value::String("info".into()),
            &TypeAst::Enum {
                cases: vec!["trace".into(), "debug".into(), "info".into()],
            },
            false,
        )
        .unwrap();
        let parsed: Value = wasm_wave::from_str(&enum_ty, &wave).unwrap();
        assert_eq!(parsed.unwrap_enum(), "info");
    }

    /// Compound-type encode paths exist for forward-compat; this
    /// pins them to canonical WAVE even though no builtin currently
    /// declares a compound-typed key. If/when codegen gains compound
    /// support, this is the safety rail.
    #[test]
    fn compound_encoders_round_trip_through_wasm_wave() {
        use wasm_wave::value::{Type as WaveType, Value};
        use wasm_wave::wasm::{WasmTypeKind, WasmValue};

        // list<u32>
        let list_ty = WaveType::list(WaveType::simple(WasmTypeKind::U32).unwrap());
        let list_ast = TypeAst::List(Box::new(TypeAst::U32));
        let wave = encode_toml_as_wave(
            &toml::Value::Array(vec![
                toml::Value::Integer(1),
                toml::Value::Integer(2),
                toml::Value::Integer(3),
            ]),
            &list_ast,
            false,
        )
        .expect("encode list");
        let parsed: Value = wasm_wave::from_str(&list_ty, &wave).expect("parse list");
        let items: Vec<u32> = parsed.unwrap_list().map(|v| v.unwrap_u32()).collect();
        assert_eq!(items, vec![1, 2, 3]);

        // option<string> with a present value
        let opt_ty = WaveType::option(WaveType::simple(WasmTypeKind::String).unwrap());
        let opt_ast = TypeAst::Option(Box::new(TypeAst::String));
        let wave = encode_toml_as_wave(&toml::Value::String("hi".into()), &opt_ast, false)
            .expect("encode option");
        let parsed: Value = wasm_wave::from_str(&opt_ty, &wave).expect("parse option");
        let inner = parsed.unwrap_option().expect("some");
        assert_eq!(&*inner.unwrap_string(), "hi");

        // tuple<u32, string>
        let tup_ty = WaveType::tuple(
            [
                WaveType::simple(WasmTypeKind::U32).unwrap(),
                WaveType::simple(WasmTypeKind::String).unwrap(),
            ]
            .as_slice(),
        )
        .expect("tuple type");
        let tup_ast = TypeAst::Tuple(vec![TypeAst::U32, TypeAst::String]);
        let wave = encode_toml_as_wave(
            &toml::Value::Array(vec![
                toml::Value::Integer(42),
                toml::Value::String("ok".into()),
            ]),
            &tup_ast,
            false,
        )
        .expect("encode tuple");
        let parsed: Value = wasm_wave::from_str(&tup_ty, &wave).expect("parse tuple");
        let mut elems = parsed.unwrap_tuple();
        assert_eq!(elems.next().unwrap().unwrap_u32(), 42);
        assert_eq!(&*elems.next().unwrap().unwrap_string(), "ok");
        assert!(elems.next().is_none());
    }

    /// Map a wasm-wave `Type` back to our `TypeAst`. Lossy in general
    /// (records/variants have names we lose) but sufficient for the
    /// scalar + enum cases the conformance test covers.
    fn lookalike_typeast(ty: &wasm_wave::value::Type) -> TypeAst {
        use wasm_wave::wasm::{WasmType, WasmTypeKind};
        match ty.kind() {
            WasmTypeKind::Bool => TypeAst::Bool,
            WasmTypeKind::U8 => TypeAst::U8,
            WasmTypeKind::U16 => TypeAst::U16,
            WasmTypeKind::U32 => TypeAst::U32,
            WasmTypeKind::U64 => TypeAst::U64,
            WasmTypeKind::S8 => TypeAst::S8,
            WasmTypeKind::S16 => TypeAst::S16,
            WasmTypeKind::S32 => TypeAst::S32,
            WasmTypeKind::S64 => TypeAst::S64,
            WasmTypeKind::F32 => TypeAst::F32,
            WasmTypeKind::F64 => TypeAst::F64,
            WasmTypeKind::Char => TypeAst::Char,
            WasmTypeKind::String => TypeAst::String,
            other => panic!("conformance test doesn't cover {other:?}"),
        }
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
}
