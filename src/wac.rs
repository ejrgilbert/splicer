use crate::adapter::{generate_tier1_adapter, generate_tier2_adapter};
use crate::contract::{validate_contract, ContractResult};
use colored::Colorize;
use cviz::model::{ComponentNode, CompositionGraph, ExportInfo, InterfaceConnection};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use wasmparser::collections::IndexSet;

/// Package prefix used for WAC instance variables (e.g. `"my:srv-a"`).
pub const INST_PREFIX: &str = "my";
const PATH_PLACEHOLDER: &str = "/path/to/comp.wasm";

/// Reserved key splicer publishes into every builtin that imports
/// `splicer:builtin-config`. The single-underscore prefix marks it as
/// splicer-internal — user manifests can never declare keys with this
/// prefix.
pub const EDGE_ID_CONFIG_KEY: &str = "_splicer_edge_id";

/// Canonical edge_id for an `interface` edge from `from` (the caller)
/// to `to` (the provider). `from = None` is the boundary case — the
/// caller is external to the composition; the rendered form drops the
/// caller segment, leaving the leading `->` as a marker. Format:
/// `{interface}::{caller}->{provider}`, or `{interface}::->{provider}`
/// at the boundary.
pub fn derive_edge_id(interface: &str, from: Option<&str>, to: &str) -> String {
    match from {
        Some(caller) => format!("{interface}::{caller}->{to}"),
        None => format!("{interface}::->{to}"),
    }
}
use crate::parse::config::{AdapterInjectionInfo, Injection, SpliceRule};
use crate::split::gen_split_path;
use anyhow::Context;

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
        let split_to_use = resolved_split_num(consumer_id, composition, shim_comps);
        Some(gen_split_path(splits_path, split_to_use))
    }
}

#[derive(Clone, Debug)]
struct Contract {
    name: String,
    ty_fingerprint: Option<String>,
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
    /// Hook interfaces the wrapped middleware exports
    /// (e.g. `"splicer:tier1/before"` or `"splicer:tier2/before"`).
    /// Unversioned; the version is derived from the package prefix
    /// at WAC-generation time.
    pub matched_hook_interfaces: Vec<String>,
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
    /// True iff at least one rule produced a full match. `false` when
    /// no rules were supplied or every rule was a no-op.
    pub any_rule_matched: bool,
}

/// One `let` declaration in the rendered WAC. Each entity is a node
/// instance, a middleware instance, a tier-1 adapter wrapper, or a
/// builtin-config provider — all share the same shape.
#[derive(Debug, Clone)]
struct Entity {
    /// The identifier introduced by `let var = ...`.
    var: String,
    /// The package name in `new my:pkg`.
    pkg: String,
    /// Explicit imports to wire as `"iface": src["iface"],`. Order is
    /// preserved for deterministic rendering.
    imports: Vec<(String, String)>,
    /// Whether to append `...` so wac compose pulls remaining imports
    /// from the host or matching peers.
    catchall: bool,
}

impl Entity {
    /// Catchall-only entity: `let var = new my:pkg { ... };`. No
    /// explicit imports.
    fn leaf(var: impl Into<String>, pkg: impl Into<String>) -> Self {
        Self {
            var: var.into(),
            pkg: pkg.into(),
            imports: vec![],
            catchall: true,
        }
    }

    /// Catchall entity with explicit import wirings:
    /// `let var = new my:pkg { "iface": src["iface"], ..., ... };`.
    fn wired(
        var: impl Into<String>,
        pkg: impl Into<String>,
        imports: Vec<(String, String)>,
    ) -> Self {
        Self {
            var: var.into(),
            pkg: pkg.into(),
            imports,
            catchall: true,
        }
    }

    /// Vars this entity references on its right-hand side. Used by the
    /// renderer's topological sort.
    fn deps(&self) -> impl Iterator<Item = &str> {
        self.imports.iter().map(|(_, v)| v.as_str())
    }

    /// Render this entity as a single `let var = new my:pkg { ... };`
    /// declaration. No trailing newline.
    fn render(&self) -> String {
        if self.imports.is_empty() && !self.catchall {
            return format!("let {} = new {INST_PREFIX}:{} {{}};", self.var, self.pkg);
        }
        let mut s = format!("let {} = new {INST_PREFIX}:{} {{", self.var, self.pkg);
        for (iface, src) in &self.imports {
            s.push_str(&format!("\n    \"{iface}\": {src}[\"{iface}\"],"));
        }
        if self.catchall {
            s.push_str("\n    ...");
        }
        s.push_str("\n};");
        s
    }
}

/// The plan IR — an ordered set of WAC entities + exports plus the
/// routing decisions that produced them. Built up by a sequence of
/// [`EmitPlan`] transformations; rendered by [`EmitPlan::render`].
#[derive(Debug, Default)]
struct EmitPlan {
    // ── Output (consumed by `render`) ──
    /// All entities to emit, keyed by their var name.
    entities: BTreeMap<String, Entity>,
    /// `(export_iface, source_var)` to emit `export src_var["iface"];` for.
    exports: Vec<(String, String)>,
    /// Composition node id → pkg name (drives wac_deps for the original
    /// component splits).
    used_comp_nodes: HashMap<u32, String>,
    /// `pkg → path` for middleware/adapter/config-provider components
    /// (drives wac_deps for injected components). Keyed by pkg so
    /// multiple instances of the same middleware (e.g. `middleware-a`,
    /// `middleware-a-1`) share one dep entry; `BTreeMap` keeps the
    /// emitted command line deterministic.
    used_middlewares: BTreeMap<String, String>,

    // ── Planning state (carried between pipeline steps) ──
    /// First non-`None` alias seen per node id. Drives the var assigned
    /// to that node. Subsequent differing aliases for the same id are
    /// silently ignored on the assumption that `add_to_inject_plan`
    /// has already rejected inter-rule alias conflicts; the order
    /// chosen here is "first chain to mention the node wins".
    aliases: HashMap<u32, Option<String>>,
    /// Composition node id → wac var. Multiple ids may share a var when
    /// they resolve to the same split.
    node_vars: HashMap<u32, String>,
    /// `(consumer_id, iface)` → source var that should provide that
    /// import after middleware routing. `consumer_id` is shim-resolved
    /// so it matches the `with_exports` lookup convention.
    routing: HashMap<(u32, String), String>,
    /// `(provider_id, export_iface)` → wrapped var, populated when a
    /// top-level wrap fronts the provider's export of that iface.
    /// `provider_id` is shim-resolved.
    export_routing: HashMap<(u32, String), String>,
    /// Use-count per simple-middleware name. First use keeps the bare
    /// `mdl.name`; subsequent uses suffix `-1`, `-2`, … so each
    /// position gets its own instance (with its own state).
    simple_mdl_counts: HashMap<String, usize>,
    /// Use-count per tier-1 adapter `(mdl, iface)` pair. Same scheme as
    /// [`Self::simple_mdl_counts`] — distinct downstream wirings get
    /// distinct instances.
    adapter_counts: HashMap<String, usize>,
    /// Real-middleware vars already added (dedup tier-1 across rules).
    emitted_real_vars: HashSet<String>,
}

impl EmitPlan {
    fn new() -> Self {
        Self::default()
    }

    /// Pipeline step: collect the first alias each node receives across
    /// all chains. `None` and missing both mean "use the sanitized node
    /// name"; `Some(name)` overrides it.
    fn with_aliases(mut self, chains: &[Chain]) -> Self {
        for chain in chains {
            for (id, alias) in &chain.aliases {
                self.aliases.entry(*id).or_insert_with(|| alias.clone());
            }
        }
        self
    }

    /// Pipeline step: assign one wac var per chain-participating node.
    /// Nodes resolving to the same split share a var (one wac instance
    /// per physical component), preserving resource-type identity.
    /// Nodes outside every chain are skipped — wac compose's
    /// peer-level flattening can surface auxiliary nodes whose
    /// sanitized names would collide with chain participants.
    fn with_node_vars(
        mut self,
        composition: &CompositionGraph,
        chains: &[Chain],
        shim_comps: &HashMap<usize, usize>,
    ) -> Self {
        let mut chain_nodes: BTreeSet<u32> = BTreeSet::new();
        for chain in chains {
            for id in &chain.chain {
                chain_nodes.insert(*id);
            }
        }
        let mut split_to_var: HashMap<usize, String> = HashMap::new();
        for id in &chain_nodes {
            let resolved_split = resolved_split_num(*id, composition, shim_comps);
            if let Some(existing) = split_to_var.get(&resolved_split) {
                self.node_vars.insert(*id, existing.clone());
                continue;
            }
            let pkg = match self.aliases.get(id) {
                Some(Some(a)) => a.clone(),
                _ => sanitize_wac_id(get_name(&composition.nodes[id])),
            };
            self.node_vars.insert(*id, pkg.clone());
            split_to_var.insert(resolved_split, pkg);
        }
        self
    }

    /// Pipeline step: walk each chain's `inject_plan`, materializing
    /// middleware entities and recording the source var that each
    /// `(consumer, iface)` import should be wired from. Top-level
    /// wraps (chain_idx == chain.len()) are recorded into
    /// [`Self::export_routing`] to front the consumer's export later.
    ///
    /// Middlewares within one slot are applied in reverse declaration
    /// order so the first entry in the YAML's `inject:` list ends up
    /// the outermost wrapper (called first at runtime).
    fn with_chain_routing(
        mut self,
        chains: &[Chain],
        composition: &CompositionGraph,
        shim_comps: &HashMap<usize, usize>,
    ) -> anyhow::Result<Self> {
        for Chain {
            interface,
            chain,
            inject_plan,
            ..
        } in chains
        {
            let mut prev_var: Option<String> = None;
            for i in 0..chain.len() {
                let id = chain[i];
                let node_var = self.node_vars.get(&id).cloned().ok_or_else(|| {
                    anyhow::anyhow!(
                        "with_chain_routing: chain participant {id} has no var; \
                         with_node_vars must run before with_chain_routing"
                    )
                })?;
                // Routing keys store the shim-resolved id so the
                // `with_exports` lookup matches; otherwise an export
                // sourced from a shim wouldn't find the wiring stored
                // under the raw chain id.
                let routing_id = resolve_shim_node(id, composition, shim_comps);

                if i > 0 {
                    let upstream = prev_var.clone().ok_or_else(|| {
                        anyhow::anyhow!(
                            "with_chain_routing: chain position {i} has no upstream var; \
                             chain construction produced an inconsistent ordering"
                        )
                    })?;
                    let current = self.fold_inject_middlewares(
                        upstream,
                        inject_plan.get(&i),
                        interface,
                        composition,
                        shim_comps,
                    )?;
                    self.routing
                        .insert((routing_id, interface.name.clone()), current);
                }
                // The current node consumes from the wrapped source above
                // and re-provides the iface through itself; the next
                // iteration's source is the node, not the wrapped input.
                prev_var = Some(node_var.clone());

                if i == chain.len() - 1 {
                    if let Some(top) = inject_plan.get(&(i + 1)) {
                        if !top.is_empty() {
                            let current = self.fold_inject_middlewares(
                                node_var.clone(),
                                Some(top),
                                interface,
                                composition,
                                shim_comps,
                            )?;
                            self.export_routing
                                .insert((routing_id, interface.name.clone()), current);
                        }
                    }
                }
            }
        }
        Ok(self)
    }

    /// Pipeline step: build composition-node entities. Each
    /// non-host import is wired from [`Self::routing`] when a chain
    /// covered it, or from the source's node var as a fallback.
    /// Split-deduped node ids share an entity (the first id wins).
    ///
    /// Routing lookups normalize the consumer id with `resolve_shim_node`
    /// to match the convention `with_chain_routing` uses when storing.
    ///
    /// Bails if a node's sanitized name collides with a previously
    /// added middleware/adapter/config-provider entity — silently
    /// overwriting would lose the middleware's wiring.
    fn with_node_entities(
        mut self,
        composition: &CompositionGraph,
        shim_comps: &HashMap<usize, usize>,
    ) -> anyhow::Result<Self> {
        let mut emitted: HashSet<String> = HashSet::new();
        let mut node_ids: Vec<u32> = self.node_vars.keys().copied().collect();
        node_ids.sort();
        for id in node_ids {
            let var = self.node_vars[&id].clone();
            if !emitted.insert(var.clone()) {
                continue;
            }
            // Node names are derived from composition input; middleware
            // names from rule YAML. A clash means user-visible config is
            // ambiguous — fail loudly with both names so the user can
            // alias one side.
            if self.entities.contains_key(&var) {
                anyhow::bail!(
                    "WAC var name collision: composition node `{var}` \
                     conflicts with a previously-emitted middleware entity \
                     (rename the node alias or the middleware's `name:`)"
                );
            }
            let node = &composition.nodes[&id];
            let routing_id = resolve_shim_node(id, composition, shim_comps);
            let mut imports: Vec<(String, String)> = Vec::new();
            for conn in &node.imports {
                if conn.is_host_import {
                    continue;
                }
                let iface = &conn.interface_name;
                let src_var = if let Some(routed) = self.routing.get(&(routing_id, iface.clone())) {
                    routed.clone()
                } else if let Some(src_id) = conn.source_instance {
                    match self.node_vars.get(&src_id) {
                        Some(v) => v.clone(),
                        None => continue,
                    }
                } else {
                    continue;
                };
                imports.push((iface.clone(), src_var));
            }
            self.entities.insert(
                var.clone(),
                Entity::wired(var.clone(), var.clone(), imports),
            );
            self.used_comp_nodes.insert(id, var);
        }
        Ok(self)
    }

    /// Pipeline step: build the `export` list from
    /// `composition.component_exports`. Skips spurious shim re-exports
    /// (an iface routed internally whose export source is never a
    /// chain consumer) and routes through any top-level-wrap
    /// middleware recorded in [`Self::export_routing`].
    ///
    /// Both `chain_consumers` membership and `export_routing` lookups
    /// use shim-resolved ids so the export source matches whatever
    /// `with_chain_routing` stored, even when the chain participant
    /// and the export source are distinct shim aliases of the same
    /// underlying instance.
    fn with_exports(
        mut self,
        composition: &CompositionGraph,
        chains: &[Chain],
        handled_interfaces: &HashSet<String>,
        shim_comps: &HashMap<usize, usize>,
    ) -> Self {
        let mut chain_consumers: HashSet<u32> = HashSet::new();
        for chain in chains {
            if let Some(last) = chain.chain.last() {
                chain_consumers.insert(resolve_shim_node(*last, composition, shim_comps));
            }
        }
        for (export_iface, info) in composition.component_exports.iter() {
            let effective_id = resolve_shim_node(info.source_instance, composition, shim_comps);
            if handled_interfaces.contains(export_iface) && !chain_consumers.contains(&effective_id)
            {
                continue;
            }
            let export_var = if let Some(wrapped) = self
                .export_routing
                .get(&(effective_id, export_iface.clone()))
            {
                wrapped.clone()
            } else if let Some(v) = self.node_vars.get(&effective_id) {
                v.clone()
            } else {
                continue;
            };
            self.exports.push((export_iface.clone(), export_var));
        }
        self
    }

    /// Wrap `initial` with each middleware in `middlewares` in reverse
    /// declaration order, threading the result of one wrap into the
    /// downstream slot of the next. Returns the outermost var (or
    /// `initial` unchanged if `middlewares` is `None` or empty).
    ///
    /// Reverse order preserves declared ordering: the first middleware
    /// in the config ends up the outermost wrapper, which is what
    /// callers want.
    fn fold_inject_middlewares(
        &mut self,
        initial: String,
        middlewares: Option<&IndexSet<Injection>>,
        interface: &Contract,
        composition: &CompositionGraph,
        shim_comps: &HashMap<usize, usize>,
    ) -> anyhow::Result<String> {
        let Some(middlewares) = middlewares else {
            return Ok(initial);
        };
        // IndexSet's iter doesn't impl DoubleEndedIterator, so the
        // .collect() is required for `.rev()`.
        let ordered: Vec<&Injection> = middlewares.iter().collect();
        let mut current = initial;
        for mdl in ordered.iter().rev() {
            current = self.add_middleware(mdl, interface, &current, composition, shim_comps)?;
        }
        Ok(current)
    }

    /// Materialize entities for a single middleware injection and
    /// return the var to use as the wrapped iface's new source.
    ///
    /// Tier-1 adapters expand to up to three entities (real middleware
    /// host-imports-only + optional config provider + adapter wrapper);
    /// simple middlewares are one entity. Both flavors disambiguate
    /// repeated uses by appending `-N` to the WAC `var`, while keeping
    /// the WAC `pkg` constant (one wac_deps entry, multiple
    /// instantiations).
    fn add_middleware(
        &mut self,
        mdl: &Injection,
        interface: &Contract,
        downstream_var: &str,
        composition: &CompositionGraph,
        shim_comps: &HashMap<usize, usize>,
    ) -> anyhow::Result<String> {
        use crate::contract::{
            versioned_interface, TIER1_PACKAGE, TIER1_VERSION, TIER2_PACKAGE, TIER2_VERSION,
        };

        let real_pkg = mdl.name.clone();
        let mdl_path = mdl
            .path
            .as_ref()
            .cloned()
            .unwrap_or_else(|| PATH_PLACEHOLDER.to_string());

        if let Some(adapter_info) = &mdl.adapter_info {
            let adapter_pkg = format!("{}-adapter-{}", mdl.name, sanitize_wac_id(&interface.name));
            let cfg_pkg = mdl
                .config_provider_path
                .as_ref()
                .map(|_| format!("{real_pkg}-config"));

            // Real middleware (host-imports-only) is shared across all
            // adapter uses of this `mdl.name`; emit once, dedup on
            // subsequent calls.
            if self.emitted_real_vars.insert(real_pkg.clone()) {
                self.used_middlewares.insert(real_pkg.clone(), mdl_path);
                match cfg_pkg.as_ref() {
                    Some(cfg_pkg) => {
                        self.entities
                            .insert(cfg_pkg.clone(), Entity::leaf(cfg_pkg, cfg_pkg));
                        self.used_middlewares.insert(
                            cfg_pkg.clone(),
                            mdl.config_provider_path.as_ref().unwrap().clone(),
                        );
                        self.entities.insert(
                            real_pkg.clone(),
                            Entity::wired(
                                &real_pkg,
                                &real_pkg,
                                vec![(
                                    crate::config_provider::BUILTIN_CONFIG_GET_VERSIONED
                                        .to_string(),
                                    cfg_pkg.clone(),
                                )],
                            ),
                        );
                    }
                    None => {
                        self.entities
                            .insert(real_pkg.clone(), Entity::leaf(&real_pkg, &real_pkg));
                    }
                }
            }

            // Adapter pkg stays constant (same generated wasm); each
            // chain position gets its own var.
            let adapter_var = disambiguated_var(&mut self.adapter_counts, &adapter_pkg);

            let mut imports: Vec<(String, String)> =
                vec![(interface.name.clone(), downstream_var.to_string())];
            for hook_iface in &adapter_info.matched_hook_interfaces {
                let version = if hook_iface.starts_with(&format!("{TIER1_PACKAGE}/")) {
                    TIER1_VERSION
                } else if hook_iface.starts_with(&format!("{TIER2_PACKAGE}/")) {
                    TIER2_VERSION
                } else {
                    anyhow::bail!(
                        "matched hook interface '{hook_iface}' is not part of any known tier package",
                    );
                };
                imports.push((versioned_interface(hook_iface, version), real_pkg.clone()));
            }
            // Resource-bearing factored-types imports — `...` doesn't
            // unify resource type identity across separately-imported
            // instances from a non-host component. The adapter wasm
            // was just generated by this same splice run; if it's
            // unreadable, that's a bug worth surfacing.
            let adapter_bytes = std::fs::read(&adapter_info.adapter_path).map_err(|e| {
                anyhow::anyhow!(
                    "failed to read freshly-generated adapter wasm at `{}`: {e}",
                    adapter_info.adapter_path,
                )
            })?;
            for extra in factored_types_to_wire(
                &resource_bearing_imports(&adapter_bytes),
                &interface.name,
                composition,
                shim_comps,
            )? {
                imports.push((extra, downstream_var.to_string()));
            }
            self.entities.insert(
                adapter_var.clone(),
                Entity::wired(&adapter_var, &adapter_pkg, imports),
            );
            self.used_middlewares
                .insert(adapter_pkg, adapter_info.adapter_path.clone());

            Ok(adapter_var)
        } else {
            // Simple middleware: each chain position needs its own
            // instance (independent state); pkg stays = `mdl.name`.
            let mw_var = disambiguated_var(&mut self.simple_mdl_counts, &real_pkg);
            // Tier-4 (virtualize) middlewares don't import the target
            // (they replace the downstream instead of forwarding); the
            // wac wire must be skipped for those.
            let imports = if mdl.tier.is_some_and(|t| !t.imports_target()) {
                Vec::new()
            } else {
                vec![(interface.name.clone(), downstream_var.to_string())]
            };
            self.entities
                .insert(mw_var.clone(), Entity::wired(&mw_var, &real_pkg, imports));
            self.used_middlewares.insert(real_pkg, mdl_path);
            Ok(mw_var)
        }
    }

    /// Render the plan to WAC text. Entities are emitted in topological
    /// order so every reference resolves to a name introduced earlier
    /// in the file (wac compose rejects forward references). Cycles
    /// bail loudly with the offending var names.
    fn render(&self, pkg_name: &str) -> anyhow::Result<String> {
        let mut lines = vec![format!("package {pkg_name};")];
        for var in self.topo_sort()? {
            lines.push(self.entities[&var].render());
        }
        for (iface, src) in &self.exports {
            lines.push(format!("export {src}[\"{iface}\"];"));
        }
        Ok(lines.join("\n\n"))
    }

    /// Kahn's algorithm. External references (vars not in the plan)
    /// are ignored — they're host imports or wac compose's
    /// responsibility to resolve.
    fn topo_sort(&self) -> anyhow::Result<Vec<String>> {
        let mut in_degree: BTreeMap<String, usize> = BTreeMap::new();
        let mut dependents: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for var in self.entities.keys() {
            in_degree.insert(var.clone(), 0);
        }
        for (var, entity) in &self.entities {
            let mut seen: HashSet<&str> = HashSet::new();
            for dep in entity.deps() {
                if dep == var.as_str() || !self.entities.contains_key(dep) || !seen.insert(dep) {
                    continue;
                }
                *in_degree.get_mut(var).unwrap() += 1;
                dependents
                    .entry(dep.to_string())
                    .or_default()
                    .push(var.clone());
            }
        }

        let mut queue: std::collections::VecDeque<String> = in_degree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(v, _)| v.clone())
            .collect();
        let mut order: Vec<String> = Vec::with_capacity(self.entities.len());
        while let Some(var) = queue.pop_front() {
            order.push(var.clone());
            if let Some(deps_of) = dependents.get(&var) {
                for dep in deps_of {
                    let d = in_degree.get_mut(dep).unwrap();
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(dep.clone());
                    }
                }
            }
        }

        if order.len() != self.entities.len() {
            let placed: HashSet<&str> = order.iter().map(String::as_str).collect();
            let mut cyclic: Vec<&str> = self
                .entities
                .keys()
                .map(String::as_str)
                .filter(|v| !placed.contains(v))
                .collect();
            cyclic.sort();
            anyhow::bail!(
                "WAC plan has dependency cycles among: [{}]",
                cyclic.join(", ")
            );
        }
        Ok(order)
    }
}

/// Read-only state shared across every rule + chain pass of a single
/// `generate_wac` run.
struct SpliceCtx<'a> {
    composition: &'a CompositionGraph,
    splits_path: &'a str,
    shim_comps: &'a HashMap<usize, usize>,
}

/// Mutable per-run state accumulated as rules apply: contract-export
/// memos, generated adapters, and decode-result caches for the
/// sync-target / async-middleware preflight (so the same `.wasm` is
/// only decoded once per `generate_wac` run, not per injection).
#[derive(Default)]
struct SpliceAccumulators {
    checked_middlewares: HashMap<String, BTreeMap<String, ExportInfo>>,
    generated_adapters: Vec<GeneratedAdapter>,
    /// Decode-cache: `(target_split_path, target_interface)` → has
    /// at least one sync (non-async) function declared on that
    /// interface.
    target_has_sync_cache: HashMap<(String, String), bool>,
    /// Decode-cache: `middleware_path` → qualified name of the first
    /// non-`wasi:*` peer import declared `async func`, or `None` if
    /// no offender.
    middleware_first_async_peer_cache: HashMap<String, Option<String>>,
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
    log_shim_resolutions(&shim_comps);

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
            ..
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
            ..
        },
    ) in composition.component_exports.iter()
    {
        if handled_interfaces.contains(interface) {
            continue;
        }
        // if we've reached this point, it's guaranteed to not be a chain (chains were handled above)
        // this is just a single exported service func.
        chains.push(Chain {
            interface: Contract {
                name: interface.to_string(),
                ty_fingerprint: fingerprint.clone(),
            },
            chain: vec![*source_inst],
            aliases: HashMap::new(),
            inject_plan: HashMap::new(),
        });
    }

    let ctx = SpliceCtx {
        composition,
        splits_path,
        shim_comps: &shim_comps,
    };
    let mut accs = SpliceAccumulators::default();

    // Apply the rules in order of their declaration in the configuration.
    // This enforces an ordering semantic for the rule application.
    let mut diagnostics: Vec<ContractResult> = vec![];
    let mut any_rule_matched = false;
    for (rule_idx, rule) in rules.iter().enumerate() {
        let mut any_interface_matched = false;
        let mut any_full_match = false;
        for chain in chains.iter_mut() {
            let between = apply_rule_between(rule, chain, &ctx, &mut accs)?;
            let before = apply_rule_before(rule, chain, &ctx, &mut accs)?;
            any_interface_matched |= between.interface_matched | before.interface_matched;
            any_full_match |= between.full_match | before.full_match;
            diagnostics.extend(between.contract_results);
            diagnostics.extend(before.contract_results);
        }
        any_rule_matched |= any_full_match;
        if !any_full_match {
            let iface = rule.interface();
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

    // Plan + render. The plan accumulates entities and routing data
    // without committing to an order; the renderer topo-sorts and emits
    // `let` lines in dependency order. This is what lets a "middle"
    // node (provider on one boundary, consumer on another) receive the
    // right wiring regardless of which chain visits it first.
    let plan = EmitPlan::new()
        .with_aliases(&chains)
        .with_node_vars(composition, &chains, &shim_comps)
        .with_chain_routing(&chains, composition, &shim_comps)?
        .with_node_entities(composition, &shim_comps)?
        .with_exports(composition, &chains, &handled_interfaces, &shim_comps);
    let wac = plan.render(pkg_name)?;
    let args = gen_wac_args(
        shim_comps,
        splits_path,
        composition,
        &plan.used_comp_nodes,
        &plan.used_middlewares,
        node_paths,
    );

    Ok(WacOutput {
        wac,
        wac_deps: args,
        diagnostics,
        generated_adapters: accs.generated_adapters,
        any_rule_matched,
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
    used_mdls: &BTreeMap<String, String>,
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
            let split_to_use = resolved_split_num(*inst_id, graph, &shim_comps);
            PathBuf::from(gen_split_path(splits_path, split_to_use))
        };
        deps.insert(format!("{INST_PREFIX}:{name}"), comp_path);
    }

    for (mw_name, mw_path) in used_mdls {
        deps.insert(format!("{INST_PREFIX}:{mw_name}"), PathBuf::from(mw_path));
    }

    deps
}

/// Pure: follow the shim chain until landing on a non-shim split.
/// See [`log_shim_resolutions`] for the debug-level notice that
/// fires once per non-trivial resolution at the top of [`generate_wac`].
fn resolve_shim(mut component_num: usize, shim_comps: &HashMap<usize, usize>) -> usize {
    while is_shim_split_num(component_num, shim_comps) {
        component_num = shim_comps[&component_num];
    }
    component_num
}

/// Convert a graph node id to its split number (split0 is the root;
/// nodes are offset by -1 in the split keyspace).
fn node_split_num(node_id: u32, composition: &CompositionGraph) -> usize {
    (composition.nodes[&node_id].component_num + 1) as usize
}

/// Resolve a graph node id to the split number of its non-shim outer.
/// Composes [`node_split_num`] + [`resolve_shim`].
fn resolved_split_num(
    node_id: u32,
    composition: &CompositionGraph,
    shim_comps: &HashMap<usize, usize>,
) -> usize {
    resolve_shim(node_split_num(node_id, composition), shim_comps)
}

/// Emit one debug-level note per non-trivial `shim → resolved` mapping in
/// `shim_comps`. Called once at the start of [`generate_wac`] so the
/// same assumption isn't announced twice when both the adapter-gen and
/// wac-dep paths later call [`resolve_shim`] for the same shim.
///
/// Hidden behind `RUST_LOG` (default off) since the heuristic is reliable
/// in the common case; opt in via `RUST_LOG=splicer=debug` to see it.
fn log_shim_resolutions(shim_comps: &HashMap<usize, usize>) {
    let mut shim_keys: Vec<usize> = shim_comps.keys().copied().collect();
    shim_keys.sort();
    for shim_num in shim_keys {
        let resolved = resolve_shim(shim_num, shim_comps);
        if resolved != shim_num {
            tracing::debug!(
                "Assumption made: split{shim_num} appears to be a shim component, \
                 defaulting to split{resolved} in the generated wac command. \
                 If this is incorrect, modify the generated wac command."
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

/// For injections whose builtin imports `splicer:builtin-config`,
/// clone each with an edge-uniquified `name` and emit a per-edge
/// `<name>-config.wasm` stamped with the resolved `edge_id`.
/// Injections without a config provider pass through unchanged.
/// Callers feed the returned vec to `add_to_inject_plan` so each
/// physical edge gets its own WAC instance + provider.
fn build_per_edge_providers(
    inject: &[Injection],
    edge_id: &str,
    splits_path: &str,
) -> anyhow::Result<Vec<Injection>> {
    let splits_dir = std::path::Path::new(splits_path);
    // Suffix flows into `my:<name>` package keys, so it must be a
    // valid WAC identifier
    let edge_suffix = sanitize_wac_id(edge_id);
    inject
        .iter()
        .map(|inj| {
            if inj.config_as_wave.is_none() {
                return Ok(inj.clone());
            }
            let mut clone = inj.clone();
            clone.name = format!("{}-{edge_suffix}", inj.name);
            crate::config_provider::build_provider_for_edge(&mut clone, edge_id, splits_dir)?;
            Ok(clone)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn apply_rule_between(
    rule: &SpliceRule,
    chain: &mut Chain,
    ctx: &SpliceCtx,
    accs: &mut SpliceAccumulators,
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
            let inner_node = &ctx.composition.nodes[&inner_id];
            let outer_node = &ctx.composition.nodes[&outer_id];

            let inner_var = get_name(inner_node).to_string();
            let outer_var = get_name(outer_node).to_string();
            if *interface != chain.interface.name {
                continue;
            }
            interface_matched = true;
            if *inner_name == inner_var && *outer_name == outer_var {
                full_match = true;
                let edge_id = derive_edge_id(interface, Some(&outer_var), &inner_var);
                let edge_inject = build_per_edge_providers(inject, &edge_id, ctx.splits_path)?;
                let new_aliases = vec![
                    (inner_id, inner_alias.clone()),
                    (outer_id, outer_alias.clone()),
                ];
                let consumer_path = chain.consumer_split_path(
                    i + 1,
                    ctx.composition,
                    ctx.splits_path,
                    ctx.shim_comps,
                );
                contract_results.extend(add_to_inject_plan(
                    interface,
                    &edge_inject,
                    i + 1,
                    &new_aliases,
                    &mut chain.aliases,
                    &mut chain.inject_plan,
                    &chain.interface.ty_fingerprint,
                    consumer_path,
                    ctx,
                    accs,
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

fn apply_rule_before(
    rule: &SpliceRule,
    chain: &mut Chain,
    ctx: &SpliceCtx,
    accs: &mut SpliceAccumulators,
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
            let outer_node = &ctx.composition.nodes[id];
            if let Some(provider) = provider_name {
                if get_name(outer_node) != *provider {
                    continue;
                }
            }
            full_match = true;
            let provider_var = get_name(outer_node).to_string();
            // Caller name comes from the next position toward the
            // consumer side of the chain; `None` when this match is
            // the outermost chain position (boundary edge — caller
            // is external to the composition).
            let caller_var = chain
                .chain
                .get(i + 1)
                .map(|caller_id| get_name(&ctx.composition.nodes[caller_id]).to_string());
            let edge_id = derive_edge_id(interface, caller_var.as_deref(), &provider_var);
            let edge_inject = build_per_edge_providers(inject, &edge_id, ctx.splits_path)?;
            let new_aliases = vec![(*id, provider_alias.clone())];
            // Prefer the consumer's split (i+1) so the adapter copies
            // its import surface. At the outermost chain position
            // there's no consumer, so fall back to the provider's own
            // split (i) — the adapter mirrors the provider's full
            // import topology.
            let consumer_path = chain
                .consumer_split_path(i + 1, ctx.composition, ctx.splits_path, ctx.shim_comps)
                .or_else(|| {
                    chain.consumer_split_path(i, ctx.composition, ctx.splits_path, ctx.shim_comps)
                });
            contract_results.extend(add_to_inject_plan(
                interface,
                &edge_inject,
                i + 1,
                &new_aliases,
                &mut chain.aliases,
                &mut chain.inject_plan,
                &chain.interface.ty_fingerprint,
                consumer_path,
                ctx,
                accs,
            )?);
        }
    }
    Ok(RuleApplyResult {
        contract_results,
        interface_matched,
        full_match,
    })
}

/// Codegen+build any tier-3/4 builtins in `to_inject`, stamping the
/// produced wrapper path on a clone. Non-tier-3/4 entries pass through.
fn materialize_tier3_4_inline(
    interface_name: &str,
    to_inject: &[Injection],
    consumer_split: Option<&str>,
    ctx: &SpliceCtx,
) -> anyhow::Result<Vec<Injection>> {
    let has_tier3_4 = to_inject.iter().any(|inj| {
        inj.builtin
            .as_deref()
            .map(crate::builtins::typed::is_typed)
            .unwrap_or(false)
    });
    if !has_tier3_4 {
        return Ok(to_inject.to_vec());
    }

    let mut out: Vec<Injection> = Vec::with_capacity(to_inject.len());
    for inj in to_inject.iter() {
        let Some(builtin) = inj.builtin.as_deref() else {
            out.push(inj.clone());
            continue;
        };
        if !crate::builtins::typed::is_typed(builtin) {
            out.push(inj.clone());
            continue;
        }
        let split_path = consumer_split.ok_or_else(|| {
            anyhow::anyhow!("no split for tier-3/4 '{builtin}' on '{interface_name}'")
        })?;
        let split_bytes = std::fs::read(split_path)
            .with_context(|| format!("read split for tier-3/4 codegen: {split_path}"))?;
        let (wrapper_path, tier) = crate::builtins::typed::materialize(
            std::path::Path::new(ctx.splits_path),
            builtin,
            &split_bytes,
            interface_name,
        )
        .with_context(|| format!("materialize tier-3/4 '{builtin}' on '{interface_name}'"))?;
        let path_str = wrapper_path
            .to_str()
            .ok_or_else(|| {
                anyhow::anyhow!("wrapper path is not UTF-8: {}", wrapper_path.display())
            })?
            .to_string();
        out.push(Injection {
            path: Some(path_str),
            tier: Some(tier),
            ..inj.clone()
        });
    }
    Ok(out)
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
    consumer_split: Option<String>,
    ctx: &SpliceCtx,
    accs: &mut SpliceAccumulators,
) -> anyhow::Result<Vec<ContractResult>> {
    // Tier-3/4 wrappers ARE the adapter — codegen them here so
    // validate_contract sees their exports.
    let to_inject_materialized =
        materialize_tier3_4_inline(interface_name, to_inject, consumer_split.as_deref(), ctx)?;

    // Check that the import/export contract is upheld by this plan and return results
    // to the caller — logging and error-handling is the caller's responsibility.
    let contract_results = validate_contract(
        &to_inject_materialized,
        interface_name,
        contract_fingerprint,
        &mut accs.checked_middlewares,
    );

    // For tier-1 compatible middleware, generate a adapter component and substitute
    // the injection path so the rest of the WAC generation uses the adapter.
    let mut resolved: Vec<Injection> = Vec::with_capacity(to_inject_materialized.len());
    let mut final_results: Vec<ContractResult> = Vec::with_capacity(contract_results.len());
    for (injection, result) in to_inject_materialized.iter().zip(contract_results) {
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
                if let Some(mw_path) = injection.path.as_deref() {
                    preflight_sync_target_async_middleware(
                        &injection.name,
                        mw_path,
                        interface_name,
                        consumer_split_path,
                        accs,
                    )?;
                }
                let adapter_path = generate_tier1_adapter(
                    &injection.name,
                    interface_name,
                    &matched_interfaces,
                    ctx.splits_path,
                    consumer_split_path,
                )?;
                accs.generated_adapters.push(GeneratedAdapter {
                    adapter_path: adapter_path.clone(),
                    middleware_name: injection.name.clone(),
                    target_interface: interface_name.to_string(),
                    matched_hook_interfaces: matched_interfaces.clone(),
                });
                resolved.push(Injection {
                    adapter_info: Some(AdapterInjectionInfo {
                        adapter_path,
                        matched_hook_interfaces: matched_interfaces,
                    }),
                    ..injection.clone()
                });
                // Tier1Compatible is fully handled here; no diagnostic needed upstream.
            }
            ContractResult::Tier2Compatible(matched_interfaces) => {
                let consumer_split_path = consumer_split.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "No consumer/provider split available for interface '{interface_name}' \
                         while generating tier-2 adapter for middleware '{}'.",
                        injection.name
                    )
                })?;
                if let Some(mw_path) = injection.path.as_deref() {
                    preflight_sync_target_async_middleware(
                        &injection.name,
                        mw_path,
                        interface_name,
                        consumer_split_path,
                        accs,
                    )?;
                }
                let adapter_path = generate_tier2_adapter(
                    &injection.name,
                    interface_name,
                    &matched_interfaces,
                    ctx.splits_path,
                    consumer_split_path,
                )?;
                accs.generated_adapters.push(GeneratedAdapter {
                    adapter_path: adapter_path.clone(),
                    middleware_name: injection.name.clone(),
                    target_interface: interface_name.to_string(),
                    matched_hook_interfaces: matched_interfaces.clone(),
                });
                resolved.push(Injection {
                    adapter_info: Some(AdapterInjectionInfo {
                        adapter_path,
                        matched_hook_interfaces: matched_interfaces,
                    }),
                    ..injection.clone()
                });
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

/// Helper to get the instance name from a node
fn get_name(node: &ComponentNode) -> &str {
    node.display_label()
}

/// Decide which adapter imports to wire as factored-types, given the
/// adapter's resource-bearing imports, the splice target, and the
/// composition graph. Three cases per import:
///   1. **Host-provided** (every import edge is `is_host_import`):
///      skip — `...` resolves it via the runtime's single shared
///      instance.
///   2. **Same provider as the target** (every non-host import edge
///      sources from a node that resolves to the same split as the
///      target's provider): wire from the downstream.
///   3. **Different provider than the target**: multi-provider
///      factored types. Bail — the downstream doesn't actually export
///      this interface, and wiring from a sibling provider needs
///      plumbing this code path doesn't yet have.
fn factored_types_to_wire(
    resource_imports: &[String],
    target_iface: &str,
    composition: &CompositionGraph,
    shim_comps: &HashMap<usize, usize>,
) -> anyhow::Result<Vec<String>> {
    // Resolved-shim source split numbers that provide `iface`:
    //   - Top-level component exports (when `iface` is a leaf export
    //     of the composition, e.g. when splicing a lone provider).
    //   - Plus every non-host import edge sourcing `iface` (when
    //     `iface` is consumed internally, e.g. consumer → provider).
    // Empty set = host-provided.
    let providers = |iface: &str| -> std::collections::HashSet<usize> {
        let mut out = std::collections::HashSet::new();
        if let Some(info) = composition.component_exports.get(iface) {
            out.insert(resolved_split_num(
                info.source_instance,
                composition,
                shim_comps,
            ));
        }
        for node in composition.nodes.values() {
            for conn in &node.imports {
                if conn.interface_name != iface || conn.is_host_import {
                    continue;
                }
                if let Some(src) = conn.source_instance {
                    out.insert(resolved_split_num(src, composition, shim_comps));
                }
            }
        }
        out
    };
    let target_providers = providers(target_iface);
    let mut out = Vec::new();
    for extra in resource_imports {
        if extra == target_iface {
            continue;
        }
        let extra_providers = providers(extra);
        if extra_providers.is_empty() {
            continue; // host-provided
        }
        if extra_providers != target_providers {
            anyhow::bail!(
                "splicer can't yet wire factored-types interface `{extra}` for adapter \
                 on `{target_iface}`: the resource-bearing types interface is exported \
                 by a different component than the target. Splicer's tier-1 wiring \
                 currently assumes both interfaces come from the same provider. \
                 Workaround: have one component export both interfaces."
            );
        }
        out.push(extra.clone());
    }
    Ok(out)
}

/// Preflight: refuse to splice a middleware that imports any
/// `async func` peer-component interface (non-`wasi:*`) onto a target
/// interface that has any `func` (sync at WIT) function. The runtime
/// wedge that combination produces is documented in
/// `docs/TODO/sync-wit-suspend-limit.md`: a sync-WIT-rooted wasm task
/// cannot suspend, so any canon-async wait inside the middleware's
/// hook body — driving an async peer-component import — traps with
/// `cannot block a synchronous task before returning`.
///
/// This is a conservative check: it only catches imports that are
/// `async func` at the middleware's WIT level. A middleware that
/// declares a sync-WIT import but still ends up canon-lower-async'ing
/// it (e.g. via `wit_bindgen::generate!({ async: true })` without
/// per-import filtering) would slip through, but those cases tend to
/// be authored by builtin maintainers who follow the documented
/// pattern. Wasm-bytecode-level scan would be more complete; deferred.
fn preflight_sync_target_async_middleware(
    middleware_name: &str,
    middleware_path: &str,
    target_interface: &str,
    target_split_path: &str,
    accs: &mut SpliceAccumulators,
) -> anyhow::Result<()> {
    let target_key = (target_split_path.to_string(), target_interface.to_string());
    let has_sync = *accs
        .target_has_sync_cache
        .entry(target_key)
        .or_insert_with(|| target_interface_has_sync_func(target_interface, target_split_path));
    if !has_sync {
        return Ok(());
    }
    let offender = accs
        .middleware_first_async_peer_cache
        .entry(middleware_path.to_string())
        .or_insert_with(|| first_async_peer_import(middleware_path))
        .clone();
    let Some(offender) = offender else {
        return Ok(());
    };
    anyhow::bail!(
        "Cannot splice middleware '{middleware_name}' onto SYNC-WIT target \
         interface '{target_interface}': the middleware imports '{offender}', \
         which is `async func` from a peer component. Awaiting a peer-component \
         async call inside a hook body wedges the wasm task at runtime — splicer's \
         generated adapter has to lift `{target_interface}` as sync-WIT (matching \
         the target's contract), and sync-WIT-rooted tasks cannot suspend. \
         Splice this middleware on an `async func` target interface, or rewrite \
         the middleware to read its substrate inline (no `.await`). \
         See docs/TODO/sync-wit-suspend-limit.md.",
    );
}

/// True iff `target_interface` (resolved via the component at
/// `target_split_path`, which imports or exports it) has at least one
/// function declared as `func` (sync) at WIT — i.e. splicer's adapter
/// would be forced to lift that signature as sync.
fn target_interface_has_sync_func(target_interface: &str, target_split_path: &str) -> bool {
    let Ok(bytes) = std::fs::read(target_split_path) else {
        return false;
    };
    let Ok(decoded) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wit_component::decode(&bytes)
    })) else {
        return false;
    };
    let Ok(wit_component::DecodedWasm::Component(resolve, world_id)) = decoded else {
        return false;
    };
    let world = &resolve.worlds[world_id];
    // Look across both imports and exports of the split — splicer
    // calls into either side, depending on direction.
    let surfaces = world.imports.values().chain(world.exports.values());
    for item in surfaces {
        let wit_parser::WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let Some(qname) = resolve.id_of(*id) else {
            continue;
        };
        if qname.split('@').next().unwrap_or(&qname)
            != target_interface
                .split('@')
                .next()
                .unwrap_or(target_interface)
        {
            continue;
        }
        let iface = &resolve.interfaces[*id];
        if iface.functions.values().any(|f| !f.kind.is_async()) {
            return true;
        }
    }
    false
}

/// Return the qualified name of the first import in the middleware's
/// world that is `async func` and belongs to a non-`wasi:*` package
/// (so it's a peer component, not a host import). `None` if no such
/// import exists.
fn first_async_peer_import(middleware_path: &str) -> Option<String> {
    let bytes = std::fs::read(middleware_path).ok()?;
    let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wit_component::decode(&bytes)
    }))
    .ok()?
    .ok()?;
    let wit_component::DecodedWasm::Component(resolve, world_id) = decoded else {
        return None;
    };
    let world = &resolve.worlds[world_id];
    for (_key, item) in &world.imports {
        let wit_parser::WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let Some(qname) = resolve.id_of(*id) else {
            continue;
        };
        // wasi:* is host-provided; the wasmtime runtime drives those
        // suspends correctly even from a sync-rooted task because the
        // host owns the scheduler. Peer components don't.
        if qname.starts_with("wasi:") {
            continue;
        }
        let iface = &resolve.interfaces[*id];
        for (fn_name, func) in &iface.functions {
            if func.kind.is_async() {
                return Some(format!("{qname}#{fn_name}"));
            }
        }
    }
    None
}

/// Qualified names of the component's interface imports whose
/// instance type contains at least one resource. Best-effort: empty
/// on decode errors.
fn resource_bearing_imports(bytes: &[u8]) -> Vec<String> {
    let Ok(decoded) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wit_component::decode(bytes)
    })) else {
        return Vec::new();
    };
    let Ok(wit_component::DecodedWasm::Component(resolve, world_id)) = decoded else {
        return Vec::new();
    };
    let world = &resolve.worlds[world_id];
    let mut result = Vec::new();
    for (_key, item) in &world.imports {
        let wit_parser::WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let iface = &resolve.interfaces[*id];
        let has_resource = iface
            .types
            .values()
            .any(|tid| matches!(resolve.types[*tid].kind, wit_parser::TypeDefKind::Resource));
        if !has_resource {
            continue;
        }
        if let Some(name) = resolve.id_of(*id) {
            result.push(name);
        }
    }
    result
}

/// Returns true if the split-file number `split_num` corresponds to a shim.
///
/// `split_num` is `node.component_num + 1` — the key space used by the
/// `shim_comps` map produced by `split_out_composition`.
fn is_shim_split_num(split_num: usize, shim_comps: &HashMap<usize, usize>) -> bool {
    shim_comps.contains_key(&split_num)
}

/// If `inst_id` is a shim node, return its resolved-outer node id.
fn resolve_shim_node(
    inst_id: u32,
    composition: &CompositionGraph,
    shim_comps: &HashMap<usize, usize>,
) -> u32 {
    if !composition.nodes.contains_key(&inst_id) {
        return inst_id;
    }
    let split_num = node_split_num(inst_id, composition);
    let resolved = resolved_split_num(inst_id, composition, shim_comps);
    if resolved == split_num {
        return inst_id;
    }
    composition
        .nodes
        .iter()
        .find(|(_, n)| (n.component_num + 1) as usize == resolved)
        .map(|(id, _)| *id)
        .unwrap_or(inst_id)
}

/// First use of `pkg` returns `pkg`; subsequent uses get suffixed
/// `pkg-1`, `pkg-2`, ... so multiple instances of the same component
/// at different chain positions don't share a WAC var name.
fn disambiguated_var(counts: &mut HashMap<String, usize>, pkg: &str) -> String {
    let count = counts.entry(pkg.to_string()).or_insert(0);
    let var = if *count == 0 {
        pkg.to_string()
    } else {
        format!("{pkg}-{count}")
    };
    *count += 1;
    var
}

/// Convert an arbitrary node label into a valid WAC kebab-case identifier.
///
/// Node names in pre-composed binaries often look like `my:service/foo-shim`
/// `v`-prefixes digit-leading segments and strips a redundant `my-`
/// to avoid the eventual `my:my-...` package name.
fn sanitize_wac_id(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let stripped = sanitized
        .strip_prefix(&format!("{INST_PREFIX}-"))
        .unwrap_or(&sanitized);
    stripped
        .split('-')
        .filter(|s| !s.is_empty())
        .map(|seg| match seg.chars().next() {
            Some(c) if c.is_ascii_digit() => format!("v{seg}"),
            _ => seg.to_string(),
        })
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── edge_id ──────────────────────────────────────────────────────

    #[test]
    fn edge_id_internal_renders_caller_to_provider() {
        assert_eq!(
            derive_edge_id(
                "wasi:http/handler@0.3.0-rc-2026-01-06",
                Some("srv-b"),
                "srv-a",
            ),
            "wasi:http/handler@0.3.0-rc-2026-01-06::srv-b->srv-a",
        );
    }

    #[test]
    fn edge_id_boundary_drops_caller_segment() {
        assert_eq!(
            derive_edge_id("wasi:http/handler@0.3.0", None, "srv-a"),
            "wasi:http/handler@0.3.0::->srv-a",
        );
    }

    #[test]
    fn edge_id_before_and_between_targeting_same_edge_collide() {
        // The doc's invariant: a `before(provider=B)` matching caller A
        // and a `between(outer=A, inner=B)` identify the same physical
        // edge, so they must render to the same edge_id.
        let from_between = derive_edge_id("ns:pkg/iface@1.0.0", Some("A"), "B");
        let from_before = derive_edge_id("ns:pkg/iface@1.0.0", Some("A"), "B");
        assert_eq!(from_between, from_before);
    }

    #[test]
    fn edge_id_derivation_is_deterministic() {
        let a = derive_edge_id("ns:pkg/iface@1.2.3", Some("caller"), "provider");
        let b = derive_edge_id("ns:pkg/iface@1.2.3", Some("caller"), "provider");
        assert_eq!(a, b);
    }

    // ── synth_graph + everything else ────────────────────────────────

    /// Build a graph with the given import edges. Each entry is
    /// `(consumer_node_id, interface, source_node_id, is_host_import)`.
    /// `n_nodes` placeholder nodes are created up-front.
    fn synth_graph(n_nodes: u32, edges: &[(u32, &str, Option<u32>, bool)]) -> CompositionGraph {
        let mut graph = CompositionGraph::new();
        let mut nodes: HashMap<u32, ComponentNode> = HashMap::new();
        for i in 0..n_nodes {
            nodes.insert(i, ComponentNode::new(format!("$node-{i}"), i, i));
        }
        for (consumer, iface, src, is_host) in edges {
            let n = nodes.get_mut(consumer).expect("node id in range");
            n.add_import(InterfaceConnection {
                interface_name: iface.to_string(),
                source_instance: *src,
                is_host_import: *is_host,
                fingerprint: None,
                interface_type: None,
            });
        }
        for (i, node) in nodes {
            graph.add_node(i, node);
        }
        graph
    }

    /// Single-provider factored types: a single provider node is the
    /// non-host source of both api and types. Wire types from
    /// downstream.
    #[test]
    fn factored_types_same_provider_wires() {
        // node 1 = consumer; node 0 = provider. consumer imports both
        // api and types from provider (non-host).
        let graph = synth_graph(
            2,
            &[
                (1, "my:shape/api@1.0.0", Some(0), false),
                (1, "my:shape/types@1.0.0", Some(0), false),
            ],
        );
        let extras = factored_types_to_wire(
            &["my:shape/types@1.0.0".to_string()],
            "my:shape/api@1.0.0",
            &graph,
            &HashMap::new(),
        )
        .expect("same-provider factored types should wire");
        assert_eq!(extras, vec!["my:shape/types@1.0.0".to_string()]);
    }

    /// Host-provided types: every import edge is `is_host_import`. The
    /// runtime's single shared instance handles it via `...`.
    #[test]
    fn factored_types_host_provided_skipped() {
        // node 0 = srv; imports wasi:http/handler from node 1, and
        // wasi:http/types from the host.
        let graph = synth_graph(
            2,
            &[
                (0, "wasi:http/handler@0.3.0", Some(1), false),
                (0, "wasi:http/types@0.3.0", None, true),
            ],
        );
        let extras = factored_types_to_wire(
            &["wasi:http/types@0.3.0".to_string()],
            "wasi:http/handler@0.3.0",
            &graph,
            &HashMap::new(),
        )
        .expect("host-provided types should be skipped, not error");
        assert!(extras.is_empty());
    }

    /// Pathological multi-provider case: api is provided by node 1,
    /// types is provided by node 0. Adapter on api can't safely wire
    /// types from its downstream (node 1), so splicer bails.
    #[test]
    fn factored_types_multi_provider_bails() {
        // node 2 = consumer. consumer imports api from node 1
        // (api-provider), types from node 0 (types-provider).
        let graph = synth_graph(
            3,
            &[
                (2, "my:shape/api@1.0.0", Some(1), false),
                (2, "my:shape/types@1.0.0", Some(0), false),
            ],
        );
        let err = factored_types_to_wire(
            &["my:shape/types@1.0.0".to_string()],
            "my:shape/api@1.0.0",
            &graph,
            &HashMap::new(),
        )
        .expect_err("multi-provider factored types should bail");
        let msg = err.to_string();
        assert!(
            msg.contains("my:shape/types@1.0.0"),
            "error should name the offending interface; got: {msg}"
        );
        assert!(
            msg.contains("different component"),
            "error should explain the multi-provider problem; got: {msg}"
        );
    }

    // ── splicer:builtin-config provider wiring ───────────────────────

    /// Write a minimal valid component to a unique temp path and return
    /// it. `add_middleware` reads the adapter wasm to discover
    /// resource-bearing imports, so unit tests targeting it need a real
    /// file on disk.
    fn write_minimal_component(name: &str) -> String {
        let bytes = wat::parse_str("(component)").expect("compile minimal component");
        let path =
            std::env::temp_dir().join(format!("splicer-test-{}-{}.wasm", name, std::process::id()));
        std::fs::write(&path, bytes).expect("write tempfile");
        path.to_string_lossy().into_owned()
    }

    /// With `config_provider_path` set, [`EmitPlan::add_middleware`]
    /// emits the patched provider as its own entity and wires its
    /// `get` export into the real middleware's import.
    #[test]
    fn config_provider_wired_when_path_set() {
        use crate::parse::config::{AdapterInjectionInfo, Injection};
        let cfg_path = write_minimal_component("metrics-config");
        let adapter_path = write_minimal_component("metrics-adapter");
        let mdl = Injection {
            name: "metrics".to_string(),
            path: Some("/tmp/metrics.wasm".to_string()),
            builtin: Some("otel-bare-metrics".to_string()),
            builtin_config: Default::default(),
            config_as_wave: None,
            config_provider_path: Some(cfg_path.clone()),
            adapter_info: Some(AdapterInjectionInfo {
                adapter_path,
                matched_hook_interfaces: vec!["splicer:tier1/before".to_string()],
            }),
            tier: None,
        };
        let contract = Contract {
            name: "wasi:http/handler@0.3.0".to_string(),
            ty_fingerprint: None,
        };
        let graph = synth_graph(1, &[]);
        let mut plan = EmitPlan::new();
        let _adapter_var = plan
            .add_middleware(&mdl, &contract, "downstream", &graph, &HashMap::new())
            .expect("plan");

        let cfg = plan
            .entities
            .get("metrics-config")
            .expect("config provider entity must be added");
        assert!(
            cfg.imports.is_empty() && cfg.catchall,
            "config provider should be `{{ ... }}` form; got: {cfg:?}"
        );

        let real = plan
            .entities
            .get("metrics")
            .expect("real middleware entity must be added");
        assert!(
            real.imports
                .iter()
                .any(|(iface, src)| iface == "splicer:builtin-config/get@0.1.0"
                    && src == "metrics-config"),
            "real middleware must wire the provider's `get` export; got: {real:?}"
        );

        // wac_deps must include the patched provider so wac compose can
        // find its bytes.
        assert!(
            plan.used_middlewares
                .iter()
                .any(|(name, path)| name == "metrics-config" && path == &cfg_path),
            "config provider must be registered in used_middlewares; got: {:?}",
            plan.used_middlewares
        );
    }

    /// Without `config_provider_path`, the real middleware is the
    /// unchanged `{ ... }` form — every existing tier-1 builtin
    /// (hello-tier1, otel-bare-spans) must keep working.
    #[test]
    fn config_provider_not_wired_when_path_unset() {
        use crate::parse::config::{AdapterInjectionInfo, Injection};
        let adapter_path = write_minimal_component("hello-adapter");
        let mdl = Injection {
            name: "hello".to_string(),
            path: Some("/tmp/hello.wasm".to_string()),
            builtin: Some("hello-tier1".to_string()),
            builtin_config: Default::default(),
            config_as_wave: None,
            config_provider_path: None,
            adapter_info: Some(AdapterInjectionInfo {
                adapter_path,
                matched_hook_interfaces: vec!["splicer:tier1/before".to_string()],
            }),
            tier: None,
        };
        let contract = Contract {
            name: "wasi:http/handler@0.3.0".to_string(),
            ty_fingerprint: None,
        };
        let graph = synth_graph(1, &[]);
        let mut plan = EmitPlan::new();
        let _adapter_var = plan
            .add_middleware(&mdl, &contract, "downstream", &graph, &HashMap::new())
            .expect("plan");

        let real = plan
            .entities
            .get("hello")
            .expect("real middleware entity must be added");
        assert!(
            real.imports.is_empty() && real.catchall,
            "real middleware should be `{{ ... }}` form; got: {real:?}"
        );
        assert!(
            !plan.entities.contains_key("hello-config"),
            "no config provider entity should be added; got entities: {:?}",
            plan.entities.keys().collect::<Vec<_>>()
        );
        assert!(
            plan.used_middlewares
                .iter()
                .all(|(name, _)| name != "hello-config"),
            "no config provider should appear in used_middlewares; got: {:?}",
            plan.used_middlewares
        );
    }

    // ── chain pass: middle node that is both consumer and provider ──

    /// A "middle" node that PROVIDES one interface to its parent AND
    /// CONSUMES a different interface from a child must still get its
    /// child-side import re-routed through middleware injected by a
    /// `before` rule on that child interface.
    ///
    /// Topology:
    ///     inner ── test:demo/inner ──▶ middle ── test:demo/middle ──▶ outer
    /// Rule:
    ///     before: test:demo/inner, provider=inner  →  inject `mw`
    /// Expected:
    ///     `let middle = new my:middle { "test:demo/inner": mw[...], ... };`
    /// Actual (bug):
    ///     middle is greedy-instantiated by the outer-side chain
    ///     before the inner-side chain runs, so the override is
    ///     silently dropped. The `mw` instance is emitted but never
    ///     wired into middle.
    #[test]
    fn middle_node_consumer_import_routes_through_inner_middleware() {
        use crate::parse::config::{Injection, SpliceRule};
        use cviz::model::{ComponentNode, CompositionGraph, InterfaceConnection};

        let mut graph = CompositionGraph::new();

        // node 0 = inner (leaf provider).
        graph.add_node(0, ComponentNode::new("$inner".to_string(), 0, 0));

        // node 1 = middle (consumes inner, provides middle iface).
        let mut middle = ComponentNode::new("$middle".to_string(), 1, 1);
        middle.add_import(InterfaceConnection {
            interface_name: "test:demo/inner".to_string(),
            source_instance: Some(0),
            is_host_import: false,
            fingerprint: None,
            interface_type: None,
        });
        graph.add_node(1, middle);

        // node 2 = outer (consumes middle, provides top-level export).
        let mut outer = ComponentNode::new("$outer".to_string(), 2, 2);
        outer.add_import(InterfaceConnection {
            interface_name: "test:demo/middle".to_string(),
            source_instance: Some(1),
            is_host_import: false,
            fingerprint: None,
            interface_type: None,
        });
        graph.add_node(2, outer);

        // Anchor for the export pass — composition must export
        // something for `generate_wac` to terminate normally.
        graph.add_export("test:demo/outer".to_string(), 2, None);

        // `path: None` so contract validation degrades to a Warn and
        // the chain pass uses the simple `create_mdl` path (no fs /
        // adapter generation).
        let rules = vec![SpliceRule::Before {
            interface: "test:demo/inner".to_string(),
            provider_name: Some("inner".to_string()),
            provider_alias: None,
            inject: vec![Injection {
                name: "mw".to_string(),
                path: None,
                builtin: None,
                builtin_config: Default::default(),
                config_as_wave: None,
                config_provider_path: None,
                adapter_info: None,
                tier: None,
            }],
        }];

        let out = generate_wac(
            HashMap::new(),
            "/tmp/splicer-test-splits",
            &graph,
            &rules,
            None,
            "test:nested",
        )
        .expect("generate_wac");

        assert!(
            out.wac.contains("let mw = new my:mw {"),
            "mw middleware var must be emitted; got:\n{}",
            out.wac
        );

        let start = out
            .wac
            .find("let middle = new my:middle")
            .expect("middle instance must be emitted");
        let end = out.wac[start..]
            .find("};")
            .expect("middle block must close");
        let middle_block = &out.wac[start..start + end];

        assert!(
            middle_block.contains("\"test:demo/inner\": mw[\"test:demo/inner\"]"),
            "middle must wire `test:demo/inner` through the injected \
             `mw` middleware (the inner `before` rule), but it doesn't. \
             middle block:\n{middle_block}\n\nFull WAC:\n{}",
            out.wac
        );
    }
}
