use crate::parse::config::Injection;
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
        let exports = checked_middlewares
            .entry(name.to_string())
            .or_insert_with(|| discover_middleware_exports(path));

        if let Some(ExportInfo { fingerprint, .. }) = exports.get(interface_name) {
            if !compatible_fingerprints(contract_fingerprint, fingerprint) {
                results.push(ContractResult::Error(format!(
                    "incompatible type signatures for middleware '{}' on interface '{}'",
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

fn discover_middleware_exports(wasm_path: &Option<String>) -> BTreeMap<String, ExportInfo> {
    if let Some(path) = wasm_path {
        let buff = fs::read(path).unwrap(); // todo: make this more elegant (handle the error)!
        let graph = parse_component(&buff).expect("Unable to discover composition"); // todo: make this more elegant (handle the error!)

        graph.component_exports
    } else {
        BTreeMap::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cviz::model::ExportInfo;

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
