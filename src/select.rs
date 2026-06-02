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
use cviz::model::{
    CompositionGraph, FuncSignature, InterfaceType, TypeArena, ValueType, ValueTypeId,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::str::FromStr;

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

#[derive(Debug)]
pub struct RuleMatcher {
    kind: SiteKind,
    interface: Pattern,
    all_funcs: Option<FuncPred>,
    constraints: Vec<Constraint>,
}

impl RuleMatcher {
    pub(crate) fn new(
        kind: SiteKind,
        interface: Pattern,
        all_funcs: Option<FuncPred>,
        constraints: Vec<Constraint>,
    ) -> Self {
        Self {
            kind,
            interface,
            all_funcs,
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
    /// and `all-funcs:` predicate gate the whole chain (both constant
    /// across one); node constraints are then checked per site.
    /// Error if configuration has `all-funcs` but the interface func
    /// sig is unknown.
    pub(crate) fn select(
        &self,
        chain_idx: usize,
        chain_nodes: &[u32],
        interface_name: &str,
        interface_type: Option<&InterfaceType>,
        graph: &CompositionGraph,
    ) -> anyhow::Result<Vec<SpliceSite>> {
        if !self.interface.is_match(interface_name) {
            return Ok(vec![]);
        }
        if let Some(pred) = &self.all_funcs {
            let Some(ty) = interface_type else {
                anyhow::bail!(
                    "interface '{interface_name}' matched a rule's `all-funcs:` predicate, \
                     but its type signature could not be parsed — the match is undecidable"
                );
            };
            if !pred.holds(ty, &graph.arena) {
                return Ok(vec![]);
            }
        }
        let ctx = MatchCtx { graph, chain_nodes };
        Ok(enumerate_sites(self.kind, chain_idx, chain_nodes.len())
            .into_iter()
            .filter(|site| self.constraints.iter().all(|c| c.eval(site, &ctx)))
            .collect())
    }
}

#[derive(Debug)]
pub(crate) struct FuncPred {
    pub(crate) is_async: Option<bool>,
    pub(crate) scopes: Vec<FuncScope>,
    pub(crate) args: Vec<ValueProperty>,
    pub(crate) results: Vec<ValueProperty>,
}

/// Which WIT-tree surface a function lives on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FuncScope {
    Interface,
    /// (e.g. `[constructor]*`, `[method]*`, `[static]*`)
    Resource,
}
impl FromStr for FuncScope {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "interface" => Ok(Self::Interface),
            "resource" => Ok(Self::Resource),
            _ => Err(()),
        }
    }
}

/// A structural property of a single WIT value type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ValueProperty {
    Concrete,
    Defaultable,
}
impl FromStr for ValueProperty {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "concrete" => Ok(Self::Concrete),
            "defaultable" => Ok(Self::Defaultable),
            _ => Err(()),
        }
    }
}

impl FuncPred {
    pub(crate) fn new(
        is_async: Option<bool>,
        scopes: Vec<FuncScope>,
        args: Vec<ValueProperty>,
        results: Vec<ValueProperty>,
    ) -> Self {
        Self {
            is_async,
            scopes,
            args,
            results,
        }
    }

    /// True iff `ty` has at least one function and **every** function is
    /// in one of the configured `scopes` and satisfies this predicate.
    fn holds(&self, ty: &InterfaceType, arena: &TypeArena) -> bool {
        let mut iter = funcs(ty);
        let Some((first_name, first)) = iter.next() else {
            // Empty interfaces have nothing to interpose on.
            return false;
        };
        let in_scope_and_passes = |name: &str, f: &FuncSignature| {
            self.scopes.iter().any(|s| matches_scope(name, *s)) && self.holds_one(f, arena)
        };
        in_scope_and_passes(first_name, first) && iter.all(|(name, f)| in_scope_and_passes(name, f))
    }

    /// True iff the function satisfies the predicate.
    fn holds_one(&self, f: &FuncSignature, arena: &TypeArena) -> bool {
        if self.is_async.is_some_and(|want| f.is_async != want) {
            return false;
        }
        self.args
            .iter()
            .all(|p| f.params.iter().all(|id| p.holds(*id, arena)))
            && self
                .results
                .iter()
                .all(|p| f.results.iter().all(|id| p.holds(*id, arena)))
    }
}

impl ValueProperty {
    fn holds(&self, id: ValueTypeId, arena: &TypeArena) -> bool {
        match self {
            ValueProperty::Concrete => is_concrete(id, arena),
            ValueProperty::Defaultable => is_defaultable(id, arena),
        }
    }
}

/// Every function of an interface.
fn funcs(ty: &InterfaceType) -> impl Iterator<Item = (&str, &FuncSignature)> {
    let (instance, single) = match ty {
        InterfaceType::Instance(inst) => (
            Some(inst.functions.iter().map(|(n, f)| (n.as_str(), f))),
            None,
        ),
        InterfaceType::Func(f) => (None, Some(("", f))),
    };
    instance.into_iter().flatten().chain(single)
}

fn matches_scope(name: &str, scope: FuncScope) -> bool {
    match scope {
        FuncScope::Interface => !name.starts_with('['),
        FuncScope::Resource => todo!(
            "scope: resource — splicer's adapter codegen doesn't yet \
             handle resource constructor/method/static surfaces. \
             Implement here (`name.starts_with('[')`) and extend the \
             wrapper-component logic in `src/adapter/abi/emit.rs` to \
             carry resource handles across the wrap."
        ),
    }
}

/// True iff `id` is directly-representable data — no resource/async
/// handle and no error-context anywhere within it.
fn is_concrete(id: ValueTypeId, arena: &TypeArena) -> bool {
    match arena.lookup_val(id) {
        // Abstract: opaque, runtime/host-backed values the guest can't
        // reconstruct from self-describing bytes.
        ValueType::Resource(_) | ValueType::AsyncHandle | ValueType::ErrorContext => false,
        // Self-describing scalars and name-only aggregates.
        ValueType::Bool
        | ValueType::S8
        | ValueType::U8
        | ValueType::S16
        | ValueType::U16
        | ValueType::S32
        | ValueType::U32
        | ValueType::S64
        | ValueType::U64
        | ValueType::F32
        | ValueType::F64
        | ValueType::Char
        | ValueType::String
        | ValueType::Enum(_)
        | ValueType::Flags(_) => true,
        // Containers: concrete iff their contents are.
        ValueType::List(t) | ValueType::Option(t) | ValueType::FixedSizeList(t, _) => {
            is_concrete(*t, arena)
        }
        ValueType::Tuple(ts) => ts.iter().all(|t| is_concrete(*t, arena)),
        ValueType::Record(fs) => fs.iter().all(|(_, t)| is_concrete(*t, arena)),
        ValueType::Variant(cs) => cs
            .iter()
            .all(|(_, t)| t.is_none_or(|t| is_concrete(t, arena))),
        ValueType::Result { ok, err } => [ok, err]
            .into_iter()
            .flatten()
            .all(|t| is_concrete(*t, arena)),
        ValueType::Map(k, v) => is_concrete(*k, arena) && is_concrete(*v, arena),
    }
}

/// True iff an unambiguous default value can be synthesized.
fn is_defaultable(id: ValueTypeId, arena: &TypeArena) -> bool {
    match arena.lookup_val(id) {
        // Have a canonical zero/empty default.
        ValueType::Bool
        | ValueType::S8
        | ValueType::U8
        | ValueType::S16
        | ValueType::U16
        | ValueType::S32
        | ValueType::U32
        | ValueType::S64
        | ValueType::U64
        | ValueType::F32
        | ValueType::F64
        | ValueType::Char
        | ValueType::String
        | ValueType::Option(_)
        | ValueType::List(_)
        | ValueType::Map(_, _)
        | ValueType::Flags(_) => true,
        // Defaultable iff their members are.
        ValueType::FixedSizeList(t, _) => is_defaultable(*t, arena),
        ValueType::Tuple(ts) => ts.iter().all(|t| is_defaultable(*t, arena)),
        ValueType::Record(fs) => fs.iter().all(|(_, t)| is_defaultable(*t, arena)),
        // No canonical case to pick.
        ValueType::Variant(_) | ValueType::Enum(_) | ValueType::Result { .. } => false,
        // Abstract: no synthesizable default.
        ValueType::Resource(_) | ValueType::AsyncHandle | ValueType::ErrorContext => false,
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
    pub(crate) fn all_funcs(&self) -> Option<&FuncPred> {
        self.all_funcs.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cviz::model::{ComponentNode, CompositionGraph, InstanceInterface};

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
        RuleMatcher::new(kind, pat(interface), None, constraints)
            .select(0, &[0u32, 1, 2], IFACE, None, &graph)
            .expect("select without all-funcs is infallible")
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
        let m = RuleMatcher::new(SiteKind::Before, pat(&["wasi:*"]), None, vec![]);
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

    // ── all-funcs: property helpers ──────────────────────────────────

    /// Intern a function signature into `arena`. Params/results are
    /// top-level value types (nest by interning inner ids first).
    fn func(
        arena: &mut TypeArena,
        is_async: bool,
        params: Vec<ValueType>,
        results: Vec<ValueType>,
    ) -> FuncSignature {
        FuncSignature {
            is_async,
            param_names: vec![],
            params: params.into_iter().map(|t| arena.intern_val(t)).collect(),
            results: results.into_iter().map(|t| arena.intern_val(t)).collect(),
        }
    }

    fn instance(funcs: Vec<(&str, FuncSignature)>) -> InterfaceType {
        InterfaceType::Instance(InstanceInterface {
            functions: funcs.into_iter().map(|(n, f)| (n.to_string(), f)).collect(),
            type_exports: Default::default(),
        })
    }

    fn one_func_instance(
        a: &mut TypeArena,
        is_async: bool,
        args: Vec<ValueType>,
        results: Vec<ValueType>,
    ) -> InterfaceType {
        instance(vec![("h", func(a, is_async, args, results))])
    }

    #[test]
    fn concrete_rejects_handles_and_error_context_in_every_container() {
        let mut a = TypeArena::default();
        for abstract_ty in [
            ValueType::Resource("request".into()),
            ValueType::AsyncHandle,
            ValueType::ErrorContext,
        ] {
            let id = a.intern_val(abstract_ty);
            assert!(
                !is_concrete(id, &a),
                "bare abstract type must not be concrete"
            );
            // Each container wrapping it inherits the abstractness.
            for wrapped in [
                ValueType::List(id),
                ValueType::Option(id),
                ValueType::FixedSizeList(id, 3),
                ValueType::Tuple(vec![id]),
                ValueType::Record(vec![("f".into(), id)]),
                ValueType::Variant(vec![("c".into(), Some(id))]),
                ValueType::Result {
                    ok: Some(id),
                    err: None,
                },
                ValueType::Map(id, id),
            ] {
                let w = a.intern_val(wrapped);
                assert!(
                    !is_concrete(w, &a),
                    "container of abstract type must not be concrete"
                );
            }
        }
    }

    #[test]
    fn concrete_accepts_handle_free_data() {
        let mut a = TypeArena::default();
        let u = a.intern_val(ValueType::U32);
        let s = a.intern_val(ValueType::String);
        let list = a.intern_val(ValueType::List(s));
        let rec = a.intern_val(ValueType::Record(vec![
            ("n".into(), u),
            ("xs".into(), list),
        ]));
        assert!(is_concrete(rec, &a));
        let en = a.intern_val(ValueType::Enum(vec!["a".into(), "b".into()]));
        assert!(is_concrete(en, &a));
        let fl = a.intern_val(ValueType::Flags(vec!["x".into()]));
        assert!(is_concrete(fl, &a));
    }

    #[test]
    fn defaultable_classification() {
        let mut a = TypeArena::default();
        let u = a.intern_val(ValueType::U32);
        // result / variant / enum: no canonical default.
        let res = a.intern_val(ValueType::Result {
            ok: Some(u),
            err: None,
        });
        assert!(!is_defaultable(res, &a));
        let var = a.intern_val(ValueType::Variant(vec![("c".into(), None)]));
        assert!(!is_defaultable(var, &a));
        let en = a.intern_val(ValueType::Enum(vec!["a".into()]));
        assert!(!is_defaultable(en, &a));
        // option<resource>: defaults to `none` regardless of element.
        let r = a.intern_val(ValueType::Resource("x".into()));
        let opt = a.intern_val(ValueType::Option(r));
        assert!(is_defaultable(opt, &a));
        // record: defaultable iff all members are.
        let ok_rec = a.intern_val(ValueType::Record(vec![("a".into(), u), ("b".into(), opt)]));
        assert!(is_defaultable(ok_rec, &a));
        let bad_rec = a.intern_val(ValueType::Record(vec![("a".into(), u), ("b".into(), res)]));
        assert!(!is_defaultable(bad_rec, &a));
    }

    #[test]
    fn concrete_and_defaultable_are_independent() {
        let mut a = TypeArena::default();
        let u = a.intern_val(ValueType::U32);
        let s = a.intern_val(ValueType::String);
        // result<u32, string>: concrete, not defaultable.
        let res = a.intern_val(ValueType::Result {
            ok: Some(u),
            err: Some(s),
        });
        assert!(is_concrete(res, &a));
        assert!(!is_defaultable(res, &a));
        // option<resource>: defaultable, not concrete.
        let r = a.intern_val(ValueType::Resource("h".into()));
        let opt = a.intern_val(ValueType::Option(r));
        assert!(!is_concrete(opt, &a));
        assert!(is_defaultable(opt, &a));
    }

    // ── all-funcs: select gate ───────────────────────────────────────

    /// A graph with a single node, whose `arena` holds the interned
    /// value types an `all-funcs` predicate reads.
    fn one_node_graph() -> CompositionGraph {
        let mut graph = CompositionGraph::new();
        graph.add_node(0, ComponentNode::new("$node-0".into(), 0, 0));
        graph
    }

    /// Run a `before` matcher (`interface` glob + `all-funcs`) over
    /// [`IFACE`] on `graph`'s one-node chain. `interface_type` is the
    /// structured type the predicate inspects (`None` ⇒ undecidable).
    fn run_pred(
        interface: &[&str],
        all_funcs: FuncPred,
        interface_type: Option<&InterfaceType>,
        graph: &CompositionGraph,
    ) -> anyhow::Result<Vec<SpliceSite>> {
        RuleMatcher::new(SiteKind::Before, pat(interface), Some(all_funcs), vec![]).select(
            0,
            &[0u32],
            IFACE,
            interface_type,
            graph,
        )
    }

    /// `run_pred` for the common case: a matching `interface`, with the
    /// type built by `build` into the same arena `select` reads (so the
    /// `ValueTypeId`s stay consistent).
    fn select_with_pred(
        all_funcs: FuncPred,
        build: impl FnOnce(&mut TypeArena) -> InterfaceType,
    ) -> anyhow::Result<Vec<SpliceSite>> {
        let mut graph = one_node_graph();
        let ty = build(&mut graph.arena);
        run_pred(&["wasi:*"], all_funcs, Some(&ty), &graph)
    }

    /// Default-scope predicate (`[Interface]`) — matches what
    /// `compile_func_pred` produces when `scope:` is omitted.
    fn iface_pred(
        is_async: Option<bool>,
        args: Vec<ValueProperty>,
        results: Vec<ValueProperty>,
    ) -> FuncPred {
        FuncPred::new(is_async, vec![FuncScope::Interface], args, results)
    }

    #[test]
    fn all_funcs_async_selects_all_async_rejects_a_sync() {
        let all_async = select_with_pred(iface_pred(Some(true), vec![], vec![]), |a| {
            instance(vec![
                ("h", func(a, true, vec![], vec![ValueType::U32])),
                ("g", func(a, true, vec![ValueType::String], vec![])),
            ])
        })
        .expect("decidable");
        assert_eq!(all_async.len(), 1);

        let has_sync = select_with_pred(iface_pred(Some(true), vec![], vec![]), |a| {
            instance(vec![
                ("h", func(a, true, vec![], vec![])),
                ("g", func(a, false, vec![], vec![])),
            ])
        })
        .expect("decidable");
        assert!(has_sync.is_empty());
    }

    #[test]
    fn all_funcs_results_concrete_rejects_resource_accepts_primitive() {
        let pred = || iface_pred(None, vec![], vec![ValueProperty::Concrete]);
        let resource_result = select_with_pred(pred(), |a| {
            one_func_instance(a, true, vec![], vec![ValueType::Resource("r".into())])
        })
        .expect("decidable");
        assert!(resource_result.is_empty());

        let primitive_result = select_with_pred(pred(), |a| {
            one_func_instance(a, true, vec![], vec![ValueType::U32])
        })
        .expect("decidable");
        assert_eq!(primitive_result.len(), 1);
    }

    #[test]
    fn all_funcs_property_list_ands() {
        let pred = || {
            iface_pred(
                None,
                vec![],
                vec![ValueProperty::Concrete, ValueProperty::Defaultable],
            )
        };
        // result<u32,string> is concrete but NOT defaultable → AND fails.
        let concrete_not_defaultable = select_with_pred(pred(), |a| {
            let u = a.intern_val(ValueType::U32);
            let s = a.intern_val(ValueType::String);
            let res = a.intern_val(ValueType::Result {
                ok: Some(u),
                err: Some(s),
            });
            InterfaceType::Func(FuncSignature {
                is_async: true,
                param_names: vec![],
                params: vec![],
                results: vec![res],
            })
        })
        .expect("decidable");
        assert!(concrete_not_defaultable.is_empty());

        // A bare u32 satisfies both.
        let both = select_with_pred(pred(), |a| {
            one_func_instance(a, true, vec![], vec![ValueType::U32])
        })
        .expect("decidable");
        assert_eq!(both.len(), 1);
    }

    #[test]
    fn all_funcs_undecidable_errors_naming_the_interface() {
        let graph = one_node_graph();
        let err = run_pred(
            &["wasi:*"],
            iface_pred(Some(true), vec![], vec![]),
            None,
            &graph,
        )
        .expect_err("missing type under all-funcs is undecidable");
        let msg = err.to_string();
        assert!(
            msg.contains(IFACE),
            "error should name the interface; got: {msg}"
        );
        assert!(msg.contains("undecidable"), "got: {msg}");
    }

    #[test]
    fn all_funcs_missing_type_is_fine_when_interface_excluded() {
        // The undecidable error only fires for a chain that passed the
        // interface glob; a non-matching interface short-circuits first.
        let graph = one_node_graph();
        let sites = run_pred(
            &["my:*"],
            iface_pred(Some(true), vec![], vec![]),
            None,
            &graph,
        )
        .expect("excluded interface never reaches the predicate");
        assert!(sites.is_empty());
    }

    #[test]
    fn all_funcs_rejects_empty_interface() {
        // An interface with no functions has nothing to interpose on, so
        // `all-funcs:` must reject it (overrides the vacuous ∀). This is
        // what stops a broad glob from accidentally matching an
        // inline-resource types interface.
        let sites = select_with_pred(
            iface_pred(None, vec![], vec![ValueProperty::Concrete]),
            |_| instance(vec![]),
        )
        .expect("decidable");
        assert!(sites.is_empty(), "function-free interface must not match");
    }

    // ── scope: axis ──────────────────────────────────────────────────

    /// Resource-surface name: by convention, anything starting with
    /// `[constructor]` / `[method]` / `[static]`.
    const RES_CTOR: &str = "[constructor]r";
    const RES_METHOD: &str = "[method]r.f";
    const RES_STATIC: &str = "[static]r.g";

    #[test]
    fn default_scope_filters_out_resource_only_interface() {
        // A types-only interface whose `functions` map carries only
        // resource surfaces — default `scope: interface` post-filters
        // them all away, so the empty-set rejection fires.
        let sites = select_with_pred(iface_pred(None, vec![], vec![]), |a| {
            instance(vec![
                (RES_CTOR, func(a, false, vec![], vec![])),
                (RES_METHOD, func(a, false, vec![], vec![])),
                (RES_STATIC, func(a, false, vec![], vec![])),
            ])
        })
        .expect("decidable");
        assert!(
            sites.is_empty(),
            "resource-only interface must be filtered out by default scope"
        );
    }

    #[test]
    fn default_scope_rejects_mixed_interface() {
        // Splicer interposes on whole interfaces — a mixed interface
        // (free fn + inline resource surface) under `scope: interface`
        // is rejected because the resource surface is out of scope and
        // there's no way to interpose on only the free function.
        let sites = select_with_pred(iface_pred(Some(true), vec![], vec![]), |a| {
            instance(vec![
                ("h", func(a, true, vec![], vec![ValueType::U32])),
                (RES_CTOR, func(a, false, vec![], vec![])),
            ])
        })
        .expect("decidable");
        assert!(
            sites.is_empty(),
            "mixed interface must be rejected by default scope"
        );
    }

    #[test]
    #[should_panic(expected = "scope: resource")]
    fn scope_resource_is_a_todo_for_now() {
        // `scope: resource` is the forward-compat seam: splicer's
        // adapter codegen can't yet wrap resource surfaces, so
        // `matches_scope` for `FuncScope::Resource` panics with a
        // pointer to the implementation site. Replace this test (and
        // lift the `todo!()`) when resource-surface codegen lands.
        let pred = FuncPred::new(None, vec![FuncScope::Resource], vec![], vec![]);
        let _ = select_with_pred(pred, |a| {
            instance(vec![(RES_CTOR, func(a, false, vec![], vec![]))])
        });
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
