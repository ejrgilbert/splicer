use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use std::fs;
use std::path::PathBuf;

use splicer::types::ContractResult;
use splicer::{compose, splice, ComponentInput, ComposeRequest, SpliceRequest};

const DEFAULT_PKG: &str = "example:composition";
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
    /// Reads the splice configuration, splits the composed binary, and emits
    /// a WAC file + the `wac compose` command needed to reassemble it with the
    /// injected middleware.
    Splice {
        /// Path to the splice configuration in YAML format.
        #[arg(value_name = "SPLICE_CFG")]
        splice_cfg_file: PathBuf,

        /// Pre-composed Wasm component binary to splice into.
        #[arg(value_name = "COMP_WASM")]
        comp_wasm: PathBuf,

        /// Output destination for the generated WAC (defaults to output.wac).
        #[arg(short, long)]
        output_wac: Option<PathBuf>,

        /// Directory where split sub-components are written.
        #[arg(short, long)]
        dir_splits: Option<String>,

        /// Package name written at the top of the generated WAC.
        #[arg(long, default_value = DEFAULT_PKG)]
        package: String,

        /// Demote type-incompatibility errors to warnings so injection proceeds
        /// even when middleware type signatures cannot be verified.
        #[arg(long, default_value_t = false)]
        skip_type_check: bool,
    },

    /// Synthesize a composition from N individual Wasm components.
    ///
    /// Matches each component's exports to the imports of the others,
    /// topologically sorts them, and emits a WAC file + the `wac compose`
    /// command needed to build the final composed binary.
    ///
    /// No splice configuration is required — the composition graph is
    /// discovered automatically from the components' import/export surfaces.
    ///
    /// Each argument is either a plain path (`path/to/comp.wasm`) or an
    /// aliased path (`alias=path/to/comp.wasm`).  Aliases are required when
    /// two components share the same filename stem, e.g.:
    ///
    ///   splicer compose svc0=~/dir0/service.wasm svc1=~/dir1/service.wasm
    Compose {
        /// Two or more Wasm components, each as `path` or `alias=path`.
        #[arg(value_name = "COMP_WASM", num_args = 2..)]
        wasms: Vec<String>,

        /// Output destination for the generated WAC (defaults to output.wac).
        #[arg(short, long)]
        output_wac: Option<PathBuf>,

        /// Package name written at the top of the generated WAC.
        #[arg(long, default_value = DEFAULT_PKG)]
        package: String,
    },
}

fn main() -> Result<()> {
    match Args::parse().command {
        Command::Splice {
            splice_cfg_file,
            comp_wasm,
            output_wac,
            dir_splits,
            package,
            skip_type_check,
        } => {
            let rules_yaml = fs::read_to_string(&splice_cfg_file)
                .with_context(|| format!("Failed to read: {}", splice_cfg_file.display()))?;
            let splits_dir =
                PathBuf::from(dir_splits.unwrap_or_else(|| DEFAULT_SPLITS_DIR.to_string()));
            let out = splice(SpliceRequest {
                composition_wasm: comp_wasm,
                rules_yaml,
                package_name: package,
                splits_dir,
                skip_type_check,
            })?;

            print_diagnostics(&out.diagnostics, skip_type_check);
            write_and_announce(&out.wac, output_wac, |path| out.wac_compose_cmd(path))
        }

        Command::Compose {
            wasms,
            output_wac,
            package,
        } => {
            // Parse each entry as `alias=path` or bare `path`. The
            // duplicate-name check + file reads happen inside
            // `splicer::compose`.
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

            let out = compose(ComposeRequest {
                components,
                package_name: package,
            })?;

            print_diagnostics(&out.diagnostics, false);
            write_and_announce(&out.wac, output_wac, |path| out.wac_compose_cmd(path))
        }
    }
}

/// Write the generated WAC source to disk and print the
/// `wac compose` invocation that consumes it.
fn write_and_announce(
    wac: &str,
    output_wac: Option<PathBuf>,
    format_cmd: impl FnOnce(&str) -> String,
) -> Result<()> {
    let output_path = output_wac.unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT_WAC));
    fs::write(&output_path, wac)
        .with_context(|| format!("Failed to write output: {}", output_path.display()))?;
    eprintln!("Generated `wac` written to: {}\n", output_path.display());
    let wac_path_str = output_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "output WAC path contains non-UTF-8 bytes: {}",
            output_path.display()
        )
    })?;
    println!("{}", format_cmd(wac_path_str));
    Ok(())
}

/// Render the diagnostics list to stderr with the same colored
/// styling the CLI has always used. Library callers (and
/// `splicer::splice` / `splicer::compose`) handle their own
/// diagnostics through the returned `Vec<ContractResult>`.
fn print_diagnostics(diagnostics: &[ContractResult], skip_type_check: bool) {
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
            ContractResult::Error(msg) => {
                // splicer::splice would have returned Err already
                // unless skip_type_check was set, so seeing one here
                // means the caller asked us to demote it.
                let _ = skip_type_check;
                eprintln!(
                    "{}: type check skipped — {}",
                    "WARN".yellow().bold(),
                    msg.yellow()
                );
            }
        }
    }
}
