//! Spike: validate that splicer's planned WIT-level adapter-generator
//! rewrite is actually viable.
//!
//! Three things need to hold for the new architecture to work:
//!
//! 1. `wit_component::decode` faithfully extracts a `Resolve` + `WorldId`
//!    from a wit-bindgen-compiled component.
//! 2. `wit_parser::Resolve::push_path` can merge an external WIT file
//!    (`wit/tier1/world.wit`) into that same Resolve.
//! 3. `wit_component::ComponentEncoder::module(...).encode()` produces a
//!    valid component when given a core wasm module + the merged WIT.
//!    For this spike we hand it `wit_component::dummy_module(...)` which
//!    is a no-op core module that satisfies the world's import/export
//!    contract — that's enough to prove ComponentEncoder accepts our
//!    flow without committing to a real dispatch core module yet.
//!
//! Run with: `cargo run --example wit_component_spike`. Writes the
//! produced component to `/tmp/spike_adapter.wasm`.

use anyhow::{Context, Result};
use std::path::PathBuf;
use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
use wit_parser::{LiftLowerAbi, ManglingAndAbi, Resolve};

/// Spike WIT — adds a string param + string result on top of the
/// primitive case so we can read off what cabi_realloc / cabi_post
/// shape ComponentEncoder demands when strings are present.
const SPIKE_WIT: &str = r#"
package spike:demo;

interface api {
    echo: func(input: string) -> string;
}

world adapter {
    import api;
    export api;
    import splicer:tier1/before@0.1.0;
    import splicer:tier1/after@0.1.0;
}
"#;

const TIER1_WORLD_WIT: &str = "wit/tier1/world.wit";

fn main() -> Result<()> {
    // ── Step 1: build a Resolve from inline WIT + tier1's world.wit.
    //    This is the "we construct the WIT in memory" half of the
    //    rewrite plan: splicer would do this from the input component's
    //    discovered interfaces plus the tier1 file.
    let mut resolve = Resolve::default();
    // Push tier1 first so the spike's `import splicer:tier1/...` lines
    // resolve against an already-known package.
    let tier1_path = std::env::current_dir()?.join(TIER1_WORLD_WIT);
    let (tier1_pkg, _tier1_paths) = resolve
        .push_path(&tier1_path)
        .with_context(|| format!("merge tier1 WIT from {}", tier1_path.display()))?;
    let spike_pkg = resolve
        .push_str("spike.wit", SPIKE_WIT)
        .context("parse inline spike WIT")?;

    eprintln!(
        "[spike] resolve has {} packages: {:?}",
        resolve.packages.len(),
        resolve
            .packages
            .iter()
            .map(|(_, p)| p.name.to_string())
            .collect::<Vec<_>>()
    );
    let _ = (spike_pkg, tier1_pkg);

    // ── Step 2: pick the spike `adapter` world.
    let world_id = resolve
        .select_world(&[spike_pkg], Some("adapter"))
        .context("select adapter world")?;
    let world = &resolve.worlds[world_id];
    eprintln!(
        "[spike] world `{}`: {} imports, {} exports",
        world.name,
        world.imports.len(),
        world.exports.len(),
    );

    // ── Step 3: synthesize a dummy core module + embed metadata so we
    //    can read off what shape ComponentEncoder demands for the
    //    current spike WIT. Useful as a discovery tool; production
    //    splicer emits its own dispatch module instead.
    let mut core_module = dummy_module(
        &resolve,
        world_id,
        ManglingAndAbi::Legacy(LiftLowerAbi::Sync),
    );
    embed_component_metadata(&mut core_module, &resolve, world_id, StringEncoding::UTF8)
        .context("embed_component_metadata")?;
    eprintln!("[spike] dispatch core module: {} bytes", core_module.len());
    std::fs::write("/tmp/spike_dispatch_core.wasm", &core_module)?;

    // ── Step 4: encode core module → component.
    let encoded = ComponentEncoder::default()
        .validate(true)
        .module(&core_module)
        .context("ComponentEncoder::module")?
        .encode()
        .context("ComponentEncoder::encode")?;

    let out_path = PathBuf::from("/tmp/spike_adapter.wasm");
    std::fs::write(&out_path, &encoded)?;
    eprintln!(
        "[spike] encoded component: {} ({} bytes)",
        out_path.display(),
        encoded.len(),
    );

    // ── Step 5: independent validation via wasmparser.
    let mut validator =
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    validator
        .validate_all(&encoded)
        .context("post-encode validation")?;

    println!("[spike] OK — push WIT → encode → validate all passed.");
    println!("[spike] Output: {}", out_path.display());
    Ok(())
}

