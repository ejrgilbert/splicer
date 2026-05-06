use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use std::fs;
use std::path::{Path, PathBuf};

use splicer::types::ContractResult;
use splicer::{compose, splice, Bundle, ComponentInput, ComposeRequest, SpliceRequest};

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
        } => run_splice(
            splice_cfg_file,
            comp_wasm,
            output,
            emit_wac,
            plan,
            splits_dir,
            package,
            skip_type_check,
        ),

        Command::Compose {
            wasms,
            output,
            emit_wac,
            plan,
            package,
        } => run_compose(wasms, output, emit_wac, plan, package),
    }
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
    })?;
    print_diagnostics(&bundle.diagnostics);

    finish(bundle, output, emit_wac, plan, splits)
}

fn run_compose(
    wasms: Vec<String>,
    output: Option<PathBuf>,
    emit_wac: Option<PathBuf>,
    plan: bool,
    package: String,
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
    finish(bundle, output, emit_wac, plan, SplitsLocation::None)
}

/// Tail-end of both subcommands: write the WAC if requested, then
/// either print the `--plan` shell command or run in-process compose
/// and write the composed `.wasm`.
fn finish(
    bundle: Bundle,
    output: Option<PathBuf>,
    emit_wac: Option<PathBuf>,
    plan: bool,
    splits: SplitsLocation,
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

    let output_path = output.unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT_WASM));
    fs::write(&output_path, &composed)
        .with_context(|| format!("Failed to write composed wasm: {}", output_path.display()))?;
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
            // Tier1Compatible is consumed inside `splicer::splice` /
            // `splicer::compose` (the adapter is generated and the
            // injection path is substituted), so it should never reach
            // a user-facing diagnostic list.
            ContractResult::Tier1Compatible(_) => unreachable!(
                "Tier1Compatible should not surface in the diagnostics list returned by splicer::splice"
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
