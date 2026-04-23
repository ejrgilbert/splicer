use crate::adapter::generate_tier1_adapter;
use crate::contract::{validate_contract, ContractResult};
use colored::Colorize;
use cviz::model::{
    ComponentNode, CompositionGraph, ExportInfo, InterfaceConnection, InterfaceType, InternedId,
    TypeArena,
};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use wasmparser::collections::IndexSet;

/// Package prefix used for WAC instance variables (e.g. `"my:srv-a"`).
pub const INST_PREFIX: &str = "my";
const PATH_PLACEHOLDER: &str = "/path/to/comp.wasm";
use crate::parse::config::{AdapterInjectionInfo, Injection, SpliceRule};
use crate::split::gen_split_path;

// chain_idx -> set of middlewares to inject AFTER
type InjectPlan = HashMap<usize, IndexSet<Injection>>;

struct Chain {
    interface: Contract,
    chain: Vec<u32>,
    aliases: HashMap<u32, Option<String>>,
    // middlewares to inject after the specified index in the chain
    inject_plan: InjectPlan,
}

impl Chain {
    /// Returns the split path of the component that consumes the handler at
    /// the given chain position.
    ///
    /// The consumer is the component that IMPORTS the handler interface —
    /// the adapter copies its import structure to get the right types.
    /// At `chain_idx`, the consumer is `chain[chain_idx]`.
    fn consumer_split_path(
        &self,
        chain_idx: usize,
        composition: &CompositionGraph,
        splits_path: &str,
        shim_comps: &HashMap<usize, usize>,
    ) -> Option<String> {
        let consumer_id = *self.chain.get(chain_idx)?;
        let component_num = composition.nodes.get(&consumer_id)?.component_num + 1;
        let split_to_use = resolve_shim(component_num as usize, shim_comps);
        Some(gen_split_path(splits_path, split_to_use))
    }
}

#[derive(Clone, Debug)]
struct Contract {
    name: String,
    ty_fingerprint: Option<String>,
    interface_type: Option<InterfaceType>,
}

/// One entry in [`WacOutput::generated_adapters`] — a tier-1 adapter
/// component that splicer wrote to disk while resolving an injection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedAdapter {
    /// Path on disk to the generated adapter `.wasm` file.
    pub adapter_path: String,
    /// Name of the middleware injection the adapter wraps.
    pub middleware_name: String,
    /// Target interface the adapter exports (e.g.
    /// `"wasi:http/handler@0.3.0-rc-2026-01-06"`).
    pub target_interface: String,
    /// Tier-1 hook interfaces the wrapped middleware exports
    /// (e.g. `"splicer:tier1/before"`).
    pub tier1_interfaces: Vec<String>,
}

/// Output of [`generate_wac`].
pub struct WacOutput {
    /// The generated WAC source text.
    pub wac: String,
    /// Per-dependency `(package_key → wasm path)` map ready to feed to
    /// `wac-resolver::FileSystemPackageResolver::new` as the `overrides`
    /// argument, or to format into a `wac compose ... --dep <key>=<path>`
    /// shell command.
    ///
    /// Keys are fully-qualified WAC package keys (e.g. `"my:srv-a"`) —
    /// the same form that appears in the generated WAC source on the
    /// right-hand side of `new`. Stored in a `BTreeMap` so the order
    /// is deterministic across runs.
    pub wac_deps: BTreeMap<String, PathBuf>,
    /// Diagnostics from contract validation, one per middleware injection attempted.
    pub diagnostics: Vec<ContractResult>,
    /// Tier-1 adapter components that were generated and written to
    /// disk while resolving the splice rules. Empty when no rule
    /// matched a tier-1 type-erased middleware.
    pub generated_adapters: Vec<GeneratedAdapter>,
}

/// Generate WAC from a composition graph and a set of splicing rules.
///
/// `node_paths` is `Some` for the multi-component path; when present each node's
/// original `.wasm` path is used directly instead of deriving a split path.
pub fn generate_wac(
    shim_comps: HashMap<usize, usize>,
    splits_path: &str,
    composition: &CompositionGraph,
    rules: &[SpliceRule],
    node_paths: Option<&HashMap<u32, PathBuf>>,
    pkg_name: &str,
) -> anyhow::Result<WacOutput> {
    // Emit the "shim split defaulting" WARN(s) exactly once up-front.
    // Without this, the notice would fire twice — once from the adapter
    // generator's `consumer_split_path` lookup, once from the wac-dep
    // map — because both paths call `resolve_shim` for the same shim.
    warn_about_shim_resolutions(&shim_comps);

    let mut wac_lines = vec![format!("package {pkg_name};")];

    let mut handled_interfaces = HashSet::new();

    let mut chains = vec![];
    let mut ordered_node_ids = composition.nodes.keys().collect::<Vec<_>>();
    ordered_node_ids.sort_by_key(|id| Reverse(**id));
    for outer_node_id in ordered_node_ids {
        let node = &composition.nodes[outer_node_id];

        // construct all the chains in the component
        // must do so by starting at largest instance IDs to smallest to get the largest chain!
        for InterfaceConnection {
            interface_name,
            source_instance,
            is_host_import,
            fingerprint,
            interface_type: iface_ty,
        } in node.imports.iter()
        {
            let mut chain = vec![*outer_node_id];
            if *is_host_import {
                continue;
            }
            let mut current_id = source_instance.unwrap();

            chain.push(source_instance.unwrap());
            while let Some(node) = composition.nodes.get(&current_id) {
                if let Some(conn) = node
                    .imports
                    .iter()
                    .find(|c| c.interface_name == *interface_name)
                {
                    if !conn.is_host_import {
                        let src_id = conn.source_instance.unwrap();
                        chain.push(src_id);
                        current_id = src_id;
                        continue;
                    }
                }
                break;
            }

            if !handled_interfaces.contains(interface_name) && chain.len() > 1 {
                chain.reverse();
                chains.push(Chain {
                    interface: Contract {
                        name: interface_name.to_string(),
                        ty_fingerprint: fingerprint.clone(),
                        interface_type: iface_ty.clone(),
                    },
                    chain,
                    aliases: HashMap::new(),
                    inject_plan: HashMap::new(),
                });
            }
            handled_interfaces.insert(interface_name.to_string());
        }
    }

    // handle standalone exported interfaces!
    for (
        interface,
        ExportInfo {
            source_instance: source_inst,
            fingerprint,
            ty: export_ty,
        },
    ) in composition.component_exports.iter()
    {
        if handled_interfaces.contains(interface) {
            continue;
        }
        // Resolve the interface type from the export info if available.
        let interface_type = export_ty.and_then(|id| match id {
            InternedId::Interface(iface_id) => {
                Some(composition.arena.lookup_interface(iface_id).clone())
            }
            _ => None,
        });
        // if we've reached this point, it's guaranteed to not be a chain (chains were handled above)
        // this is just a single exported service func.
        chains.push(Chain {
            interface: Contract {
                name: interface.to_string(),
                ty_fingerprint: fingerprint.clone(),
                interface_type,
            },
            chain: vec![*source_inst],
            aliases: HashMap::new(),
            inject_plan: HashMap::new(),
        });
    }

    // This is to allow for caching the export contract discover of middleware components.
    let mut checked_middlewares = HashMap::new();

    // Apply the rules in order of their declaration in the configuration.
    // This enforces an ordering semantic for the rule application.
    let mut diagnostics: Vec<ContractResult> = vec![];
    let mut generated_adapters: Vec<GeneratedAdapter> = vec![];
    for (rule_idx, rule) in rules.iter().enumerate() {
        let mut any_interface_matched = false;
        let mut any_full_match = false;
        for chain in chains.iter_mut() {
            let between = apply_rule_between(
                rule,
                chain,
                composition,
                splits_path,
                &shim_comps,
                &mut checked_middlewares,
                &mut generated_adapters,
            )?;
            let before = apply_rule_before(
                rule,
                chain,
                composition,
                splits_path,
                &shim_comps,
                &mut checked_middlewares,
                &mut generated_adapters,
            )?;
            any_interface_matched |= between.interface_matched | before.interface_matched;
            any_full_match |= between.full_match | before.full_match;
            diagnostics.extend(between.contract_results);
            diagnostics.extend(before.contract_results);
        }
        if !any_full_match {
            let iface = rule_interface(rule);
            if !any_interface_matched {
                // Interface name itself wasn't found — suggest close matches.
                let available: Vec<&str> =
                    chains.iter().map(|c| c.interface.name.as_str()).collect();
                let iface_base = iface.split('@').next().unwrap_or(iface);
                let possibly_intended: Vec<&str> = available
                    .iter()
                    .copied()
                    .filter(|&avail| {
                        let avail_base = avail.split('@').next().unwrap_or(avail);
                        avail_base == iface_base
                            || avail.starts_with(iface)
                            || iface.starts_with(avail)
                    })
                    .collect();
                let intended_msg = if possibly_intended.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n\t  Possibly intended:    [{}]",
                        possibly_intended.join(", ")
                    )
                };
                eprintln!(
                    "{}: rule {} — interface '{}' was not found in the composition.\n\
                     \t  Available interfaces: [{}]{}",
                    "WARN".yellow().bold(),
                    rule_idx + 1,
                    iface,
                    available.join(", "),
                    intended_msg
                );
            } else {
                // Interface matched but node names didn't — show available node names
                // for chains on that interface so the user can fix their config.
                let node_names: Vec<String> = chains
                    .iter()
                    .filter(|c| c.interface.name == iface)
                    .flat_map(|c| {
                        c.chain
                            .iter()
                            .map(|id| get_name(&composition.nodes[id]).to_string())
                    })
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                eprintln!(
                    "{}: rule {} — interface '{}' matched but no node names matched.\n\
                     \t  Nodes on that interface: [{}]\n\
                     \t  Check the 'name' fields in your config against these exactly.",
                    "WARN".yellow().bold(),
                    rule_idx + 1,
                    iface,
                    node_names.join(", ")
                );
            }
        }
    }

    // Let's now generate WAC to handle the chains we've planned to emit
    let mut mdl_override = None;
    let mut last = String::new();
    let mut instance_vars: HashMap<u32, String> = HashMap::new();
    // orig_inst_id -> generated_outer_var
    let mut outer_instances: HashMap<u32, String> = HashMap::new();
    // inst_id -> used_name
    let mut used_comp_nodes: HashMap<u32, String> = HashMap::new();
    // (used_name, path)
    let mut used_middlewares: Vec<(String, String)> = Vec::new();
    // Real-middleware wac vars already emitted via `let mdl = new
    // my:mdl { ... };`. Multiple rules can inject the same
    // middleware (different target interfaces share the same wrapped
    // hooks), so we emit the `let` once and reuse the var.
    let mut emitted_mdl_vars: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Pre-instantiation pass for fan-in topologies.
    //
    // A node that only ever appears at position 0 (innermost) across all chains is a
    // pure provider — it doesn't consume any chained interface itself and is never the
    // target of middleware injection.  We instantiate these eagerly in ascending node-ID
    // order (which is topological order for synthetically-built graphs) so that when a
    // fan-in consumer node is first encountered, ALL of its provider deps are already in
    // `instance_vars` and can be wired up correctly in a single `let` statement.
    //
    // Nodes that appear at any position > 0 in MORE THAN ONE chain are "fan-in
    // consumers".  Their instantiation is deferred until after the chain pass so that
    // every per-interface middleware is created first.  Without deferral the consumer
    // would be instantiated in the first chain it appears in, hardwiring the raw
    // provider before later chains have a chance to inject middleware.
    let fan_in_consumers: HashSet<u32>;
    {
        let mut node_positions: HashMap<u32, BTreeSet<usize>> = HashMap::new();
        for chain in &chains {
            for (pos, &id) in chain.chain.iter().enumerate() {
                node_positions.entry(id).or_default().insert(pos);
            }
        }

        // Count how many chains each node appears in at a non-zero position.
        let mut non_zero_chain_count: HashMap<u32, usize> = HashMap::new();
        for chain in &chains {
            for (pos, &id) in chain.chain.iter().enumerate() {
                if pos > 0 {
                    *non_zero_chain_count.entry(id).or_default() += 1;
                }
            }
        }
        fan_in_consumers = non_zero_chain_count
            .into_iter()
            .filter(|(_, n)| *n > 1)
            .map(|(id, _)| id)
            .collect();

        let mut pure_providers: Vec<u32> = node_positions
            .iter()
            .filter(|(_, positions)| positions.iter().all(|&p| p == 0))
            .map(|(&id, _)| id)
            .collect();
        pure_providers.sort(); // ascending = topological order for synthetic graphs

        // Collect aliases assigned to pure-provider nodes by any rule so that nodes
        // pre-instantiated here use the same name that the chain pass would assign.
        let mut pre_pass_aliases: HashMap<u32, Option<String>> = HashMap::new();
        for chain in &chains {
            for (&id, alias) in &chain.aliases {
                pre_pass_aliases.insert(id, alias.clone());
            }
        }

        for node_id in pure_providers {
            let node = &composition.nodes[&node_id];
            get_or_create_inst(
                node_id,
                &pre_pass_aliases,
                &mut used_comp_nodes,
                node,
                &mut instance_vars,
                &None,
                &mut wac_lines,
            );
        }
    }

    // Per fan-in consumer: the final provider var for each of its imported interfaces
    // after middleware has been applied.  Populated during the chain pass below.
    let mut fan_in_iface_vars: HashMap<u32, HashMap<String, String>> = HashMap::new();
    // Aliases for fan-in consumers (first chain that sets them wins).
    let mut fan_in_aliases: HashMap<u32, HashMap<u32, Option<String>>> = HashMap::new();

    for Chain {
        interface: chain_interface,
        chain,
        aliases,
        inject_plan,
    } in chains.iter()
    {
        for (i, id) in chain.iter().enumerate() {
            let is_fan_in_last = fan_in_consumers.contains(id) && i == chain.len() - 1;

            if !is_fan_in_last {
                let node = &composition.nodes[id];
                let node_var = get_or_create_inst(
                    *id,
                    aliases,
                    &mut used_comp_nodes,
                    node,
                    &mut instance_vars,
                    &mdl_override,
                    &mut wac_lines,
                );
                // set up what to wire in next
                last = node_var;
                mdl_override = Some((chain_interface.clone(), last.clone()));
            }

            if let Some(middlewares) = inject_plan.get(&(i + 1)) {
                // if the NEXT node has a middleware BEFORE it, inject here!
                // Reverse the list of items to inject (this keeps me from having to deal with this in the `wac` generation logic).
                // Through doing this, the order of middlewares invoked will follow the order of declaration in the configuration.
                let reversed_list = reverse_set(middlewares);
                for mdl in reversed_list.iter() {
                    if let Some(adapter_info) = &mdl.adapter_info {
                        // instantiate the middleware+adapter in wac script
                        let (adapter_var, extra_args) = create_tier1_mdl(
                            &last,
                            mdl,
                            chain_interface,
                            adapter_info,
                            &mut wac_lines,
                            &mut emitted_mdl_vars,
                        );
                        last = adapter_var;
                        used_middlewares.extend(extra_args);
                    } else {
                        // instantiate the middleware in wac script
                        last = create_mdl(&last, &mdl.name, chain_interface, &mut wac_lines);
                        used_middlewares.push((
                            last.clone(),
                            mdl.path
                                .as_ref()
                                .cloned()
                                .unwrap_or(PATH_PLACEHOLDER.to_string()),
                        ));
                    }
                    mdl_override = Some((chain_interface.clone(), last.clone()));
                }
            }

            if is_fan_in_last {
                // Record the final provider var for this interface so we can wire it
                // when the consumer is instantiated after all chains are processed.
                fan_in_iface_vars
                    .entry(*id)
                    .or_default()
                    .insert(chain_interface.name.clone(), last.clone());
                fan_in_aliases.entry(*id).or_insert_with(|| aliases.clone());
            } else if i == chain.len() - 1 {
                // If we're at the end of the chain, remember what our outermost layer is now.
                // This makes sure we actually export middleware if it overrode the outermost service.
                outer_instances.insert(*id, last.clone());
            }
        }
    }

    // Deferred instantiation of fan-in consumers.
    //
    // Now that every per-interface middleware has been created, we can instantiate
    // each fan-in consumer once with all of its imports wired correctly.
    for (consumer_id, iface_vars) in fan_in_iface_vars.iter() {
        let consumer_node = &composition.nodes[consumer_id];
        let aliases = fan_in_aliases.get(consumer_id).unwrap();

        let alias = aliases.get(consumer_id).cloned();
        let pkg = if let Some(Some(a)) = alias {
            a
        } else {
            sanitize_wac_id(get_name(consumer_node))
        };
        used_comp_nodes.insert(*consumer_id, pkg.clone());
        let node_var = instance_vars
            .entry(*consumer_id)
            .or_insert_with(|| pkg.clone())
            .clone();

        let mut line = format!("let {node_var} = new {INST_PREFIX}:{pkg} {{");
        for conn in &consumer_node.imports {
            if !conn.is_host_import {
                let iface = &conn.interface_name;
                let src_var = if let Some(v) = iface_vars.get(iface) {
                    v.clone()
                } else if let Some(v) = conn.source_instance.and_then(|id| instance_vars.get(&id)) {
                    v.clone()
                } else {
                    continue;
                };
                line.push_str(&format!("\n    \"{iface}\": {src_var}[\"{iface}\"],"));
            }
        }
        line.push_str("\n    ...\n};");
        wac_lines.push(line);

        outer_instances.insert(*consumer_id, node_var.clone());
    }

    // Generate WAC to export the appropriate functions
    for (
        export_name,
        ExportInfo {
            source_instance: outer_inst_id,
            ..
        },
    ) in composition.component_exports.iter()
    {
        // A shim sub-component that provides an interface to another node in the
        // graph will appear in `handled_interfaces` (the interface is internal
        // wiring) but NOT in `outer_instances` (it is not the outermost node of
        // its chain).  If such a node is also present in `component_exports` it
        // is a spurious root-level export produced when wac compose flattens
        // shim sub-components to the peer level.  Exporting it would reference
        // the wrong (intermediate) instance, so we skip it here.
        //
        // Legitimate final exports (e.g. srv re-exporting an interface it
        // consumes from a provider) ARE in `outer_instances` (srv is the last
        // node of its chain), so they pass this check.
        if handled_interfaces.contains(export_name) && !outer_instances.contains_key(outer_inst_id)
        {
            continue;
        }

        let node_var = if let Some(generated_outer) = outer_instances.get(outer_inst_id) {
            generated_outer.clone()
        } else {
            let outer_node = &composition.nodes[outer_inst_id];
            get_or_create_inst(
                *outer_inst_id,
                &HashMap::new(),
                &mut used_comp_nodes,
                outer_node,
                &mut instance_vars,
                &None,
                &mut wac_lines,
            )
        };

        let export_line = format!("export {node_var}[\"{export_name}\"];");
        wac_lines.push(export_line);
    }

    // Create the wac command arguments!
    let args = gen_wac_args(
        shim_comps,
        splits_path,
        composition,
        &used_comp_nodes,
        &used_middlewares,
        node_paths,
    );

    Ok(WacOutput {
        wac: wac_lines.join("\n\n"),
        wac_deps: args,
        diagnostics,
        generated_adapters,
    })
}

/// Build the dependency map: a `BTreeMap` keyed by the fully-qualified
/// WAC package key (e.g. `"my:srv-a"`) so the result is directly
/// consumable by `wac-resolver::FileSystemPackageResolver`. Sorted
/// for deterministic shell-command formatting.
fn gen_wac_args(
    shim_comps: HashMap<usize, usize>,
    splits_path: &str,
    graph: &CompositionGraph,
    used_comps: &HashMap<u32, String>,
    used_mdls: &Vec<(String, String)>,
    node_paths: Option<&HashMap<u32, PathBuf>>,
) -> BTreeMap<String, PathBuf> {
    let mut deps: BTreeMap<String, PathBuf> = BTreeMap::new();

    for (inst_id, name) in used_comps.iter() {
        let comp_path: PathBuf = if let Some(paths) = node_paths {
            // Multi-component mode: use the original wasm path directly.
            paths
                .get(inst_id)
                .cloned()
                .unwrap_or_else(|| PathBuf::from(PATH_PLACEHOLDER))
        } else {
            // Single-component mode: derive path from the split directory.
            // We reserve component 0 for the root component, so add one here.
            let component_num = graph.nodes[inst_id].component_num + 1;
            let split_to_use = resolve_shim(component_num as usize, &shim_comps);
            PathBuf::from(gen_split_path(splits_path, split_to_use))
        };
        deps.insert(format!("{INST_PREFIX}:{name}"), comp_path);
    }

    // handle the used middlewares
    for (mw_name, mw_path) in used_mdls {
        deps.insert(format!("{INST_PREFIX}:{mw_name}"), PathBuf::from(mw_path));
    }

    deps
}
/// Pure: follow the shim chain until landing on a non-shim split.
/// See [`warn_about_shim_resolutions`] for the user-facing notice that
/// fires once per non-trivial resolution at the top of [`generate_wac`].
fn resolve_shim(mut component_num: usize, shim_comps: &HashMap<usize, usize>) -> usize {
    while is_shim_split_num(component_num, shim_comps) {
        component_num = shim_comps[&component_num];
    }
    component_num
}

/// Emit one WARN per non-trivial `shim → resolved` mapping in
/// `shim_comps`. Called once at the start of [`generate_wac`] so the
/// same assumption isn't announced twice when both the adapter-gen and
/// wac-dep paths later call [`resolve_shim`] for the same shim.
fn warn_about_shim_resolutions(shim_comps: &HashMap<usize, usize>) {
    let mut shim_keys: Vec<usize> = shim_comps.keys().copied().collect();
    shim_keys.sort();
    for shim_num in shim_keys {
        let resolved = resolve_shim(shim_num, shim_comps);
        if resolved != shim_num {
            eprintln!(
                "{}: {}",
                "WARN".yellow().bold(),
                format!(
                    "\tAssumption made! It is likely that split{shim_num} is a shim component,\n\
                     \tdefaulting to split{resolved} instead in the generated wac command!\n\
                     \tIf this assumption is incorrect, modify the generated wac command."
                )
                .yellow()
            );
        }
    }
}

/// Return value from rule application functions.
/// Separates "interface matched" from "full rule matched (interface + node names)",
/// so callers can emit precise diagnostics.
struct RuleApplyResult {
    contract_results: Vec<ContractResult>,
    /// True if the chain's interface matched the rule's interface field (regardless
    /// of whether the node-name conditions were also satisfied).
    interface_matched: bool,
    /// True if the full rule matched (interface + all node-name conditions).
    full_match: bool,
}

#[allow(clippy::too_many_arguments)]
fn apply_rule_between(
    rule: &SpliceRule,
    chain: &mut Chain,
    composition: &CompositionGraph,
    splits_path: &str,
    shim_comps: &HashMap<usize, usize>,
    checked_middlewares: &mut HashMap<String, BTreeMap<String, ExportInfo>>,
    generated_adapters: &mut Vec<GeneratedAdapter>,
) -> anyhow::Result<RuleApplyResult> {
    let mut contract_results = vec![];
    let mut interface_matched = false;
    let mut full_match = false;
    if let SpliceRule::Between {
        interface,
        inner_name,
        inner_alias,
        outer_name,
        outer_alias,
        inject,
    } = rule
    {
        for (i, window) in chain.chain.windows(2).enumerate() {
            let inner_id = window[0];
            let outer_id = window[1];
            let inner_node = &composition.nodes[&inner_id];
            let outer_node = &composition.nodes[&outer_id];

            let inner_var = get_name(inner_node).to_string();
            let outer_var = get_name(outer_node).to_string();
            if *interface != chain.interface.name {
                continue;
            }
            interface_matched = true;
            if *inner_name == inner_var && *outer_name == outer_var {
                full_match = true;
                let new_aliases = vec![
                    (inner_id, inner_alias.clone()),
                    (outer_id, outer_alias.clone()),
                ];
                let consumer_path =
                    chain.consumer_split_path(i + 1, composition, splits_path, shim_comps);
                contract_results.extend(add_to_inject_plan(
                    interface,
                    inject,
                    i + 1,
                    &new_aliases,
                    &mut chain.aliases,
                    &mut chain.inject_plan,
                    &chain.interface.ty_fingerprint,
                    chain.interface.interface_type.as_ref(),
                    splits_path,
                    consumer_path,
                    &composition.arena,
                    checked_middlewares,
                    generated_adapters,
                )?);
            }
        }
    }
    Ok(RuleApplyResult {
        contract_results,
        interface_matched,
        full_match,
    })
}

#[allow(clippy::too_many_arguments)]
fn apply_rule_before(
    rule: &SpliceRule,
    chain: &mut Chain,
    composition: &CompositionGraph,
    splits_path: &str,
    shim_comps: &HashMap<usize, usize>,
    checked_middlewares: &mut HashMap<String, BTreeMap<String, ExportInfo>>,
    generated_adapters: &mut Vec<GeneratedAdapter>,
) -> anyhow::Result<RuleApplyResult> {
    let mut contract_results = vec![];
    let mut interface_matched = false;
    let mut full_match = false;
    if let SpliceRule::Before {
        interface,
        provider_name,
        provider_alias,
        inject,
    } = rule
    {
        for (i, id) in chain.chain.iter().enumerate() {
            if *interface != chain.interface.name {
                continue;
            }
            interface_matched = true;
            let outer_node = &composition.nodes[id];
            if let Some(provider) = provider_name {
                if get_name(outer_node) != *provider {
                    continue;
                }
            }
            full_match = true;
            let new_aliases = vec![(*id, provider_alias.clone())];
            // Prefer the consumer's split (i+1) so the adapter copies
            // its import surface. At the outermost chain position
            // there's no consumer, so fall back to the provider's own
            // split (i) — the adapter mirrors the provider's full
            // import topology.
            let consumer_path = chain
                .consumer_split_path(i + 1, composition, splits_path, shim_comps)
                .or_else(|| chain.consumer_split_path(i, composition, splits_path, shim_comps));
            contract_results.extend(add_to_inject_plan(
                interface,
                inject,
                i + 1,
                &new_aliases,
                &mut chain.aliases,
                &mut chain.inject_plan,
                &chain.interface.ty_fingerprint,
                chain.interface.interface_type.as_ref(),
                splits_path,
                consumer_path,
                &composition.arena,
                checked_middlewares,
                generated_adapters,
            )?);
        }
    }
    Ok(RuleApplyResult {
        contract_results,
        interface_matched,
        full_match,
    })
}

#[allow(clippy::too_many_arguments)]
fn add_to_inject_plan(
    interface_name: &str,
    to_inject: &[Injection],
    chain_idx: usize,
    new_aliases: &[(u32, Option<String>)],
    aliases: &mut HashMap<u32, Option<String>>,
    inject_plan: &mut InjectPlan,
    contract_fingerprint: &Option<String>,
    interface_type: Option<&InterfaceType>,
    splits_path: &str,
    consumer_split: Option<String>,
    arena: &TypeArena,
    checked_middlewares: &mut HashMap<String, BTreeMap<String, ExportInfo>>,
    generated_adapters: &mut Vec<GeneratedAdapter>,
) -> anyhow::Result<Vec<ContractResult>> {
    // Check that the import/export contract is upheld by this plan and return results
    // to the caller — logging and error-handling is the caller's responsibility.
    let contract_results = validate_contract(
        to_inject,
        interface_name,
        contract_fingerprint,
        checked_middlewares,
    );

    // For tier-1 compatible middleware, generate a adapter component and substitute
    // the injection path so the rest of the WAC generation uses the adapter.
    let mut resolved: Vec<Injection> = Vec::with_capacity(to_inject.len());
    let mut final_results: Vec<ContractResult> = Vec::with_capacity(contract_results.len());
    for (injection, result) in to_inject.iter().zip(contract_results) {
        match result {
            ContractResult::Tier1Compatible(matched_interfaces) => {
                // `consumer_split` is the split the adapter inherits
                // its import preamble from. Callers upstream (the chain
                // walker in `apply_rule_before`) fall back from the
                // consumer at `i + 1` to the provider at `i`, so this
                // should always be `Some` for a valid composition. If
                // it isn't, something upstream shipped us a broken
                // chain and we can't generate a sound adapter.
                let consumer_split_path = consumer_split.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "No consumer/provider split available for interface '{interface_name}' \
                         while generating adapter for middleware '{}'. Please open an issue \
                         with a repro at https://github.com/ejrgilbert/splicer/issues",
                        injection.name
                    )
                })?;
                let adapter_path = generate_tier1_adapter(
                    &injection.name,
                    interface_name,
                    &matched_interfaces,
                    interface_type,
                    splits_path,
                    consumer_split_path,
                    arena,
                )?;
                generated_adapters.push(GeneratedAdapter {
                    adapter_path: adapter_path.clone(),
                    middleware_name: injection.name.clone(),
                    target_interface: interface_name.to_string(),
                    tier1_interfaces: matched_interfaces.clone(),
                });
                resolved.push(Injection {
                    name: injection.name.clone(),
                    // Keep the original middleware path; adapter_path goes in adapter_info.
                    path: injection.path.clone(),
                    adapter_info: Some(AdapterInjectionInfo {
                        adapter_path,
                        tier1_interfaces: matched_interfaces,
                    }),
                });
                // Tier1Compatible is fully handled here; no diagnostic needed upstream.
            }
            other => {
                resolved.push(injection.clone());
                final_results.push(other);
            }
        }
    }

    let middlewares = inject_plan
        .entry(chain_idx)
        .or_insert(IndexSet::from_iter(resolved.iter().cloned()));

    for (inst_id, new_alias) in new_aliases {
        if let (Some(new_alias), Some(Some(configured_alias))) = (new_alias, aliases.get(inst_id)) {
            if new_alias != configured_alias {
                anyhow::bail!(
                    "Internal error: alias conflict for interface '{interface_name}' — \
                     was configured as '{configured_alias}', but the tool prepared it as \
                     '{new_alias}' in some previous injection pass. Please report this bug."
                );
            }
        }
        aliases.insert(*inst_id, new_alias.clone());
    }

    middlewares.extend(resolved);
    Ok(final_results)
}

fn get_or_create_inst(
    inst_id: u32,
    aliases: &HashMap<u32, Option<String>>,
    used_comp_nodes: &mut HashMap<u32, String>,
    node: &ComponentNode,
    instance_vars: &mut HashMap<u32, String>,
    with_override: &Option<(Contract, String)>,
    wac_lines: &mut Vec<String>,
) -> String {
    if let Some(var) = instance_vars.get(&inst_id) {
        return var.clone();
    }
    let alias = aliases.get(&inst_id).cloned();

    // it hasn't been instantiated yet! do so here
    let pkg = if let Some(Some(alias)) = alias {
        alias.clone()
    } else {
        sanitize_wac_id(get_name(node))
    };
    used_comp_nodes.insert(inst_id, pkg.clone());
    let node_var = instance_vars
        .entry(inst_id)
        .or_insert_with(|| pkg.clone())
        .clone();

    let mut line = format!("let {node_var} = new {INST_PREFIX}:{pkg} {{");
    for conn in &node.imports {
        if !conn.is_host_import {
            let src_id = conn.source_instance;
            if let Some((
                Contract {
                    name: override_interface,
                    ..
                },
                override_var,
            )) = &with_override
            {
                let src_var = if conn.interface_name == *override_interface {
                    override_var.clone()
                } else if let Some(src_var) = instance_vars.get(&src_id.unwrap()) {
                    // could be an import from the host!
                    // only do this if it's not
                    src_var.clone()
                } else {
                    continue;
                };
                line.push_str(&format!(
                    "\n    \"{iface}\": {src}[\"{iface}\"],",
                    iface = conn.interface_name,
                    src = src_var
                ));
            }
        }
    }
    line.push_str("\n    ...\n};");
    wac_lines.push(line);

    node_var
}

fn create_mdl(
    input_inst: &String,
    mw: &String,
    interface: &Contract,
    wac_lines: &mut Vec<String>,
) -> String {
    let mw_line = format!(
        "let {mw} = new {INST_PREFIX}:{mw} {{\n    \"{interface}\": {input_inst}[\"{interface}\"], ...\n}};",
        interface = interface.name,
    );
    wac_lines.push(mw_line);

    mw.clone()
}

/// Emit WAC for a tier-1 adapter injection: two instances — the real middleware
/// (host-imports only) and the generated adapter wrapper that wires both.
///
/// Returns `(adapter_var_name, [(pkg_name, path), ...])` where the vec has two
/// entries: one for the real middleware and one for the adapter component.
fn create_tier1_mdl(
    downstream_inst: &str,
    mdl: &Injection,
    interface: &Contract,
    adapter_info: &AdapterInjectionInfo,
    wac_lines: &mut Vec<String>,
    emitted_mdl_vars: &mut std::collections::HashSet<String>,
) -> (String, Vec<(String, String)>) {
    let real_var = mdl.name.clone();
    // The adapter's core-wasm signature is specialized per target
    // interface, so a single middleware injected on multiple rules
    // must produce distinct adapter packages — one per interface —
    // or the generated wac's `deps` map collides under one pkg name
    // and only the last-generated adapter wasm reaches wac compose.
    let adapter_var = format!("{}-adapter-{}", mdl.name, sanitize_wac_id(&interface.name));

    // Real middleware — only has host imports, so no explicit wiring needed.
    // Emit the `let` once per mdl.name; adapters on later rules reuse
    // the same var.
    if emitted_mdl_vars.insert(real_var.clone()) {
        wac_lines.push(format!(
            "let {real_var} = new {INST_PREFIX}:{real_var} {{ ... }};"
        ));
    }

    // Proxy — wires the downstream target interface and the tier-1 hook interfaces
    // from the real middleware instance. The adapter's hook imports are versioned,
    // so the WAC lines use the versioned names to match both sides.
    use crate::contract::{versioned_interface, TIER1_VERSION};
    let mut adapter_line = format!(
        "let {adapter_var} = new {INST_PREFIX}:{adapter_var} {{\n    \"{iface}\": {downstream_inst}[\"{iface}\"],",
        iface = interface.name,
    );
    for tier1_iface in &adapter_info.tier1_interfaces {
        let versioned = versioned_interface(tier1_iface, TIER1_VERSION);
        adapter_line.push_str(&format!(
            "\n    \"{versioned}\": {real_var}[\"{versioned}\"],"
        ));
    }
    adapter_line.push_str("\n    ...\n};");
    wac_lines.push(adapter_line);

    let used = vec![
        (
            real_var,
            mdl.path
                .as_ref()
                .cloned()
                .unwrap_or(PATH_PLACEHOLDER.to_string()),
        ),
        (adapter_var.clone(), adapter_info.adapter_path.clone()),
    ];
    (adapter_var, used)
}

fn rule_interface(rule: &SpliceRule) -> &str {
    match rule {
        SpliceRule::Before { interface, .. } => interface,
        SpliceRule::Between { interface, .. } => interface,
    }
}

/// Helper to get the instance name from a node
fn get_name(node: &ComponentNode) -> &str {
    node.display_label()
}

/// Returns true if the split-file number `split_num` corresponds to a shim.
///
/// `split_num` is `node.component_num + 1` — the key space used by the
/// `shim_comps` map produced by `split_out_composition`.
fn is_shim_split_num(split_num: usize, shim_comps: &HashMap<usize, usize>) -> bool {
    shim_comps.contains_key(&split_num)
}

/// Convert an arbitrary node label into a valid WAC kebab-case identifier.
///
/// Node names in pre-composed binaries often look like `my:service/foo-shim`
/// (a WIT package path).  WAC identifiers may only contain `[a-z0-9-]`, so we
/// replace every invalid character with `-`.
///
/// Because the caller wraps the result in `new {INST_PREFIX}:{name}`, we also
/// strip a leading `my-` that would otherwise double the namespace prefix into
/// `my:my-…` when the raw name already started with `my:`.
fn sanitize_wac_id(raw: &str) -> String {
    let sanitized = raw.replace([':', '/', '.', '_', '@'], "-");
    sanitized
        .strip_prefix(&format!("{INST_PREFIX}-"))
        .map(str::to_string)
        .unwrap_or(sanitized)
}

fn reverse_set(set: &IndexSet<Injection>) -> Vec<Injection> {
    let mut res = vec![];
    for item in set.iter() {
        res.insert(0, item.clone());
    }
    res
}
