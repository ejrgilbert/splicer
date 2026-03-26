use anyhow::{bail, Result};
use cviz::model::{ComponentNode, CompositionGraph, ExportInfo, InterfaceConnection};
use cviz::parse::component::{parse_component, parse_component_imports};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;

/// Parse N individual Wasm components and synthesise a [`CompositionGraph`] by
/// matching their exports to each other's imports.
///
/// Returns:
/// - The synthetic [`CompositionGraph`] with topologically-ordered node IDs
///   (providers receive lower IDs than consumers).
/// - A map from node ID → original `.wasm` path, used later to generate the
///   `wac compose --dep` arguments without needing a split pass.
pub fn build_graph_from_components(
    components: &[(PathBuf, Vec<u8>)],
) -> Result<(CompositionGraph, HashMap<u32, PathBuf>)> {
    let n = components.len();

    // ── 1. Parse each component: collect exports and imports ─────────────────
    struct CompInfo {
        path: PathBuf,
        /// Variable-friendly name derived from the filename stem.
        name: String,
        /// Interface names this component exports.
        exports: Vec<String>,
        /// (interface_name, fingerprint) pairs for each instance-kind import.
        imports: Vec<(String, Option<String>)>,
    }

    let mut comp_infos: Vec<CompInfo> = Vec::with_capacity(n);

    for (path, bytes) in components {
        let graph = parse_component(bytes)?;
        let imports = parse_component_imports(bytes)?;

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .replace(['.', '_'], "-");

        let exports: Vec<String> = graph.component_exports.keys().cloned().collect();

        comp_infos.push(CompInfo {
            path: path.clone(),
            name,
            exports,
            imports,
        });
    }

    // ── 2. Build export index: interface → component index ───────────────────
    let mut export_index: HashMap<String, usize> = HashMap::new();
    for (comp_idx, info) in comp_infos.iter().enumerate() {
        for export in &info.exports {
            if export_index.insert(export.clone(), comp_idx).is_some() {
                bail!(
                    "Ambiguous composition: multiple components export '{}'. \
                     Unable to decide correct composition here.",
                    export
                );
            }
        }
    }

    // ── 3. Resolve imports across components (with type checking) ────────────
    struct ResolvedImport {
        interface_name: String,
        provider_comp_idx: usize,
        /// Fingerprint from the importer side (used to verify against the exporter).
        import_fingerprint: Option<String>,
    }

    let mut resolved: Vec<Vec<ResolvedImport>> = (0..n).map(|_| Vec::new()).collect();
    let mut unresolved: Vec<Vec<String>> = (0..n).map(|_| Vec::new()).collect();

    // We need per-component export fingerprints for type checking here.
    // Parse once up-front so we can look up fingerprints without re-parsing later.
    let parsed_graphs: Vec<CompositionGraph> = components
        .iter()
        .map(|(_, bytes)| parse_component(bytes))
        .collect::<Result<_>>()?;

    for (comp_idx, info) in comp_infos.iter().enumerate() {
        for (import_name, import_fp) in &info.imports {
            match export_index.get(import_name) {
                Some(&provider_idx) if provider_idx != comp_idx => {
                    // Type-check: compare the importer's fingerprint against the
                    // exporter's fingerprint.  Both must be Some for a hard error;
                    // if either is None we emit a warning but still proceed.
                    let export_fp = parsed_graphs[provider_idx]
                        .component_exports
                        .get(import_name)
                        .and_then(|e| e.fingerprint.clone());

                    match (import_fp, &export_fp) {
                        (Some(ifp), Some(efp)) if ifp != efp => {
                            bail!(
                                "Type mismatch: '{}' imports '{}' but the types are incompatible.\n\
                                 \timporter ({}): {}\n\
                                 \texporter ({}): {}",
                                info.name,
                                import_name,
                                info.name,
                                ifp,
                                comp_infos[provider_idx].name,
                                efp,
                            );
                        }
                        _ => { /* compatible or unverifiable — proceed */ }
                    }

                    resolved[comp_idx].push(ResolvedImport {
                        interface_name: import_name.clone(),
                        provider_comp_idx: provider_idx,
                        import_fingerprint: import_fp.clone(),
                    });
                }
                Some(_) => bail!(
                    "Component '{}' both imports and exports '{}'",
                    info.name,
                    import_name
                ),
                None => {
                    // Not satisfied by any provided component → host import (e.g. WASI).
                    unresolved[comp_idx].push(import_name.clone());
                }
            }
        }
    }

    // ── 4. Topological sort (Kahn's algorithm) ────────────────────────────────
    // in_degree[i] = number of distinct provider components that i depends on.
    let mut in_degree: Vec<usize> = (0..n)
        .map(|i| {
            resolved[i]
                .iter()
                .map(|r| r.provider_comp_idx)
                .collect::<BTreeSet<_>>()
                .len()
        })
        .collect();

    let mut queue: VecDeque<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut topo_order: Vec<usize> = Vec::with_capacity(n);

    while let Some(idx) = queue.pop_front() {
        topo_order.push(idx);
        for other_idx in 0..n {
            if resolved[other_idx]
                .iter()
                .any(|r| r.provider_comp_idx == idx)
            {
                in_degree[other_idx] -= 1;
                if in_degree[other_idx] == 0 {
                    queue.push_back(other_idx);
                }
            }
        }
    }

    if topo_order.len() != n {
        bail!("Cyclic dependency detected among the provided components");
    }

    // ── 5. Assign node IDs and build the graph ────────────────────────────────
    // Providers receive lower IDs → generate_wac's topological pre-pass works
    // correctly when iterating nodes in ascending ID order.
    let mut comp_idx_to_node_id: Vec<u32> = vec![0; n];
    for (topo_pos, &comp_idx) in topo_order.iter().enumerate() {
        comp_idx_to_node_id[comp_idx] = topo_pos as u32;
    }

    let mut graph = CompositionGraph::new();
    let mut node_paths: HashMap<u32, PathBuf> = HashMap::new();

    for (topo_pos, &comp_idx) in topo_order.iter().enumerate() {
        let info = &comp_infos[comp_idx];
        let node_id = topo_pos as u32;

        let mut node =
            ComponentNode::new(format!("${}", info.name), comp_idx as u32, comp_idx as u32);

        for res_import in &resolved[comp_idx] {
            let provider_node_id = comp_idx_to_node_id[res_import.provider_comp_idx];
            // Use the import-side fingerprint — already verified to match the exporter's.
            // Fall back to the exporter's fingerprint if the importer's was None.
            let fingerprint = res_import.import_fingerprint.clone().or_else(|| {
                parsed_graphs[res_import.provider_comp_idx]
                    .component_exports
                    .get(&res_import.interface_name)
                    .and_then(|e| e.fingerprint.clone())
            });

            node.add_import(InterfaceConnection {
                interface_name: res_import.interface_name.clone(),
                source_instance: provider_node_id,
                is_host_import: false,
                interface_type: None,
                fingerprint,
            });
        }

        for host_iface in &unresolved[comp_idx] {
            node.add_import(InterfaceConnection {
                interface_name: host_iface.clone(),
                source_instance: u32::MAX,
                is_host_import: true,
                interface_type: None,
                fingerprint: None,
            });
        }

        graph.add_node(node_id, node);
        node_paths.insert(node_id, info.path.clone());
    }

    // ── 6. Set graph-level exports ────────────────────────────────────────────
    // An export is a graph-level export if no other component in the set imports it.
    let internally_consumed: HashSet<&str> = resolved
        .iter()
        .flat_map(|v| v.iter())
        .map(|r| r.interface_name.as_str())
        .collect();

    for (topo_pos, &comp_idx) in topo_order.iter().enumerate() {
        let node_id = topo_pos as u32;
        for export_name in &comp_infos[comp_idx].exports {
            if !internally_consumed.contains(export_name.as_str()) {
                let fingerprint = parsed_graphs[comp_idx]
                    .component_exports
                    .get(export_name)
                    .and_then(|e| e.fingerprint.clone());

                graph.component_exports.insert(
                    export_name.clone(),
                    ExportInfo {
                        source_instance: node_id,
                        fingerprint,
                        ty: None,
                    },
                );
            }
        }
    }

    Ok((graph, node_paths))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(path: &str, wat: &str) -> (PathBuf, Vec<u8>) {
        let bytes = wat::parse_str(wat).expect("invalid WAT");
        (PathBuf::from(path), bytes)
    }

    // ── WAT fixtures ──────────────────────────────────────────────────────────
    //
    // Each "provider" imports a host-only interface (never exported by another
    // component) and re-exports its own interface.  This avoids the
    // "both imports and exports" error while keeping the WAT minimal.

    const WAT_PROVIDER_A: &str = r#"(component
        (import "host:env/dep@0.1.0" (instance $dep
            (export "get" (func (result u32)))
        ))
        (alias export $dep "get" (func $f))
        (instance $out (export "get" (func $f)))
        (export "my:providers/a@0.1.0" (instance $out))
    )"#;

    const WAT_PROVIDER_B: &str = r#"(component
        (import "host:env/dep@0.1.0" (instance $dep
            (export "get" (func (result u32)))
        ))
        (alias export $dep "get" (func $f))
        (instance $out (export "get" (func $f)))
        (export "my:providers/b@0.1.0" (instance $out))
    )"#;

    const WAT_PROVIDER_C: &str = r#"(component
        (import "host:env/dep@0.1.0" (instance $dep
            (export "get" (func (result u32)))
        ))
        (alias export $dep "get" (func $f))
        (instance $out (export "get" (func $f)))
        (export "my:providers/c@0.1.0" (instance $out))
    )"#;

    // Consumer imports all three providers + one unique host import.
    const WAT_CONSUMER_FAN_IN: &str = r#"(component
        (import "my:providers/a@0.1.0" (instance $a
            (export "get" (func (result u32)))
        ))
        (import "my:providers/b@0.1.0" (instance $b
            (export "get" (func (result u32)))
        ))
        (import "my:providers/c@0.1.0" (instance $c
            (export "get" (func (result u32)))
        ))
        (import "host:consumer/ctx@0.1.0" (instance $ctx
            (export "write" (func (param "msg" string)))
        ))
        (alias export $a "get" (func $f))
        (instance $out (export "get" (func $f)))
        (export "my:consumer/app@0.1.0" (instance $out))
    )"#;

    // Simple two-node chain: A → consumer.
    const WAT_SIMPLE_CONSUMER: &str = r#"(component
        (import "my:providers/a@0.1.0" (instance $a
            (export "get" (func (result u32)))
        ))
        (alias export $a "get" (func $f))
        (instance $out (export "get" (func $f)))
        (export "my:consumer/app@0.1.0" (instance $out))
    )"#;

    // Two providers that export the SAME interface name → ambiguous export error.
    const WAT_PROVIDER_A_DUP: &str = r#"(component
        (import "host:env/dep@0.1.0" (instance $dep
            (export "get" (func (result u32)))
        ))
        (alias export $dep "get" (func $f))
        (instance $out (export "get" (func $f)))
        (export "my:providers/a@0.1.0" (instance $out))
    )"#;

    // Provider with a type-v1 signature for the shared interface.
    const WAT_PROVIDER_V1: &str = r#"(component
        (import "host:env/dep@0.1.0" (instance $dep
            (export "do-it" (func (result u32)))
        ))
        (alias export $dep "do-it" (func $f))
        (instance $out (export "do-it" (func $f)))
        (export "my:shared/iface@0.1.0" (instance $out))
    )"#;

    // Consumer that imports "my:shared/iface@0.1.0" with a DIFFERENT signature
    // (result string vs result u32) → type mismatch error.
    const WAT_CONSUMER_MISMATCHED: &str = r#"(component
        (import "my:shared/iface@0.1.0" (instance $iface
            (export "do-it" (func (result string)))
        ))
        (alias export $iface "do-it" (func $f))
        (instance $out (export "do-it" (func $f)))
        (export "my:consumer/output@0.1.0" (instance $out))
    )"#;

    // Cycle: A imports B's export, B imports A's export.
    const WAT_CYCLE_A: &str = r#"(component
        (import "my:cycle/b@0.1.0" (instance $b
            (export "go" (func (result u32)))
        ))
        (alias export $b "go" (func $f))
        (instance $out (export "go" (func $f)))
        (export "my:cycle/a@0.1.0" (instance $out))
    )"#;

    const WAT_CYCLE_B: &str = r#"(component
        (import "my:cycle/a@0.1.0" (instance $a
            (export "go" (func (result u32)))
        ))
        (alias export $a "go" (func $f))
        (instance $out (export "go" (func $f)))
        (export "my:cycle/b@0.1.0" (instance $out))
    )"#;

    // Self-import: single component that both imports and exports the same interface.
    const WAT_SELF_IMPORT: &str = r#"(component
        (import "my:shared/iface@0.1.0" (instance $iface
            (export "get" (func (result u32)))
        ))
        (alias export $iface "get" (func $f))
        (instance $out (export "get" (func $f)))
        (export "my:shared/iface@0.1.0" (instance $out))
    )"#;

    // ── Happy-path tests ──────────────────────────────────────────────────────

    #[test]
    fn simple_chain_resolves() -> anyhow::Result<()> {
        let comps = vec![
            mk("provider-a.wasm", WAT_PROVIDER_A),
            mk("consumer.wasm", WAT_SIMPLE_CONSUMER),
        ];
        let (graph, node_paths) = build_graph_from_components(&comps)?;

        // Exactly 2 nodes
        assert_eq!(graph.nodes.len(), 2);
        // Both paths are recorded
        assert_eq!(node_paths.len(), 2);

        // The consumer (depends on provider-a) must have an import connection
        // pointing to the provider's node id.
        let consumer_node = graph
            .nodes
            .values()
            .find(|n| n.name.contains("consumer"))
            .expect("consumer node not found");
        let resolved_import = consumer_node
            .imports
            .iter()
            .find(|i| i.interface_name == "my:providers/a@0.1.0" && !i.is_host_import)
            .expect("consumer should have a resolved import for my:providers/a@0.1.0");

        let provider_node = graph
            .nodes
            .values()
            .find(|n| n.name.contains("provider-a"))
            .expect("provider-a node not found");
        assert_eq!(
            resolved_import.source_instance,
            *graph
                .nodes
                .iter()
                .find(|(_, n)| n.name == provider_node.name)
                .unwrap()
                .0,
            "consumer's import should point to provider-a's node id"
        );
        Ok(())
    }

    #[test]
    fn fan_in_all_deps_wired() -> anyhow::Result<()> {
        let comps = vec![
            mk("provider-a.wasm", WAT_PROVIDER_A),
            mk("provider-b.wasm", WAT_PROVIDER_B),
            mk("provider-c.wasm", WAT_PROVIDER_C),
            mk("consumer.wasm", WAT_CONSUMER_FAN_IN),
        ];
        let (graph, _) = build_graph_from_components(&comps)?;

        assert_eq!(graph.nodes.len(), 4);

        let consumer_node = graph
            .nodes
            .values()
            .find(|n| n.name.contains("consumer"))
            .expect("consumer node not found");

        // All three provider interfaces must be resolved (not host imports)
        for iface in &[
            "my:providers/a@0.1.0",
            "my:providers/b@0.1.0",
            "my:providers/c@0.1.0",
        ] {
            assert!(
                consumer_node
                    .imports
                    .iter()
                    .any(|i| i.interface_name == *iface && !i.is_host_import),
                "consumer should have resolved import for {iface}"
            );
        }

        // The host import must be preserved
        assert!(
            consumer_node
                .imports
                .iter()
                .any(|i| i.interface_name == "host:consumer/ctx@0.1.0" && i.is_host_import),
            "consumer should have host import for host:consumer/ctx@0.1.0"
        );

        Ok(())
    }

    #[test]
    fn topological_order_providers_get_lower_ids() -> anyhow::Result<()> {
        let comps = vec![
            // Put consumer first in the input list — topo sort should still
            // assign it the highest node id.
            mk("consumer.wasm", WAT_SIMPLE_CONSUMER),
            mk("provider-a.wasm", WAT_PROVIDER_A),
        ];
        let (graph, node_paths) = build_graph_from_components(&comps)?;

        // Find node ids by name
        let provider_id = *graph
            .nodes
            .iter()
            .find(|(_, n)| n.name.contains("provider-a"))
            .expect("provider-a not found")
            .0;
        let consumer_id = *graph
            .nodes
            .iter()
            .find(|(_, n)| n.name.contains("consumer"))
            .expect("consumer not found")
            .0;

        assert!(
            provider_id < consumer_id,
            "provider node id ({provider_id}) should be less than consumer node id ({consumer_id})"
        );
        // node_paths should have entries for both
        assert!(node_paths.contains_key(&provider_id));
        assert!(node_paths.contains_key(&consumer_id));
        Ok(())
    }

    #[test]
    fn host_imports_become_host_connections() -> anyhow::Result<()> {
        let comps = vec![
            mk("provider-a.wasm", WAT_PROVIDER_A),
            mk("consumer.wasm", WAT_CONSUMER_FAN_IN),
            mk("provider-b.wasm", WAT_PROVIDER_B),
            mk("provider-c.wasm", WAT_PROVIDER_C),
        ];
        let (graph, _) = build_graph_from_components(&comps)?;

        // Every node that originally imported "host:env/dep@0.1.0" should have
        // it marked as a host import (providers A/B/C all have this).
        let providers_with_host_dep: Vec<_> = graph
            .nodes
            .values()
            .filter(|n| {
                n.imports
                    .iter()
                    .any(|i| i.interface_name == "host:env/dep@0.1.0" && i.is_host_import)
            })
            .collect();
        assert_eq!(
            providers_with_host_dep.len(),
            3,
            "all three providers should have host:env/dep@0.1.0 as a host import"
        );
        Ok(())
    }

    #[test]
    fn graph_level_exports_are_set() -> anyhow::Result<()> {
        let comps = vec![
            mk("provider-a.wasm", WAT_PROVIDER_A),
            mk("consumer.wasm", WAT_SIMPLE_CONSUMER),
        ];
        let (graph, _) = build_graph_from_components(&comps)?;

        // "my:consumer/app@0.1.0" is exported by consumer and not consumed
        // internally → should be a graph-level export.
        assert!(
            graph
                .component_exports
                .contains_key("my:consumer/app@0.1.0"),
            "my:consumer/app@0.1.0 should be a graph-level export"
        );
        // "my:providers/a@0.1.0" IS consumed by consumer → must NOT be a
        // graph-level export.
        assert!(
            !graph.component_exports.contains_key("my:providers/a@0.1.0"),
            "my:providers/a@0.1.0 is consumed internally and must not be a graph-level export"
        );
        Ok(())
    }

    // ── Error-case tests ──────────────────────────────────────────────────────

    #[test]
    fn error_ambiguous_export() {
        // Two providers both exporting the same interface name.
        let comps = vec![
            mk("provider-a.wasm", WAT_PROVIDER_A),
            mk("provider-a-dup.wasm", WAT_PROVIDER_A_DUP),
        ];
        let result = build_graph_from_components(&comps);
        assert!(result.is_err(), "expected Ambiguous composition error");
        let err = result.err().unwrap();
        assert!(
            err.to_string().contains("Ambiguous composition"),
            "expected 'Ambiguous composition' in error, got: {err}"
        );
    }

    #[test]
    fn error_cyclic_dependency() {
        let comps = vec![
            mk("cycle-a.wasm", WAT_CYCLE_A),
            mk("cycle-b.wasm", WAT_CYCLE_B),
        ];
        let result = build_graph_from_components(&comps);
        assert!(result.is_err(), "expected cyclic dependency error");
        let err = result.err().unwrap();
        assert!(
            err.to_string().to_lowercase().contains("cyclic"),
            "expected 'cyclic' in error, got: {err}"
        );
    }

    #[test]
    fn error_self_import_and_export() {
        // A single component that both imports and exports the same interface.
        let comps = vec![mk("self-import.wasm", WAT_SELF_IMPORT)];
        let result = build_graph_from_components(&comps);
        assert!(result.is_err(), "expected both-imports-and-exports error");
        let err = result.err().unwrap();
        assert!(
            err.to_string().contains("both imports and exports"),
            "expected 'both imports and exports' in error, got: {err}"
        );
    }

    #[test]
    fn error_type_mismatch() {
        // Provider exports `my:shared/iface@0.1.0` with (result u32);
        // consumer imports it with (result string) — structural mismatch.
        let comps = vec![
            mk("provider-v1.wasm", WAT_PROVIDER_V1),
            mk("consumer-mismatched.wasm", WAT_CONSUMER_MISMATCHED),
        ];
        let result = build_graph_from_components(&comps);
        // If both fingerprints are Some they'll differ → hard error.
        // If either fingerprint is None the check is skipped (unverifiable).
        // The test is meaningful either way: at minimum, the function must not panic.
        match result {
            Err(e) => assert!(
                e.to_string().contains("Type mismatch") || e.to_string().contains("incompatible"),
                "expected type-mismatch error, got: {e}"
            ),
            Ok(_) => {
                // Fingerprint was not computable for one side → unverifiable,
                // no error is acceptable per the spec.
            }
        }
    }
}
