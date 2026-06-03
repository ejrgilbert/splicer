use std::path::PathBuf;

use anyhow::{Context, Result};
use cviz::canonical_edge_id;
use cviz::model::CompositionGraph;
use cviz::parse::component::parse_component;
use cviz::{Highlights, Selection};

use crate::parse::config::{parse_yaml, SpliceRule};
use crate::select::{FuncPred, FuncScope, ValueProperty};
use crate::wac::build_chain_skeletons;

#[derive(Debug, Clone)]
pub struct PreviewRequest {
    pub composition_wasm: PathBuf,
    pub rules_yaml: String,
    /// Only renders specified rule's targets
    pub only_rule: Option<usize>,
}

pub struct PreviewOutput {
    pub graph: CompositionGraph,
    pub highlights: Highlights,
    pub unmatched_rules: Vec<usize>,
}

pub fn preview(req: PreviewRequest) -> Result<PreviewOutput> {
    let PreviewRequest {
        composition_wasm,
        rules_yaml,
        only_rule,
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
    preview_with_graph(graph, &rules_yaml, only_rule)
}

pub fn preview_with_graph(
    graph: CompositionGraph,
    rules_yaml: &str,
    only_rule: Option<usize>,
) -> Result<PreviewOutput> {
    let rules = parse_yaml(rules_yaml).context("Failed to parse splice rules YAML")?;

    let (skeletons, _handled) = build_chain_skeletons(&graph);

    let mut highlights = Highlights::default();
    let mut unmatched_rules: Vec<usize> = Vec::new();

    for (idx, rule) in rules.iter().enumerate() {
        let rule_num = idx + 1;
        if let Some(only) = only_rule {
            if only != rule_num {
                continue;
            }
        }

        let desc = rule_description(rule_num, rule);
        highlights
            .register_tag(rule_num as u32, &desc)
            .with_context(|| format!("registering tag for rule {rule_num}"))?;

        let matcher = rule.matcher();
        let mut any_match = false;
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
                any_match = true;
                let provider_id = sk.chain[site.provider_idx];
                let caller_label = site.has_caller.then(|| {
                    let cid = sk.chain[site.provider_idx + 1];
                    graph.nodes[&cid].canonical_id().to_string()
                });
                let provider_label = graph.nodes[&provider_id].canonical_id();
                let edge_id =
                    canonical_edge_id(&sk.interface_name, caller_label.as_deref(), provider_label);
                highlights.mark(Selection::edge(edge_id).tag(rule_num as u32));
            }
        }
        if !any_match {
            unmatched_rules.push(rule_num);
        }
    }

    Ok(PreviewOutput {
        graph,
        highlights,
        unmatched_rules,
    })
}

/// One-line legend string for `rule`: `#N <kind> <field>=<value> ...`.
/// `inject:` is never rendered since it's the *effect*, not the *selection*.
pub fn rule_description(idx: usize, rule: &SpliceRule) -> String {
    let mut parts: Vec<String> = vec![format!("#{idx}")];
    match rule {
        SpliceRule::Before { matcher, .. } => {
            parts.push("before".to_string());
            parts.push(format!(
                "interface={}",
                quoted_globs(matcher.interface_raw())
            ));
            if let Some(p) = matcher.provider_raw() {
                parts.push(format!("provider={}", quoted_globs(p)));
            }
            if let Some(f) = matcher.all_funcs() {
                parts.push(format!("all-funcs={}", render_func_pred(f)));
            }
        }
        SpliceRule::Between { matcher, .. } => {
            parts.push("between".to_string());
            parts.push(format!(
                "interface={}",
                quoted_globs(matcher.interface_raw())
            ));

            if let Some(p) = matcher.provider_raw() {
                parts.push(format!("inner={}", quoted_globs(p)));
            }
            if let Some(c) = matcher.caller_raw() {
                parts.push(format!("outer={}", quoted_globs(c)));
            }
            if let Some(f) = matcher.all_funcs() {
                parts.push(format!("all-funcs={}", render_func_pred(f)));
            }
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
