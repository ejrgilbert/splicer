use cviz::model::{ComponentNode, CompositionGraph, InterfaceConnection};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use colored::Colorize;
use wasmparser::collections::IndexSet;

pub const INST_PREFIX: &str = "my";
const PATH_PLACEHOLDER: &str = "/path/to/comp.wasm";
use crate::parse::config::{Injection, SpliceRule};
use crate::split::gen_split_path;

// chain_idx -> set of middlewares to inject AFTER
type InjectPlan = HashMap<usize, IndexSet<Injection>>;

struct Chain {
    interface: String,
    chain: Vec<u32>,
    aliases: HashMap<u32, Option<String>>,
    // middlewares to inject after the specified index in the chain
    inject_plan: InjectPlan
}

/// Generate WAC from a composition graph and a set of splicing rules.
/// Returns:
/// - The generated Wac
/// - A list of the `wac compose` args: (service-name, service-path)
pub fn generate_wac(shim_comps: Vec<usize>, splits_path: &str, composition: &CompositionGraph, rules: &[SpliceRule]) -> (String, Vec<(String, String)>) {
    let mut wac_lines = vec!["package example:composition;".to_string()];

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
        } in node.imports.iter()
        {
            let mut chain = vec![*outer_node_id];
            if !is_host_import {
                chain.push(*source_instance);
            }
            let mut current_id = *source_instance;
            while let Some(node) = composition.nodes.get(&current_id) {
                if let Some(conn) = node
                    .imports
                    .iter()
                    .find(|c| c.interface_name == *interface_name)
                {
                    if !conn.is_host_import {
                        let src_id = conn.source_instance;
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
                    interface: interface_name.to_string(),
                    chain,
                    aliases: HashMap::new(),
                    inject_plan: HashMap::new(),
                });
            }
            handled_interfaces.insert(interface_name.to_string());
        }
    }

    // handle standalone exported interfaces!
    for (interface, source_inst) in composition.component_exports.iter() {
        if handled_interfaces.contains(interface) {
            continue;
        }

        // if we've reached this point, it's guaranteed to not be a chain (chains were handled above)
        // this is just a single exported service func.
        chains.push(Chain {
            interface: interface.clone(),
            chain: vec![*source_inst],
            aliases: HashMap::new(),
            inject_plan: HashMap::new(),
        });
    }

    // Apply the rules in order of their declaration in the configuration.
    // This enforces an ordering semantic for the rule application.
    for rule in rules.iter() {
        for chain in chains.iter_mut() {
            apply_rule_between(rule, chain, composition);
            apply_rule_before(rule, chain, composition);
        }
    }

    // Let's now generate WAC to handle the chains we've planned to emit
    let mut mdl_override = None;
    let mut last;
    let mut instance_vars: HashMap<u32, String> = HashMap::new();
    let mut outer_instances: HashMap<u32, String> = HashMap::new(); // orig_inst_id -> generated_outer_var
    let mut used_comp_nodes: HashMap<u32, String> = HashMap::new(); // inst_id -> used_name
    let mut used_middlewares: Vec<(String, String)> = Vec::new(); // (used_name, path)
    for Chain {
        interface: chain_interface,
        chain,
        aliases,
        inject_plan,
    } in chains.iter()
    {
        for (i, id) in chain.iter().enumerate() {
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

            if let Some(middlewares) = inject_plan.get(&(i + 1)) {
                // if the NEXT node has a middleware BEFORE it, inject here!
                // Reverse the list of items to inject (this keeps me from having to deal with this in the `wac` generation logic).
                // Through doing this, the order of middlewares invoked will follow the order of declaration in the configuration.
                let reversed_list = reverse_set(middlewares);
                for mdl in reversed_list.iter() {
                    // instantiate
                    last = create_mdl(&last, &mdl.name, chain_interface, &mut wac_lines);
                    used_middlewares.push((last.clone(), mdl.path.as_ref().cloned().unwrap_or(PATH_PLACEHOLDER.to_string())));
                    mdl_override = Some((chain_interface.clone(), last.clone()));
                }
            }
            if i == chain.len() - 1 {
                // If we're at the end of the chain, remember what our outermost layer is now.
                // This makes sure we actually export middleware if it overrode the outermost service.
                outer_instances.insert(*id, last.clone());
            }
        }
    }

    // Generate WAC to export the appropriate functions
    for (export_name, outer_inst_id) in composition.component_exports.iter() {
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
    let args = gen_wac_args(shim_comps, splits_path, composition, &used_comp_nodes, &used_middlewares);

    (wac_lines.join("\n\n"), args)
}

fn gen_wac_args(shim_comps: Vec<usize>, splits_path: &str, graph: &CompositionGraph, used_comps: &HashMap<u32, String>, used_mdls: &Vec<(String, String)>) -> Vec<(String, String)> {
    // List of (used_name, path)
    let mut args = vec![];

    // handle the used component parts
    for (inst_id, name) in used_comps.iter() {
        // we reserve component 0 for the root component, so add one here!
        let component_num = graph.nodes[inst_id].component_num + 1;
        let comp_offset = if shim_comps.contains(&(component_num as usize)) {
            // this is likely a shim component
            eprintln!("{}: {}", "WARN".yellow().bold(), format!("\tAssumption made! It is likely that split{} is a shim component, defaulting to split{} instead in the generated wac command!\n\
                                                     \tIf this assumption is incorrect, modify the generated wac command.", component_num, component_num - 1).yellow());
            1
        } else { 0 };
        let comp_path = gen_split_path(splits_path, (component_num - comp_offset) as usize);
        args.push((name.clone(), comp_path));
    }

    // handle the used middlewares
    args.extend(used_mdls.clone());

    args
}

fn apply_rule_between(rule: &SpliceRule, chain: &mut Chain, composition: &CompositionGraph) {
    let Chain {
        interface: chain_interface,
        chain,
        inject_plan,
        aliases,
    } = chain;
    if let SpliceRule::Between {
        interface,
        inner_name,
        inner_alias,
        outer_name,
        outer_alias,
        inject,
    } = rule
    {
        for (i, window) in chain.windows(2).enumerate() {
            let inner_id = window[0];
            let outer_id = window[1];
            let inner_node = &composition.nodes[&inner_id];
            let outer_node = &composition.nodes[&outer_id];

            let inner_var = get_name(inner_node).to_string();
            let outer_var = get_name(outer_node).to_string();
            if interface != chain_interface {
                continue;
            }
            if *inner_name == inner_var && *outer_name == outer_var {
                let new_aliases = vec![
                    (inner_id, inner_alias.clone()),
                    (outer_id, outer_alias.clone())
                ];

                // matches! We want to inject BEFORE the outer's index
                add_to_inject_plan(inject, i + 1, &new_aliases, aliases, inject_plan);
            }
        }
    }
}

fn apply_rule_before(rule: &SpliceRule, chain: &mut Chain, composition: &CompositionGraph) {
    let Chain {
        interface: chain_interface,
        chain,
        inject_plan,
        aliases,
    } = chain;
    if let SpliceRule::Before {
        interface,
        provider_name,
        provider_alias,
        inject,
    } = rule
    {
        for (i, id) in chain.iter().enumerate() {
            if interface != chain_interface {
                continue;
            }
            let outer_node = &composition.nodes[id];
            if let Some(provider) = provider_name {
                if get_name(outer_node) != *provider {
                    continue;
                }
            }
            let new_aliases = vec![
                (*id, provider_alias.clone()),
            ];
            // matches! We want to inject BEFORE the instance this guy's plugged into
            add_to_inject_plan(inject, i + 1, &new_aliases, aliases, inject_plan);
        }
    }
}

fn add_to_inject_plan(
    to_inject: &[Injection],
    chain_idx: usize,
    new_aliases: &[(u32, Option<String>)],
    aliases: &mut HashMap<u32, Option<String>>,
    inject_plan: &mut InjectPlan,
) {
    let middlewares = inject_plan.entry(chain_idx).or_insert(IndexSet::from_iter(to_inject.iter().cloned()));

    for (inst_id, new_alias) in new_aliases {
        if let (Some(new_alias), Some(Some(configured_alias))) = (new_alias, aliases.get(&inst_id)) {
            if new_alias != configured_alias {
                // panic! conflicting aliases!
                todo!()
            }
        }
        aliases.insert(*inst_id, new_alias.clone());
    }

    middlewares.extend(to_inject.iter().cloned());
}

fn get_or_create_inst(
    inst_id: u32,
    aliases: &HashMap<u32, Option<String>>,
    used_comp_nodes: &mut HashMap<u32, String>,
    node: &ComponentNode,
    instance_vars: &mut HashMap<u32, String>,
    with_override: &Option<(String, String)>,
    wac_lines: &mut Vec<String>,
) -> String {
    if let Some(var) = instance_vars.get(&inst_id) {
        return var.clone();
    }
    let alias = if let Some(alias) = aliases.get(&inst_id)
    {
        Some(alias.clone())
    } else {
        None
    };

    // it hasn't been instantiated yet! do so here
    let pkg = if let Some(Some(alias)) = alias {
        alias.clone()
    } else {
        get_name(node).to_string()
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
            if let Some((override_interface, override_var)) = &with_override {
                let src_var = if conn.interface_name == *override_interface {
                    override_var.clone()
                } else if let Some(src_var) = instance_vars.get(&src_id) {
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
    interface: &String,
    wac_lines: &mut Vec<String>,
) -> String {
    let mw_line = format!(
        "let {mw} = new {INST_PREFIX}:{mw} {{\n    \"{interface}\": {input_inst}[\"{interface}\"], ...\n}};"
    );
    wac_lines.push(mw_line);

    mw.clone()
}

/// Helper to get the instance name from a node
fn get_name(node: &ComponentNode) -> &str {
    node.display_label()
}

fn reverse_set(set: &IndexSet<Injection>) -> Vec<Injection> {
    let mut res = vec![];
    for item in set.iter() {
        res.insert(0, item.clone());
    }
    res
}
