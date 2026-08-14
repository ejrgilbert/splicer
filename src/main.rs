use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use std::fs;
use std::path::{Path, PathBuf};

use splicer::cviz::output::graph::{generate_graph_ascii, GraphRenderOpts};
use splicer::cviz::output::{mermaid::generate_mermaid, terminal_columns, ColorMode, Direction};
use splicer::types::{ContractResult, SizeReport};
use splicer::{
    builtin_info, compose, format_skip_summary, preview, splice, Bundle, ComponentInput,
    ComposeRequest, PreviewRequest, SpliceRequest,
};

const DEFAULT_PKG: &str = "example:composition";
const DEFAULT_OUTPUT_WASM: &str = "composed.wasm";
const DEFAULT_OUTPUT_WAC: &str = "output.wac";
const DEFAULT_SPLITS_DIR: &str = "./splits";

#[derive(Parser, Debug)]
#[command(name = "splicer")]
#[command(
    version,
    about = "Plan and generate WebAssembly component compositions."
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Inject middleware into an existing composed Wasm component.
    ///
    /// Reads the splice configuration, splits the composed binary, runs
    /// the in-process compose pipeline, and writes a single composed
    /// `.wasm` to disk. Pass `--plan` to skip composing and instead emit
    /// a WAC file plus the equivalent `wac compose` shell command.
    Splice {
        /// Path to the splice configuration in YAML format.
        #[arg(value_name = "SPLICE_CFG")]
        splice_cfg_file: PathBuf,

        /// Pre-composed Wasm component binary to splice into.
        #[arg(value_name = "COMP_WASM")]
        comp_wasm: PathBuf,

        /// Path for the composed Wasm output (default: composed.wasm).
        #[arg(short = 'o', long = "output", value_name = "PATH")]
        output: Option<PathBuf>,

        /// Persist the intermediate WAC source for debugging or auditing.
        /// Bare flag uses ./output.wac; pass a path to override.
        #[arg(
            long = "emit-wac",
            value_name = "PATH",
            num_args = 0..=1,
            default_missing_value = DEFAULT_OUTPUT_WAC,
        )]
        emit_wac: Option<PathBuf>,

        /// Skip in-process compose. Persist the WAC + splits and print
        /// the equivalent `wac compose ...` shell command to stdout.
        #[arg(long)]
        plan: bool,

        /// Directory where split sub-components are written. When
        /// omitted, splits go to a tempdir (cleaned up on success);
        /// passing this flag persists them on disk.
        #[arg(short = 'd', long = "splits-dir", value_name = "DIR")]
        splits_dir: Option<PathBuf>,

        /// Package name written at the top of the generated WAC.
        #[arg(long, default_value = DEFAULT_PKG)]
        package: String,

        /// Demote type-incompatibility errors to warnings so injection
        /// proceeds even when middleware type signatures cannot be
        /// verified.
        #[arg(long, default_value_t = false)]
        skip_type_check: bool,

        /// Fail the splice if any tier-3/4 strategy's bound doesn't fit
        /// a matched interface. Default: skip those matches and warn.
        #[arg(long, default_value_t = false)]
        strict: bool,

        /// Emit a code-size breakdown. Pass `json` for a machine-readable report.
        /// Under `--plan`, glue/total are omitted.
        #[arg(
            long = "metrics",
            value_name = "FORMAT",
            num_args = 0..=1,
            default_missing_value = "table",
            value_enum,
        )]
        metrics: Option<MetricsFormat>,

        /// Write the `--metrics` report to this file instead of stderr.
        #[arg(long = "metrics-out", value_name = "PATH")]
        metrics_out: Option<PathBuf>,
    },

    /// Synthesize a composition from N individual Wasm components.
    ///
    /// Matches each component's exports to the imports of the others,
    /// topologically sorts them, and produces a single composed `.wasm`.
    /// Pass `--plan` to skip composing and instead emit a WAC file plus
    /// the equivalent `wac compose` shell command.
    ///
    /// Each argument is either a plain path (`path/to/comp.wasm`) or an
    /// aliased path (`alias=path/to/comp.wasm`).  Aliases are required
    /// when two components share the same filename stem, e.g.:
    ///
    ///   splicer compose svc0=~/dir0/service.wasm svc1=~/dir1/service.wasm
    Compose {
        /// Two or more Wasm components, each as `path` or `alias=path`.
        #[arg(value_name = "COMP_WASM", num_args = 2..)]
        wasms: Vec<String>,

        /// Path for the composed Wasm output (default: composed.wasm).
        #[arg(short = 'o', long = "output", value_name = "PATH")]
        output: Option<PathBuf>,

        /// Persist the intermediate WAC source for debugging or auditing.
        /// Bare flag uses ./output.wac; pass a path to override.
        #[arg(
            long = "emit-wac",
            value_name = "PATH",
            num_args = 0..=1,
            default_missing_value = DEFAULT_OUTPUT_WAC,
        )]
        emit_wac: Option<PathBuf>,

        /// Skip in-process compose. Persist the WAC and print the
        /// equivalent `wac compose ...` shell command to stdout.
        #[arg(long)]
        plan: bool,

        /// Package name written at the top of the generated WAC.
        #[arg(long, default_value = DEFAULT_PKG)]
        package: String,

        /// Emit a code-size breakdown. Pass `json` for a machine-readable report.
        /// Under `--plan`, glue/total are omitted.
        #[arg(
            long = "metrics",
            value_name = "FORMAT",
            num_args = 0..=1,
            default_missing_value = "table",
            value_enum,
        )]
        metrics: Option<MetricsFormat>,

        /// Write the `--metrics` report to this file instead of stderr.
        #[arg(long = "metrics-out", value_name = "PATH")]
        metrics_out: Option<PathBuf>,
    },

    /// Render the composition with each rule's matched edges highlighted.
    ///
    /// Runs the rules' `select` pass without mutating anything. The
    /// diagram's legend pairs each tag bracket with the rule that
    /// matched it.
    Preview {
        /// Splice configuration in YAML format.
        #[arg(value_name = "SPLICE_CFG")]
        splice_cfg_file: PathBuf,

        /// Pre-composed Wasm component to render.
        #[arg(value_name = "COMP_WASM")]
        comp_wasm: PathBuf,

        /// Output file (default: stdout).
        #[arg(short = 'o', long = "output", value_name = "PATH")]
        output: Option<PathBuf>,

        /// Rendering format.
        #[arg(short = 'f', long = "format", default_value = "ascii", value_enum)]
        format: PreviewFormat,

        /// Only highlight rule N (1-based).
        #[arg(long = "rule", value_name = "N")]
        rule: Option<usize>,

        /// Hide WIT type signatures.
        #[arg(long = "no-types", action = clap::ArgAction::SetTrue)]
        no_types: bool,

        /// Mermaid diagram direction (Mermaid only).
        #[arg(short = 'd', long = "direction", default_value = "lr", value_enum)]
        direction: Direction,

        /// Force ANSI color (auto-detected by default).
        #[arg(long, default_value = "auto")]
        color: ColorMode,

        /// Compile each tier-3/4 match and show only those that fit,
        /// pruning strategies that don't compile against the interface.
        /// Runs cargo (slow); default preview is selection-only.
        #[arg(long, default_value_t = false)]
        exact: bool,
    },

    /// Inspect builtin middleware shipped with this splicer.
    ///
    /// With no argument, lists every builtin and its one-line
    /// description. With a builtin name, prints the description and
    /// the table of configurable keys (name, type, default, doc) —
    /// the same keys accepted in YAML under `inject.builtin.config`.
    /// Resolves builtin bytes via the same override → cache → OCI
    /// pipeline as `splice`, so the first run for a given builtin
    /// may incur an OCI pull.
    Builtin {
        /// Builtin name to describe. Omit to list every builtin.
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
}

fn main() -> Result<()> {
    // Diagnostics off by default. Users opt in via `RUST_LOG` — e.g.
    // `RUST_LOG=splicer::adapter::filter=debug splicer splice …` to see
    // the closure walker's decisions, or `RUST_LOG=splicer=debug` for the
    // full pipeline. Writes to stderr so normal stdout output is unaffected.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .with_writer(std::io::stderr)
        .init();

    match Args::parse().command {
        Command::Splice {
            splice_cfg_file,
            comp_wasm,
            output,
            emit_wac,
            plan,
            splits_dir,
            package,
            skip_type_check,
            strict,
            metrics,
            metrics_out,
        } => run_splice(
            splice_cfg_file,
            comp_wasm,
            output,
            emit_wac,
            plan,
            splits_dir,
            package,
            skip_type_check,
            strict,
            metrics,
            metrics_out,
        ),

        Command::Compose {
            wasms,
            output,
            emit_wac,
            plan,
            package,
            metrics,
            metrics_out,
        } => run_compose(wasms, output, emit_wac, plan, package, metrics, metrics_out),

        Command::Preview {
            splice_cfg_file,
            comp_wasm,
            output,
            format,
            rule,
            no_types,
            direction,
            color,
            exact,
        } => run_preview(
            splice_cfg_file,
            comp_wasm,
            output,
            format,
            rule,
            no_types,
            direction,
            color,
            exact,
        ),

        Command::Builtin { name } => run_builtin(name),
    }
}

#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum PreviewFormat {
    #[default]
    Ascii,
    Mermaid,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum MetricsFormat {
    Table,
    Json,
}

#[allow(clippy::too_many_arguments)]
fn run_preview(
    splice_cfg_file: PathBuf,
    comp_wasm: PathBuf,
    output: Option<PathBuf>,
    format: PreviewFormat,
    rule: Option<usize>,
    no_types: bool,
    direction: Direction,
    color: ColorMode,
    exact: bool,
) -> Result<()> {
    let rules_yaml = fs::read_to_string(&splice_cfg_file)
        .with_context(|| format!("Failed to read: {}", splice_cfg_file.display()))?;

    let result = preview(PreviewRequest {
        composition_wasm: comp_wasm,
        rules_yaml,
        only_rule: rule,
        exact,
    })?;

    let opts = GraphRenderOpts::default();
    let show_types = !no_types;
    let use_color = color.resolve_for_stdout(output.is_some());

    let mut condensed = false;
    let rendered = match format {
        PreviewFormat::Ascii => {
            let max_w = terminal_columns();
            let out = generate_graph_ascii(
                &result.graph,
                &opts,
                show_types,
                max_w,
                Some(&result.highlights),
                use_color,
            );
            condensed = out.condensed;
            out.ascii
        }
        PreviewFormat::Mermaid => generate_mermaid(
            &result.graph,
            &opts,
            direction,
            show_types,
            Some(&result.highlights),
        ),
    };

    if let Some(path) = output {
        fs::write(&path, &rendered)
            .with_context(|| format!("Failed to write preview: {}", path.display()))?;
        eprintln!("Preview written to: {}", path.display());
    } else {
        println!("{}", rendered);
    }

    if condensed {
        eprintln!();
        eprintln!(
            "note: the diagram was condensed to fit; rerun with `-f mermaid` for a wider view."
        );
    }

    for rule_num in &result.unmatched_rules {
        eprintln!(
            "{}: rule {} matched no edges",
            "WARN".yellow().bold(),
            rule_num,
        );
    }
    for rule_num in &result.incompatible_rules {
        eprintln!(
            "{}: rule {} matched edges, but its strategy compiled against none of them",
            "WARN".yellow().bold(),
            rule_num,
        );
    }

    Ok(())
}

fn run_builtin(name: Option<String>) -> Result<()> {
    match name {
        None => print_builtin_list(),
        Some(n) => print_builtin_details(&n),
    }
}

/// `splicer builtin` (no arg). Renders every shipped builtin's
/// tier badge and description in three columns. A builtin whose
/// manifest can't be resolved (network error, missing section)
/// prints a placeholder rather than aborting the whole listing.
fn print_builtin_list() -> Result<()> {
    let entries = builtin_info::list_with_manifests();
    let name_width = entries.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let tier_width = entries
        .iter()
        .map(|(_, r)| tier_badge(r.as_ref()).len())
        .max()
        .unwrap_or(0);
    for (name, result) in &entries {
        let badge = tier_badge(result.as_ref());
        let desc = match result {
            Ok(Some(m)) => m.builtin.description.clone(),
            Ok(None) => "(no embedded manifest)".to_string(),
            Err(e) => format!("(manifest unavailable: {e})"),
        };
        println!(
            "  {:<nw$}  {:<tw$}  {}",
            name,
            badge,
            desc,
            nw = name_width,
            tw = tier_width,
        );
    }
    Ok(())
}

/// `[tier-N label]` badge for the `builtin list` output, or `[??]`
/// when the manifest isn't readable.
fn tier_badge(result: Result<&Option<builtin_info::Manifest>, &anyhow::Error>) -> String {
    match result {
        Ok(Some(m)) => format!(
            "[tier-{} {}]",
            u8::from(m.builtin.tier),
            m.builtin.tier.label()
        ),
        _ => "[??]".to_string(),
    }
}

/// `splicer builtin <name>`. Resolves the named builtin, extracts its
/// manifest, and renders the description + accepted config keys.
fn print_builtin_details(name: &str) -> Result<()> {
    let manifest = builtin_info::resolve_manifest(name).with_context(|| {
        let known = builtin_info::known_names();
        format!(
            "could not load manifest for builtin '{name}'. \
             Known builtins: [{}]",
            known.join(", ")
        )
    })?;
    println!("{}", name.bold().bright_white());
    println!("  {}", manifest.builtin.description.italic().white());
    println!(
        "  {}",
        format!(
            "tier-{} ({})",
            u8::from(manifest.builtin.tier),
            manifest.builtin.tier.label()
        )
        .purple()
    );
    if manifest.keys.is_empty() {
        println!();
        println!("This builtin accepts no config keys.");
        return Ok(());
    }
    println!();
    println!(
        "{}",
        "Config keys and in-YAML defaults (overridable via `inject.builtin.config:`):\n"
            .bold()
            .bright_white()
    );
    for key in &manifest.keys {
        for wrapped in wrap_doc(&key.doc, doc_wrap_width()) {
            println!("  {}{}", "/// ".dimmed(), wrapped.italic().white().dimmed());
        }
        if key.case_insensitive {
            println!(
                "  {}{}",
                "/// ".dimmed(),
                "(matched case-insensitively)".italic().white().dimmed()
            );
        }
        println!(
            "  {}: {} = {};",
            key.name.cyan(),
            key.wit_type.yellow(),
            key.default_display().green(),
        );
        println!();
    }
    Ok(())
}

/// Available columns for the wrapped doc body, accounting for the
/// `  /// ` (6-char) leading prefix. Reads `$COLUMNS` (set by most
/// shells) and falls back to 80. Clamps to a sane minimum so a tiny
/// terminal still produces something rather than one-word-per-line.
fn doc_wrap_width() -> usize {
    let cols: usize = std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(80);
    cols.saturating_sub(6).max(40)
}

/// Split a doc into whitespace-collapsed paragraphs (blank line
/// breaks) and word-wrap each paragraph at `width`. Empty docs
/// produce no lines; multi-paragraph docs are separated by an
/// empty rendered line.
fn wrap_doc(doc: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut first_para = true;
    for paragraph in doc.split("\n\n") {
        let collapsed: String = paragraph.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            continue;
        }
        if !first_para {
            out.push(String::new());
        }
        first_para = false;
        let mut line = String::new();
        for word in collapsed.split_whitespace() {
            if line.is_empty() {
                line.push_str(word);
            } else if line.len() + 1 + word.len() > width {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
            } else {
                line.push(' ');
                line.push_str(word);
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn run_splice(
    splice_cfg_file: PathBuf,
    comp_wasm: PathBuf,
    output: Option<PathBuf>,
    emit_wac: Option<PathBuf>,
    plan: bool,
    splits_dir: Option<PathBuf>,
    package: String,
    skip_type_check: bool,
    strict: bool,
    metrics: Option<MetricsFormat>,
    metrics_out: Option<PathBuf>,
) -> Result<()> {
    let rules_yaml = fs::read_to_string(&splice_cfg_file)
        .with_context(|| format!("Failed to read: {}", splice_cfg_file.display()))?;

    // Pick where splits live. Tempdir guard is held until after
    // to_wasm() runs (or leaked on failure / --plan).
    let needs_persist = plan || emit_wac.is_some() || splits_dir.is_some();
    let splits = SplitsLocation::resolve(splits_dir, needs_persist)?;

    let bundle = splice(SpliceRequest {
        composition_wasm: comp_wasm,
        rules_yaml,
        package_name: package,
        splits_dir: splits.path().to_path_buf(),
        skip_type_check,
        strict,
    })?;
    print_diagnostics(&bundle.diagnostics);
    if let Some(summary) = format_skip_summary(&bundle.skips) {
        eprintln!("{}: {summary}", "WARN".yellow().bold());
    }

    if !plan && !bundle.any_rule_matched {
        anyhow::bail!(
            "no splice rules applied: every rule failed to match any nodes; see warnings above"
        );
    }

    finish(bundle, output, emit_wac, plan, splits, metrics, metrics_out)
}

#[allow(clippy::too_many_arguments)]
fn run_compose(
    wasms: Vec<String>,
    output: Option<PathBuf>,
    emit_wac: Option<PathBuf>,
    plan: bool,
    package: String,
    metrics: Option<MetricsFormat>,
    metrics_out: Option<PathBuf>,
) -> Result<()> {
    let components: Vec<ComponentInput> = wasms
        .iter()
        .map(|entry| {
            if let Some((alias, rest)) = entry.split_once('=') {
                ComponentInput {
                    alias: Some(alias.to_string()),
                    path: PathBuf::from(rest),
                }
            } else {
                ComponentInput {
                    alias: None,
                    path: PathBuf::from(entry),
                }
            }
        })
        .collect();

    let bundle = compose(ComposeRequest {
        components,
        package_name: package,
    })?;
    print_diagnostics(&bundle.diagnostics);

    // Compose has no splits dir to manage.
    finish(
        bundle,
        output,
        emit_wac,
        plan,
        SplitsLocation::None,
        metrics,
        metrics_out,
    )
}

/// Tail-end of both subcommands: write the WAC if requested, then
/// either print the `--plan` shell command or run in-process compose
/// and write the composed `.wasm`.
#[allow(clippy::too_many_arguments)]
fn finish(
    bundle: Bundle,
    output: Option<PathBuf>,
    emit_wac: Option<PathBuf>,
    plan: bool,
    splits: SplitsLocation,
    metrics: Option<MetricsFormat>,
    metrics_out: Option<PathBuf>,
) -> Result<()> {
    if plan {
        // --plan implies --emit-wac if the user didn't pass one.
        let wac_path = emit_wac.unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT_WAC));
        write_wac(&wac_path, &bundle.wac)?;
        let wac_path_str = path_str(&wac_path)?;
        // Plan mode keeps the splits dir on disk so the printed
        // command actually works.
        splits.persist();
        println!("{}", bundle.wac_compose_cmd(wac_path_str));
        eprintln!(
            "{}",
            format!("WAC saved to: {}", wac_path.display()).dimmed()
        );
        if let Some(fmt) = metrics {
            // No compose ran, so glue/total are undefined
            emit_metrics(&bundle.size_report, fmt, metrics_out.as_deref())?;
        }
        return Ok(());
    }

    // Default mode: optionally persist the WAC, then compose to wasm.
    if let Some(ref wac_path) = emit_wac {
        write_wac(wac_path, &bundle.wac)?;
    }

    let composed = match bundle.to_wasm() {
        Ok(b) => b,
        Err(e) => return Err(handle_compose_failure(e, &bundle, emit_wac, splits)),
    };

    if let Some(fmt) = metrics {
        let mut report = bundle.size_report.clone();
        report.set_composed(composed.len() as u64);
        emit_metrics(&report, fmt, metrics_out.as_deref())?;
    }

    let output_path = output.unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT_WASM));
    fs::write(&output_path, &composed)
        .with_context(|| format!("Failed to write composed wasm: {}", output_path.display()))?;
    Ok(())
}

/// Render the size report and write it to `out` (or stderr when `None`).
fn emit_metrics(report: &SizeReport, format: MetricsFormat, out: Option<&Path>) -> Result<()> {
    let rendered = match format {
        MetricsFormat::Table => report.to_table(),
        MetricsFormat::Json => report.to_json(),
    };
    match out {
        Some(path) => {
            fs::write(path, &rendered)
                .with_context(|| format!("Failed to write metrics: {}", path.display()))?;
            eprintln!(
                "{}",
                format!("metrics written to: {}", path.display()).dimmed()
            );
        }
        None => eprintln!("{rendered}"),
    }
    Ok(())
}

/// On compose failure: persist the WAC (if not already), keep the
/// splits dir on disk, and surface a single error containing both
/// paths plus the standalone `wac compose` shell command for repro.
fn handle_compose_failure(
    err: anyhow::Error,
    bundle: &Bundle,
    emit_wac: Option<PathBuf>,
    splits: SplitsLocation,
) -> anyhow::Error {
    let wac_path = match emit_wac {
        Some(p) => p,
        None => match persist_wac_on_failure(&bundle.wac) {
            Ok(p) => p,
            Err(write_err) => {
                return err.context(format!(
                    "in-process compose failed and WAC could not be preserved: {write_err:#}"
                ));
            }
        },
    };

    // Keep splits on disk so the printed command's deps still resolve.
    let splits_path = splits.persist();

    let wac_path_str = match wac_path.to_str() {
        Some(s) => s,
        None => {
            return err.context(format!(
                "in-process compose failed; WAC saved at non-UTF-8 path {}",
                wac_path.display()
            ));
        }
    };
    let repro = bundle.wac_compose_cmd(wac_path_str);

    let mut msg = format!(
        "in-process compose failed.\n\nWAC preserved at: {}",
        wac_path.display()
    );
    if let Some(sp) = splits_path {
        msg.push_str(&format!("\nSplits preserved at: {}", sp.display()));
    }
    msg.push_str("\n\nReproduce standalone with:\n");
    msg.push_str(&repro);

    err.context(msg)
}

/// Write the WAC to a leaked tempdir under `splicer-failed-<rand>/`
/// when the user didn't ask for a specific path. Returned path is
/// absolute so the surrounding error message is copy-paste-able.
fn persist_wac_on_failure(wac: &str) -> Result<PathBuf> {
    let dir = tempfile::Builder::new()
        .prefix("splicer-failed-")
        .tempdir()
        .context("Failed to create tempdir for WAC preservation")?;
    let path = dir.keep().join("output.wac");
    write_wac(&path, wac)?;
    Ok(path)
}

fn write_wac(path: &Path, wac: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create directory for WAC: {}", parent.display())
            })?;
        }
    }
    fs::write(path, wac).with_context(|| format!("Failed to write WAC: {}", path.display()))
}

fn path_str(p: &Path) -> Result<&str> {
    p.to_str()
        .ok_or_else(|| anyhow::anyhow!("path contains non-UTF-8 bytes: {}", p.display()))
}

/// Where splits are written, plus the optional tempdir handle that
/// keeps them alive. Drop the handle to clean up; call
/// [`SplitsLocation::persist`] to keep the splits on disk past the
/// process exit (used by `--plan` and on compose failure).
enum SplitsLocation {
    /// No splits dir for this run (e.g. the `compose` subcommand,
    /// which composes from individual components and doesn't split).
    None,
    /// User-supplied or default-on-disk path. Always preserved.
    Persistent(PathBuf),
    /// Tempdir, cleaned up on drop unless `persist()` is called.
    Temp(tempfile::TempDir),
}

impl SplitsLocation {
    fn resolve(user_path: Option<PathBuf>, needs_persist: bool) -> Result<Self> {
        if let Some(p) = user_path {
            return Ok(Self::Persistent(p));
        }
        if needs_persist {
            return Ok(Self::Persistent(PathBuf::from(DEFAULT_SPLITS_DIR)));
        }
        let dir = tempfile::Builder::new()
            .prefix("splicer-splits-")
            .tempdir()
            .context("Failed to create tempdir for splits")?;
        Ok(Self::Temp(dir))
    }

    fn path(&self) -> &Path {
        match self {
            // The lib's `splice()` requires a splits_dir; for compose
            // we never invoke split_out_composition, so this branch
            // never returns to the lib. "" is a sentinel.
            Self::None => Path::new(""),
            Self::Persistent(p) => p.as_path(),
            Self::Temp(d) => d.path(),
        }
    }

    /// Consume `self` and ensure the splits dir survives process
    /// exit. Returns the path on disk (`None` for `Self::None`).
    fn persist(self) -> Option<PathBuf> {
        match self {
            Self::None => None,
            Self::Persistent(p) => Some(p),
            Self::Temp(d) => Some(d.keep()),
        }
    }
}

/// Render the diagnostics list to stderr with the same colored
/// styling the CLI has always used. Library callers (and
/// `splicer::splice` / `splicer::compose`) handle their own
/// diagnostics through the returned `Vec<ContractResult>`.
fn print_diagnostics(diagnostics: &[ContractResult]) {
    for diag in diagnostics {
        match diag {
            ContractResult::Ok => {}
            // Tier{1,2}Compatible are consumed inside `splicer::splice`
            // / `splicer::compose` — tier-1 dispatches to adapter
            // generation; tier-2 currently bails before reaching this
            // diagnostics path. Neither should surface here.
            ContractResult::Tier1Compatible(_) => unreachable!(
                "Tier1Compatible should not surface in the diagnostics list returned by splicer::splice"
            ),
            ContractResult::Tier2Compatible(_) => unreachable!(
                "Tier2Compatible should not surface in the diagnostics list returned by splicer::splice"
            ),
            ContractResult::Warn(msg) => {
                eprintln!("{}: {}", "WARN".yellow().bold(), msg.yellow())
            }
            // splicer::splice would have returned Err unless
            // skip_type_check was set; seeing an Error here means the
            // caller asked us to demote it.
            ContractResult::Error(msg) => eprintln!(
                "{}: type check skipped — {}",
                "WARN".yellow().bold(),
                msg.yellow()
            ),
        }
    }
}
