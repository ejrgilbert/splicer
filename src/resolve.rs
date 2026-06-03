use crate::parse::config::{Direction, Injection, SpliceRule};
use crate::select::{Constraint, FuncPred, Pattern, RuleMatcher, SiteKind};
use crate::wac::ChainSkeleton;
use cviz::model::CompositionGraph;
use std::collections::BTreeSet;

/// The unresolved-selector payload that flows into [`expand_subgraph`].
/// Borrowed from a [`SpliceRule::OnSubgraph`] variant — no ownership
/// changes, the expansion needs to read but not mutate.
struct Unresolved<'a> {
    nodes: &'a [String],
    direction: Direction,
    iface_filter: &'a Pattern,
    all_funcs: Option<&'a FuncPred>,
    inject: &'a [Injection],
}

/// Resolve graph-dependent selectors against the composition graph.
pub fn resolve_rules(
    rules: Vec<SpliceRule>,
    graph: &CompositionGraph,
) -> anyhow::Result<Vec<SpliceRule>> {
    let (skeletons, _) = crate::wac::build_chain_skeletons(graph);
    let mut out: Vec<SpliceRule> = Vec::with_capacity(rules.len());
    for rule in rules {
        match rule {
            SpliceRule::OnSubgraph {
                nodes,
                direction,
                interface,
                all_funcs,
                inject,
            } => {
                expand_subgraph(
                    graph,
                    &skeletons,
                    &Unresolved {
                        nodes: &nodes,
                        direction,
                        iface_filter: &interface,
                        all_funcs: all_funcs.as_ref(),
                        inject: &inject,
                    },
                    &mut out,
                );
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Walk every chain in the composition; for each boundary edge of
/// `nodes` (exactly one endpoint in the set), emit a per-edge
/// [`SpliceRule::Between`] (or [`SpliceRule::Before`] for the top-level
/// boundary position) with the chain's literal interface and node
/// names pinned. Direction picks which side of the boundary qualifies.
fn expand_subgraph(
    graph: &CompositionGraph,
    skeletons: &[ChainSkeleton],
    rule: &Unresolved<'_>,
    out: &mut Vec<SpliceRule>,
) {
    let subgraph: BTreeSet<&str> = rule.nodes.iter().map(String::as_str).collect();
    for sk in skeletons {
        if !rule.iface_filter.is_match(&sk.interface_name) {
            continue;
        }
        // Boundary edge iff exactly one endpoint is in the subgraph:
        //   provider in, caller out → external → subgraph → inbound
        //   caller in, provider out → subgraph → external → outbound
        for i in 0..sk.chain.len().saturating_sub(1) {
            let provider = node_label(graph, sk.chain[i]);
            let caller = node_label(graph, sk.chain[i + 1]);
            let (provider_in, caller_in) = (subgraph.contains(provider), subgraph.contains(caller));
            let matched = match (provider_in, caller_in) {
                (true, false) => matches!(rule.direction, Direction::Inbound | Direction::Both),
                (false, true) => matches!(rule.direction, Direction::Outbound | Direction::Both),
                _ => false,
            };
            if matched {
                out.push(make_between_literal(
                    &sk.interface_name,
                    provider,
                    caller,
                    rule.all_funcs,
                    rule.inject.to_vec(),
                ));
            }
        }
        // Chain's tail provider exports to outside the composition; if
        // it's in the subgraph, the external caller is out-of-subgraph
        // → inbound.
        if !matches!(rule.direction, Direction::Inbound | Direction::Both) {
            continue;
        }
        let Some(&last_id) = sk.chain.last() else {
            continue;
        };
        let last = node_label(graph, last_id);
        if subgraph.contains(last) {
            out.push(make_before_literal(
                &sk.interface_name,
                last,
                rule.all_funcs,
                rule.inject.to_vec(),
            ));
        }
    }
}

fn node_label(graph: &CompositionGraph, id: u32) -> &str {
    graph.nodes[&id].display_label()
}

fn pattern_from_literal(s: &str) -> Pattern {
    Pattern::compile(vec![s.to_string()]).expect("literal can't be an invalid glob")
}

/// Build a `SpliceRule::Between` with literal interface and literal
/// inner/outer node names — the shape emitted per matched boundary edge.
fn make_between_literal(
    interface_name: &str,
    inner: &str,
    outer: &str,
    all_funcs: Option<&FuncPred>,
    inject: Vec<Injection>,
) -> SpliceRule {
    SpliceRule::Between {
        matcher: RuleMatcher::new(
            SiteKind::Between,
            pattern_from_literal(interface_name),
            all_funcs.cloned(),
            vec![
                Constraint::Provider(pattern_from_literal(inner)),
                Constraint::Caller(pattern_from_literal(outer)),
            ],
        ),
        inner_alias: None,
        outer_alias: None,
        inject,
    }
}

fn make_before_literal(
    interface_name: &str,
    provider: &str,
    all_funcs: Option<&FuncPred>,
    inject: Vec<Injection>,
) -> SpliceRule {
    SpliceRule::Before {
        matcher: RuleMatcher::new(
            SiteKind::Before,
            pattern_from_literal(interface_name),
            all_funcs.cloned(),
            vec![Constraint::Provider(pattern_from_literal(provider))],
        ),
        provider_alias: None,
        inject,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::config::parse_yaml;

    /// Three-node chain `A → B → C` over `IFACE`, all in instance-id
    /// order so `build_chain_skeletons` produces a single skeleton
    /// `chain = [0, 1, 2]` (innermost A, outermost C).
    fn chain_a_b_c() -> CompositionGraph {
        use cviz::model::{ComponentNode, InterfaceConnection};
        let mut g = CompositionGraph::new();
        // Node 0 = A (innermost provider).
        g.add_node(0, ComponentNode::new("$A".into(), 0, 0));
        // Node 1 = B (imports IFACE from A).
        let mut b = ComponentNode::new("$B".into(), 1, 1);
        b.add_import(InterfaceConnection {
            interface_name: IFACE.into(),
            source_instance: Some(0),
            is_host_import: false,
            interface_type: None,
            fingerprint: None,
        });
        g.add_node(1, b);
        // Node 2 = C (imports IFACE from B; outermost).
        let mut c = ComponentNode::new("$C".into(), 2, 2);
        c.add_import(InterfaceConnection {
            interface_name: IFACE.into(),
            source_instance: Some(1),
            is_host_import: false,
            interface_type: None,
            fingerprint: None,
        });
        g.add_node(2, c);
        g
    }

    const IFACE: &str = "wasi:http/handler@0.3.0";

    fn yaml_subgraph(nodes: &[&str], direction: &str) -> String {
        let nodes_yaml = nodes
            .iter()
            .map(|n| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"
version: 1
rules:
  - on_subgraph:
      nodes: [{nodes_yaml}]
      direction: {direction}
    inject:
      - name: mw
        path: ./mw.wasm
"#
        )
    }

    #[derive(Debug, PartialEq, Eq)]
    struct BetweenLiteral {
        interface: String,
        inner: String,
        outer: String,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct BeforeLiteral {
        interface: String,
        provider: String,
    }

    #[derive(Default, Debug)]
    struct Summary {
        betweens: Vec<BetweenLiteral>,
        befores: Vec<BeforeLiteral>,
    }

    /// Pull each emitted `Between` rule's (interface, inner, outer) and
    /// each `Before` rule's (interface, provider) out of `rules`. Lets
    /// the resolve-pass tests assert the shape of the expansion without
    /// depending on internal `RuleMatcher` representation.
    fn summarize_rules(rules: &[SpliceRule]) -> Summary {
        let first_raw =
            |opt: Option<&[String]>| opt.and_then(|s| s.first()).cloned().unwrap_or_default();
        let mut out = Summary::default();
        for r in rules {
            match r {
                SpliceRule::Between { matcher, .. } => out.betweens.push(BetweenLiteral {
                    interface: first_raw(Some(matcher.interface_raw())),
                    inner: first_raw(matcher.provider_raw()),
                    outer: first_raw(matcher.caller_raw()),
                }),
                SpliceRule::Before { matcher, .. } => out.befores.push(BeforeLiteral {
                    interface: first_raw(Some(matcher.interface_raw())),
                    provider: first_raw(matcher.provider_raw()),
                }),
                SpliceRule::OnSubgraph { .. } => {
                    panic!("resolve_rules should have expanded OnSubgraph")
                }
            }
        }
        out
    }

    fn between(inner: &str, outer: &str) -> BetweenLiteral {
        BetweenLiteral {
            interface: IFACE.into(),
            inner: inner.into(),
            outer: outer.into(),
        }
    }
    fn before(provider: &str) -> BeforeLiteral {
        BeforeLiteral {
            interface: IFACE.into(),
            provider: provider.into(),
        }
    }

    #[test]
    fn resolve_on_subgraph_both_emits_inbound_and_outbound_boundary() {
        // S = {B}. Chain A → B → C:
        //   (A,B): A∉S, B∈S → outbound (B calls A)
        //   (B,C): B∈S, C∉S → inbound (C calls B)
        // C∉S so no top-level inbound rule.
        let g = chain_a_b_c();
        let rules = parse_yaml(&yaml_subgraph(&["B"], "both")).unwrap();
        let resolved = resolve_rules(rules, &g).unwrap();
        let s = summarize_rules(&resolved);
        assert!(s.befores.is_empty(), "C∉S so no boundary Before rule");
        assert_eq!(s.betweens.len(), 2);
        assert!(
            s.betweens.contains(&between("A", "B")),
            "outbound (B calls A) edge missing"
        );
        assert!(
            s.betweens.contains(&between("B", "C")),
            "inbound (C calls B) edge missing"
        );
    }

    #[test]
    fn resolve_on_subgraph_inbound_only() {
        let g = chain_a_b_c();
        let rules = parse_yaml(&yaml_subgraph(&["B"], "inbound")).unwrap();
        let s = summarize_rules(&resolve_rules(rules, &g).unwrap());
        assert!(s.befores.is_empty());
        assert_eq!(s.betweens, vec![between("B", "C")]);
    }

    #[test]
    fn resolve_on_subgraph_outbound_only() {
        let g = chain_a_b_c();
        let rules = parse_yaml(&yaml_subgraph(&["B"], "outbound")).unwrap();
        let s = summarize_rules(&resolve_rules(rules, &g).unwrap());
        assert!(s.befores.is_empty());
        assert_eq!(s.betweens, vec![between("A", "B")]);
    }

    #[test]
    fn resolve_on_subgraph_top_level_boundary_emits_before() {
        // S = {B, C}. Chain A → B → C:
        //   (A,B): A∉S, B∈S → outbound
        //   (B,C): both in S → internal, not boundary
        // Top of chain C∈S → external calls C = inbound boundary → emits Before.
        let g = chain_a_b_c();
        let rules = parse_yaml(&yaml_subgraph(&["B", "C"], "both")).unwrap();
        let s = summarize_rules(&resolve_rules(rules, &g).unwrap());
        assert_eq!(s.betweens, vec![between("A", "B")]);
        assert_eq!(s.befores, vec![before("C")]);
    }

    #[test]
    fn resolve_passthrough_for_non_subgraph_rules() {
        // resolve_rules is a no-op for Before/Between rules.
        let g = chain_a_b_c();
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).unwrap();
        let resolved = resolve_rules(rules, &g).unwrap();
        assert_eq!(resolved.len(), 1);
        assert!(matches!(resolved[0], SpliceRule::Before { .. }));
    }
}
