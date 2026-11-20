use crate::select::{
    Constraint, FuncPred, FuncScope, Pattern, RuleMatcher, SiteKind, ValueProperty,
};
use anyhow::bail;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

/// Parse a YAML splice configuration string into a list of validated
/// [`SpliceRule`]s ready to pass to [`crate::lowlevel::generate_wac`].
pub fn parse_yaml(yaml_str: &str) -> anyhow::Result<Vec<SpliceRule>> {
    let config: ConfigFile = serde_yaml::from_str(yaml_str)?;
    config.validate()?;
    config.into_splice_rules()
}

/// --- YAML config structures ---
#[derive(Debug, Deserialize)]
pub struct ConfigFile {
    pub version: u32,
    pub rules: Vec<YamlRule>,
}

#[derive(Debug, Deserialize)]
pub struct YamlRule {
    before: Option<YamlStrategyBefore>,
    between: Option<YamlStrategyBetween>,
    on_node: Option<YamlStrategyOnNode>,
    on_subgraph: Option<YamlStrategyOnSubgraph>,
    inject: Vec<YamlInjection>,
}

#[derive(Debug, Deserialize)]
pub struct YamlInjection {
    pub name: Option<String>,
    pub path: Option<String>,
    pub builtin: Option<BuiltinSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum BuiltinSpec {
    Name(String),
    Detailed {
        name: String,
        alias: Option<String>,
        #[serde(default)]
        config: BTreeMap<String, serde_yaml::Value>,
    },
}

impl BuiltinSpec {
    fn builtin_name(&self) -> &str {
        match self {
            BuiltinSpec::Name(n) => n,
            BuiltinSpec::Detailed { name, .. } => name,
        }
    }
    fn alias(&self) -> Option<&str> {
        match self {
            BuiltinSpec::Name(_) => None,
            BuiltinSpec::Detailed { alias, .. } => alias.as_deref(),
        }
    }
    /// Empty for the short-form `builtin: <name>` shape.
    fn config(&self) -> &BTreeMap<String, serde_yaml::Value> {
        static EMPTY: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        match self {
            BuiltinSpec::Name(_) => &EMPTY,
            BuiltinSpec::Detailed { config, .. } => config,
        }
    }
}

/// A YAML field accepting either a single scalar or a list of them.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn as_slice(&self) -> &[T] {
        match self {
            OneOrMany::One(s) => std::slice::from_ref(s),
            OneOrMany::Many(v) => v,
        }
    }
    fn into_vec(self) -> Vec<T> {
        match self {
            OneOrMany::One(s) => vec![s],
            OneOrMany::Many(v) => v,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct YamlStrategyBefore {
    interface: OneOrMany<String>,
    provider: Option<YamlProviderOpt>,
    #[serde(rename = "all-funcs")]
    all_funcs: Option<YamlFuncPred>,
}

#[derive(Debug, Deserialize)]
pub struct YamlStrategyBetween {
    interface: OneOrMany<String>,
    inner: Option<YamlProviderOpt>,
    outer: Option<YamlProviderOpt>,
    #[serde(rename = "all-funcs")]
    all_funcs: Option<YamlFuncPred>,
}

/// `on_node` selector: every edge touching the named node in a given
/// direction. Desugars at parse time to one or two `before`/`between`
/// `SpliceRule`s; never reaches downstream code as its own variant.
#[derive(Debug, Deserialize)]
pub struct YamlStrategyOnNode {
    name: String,
    #[serde(default)]
    direction: Direction,
    alias: Option<String>,
    interface: Option<OneOrMany<String>>,
    #[serde(rename = "all-funcs")]
    all_funcs: Option<YamlFuncPred>,
}

/// `on_subgraph` selector: every boundary edge of a node set.
#[derive(Debug, Deserialize)]
pub struct YamlStrategyOnSubgraph {
    nodes: Vec<String>,
    #[serde(default)]
    direction: Direction,
    interface: Option<OneOrMany<String>>,
    #[serde(rename = "all-funcs")]
    all_funcs: Option<YamlFuncPred>,
}

/// Which side of the edge the named node sits on.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Node is the provider (exporter) side.
    Inbound,
    /// Node is the caller (importer) side.
    Outbound,
    /// Union: splicer wraps every edge touching the node.
    #[default]
    Both,
}
impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Direction::Inbound => "inbound",
            Direction::Outbound => "outbound",
            Direction::Both => "both",
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct YamlFuncPred {
    #[serde(rename = "async")]
    is_async: Option<bool>,
    scope: Option<OneOrMany<String>>,
    args: Option<OneOrMany<String>>,
    results: Option<OneOrMany<String>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct YamlProviderOpt {
    name: Option<OneOrMany<String>>,
    // Alias the matched provider to this name in the generated wac
    alias: Option<String>,
}

/// Stamped on an [`Injection`] by `add_to_inject_plan` when it resolves
/// to a tier-1 adapter. Not part of the YAML config.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AdapterInjectionInfo {
    pub adapter_path: String,
    pub matched_hook_interfaces: Vec<String>,
}

/// Describes a built edge shim component that sits between the consumer (holding T' handles)
/// and a raw collateral interface.
#[derive(Clone, Debug)]
pub struct EdgeShimSpec {
    /// Qualified collateral interface (e.g. `"my:service/shapes-viewer"`).
    pub collateral_iface: String,
    /// Qualified raw sibling types interface (e.g. `"my:service/shapes-handles-types"`).
    pub raw_types_iface: String,
    /// T' sibling types export key (e.g. `"splicer:wrapper/shapes-handles-types@0.0.0"`).
    pub t_prime_types_export: String,
    /// WAC export key for the edge shim (e.g. `"splicer:edge-shim/shapes-viewer@0.0.0"`).
    pub shim_export_key: String,
    /// Path to the built edge shim component wasm.
    pub shim_path: String,
}

/// A middleware to inject at a splice point. From the YAML `inject`
/// list, or programmatically via [`Injection::from_path`].
#[derive(Clone, Debug, Deserialize)]
pub struct Injection {
    /// WAC variable name for this injection.
    pub name: String,
    /// Path to the middleware `.wasm` on disk. Required for user-form;
    /// builtin / tier-3-4 entries start `None` and get stamped during
    /// materialization.
    pub path: Option<String>,
    /// Splicer-shipped builtin name.
    #[serde(skip)]
    pub builtin: Option<String>,
    #[serde(skip)]
    pub builtin_config: BTreeMap<String, toml::Value>,
    #[serde(skip)]
    pub(crate) config_as_wave: Option<BTreeMap<String, String>>,
    #[serde(skip)]
    pub(crate) config_provider_path: Option<String>,
    #[serde(skip)]
    pub(crate) adapter_info: Option<AdapterInjectionInfo>,
    #[serde(skip)]
    pub(crate) tier: Option<builtin_protocol::Tier>,
    /// Interfaces this injection's wrapper _exports_ that carry resource
    /// types. Stamped during tier-3/4 materialization; chain routing wires
    /// the consumer's imports of these siblings through the wrapper so it
    /// sees one consistent resource identity. Empty for non-wrapping tiers.
    #[serde(skip)]
    pub(crate) resource_bearing_exports: Vec<String>,
    /// T' mode: (consumer_import_key, t_prime_export_key) cross-name wires
    /// for WAC routing. Stamped from TargetWit during materialization.
    /// Empty for non-T' wrappers.
    #[serde(skip)]
    pub(crate) t_prime_redirects: Vec<(String, String)>,
    /// One entry per collateral interface that needs a T' handle unwrap edge shim.
    /// Stamped during materialization; empty until then.
    #[serde(skip)]
    pub(crate) edge_shim_specs: Vec<EdgeShimSpec>,
}

impl PartialEq for Injection {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.path == other.path && self.builtin == other.builtin
    }
}

impl Eq for Injection {}

impl std::hash::Hash for Injection {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.name.hash(h);
        self.path.hash(h);
        self.builtin.hash(h);
    }
}

impl PartialOrd for Injection {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Injection {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.name, &self.path, &self.builtin).cmp(&(&other.name, &other.path, &other.builtin))
    }
}

impl Injection {
    /// Construct an [`Injection`] for a middleware that should be
    /// loaded from a `.wasm` file at `path`.
    pub fn from_path(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: Some(path.into()),
            builtin: None,
            builtin_config: BTreeMap::new(),
            config_as_wave: None,
            config_provider_path: None,
            adapter_info: None,
            tier: None,
            resource_bearing_exports: Vec::new(),
            t_prime_redirects: Vec::new(),
            edge_shim_specs: Vec::new(),
        }
    }

    /// Construct an [`Injection`] referencing a splicer-shipped builtin
    /// by name. The splice pipeline materializes the embedded bytes
    /// before contract validation runs.
    pub fn from_builtin(builtin: impl Into<String>) -> Self {
        let name = builtin.into();
        Self {
            name: name.clone(),
            path: None,
            builtin: Some(name),
            builtin_config: BTreeMap::new(),
            config_as_wave: None,
            config_provider_path: None,
            adapter_info: None,
            tier: None,
            resource_bearing_exports: Vec::new(),
            t_prime_redirects: Vec::new(),
            edge_shim_specs: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub enum SpliceRule {
    /// Inject middleware before a provider on an interface edge.
    Before {
        /// Compiled interface + provider-name patterns.
        matcher: RuleMatcher,
        /// Optional alias for the matched provider in the generated WAC.
        provider_alias: Option<String>,
        /// Middleware to inject (in order).
        inject: Vec<Injection>,
    },
    /// Inject middleware between two components on an interface edge.
    Between {
        /// Compiled interface + inner/outer-name patterns.
        matcher: RuleMatcher,
        /// Optional alias for the inner (provider-side) component.
        inner_alias: Option<String>,
        /// Optional alias for the outer (caller-side) component.
        outer_alias: Option<String>,
        /// Middleware to inject (in order).
        inject: Vec<Injection>,
    },
    /// Unresolved `on_node` selector.
    OnNode {
        /// Compiled node-name pattern.
        name: Pattern,
        direction: Direction,
        /// Compiled interface pattern (defaults to `"*"`).
        interface: Pattern,
        /// Compiled `all-funcs:` predicate, threaded onto each emitted rule.
        all_funcs: Option<FuncPred>,
        /// Optional WAC-var rename, propagated to both emitted rules.
        alias: Option<String>,
        /// Middleware to inject (in order).
        inject: Vec<Injection>,
    },
    /// Unresolved `on_subgraph` selector.
    OnSubgraph {
        /// Literal node names forming the subgraph.
        nodes: Vec<String>,
        direction: Direction,
        /// Compiled interface filter (defaults to `"*"`).
        interface: Pattern,
        /// Compiled `all-funcs:` predicate, threaded onto each emitted rule.
        all_funcs: Option<FuncPred>,
        /// Middleware to inject (in order).
        inject: Vec<Injection>,
    },
}

impl SpliceRule {
    /// The compiled matcher for this rule. Panics on the unresolved
    /// variants (`OnNode`, `OnSubgraph`); those must be expanded first.
    pub(crate) fn matcher(&self) -> &RuleMatcher {
        match self {
            SpliceRule::Before { matcher, .. } | SpliceRule::Between { matcher, .. } => matcher,
            SpliceRule::OnNode { .. } | SpliceRule::OnSubgraph { .. } => {
                panic!("unresolved variant: call resolve_rules() before .matcher()")
            }
        }
    }

    /// The injection list for this rule.
    pub fn inject(&self) -> &[Injection] {
        match self {
            SpliceRule::Before { inject, .. }
            | SpliceRule::Between { inject, .. }
            | SpliceRule::OnNode { inject, .. }
            | SpliceRule::OnSubgraph { inject, .. } => inject,
        }
    }

    /// Mutable view of the injection list (used to stamp materialized
    /// paths on builtin / tier-3-4 entries).
    pub fn inject_mut(&mut self) -> &mut Vec<Injection> {
        match self {
            SpliceRule::Before { inject, .. }
            | SpliceRule::Between { inject, .. }
            | SpliceRule::OnNode { inject, .. }
            | SpliceRule::OnSubgraph { inject, .. } => inject,
        }
    }

    #[cfg(test)]
    pub(crate) fn before(
        interface: &str,
        provider_name: Option<&str>,
        provider_alias: Option<String>,
        inject: Vec<Injection>,
    ) -> Self {
        let interface = Pattern::compile(vec![interface.to_string()]).unwrap();
        let mut constraints = vec![];
        if let Some(p) = provider_name {
            constraints.push(Constraint::Provider(
                Pattern::compile(vec![p.to_string()]).unwrap(),
            ));
        }
        SpliceRule::Before {
            matcher: RuleMatcher::new(SiteKind::Before, interface, None, constraints),
            provider_alias,
            inject,
        }
    }
}

/// Reject characters that could close an `import ...;` clause or
/// inject new WIT declarations when interpolated into a synthesized
/// adapter world. Permits fully-qualified use-paths and glob patterns;
/// shape checks happen downstream.
fn validate_interface_name(rule_num: usize, interface: &str) -> anyhow::Result<()> {
    let safe = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '.' | ':' | '/' | '@' | '*' | '?' | '[' | ']')
    };
    if let Some(bad) = interface.chars().find(|c| !safe(*c)) {
        bail!(
            "rule {rule_num}: 'interface' contains disallowed character {bad:?} in '{interface}'"
        );
    }
    Ok(())
}

fn validate_interface_field(rule_num: usize, iface: &OneOrMany<String>) -> anyhow::Result<()> {
    let pats = iface.as_slice();
    if pats.is_empty() || pats.iter().any(|a| a.is_empty()) {
        bail!("rule {rule_num}: 'interface' must not be empty");
    }
    for pat in pats {
        validate_interface_name(rule_num, pat)?;
    }
    Ok(())
}

fn check_node_name(
    rule_num: usize,
    field: &str,
    provider: Option<&YamlProviderOpt>,
) -> anyhow::Result<()> {
    if let Some(name) = provider.and_then(|p| p.name.as_ref()) {
        let pats = name.as_slice();
        if pats.is_empty() || pats.iter().any(|a| a.is_empty()) {
            bail!(
                "rule {rule_num}: '{field}' name must not be empty if specified \
                 (omit the key to leave it unset)"
            );
        }
    }
    Ok(())
}

/// Compile a [`OneOrMany<String>`] into a [`Pattern`], wrapping a bad-glob
/// error with rule/field context so it reads as a config error.
fn compile_pattern(
    rule_num: usize,
    field: &str,
    spec: OneOrMany<String>,
) -> anyhow::Result<Pattern> {
    Pattern::compile(spec.into_vec()).map_err(|e| anyhow::anyhow!("rule {rule_num}: '{field}' {e}"))
}

/// Compile a node-name field's pattern, if its `name` is set. A missing
/// provider block or `name` means "match any" — `None`, no constraint.
/// Callers wrap the result in the right [`Constraint`] axis.
fn node_name_pattern(
    rule_num: usize,
    field: &str,
    provider: Option<YamlProviderOpt>,
) -> anyhow::Result<Option<Pattern>> {
    provider
        .and_then(|p| p.name)
        .map(|name| compile_pattern(rule_num, field, name))
        .transpose()
}

fn compile_func_pred(
    rule_num: usize,
    pred: Option<YamlFuncPred>,
) -> anyhow::Result<Option<FuncPred>> {
    let Some(YamlFuncPred {
        is_async,
        scope,
        args,
        results,
    }) = pred
    else {
        return Ok(None);
    };

    if is_async.is_none() && scope.is_none() && args.is_none() && results.is_none() {
        bail!(
            "rule {rule_num}: 'all-funcs' has no constraints — omit the key to impose no \
             function requirement, or set 'async'/'scope'/'args'/'results'"
        );
    }
    let scopes = compile_scopes(rule_num, scope)?;
    let args = compile_value_props(rule_num, "args", args)?;
    let results = compile_value_props(rule_num, "results", results)?;
    Ok(Some(FuncPred::new(is_async, scopes, args, results)))
}

fn compile_value_props(
    rule_num: usize,
    field: &str,
    spec: Option<OneOrMany<String>>,
) -> anyhow::Result<Vec<ValueProperty>> {
    let Some(spec) = spec else {
        return Ok(vec![]);
    };
    spec.into_vec()
        .iter()
        .map(|kw| {
            kw.parse::<ValueProperty>().map_err(|()| {
                anyhow::anyhow!(
                    "rule {rule_num}: 'all-funcs.{field}' has unknown property '{kw}' \
                     (expected 'concrete' or 'defaultable')"
                )
            })
        })
        .collect()
}

fn compile_scopes(
    rule_num: usize,
    spec: Option<OneOrMany<String>>,
) -> anyhow::Result<Vec<FuncScope>> {
    let Some(spec) = spec else {
        // default to `interface` scope (common case)
        return Ok(vec![FuncScope::Interface]);
    };
    let raw = spec.into_vec();
    if raw.is_empty() {
        bail!("rule {rule_num}: 'all-funcs.scope' must list at least one value");
    }
    raw.iter()
        .map(|kw| {
            kw.parse::<FuncScope>().map_err(|()| {
                anyhow::anyhow!(
                    "rule {rule_num}: 'all-funcs.scope' has unknown value '{kw}' \
                     (expected 'interface' or 'resource')"
                )
            })
        })
        .collect()
}

impl ConfigFile {
    /// Validate the parsed configuration, returning a descriptive error for any problem.
    ///
    /// Checks (in order):
    /// 1. Supported version number.
    /// 2. Each rule specifies exactly one strategy (`before` XOR `between`).
    /// 3. Each rule's `inject` list is non-empty.
    /// 4. Each injection name is non-empty.
    /// 5. User-form injections carry a non-empty `path` (builtin form
    ///    fills it in at splice time).
    /// 6. Interface names are non-empty.
    /// 7. `before` provider `name`, when present, is non-empty.
    /// 8. `between` `inner` and `outer` must name different instances.
    /// 9. Injection names are globally unique across all rules (required because
    ///    each name becomes a WAC instance identifier and `--dep` argument key).
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.version != 1 {
            bail!(
                "unsupported config version {}: only version 1 is supported",
                self.version
            );
        }

        // name → first rule index (1-based) for duplicate detection
        let mut seen_names: HashMap<&str, usize> = HashMap::new();

        for (i, rule) in self.rules.iter().enumerate() {
            let rule_num = i + 1;

            // Must be exactly one strategy.
            let strategy_count = [
                rule.before.is_some(),
                rule.between.is_some(),
                rule.on_node.is_some(),
                rule.on_subgraph.is_some(),
            ]
            .iter()
            .filter(|x| **x)
            .count();
            match strategy_count {
                0 => bail!("rule {rule_num}: a rule must specify one strategy"),
                1 => {}
                _ => bail!("rule {rule_num}: a rule may specify only one strategy"),
            }

            // Strategy-specific checks.
            if let Some(b) = &rule.before {
                validate_interface_field(rule_num, &b.interface)?;
                check_node_name(rule_num, "provider", b.provider.as_ref())?;
            }
            if let Some(bw) = &rule.between {
                validate_interface_field(rule_num, &bw.interface)?;
                check_node_name(rule_num, "inner", bw.inner.as_ref())?;
                check_node_name(rule_num, "outer", bw.outer.as_ref())?;
                // Reject only when both names are present and identical
                // literal patterns.
                let inner = bw.inner.as_ref().and_then(|p| p.name.as_ref());
                let outer = bw.outer.as_ref().and_then(|p| p.name.as_ref());
                if let (Some(i), Some(o)) = (inner, outer) {
                    if i.as_slice() == o.as_slice() {
                        bail!(
                            "rule {rule_num} (between): 'inner' and 'outer' must name different \
                             instances, but both are '{}'",
                            i.as_slice().join(", ")
                        );
                    }
                }
            }
            if let Some(on) = &rule.on_node {
                if on.name.is_empty() {
                    bail!("rule {rule_num} (on_node): 'name' must not be empty");
                }
                if let Some(iface) = &on.interface {
                    validate_interface_field(rule_num, iface)?;
                }
            }
            if let Some(sub) = &rule.on_subgraph {
                if sub.nodes.is_empty() {
                    bail!("rule {rule_num} (on_subgraph): 'nodes' must list at least one entry");
                }
                let mut seen: HashMap<&str, ()> = HashMap::new();
                for n in &sub.nodes {
                    if n.is_empty() {
                        bail!("rule {rule_num} (on_subgraph): 'nodes' entries must not be empty");
                    }
                    if seen.insert(n.as_str(), ()).is_some() {
                        bail!("rule {rule_num} (on_subgraph): 'nodes' contains duplicate '{n}'");
                    }
                }
                if let Some(iface) = &sub.interface {
                    validate_interface_field(rule_num, iface)?;
                }
            }

            // inject list must be non-empty.
            if rule.inject.is_empty() {
                bail!("rule {rule_num}: 'inject' list must contain at least one entry");
            }

            for (j, inj) in rule.inject.iter().enumerate() {
                let inj_num = j + 1;

                // user form vs builtin form are mutually exclusive.
                match (&inj.builtin, &inj.name, &inj.path) {
                    (None, None, _) => {
                        bail!("rule {rule_num}, injection {inj_num}: missing 'name' or 'builtin'")
                    }
                    (Some(_), Some(_), _) => bail!(
                        "rule {rule_num}, injection {inj_num}: 'builtin' replaces top-level \
                         'name' — move the WAC-var override to 'builtin.alias'"
                    ),
                    (Some(_), _, Some(_)) => bail!(
                        "rule {rule_num}, injection {inj_num}: 'builtin' and 'path' are mutually \
                         exclusive — drop one"
                    ),
                    _ => {}
                }
                if inj.name.as_deref() == Some("") {
                    bail!("rule {rule_num}, injection {inj_num}: injection name must not be empty");
                }
                if inj.path.as_deref() == Some("") {
                    bail!("rule {rule_num}, injection {inj_num}: 'path' must not be empty");
                }
                if inj.builtin.is_none() && inj.path.is_none() {
                    bail!(
                        "rule {rule_num}, injection {inj_num}: user-form injection requires \
                         'path' (splicer needs the bytes to fingerprint the middleware)"
                    );
                }
                if let Some(spec) = &inj.builtin {
                    if spec.builtin_name().is_empty() {
                        bail!(
                            "rule {rule_num}, injection {inj_num}: builtin 'name' must not be \
                             empty"
                        );
                    }
                    if spec.alias() == Some("") {
                        bail!(
                            "rule {rule_num}, injection {inj_num}: builtin 'alias' must not be \
                             empty if specified (omit the key to leave it unset)"
                        );
                    }
                    // Surface bad config shapes at parse time, not
                    // splice time. The into_injection path expects
                    // this to have run. Empty maps are fine.
                    yaml_config_to_toml(spec.config()).map_err(|e| {
                        anyhow::anyhow!("rule {rule_num}, injection {inj_num}: {e}")
                    })?;
                }

                // Effective WAC-var name for uniqueness: builtin form
                // uses `alias` falling back to the builtin's name; user
                // form uses the top-level `name`.
                let effective_name = if let Some(spec) = &inj.builtin {
                    spec.alias().unwrap_or_else(|| spec.builtin_name())
                } else {
                    inj.name.as_deref().expect("validated above")
                };

                // Global uniqueness: injection names are used as WAC identifiers.
                if let Some(first_rule) = seen_names.get(effective_name) {
                    bail!(
                        "injection name '{effective_name}' is used in rule {rule_num} but was \
                         already declared in rule {first_rule}; each injection must have a \
                         globally unique name"
                    );
                }
                seen_names.insert(effective_name, rule_num);
            }
        }

        Ok(())
    }

    /// Convert validated YAML rules into normalized [`SpliceRule`]s,
    /// compiling each rule's patterns into a [`RuleMatcher`]. A bad glob
    /// surfaces here as a config error, before any generation.
    ///
    /// Higher-level selectors (`on_node`) fan out to multiple per-edge
    /// rules here, so one YAML rule may produce >1 `SpliceRule`.
    /// Assumes [`ConfigFile::validate`] has already been called.
    pub fn into_splice_rules(self) -> anyhow::Result<Vec<SpliceRule>> {
        self.rules
            .into_iter()
            .enumerate()
            .map(|(i, rule)| desugar_rule(i + 1, rule))
            .collect::<anyhow::Result<Vec<_>>>()
            .map(|nested| nested.into_iter().flatten().collect())
    }
}

/// Compile one validated YAML rule into one or more normalized
/// [`SpliceRule`]s. `before`/`between`/`on_subgraph` produce one each;
/// `on_node` fans out to one or two depending on `direction`. The
/// tuple match is exhaustive: a new strategy field on `YamlRule` won't
/// compile until a matching arm is added here.
fn desugar_rule(rule_num: usize, rule: YamlRule) -> anyhow::Result<Vec<SpliceRule>> {
    let YamlRule {
        before,
        between,
        on_node,
        on_subgraph,
        inject,
    } = rule;
    let inject: Vec<Injection> = inject.into_iter().map(into_injection).collect();
    match (before, between, on_node, on_subgraph) {
        (Some(s), None, None, None) => Ok(vec![compile_before(rule_num, s, inject)?]),
        (None, Some(s), None, None) => Ok(vec![compile_between(rule_num, s, inject)?]),
        (None, None, Some(s), None) => Ok(vec![compile_on_node(rule_num, s, inject)?]),
        (None, None, None, Some(s)) => Ok(vec![compile_on_subgraph(rule_num, s, inject)?]),
        _ => unreachable!("validate() guarantees exactly one strategy per rule"),
    }
}

fn compile_before(
    rule_num: usize,
    spec: YamlStrategyBefore,
    inject: Vec<Injection>,
) -> anyhow::Result<SpliceRule> {
    let YamlStrategyBefore {
        interface,
        provider,
        all_funcs,
    } = spec;
    let interface = compile_pattern(rule_num, "interface", interface)?;
    let all_funcs = compile_func_pred(rule_num, all_funcs)?;
    let provider_alias = provider.as_ref().and_then(|p| p.alias.clone());
    let constraints = node_name_pattern(rule_num, "provider", provider)?
        .map(Constraint::Provider)
        .into_iter()
        .collect();
    Ok(SpliceRule::Before {
        matcher: RuleMatcher::new(SiteKind::Before, interface, all_funcs, constraints),
        provider_alias,
        inject,
    })
}

fn compile_between(
    rule_num: usize,
    spec: YamlStrategyBetween,
    inject: Vec<Injection>,
) -> anyhow::Result<SpliceRule> {
    let YamlStrategyBetween {
        interface,
        inner,
        outer,
        all_funcs,
    } = spec;
    let interface = compile_pattern(rule_num, "interface", interface)?;
    let all_funcs = compile_func_pred(rule_num, all_funcs)?;
    let inner_alias = inner.as_ref().and_then(|p| p.alias.clone());
    let outer_alias = outer.as_ref().and_then(|p| p.alias.clone());
    let constraints = [
        node_name_pattern(rule_num, "inner", inner)?.map(Constraint::Provider),
        node_name_pattern(rule_num, "outer", outer)?.map(Constraint::Caller),
    ]
    .into_iter()
    .flatten()
    .collect();
    Ok(SpliceRule::Between {
        matcher: RuleMatcher::new(SiteKind::Between, interface, all_funcs, constraints),
        inner_alias,
        outer_alias,
        inject,
    })
}

/// Compile `on_node` into the unresolved [`SpliceRule::OnNode`] variant.
/// Expansion happens later.
fn compile_on_node(
    rule_num: usize,
    spec: YamlStrategyOnNode,
    inject: Vec<Injection>,
) -> anyhow::Result<SpliceRule> {
    let YamlStrategyOnNode {
        name,
        direction,
        alias,
        interface,
        all_funcs,
    } = spec;
    let interface = interface.unwrap_or_else(|| OneOrMany::One("*".to_string()));
    let interface = compile_pattern(rule_num, "interface", interface)?;
    let name = compile_pattern(rule_num, "on_node.name", OneOrMany::One(name))?;
    let all_funcs = compile_func_pred(rule_num, all_funcs)?;
    Ok(SpliceRule::OnNode {
        name,
        direction,
        interface,
        all_funcs,
        alias,
        inject,
    })
}

fn compile_on_subgraph(
    rule_num: usize,
    spec: YamlStrategyOnSubgraph,
    inject: Vec<Injection>,
) -> anyhow::Result<SpliceRule> {
    let YamlStrategyOnSubgraph {
        nodes,
        direction,
        interface,
        all_funcs,
    } = spec;
    let iface_pats = interface.unwrap_or_else(|| OneOrMany::One("*".to_string()));
    let interface = compile_pattern(rule_num, "interface", iface_pats)?;
    let all_funcs = compile_func_pred(rule_num, all_funcs)?;
    Ok(SpliceRule::OnSubgraph {
        nodes,
        direction,
        interface,
        all_funcs,
        inject,
    })
}

/// Assumes [`ConfigFile::validate`] ran.
fn into_injection(yaml: YamlInjection) -> Injection {
    let YamlInjection {
        name,
        path,
        builtin,
    } = yaml;
    let (wac_name, builtin_name, builtin_config) = match builtin {
        Some(spec) => {
            let bname = spec.builtin_name().to_string();
            let alias = spec.alias().map(str::to_string);
            // `validate()` ran first, so stringification can't fail
            // here — expect on the result rather than threading
            // Result through the rule-construction path.
            let cfg = yaml_config_to_toml(spec.config()).expect("validate() ran");
            (alias.unwrap_or_else(|| bname.clone()), Some(bname), cfg)
        }
        None => (name.expect("validated"), None, BTreeMap::new()),
    };
    Injection {
        name: wac_name,
        path,
        builtin: builtin_name,
        builtin_config,
        config_as_wave: None,
        config_provider_path: None,
        adapter_info: None,
        tier: None,
        resource_bearing_exports: Vec::new(),
        t_prime_redirects: Vec::new(),
        edge_shim_specs: Vec::new(),
    }
}

fn yaml_config_to_toml(
    values: &BTreeMap<String, serde_yaml::Value>,
) -> anyhow::Result<BTreeMap<String, toml::Value>> {
    let mut out = BTreeMap::new();
    for (key, val) in values {
        let v = yaml_to_toml(val).map_err(|e| anyhow::anyhow!("config key '{key}': {e}"))?;
        out.insert(key.clone(), v);
    }
    Ok(out)
}

fn yaml_to_toml(v: &serde_yaml::Value) -> anyhow::Result<toml::Value> {
    use serde_yaml::Value as Y;
    Ok(match v {
        Y::String(s) => toml::Value::String(s.clone()),
        Y::Bool(b) => toml::Value::Boolean(*b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                toml::Value::Float(f)
            } else {
                bail!("number {n} could not be represented as i64 or f64");
            }
        }
        Y::Null => bail!("value is null; omit the key or use an empty string scalar instead"),
        Y::Sequence(items) => {
            let parts: anyhow::Result<Vec<_>> = items.iter().map(yaml_to_toml).collect();
            toml::Value::Array(parts?)
        }
        Y::Mapping(m) => {
            let mut table = toml::map::Map::new();
            for (k, v) in m {
                let key = match k {
                    Y::String(s) => s.clone(),
                    other => bail!("table key must be a string, got {other:?}",),
                };
                table.insert(key, yaml_to_toml(v)?);
            }
            toml::Value::Table(table)
        }
        Y::Tagged(_) => bail!("YAML-tagged value isn't supported by the substrate"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_before_rule() {
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
      provider:
        name: srv-b
    inject:
      - name: middleware-a
        path: ./middleware-a.wasm
"#;
        let rules = parse_yaml(yaml).unwrap();
        assert_eq!(rules.len(), 1);
        let SpliceRule::Before {
            matcher,
            provider_alias,
            inject,
        } = &rules[0]
        else {
            panic!("expected Before rule");
        };
        assert_eq!(matcher.interface_raw(), ["wasi:http/handler@0.3.0"]);
        assert!(matcher.interface_matches("wasi:http/handler@0.3.0"));
        assert_eq!(matcher.provider_raw(), Some(&["srv-b".to_string()][..]));
        assert!(provider_alias.is_none());
        assert_eq!(inject.len(), 1);
        assert_eq!(inject[0].name, "middleware-a");
        assert_eq!(inject[0].path.as_deref(), Some("./middleware-a.wasm"));
    }

    #[test]
    fn parse_before_rule_no_provider() {
        // `provider` is optional — omitting it means inject before every instance.
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
        assert_eq!(rules.len(), 1);
        let SpliceRule::Before {
            matcher,
            provider_alias,
            ..
        } = &rules[0]
        else {
            panic!("expected Before rule");
        };
        // No provider constraint ⇒ matches any node.
        assert!(matcher.provider_raw().is_none());
        assert!(provider_alias.is_none());
    }

    #[test]
    fn parse_between_rule() {
        let yaml = r#"
version: 1
rules:
  - between:
      interface: wasi:http/handler@0.3.0
      inner:
        name: srv-b
        alias: renamed-b
      outer:
        name: srv
    inject:
      - name: mw-a
        path: /tmp/mw-a.wasm
      - name: mw-b
        path: /tmp/mw-b.wasm
"#;
        let rules = parse_yaml(yaml).unwrap();
        assert_eq!(rules.len(), 1);
        let SpliceRule::Between {
            matcher,
            inner_alias,
            outer_alias,
            inject,
        } = &rules[0]
        else {
            panic!("expected Between rule");
        };
        assert_eq!(matcher.interface_raw(), ["wasi:http/handler@0.3.0"]);
        assert_eq!(matcher.provider_raw(), Some(&["srv-b".to_string()][..]));
        assert_eq!(matcher.caller_raw(), Some(&["srv".to_string()][..]));
        assert_eq!(inner_alias.as_deref(), Some("renamed-b"));
        assert!(outer_alias.is_none());
        assert_eq!(inject.len(), 2);
        assert_eq!(inject[1].path.as_deref(), Some("/tmp/mw-b.wasm"));
    }

    #[test]
    fn parse_multi_rule() {
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
    inject:
      - name: first
        path: /tmp/first.wasm
  - between:
      interface: wasi:http/handler@0.3.0
      inner:
        name: srv-b
      outer:
        name: srv
    inject:
      - name: second
        path: /tmp/second.wasm
"#;
        let rules = parse_yaml(yaml).unwrap();
        assert_eq!(rules.len(), 2);
        assert!(matches!(rules[0], SpliceRule::Before { .. }));
        assert!(matches!(rules[1], SpliceRule::Between { .. }));
        // Order is preserved
        let SpliceRule::Before { inject: inj0, .. } = &rules[0] else {
            unreachable!()
        };
        let SpliceRule::Between { inject: inj1, .. } = &rules[1] else {
            unreachable!()
        };
        assert_eq!(inj0[0].name, "first");
        assert_eq!(inj1[0].name, "second");
    }

    #[test]
    fn parse_missing_interface() {
        // `interface` is required inside `before`; omitting it is a parse error.
        let yaml = r#"
version: 1
rules:
  - before:
      provider:
        name: srv-b
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let result = parse_yaml(yaml);
        assert!(
            result.is_err(),
            "expected parse error for missing interface field"
        );
    }

    #[test]
    fn parse_unknown_version() {
        let yaml = r#"
version: 99
rules: []
"#;
        let err = parse_yaml(yaml).unwrap_err().to_string();
        assert!(
            err.contains("unsupported config version"),
            "unexpected error: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Validation error cases
    // -----------------------------------------------------------------------

    fn assert_err(yaml: &str, expected_fragment: &str) {
        let err = parse_yaml(yaml).unwrap_err().to_string();
        assert!(
            err.contains(expected_fragment),
            "expected error containing {expected_fragment:?}, got: {err}"
        );
    }

    #[test]
    fn validate_both_before_and_between() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    between:
      interface: wasi:http/handler
      inner:
        name: a
      outer:
        name: b
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "may specify only one strategy",
        );
    }

    #[test]
    fn validate_neither_before_nor_between() {
        assert_err(
            r#"
version: 1
rules:
  - inject:
      - name: mw
        path: ./mw.wasm
"#,
            "must specify one strategy",
        );
    }

    #[test]
    fn validate_empty_inject_list() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject: []
"#,
            "'inject' list must contain at least one entry",
        );
    }

    #[test]
    fn validate_empty_injection_name() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - name: ""
"#,
            "injection name must not be empty",
        );
    }

    #[test]
    fn validate_empty_injection_path() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - name: mw
        path: ""
"#,
            "'path' must not be empty",
        );
    }

    #[test]
    fn validate_missing_injection_path() {
        // User-form injection without `path:` is a config error —
        // splicer needs the bytes on disk to verify type signatures.
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - name: mw
"#,
            "user-form injection requires 'path'",
        );
    }

    #[test]
    fn validate_empty_interface_name() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: ""
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "'interface' must not be empty",
        );
    }

    #[test]
    fn validate_interface_name_glob_pattern() {
        // Glob patterns (e.g. `wasi*`, `wasi:http/*`) must pass
        // config validation; downstream resolution does the matching.
        let yaml = r#"
version: 1
rules:
  - before:
      interface: "wasi:http/*"
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        parse_yaml(yaml).expect("glob pattern should parse cleanly");
    }

    #[test]
    fn parse_interface_pattern_list() {
        // A YAML list of patterns matches if any one matches (OR).
        let yaml = r#"
version: 1
rules:
  - before:
      interface: ["wasi:*", "my:srv/*"]
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("list form should parse");
        let m = rules[0].matcher();
        assert_eq!(m.interface_raw(), ["wasi:*", "my:srv/*"]);
        assert!(m.interface_matches("wasi:http/handler@0.3.0"));
        assert!(m.interface_matches("my:srv/api@1.0.0"));
        assert!(!m.interface_matches("other:pkg/iface@1.0.0"));
    }

    #[test]
    fn parse_between_optional_names() {
        // `inner`/`outer` are now optional; omitting them matches any.
        let yaml = r#"
version: 1
rules:
  - between:
      interface: "wasi:*"
      outer:
        name: auth
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("optional inner should parse");
        let SpliceRule::Between { matcher, .. } = &rules[0] else {
            panic!("expected Between");
        };
        assert!(matcher.provider_raw().is_none());
        assert_eq!(matcher.caller_raw(), Some(&["auth".to_string()][..]));
    }

    #[test]
    fn validate_bad_glob_is_config_error() {
        // An unterminated char class passes char-safety but fails to
        // compile — surfaced as a config error before any generation.
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: "wasi:["
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "invalid glob pattern",
        );
    }

    #[test]
    fn validate_between_glob_inner_outer_allowed() {
        // Different patterns on inner/outer are fine even though both
        // are globs — only identical literal patterns are rejected.
        let yaml = r#"
version: 1
rules:
  - between:
      interface: "*"
      inner:
        name: "wasi*"
      outer:
        name: "mysrv*"
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        parse_yaml(yaml).expect("distinct globbed inner/outer should parse");
    }

    #[test]
    fn validate_interface_name_injection() {
        // A semicolon and a second world declaration would inject an
        // extra world if formatted into the synthesized adapter WIT.
        assert_err(
            "version: 1
rules:
  - before:
      interface: \"foo;\\nworld evil { import bar/baz; }\\n\"
    inject:
      - name: mw
        path: ./mw.wasm
",
            "disallowed character",
        );
    }

    #[test]
    fn validate_interface_name_whitespace() {
        // Whitespace inside the path opens an injection vector once
        // interpolated; reject regardless of glob vs. canonical form.
        assert_err(
            "version: 1
rules:
  - before:
      interface: \"wasi : http / handler\"
    inject:
      - name: mw
        path: ./mw.wasm
",
            "disallowed character",
        );
    }

    #[test]
    fn validate_empty_before_provider_name() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
      provider:
        name: ""
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "'provider' name must not be empty if specified",
        );
    }

    #[test]
    fn validate_between_same_inner_outer() {
        assert_err(
            r#"
version: 1
rules:
  - between:
      interface: wasi:http/handler
      inner:
        name: srv
      outer:
        name: srv
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "'inner' and 'outer' must name different instances",
        );
    }

    #[test]
    fn validate_duplicate_injection_name_across_rules() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - name: mw-a
        path: ./mw-a.wasm
  - before:
      interface: wasi:logging/log
    inject:
      - name: mw-a
        path: ./mw-a.wasm
"#,
            "injection name 'mw-a' is used in rule 2 but was already declared in rule 1",
        );
    }

    #[test]
    fn validate_duplicate_injection_name_within_rule() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - name: mw-a
        path: ./mw-a.wasm
      - name: mw-a
        path: ./mw-a.wasm
"#,
            "injection name 'mw-a' is used in rule 1 but was already declared in rule 1",
        );
    }

    // -----------------------------------------------------------------------
    // Builtin form
    // -----------------------------------------------------------------------

    #[test]
    fn parse_builtin_short_form() {
        // `builtin: <scalar>` — name defaults from the builtin name.
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - builtin: hello-tier1
"#;
        let rules = parse_yaml(yaml).unwrap();
        let SpliceRule::Before { inject, .. } = &rules[0] else {
            panic!("expected Before");
        };
        assert_eq!(inject.len(), 1);
        assert_eq!(inject[0].name, "hello-tier1");
        assert_eq!(inject[0].builtin.as_deref(), Some("hello-tier1"));
        assert!(inject[0].path.is_none());
    }

    #[test]
    fn parse_builtin_long_form_with_alias() {
        // `builtin: { name: ..., alias: ... }` — alias becomes WAC var.
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - builtin:
          name: hello-tier1
          alias: greeter
"#;
        let rules = parse_yaml(yaml).unwrap();
        let SpliceRule::Before { inject, .. } = &rules[0] else {
            panic!("expected Before");
        };
        assert_eq!(inject[0].name, "greeter");
        assert_eq!(inject[0].builtin.as_deref(), Some("hello-tier1"));
    }

    #[test]
    fn parse_builtin_long_form_no_alias() {
        // `builtin: { name: ... }` without alias — name defaults from
        // the builtin name.
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - builtin:
          name: hello-tier1
"#;
        let rules = parse_yaml(yaml).unwrap();
        let SpliceRule::Before { inject, .. } = &rules[0] else {
            panic!("expected Before");
        };
        assert_eq!(inject[0].name, "hello-tier1");
        assert_eq!(inject[0].builtin.as_deref(), Some("hello-tier1"));
    }

    #[test]
    fn validate_builtin_with_top_level_name_rejected() {
        // The builtin form scopes the WAC-var override inside the
        // `builtin:` map; a top-level `name:` next to it is ambiguous.
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - name: greeter
        builtin: hello-tier1
"#,
            "'builtin' replaces top-level 'name'",
        );
    }

    #[test]
    fn validate_builtin_with_path_rejected() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - builtin: hello-tier1
        path: ./mw.wasm
"#,
            "'builtin' and 'path' are mutually exclusive",
        );
    }

    #[test]
    fn validate_neither_name_nor_builtin() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - path: ./mw.wasm
"#,
            "missing 'name' or 'builtin'",
        );
    }

    #[test]
    fn validate_builtin_long_form_empty_alias() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - builtin:
          name: hello-tier1
          alias: ""
"#,
            "builtin 'alias' must not be empty if specified",
        );
    }

    // -----------------------------------------------------------------------
    // Builtin config block
    // -----------------------------------------------------------------------

    #[test]
    fn parse_builtin_config_block_stringifies_scalars() {
        // YAML scalars (numbers, bools, strings) all flatten to strings
        // by the time the injection lands in the splice pipeline.
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
    inject:
      - builtin:
          name: hello-tier1
          config:
            buffer: 100
            flush_after_seconds: 10.0
            note: "hi there"
            enable: true
"#;
        let rules = parse_yaml(yaml).expect("parse");
        let SpliceRule::Before { inject, .. } = &rules[0] else {
            panic!("expected Before");
        };
        let cfg = &inject[0].builtin_config;
        assert_eq!(cfg.get("buffer"), Some(&toml::Value::Integer(100)));
        assert_eq!(
            cfg.get("flush_after_seconds"),
            Some(&toml::Value::Float(10.0))
        );
        assert_eq!(
            cfg.get("note"),
            Some(&toml::Value::String("hi there".into()))
        );
        assert_eq!(cfg.get("enable"), Some(&toml::Value::Boolean(true)));
    }

    #[test]
    fn parse_builtin_config_block_defaults_empty() {
        // No `config:` block → empty map, every builtin still works.
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
    inject:
      - builtin:
          name: hello-tier1
"#;
        let rules = parse_yaml(yaml).expect("parse");
        let SpliceRule::Before { inject, .. } = &rules[0] else {
            panic!("expected Before");
        };
        assert!(inject[0].builtin_config.is_empty());
    }

    #[test]
    fn parse_builtin_short_form_has_no_config() {
        // Short form (`builtin: name`) carries no config map.
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
    inject:
      - builtin: hello-tier1
"#;
        let rules = parse_yaml(yaml).expect("parse");
        let SpliceRule::Before { inject, .. } = &rules[0] else {
            panic!("expected Before");
        };
        assert!(inject[0].builtin_config.is_empty());
    }

    #[test]
    fn parse_builtin_config_block_preserves_list() {
        // YAML sequences round-trip as toml::Value::Array — splice-
        // time type-checking against the builtin's WIT type happens
        // later in `ensure_provider_for`. The parser only has to keep
        // structural fidelity.
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
    inject:
      - builtin:
          name: hello-tier1
          config:
            rules:
              - "1.2.3.4/32"
              - "5.6.7.8/32"
"#;
        let rules = parse_yaml(yaml).expect("parse");
        let SpliceRule::Before { inject, .. } = &rules[0] else {
            panic!("expected Before");
        };
        let toml::Value::Array(xs) = inject[0].builtin_config.get("rules").unwrap() else {
            panic!("expected array");
        };
        assert_eq!(xs.len(), 2);
    }

    #[test]
    fn parse_builtin_config_block_preserves_map() {
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
    inject:
      - builtin:
          name: hello-tier1
          config:
            limits:
              max: 100
              min: 0
"#;
        let rules = parse_yaml(yaml).expect("parse");
        let SpliceRule::Before { inject, .. } = &rules[0] else {
            panic!("expected Before");
        };
        let toml::Value::Table(t) = inject[0].builtin_config.get("limits").unwrap() else {
            panic!("expected table");
        };
        assert_eq!(t.get("max"), Some(&toml::Value::Integer(100)));
        assert_eq!(t.get("min"), Some(&toml::Value::Integer(0)));
    }

    #[test]
    fn validate_builtin_config_rejects_null() {
        // Explicit nulls signal a config bug — surface clearly rather
        // than silently emitting an empty string.
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler@0.3.0
    inject:
      - builtin:
          name: hello-tier1
          config:
            buffer: null
"#,
            "is null",
        );
    }

    #[test]
    fn validate_duplicate_alias_collides_with_user_name() {
        // The alias is the WAC var, so it must be globally unique
        // alongside user middleware names.
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: wasi:http/handler
    inject:
      - name: greeter
        path: ./greeter.wasm
      - builtin:
          name: hello-tier1
          alias: greeter
"#,
            "injection name 'greeter' is used in rule 1 but was already declared in rule 1",
        );
    }

    // -----------------------------------------------------------------------
    // all-funcs predicate
    // -----------------------------------------------------------------------

    #[test]
    fn parse_all_funcs_scalar_and_list() {
        // `args` scalar, `results` list — the list ANDs both properties.
        let yaml = r#"
version: 1
rules:
  - before:
      interface: "*"
      all-funcs:
        async: true
        args: concrete
        results: [concrete, defaultable]
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("all-funcs should parse");
        let pred = rules[0].matcher().all_funcs().expect("all_funcs present");
        assert_eq!(pred.is_async, Some(true));
        assert_eq!(pred.args, [ValueProperty::Concrete]);
        assert_eq!(
            pred.results,
            [ValueProperty::Concrete, ValueProperty::Defaultable]
        );
    }

    #[test]
    fn parse_all_funcs_async_false() {
        // The symmetric all-sync gate.
        let yaml = r#"
version: 1
rules:
  - between:
      interface: "wasi:*"
      all-funcs:
        async: false
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("async:false should parse");
        let pred = rules[0].matcher().all_funcs().expect("all_funcs present");
        assert_eq!(pred.is_async, Some(false));
        assert!(pred.args.is_empty() && pred.results.is_empty());
    }

    #[test]
    fn parse_all_funcs_absent_is_none() {
        let yaml = r#"
version: 1
rules:
  - before:
      interface: "wasi:*"
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("parse");
        assert!(rules[0].matcher().all_funcs().is_none());
    }

    #[test]
    fn validate_all_funcs_unknown_keyword() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: "*"
      all-funcs:
        results: [concrete, bogus]
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "unknown property 'bogus'",
        );
    }

    #[test]
    fn validate_all_funcs_empty_rejected() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: "*"
      all-funcs: {}
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "'all-funcs' has no constraints",
        );
    }

    #[test]
    fn parse_all_funcs_scope_scalar_and_list() {
        let yaml = r#"
version: 1
rules:
  - before:
      interface: "*"
      all-funcs:
        scope: interface
    inject:
      - name: mw
        path: ./mw.wasm
  - before:
      interface: "*"
      all-funcs:
        scope: [interface, resource]
    inject:
      - name: mw2
        path: ./mw2.wasm
"#;
        let rules = parse_yaml(yaml).expect("scope should parse");
        let scalar = rules[0].matcher().all_funcs().expect("all_funcs present");
        assert_eq!(scalar.scopes, [FuncScope::Interface]);
        let list = rules[1].matcher().all_funcs().expect("all_funcs present");
        assert_eq!(list.scopes, [FuncScope::Interface, FuncScope::Resource]);
    }

    #[test]
    fn parse_all_funcs_scope_absent_defaults_to_interface() {
        let yaml = r#"
version: 1
rules:
  - before:
      interface: "*"
      all-funcs:
        async: true
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("parse");
        let pred = rules[0].matcher().all_funcs().expect("all_funcs present");
        assert_eq!(pred.scopes, [FuncScope::Interface]);
    }

    #[test]
    fn validate_all_funcs_scope_unknown_value() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: "*"
      all-funcs:
        scope: bogus
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "unknown value 'bogus'",
        );
    }

    // -----------------------------------------------------------------------
    // on_node selector
    // -----------------------------------------------------------------------

    /// Helper: assert `rules` has one `OnNode` and return its fields by
    /// reference. Keeps the per-test pattern-matching boilerplate down.
    fn one_on_node(rules: &[SpliceRule]) -> (&Pattern, Direction, &Pattern, Option<&str>) {
        assert_eq!(rules.len(), 1, "on_node should parse to one OnNode variant");
        let SpliceRule::OnNode {
            name,
            direction,
            interface,
            alias,
            ..
        } = &rules[0]
        else {
            panic!("expected OnNode, got {:?}", rules[0]);
        };
        (name, *direction, interface, alias.as_deref())
    }

    #[test]
    fn parse_on_node_default_direction_is_both() {
        let yaml = r#"
version: 1
rules:
  - on_node:
      name: srv-b
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("on_node should parse");
        let (name, direction, interface, _) = one_on_node(&rules);
        assert_eq!(direction, Direction::Both);
        assert_eq!(name.raw(), &["srv-b".to_string()]);
        assert_eq!(interface.raw(), &["*".to_string()]);
    }

    #[test]
    fn parse_on_node_inbound() {
        let yaml = r#"
version: 1
rules:
  - on_node:
      name: srv-b
      direction: inbound
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("inbound should parse");
        assert_eq!(one_on_node(&rules).1, Direction::Inbound);
    }

    #[test]
    fn parse_on_node_outbound() {
        let yaml = r#"
version: 1
rules:
  - on_node:
      name: srv-b
      direction: outbound
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("outbound should parse");
        assert_eq!(one_on_node(&rules).1, Direction::Outbound);
    }

    #[test]
    fn parse_on_node_interface_narrows_match() {
        let yaml = r#"
version: 1
rules:
  - on_node:
      name: srv-b
      direction: inbound
      interface: "wasi:*"
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("interface should parse");
        let (_, _, interface, _) = one_on_node(&rules);
        assert_eq!(interface.raw(), &["wasi:*".to_string()]);
        assert!(interface.is_match("wasi:http/handler@0.3.0"));
        assert!(!interface.is_match("my:srv/api@1.0.0"));
    }

    #[test]
    fn parse_on_node_name_accepts_glob() {
        let yaml = r#"
version: 1
rules:
  - on_node:
      name: "srv-*"
      direction: inbound
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("glob name should parse");
        let (name, _, _, _) = one_on_node(&rules);
        assert_eq!(name.raw(), &["srv-*".to_string()]);
        assert!(name.is_match("srv-x"));
        assert!(!name.is_match("auth"));
    }

    #[test]
    fn validate_on_node_empty_name_rejected() {
        assert_err(
            r#"
version: 1
rules:
  - on_node:
      name: ""
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "(on_node): 'name' must not be empty",
        );
    }

    #[test]
    fn validate_on_node_unknown_direction_rejected() {
        // serde catches the bad keyword at deserialize time.
        let yaml = r#"
version: 1
rules:
  - on_node:
      name: srv-b
      direction: sideways
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        assert!(parse_yaml(yaml).is_err(), "bogus direction should error");
    }

    #[test]
    fn validate_on_node_with_before_rejected() {
        assert_err(
            r#"
version: 1
rules:
  - before:
      interface: "*"
    on_node:
      name: srv-b
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "may specify only one strategy",
        );
    }

    #[test]
    fn parse_on_node_alias_propagates_to_both_directions() {
        // alias becomes `provider_alias` on the inbound Before AND
        // `outer_alias` on the outbound Between, so the rename is
        // consistent across both desugared rules.
        let yaml = r#"
version: 1
rules:
  - on_node:
      name: srv-b
      alias: renamed-b
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("alias should parse");
        assert_eq!(one_on_node(&rules).3, Some("renamed-b"));
    }

    #[test]
    fn parse_on_node_all_funcs() {
        // all-funcs is interface-scoped; resolved rules inherit it
        // uniformly (verified in the resolve tests).
        let yaml = r#"
version: 1
rules:
  - on_node:
      name: srv-b
      all-funcs:
        async: true
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("all-funcs should parse");
        assert_eq!(rules.len(), 1);
        let SpliceRule::OnNode { all_funcs, .. } = &rules[0] else {
            panic!("expected OnNode");
        };
        let pred = all_funcs.as_ref().expect("predicate present");
        assert_eq!(pred.is_async, Some(true));
    }

    #[test]
    fn validate_on_node_bad_filter_glob() {
        // `[` is in the char-safety allowlist but doesn't compile as a
        // glob; the per-pattern glob compile catches it.
        assert_err(
            r#"
version: 1
rules:
  - on_node:
      name: srv-b
      interface: "wasi:["
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "invalid glob pattern",
        );
    }

    // -----------------------------------------------------------------------
    // on_subgraph selector
    // -----------------------------------------------------------------------

    #[test]
    fn parse_on_subgraph_produces_unresolved_variant() {
        // `on_subgraph` is graph-dependent; parse produces the
        // OnSubgraph variant for resolve_rules to expand later.
        let yaml = r#"
version: 1
rules:
  - on_subgraph:
      nodes: [A, B, C]
      direction: inbound
      interface: "wasi:*"
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("on_subgraph should parse");
        assert_eq!(rules.len(), 1);
        let SpliceRule::OnSubgraph {
            nodes,
            direction,
            interface,
            ..
        } = &rules[0]
        else {
            panic!("expected OnSubgraph variant");
        };
        assert_eq!(nodes, &["A", "B", "C"]);
        assert_eq!(*direction, Direction::Inbound);
        assert!(interface.is_match("wasi:http/handler@0.3.0"));
        assert!(!interface.is_match("my:srv/api@1.0.0"));
    }

    #[test]
    fn parse_on_subgraph_default_direction_is_both() {
        let yaml = r#"
version: 1
rules:
  - on_subgraph:
      nodes: [A]
    inject:
      - name: mw
        path: ./mw.wasm
"#;
        let rules = parse_yaml(yaml).expect("default direction should parse");
        let SpliceRule::OnSubgraph { direction, .. } = &rules[0] else {
            panic!("expected OnSubgraph");
        };
        assert_eq!(*direction, Direction::Both);
    }

    #[test]
    fn validate_on_subgraph_empty_nodes_rejected() {
        assert_err(
            r#"
version: 1
rules:
  - on_subgraph:
      nodes: []
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "must list at least one entry",
        );
    }

    #[test]
    fn validate_on_subgraph_empty_node_name_rejected() {
        assert_err(
            r#"
version: 1
rules:
  - on_subgraph:
      nodes: [A, "", B]
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "'nodes' entries must not be empty",
        );
    }

    #[test]
    fn validate_on_subgraph_duplicate_nodes_rejected() {
        assert_err(
            r#"
version: 1
rules:
  - on_subgraph:
      nodes: [A, B, A]
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "'nodes' contains duplicate 'A'",
        );
    }

    #[test]
    fn validate_on_subgraph_with_on_node_rejected() {
        assert_err(
            r#"
version: 1
rules:
  - on_node:
      name: X
    on_subgraph:
      nodes: [A, B]
    inject:
      - name: mw
        path: ./mw.wasm
"#,
            "may specify only one strategy",
        );
    }
}
