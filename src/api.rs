//! Implementation of the top-level [`splice`] and [`compose`] entry
//! points. See the [crate-level docs](crate) for usage examples and
//! the full API guide.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cviz::parse::component::parse_component;
use wac_graph::EncodeOptions;
use wac_parser::Document;
use wac_resolver::{packages, FileSystemPackageResolver};

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

// ── Compose request ────────────────────────────────────────────────────────

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

// ── Bundle: shared output of splice and compose ────────────────────────────

/// Output of [`splice`] and [`compose`]: the generated WAC source,
/// the dep map it references, contract diagnostics, and any tier-1
/// adapter components splicer wrote to disk. Most callers reach for
/// [`Bundle::to_wasm`] to go straight to a composed component.
#[derive(Debug, Clone)]
pub struct Bundle {
    /// The generated WAC source. Pass to [`Bundle::to_wasm`], or
    /// write it to disk and run `wac compose` yourself.
    pub wac: String,

    /// Per-dependency `package_key → wasm_path` map. Keys are
    /// fully-qualified WAC package keys (e.g. `"my:srv-a"`); the
    /// shape matches `wac_resolver::FileSystemPackageResolver::new`'s
    /// `overrides` argument. Paths are always absolute, so consumers
    /// are unaffected by changes to the process working directory.
    /// See [`Bundle::wac_compose_cmd`] for the shell-command form.
    pub wac_deps: BTreeMap<String, PathBuf>,

    /// Contract validation diagnostics. On `Ok` from `splice`, holds
    /// only `Ok`/`Warn` entries; `Error` entries fail `splice` unless
    /// `skip_type_check` was set. `compose` does not run contract
    /// checks, so its bundles ship empty.
    pub diagnostics: Vec<ContractResult>,

    /// Tier-1 adapter components splicer generated. Populated by
    /// `splice` when a rule wraps a tier-1 type-erased middleware;
    /// `compose` leaves it empty. Each entry carries the on-disk
    /// path, the wrapped middleware name, the target interface, and
    /// which `splicer:tier1/*` hook interfaces it exports.
    ///
    /// Adapter paths also appear in [`Bundle::wac_deps`] under their
    /// adapter package key — this field is for callers who want the
    /// metadata too.
    pub generated_adapters: Vec<GeneratedAdapter>,
}

impl Bundle {
    /// Format a `wac compose <wac_path> --dep ...` shell command,
    /// where `wac_path` is where you wrote [`Bundle::wac`] to disk.
    pub fn wac_compose_cmd(&self, wac_path: &str) -> String {
        format_wac_compose_cmd(wac_path, &self.wac_deps)
    }

    /// Compose this bundle into a single Wasm component, in-process.
    /// Equivalent to `wac compose` on [`Bundle::wac`] with every dep
    /// from [`Bundle::wac_deps`]; the result is wasmparser-validated
    /// before return.
    pub fn to_wasm(&self) -> Result<Vec<u8>> {
        compose_wac(&self.wac, &self.wac_deps)
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
pub fn splice(req: SpliceRequest) -> Result<Bundle> {
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

    let mut wac_deps = out.wac_deps;
    canonicalize_wac_deps(&mut wac_deps)?;

    Ok(Bundle {
        wac: out.wac,
        wac_deps,
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
pub fn compose(req: ComposeRequest) -> Result<Bundle> {
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

    let mut wac_deps = out.wac_deps;
    canonicalize_wac_deps(&mut wac_deps)?;

    Ok(Bundle {
        wac: out.wac,
        wac_deps,
        diagnostics: out.diagnostics,
        generated_adapters: out.generated_adapters,
    })
}

/// In-process equivalent of `wac compose`: parse `wac`, resolve
/// every package reference against `wac_deps`, and encode the result
/// into wasmparser-validated bytes. `wac_deps` must cover every
/// package the WAC references — the resolver does not fall back to
/// the filesystem.
///
/// Equivalent to [`Bundle::to_wasm`] when called on a splicer-emitted
/// bundle; expose this directly when you've assembled `wac` and
/// `wac_deps` from somewhere other than [`splice`] / [`compose`].
pub fn compose_wac(wac: &str, wac_deps: &BTreeMap<String, PathBuf>) -> Result<Vec<u8>> {
    let doc = Document::parse(wac).context("Failed to parse generated WAC source")?;
    let keys = packages(&doc).context("Failed to discover packages from WAC")?;

    // `disable_filesystem: true` means the resolver only consults
    // `overrides`, so the first arg (the on-disk search base) is
    // never read. Hardcoding "." just keeps the type happy.
    let overrides: HashMap<String, PathBuf> = wac_deps.clone().into_iter().collect();
    let resolver = FileSystemPackageResolver::new(Path::new("."), overrides, true);
    let pkgs = resolver
        .resolve(&keys)
        .context("Failed to resolve WAC packages")?;

    let resolution = doc.resolve(pkgs).context("Failed to resolve WAC document")?;
    let composed: Vec<u8> = resolution
        .encode(EncodeOptions::default())
        .context("Failed to encode composed component")?;

    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    validator
        .validate_all(&composed)
        .context("Composed component bytes failed wasmparser validation")?;

    Ok(composed)
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

/// Rewrite every path in `wac_deps` to its canonical absolute form.
/// Splicer always returns absolute paths in [`Bundle::wac_deps`] so
/// that downstream consumers (`Bundle::to_wasm`, the printed
/// `wac compose` shell command, lib callers that change cwd between
/// calls) stay correct regardless of process working directory.
///
/// Each path in the map must already exist on disk — splits,
/// generated adapters, materialized builtins, and user-supplied
/// injection wasms are all written or required to exist before this
/// runs. A missing path here means an upstream pipeline stage
/// produced a bogus reference; we surface that as a clear error.
fn canonicalize_wac_deps(deps: &mut BTreeMap<String, PathBuf>) -> Result<()> {
    for (key, path) in deps.iter_mut() {
        let canonical = std::fs::canonicalize(&*path).with_context(|| {
            format!(
                "Failed to canonicalize wac_deps path for '{key}': {}",
                path.display()
            )
        })?;
        *path = canonical;
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
    /// `canonicalize_wac_deps` should rewrite each value to its
    /// absolute, symlink-resolved form so downstream consumers stay
    /// correct under cwd shifts.
    #[test]
    fn canonicalize_wac_deps_makes_paths_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let abs = tmp.path().join("a.wasm");
        std::fs::write(&abs, b"\0asm\x0d\0\0\0").unwrap();

        // Build a relative path to the same file by cd'ing into the
        // tempdir for the duration of the test.
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let mut deps: BTreeMap<String, PathBuf> = BTreeMap::new();
        deps.insert("my:relative".into(), PathBuf::from("a.wasm"));
        deps.insert("my:absolute".into(), abs.clone());

        canonicalize_wac_deps(&mut deps).unwrap();

        std::env::set_current_dir(prev_cwd).unwrap();

        for (key, p) in &deps {
            assert!(p.is_absolute(), "{key} -> {} is not absolute", p.display());
        }
    }

    /// A wac_deps entry pointing at a non-existent file should
    /// surface a clear error naming the offending key + path.
    #[test]
    fn canonicalize_wac_deps_errors_on_missing_path() {
        let mut deps: BTreeMap<String, PathBuf> = BTreeMap::new();
        deps.insert(
            "my:ghost".into(),
            PathBuf::from("/definitely/does/not/exist/ghost.wasm"),
        );
        let err = canonicalize_wac_deps(&mut deps).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("my:ghost"), "error names key: {msg}");
        assert!(msg.contains("ghost.wasm"), "error names path: {msg}");
    }

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
