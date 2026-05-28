//! Glob pattern matching and the predicate layer that selects a rule's
//! splice sites along a *chain* — the run of component instances wired
//! over one interface (`provider → … → consumer`), where each position
//! is a candidate [`SpliceSite`].
//!
//! [`Pattern`] holds the compiled globs; [`RuleMatcher`] pairs a
//! chain-scoped interface pattern (one per chain) with site-scoped node
//! [`Constraint`]s (one per site, checked by [`Constraint::eval`]) and
//! enumerates the matching sites. Matching is pure — the effect it feeds
//! (codegen, inject-plan mutation) lives in [`crate::wac`].

use anyhow::Context;
use cviz::model::CompositionGraph;
use globset::{Glob, GlobSet, GlobSetBuilder};

/// One match field: a set of glob patterns; matches if any one matches
/// (OR-combined). Keeps the raw strings for diagnostics.
///
/// Matching is *flat* — `*`/`?` cross `/` and `:` (interface names
/// aren't file paths), so `wasi:*` matches `wasi:http/handler@0.3.0`.
/// This is globset's default (`literal_separator` off); a unit test
/// pins it.
pub struct Pattern {
    raw: Vec<String>,
    set: GlobSet,
}

impl std::fmt::Debug for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The compiled GlobSet has no useful Debug; the raw patterns do.
        f.debug_struct("Pattern").field("raw", &self.raw).finish()
    }
}

impl Pattern {
    /// Compile the patterns into an OR-combined matcher. An invalid glob
    /// is an error (surfaced at config-validate time, not mid-generation).
    pub(crate) fn compile(raw: Vec<String>) -> anyhow::Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pat in &raw {
            let glob = Glob::new(pat).with_context(|| format!("invalid glob pattern '{pat}'"))?;
            builder.add(glob);
        }
        let set = builder.build().context("compiling glob set")?;
        Ok(Self { raw, set })
    }

    /// True iff any pattern matches `s`.
    pub(crate) fn is_match(&self, s: &str) -> bool {
        self.set.is_match(s)
    }

    /// Raw pattern strings, for diagnostics.
    pub(crate) fn raw(&self) -> &[String] {
        &self.raw
    }
}

/// A site-scoped predicate: a test on one of the edge's nodes at a
/// candidate [`SpliceSite`], evaluated by [`Constraint::eval`]. The
/// interface is chain-scoped (constant across a chain), so it lives on
/// [`RuleMatcher`] rather than here.
#[derive(Debug)]
pub(crate) enum Constraint {
    /// Provider-side node name (`before`: provider; `between`: inner).
    Provider(Pattern),
    /// Caller-side node name (`between`: outer). Fails at a boundary site.
    Caller(Pattern),
}

/// Context for evaluating a candidate site. Carries the graph and the
/// node ids of the chain the site lives in.
pub(crate) struct MatchCtx<'a> {
    graph: &'a CompositionGraph,
    chain_nodes: &'a [u32],
}

/// A candidate splice location within one chain — purely positional.
/// `provider_idx` indexes the chain's node list; the caller (consumer
/// side) is the next node. The interface/fingerprint live on the chain
/// (constant across it), so the effect reads them via `chain_idx`.
#[derive(Debug)]
pub(crate) struct SpliceSite {
    /// Index into the run's `chains`, so the effect knows which to mutate.
    pub(crate) chain_idx: usize,
    /// Position of the provider node within the chain.
    pub(crate) provider_idx: usize,
    /// False at the outermost boundary (no caller), where a `Caller`
    /// constraint can never hold.
    pub(crate) has_caller: bool,
}

fn node_name(graph: &CompositionGraph, id: u32) -> &str {
    graph.nodes[&id].display_label()
}

impl Constraint {
    /// Whether this site-scoped constraint holds at `site`. Exhaustive —
    /// a new `Constraint` variant adds an arm here and nothing else moves.
    pub(crate) fn eval(&self, site: &SpliceSite, ctx: &MatchCtx) -> bool {
        match self {
            Constraint::Provider(p) => {
                p.is_match(node_name(ctx.graph, ctx.chain_nodes[site.provider_idx]))
            }
            Constraint::Caller(p) => {
                site.has_caller
                    && p.is_match(node_name(ctx.graph, ctx.chain_nodes[site.provider_idx + 1]))
            }
        }
    }
}

/// How a rule enumerates candidate sites along a chain.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SiteKind {
    /// Every chain position, including the outermost boundary (no
    /// caller) — `before`.
    Before,
    /// Every adjacent `(provider, caller)` window — `between`.
    Between,
}

/// Compiled matcher for one rule: how to enumerate its sites, the
/// chain-scoped `interface` pattern that gates the whole chain, and the
/// site-scoped node `constraints` every matched site must satisfy (AND).
/// Built once at config-validate time.
#[derive(Debug)]
pub struct RuleMatcher {
    kind: SiteKind,
    interface: Pattern,
    constraints: Vec<Constraint>,
}

impl RuleMatcher {
    pub(crate) fn new(kind: SiteKind, interface: Pattern, constraints: Vec<Constraint>) -> Self {
        Self {
            kind,
            interface,
            constraints,
        }
    }

    /// Whether the rule's interface pattern matches `interface_name`.
    /// Interface is chain-scoped — checked once per chain, by both this
    /// (for diagnostics) and [`RuleMatcher::select`]'s gate.
    pub(crate) fn interface_matches(&self, interface_name: &str) -> bool {
        self.interface.is_match(interface_name)
    }

    /// Raw interface patterns, for diagnostics.
    pub(crate) fn interface_raw(&self) -> &[String] {
        self.interface.raw()
    }

    /// Candidate sites in this chain that satisfy the rule. The interface
    /// gates the whole chain (it's constant across one); node constraints
    /// are then checked per site.
    pub(crate) fn select(
        &self,
        chain_idx: usize,
        chain_nodes: &[u32],
        interface_name: &str,
        graph: &CompositionGraph,
    ) -> Vec<SpliceSite> {
        if !self.interface.is_match(interface_name) {
            return vec![];
        }
        let ctx = MatchCtx { graph, chain_nodes };
        enumerate_sites(self.kind, chain_idx, chain_nodes.len())
            .into_iter()
            .filter(|site| self.constraints.iter().all(|c| c.eval(site, &ctx)))
            .collect()
    }
}

/// Every candidate site in a chain of `n_nodes`, for the given
/// enumeration kind, before constraints are applied.
fn enumerate_sites(kind: SiteKind, chain_idx: usize, n_nodes: usize) -> Vec<SpliceSite> {
    let site = |provider_idx: usize, has_caller: bool| SpliceSite {
        chain_idx,
        provider_idx,
        has_caller,
    };
    match kind {
        SiteKind::Before => (0..n_nodes).map(|i| site(i, i + 1 < n_nodes)).collect(),
        SiteKind::Between => (0..n_nodes.saturating_sub(1))
            .map(|i| site(i, true))
            .collect(),
    }
}

/// Glob-*independent* "did you mean" over available interface names, run
/// per raw pattern: base-name (split on `@`) equality or prefix overlap.
/// Never re-applies the glob — a pattern that matched nothing in the
/// real pass would match nothing here too (PR #40's bug).
pub(crate) fn suggest_interfaces<'a>(raw: &[String], available: &[&'a str]) -> Vec<&'a str> {
    let mut out: Vec<&str> = vec![];
    for pat in raw {
        let pat_base = pat.split('@').next().unwrap_or(pat);
        for &avail in available {
            let avail_base = avail.split('@').next().unwrap_or(avail);
            let hit =
                avail_base == pat_base || avail.starts_with(pat.as_str()) || pat.starts_with(avail);
            if hit && !out.contains(&avail) {
                out.push(avail);
            }
        }
    }
    out
}

#[cfg(test)]
impl RuleMatcher {
    /// Raw provider patterns, if the rule constrains the provider-side
    /// node. Test-only introspection.
    pub(crate) fn provider_raw(&self) -> Option<&[String]> {
        self.constraints.iter().find_map(|c| match c {
            Constraint::Provider(p) => Some(p.raw()),
            _ => None,
        })
    }

    /// Raw caller patterns, if the rule constrains the caller-side node.
    /// Test-only introspection.
    pub(crate) fn caller_raw(&self) -> Option<&[String]> {
        self.constraints.iter().find_map(|c| match c {
            Constraint::Caller(p) => Some(p.raw()),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cviz::model::{ComponentNode, CompositionGraph};

    const IFACE: &str = "wasi:http/handler@0.3.0";

    fn pat(alts: &[&str]) -> Pattern {
        Pattern::compile(alts.iter().map(|s| s.to_string()).collect()).expect("valid glob")
    }

    // ── Pattern ──────────────────────────────────────────────────────

    #[test]
    fn exact_matches_only_itself() {
        let p = pat(&["wasi:http/handler@0.3.0"]);
        assert!(p.is_match("wasi:http/handler@0.3.0"));
        assert!(!p.is_match("wasi:http/handler@0.2.0"));
        assert!(!p.is_match("wasi:logging/log@0.1.0"));
    }

    #[test]
    fn star_matches_flatly_across_slash_and_colon() {
        // The flatness guarantee: `*` crosses `/` and `:` since interface
        // names aren't file paths.
        assert!(pat(&["wasi:*"]).is_match("wasi:http/handler@0.3.0"));
        assert!(pat(&["wasi:http/*"]).is_match("wasi:http/handler@0.3.0"));
        assert!(pat(&["*"]).is_match("anything:at/all@9.9.9"));
        assert!(pat(&["*handler*"]).is_match("wasi:http/handler@0.3.0"));
        assert!(!pat(&["my:*"]).is_match("wasi:http/handler@0.3.0"));
    }

    #[test]
    fn question_mark_matches_single_char() {
        assert!(pat(&["wasi:logging/log@0.?.0"]).is_match("wasi:logging/log@0.1.0"));
        assert!(!pat(&["wasi:logging/log@0.?.0"]).is_match("wasi:logging/log@0.10.0"));
    }

    #[test]
    fn char_class_matches() {
        let p = pat(&["wasi:logging/log@0.[0-9].0"]);
        assert!(p.is_match("wasi:logging/log@0.1.0"));
        assert!(p.is_match("wasi:logging/log@0.9.0"));
        assert!(!p.is_match("wasi:logging/log@0.a.0"));
    }

    #[test]
    fn pattern_list_is_or() {
        let p = pat(&["wasi:*", "my:srv/*"]);
        assert!(p.is_match("wasi:http/handler@0.3.0"));
        assert!(p.is_match("my:srv/api@1.0.0"));
        assert!(!p.is_match("other:pkg/iface@1.0.0"));
    }

    #[test]
    fn invalid_glob_is_an_error() {
        // Unterminated character class.
        assert!(Pattern::compile(vec!["wasi:[".to_string()]).is_err());
    }

    // ── select / enumeration ─────────────────────────────────────────

    /// Compile a matcher (`interface` + node `constraints`) and select
    /// its sites on a standard three-node chain `node-0 → node-1 →
    /// node-2` over [`IFACE`]. `select` takes the chain explicitly, so
    /// the graph only needs the nodes to exist for name lookups.
    fn selected(
        kind: SiteKind,
        interface: &[&str],
        constraints: Vec<Constraint>,
    ) -> Vec<SpliceSite> {
        let mut graph = CompositionGraph::new();
        for i in 0..3u32 {
            graph.add_node(i, ComponentNode::new(format!("$node-{i}"), i, i));
        }
        RuleMatcher::new(kind, pat(interface), constraints).select(0, &[0u32, 1, 2], IFACE, &graph)
    }

    #[test]
    fn before_enumerates_every_position_including_boundary() {
        let sites = selected(SiteKind::Before, &["wasi:*"], vec![]);
        // Every position 0,1,2 — including position 2, the boundary
        // (its consumer is external).
        assert_eq!(sites.len(), 3);
        assert!(sites.iter().any(|s| s.provider_idx == 2 && !s.has_caller));
        assert_eq!(sites.iter().filter(|s| s.has_caller).count(), 2);
    }

    #[test]
    fn between_enumerates_windows_only() {
        let sites = selected(SiteKind::Between, &["wasi:*"], vec![]);
        // Two adjacent windows; never a boundary site.
        assert_eq!(sites.len(), 2);
        assert!(sites.iter().all(|s| s.has_caller));
    }

    #[test]
    fn interface_pattern_gates_the_whole_chain() {
        // A non-matching interface yields no sites.
        assert!(selected(SiteKind::Before, &["my:*"], vec![]).is_empty());
    }

    #[test]
    fn interface_matches_reflects_the_pattern() {
        let m = RuleMatcher::new(SiteKind::Before, pat(&["wasi:*"]), vec![]);
        assert!(m.interface_matches(IFACE));
        assert!(!m.interface_matches("my:srv/api@1.0.0"));
    }

    #[test]
    fn provider_constraint_filters_by_node_name() {
        let sites = selected(
            SiteKind::Before,
            &["wasi:*"],
            vec![Constraint::Provider(pat(&["node-1"]))],
        );
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].provider_idx, 1);
    }

    #[test]
    fn caller_constraint_filters_and_needs_a_caller() {
        // outer (caller) = node-2 over the window (provider node-1, caller node-2).
        let sites = selected(
            SiteKind::Between,
            &["wasi:*"],
            vec![Constraint::Caller(pat(&["node-2"]))],
        );
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].provider_idx, 1);
    }

    #[test]
    fn provider_glob_fans_out_across_positions() {
        // `node-*` matches all three; `before` enumerates every position.
        let sites = selected(
            SiteKind::Before,
            &["wasi:*"],
            vec![Constraint::Provider(pat(&["node-*"]))],
        );
        assert_eq!(sites.len(), 3);
    }

    // ── suggest_interfaces ───────────────────────────────────────────

    #[test]
    fn suggest_uses_base_name_and_prefix_not_the_glob() {
        let available = [
            "wasi:http/handler@0.3.0",
            "wasi:logging/log@0.1.0",
            "my:srv/api@1.0.0",
        ];
        // Version skew: same base name, different `@version`.
        let got = suggest_interfaces(&["wasi:http/handler@9.9.9".to_string()], &available);
        assert_eq!(got, vec!["wasi:http/handler@0.3.0"]);
        // A glob that matched nothing in the real pass is NOT re-applied:
        // "wasi:*" doesn't prefix-match any available name literally.
        let got = suggest_interfaces(&["wasi:does-not/exist@0.1.0".to_string()], &available);
        assert!(got.is_empty());
    }
}
