//! Implementation of the top-level [`splice`] and [`compose`] entry
//! points. See the [crate-level docs](crate) for usage examples and
//! the full API guide.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anyhow::{Context, Result};
use cviz::parse::component::parse_component;

use crate::builtins;
use crate::compose::{build_graph_from_components, filename_from_path};
use crate::contract::ContractResult;
use crate::parse::config::{parse_yaml, SpliceRule};
use crate::split::split_out_composition;
use crate::wac::{generate_wac, GeneratedAdapter};

// ── Splice request / output ────────────────────────────────────────────────

/// Inputs to [`splice`].
#[derive(Debug, Clone)]
pub struct SpliceRequest {
    /// Path to the pre-composed Wasm component to splice middleware into.
    /// The file is read from disk and parsed into a composition graph.
    pub composition_wasm: PathBuf,

    /// Splice rules in YAML format. The caller is responsible for
    /// loading the YAML — splicer will not read it from disk.
    pub rules_yaml: String,

    /// Package name written to the top of the generated WAC source
    /// (e.g. `"example:composition"`).
    pub package_name: String,

    /// Directory where split sub-components and generated adapter
    /// components are written. Created if it does not exist.
    pub splits_dir: PathBuf,

    /// When `true`, contract type-check errors are demoted to
    /// warnings and `splice()` succeeds. When `false`, the function
    /// returns `Err` if any contract check fails.
    pub skip_type_check: bool,
}

/// Output of [`splice`].
#[derive(Debug, Clone)]
pub struct SpliceOutput {
    /// The generated WAC source as a UTF-8 string. The caller is
    /// responsible for writing it to disk (or feeding it directly
    /// into [`wac-parser`](https://docs.rs/wac-parser)).
    pub wac: String,

    /// Per-dependency `(package_key → wasm_path)` map. The keys are
    /// fully-qualified WAC package keys (e.g. `"my:srv-a"`) — exactly
    /// the form you'd hand to
    /// [`wac_resolver::FileSystemPackageResolver::new`](https://docs.rs/wac-resolver)
    /// as the `overrides` argument, or to format into a
    /// `wac compose ... --dep <key>=<path>` shell command via
    /// [`SpliceOutput::wac_compose_cmd`].
    pub wac_deps: BTreeMap<String, PathBuf>,

    /// Diagnostics emitted during contract validation. When `splice`
    /// returns `Ok`, this list contains only `Ok` / `Warn` entries.
    /// `Error` entries cause `splice` to return `Err` unless
    /// `skip_type_check` was set on the request.
    pub diagnostics: Vec<ContractResult>,

    /// Tier-1 adapter components that splicer generated and wrote to
    /// disk while resolving the splice rules. Empty when no rule
    /// matched a tier-1 type-erased middleware. Each entry names
    /// the on-disk path, the wrapped middleware, the target interface,
    /// and which tier-1 hook interfaces (`splicer:tier1/before`,
    /// `splicer:tier1/after`, `splicer:tier1/blocking`) the
    /// middleware exports.
    ///
    /// Adapter paths are also present in [`SpliceOutput::wac_deps`]
    /// under their adapter package key — `generated_adapters` is
    /// for callers who want richer metadata about which middleware
    /// got wrapped and why.
    pub generated_adapters: Vec<GeneratedAdapter>,
}

impl SpliceOutput {
    /// Format a `wac compose <wac_path> --dep ...` shell command
    /// using `wac_path` as the path to the WAC source the caller
    /// wrote to disk.
    pub fn wac_compose_cmd(&self, wac_path: &str) -> String {
        format_wac_compose_cmd(wac_path, &self.wac_deps)
    }
}

// ── Compose request / output ───────────────────────────────────────────────

/// One component to feed into [`compose`].
#[derive(Debug, Clone)]
pub struct ComponentInput {
    /// Optional alias used as the variable name in the generated WAC.
    /// When `None`, the file stem of `path` is used. Aliases must be
    /// unique within a [`ComposeRequest`].
    pub alias: Option<String>,

    /// Path to the Wasm component file. The bytes are read by
    /// [`compose`].
    pub path: PathBuf,
}

/// Inputs to [`compose`].
#[derive(Debug, Clone)]
pub struct ComposeRequest {
    /// Two or more components to compose. Their import/export
    /// surfaces are matched automatically and the resulting
    /// composition is topologically sorted before WAC generation.
    pub components: Vec<ComponentInput>,

    /// Package name written to the top of the generated WAC source.
    pub package_name: String,
}

/// Output of [`compose`].
#[derive(Debug, Clone)]
pub struct ComposeOutput {
    /// The generated WAC source.
    pub wac: String,
    /// Per-dependency `(package_key → wasm_path)` map. Same shape as
    /// [`SpliceOutput::wac_deps`] — directly consumable by
    /// `wac-resolver::FileSystemPackageResolver`.
    pub wac_deps: BTreeMap<String, PathBuf>,
    /// Diagnostics from validation. (Compose does not run type
    /// checks today, so this is currently always empty — exposed
    /// for forward compatibility.)
    pub diagnostics: Vec<ContractResult>,
    /// Tier-1 adapter components generated during composition.
    /// Compose does not splice middleware, so this is currently
    /// always empty — exposed for forward compatibility.
    pub generated_adapters: Vec<GeneratedAdapter>,
}

impl ComposeOutput {
    /// Format a `wac compose <wac_path> --dep ...` shell command.
    pub fn wac_compose_cmd(&self, wac_path: &str) -> String {
        format_wac_compose_cmd(wac_path, &self.wac_deps)
    }
}

// ── Top-level functions ────────────────────────────────────────────────────

/// Splice middleware into a pre-composed Wasm component.
///
/// Equivalent to the `splicer splice` CLI subcommand. Reads
/// `req.composition_wasm`, parses `req.rules_yaml`, splits the
/// composition into sub-components under `req.splits_dir`, runs
/// contract validation on the configured middleware, generates an
/// adapter component for any tier-1 type-erased middleware, and
/// returns the resulting WAC source.
///
/// Returns `Err` when:
/// - The composition wasm cannot be read or parsed.
/// - The YAML rules are malformed.
/// - The splitter fails to write split sub-components.
/// - Contract validation produces an `Error` diagnostic and
///   `req.skip_type_check` is `false`.
pub fn splice(req: SpliceRequest) -> Result<SpliceOutput> {
    let SpliceRequest {
        composition_wasm,
        rules_yaml,
        package_name,
        splits_dir,
        skip_type_check,
    } = req;

    let mut cfg = parse_yaml(&rules_yaml).context("Failed to parse splice rules YAML")?;

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

    let splits_dir_str = splits_dir
        .to_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "splits_dir contains non-UTF-8 bytes: {}",
                splits_dir.display()
            )
        })?
        .to_string();
    let (splits_path, shim_comps) =
        split_out_composition(&composition_wasm, &Some(splits_dir_str))?;

    // Materialize builtin middleware bytes to disk now that splits_dir
    // is established. Stamps `injection.path` so the rest of the
    // pipeline (contract validation, tier-1 detection, adapter
    // generation, WAC) treats builtins as ordinary path-backed
    // middleware.
    materialize_builtins(&mut cfg, std::path::Path::new(&splits_path))?;

    let out = generate_wac(shim_comps, &splits_path, &graph, &cfg, None, &package_name)?;

    if !skip_type_check {
        for diag in &out.diagnostics {
            if let ContractResult::Error(msg) = diag {
                anyhow::bail!("Contract type-check error: {msg}");
            }
        }
    }

    Ok(SpliceOutput {
        wac: out.wac,
        wac_deps: out.wac_deps,
        diagnostics: out.diagnostics,
        generated_adapters: out.generated_adapters,
    })
}

/// Synthesize a composition from N individual components.
///
/// Equivalent to the `splicer compose` CLI subcommand. Reads each
/// component file, builds a composition graph by matching their
/// import/export surfaces, and emits the WAC source.
///
/// Returns `Err` when:
/// - Two `ComponentInput`s resolve to the same name.
/// - A component file cannot be read.
/// - Graph synthesis fails (e.g. unresolved imports, cycles, etc.).
pub fn compose(req: ComposeRequest) -> Result<ComposeOutput> {
    let ComposeRequest {
        components,
        package_name,
    } = req;

    // Resolve aliases (or filename stems), read each file's bytes,
    // and check for name conflicts before any composition work.
    let mut resolved: Vec<(String, PathBuf, Vec<u8>)> = Vec::with_capacity(components.len());
    for ComponentInput { alias, path } in &components {
        let name = alias.clone().unwrap_or_else(|| filename_from_path(path));
        let bytes = std::fs::read(path)
            .with_context(|| format!("Failed to read Wasm component: {}", path.display()))?;
        resolved.push((name, path.clone(), bytes));
    }

    // Duplicate-name check (mirrors the CLI's pre-flight check so the
    // error message is the same regardless of entry point).
    {
        let mut seen: HashMap<&str, &PathBuf> = HashMap::new();
        for (name, path, _) in &resolved {
            if let Some(prev) = seen.insert(name.as_str(), path) {
                anyhow::bail!(
                    "Name conflict: '{}' and '{}' both resolve to the name '{}'.\n\
                     Use aliases to disambiguate, e.g.:\n\
                     \t{}0={} {}1={}",
                    prev.display(),
                    path.display(),
                    name,
                    name,
                    prev.display(),
                    name,
                    path.display(),
                );
            }
        }
    }

    let (graph, node_paths) = build_graph_from_components(&resolved)?;

    let out = generate_wac(
        HashMap::new(),
        "",
        &graph,
        &[],
        Some(&node_paths),
        &package_name,
    )?;

    Ok(ComposeOutput {
        wac: out.wac,
        wac_deps: out.wac_deps,
        diagnostics: out.diagnostics,
        generated_adapters: out.generated_adapters,
    })
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Walk every injection in `rules`; for builtin-form entries, write
/// the embedded bytes from [`crate::builtins`] to disk under
/// `splits_dir/builtins/` and stamp the resulting absolute path onto
/// the injection. After this runs, every injection that came in via
/// `builtin: ...` looks identical to a path-backed user middleware
/// from the rest of the pipeline's perspective.
fn materialize_builtins(rules: &mut [SpliceRule], splits_dir: &std::path::Path) -> Result<()> {
    for rule in rules.iter_mut() {
        for inj in rule.inject_mut().iter_mut() {
            let Some(builtin) = inj.builtin.as_deref() else {
                continue;
            };
            let path = builtins::materialize_into(splits_dir, builtin)
                .with_context(|| format!("Failed to materialize builtin '{builtin}'"))?;
            let path_str = path
                .to_str()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "materialized builtin path contains non-UTF-8 bytes: {}",
                        path.display()
                    )
                })?
                .to_string();
            inj.path = Some(path_str);
        }
    }
    Ok(())
}

/// Format a `wac compose <wac_path> --dep ...` shell command line
/// from the wac source path and the per-dependency `(package_key,
/// wasm_path)` map returned by [`splice`] or [`compose`].
pub fn format_wac_compose_cmd(wac_path: &str, deps: &BTreeMap<String, PathBuf>) -> String {
    let mut cmd = format!("wac compose {wac_path} ");
    for (pkg_key, pkg_path) in deps {
        cmd.push_str(&format!(
            "\\\n    --dep {pkg_key}=\"{}\" ",
            pkg_path.display()
        ));
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::with_fake_builtins;
    use crate::parse::config::parse_yaml;
    use std::path::Path;

    /// `materialize_builtins` should resolve every builtin-form
    /// injection's bytes (here, from a local override) and stamp
    /// `inj.path` so the rest of the pipeline sees a normal
    /// path-backed middleware.
    #[test]
    fn builtin_yaml_roundtrips_through_materialize() {
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:logging/log@0.1.0
    inject:
      - builtin: hello-tier1
"#;
        with_fake_builtins(&["hello-tier1"], || {
            let mut rules = parse_yaml(yaml).expect("parse");
            let tmp = tempfile::tempdir().unwrap();
            materialize_builtins(&mut rules, tmp.path()).expect("materialize");

            let inj = &rules[0].inject()[0];
            assert_eq!(inj.builtin.as_deref(), Some("hello-tier1"));
            let path = inj.path.as_deref().expect("path stamped");
            let bytes = std::fs::read(path).expect("file written");
            assert!(bytes.starts_with(b"\0asm"), "materialized bytes are wasm");
            // `Path::ends_with` is component-aware, so this works on both
            // unix (`builtins/hello-tier1.wasm`) and windows
            // (`builtins\hello-tier1.wasm`).
            assert!(
                Path::new(path).ends_with("builtins/hello-tier1.wasm"),
                "path lives under splits_dir/builtins/: {path}"
            );
        });
    }

    /// User-form injections (`name` + `path`) must pass through
    /// untouched — only `builtin:` entries get rewritten.
    #[test]
    fn user_form_injection_left_alone() {
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:logging/log@0.1.0
    inject:
      - name: tracing
        path: /opt/middleware/tracing.wasm
"#;
        let mut rules = parse_yaml(yaml).expect("parse");
        let tmp = tempfile::tempdir().unwrap();
        materialize_builtins(&mut rules, tmp.path()).expect("materialize");

        let inj = &rules[0].inject()[0];
        assert!(inj.builtin.is_none());
        assert_eq!(inj.path.as_deref(), Some("/opt/middleware/tracing.wasm"));
        // The builtins/ subdir should never be created when nothing
        // referenced a builtin.
        assert!(!tmp.path().join("builtins").exists());
    }

    /// A mix of user-form and builtin-form injections in the same
    /// rule list. Each is handled independently.
    #[test]
    fn mixed_user_and_builtin_injections() {
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:logging/log@0.1.0
    inject:
      - name: tracing
        path: ./tracing.wasm
      - builtin: hello-tier1
"#;
        with_fake_builtins(&["hello-tier1"], || {
            let mut rules = parse_yaml(yaml).expect("parse");
            let tmp = tempfile::tempdir().unwrap();
            materialize_builtins(&mut rules, tmp.path()).expect("materialize");

            let inject = &rules[0].inject();
            assert_eq!(inject[0].path.as_deref(), Some("./tracing.wasm"));
            let materialized = inject[1].path.as_deref().unwrap();
            assert!(Path::new(materialized).ends_with("builtins/hello-tier1.wasm"));
        });
    }

    /// The long-form `builtin: { name: ..., alias: ... }` should land
    /// the alias in `inj.name` (the WAC variable) while still
    /// materializing the bytes of the builtin named in `builtin.name`.
    #[test]
    fn builtin_long_form_alias_used_as_wac_var() {
        let yaml = r#"
version: 1
rules:
  - before:
      interface: wasi:logging/log@0.1.0
    inject:
      - builtin:
          name: hello-tier1
          alias: greeter
"#;
        with_fake_builtins(&["hello-tier1"], || {
            let mut rules = parse_yaml(yaml).expect("parse");
            let tmp = tempfile::tempdir().unwrap();
            materialize_builtins(&mut rules, tmp.path()).expect("materialize");

            let inj = &rules[0].inject()[0];
            assert_eq!(inj.name, "greeter");
            assert_eq!(inj.builtin.as_deref(), Some("hello-tier1"));
            let materialized = inj.path.as_deref().unwrap();
            assert!(Path::new(materialized).ends_with("builtins/hello-tier1.wasm"));
        });
    }

    /// Naming a builtin that doesn't exist surfaces a clear error
    /// listing what's available.
    #[test]
    fn unknown_builtin_errors_with_available() {
        // Construct rules directly — parse_yaml can't produce an
        // unknown builtin, since the registry isn't consulted at
        // parse time.
        use crate::parse::config::{Injection, SpliceRule};
        let mut rules = vec![SpliceRule::Before {
            interface: "wasi:logging/log@0.1.0".into(),
            provider_name: None,
            provider_alias: None,
            inject: vec![Injection {
                name: "ghost".into(),
                path: None,
                builtin: Some("does-not-exist".into()),
                adapter_info: None,
            }],
        }];
        let tmp = tempfile::tempdir().unwrap();
        let err = materialize_builtins(&mut rules, tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("does-not-exist"), "error names builtin: {msg}");
        assert!(msg.contains("hello-tier1"), "error lists available: {msg}");
    }
}
