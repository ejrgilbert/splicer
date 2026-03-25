use crate::parse::config::Injection;
use anyhow::Context;
use cviz::model::{compatible_fingerprints, ExportInfo};
use cviz::parse::component::parse_component;
use std::collections::{BTreeMap, HashMap};
use std::fs;

/// The outcome of a single middleware contract check.
#[derive(Debug, PartialEq)]
pub enum ContractResult {
    /// The middleware exports the interface and the type fingerprints match.
    Ok,
    /// The middleware could not be validated — either no path was provided or
    /// the middleware does not export the contracted interface.  Injection can
    /// still proceed, but type safety is unconfirmed.
    Warn(String),
    /// The middleware exports the interface but with an incompatible type
    /// fingerprint.  Injection should be blocked.
    Error(String),
}

/// Check that every middleware in `to_inject` is type-compatible with the
/// interface being contracted on.
///
/// Returns one [`ContractResult`] per injection in the same order.
/// Callers are responsible for acting on the results (logging, aborting, etc.).
pub fn validate_contract(
    to_inject: &[Injection],
    interface_name: &str,
    contract_fingerprint: &Option<String>,
    checked_middlewares: &mut HashMap<String, BTreeMap<String, ExportInfo>>,
) -> Vec<ContractResult> {
    let mut results = vec![];
    for Injection { name, path } in to_inject.iter() {
        if !checked_middlewares.contains_key(name.as_str()) {
            match discover_middleware_exports(path) {
                Ok(exports) => {
                    checked_middlewares.insert(name.clone(), exports);
                }
                Err(err) => {
                    results.push(ContractResult::Warn(format!(
                        "Unable to load middleware '{name}': {err:#}"
                    )));
                    continue;
                }
            }
        }
        let exports = checked_middlewares.get(name.as_str()).unwrap();

        if let Some(ExportInfo { fingerprint, .. }) = exports.get(interface_name) {
            if !compatible_fingerprints(contract_fingerprint, fingerprint) {
                results.push(ContractResult::Error(format!(
                    "incompatible type signatures for middleware '{}' on interface '{}'\n\t{name}:\t{fingerprint:?}\n\ttarget: {contract_fingerprint:?}",
                    name, interface_name
                )));
            } else {
                results.push(ContractResult::Ok);
            }
        } else {
            results.push(ContractResult::Warn(format!(
                "Unable to validate contract for injection '{}' on interface '{}'",
                name, interface_name
            )));
        }
    }
    results
}

fn discover_middleware_exports(
    wasm_path: &Option<String>,
) -> anyhow::Result<BTreeMap<String, ExportInfo>> {
    let Some(path) = wasm_path else {
        return Ok(BTreeMap::default());
    };
    let buff = fs::read(path).with_context(|| format!("failed to read '{path}'"))?;
    let graph = parse_component(&buff)
        .with_context(|| format!("failed to parse Wasm component '{path}'"))?;
    Ok(graph.component_exports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cviz::model::ExportInfo;

    fn discover_exports_from_bytes(bytes: &[u8]) -> BTreeMap<String, ExportInfo> {
        let graph = parse_component(bytes).expect("Unable to parse component");
        graph.component_exports
    }

    fn injection(name: &str) -> Injection {
        Injection {
            name: name.to_string(),
            path: None,
        }
    }

    fn export_with_fingerprint(fp: &str) -> ExportInfo {
        ExportInfo {
            source_instance: 0,
            fingerprint: Some(fp.to_string()),
            ty: None,
        }
    }

    /// Pre-populate the middleware cache so that discovery is bypassed.
    fn cache_with(
        mw_name: &str,
        interface: &str,
        fp: &str,
    ) -> HashMap<String, BTreeMap<String, ExportInfo>> {
        let mut exports = BTreeMap::new();
        exports.insert(interface.to_string(), export_with_fingerprint(fp));
        let mut cache = HashMap::new();
        cache.insert(mw_name.to_string(), exports);
        cache
    }

    // -----------------------------------------------------------------------
    // WARN: unable to validate
    // -----------------------------------------------------------------------

    #[test]
    fn warn_when_no_path() {
        // Injection with no path → discovery returns empty → cannot validate.
        let mut cache = HashMap::new();
        let results = validate_contract(&[injection("mw")], "wasi:http/handler", &None, &mut cache);
        assert_eq!(results.len(), 1);
        assert!(
            matches!(results[0], ContractResult::Warn(_)),
            "expected Warn, got {:?}",
            results[0]
        );
    }

    #[test]
    fn warn_when_interface_not_in_exports() {
        // Middleware is in cache but does not export the contracted interface.
        let mut exports = BTreeMap::new();
        exports.insert(
            "other:pkg/other".to_string(),
            export_with_fingerprint("fp-x"),
        );
        let mut cache = HashMap::new();
        cache.insert("mw".to_string(), exports);

        let results = validate_contract(
            &[injection("mw")],
            "wasi:http/handler",
            &Some("fp-a".to_string()),
            &mut cache,
        );
        assert_eq!(results.len(), 1);
        assert!(
            matches!(results[0], ContractResult::Warn(_)),
            "expected Warn, got {:?}",
            results[0]
        );
    }

    #[test]
    fn warn_for_each_injection_without_path() {
        // Multiple injections, all without paths — each should produce a Warn.
        let injections = vec![injection("mw-a"), injection("mw-b"), injection("mw-c")];
        let mut cache = HashMap::new();
        let results = validate_contract(&injections, "wasi:http/handler", &None, &mut cache);
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| matches!(r, ContractResult::Warn(_))));
    }

    // -----------------------------------------------------------------------
    // ERROR: incompatible fingerprints
    // -----------------------------------------------------------------------

    #[test]
    fn error_when_fingerprints_incompatible() {
        // Contract expects "fp-a"; middleware exports "fp-b" → Error.
        let mut cache = cache_with("mw", "wasi:http/handler", "fp-b");
        let results = validate_contract(
            &[injection("mw")],
            "wasi:http/handler",
            &Some("fp-a".to_string()),
            &mut cache,
        );
        assert_eq!(results.len(), 1);
        assert!(
            matches!(results[0], ContractResult::Error(_)),
            "expected Error, got {:?}",
            results[0]
        );
    }

    // -----------------------------------------------------------------------
    // OK: compatible fingerprints
    // -----------------------------------------------------------------------

    #[test]
    fn ok_when_fingerprints_match() {
        let mut cache = cache_with("mw", "wasi:http/handler", "fp-a");
        let results = validate_contract(
            &[injection("mw")],
            "wasi:http/handler",
            &Some("fp-a".to_string()),
            &mut cache,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], ContractResult::Ok);
    }

    #[test]
    fn ok_when_both_fingerprints_none() {
        // If neither side has type info, compatible_fingerprints returns true → Ok.
        let mut cache = HashMap::new();
        let mut exports = BTreeMap::new();
        exports.insert(
            "wasi:http/handler".to_string(),
            ExportInfo {
                source_instance: 0,
                fingerprint: None,
                ty: None,
            },
        );
        cache.insert("mw".to_string(), exports);

        let results = validate_contract(&[injection("mw")], "wasi:http/handler", &None, &mut cache);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], ContractResult::Ok);
    }

    // -----------------------------------------------------------------------
    // WAT-based integration: discover_exports_from_bytes
    // -----------------------------------------------------------------------

    /// Middleware using the FromExports pattern produces a non-None fingerprint.
    #[test]
    fn discover_exports_from_from_exports_mw() {
        let wat = r#"(component
            (import "wasi:http/handler@0.3.0" (instance $host
                (export "handle" (func (param "req" u32) (result u32)))
            ))
            (alias export $host "handle" (func $f))
            (instance $out (export "handle" (func $f)))
            (export "wasi:http/handler@0.3.0" (instance $out))
        )"#;
        let bytes = wat::parse_str(wat).expect("failed to parse WAT");
        let exports = discover_exports_from_bytes(&bytes);

        let export = exports
            .get("wasi:http/handler@0.3.0")
            .expect("expected export for wasi:http/handler@0.3.0");
        assert!(
            export.fingerprint.is_some(),
            "expected fingerprint for FromExports middleware"
        );
    }

    /// Middleware that directly re-exports an imported instance produces a
    /// non-None fingerprint (RC-3 coverage).
    #[test]
    fn discover_exports_from_passthrough_mw() {
        let wat = r#"(component
            (import "wasi:http/handler@0.3.0" (instance $handler
                (export "handle" (func (param "req" u32) (result u32)))
            ))
            (export "wasi:http/handler@0.3.0" (instance $handler))
        )"#;
        let bytes = wat::parse_str(wat).expect("failed to parse WAT");
        let exports = discover_exports_from_bytes(&bytes);

        let export = exports
            .get("wasi:http/handler@0.3.0")
            .expect("expected export for wasi:http/handler@0.3.0");
        assert!(
            export.fingerprint.is_some(),
            "expected fingerprint for import-reexport middleware"
        );
    }

    /// A compatible WAT middleware (same signature as chain) validates as Ok.
    #[test]
    fn ok_result_for_compatible_wat_middleware() {
        // Chain component exports "handle" (param u32) -> u32
        let chain_wat = r#"(component
            (import "wasi:http/handler@0.3.0" (instance $host
                (export "handle" (func (param "req" u32) (result u32)))
            ))
            (alias export $host "handle" (func $f))
            (instance $out (export "handle" (func $f)))
            (export "wasi:http/handler@0.3.0" (instance $out))
        )"#;
        // Middleware with the same signature
        let mw_wat = r#"(component
            (import "wasi:http/handler@0.3.0" (instance $handler
                (export "handle" (func (param "req" u32) (result u32)))
            ))
            (export "wasi:http/handler@0.3.0" (instance $handler))
        )"#;

        let chain_bytes = wat::parse_str(chain_wat).expect("failed to parse chain WAT");
        let mw_bytes = wat::parse_str(mw_wat).expect("failed to parse middleware WAT");

        let chain_exports = discover_exports_from_bytes(&chain_bytes);
        let chain_fp = chain_exports
            .get("wasi:http/handler@0.3.0")
            .and_then(|e| e.fingerprint.clone());

        // Pre-populate cache with the middleware's discovered exports
        let mw_exports = discover_exports_from_bytes(&mw_bytes);
        let mut cache = HashMap::new();
        cache.insert("mw".to_string(), mw_exports);

        let inj = Injection {
            name: "mw".to_string(),
            path: None,
        };
        let results = validate_contract(&[inj], "wasi:http/handler@0.3.0", &chain_fp, &mut cache);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], ContractResult::Ok);
    }

    /// An incompatible WAT middleware (different signature) validates as Error.
    #[test]
    fn error_result_for_incompatible_wat_middleware() {
        // Chain: (param u32) -> u32
        let chain_wat = r#"(component
            (import "wasi:http/handler@0.3.0" (instance $host
                (export "handle" (func (param "req" u32) (result u32)))
            ))
            (alias export $host "handle" (func $f))
            (instance $out (export "handle" (func $f)))
            (export "wasi:http/handler@0.3.0" (instance $out))
        )"#;
        // Incompatible middleware: different param type
        let mw_wat = r#"(component
            (import "wasi:http/handler@0.3.0" (instance $handler
                (export "handle" (func (param "req" string) (result u32)))
            ))
            (export "wasi:http/handler@0.3.0" (instance $handler))
        )"#;

        let chain_bytes = wat::parse_str(chain_wat).expect("failed to parse chain WAT");
        let mw_bytes = wat::parse_str(mw_wat).expect("failed to parse middleware WAT");

        let chain_exports = discover_exports_from_bytes(&chain_bytes);
        let chain_fp = chain_exports
            .get("wasi:http/handler@0.3.0")
            .and_then(|e| e.fingerprint.clone());

        let mw_exports = discover_exports_from_bytes(&mw_bytes);
        let mut cache = HashMap::new();
        cache.insert("mw".to_string(), mw_exports);

        let inj = Injection {
            name: "mw".to_string(),
            path: None,
        };
        let results = validate_contract(&[inj], "wasi:http/handler@0.3.0", &chain_fp, &mut cache);
        assert_eq!(results.len(), 1);
        assert!(
            matches!(results[0], ContractResult::Error(_)),
            "expected Error for incompatible middleware"
        );
    }

    #[test]
    fn mixed_results_for_multiple_injections() {
        // mw-ok: matching fingerprint → Ok
        // mw-bad: mismatched fingerprint → Error
        // mw-unknown: not in cache, no path → Warn
        let injections = vec![
            injection("mw-ok"),
            injection("mw-bad"),
            injection("mw-unknown"),
        ];
        let mut cache = HashMap::new();
        cache.insert("mw-ok".to_string(), {
            let mut m = BTreeMap::new();
            m.insert(
                "wasi:http/handler".to_string(),
                export_with_fingerprint("fp-a"),
            );
            m
        });
        cache.insert("mw-bad".to_string(), {
            let mut m = BTreeMap::new();
            m.insert(
                "wasi:http/handler".to_string(),
                export_with_fingerprint("fp-b"),
            );
            m
        });
        // mw-unknown: not inserted → discovery runs, returns empty (path: None)

        let results = validate_contract(
            &injections,
            "wasi:http/handler",
            &Some("fp-a".to_string()),
            &mut cache,
        );
        assert_eq!(results.len(), 3);
        assert_eq!(results[0], ContractResult::Ok);
        assert!(matches!(results[1], ContractResult::Error(_)));
        assert!(matches!(results[2], ContractResult::Warn(_)));
    }
}
