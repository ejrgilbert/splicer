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
//! 4. Calls [`ComposeOutput::to_wasm`] to turn that into a single
//!    composed Wasm component, in-process — no shell-out, no manual
//!    plumbing through the `wac-*` crates.
//!
//! The point of the example: **`splicer::compose` plus `.to_wasm()` is
//! the whole pipeline.** If you need finer control (e.g. a custom
//! search base for unresolved package references), reach for the
//! free [`splicer::compose_wac`] function instead.

use anyhow::{Context, Result};
use splicer::{compose, ComponentInput, ComposeRequest};

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

    println!(
        "✓ wrote provider and consumer wasm to {}",
        tmp.path().display()
    );

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

    println!(
        "✓ splicer::compose produced {} bytes of WAC source, {} dep(s)",
        out.wac.len(),
        out.wac_deps.len()
    );

    // ── 3. Compose to a single Wasm component, in-process ────────────────
    let composed = out.to_wasm().context("compose to wasm")?;
    println!(
        "✓ ComposeOutput::to_wasm produced and validated {} bytes",
        composed.len()
    );

    println!();
    println!("All done — splicer's output became a composed component without leaving the process.");
    Ok(())
}
