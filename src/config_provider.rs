//! Splice-time patcher for the `splicer:builtin-config` provider
//! template.
//!
//! `build_provider` loads the template bytes (override → cache → OCI),
//! serializes the caller's KV map, and overwrites the bytes after the
//! magic sentinel that `builtins/config-provider/` plants in its
//! `static SPLICER_CONFIG_BLOB`. The data segment's total length is
//! preserved; the in-component parser uses the length field to bound
//! iteration so trailing padding stays inert.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

use crate::builtins;
use crate::parse::config::Injection;

// `BUILTIN_CONFIG_*` consts generated from `wit/builtin-config/world.wit`.
include!(concat!(env!("OUT_DIR"), "/builtin_config_constants.rs"));

// Wire-format constants + codec shared with the provider template.
#[path = "../builtins/config-provider/src/wire_format.rs"]
mod wire_format;

use wire_format::{serialize_table, CAPACITY, LEN_PREFIX_BYTES, MAGIC_BYTES, MAGIC_LEN};

/// Provider template's `builtins/<dir>` name. Listed in
/// `builtins::INTERNAL_BUILTINS` so it can't be referenced from YAML.
pub(crate) const PROVIDER_BUILTIN_NAME: &str = "config-provider";

const MAX_PAYLOAD: usize = CAPACITY - MAGIC_LEN - LEN_PREFIX_BYTES;

/// If the materialized builtin imports `splicer:builtin-config`,
/// build a patched provider with `injection.builtin_config` baked in,
/// write it under `<splits_dir>/builtins/<wac_var>-config.wasm`, and
/// stamp the resulting path onto `injection.config_provider_path`.
/// No-ops when the injection isn't a builtin, has no materialized
/// path yet, or already has a provider path set.
///
/// Errors when the builtin doesn't import the substrate but the YAML
/// still set a non-empty `config:` block — silently dropping config
/// would hide a typo or a misrouted builtin name. An empty `config:`
/// against a non-consumer is fine.
pub fn ensure_provider_for(injection: &mut Injection, splits_dir: &Path) -> Result<()> {
    if injection.config_provider_path.is_some() {
        return Ok(());
    }
    let Some(builtin_path) = injection.path.as_deref() else {
        return Ok(());
    };
    let bytes = std::fs::read(builtin_path)
        .with_context(|| format!("Failed to read materialized builtin '{}'", builtin_path))?;
    // One wasmparser walk covers both questions: does the component
    // import the substrate, and what manifest sections does it ship?
    let scan = builtin_manifest::scan_substrate_component(&bytes).map_err(|e| {
        anyhow::anyhow!(
            "injection '{name}': failed to scan builtin bytes: {e}",
            name = injection.name,
        )
    })?;
    if !scan.imports_substrate {
        if !injection.builtin_config.is_empty() {
            let mut keys: Vec<&str> = injection
                .builtin_config
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort();
            anyhow::bail!(
                "injection '{name}' set `config:` keys [{keys}], but the underlying \
                 component doesn't import `splicer:builtin-config/get` — splicer has \
                 nothing to seal the values into. Either drop the `config:` block, or \
                 inject a builtin that consumes the substrate.",
                name = injection.name,
                keys = keys.join(", "),
            );
        }
        return Ok(());
    }
    let wave_values = validate_against_manifest(injection, &scan.manifests)?;
    let provider_bytes = build_provider(&wave_values).with_context(|| {
        format!(
            "Failed to build config provider for injection '{}'",
            injection.name
        )
    })?;

    // create_dir_all is defensive — `materialize_into` made the dir, but
    // a caller wiring this independently of the pipeline still works.
    let dir = splits_dir.join("builtins");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create builtins dir: {}", dir.display()))?;
    let out = dir.join(format!("{}-config.wasm", injection.name));
    std::fs::write(&out, &provider_bytes)
        .with_context(|| format!("Failed to write config provider: {}", out.display()))?;

    let out_str = out
        .to_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "config provider path contains non-UTF-8 bytes: {}",
                out.display()
            )
        })?
        .to_string();
    injection.config_provider_path = Some(out_str);
    Ok(())
}

/// Reject YAML `config:` keys that aren't declared in the builtin's
/// embedded manifest, or whose values won't parse as the declared
/// type. Splice-time hard error — the silent-fallback-to-default
/// behavior that catches typos at runtime is exactly what this
/// substitutes for.
///
/// For shipped builtins (`injection.builtin == Some(name)`) the
/// section name in the wasm must match `name` — that catches OCI
/// misrouting (e.g. the `otel-bare-metrics` tag accidentally pointing
/// at `hello-tier1`'s bytes) the same way it catches typos. Manifest
/// absence on a shipped builtin is only an error when the user
/// actually set `config:` keys.
///
/// For user-supplied middleware (`injection.builtin == None`) we
/// validate against whichever single manifest the bytes carry, or
/// skip validation if none is present (third-party authors aren't
/// required to ship one).
fn validate_against_manifest(
    injection: &Injection,
    all: &[(String, builtin_manifest::Manifest)],
) -> Result<BTreeMap<String, String>> {
    let manifest = match injection.builtin.as_deref() {
        Some(expected) => {
            if let Some((_, m)) = all.iter().find(|(n, _)| n == expected) {
                Some(m.clone())
            } else if all.is_empty() {
                // Genuinely manifest-less builtin (pre-manifests build).
                // Lenient as long as the user didn't ask for any config.
                if injection.builtin_config.is_empty() {
                    None
                } else {
                    anyhow::bail!(
                        "injection '{name}': shipped builtin '{expected}' is missing its \
                         embedded manifest, so splicer can't validate the `config:` keys \
                         you set. Rebuild against the current builtin-manifest crate.",
                        name = injection.name,
                    );
                }
            } else {
                // Bytes DO carry manifest(s), but none for the requested
                // builtin name. That's OCI misrouting / build-tag mismatch
                // — flag it unconditionally, config: block or not.
                let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
                anyhow::bail!(
                    "injection '{name}': shipped builtin '{expected}' resolved to bytes \
                     carrying manifest(s) for [{}] — wrong OCI tag or build mismatch.",
                    names.join(", "),
                    name = injection.name,
                );
            }
        }
        None => match all.len() {
            0 => None,
            1 => Some(all[0].1.clone()),
            _ => {
                let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
                anyhow::bail!(
                    "injection '{name}': bytes carry manifests for multiple builtins \
                     [{}] but the YAML didn't declare which one applies. Use \
                     `builtin: <name>` instead of `name: ...` + `path: ...` to disambiguate.",
                    names.join(", "),
                    name = injection.name,
                );
            }
        },
    };
    let Some(manifest) = manifest else {
        // Lenient path: user-supplied middleware without a manifest
        // section. We can't type-check, but we still want the config
        // values to reach the substrate so the old "ship middleware
        // without a manifest" workflow keeps working. Loose-encode
        // each scalar as WAVE text; reject compounds since we can't
        // disambiguate them without a declared type.
        let mut wave = BTreeMap::new();
        for (key, value) in &injection.builtin_config {
            let encoded = builtin_manifest::loose_scalar_to_wave(value).map_err(|e| {
                anyhow::anyhow!(
                    "injection '{name}': config key '{key}' (no manifest available): {e}",
                    name = injection.name,
                )
            })?;
            wave.insert(key.clone(), encoded);
        }
        return Ok(wave);
    };
    let mut wave: BTreeMap<String, String> = BTreeMap::new();
    let mut unknown: Vec<&str> = Vec::new();
    let mut bad: Vec<String> = Vec::new();
    for (key, value) in &injection.builtin_config {
        match manifest.validate_value(key, value) {
            Ok(Some(canonical)) => {
                wave.insert(key.clone(), canonical);
            }
            Ok(None) => unknown.push(key.as_str()),
            Err(msg) => bad.push(msg),
        }
    }
    if unknown.is_empty() && bad.is_empty() {
        return Ok(wave);
    }
    let known_keys: Vec<&str> = manifest.keys.iter().map(|k| k.name.as_str()).collect();
    let mut msg = format!(
        "injection '{name}' has invalid `config:` against its declared manifest. \
         Run `splicer builtin {bn}` to see accepted keys.",
        name = injection.name,
        bn = injection
            .builtin
            .as_deref()
            .unwrap_or(injection.name.as_str()),
    );
    if !unknown.is_empty() {
        unknown.sort();
        msg.push_str(&format!(
            "\n  unknown keys: [{}]; known keys: [{}]",
            unknown.join(", "),
            known_keys.join(", "),
        ));
    }
    for b in &bad {
        msg.push_str("\n  ");
        msg.push_str(b);
    }
    anyhow::bail!(msg);
}

/// Thin test wrapper around [`builtin_manifest::scan_substrate_component`].
/// Production code path uses `scan_substrate_component` directly so it
/// gets the manifest sections in the same wasmparser pass.
#[cfg(test)]
fn imports_substrate(bytes: &[u8]) -> bool {
    builtin_manifest::scan_substrate_component(bytes)
        .map(|s| s.imports_substrate)
        .unwrap_or(false)
}

/// Build a patched provider component with `values` baked in.
/// Returns wasm bytes ready to wire as `splicer:builtin-config/get@0.1.0`.
///
/// Fails on: unresolvable template (no override/cache/network),
/// missing-or-duplicate magic in the template (build mismatch), or
/// serialized table over the reserved capacity.
pub fn build_provider(values: &BTreeMap<String, String>) -> Result<Vec<u8>> {
    let mut bytes = builtins::load_resolved_bytes(PROVIDER_BUILTIN_NAME)
        .context("Failed to load config-provider template bytes")?;
    patch_in_place(&mut bytes, values)?;
    Ok(bytes)
}

/// Locate `MAGIC_BYTES` in `bytes` and overwrite the bytes that
/// follow with the serialized KV table.
fn patch_in_place(bytes: &mut [u8], values: &BTreeMap<String, String>) -> Result<()> {
    let payload = serialize_table(values);
    if payload.len() > MAX_PAYLOAD {
        anyhow::bail!(
            "config-provider payload of {} bytes exceeds reserved capacity of {} bytes \
             (template `CAPACITY` minus magic + length header). Either trim the config or \
             rebuild the provider with a larger `CAPACITY`.",
            payload.len(),
            MAX_PAYLOAD,
        );
    }
    let offset = find_unique_magic(bytes)?;
    // [magic][u32 LE payload_len][payload bytes][unchanged padding]
    let len_off = offset + MAGIC_LEN;
    let payload_off = len_off + LEN_PREFIX_BYTES;
    let payload_end = payload_off + payload.len();
    bytes[len_off..len_off + LEN_PREFIX_BYTES]
        .copy_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes[payload_off..payload_end].copy_from_slice(&payload);
    Ok(())
}

/// Byte offset of the single occurrence of `MAGIC_BYTES`. Both zero
/// and >1 matches indicate a stale or miscompiled template.
fn find_unique_magic(bytes: &[u8]) -> Result<usize> {
    let mut found: Option<usize> = None;
    let mut i = 0;
    while i + MAGIC_LEN <= bytes.len() {
        if bytes[i..i + MAGIC_LEN] == MAGIC_BYTES {
            if found.is_some() {
                anyhow::bail!(
                    "config-provider template has multiple `MAGIC_BYTES` matches. The byte-scan \
                     patcher assumes exactly one. Check the template build."
                );
            }
            found = Some(i);
            i += MAGIC_LEN;
        } else {
            i += 1;
        }
    }
    found.ok_or_else(|| {
        anyhow::anyhow!(
            "config-provider template is missing the magic sentinel. The template was \
             either built against an out-of-date `MAGIC_BYTES`, or the compiler elided \
             the KV buffer. Rebuild builtins/config-provider with the in-tree sources."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_provider::wire_format::deserialize_table;
    use crate::parse::config::Injection;

    fn make_blob(payload: &[u8]) -> Vec<u8> {
        // Mimics the template's static layout: random prefix, magic,
        // length header, padding to CAPACITY, random suffix. The
        // prefix + suffix make sure the byte-scan ignores everything
        // outside the magic-anchored window.
        let prefix = b"PREFIX_BYTES_BEFORE_MAGIC".to_vec();
        let suffix = b"SUFFIX_BYTES_AFTER_TEMPLATE".to_vec();
        let mut buf = Vec::with_capacity(prefix.len() + CAPACITY + suffix.len());
        buf.extend_from_slice(&prefix);
        buf.extend_from_slice(&MAGIC_BYTES);
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(payload);
        // Pad to CAPACITY total (relative to MAGIC start).
        let written = MAGIC_LEN + LEN_PREFIX_BYTES + payload.len();
        buf.extend(std::iter::repeat_n(0xAA, CAPACITY - written));
        buf.extend_from_slice(&suffix);
        buf
    }

    /// Round-trip via the same `deserialize_table` the provider runtime
    /// uses — wire-format drift surfaces here, not just in the live
    /// builtin.
    fn parse_back(bytes: &[u8]) -> std::collections::HashMap<String, String> {
        let off = find_unique_magic(bytes).expect("magic present");
        let body = &bytes[off + MAGIC_LEN..];
        let payload_len = u32::from_le_bytes(body[..LEN_PREFIX_BYTES].try_into().unwrap()) as usize;
        let payload = &body[LEN_PREFIX_BYTES..LEN_PREFIX_BYTES + payload_len];
        deserialize_table(payload)
    }

    #[test]
    fn round_trip_empty() {
        let mut blob = make_blob(&[]);
        patch_in_place(&mut blob, &BTreeMap::new()).expect("patch");
        let parsed = parse_back(&blob);
        assert!(parsed.is_empty());
    }

    #[test]
    fn round_trip_multi_key() {
        let mut blob = make_blob(&[]);
        let mut values = BTreeMap::new();
        values.insert("buffer".to_string(), "100".to_string());
        values.insert("flush_after_seconds".to_string(), "10.0".to_string());
        values.insert("note".to_string(), "value with spaces and 🎉".to_string());
        patch_in_place(&mut blob, &values).expect("patch");
        let parsed = parse_back(&blob);
        assert_eq!(parsed.len(), values.len());
        for (k, v) in &values {
            assert_eq!(parsed.get(k), Some(v));
        }
    }

    #[test]
    fn missing_key_after_patch_yields_nothing() {
        let mut blob = make_blob(&[]);
        let mut values = BTreeMap::new();
        values.insert("buffer".to_string(), "100".to_string());
        patch_in_place(&mut blob, &values).expect("patch");
        let parsed = parse_back(&blob);
        assert!(!parsed.contains_key("not-set"));
    }

    #[test]
    fn patch_preserves_outside_window() {
        let mut blob = make_blob(&[]);
        let original = blob.clone();
        let mut values = BTreeMap::new();
        values.insert("k".to_string(), "v".to_string());
        patch_in_place(&mut blob, &values).expect("patch");

        // Bytes before MAGIC and after MAGIC + CAPACITY must be unchanged.
        let magic_off = find_unique_magic(&blob).expect("magic");
        let template_end = magic_off + CAPACITY;
        assert_eq!(&blob[..magic_off], &original[..magic_off]);
        assert_eq!(&blob[template_end..], &original[template_end..]);
    }

    #[test]
    fn overflow_rejects_cleanly() {
        let mut blob = make_blob(&[]);
        let mut values = BTreeMap::new();
        values.insert("k".to_string(), "a".repeat(MAX_PAYLOAD));
        let err = patch_in_place(&mut blob, &values).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("exceeds reserved capacity"), "{msg}");
    }

    #[test]
    fn missing_magic_surfaces_clear_error() {
        let bytes = vec![0u8; 1024];
        let err = find_unique_magic(&bytes).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("missing the magic sentinel"), "{msg}");
    }

    #[test]
    fn duplicate_magic_surfaces_clear_error() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC_BYTES);
        bytes.extend_from_slice(&[0u8; 64]);
        bytes.extend_from_slice(&MAGIC_BYTES);
        let err = find_unique_magic(&bytes).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("multiple `MAGIC_BYTES` matches"), "{msg}");
    }

    /// Regression: load the actual built template and confirm
    /// `MAGIC_BYTES` appears exactly once. An earlier template had a
    /// separate `const MAGIC` referenced at runtime, which rustc/lld
    /// lowered to a second addressable copy in the data section. Run
    /// with `cargo test -- --ignored` (or
    /// `SPLICER_BUILTINS_DIR=assets/builtins` invoking by name).
    #[test]
    #[ignore = "needs built/cached/registry-resolvable config-provider template"]
    fn built_provider_has_unique_magic() {
        build_provider(&BTreeMap::new())
            .expect("template must resolve and have exactly one MAGIC_BYTES match");
    }

    /// Smoke: a real shipped builtin (`hello-tier1`) trips
    /// `imports_substrate` and `ensure_provider_for` writes a working
    /// patched provider for it.
    #[test]
    #[ignore = "needs built/cached/registry-resolvable hello-tier1 + config-provider"]
    fn hello_tier1_smoke() {
        let hello_bytes =
            crate::builtins::load_resolved_bytes("hello-tier1").expect("hello-tier1 must resolve");
        assert!(imports_substrate(&hello_bytes));

        let splits = tempfile::tempdir().unwrap();
        let builtin_dir = splits.path().join("builtins");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        let hello_path = builtin_dir.join("hello-tier1.wasm");
        std::fs::write(&hello_path, &hello_bytes).unwrap();

        let mut inj = Injection::from_path("hello-tier1", hello_path.to_str().unwrap());
        inj.builtin_config.insert(
            "greeting".to_string(),
            toml::Value::String("wired-up-end-to-end".to_string()),
        );

        ensure_provider_for(&mut inj, splits.path()).expect("ensure_provider_for");
        let provider = inj.config_provider_path.as_deref().expect("provider path");
        let patched = std::fs::read(provider).expect("provider file");
        let parsed = parse_back(&patched);
        // Substrate now carries WAVE-text, so the stored value is the
        // quoted form. The builtin's codegen'd accessor strips the
        // quotes via `wasm_wave::from_str` at runtime.
        assert_eq!(
            parsed.get("greeting").map(String::as_str),
            Some("\"wired-up-end-to-end\"")
        );
    }

    // ── imports_substrate / ensure_provider_for ─────────────────────

    const CONSUMER_WAT: &str = r#"(component
        (import "splicer:builtin-config/get@0.1.0" (instance
            (export "get" (func (param "key" string) (result (option string))))
        ))
    )"#;

    const NON_CONSUMER_WAT: &str = r#"(component
        (import "wasi:http/handler@0.3.0" (instance
            (export "handle" (func (param "req" u32) (result u32)))
        ))
    )"#;

    #[test]
    fn imports_substrate_detects_consumer() {
        let bytes = wat::parse_str(CONSUMER_WAT).expect("wat");
        assert!(imports_substrate(&bytes));
    }

    #[test]
    fn imports_substrate_rejects_non_consumer() {
        let bytes = wat::parse_str(NON_CONSUMER_WAT).expect("wat");
        assert!(!imports_substrate(&bytes));
    }

    #[test]
    fn imports_substrate_tolerates_garbage() {
        // Decode failure must not panic out of catch_unwind.
        assert!(!imports_substrate(b"not a wasm component"));
    }

    /// End-to-end via a synthetic template blob — magic-anchored window
    /// gets patched, decodes back to the input values.
    #[test]
    fn ensure_provider_for_writes_patched_component() {
        let mut template = Vec::new();
        template.extend_from_slice(b"\0asm\x01\x00\x00\x00fake-prefix-");
        template.extend_from_slice(&MAGIC_BYTES);
        template.extend_from_slice(&0u32.to_le_bytes());
        template.extend(std::iter::repeat_n(
            0xAA,
            CAPACITY - MAGIC_LEN - LEN_PREFIX_BYTES,
        ));
        template.extend_from_slice(b"-fake-suffix");

        let consumer_bytes = wat::parse_str(CONSUMER_WAT).expect("wat");
        let splits = tempfile::tempdir().unwrap();
        let builtin_dir = splits.path().join("builtins");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        let builtin_path = builtin_dir.join("fake-consumer.wasm");
        std::fs::write(&builtin_path, &consumer_bytes).unwrap();

        // `with_fake_builtins` only plants magic-bytes fixtures; this
        // test needs the richer template, so override the env directly.
        let override_dir = tempfile::tempdir().unwrap();
        std::fs::write(override_dir.path().join("config-provider.wasm"), &template).unwrap();
        let _guard = EnvGuard::set("SPLICER_BUILTINS_DIR", override_dir.path());

        let mut inj = Injection::from_path("metrics", builtin_path.to_str().unwrap());
        inj.builtin_config
            .insert("buffer".into(), toml::Value::Integer(100));
        inj.builtin_config
            .insert("flush_after_seconds".into(), toml::Value::Float(10.0));

        ensure_provider_for(&mut inj, splits.path()).expect("ensure");

        let path = inj
            .config_provider_path
            .as_deref()
            .expect("provider path stamped");
        let patched = std::fs::read(path).expect("provider written");
        let parsed = parse_back(&patched);
        // No manifest in the synthetic wat → lenient loose-encoding
        // produces canonical WAVE text for the scalars.
        assert_eq!(parsed.get("buffer").map(String::as_str), Some("100"));
        assert_eq!(
            parsed.get("flush_after_seconds").map(String::as_str),
            Some("10.0")
        );
    }

    /// Empty `inj.builtin_config` against a substrate consumer still
    /// writes a provider (with `count = 0`); the user's `builtin: <name>`
    /// without any `config:` path must keep working.
    #[test]
    fn ensure_provider_for_emits_provider_with_empty_config() {
        let mut template = Vec::new();
        template.extend_from_slice(b"\0asm\x01\x00\x00\x00fake-prefix-");
        template.extend_from_slice(&MAGIC_BYTES);
        template.extend_from_slice(&0u32.to_le_bytes());
        template.extend(std::iter::repeat_n(
            0xAA,
            CAPACITY - MAGIC_LEN - LEN_PREFIX_BYTES,
        ));

        let consumer_bytes = wat::parse_str(CONSUMER_WAT).expect("wat");
        let splits = tempfile::tempdir().unwrap();
        let builtin_dir = splits.path().join("builtins");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        let builtin_path = builtin_dir.join("fake-consumer.wasm");
        std::fs::write(&builtin_path, &consumer_bytes).unwrap();

        let override_dir = tempfile::tempdir().unwrap();
        std::fs::write(override_dir.path().join("config-provider.wasm"), &template).unwrap();
        let _guard = EnvGuard::set("SPLICER_BUILTINS_DIR", override_dir.path());

        let mut inj = Injection::from_path("metrics", builtin_path.to_str().unwrap());
        assert!(inj.builtin_config.is_empty());

        ensure_provider_for(&mut inj, splits.path()).expect("ensure");

        let path = inj.config_provider_path.as_deref().expect("provider path");
        let patched = std::fs::read(path).expect("provider written");
        let parsed = parse_back(&patched);
        assert!(parsed.is_empty());
        assert!(!parsed.contains_key("buffer"));
    }

    /// A non-consumer builtin gets no provider — `inj.config_provider_path`
    /// stays `None` and no file is written.
    #[test]
    fn ensure_provider_for_skips_non_consumer() {
        let bytes = wat::parse_str(NON_CONSUMER_WAT).expect("wat");
        let splits = tempfile::tempdir().unwrap();
        let builtin_dir = splits.path().join("builtins");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        let builtin_path = builtin_dir.join("hello.wasm");
        std::fs::write(&builtin_path, &bytes).unwrap();

        let mut inj = Injection::from_path("hello", builtin_path.to_str().unwrap());
        let _guard = EnvGuard::clear("SPLICER_BUILTINS_DIR");

        ensure_provider_for(&mut inj, splits.path()).expect("ensure");
        assert!(inj.config_provider_path.is_none());
    }

    /// Misconfig: a non-consumer builtin with non-empty `config:` must
    /// error so silent drops can't hide a typo.
    #[test]
    fn ensure_provider_for_rejects_config_on_non_consumer() {
        let bytes = wat::parse_str(NON_CONSUMER_WAT).expect("wat");
        let splits = tempfile::tempdir().unwrap();
        let builtin_dir = splits.path().join("builtins");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        let builtin_path = builtin_dir.join("hello.wasm");
        std::fs::write(&builtin_path, &bytes).unwrap();

        let mut inj = Injection::from_path("hello", builtin_path.to_str().unwrap());
        inj.builtin_config
            .insert("buffer".to_string(), toml::Value::Integer(100));
        inj.builtin_config
            .insert("flush_after_seconds".to_string(), toml::Value::Float(10.0));
        let _guard = EnvGuard::clear("SPLICER_BUILTINS_DIR");

        let err = ensure_provider_for(&mut inj, splits.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("'hello'"), "{msg}");
        // Keys are listed sorted for a deterministic error message.
        assert!(msg.contains("buffer, flush_after_seconds"), "{msg}");
        assert!(msg.contains("doesn't import"), "{msg}");
    }

    // ── manifest validation ─────────────────────────────────────────

    /// A substrate-importing component with the manifest baked in as
    /// a component-level custom section named for `builtin_name`.
    /// `wat` emits the section in-place so the resulting bytes are a
    /// single valid component (appending sections post-hoc trips
    /// `wit_component::decode`).
    fn consumer_bytes_with_manifest(builtin_name: &str, manifest_toml: &str) -> Vec<u8> {
        let escaped = escape_wat_string(manifest_toml);
        let wat = format!(
            r#"(component
                (import "splicer:builtin-config/get@0.1.0" (instance
                    (export "get" (func (param "key" string) (result (option string))))
                ))
                (@custom "{section}" "{escaped}")
            )"#,
            section = builtin_manifest::section_name_for(builtin_name),
        );
        wat::parse_str(&wat).expect("wat with embedded manifest section")
    }

    /// Escape a TOML payload for use inside a wat string literal.
    /// Wat string literals accept arbitrary bytes via `\xx` hex
    /// escapes, which sidesteps every other character's quoting rules.
    fn escape_wat_string(s: &str) -> String {
        let mut out = String::with_capacity(s.len() * 4);
        for b in s.as_bytes() {
            out.push_str(&format!("\\{b:02x}"));
        }
        out
    }

    fn write_provider_template_to(dir: &Path) {
        let mut template = Vec::new();
        template.extend_from_slice(b"\0asm\x01\x00\x00\x00fake-prefix-");
        template.extend_from_slice(&MAGIC_BYTES);
        template.extend_from_slice(&0u32.to_le_bytes());
        template.extend(std::iter::repeat_n(
            0xAA,
            CAPACITY - MAGIC_LEN - LEN_PREFIX_BYTES,
        ));
        std::fs::write(dir.join("config-provider.wasm"), &template).unwrap();
    }

    const SAMPLE_MANIFEST_TOML: &str = r#"
[builtin]
description = "test"

[[key]]
name = "buffer"
type = "u32"
default = 1
doc = "doc"

[[key]]
name = "greeting"
type = "string"
default = "hi"
doc = "doc"
"#;

    #[test]
    fn manifest_validation_rejects_unknown_key() {
        let consumer_bytes = consumer_bytes_with_manifest("metrics", SAMPLE_MANIFEST_TOML);
        let splits = tempfile::tempdir().unwrap();
        let builtin_dir = splits.path().join("builtins");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        let builtin_path = builtin_dir.join("metrics.wasm");
        std::fs::write(&builtin_path, &consumer_bytes).unwrap();

        let override_dir = tempfile::tempdir().unwrap();
        write_provider_template_to(override_dir.path());
        let _guard = EnvGuard::set("SPLICER_BUILTINS_DIR", override_dir.path());

        let mut inj = Injection::from_builtin("metrics");
        inj.path = Some(builtin_path.to_str().unwrap().to_string());
        inj.builtin_config
            .insert("bufer".into(), toml::Value::Integer(100)); // typo

        let err = ensure_provider_for(&mut inj, splits.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown keys"), "{msg}");
        assert!(msg.contains("bufer"), "{msg}");
        assert!(msg.contains("buffer"), "{msg}");
    }

    #[test]
    fn manifest_validation_rejects_type_mismatch() {
        let consumer_bytes = consumer_bytes_with_manifest("metrics", SAMPLE_MANIFEST_TOML);
        let splits = tempfile::tempdir().unwrap();
        let builtin_dir = splits.path().join("builtins");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        let builtin_path = builtin_dir.join("metrics.wasm");
        std::fs::write(&builtin_path, &consumer_bytes).unwrap();

        let override_dir = tempfile::tempdir().unwrap();
        write_provider_template_to(override_dir.path());
        let _guard = EnvGuard::set("SPLICER_BUILTINS_DIR", override_dir.path());

        let mut inj = Injection::from_builtin("metrics");
        inj.path = Some(builtin_path.to_str().unwrap().to_string());
        inj.builtin_config
            .insert("buffer".into(), toml::Value::String("ten".into())); // wrong YAML type

        let err = ensure_provider_for(&mut inj, splits.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("type mismatch"), "{msg}");
        assert!(msg.contains("u32"), "{msg}");
    }

    #[test]
    fn manifest_validation_accepts_valid_keys() {
        let consumer_bytes = consumer_bytes_with_manifest("metrics", SAMPLE_MANIFEST_TOML);
        let splits = tempfile::tempdir().unwrap();
        let builtin_dir = splits.path().join("builtins");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        let builtin_path = builtin_dir.join("metrics.wasm");
        std::fs::write(&builtin_path, &consumer_bytes).unwrap();

        let override_dir = tempfile::tempdir().unwrap();
        write_provider_template_to(override_dir.path());
        let _guard = EnvGuard::set("SPLICER_BUILTINS_DIR", override_dir.path());

        let mut inj = Injection::from_builtin("metrics");
        inj.path = Some(builtin_path.to_str().unwrap().to_string());
        inj.builtin_config
            .insert("buffer".into(), toml::Value::Integer(100));
        inj.builtin_config
            .insert("greeting".into(), toml::Value::String("hi there".into()));

        ensure_provider_for(&mut inj, splits.path()).expect("valid config passes");
        assert!(inj.config_provider_path.is_some());
    }

    #[test]
    fn manifest_validation_requires_manifest_for_shipped_builtin_with_config() {
        // Consumer wasm without a manifest blob attached.
        let consumer_bytes = wat::parse_str(CONSUMER_WAT).expect("wat");
        let splits = tempfile::tempdir().unwrap();
        let builtin_dir = splits.path().join("builtins");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        let builtin_path = builtin_dir.join("legacy.wasm");
        std::fs::write(&builtin_path, &consumer_bytes).unwrap();

        let override_dir = tempfile::tempdir().unwrap();
        write_provider_template_to(override_dir.path());
        let _guard = EnvGuard::set("SPLICER_BUILTINS_DIR", override_dir.path());

        let mut inj = Injection::from_builtin("legacy");
        inj.path = Some(builtin_path.to_str().unwrap().to_string());
        inj.builtin_config
            .insert("anything".into(), toml::Value::Integer(1));

        let err = ensure_provider_for(&mut inj, splits.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("missing its embedded manifest"), "{msg}");
    }

    #[test]
    fn manifest_validation_rejects_mismatched_builtin_name() {
        // The wasm carries a manifest section named for 'hello-tier1'
        // but the YAML references it as builtin 'metrics' — that's an
        // OCI misrouting (or build-tag mismatch) and should fail loud,
        // not silently apply the wrong manifest.
        let consumer_bytes = consumer_bytes_with_manifest("hello-tier1", SAMPLE_MANIFEST_TOML);
        let splits = tempfile::tempdir().unwrap();
        let builtin_dir = splits.path().join("builtins");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        let builtin_path = builtin_dir.join("metrics.wasm");
        std::fs::write(&builtin_path, &consumer_bytes).unwrap();

        let override_dir = tempfile::tempdir().unwrap();
        write_provider_template_to(override_dir.path());
        let _guard = EnvGuard::set("SPLICER_BUILTINS_DIR", override_dir.path());

        let mut inj = Injection::from_builtin("metrics");
        inj.path = Some(builtin_path.to_str().unwrap().to_string());
        inj.builtin_config
            .insert("buffer".into(), toml::Value::Integer(100));

        let err = ensure_provider_for(&mut inj, splits.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("wrong OCI tag or build mismatch"), "{msg}");
        assert!(msg.contains("hello-tier1"), "{msg}");
    }

    /// Same misrouting as above but with an empty `config:` block.
    /// Pre-fix, the empty-config branch silently let this through; now
    /// the manifest-name mismatch fails loudly regardless of config.
    #[test]
    fn manifest_validation_rejects_mismatched_builtin_name_even_without_config() {
        let consumer_bytes = consumer_bytes_with_manifest("hello-tier1", SAMPLE_MANIFEST_TOML);
        let splits = tempfile::tempdir().unwrap();
        let builtin_dir = splits.path().join("builtins");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        let builtin_path = builtin_dir.join("metrics.wasm");
        std::fs::write(&builtin_path, &consumer_bytes).unwrap();

        let override_dir = tempfile::tempdir().unwrap();
        write_provider_template_to(override_dir.path());
        let _guard = EnvGuard::set("SPLICER_BUILTINS_DIR", override_dir.path());

        let mut inj = Injection::from_builtin("metrics");
        inj.path = Some(builtin_path.to_str().unwrap().to_string());
        assert!(inj.builtin_config.is_empty());

        let err = ensure_provider_for(&mut inj, splits.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("wrong OCI tag or build mismatch"), "{msg}");
    }

    #[test]
    fn manifest_validation_lenient_for_user_middleware() {
        // No manifest blob, but it's a user-supplied substrate consumer
        // (no `injection.builtin` set), so we let it through with the
        // old silent-fallback behavior rather than forcing third-party
        // middleware authors to ship a manifest.
        let consumer_bytes = wat::parse_str(CONSUMER_WAT).expect("wat");
        let splits = tempfile::tempdir().unwrap();
        let builtin_dir = splits.path().join("builtins");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        let builtin_path = builtin_dir.join("user-mw.wasm");
        std::fs::write(&builtin_path, &consumer_bytes).unwrap();

        let override_dir = tempfile::tempdir().unwrap();
        write_provider_template_to(override_dir.path());
        let _guard = EnvGuard::set("SPLICER_BUILTINS_DIR", override_dir.path());

        let mut inj = Injection::from_path("user-mw", builtin_path.to_str().unwrap());
        inj.builtin_config
            .insert("anything".into(), toml::Value::Integer(1));

        ensure_provider_for(&mut inj, splits.path()).expect("user mw without manifest passes");
        assert!(inj.config_provider_path.is_some());
    }

    /// Lock + restore an env var on drop so parallel tests can't
    /// stomp each other. Used when `with_fake_builtins`'s magic-bytes
    /// fixture isn't expressive enough.
    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn lock() -> std::sync::MutexGuard<'static, ()> {
            use std::sync::Mutex;
            static LOCK: Mutex<()> = Mutex::new(());
            LOCK.lock().unwrap_or_else(|p| p.into_inner())
        }
        fn set(key: &'static str, val: &std::path::Path) -> Self {
            let lock = Self::lock();
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, val) };
            Self {
                key,
                prev,
                _lock: lock,
            }
        }
        fn clear(key: &'static str) -> Self {
            let lock = Self::lock();
            let prev = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self {
                key,
                prev,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }
}
