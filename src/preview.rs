use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cviz::canonical_edge_id;
use cviz::model::CompositionGraph;
use cviz::parse::component::parse_component;
use cviz::{HighlightColor, Highlights, Selection};

use crate::parse::config::{parse_yaml, SpliceRule};
use crate::resolve::resolve_rules;
use crate::select::{FuncPred, FuncScope, Pattern, ValueProperty};
use crate::wac::{build_chain_skeletons, ChainSkeleton, SkipRecord};

#[derive(Debug, Clone)]
pub struct PreviewRequest {
    pub composition_wasm: PathBuf,
    pub rules_yaml: String,
    /// Only renders specified rule's targets
    pub only_rule: Option<usize>,
    /// Also check if strategy rule compiles against a matched interface.
    pub exact: bool,
}

pub struct PreviewOutput {
    pub graph: CompositionGraph,
    pub highlights: Highlights,
    /// Rules whose glob matched no interface at all.
    pub unmatched_rules: Vec<usize>,
    /// Rules that matched, but didn't compile against interface (only with --exact)
    pub incompatible_rules: Vec<usize>,
}

pub fn preview(req: PreviewRequest) -> Result<PreviewOutput> {
    let PreviewRequest {
        composition_wasm,
        rules_yaml,
        only_rule,
        exact,
    } = req;

    let bytes = std::fs::read(&composition_wasm).with_context(|| {
        format!(
            "Failed to read composition wasm: {}",
            composition_wasm.display()
        )
    })?;
    let graph = parse_component(&bytes).with_context(|| {
        format!(
            "Failed to parse composition graph from: {}",
            composition_wasm.display()
        )
    })?;

    let skips = if exact {
        compatibility_skips(&composition_wasm, &rules_yaml)?
    } else {
        Vec::new()
    };
    preview_core(graph, &rules_yaml, only_rule, &skips)
}

pub fn preview_with_graph(
    graph: CompositionGraph,
    rules_yaml: &str,
    only_rule: Option<usize>,
) -> Result<PreviewOutput> {
    preview_core(graph, rules_yaml, only_rule, &[])
}

fn preview_core(
    graph: CompositionGraph,
    rules_yaml: &str,
    only_rule: Option<usize>,
    skips: &[SkipRecord],
) -> Result<PreviewOutput> {
    let parsed = parse_yaml(rules_yaml).context("Failed to parse splice rules YAML")?;
    let (skeletons, _handled) = build_chain_skeletons(&graph);

    let mut highlights = Highlights::default();
    let mut unmatched_rules: Vec<usize> = Vec::new();
    let mut incompatible_rules: Vec<usize> = Vec::new();

    for (idx, parsed_rule) in parsed.into_iter().enumerate() {
        let rule_num = (idx + 1) as u32;
        if let Some(only) = only_rule {
            if only as u32 != rule_num {
                continue;
            }
        }

        let desc = rule_description(idx + 1, &parsed_rule);
        highlights
            .register_tag(rule_num, &desc)
            .with_context(|| format!("registering tag for rule {rule_num}"))?;

        // Structural overlay: paint nodes / internal edges in context
        // colors BEFORE the per-edge walk, so "last write wins" lets
        // matched edges override context where they overlap.
        match &parsed_rule {
            SpliceRule::OnNode { name, .. } => {
                paint_on_node_overlay(rule_num, name, &graph, &mut highlights);
            }
            SpliceRule::OnSubgraph { nodes, .. } => {
                paint_on_subgraph_overlay(rule_num, nodes, &graph, &skeletons, &mut highlights);
            }
            _ => {}
        }

        // Resolve this one user rule into per-edge rules and walk for
        // edge highlights. All emitted rules share `rule_num` because
        // they came from this iteration's user rule.
        let resolved = resolve_rules(vec![parsed_rule], &graph)?;
        // `selected` = the glob matched at least one edge; `kept` = at
        // least one survived `--exact` pruning. They diverge only when a
        // rule matched interfaces its strategy doesn't compile against.
        let mut selected = false;
        let mut kept = false;
        for rule in &resolved {
            let matcher = rule.matcher();
            for (chain_idx, sk) in skeletons.iter().enumerate() {
                let sites = matcher
                    .select(
                        chain_idx,
                        &sk.chain,
                        &sk.interface_name,
                        sk.interface_type.as_ref(),
                        &graph,
                    )
                    .with_context(|| format!("rule {rule_num}"))?;
                for site in &sites {
                    selected = true;
                    // In --exact, drop matches the real compile rejected
                    // (strategy doesn't fit this interface); empty `skips`
                    // in the default mode prunes nothing.
                    if site_pruned(rule, &sk.interface_name, skips) {
                        continue;
                    }
                    kept = true;
                    let edge_id = chain_edge_id(sk, site.provider_idx, site.has_caller, &graph);
                    highlights.mark(Selection::edge(edge_id).tag(rule_num));
                }
            }
        }
        if !selected {
            // Glob matched no interface at all.
            unmatched_rules.push(rule_num as usize);
        } else if !kept {
            // Matched interfaces, but the strategy fit none of them.
            incompatible_rules.push(rule_num as usize);
        }
    }

    Ok(PreviewOutput {
        graph,
        highlights,
        unmatched_rules,
        incompatible_rules,
    })
}

/// True if one of the rule's injected strategies didn't compile against the interface
fn site_pruned(rule: &SpliceRule, interface: &str, skips: &[SkipRecord]) -> bool {
    rule.inject().iter().any(|inj| {
        let label = inj.builtin.as_deref().unwrap_or(inj.name.as_str());
        skips
            .iter()
            .any(|s| s.interface == interface && s.strategy == label)
    })
}

/// Run the splice pipeline through tier-3/4 materialization and return the
/// matches it pruned because the strategy didn't compile against the interface
fn compatibility_skips(composition_wasm: &Path, rules_yaml: &str) -> Result<Vec<SkipRecord>> {
    let tmp = tempfile::tempdir().context("create temp splits dir for --exact preview")?;
    let bundle = crate::splice(crate::SpliceRequest {
        composition_wasm: composition_wasm.to_path_buf(),
        rules_yaml: rules_yaml.to_string(),
        package_name: "preview:exact".to_string(),
        splits_dir: tmp.path().to_path_buf(),
        skip_type_check: true,
        strict: false,
    })?;
    Ok(bundle.skips)
}

/// Context color shared by all structural overlays — sits beneath any
/// matched-edge marks the per-edge walk lays down later.
const OVERLAY_COLOR: HighlightColor = HighlightColor::Green;

/// Build the canonical edge id for the edge at `provider_idx` in `sk`.
/// `has_caller=true` looks up the caller at `provider_idx + 1`; `false`
/// produces a boundary-edge id (no in-chain caller).
fn chain_edge_id(
    sk: &ChainSkeleton,
    provider_idx: usize,
    has_caller: bool,
    graph: &CompositionGraph,
) -> String {
    let provider = graph.nodes[&sk.chain[provider_idx]].canonical_id();
    let caller = has_caller.then(|| graph.nodes[&sk.chain[provider_idx + 1]].canonical_id());
    canonical_edge_id(&sk.interface_name, caller, provider)
}

/// Mark every graph node whose display label satisfies `matches` in
/// the overlay color. Used by both on_node and on_subgraph overlays.
fn mark_nodes_where(
    rule_num: u32,
    graph: &CompositionGraph,
    highlights: &mut Highlights,
    matches: impl Fn(&str) -> bool,
) {
    for node in graph.nodes.values() {
        if matches(node.display_label()) {
            highlights.mark(
                Selection::node(node.canonical_id().to_string())
                    .color(OVERLAY_COLOR)
                    .tag(rule_num),
            );
        }
    }
}

/// Paint every graph node whose display label matches `name` (a glob).
fn paint_on_node_overlay(
    rule_num: u32,
    name: &Pattern,
    graph: &CompositionGraph,
    highlights: &mut Highlights,
) {
    mark_nodes_where(rule_num, graph, highlights, |label| name.is_match(label));
}

/// Paint every node listed in `nodes` plus every edge between two of
/// those nodes (internal edges) in the overlay color. Boundary edges
/// are NOT painted here — they get the default color from the per-edge
/// walk via "last write wins".
fn paint_on_subgraph_overlay(
    rule_num: u32,
    nodes: &[String],
    graph: &CompositionGraph,
    skeletons: &[ChainSkeleton],
    highlights: &mut Highlights,
) {
    use std::collections::BTreeSet;
    let subgraph: BTreeSet<&str> = nodes.iter().map(String::as_str).collect();
    mark_nodes_where(rule_num, graph, highlights, |label| {
        subgraph.contains(label)
    });
    for sk in skeletons {
        for i in 0..sk.chain.len().saturating_sub(1) {
            let provider_label = graph.nodes[&sk.chain[i]].display_label();
            let caller_label = graph.nodes[&sk.chain[i + 1]].display_label();
            if subgraph.contains(provider_label) && subgraph.contains(caller_label) {
                let edge_id = chain_edge_id(sk, i, true, graph);
                highlights.mark(Selection::edge(edge_id).color(OVERLAY_COLOR).tag(rule_num));
            }
        }
    }
}

/// One-line legend string for `rule`: `#N <kind> <field>=<value> ...`.
/// `inject:` is never rendered since it's the *effect*, not the *selection*.
pub fn rule_description(idx: usize, rule: &SpliceRule) -> String {
    let mut parts: Vec<String> = vec![format!("#{idx}")];
    let glob = |key: &str, raw: &[String]| format!("{key}={}", quoted_globs(raw));
    let funcs = |p: Option<&FuncPred>| p.map(|f| format!("all-funcs={}", render_func_pred(f)));

    match rule {
        SpliceRule::Before { matcher, .. } => {
            parts.push("before".into());
            parts.push(glob("interface", matcher.interface_raw()));
            parts.extend(matcher.provider_raw().map(|p| glob("provider", p)));
            parts.extend(funcs(matcher.all_funcs()));
        }
        SpliceRule::Between { matcher, .. } => {
            parts.push("between".into());
            parts.push(glob("interface", matcher.interface_raw()));
            parts.extend(matcher.provider_raw().map(|p| glob("inner", p)));
            parts.extend(matcher.caller_raw().map(|c| glob("outer", c)));
            parts.extend(funcs(matcher.all_funcs()));
        }
        SpliceRule::OnNode {
            name,
            direction,
            interface,
            all_funcs,
            ..
        } => {
            parts.push("on_node".into());
            parts.push(glob("name", name.raw()));
            parts.push(format!("direction={direction}"));
            parts.push(glob("interface", interface.raw()));
            parts.extend(funcs(all_funcs.as_ref()));
        }
        SpliceRule::OnSubgraph {
            nodes,
            direction,
            interface,
            all_funcs,
            ..
        } => {
            parts.push("on_subgraph".into());
            parts.push(format!("nodes=[{}]", nodes.join(",")));
            parts.push(format!("direction={direction}"));
            parts.push(glob("interface", interface.raw()));
            parts.extend(funcs(all_funcs.as_ref()));
        }
    }
    parts.join(" ")
}

fn quoted_globs(globs: &[String]) -> String {
    format!("\"{}\"", globs.join("|"))
}

/// `+`-joined shorthand for `all-funcs:`. Each piece is `async`/`sync`
/// or `key=val[,val]`; default scope is omitted.
fn render_func_pred(p: &FuncPred) -> String {
    let mut pieces: Vec<String> = Vec::new();
    if let Some(is_async) = p.is_async {
        pieces.push(if is_async {
            "async".into()
        } else {
            "sync".into()
        });
    }
    if !is_default_scope(&p.scopes) {
        let names: Vec<&str> = p
            .scopes
            .iter()
            .map(|s| match s {
                FuncScope::Interface => "interface",
                FuncScope::Resource => "resource",
            })
            .collect();
        pieces.push(format!("scope={}", names.join("|")));
    }
    if !p.args.is_empty() {
        pieces.push(format!("args={}", join_props(&p.args)));
    }
    if !p.results.is_empty() {
        pieces.push(format!("results={}", join_props(&p.results)));
    }
    pieces.join("+")
}

fn is_default_scope(scopes: &[FuncScope]) -> bool {
    matches!(scopes, [FuncScope::Interface])
}

fn join_props(props: &[ValueProperty]) -> String {
    props
        .iter()
        .map(|p| match p {
            ValueProperty::Concrete => "concrete",
            ValueProperty::Defaultable => "defaultable",
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::config::Injection;
    use crate::select::{Constraint, FuncPred, FuncScope, RuleMatcher, SiteKind, ValueProperty};
    use cviz::model::{ComponentNode, InterfaceConnection};

    const HTTP: &str = "wasi:http/handler@0.3.0";

    /// Minimal `consumer -> provider` graph over [`HTTP`] so a `before`
    /// rule on the interface yields a selectable site.
    fn http_chain() -> CompositionGraph {
        let mut graph = CompositionGraph::new();
        let mut srv = ComponentNode::new("$srv".to_string(), 0, 0);
        srv.add_import(InterfaceConnection {
            interface_name: HTTP.into(),
            source_instance: None,
            is_host_import: true,
            interface_type: None,
            fingerprint: None,
        });
        graph.add_node(1, srv);
        graph.add_export(HTTP.into(), 1, None);
        graph
    }

    const EXACT_PRUNE_YAML: &str = r#"
version: 1
rules:
  - before:
      interface: "wasi:http/handler@0.3.0"
    inject:
      - name: my-strat
        path: ./x.wasm
"#;

    #[test]
    fn exact_prune_marks_rule_incompatible_not_unmatched() {
        let skips = vec![SkipRecord {
            strategy: "my-strat".into(),
            interface: HTTP.into(),
            bound: None,
        }];
        let out = preview_core(http_chain(), EXACT_PRUNE_YAML, None, &skips).expect("preview");
        assert_eq!(out.incompatible_rules, vec![1]);
        assert!(out.unmatched_rules.is_empty());
        assert!(out.highlights.is_empty(), "pruned rule should mark nothing");
    }

    #[test]
    fn default_mode_keeps_match_and_leaves_incompatible_empty() {
        // Same rule, no skips (default preview): the match stays.
        let out = preview_core(http_chain(), EXACT_PRUNE_YAML, None, &[]).expect("preview");
        assert!(out.incompatible_rules.is_empty());
        assert!(out.unmatched_rules.is_empty());
        assert!(!out.highlights.is_empty(), "match should be highlighted");
    }

    const TWO_RULES_SAME_EDGE_YAML: &str = r#"
version: 1
rules:
  - before:
      interface: "wasi:http/handler@0.3.0"
    inject:
      - name: bad-strat
        path: ./bad.wasm
  - before:
      interface: "wasi:http/handler@0.3.0"
    inject:
      - name: good-strat
        path: ./good.wasm
"#;

    #[test]
    fn exact_prunes_only_failing_rule_edge_stays_highlighted() {
        // Both rules target the same edge; only `bad-strat` fails to compile.
        let skips = vec![SkipRecord {
            strategy: "bad-strat".into(),
            interface: HTTP.into(),
            bound: None,
        }];
        let out =
            preview_core(http_chain(), TWO_RULES_SAME_EDGE_YAML, None, &skips).expect("preview");
        // Rule 1 fully pruned; rule 2 is fine.
        assert_eq!(out.incompatible_rules, vec![1]);
        assert!(out.unmatched_rules.is_empty());
        // Rule 1 never marks anything, so a non-empty highlight set can
        // only come from rule 2: the edge survives via the compatible rule.
        assert!(
            !out.highlights.is_empty(),
            "edge must stay highlighted via the compatible rule 2"
        );
    }

    fn pat(globs: &[&str]) -> crate::select::Pattern {
        crate::select::Pattern::compile(globs.iter().map(|s| s.to_string()).collect())
            .expect("valid glob")
    }

    /// Build a `before` SpliceRule from the description-relevant fields.
    /// `provider`, `all_funcs` are optional; injection list is empty
    /// (we never render it).
    fn before_rule(
        interface: &[&str],
        provider: Option<&[&str]>,
        all_funcs: Option<FuncPred>,
    ) -> SpliceRule {
        let mut constraints = vec![];
        if let Some(p) = provider {
            constraints.push(Constraint::Provider(pat(p)));
        }
        SpliceRule::Before {
            matcher: RuleMatcher::new(SiteKind::Before, pat(interface), all_funcs, constraints),
            provider_alias: None,
            inject: Vec::<Injection>::new(),
        }
    }

    fn between_rule(
        interface: &[&str],
        inner: Option<&[&str]>,
        outer: Option<&[&str]>,
        all_funcs: Option<FuncPred>,
    ) -> SpliceRule {
        let mut constraints = vec![];
        if let Some(i) = inner {
            constraints.push(Constraint::Provider(pat(i)));
        }
        if let Some(o) = outer {
            constraints.push(Constraint::Caller(pat(o)));
        }
        SpliceRule::Between {
            matcher: RuleMatcher::new(SiteKind::Between, pat(interface), all_funcs, constraints),
            inner_alias: None,
            outer_alias: None,
            inject: Vec::<Injection>::new(),
        }
    }

    fn iface_only_pred(
        is_async: Option<bool>,
        args: Vec<ValueProperty>,
        results: Vec<ValueProperty>,
    ) -> FuncPred {
        FuncPred::new(is_async, vec![FuncScope::Interface], args, results)
    }

    fn rule_injecting(interface: &[&str], inject: Vec<Injection>) -> SpliceRule {
        SpliceRule::Before {
            matcher: RuleMatcher::new(SiteKind::Before, pat(interface), None, vec![]),
            provider_alias: None,
            inject,
        }
    }

    #[test]
    fn site_pruned_matches_builtin_on_interface_only() {
        let rule = rule_injecting(&["wasi:http/*"], vec![Injection::from_builtin("chaos-err")]);
        let skips = vec![SkipRecord {
            strategy: "chaos-err".into(),
            interface: "wasi:http/handler@0.3.0".into(),
            bound: None,
        }];
        assert!(site_pruned(&rule, "wasi:http/handler@0.3.0", &skips));
        // Same strategy, different interface: not pruned.
        assert!(!site_pruned(&rule, "wasi:io/streams@0.2.0", &skips));
        // Default mode (no skips): nothing prunes.
        assert!(!site_pruned(&rule, "wasi:http/handler@0.3.0", &[]));
    }

    #[test]
    fn site_pruned_matches_user_strategy_by_name() {
        let rule = rule_injecting(&["my:svc/*"], vec![Injection::from_path("my-strat", "/d")]);
        let skips = vec![SkipRecord {
            strategy: "my-strat".into(),
            interface: "my:svc/ops@1.0.0".into(),
            bound: None,
        }];
        assert!(site_pruned(&rule, "my:svc/ops@1.0.0", &skips));
        // A skip for a different strategy doesn't prune this rule.
        let other = vec![SkipRecord {
            strategy: "someone-else".into(),
            interface: "my:svc/ops@1.0.0".into(),
            bound: None,
        }];
        assert!(!site_pruned(&rule, "my:svc/ops@1.0.0", &other));
    }

    #[test]
    fn before_minimal_renders_kind_and_interface() {
        let r = before_rule(&["wasi:*"], None, None);
        assert_eq!(rule_description(1, &r), r#"#1 before interface="wasi:*""#);
    }

    #[test]
    fn between_minimal_renders_kind_and_interface() {
        let r = between_rule(&["wasi:*"], None, None, None);
        assert_eq!(rule_description(2, &r), r#"#2 between interface="wasi:*""#);
    }

    #[test]
    fn single_glob_drops_pipe() {
        let r = before_rule(&["wasi:*"], None, None);
        assert!(rule_description(1, &r).contains(r#"interface="wasi:*""#));
    }

    #[test]
    fn multi_glob_pipe_joined() {
        let r = before_rule(&["wasi:*", "my:srv/*"], None, None);
        assert!(rule_description(1, &r).contains(r#"interface="wasi:*|my:srv/*""#));
    }

    #[test]
    fn before_provider_rendered_when_set() {
        let r = before_rule(&["wasi:*"], Some(&["srv-*"]), None);
        assert_eq!(
            rule_description(4, &r),
            r#"#4 before interface="wasi:*" provider="srv-*""#
        );
    }

    #[test]
    fn between_inner_outer_rendered_in_fixed_order() {
        let r = between_rule(&["my:srv/*"], Some(&["auth"]), Some(&["api"]), None);
        assert_eq!(
            rule_description(2, &r),
            r#"#2 between interface="my:srv/*" inner="auth" outer="api""#
        );
    }

    #[test]
    fn between_inner_only_is_fine() {
        let r = between_rule(&["my:srv/*"], Some(&["auth"]), None, None);
        assert!(rule_description(2, &r).contains(r#"inner="auth""#));
        assert!(!rule_description(2, &r).contains("outer="));
    }

    #[test]
    fn all_funcs_async_renders_as_bare_keyword() {
        let r = before_rule(
            &["wasi:*"],
            None,
            Some(iface_only_pred(Some(true), vec![], vec![])),
        );
        assert!(rule_description(1, &r).ends_with("all-funcs=async"));
    }

    #[test]
    fn all_funcs_sync_renders_as_bare_keyword() {
        let r = before_rule(
            &["wasi:*"],
            None,
            Some(iface_only_pred(Some(false), vec![], vec![])),
        );
        assert!(rule_description(1, &r).ends_with("all-funcs=sync"));
    }

    #[test]
    fn all_funcs_default_scope_omitted() {
        let r = before_rule(
            &["*"],
            None,
            Some(iface_only_pred(Some(true), vec![], vec![])),
        );
        let desc = rule_description(1, &r);
        assert!(!desc.contains("scope="), "got: {desc}");
    }

    #[test]
    fn all_funcs_non_default_scope_pipe_joined() {
        let pred = FuncPred::new(
            None,
            vec![FuncScope::Interface, FuncScope::Resource],
            vec![],
            vec![],
        );
        let r = before_rule(&["*"], None, Some(pred));
        assert!(rule_description(1, &r).contains("scope=interface|resource"));
    }

    #[test]
    fn all_funcs_args_comma_joined() {
        let pred = iface_only_pred(
            None,
            vec![ValueProperty::Concrete, ValueProperty::Defaultable],
            vec![],
        );
        let r = before_rule(&["*"], None, Some(pred));
        assert!(rule_description(1, &r).contains("args=concrete,defaultable"));
    }

    #[test]
    fn all_funcs_results_comma_joined() {
        let pred = iface_only_pred(None, vec![], vec![ValueProperty::Concrete]);
        let r = before_rule(&["*"], None, Some(pred));
        assert!(rule_description(1, &r).contains("results=concrete"));
    }

    #[test]
    fn all_funcs_multi_property_plus_joined() {
        let pred = iface_only_pred(Some(true), vec![], vec![ValueProperty::Concrete]);
        let r = before_rule(&["*"], None, Some(pred));
        assert!(
            rule_description(1, &r).contains("all-funcs=async+results=concrete"),
            "{}",
            rule_description(1, &r)
        );
    }

    #[test]
    fn full_kitchen_sink_example_matches_design_doc() {
        let pred = FuncPred::new(
            Some(true),
            vec![FuncScope::Interface, FuncScope::Resource],
            vec![],
            vec![],
        );
        let r = before_rule(&["wasi:*", "my:*"], Some(&["srv-*"]), Some(pred));
        assert_eq!(
            rule_description(4, &r),
            r#"#4 before interface="wasi:*|my:*" provider="srv-*" all-funcs=async+scope=interface|resource"#,
        );
    }
}
