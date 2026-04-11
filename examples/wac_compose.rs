//! End-to-end demo: drive `wac compose` programmatically using
//! splicer's output, without ever shelling out to the `wac` CLI.
//!
//! Run with:
//!
//! ```text
//! cargo run --example wac_compose
//! ```
//!
//! What this does:
//!
//! 1. Compiles two trivial component WATs (a `provider-a` and a
//!    `consumer` that imports it).
//! 2. Writes them to a temp directory.
//! 3. Calls [`splicer::compose`] to discover the composition graph and
//!    emit a WAC source plus a `wac_deps` map.
//! 4. Hands the WAC source + `wac_deps` to the `wac-parser`,
//!    `wac-resolver`, and `wac-graph` crates to produce composed
//!    component bytes — exactly the work the `wac compose` shell
//!    command would do, just in-process.
//! 5. Validates the resulting bytes with `wasmparser` so the example
//!    fails loudly if the pipeline produces something invalid.
//!
//! The point of the example: **`SpliceOutput::wac_deps` and
//! `ComposeOutput::wac_deps` are designed to plug straight into
//! `wac_resolver::FileSystemPackageResolver`'s `overrides` argument.**
//! No translation step is required.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use splicer::{compose, ComponentInput, ComposeRequest};
use wac_graph::EncodeOptions;
use wac_parser::Document;
use wac_resolver::{packages, FileSystemPackageResolver};

// ── WAT fixtures ──────────────────────────────────────────────────────────
//
// `provider-a` imports `host:env/dep@0.1.0` and re-exports its `get`
// function under `my:providers/a@0.1.0`. `consumer` imports that
// interface and re-exposes the same function under
// `my:consumer/app@0.1.0`. Both reference an identical func type
// (re-exported through aliases), so the canonical-ABI fingerprints
// match and splicer's contract check is happy.
//
// The composed binary keeps `host:env/dep@0.1.0` as an unresolved
// host import — that's normal: wac compose will leave it for the
// host runtime to satisfy at instantiation time.

const WAT_PROVIDER_A: &str = r#"(component
    (import "host:env/dep@0.1.0" (instance $dep
        (export "get" (func (result u32)))
    ))
    (alias export $dep "get" (func $f))
    (instance $out (export "get" (func $f)))
    (export "my:providers/a@0.1.0" (instance $out))
)"#;

const WAT_CONSUMER: &str = r#"(component
    (import "my:providers/a@0.1.0" (instance $a
        (export "get" (func (result u32)))
    ))
    (alias export $a "get" (func $f))
    (instance $out (export "get" (func $f)))
    (export "my:consumer/app@0.1.0" (instance $out))
)"#;

fn main() -> Result<()> {
    // ── 1. Compile the two WAT components and write them to disk ─────────
    let tmp = tempfile::tempdir().context("create temp dir")?;
    let provider_path = tmp.path().join("provider-a.wasm");
    let consumer_path = tmp.path().join("consumer.wasm");

    let provider_bytes = wat::parse_str(WAT_PROVIDER_A).context("compile provider WAT")?;
    let consumer_bytes = wat::parse_str(WAT_CONSUMER).context("compile consumer WAT")?;

    std::fs::write(&provider_path, &provider_bytes).context("write provider wasm")?;
    std::fs::write(&consumer_path, &consumer_bytes).context("write consumer wasm")?;

    println!("✓ wrote provider and consumer wasm to {}", tmp.path().display());

    // ── 2. Run splicer::compose ──────────────────────────────────────────
    //
    // Note: we explicitly alias the components so the WAC variable
    // names are deterministic (`provider-a`, `consumer`) regardless
    // of what the on-disk filenames look like.
    let out = compose(ComposeRequest {
        components: vec![
            ComponentInput {
                alias: Some("provider-a".to_string()),
                path: provider_path.clone(),
            },
            ComponentInput {
                alias: Some("consumer".to_string()),
                path: consumer_path.clone(),
            },
        ],
        package_name: "example:composition".to_string(),
    })?;

    println!("✓ splicer::compose produced {} bytes of WAC source", out.wac.len());
    println!("  wac_deps:");
    for (key, path) in &out.wac_deps {
        println!("    {key} = {}", path.display());
    }

    // ── 3. Parse the WAC source ──────────────────────────────────────────
    let doc = Document::parse(&out.wac).context("parse WAC source")?;
    println!("✓ wac_parser::Document::parse succeeded");

    // ── 4. Discover the package keys the document references ────────────
    let keys = packages(&doc).context("discover packages from WAC")?;
    println!(
        "✓ wac_resolver::packages found {} package reference(s)",
        keys.len()
    );

    // ── 5. Resolve the package keys against splicer's wac_deps ──────────
    //
    // `FileSystemPackageResolver::new` wants `HashMap<String, PathBuf>`.
    // splicer hands us a `BTreeMap<String, PathBuf>` — collecting it
    // into a HashMap is a one-liner because the keys/values already
    // match the expected shape.
    let overrides: HashMap<String, PathBuf> = out.wac_deps.into_iter().collect();
    let resolver = FileSystemPackageResolver::new(tmp.path(), overrides, true);
    let pkgs = resolver
        .resolve(&keys)
        .context("resolve package keys against splicer's wac_deps")?;
    println!("✓ FileSystemPackageResolver::resolve succeeded");

    // ── 6. Resolve the document and encode the composed component ──────
    let resolution = doc.resolve(pkgs).context("resolve WAC document")?;
    let composed: Vec<u8> = resolution
        .encode(EncodeOptions::default())
        .context("encode composed component")?;
    println!("✓ resolution.encode produced {} bytes of composed wasm", composed.len());

    // ── 7. Validate the composed bytes ──────────────────────────────────
    let mut validator =
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    validator
        .validate_all(&composed)
        .context("validate composed component bytes")?;
    println!("✓ wasmparser validated the composed component");

    println!();
    println!("All done — splicer's output drove the wac crates end-to-end.");
    Ok(())
}
