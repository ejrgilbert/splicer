mod compose;
mod contract;
mod parse;
mod adapter;
mod split;
#[cfg(test)]
mod tests;
mod wac;

use crate::contract::ContractResult;
use crate::wac::INST_PREFIX;
use colored::Colorize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::compose::filename_from_path;
use crate::parse::config::SpliceRule;
use crate::split::split_out_composition;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cviz::parse::component::parse_component;

const DEFAULT_PKG: &str = "example:composition";

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

    /// Synthesise a composition from N individual Wasm components.
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
            let yaml_str = fs::read_to_string(&splice_cfg_file)
                .with_context(|| format!("Failed to read: {}", splice_cfg_file.display()))?;
            let cfg = parse::config::parse_yaml(&yaml_str).with_context(|| {
                format!(
                    "Failed to parse splice configuration: {}",
                    splice_cfg_file.display()
                )
            })?;

            let bytes = fs::read(&comp_wasm)?;
            let graph = parse_component(&bytes).with_context(|| {
                format!(
                    "Failed to parse composition graph from: {}",
                    comp_wasm.display()
                )
            })?;

            let (splits_path, shim_comps) = split_out_composition(&comp_wasm, &dir_splits)?;

            run_wac(
                shim_comps,
                &splits_path,
                &graph,
                &cfg,
                None,
                &package,
                output_wac,
                skip_type_check,
            )
        }

        Command::Compose {
            wasms,
            output_wac,
            package,
        } => {
            // Parse each entry as `alias=path` or bare `path`, then validate
            // that all resolved names are unique before any composition work.
            let mut components: Vec<(String, PathBuf, Vec<u8>)> = Vec::with_capacity(wasms.len());

            for entry in &wasms {
                let (name, path) = if let Some((alias, rest)) = entry.split_once('=') {
                    (alias.to_string(), PathBuf::from(rest))
                } else {
                    let path = PathBuf::from(entry);
                    (filename_from_path(&path), path)
                };

                let bytes = fs::read(&path).with_context(|| {
                    format!("Failed to read Wasm component: {}", path.display())
                })?;
                components.push((name, path, bytes));
            }

            // Duplicate-name check: surface a clear error before attempting
            // composition so the user knows exactly what went wrong.
            {
                let mut seen: HashMap<&str, &PathBuf> = HashMap::new();
                for (name, path, _) in &components {
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

            let (graph, node_paths) = compose::build_graph_from_components(&components)?;

            run_wac(
                HashMap::new(),
                "",
                &graph,
                &[],
                Some(&node_paths),
                &package,
                output_wac,
                false,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_wac(
    shim_comps: HashMap<usize, usize>,
    splits_path: &str,
    graph: &cviz::model::CompositionGraph,
    rules: &[SpliceRule],
    node_paths: Option<&HashMap<u32, PathBuf>>,
    pkg_name: &str,
    output_wac: Option<PathBuf>,
    skip_type_check: bool,
) -> Result<()> {
    let out = wac::generate_wac(shim_comps, splits_path, graph, rules, node_paths, pkg_name)?;

    for diag in out.diagnostics {
        match diag {
            ContractResult::Ok => {}
            // Tier1Compatible is fully consumed inside generate_wac (adapter is generated
            // and the injection path is substituted).  It should never surface here.
            ContractResult::Tier1Compatible(_) => unreachable!(
                "Tier1Compatible must be consumed by add_to_inject_plan before reaching diagnostics"
            ),
            ContractResult::Warn(msg) => eprintln!("{}: {}", "WARN".yellow().bold(), msg.yellow()),
            ContractResult::Error(msg) => {
                if skip_type_check {
                    eprintln!(
                        "{}: type check skipped — {}",
                        "WARN".yellow().bold(),
                        msg.yellow()
                    );
                } else {
                    panic!("ERROR: {msg}");
                }
            }
        }
    }

    let output_path = output_wac.unwrap_or_else(|| PathBuf::from("output.wac"));
    fs::write(&output_path, &out.wac)
        .with_context(|| format!("Failed to write output: {}", output_path.display()))?;
    eprintln!("Generated `wac` written to: {}\n", output_path.display());

    let wac_cmd = gen_wac_cmd(output_path.into_os_string().to_str().unwrap(), out.cmd_args)?;
    println!("{wac_cmd}");

    Ok(())
}

fn gen_wac_cmd(wac_path: &str, cmd_args: Vec<(String, String)>) -> Result<String> {
    let mut cmd = format!("wac compose {wac_path} ");
    for (srv_name, srv_path) in cmd_args {
        cmd.push_str(&format!(
            "\\\n    --dep {INST_PREFIX}:{srv_name}=\"{srv_path}\" "
        ));
    }
    Ok(cmd)
}
