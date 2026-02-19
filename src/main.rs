mod parse;
#[cfg(test)]
mod tests;
mod wac;
mod split;

use crate::wac::INST_PREFIX;
use std::fs;
use std::path::PathBuf;

use crate::parse::config::SpliceRule;
use anyhow::{Context, Result};
use clap::Parser;
use cviz::model::CompositionGraph;
use cviz::parse::component::parse_component;
use crate::split::split_out_composition;

#[derive(Parser, Debug)]
#[command(name = "splicer")]
#[command(
    version,
    about = "Plan how to splice middleware into a WebAssembly component."
)]
#[command(after_long_help = r#"
SPLICE CONFIG FORMAT (YAML)

This splice configuration describes how middleware components
should be inserted into a composition graph.

Minimal example:

Full format documentation:
https://github.com/ejrgilbert/component-interposition/blob/main/splice-config.md
"#)]
struct Args {
    /// Path to the Wasm component binary.
    #[arg(value_name = "COMP_WASM")]
    wasm: PathBuf,

    /// Path to the splice configuration in YAML format.
    #[arg(value_name = "SPLICE_CFG")]
    splice_cfg_file: PathBuf,

    /// Output destination for the generated wac (flushed to output.wac if not specified)
    #[arg(short, long)]
    output_wac: Option<PathBuf>,

    /// Output destination for the split out subcomponents of the Wasm component binary.
    #[arg(short, long)]
    dir_splits: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let graph = get_graph(&args)?;
    let cfg = get_cfg(&args)?;

    let (splits_path, shim_comps) = gen_splits(&args)?;
    let (wac, cmd_args) = wac::generate_wac(shim_comps, &splits_path, &graph, &cfg);

    let output_path = if let Some(output_path) = args.output_wac {
        output_path
    } else {
        PathBuf::from("output.wac")
    };

    fs::write(&output_path, wac)
        .with_context(|| format!("Failed to write output: {}", output_path.display()))?;
    eprintln!("Generated `wac` written to: {}\n", output_path.display());


    let wac_cmd = gen_wac_cmd(output_path.into_os_string().to_str().unwrap(), cmd_args)?;
    println!("{wac_cmd}");

    Ok(())
}

fn gen_splits(args: &Args) -> Result<(String, Vec<usize>)> {
    split_out_composition(&args.wasm, &args.dir_splits)
}

fn gen_wac_cmd(wac_path: &str, cmd_args: Vec<(String, String)>) -> Result<String> {
    let mut cmd = format!("wac compose {wac_path} ");

    for (srv_name, srv_path) in cmd_args {
        cmd.push_str(&format!("\\\n    --dep {INST_PREFIX}:{srv_name}=\"{srv_path}\" "));
    }

    Ok(cmd)
}

fn get_graph(args: &Args) -> Result<CompositionGraph> {
    // Parse the graph
    let bytes = fs::read(&args.wasm)?;
    parse_component(&bytes).with_context(|| {
        format!(
            "Failed to parse composition graph from Wasm component: {}",
            args.wasm.display()
        )
    })
}

fn get_cfg(args: &Args) -> Result<Vec<SpliceRule>> {
    // Read the splice config file
    let yaml_str = fs::read_to_string(&args.splice_cfg_file)
        .with_context(|| format!("Failed to read file: {}", args.splice_cfg_file.display()))?;

    // Parse the config
    parse::config::parse_yaml(&yaml_str).with_context(|| {
        format!(
            "Failed to parse splice configuration: {}",
            args.splice_cfg_file.display()
        )
    })
}
