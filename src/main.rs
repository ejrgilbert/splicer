mod parse;
#[cfg(test)]
mod tests;
mod wac;

use std::path::PathBuf;

use crate::parse::config::SpliceRule;
use anyhow::{Context, Result};
use clap::Parser;
use cviz::model::CompositionGraph;
use cviz::parse::json;

#[derive(Parser, Debug)]
#[command(name = "splicer")]
#[command(version, about = "Plan how to splice middleware into a WebAssembly component.")]
#[command(after_long_help = r#"
SPLICE CONFIG FORMAT (YAML)

This splice configuration describes how middleware components
should be inserted into a composition graph.

Minimal example:

Full format documentation:
https://github.com/ejrgilbert/component-interposition/blob/main/splice-config.md
"#)]
struct Args {
    /// Path to the composition graph in JSON format.
    #[arg(value_name = "JSON_GRAPH")]
    json_graph_file: PathBuf,

    /// Path to the splice configuration in YAML format.
    #[arg(value_name = "SPLICE_CFG")]
    splice_cfg_file: PathBuf,

    /// Output file (stdout if not specified)
    #[arg(short, long)]
    output: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let graph = get_graph(&args)?;
    let cfg = get_cfg(&args)?;

    let wac = wac::generate_wac(&graph, &cfg);
    if let Some(output_path) = args.output {
        std::fs::write(&output_path, wac)
            .with_context(|| format!("Failed to write output: {}", output_path.display()))?;
        eprintln!("Diagram written to: {}", output_path.display());
    } else {
        println!("\n{wac}");
    }

    Ok(())
}

fn get_graph(args: &Args) -> Result<CompositionGraph> {
    // Read the graph file
    let file = std::fs::File::open(&args.json_graph_file)
        .with_context(|| format!("Failed to read file: {}", args.json_graph_file.display()))?;

    // Parse the graph
    json::parse_json(&file).with_context(|| {
        format!(
            "Failed to parse composition graph: {}",
            args.json_graph_file.display()
        )
    })
}

fn get_cfg(args: &Args) -> Result<Vec<SpliceRule>> {
    // Read the splice config file
    let yaml_str = std::fs::read_to_string(&args.splice_cfg_file)
        .with_context(|| format!("Failed to read file: {}", args.splice_cfg_file.display()))?;

    // Parse the config
    parse::config::parse_yaml(&yaml_str).with_context(|| {
        format!(
            "Failed to parse splice configuration: {}",
            args.splice_cfg_file.display()
        )
    })
}
