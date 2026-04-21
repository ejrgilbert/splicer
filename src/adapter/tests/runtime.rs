//! Phase 2 commit 1: end-to-end runtime pipeline scaffolding.
//!
//! Scaffolds three Rust crates (provider, consumer, middleware) plus
//! their WIT into a tempdir, runs the full splicer pipeline
//! (cargo build → wasm-tools component new → wac compose →
//! splicer splice → wac compose), and validates that the final
//! composed component parses under `wasmparser::Validator`.
//!
//! What this commit proves: the pipeline machinery (scaffolding,
//! cargo, wit-bindgen, component adaptation, compose, splice) works
//! end-to-end for a hardcoded `foo(u32) -> u32` shape.
//!
//! Out of scope for commit 1: running the composed component under
//! wasmtime (commit 2) and parameterizing the shape (commit 3).
//!
//! `#[ignore]`'d because the cargo build chain takes tens of seconds
//! and we don't want it on every `cargo test`. Run on demand:
//!     cargo test --lib runtime_pipeline_u32 -- --ignored --nocapture

use std::path::{Path, PathBuf};
use std::process::Command;

// ─── Fixtures (hardcoded `foo(u32) -> u32` shape) ──────────────────

const WORKSPACE_CARGO_TOML: &str = r#"[workspace]
resolver = "2"
members = ["provider", "consumer", "middleware"]

[workspace.dependencies]
wit-bindgen = { version = "0.51.0", features = ["default", "async-spawn", "inter-task-wakeup", "async"] }
"#;

const PROVIDER_CARGO_TOML: &str = r#"[package]
name = "provider"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = { workspace = true }
"#;

const PROVIDER_LIB_RS: &str = r#"mod bindings {
    wit_bindgen::generate!({
        world: "provider",
        generate_all
    });
}

use bindings::exports::my::shape::api::Guest;

struct Provider;

impl Guest for Provider {
    fn foo(x: u32) -> u32 {
        x.wrapping_add(1)
    }
}

bindings::export!(Provider with_types_in bindings);
"#;

const PROVIDER_WORLD_WIT: &str = r#"package my:shape@1.0.0;

interface api {
    foo: func(x: u32) -> u32;
}

world provider {
    export api;
}
"#;

const CONSUMER_CARGO_TOML: &str = r#"[package]
name = "consumer"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = { workspace = true }
"#;

const CONSUMER_LIB_RS: &str = r#"mod bindings {
    wit_bindgen::generate!({
        world: "consumer",
        generate_all
    });
}

use bindings::exports::my::svc::app::Guest;
use bindings::my::shape::api;

struct Consumer;

impl Guest for Consumer {
    fn run() -> u32 {
        api::foo(10)
    }
}

bindings::export!(Consumer with_types_in bindings);
"#;

const CONSUMER_WORLD_WIT: &str = r#"package my:svc@1.0.0;

interface app {
    run: func() -> u32;
}

world consumer {
    export app;
    import my:shape/api@1.0.0;
}
"#;

const CONSUMER_SHAPE_DEP_WIT: &str = r#"package my:shape@1.0.0;

interface api {
    foo: func(x: u32) -> u32;
}
"#;

const MIDDLEWARE_CARGO_TOML: &str = r#"[package]
name = "middleware"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = { workspace = true }
"#;

const MIDDLEWARE_LIB_RS: &str = r#"mod bindings {
    wit_bindgen::generate!({
        world: "mdl",
        async: true,
        generate_all
    });
}

use bindings::exports::splicer::tier1::after::Guest as AfterGuest;
use bindings::exports::splicer::tier1::before::Guest as BeforeGuest;

struct Mdl;

impl BeforeGuest for Mdl {
    async fn before_call(_name: String) {}
}

impl AfterGuest for Mdl {
    async fn after_call(_name: String) {}
}

bindings::export!(Mdl with_types_in bindings);
"#;

const MIDDLEWARE_WORLD_WIT: &str = r#"package my:middleware@1.0.0;

world mdl {
    export splicer:tier1/before@0.1.0;
    export splicer:tier1/after@0.1.0;
}
"#;

const MIDDLEWARE_TIER1_DEP_WIT: &str = include_str!("../../../wit/tier1/world.wit");

const SPLICE_YAML: &str = r#"version: 1

rules:
  - between:
      interface: "my:shape/api@1.0.0"
      inner:
        name: provider-comp
      outer:
        name: consumer-comp
    inject:
      - name: mdl
        path: "middleware.comp.wasm"
"#;

// ─── Test ──────────────────────────────────────────────────────────

/// End-to-end pipeline sanity test: scaffold → build → compose →
/// splice → validate. Hardcoded to `foo(u32) -> u32`; commit 3 will
/// parameterize on `ValueType`.
#[test]
#[ignore]
fn test_runtime_pipeline_u32() {
    require_tool("cargo");
    require_tool("wasm-tools");
    require_tool("wac");

    let tmp = tempfile::tempdir().expect("mktempdir");
    let root = tmp.path();
    eprintln!("runtime_pipeline: work dir = {}", root.display());

    scaffold_workspace(root).expect("scaffold");

    run(
        Command::new("cargo")
            .args(["build", "--target", "wasm32-wasip1", "--workspace"])
            .current_dir(root),
        "cargo build",
    );

    let adapter =
        repo_root().join("tests/component-interposition/wasi_snapshot_preview1.reactor.wasm");
    assert!(
        adapter.exists(),
        "wasip1 reactor adapter missing at {}",
        adapter.display()
    );

    let provider_comp = wrap_component(root, "provider", &adapter);
    let consumer_comp = wrap_component(root, "consumer", &adapter);
    // wrap_component writes middleware.comp.wasm at root/; splice.yaml
    // references it by that relative path.
    let _middleware_comp = wrap_component(root, "middleware", &adapter);

    // Stage 1: synthesize a composition of provider + consumer via
    // `splicer compose`, which emits a WAC file + prints the exact
    // `wac compose` command that assembles the final .wasm. Same
    // pattern splicer itself will use for splicing in stage 2.
    let compose_wac = root.join("compose.wac");
    let composed_path = root.join("composed.wasm");
    let wac_cmd = emit_wac_command(
        Command::new("splicer")
            .args([
                "compose",
                provider_comp.to_str().unwrap(),
                consumer_comp.to_str().unwrap(),
                "-o",
                compose_wac.to_str().unwrap(),
            ])
            .current_dir(root),
        "splicer compose",
    );
    run_wac_command(
        &wac_cmd,
        &composed_path,
        root,
        "wac compose (provider+consumer)",
    );

    // Stage 2: splice the middleware in. Splicer prints the wac
    // command needed to reassemble; we run it.
    let splice_yaml_path = root.join("splice.yaml");
    std::fs::write(&splice_yaml_path, SPLICE_YAML).unwrap();
    let spliced_wac = root.join("spliced.wac");
    let splits_dir = root.join("splits");
    std::fs::create_dir_all(&splits_dir).unwrap();

    let splice_wac_cmd = emit_wac_command(
        Command::new("splicer")
            .args([
                "splice",
                splice_yaml_path.to_str().unwrap(),
                composed_path.to_str().unwrap(),
                "-o",
                spliced_wac.to_str().unwrap(),
                "-d",
                splits_dir.to_str().unwrap(),
            ])
            .current_dir(root),
        "splicer splice",
    );
    let final_path = root.join("final.wasm");
    run_wac_command(
        &splice_wac_cmd,
        &final_path,
        root,
        "wac compose (post-splice)",
    );

    // Validate: parse the final bytes and check the component-model
    // validator accepts them. No runtime execution this commit.
    let bytes = std::fs::read(&final_path).expect("read final.wasm");
    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    validator
        .validate_all(&bytes)
        .expect("final composed component must validate");
    eprintln!("runtime_pipeline: validated {} bytes", bytes.len());
}

// ─── Helpers ───────────────────────────────────────────────────────

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn require_tool(name: &str) {
    let status = Command::new(name)
        .arg("--version")
        .output()
        .unwrap_or_else(|e| panic!("`{name}` must be on PATH: {e}"));
    assert!(status.status.success(), "`{name} --version` failed");
}

fn run(cmd: &mut Command, label: &str) {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{label}: spawn failed: {e}"));
    if !out.status.success() {
        panic!(
            "{label}: exit {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

fn scaffold_workspace(root: &Path) -> std::io::Result<()> {
    std::fs::write(root.join("Cargo.toml"), WORKSPACE_CARGO_TOML)?;

    write_crate(
        root,
        "provider",
        PROVIDER_CARGO_TOML,
        PROVIDER_LIB_RS,
        &[("world.wit", PROVIDER_WORLD_WIT)],
    )?;
    write_crate(
        root,
        "consumer",
        CONSUMER_CARGO_TOML,
        CONSUMER_LIB_RS,
        &[
            ("world.wit", CONSUMER_WORLD_WIT),
            ("deps/my-shape-1.0.0/package.wit", CONSUMER_SHAPE_DEP_WIT),
        ],
    )?;
    write_crate(
        root,
        "middleware",
        MIDDLEWARE_CARGO_TOML,
        MIDDLEWARE_LIB_RS,
        &[
            ("world.wit", MIDDLEWARE_WORLD_WIT),
            (
                "deps/splicer-tier1-0.1.0/package.wit",
                MIDDLEWARE_TIER1_DEP_WIT,
            ),
        ],
    )?;
    Ok(())
}

fn write_crate(
    root: &Path,
    name: &str,
    cargo_toml: &str,
    lib_rs: &str,
    wit_files: &[(&str, &str)],
) -> std::io::Result<()> {
    let dir = root.join(name);
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::create_dir_all(dir.join("wit"))?;
    std::fs::write(dir.join("Cargo.toml"), cargo_toml)?;
    std::fs::write(dir.join("src").join("lib.rs"), lib_rs)?;
    for (rel, contents) in wit_files {
        let path = dir.join("wit").join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    Ok(())
}

/// Run `splicer compose` / `splicer splice` and return the emitted
/// `wac compose …` command line (printed on stdout).
fn emit_wac_command(cmd: &mut Command, label: &str) -> String {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{label}: spawn failed: {e}"));
    if !out.status.success() {
        panic!(
            "{label}: exit {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    let s = String::from_utf8(out.stdout).expect("splicer stdout utf8");
    s.trim().to_string()
}

/// Run the `wac compose …` shell command splicer emits, appending
/// `-o <out>` so the result lands at the expected path.
fn run_wac_command(wac_cmd: &str, out_path: &Path, cwd: &Path, label: &str) {
    run(
        Command::new("sh")
            .arg("-c")
            .arg(format!("{wac_cmd} -o {}", out_path.display()))
            .current_dir(cwd),
        label,
    );
}

fn wrap_component(root: &Path, crate_name: &str, adapter: &Path) -> PathBuf {
    let module_path = root
        .join("target")
        .join("wasm32-wasip1")
        .join("debug")
        .join(format!("{crate_name}.wasm"));
    let comp_path = root.join(format!("{crate_name}.comp.wasm"));
    run(
        Command::new("wasm-tools").args([
            "component",
            "new",
            module_path.to_str().unwrap(),
            "--adapt",
            adapter.to_str().unwrap(),
            "-o",
            comp_path.to_str().unwrap(),
        ]),
        &format!("wasm-tools component new {crate_name}"),
    );
    comp_path
}
