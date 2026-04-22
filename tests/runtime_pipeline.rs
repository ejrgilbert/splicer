//! Parameterized end-to-end runtime pipeline for the splicer.
//!
//! Scaffolds three Rust crates (provider, consumer, middleware) plus
//! their WIT into a tempdir, runs the full splicer pipeline
//! (cargo build → wasm-tools component new → wac compose →
//! splicer splice → wac compose), loads the result under wasmtime,
//! invokes the composed component's `run` function, and asserts the
//! expected control-flow markers appear in stdout.
//!
//! The test loops over a list of WIT shapes — primitives (`u32`,
//! `string`, …) and a first pass of compounds (`option<T>`,
//! `list<T>`, `tuple<…>`, `record`) — rewriting the provider's WIT +
//! Rust per shape and reusing the cargo workspace so incremental
//! compilation keeps the per-shape cost down. The consumer's outward
//! signature is shape-agnostic (`run: func()` with
//! `println!("… {r:?}")`), so only the provider + its copy of the
//! `my:shape` dep WIT change.
//!
//! `#[ignore]`'d because cargo-per-shape still adds up; run on demand:
//!     cargo test --test runtime_pipeline -- --ignored --nocapture
//!
//! Override the shape list via env var (comma-separated names):
//!     SPLICER_RUNTIME_SHAPES=u32,string cargo test --test runtime_pipeline \
//!         -- --ignored --nocapture

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::process::Command;

// ─── Shape catalog ─────────────────────────────────────────────────
//
// A `Shape` describes what varies per test iteration: the WIT type
// that `foo` returns, the matching Rust type in the provider, a
// concrete value to return, and what that value renders as in Debug
// output (used only by the pre-splice sanity check).
//
// Compounds recurse: Option/List/Tuple wrap another Shape and Record
// carries a named field list. `shape_catalog()` is the hardcoded
// deterministic coverage; `gen_shape()` drives the same enum from an
// `arbitrary::Unstructured` for the fuzz test below.

#[derive(Clone)]
enum Shape {
    Primitive {
        /// Short label used for logging + env-var filtering.
        name: &'static str,
        /// Type spelled in WIT, e.g. `u32`, `string`, `char`.
        wit_type: &'static str,
        /// Type spelled in Rust (what wit-bindgen generates for it).
        rust_ty: &'static str,
        /// Rust expression producing a concrete value of that type.
        rust_literal: &'static str,
        /// How `{value:?}` renders the literal.
        expected_debug: &'static str,
    },
    Option(Box<Shape>),
    List(Box<Shape>),
    Tuple(Vec<Shape>),
    Record {
        /// Record name in WIT. Keep single-word to dodge kebab→snake
        /// casing rules in wit-bindgen-generated field access.
        wit_name: &'static str,
        /// PascalCased wit-bindgen-generated Rust type name.
        rust_name: &'static str,
        fields: Vec<(&'static str, Shape)>,
    },
}

impl Shape {
    fn name(&self) -> String {
        match self {
            Shape::Primitive { name, .. } => (*name).to_string(),
            Shape::Option(inner) => format!("option_{}", inner.name()),
            Shape::List(inner) => format!("list_{}", inner.name()),
            Shape::Tuple(parts) => {
                let mut s = String::from("tuple");
                for p in parts {
                    s.push('_');
                    s.push_str(&p.name());
                }
                s
            }
            Shape::Record { wit_name, .. } => format!("record_{}", wit_name),
        }
    }

    fn wit_type(&self) -> String {
        match self {
            Shape::Primitive { wit_type, .. } => (*wit_type).to_string(),
            Shape::Option(inner) => format!("option<{}>", inner.wit_type()),
            Shape::List(inner) => format!("list<{}>", inner.wit_type()),
            Shape::Tuple(parts) => {
                let inside = parts
                    .iter()
                    .map(Shape::wit_type)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("tuple<{inside}>")
            }
            Shape::Record { wit_name, .. } => (*wit_name).to_string(),
        }
    }

    /// Extra interface-level type declarations (e.g.
    /// `record point { ... }`). Empty for shapes whose WIT signature
    /// is fully inline.
    fn wit_decls(&self) -> String {
        match self {
            Shape::Record {
                wit_name, fields, ..
            } => {
                let mut s = format!("record {wit_name} {{\n");
                for (fname, fshape) in fields {
                    s.push_str(&format!("    {fname}: {},\n", fshape.wit_type()));
                }
                s.push('}');
                s
            }
            _ => String::new(),
        }
    }

    fn rust_ty(&self) -> String {
        match self {
            Shape::Primitive { rust_ty, .. } => (*rust_ty).to_string(),
            Shape::Option(inner) => format!("Option<{}>", inner.rust_ty()),
            Shape::List(inner) => format!("Vec<{}>", inner.rust_ty()),
            Shape::Tuple(parts) => {
                let inside = parts
                    .iter()
                    .map(Shape::rust_ty)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({inside})")
            }
            Shape::Record { rust_name, .. } => {
                format!("bindings::exports::my::shape::api::{rust_name}")
            }
        }
    }

    fn rust_literal(&self) -> String {
        match self {
            Shape::Primitive { rust_literal, .. } => (*rust_literal).to_string(),
            Shape::Option(inner) => format!("Some({})", inner.rust_literal()),
            Shape::List(inner) => format!("vec![{}]", inner.rust_literal()),
            Shape::Tuple(parts) => {
                let inside = parts
                    .iter()
                    .map(Shape::rust_literal)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({inside})")
            }
            Shape::Record {
                rust_name, fields, ..
            } => {
                let inits = fields
                    .iter()
                    .map(|(fname, fshape)| format!("{fname}: {}", fshape.rust_literal()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("bindings::exports::my::shape::api::{rust_name} {{ {inits} }}")
            }
        }
    }

    fn expected_debug(&self) -> String {
        match self {
            Shape::Primitive { expected_debug, .. } => (*expected_debug).to_string(),
            Shape::Option(inner) => format!("Some({})", inner.expected_debug()),
            Shape::List(inner) => format!("[{}]", inner.expected_debug()),
            Shape::Tuple(parts) => {
                let inside = parts
                    .iter()
                    .map(Shape::expected_debug)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({inside})")
            }
            Shape::Record {
                rust_name, fields, ..
            } => {
                let inits = fields
                    .iter()
                    .map(|(fname, fshape)| format!("{fname}: {}", fshape.expected_debug()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{rust_name} {{ {inits} }}")
            }
        }
    }
}

/// The primitive shapes that both the hardcoded catalog and the
/// fuzz generator draw from. Kept as a function (not a const) because
/// `Shape::Primitive` is not const-constructible with nested lifetimes.
fn primitive_atoms() -> Vec<Shape> {
    vec![
        Shape::Primitive {
            name: "u32",
            wit_type: "u32",
            rust_ty: "u32",
            rust_literal: "42u32",
            expected_debug: "42",
        },
        Shape::Primitive {
            name: "s64",
            wit_type: "s64",
            rust_ty: "i64",
            rust_literal: "-42i64",
            expected_debug: "-42",
        },
        Shape::Primitive {
            name: "bool",
            wit_type: "bool",
            rust_ty: "bool",
            rust_literal: "true",
            expected_debug: "true",
        },
        Shape::Primitive {
            name: "char",
            wit_type: "char",
            rust_ty: "char",
            rust_literal: "'x'",
            expected_debug: "'x'",
        },
        Shape::Primitive {
            name: "string",
            wit_type: "string",
            rust_ty: "String",
            rust_literal: r#"String::from("hello")"#,
            expected_debug: r#""hello""#,
        },
    ]
}

fn shape_catalog() -> Vec<Shape> {
    let mut v = primitive_atoms();
    v.extend(vec![
        Shape::Option(Box::new(Shape::Primitive {
            name: "u32",
            wit_type: "u32",
            rust_ty: "u32",
            rust_literal: "7u32",
            expected_debug: "7",
        })),
        Shape::List(Box::new(Shape::Primitive {
            name: "u32",
            wit_type: "u32",
            rust_ty: "u32",
            rust_literal: "1u32",
            expected_debug: "1",
        })),
        Shape::Tuple(vec![
            Shape::Primitive {
                name: "u32",
                wit_type: "u32",
                rust_ty: "u32",
                rust_literal: "42u32",
                expected_debug: "42",
            },
            Shape::Primitive {
                name: "string",
                wit_type: "string",
                rust_ty: "String",
                rust_literal: r#"String::from("hi")"#,
                expected_debug: r#""hi""#,
            },
        ]),
        Shape::Record {
            wit_name: "point",
            rust_name: "Point",
            fields: vec![
                (
                    "x",
                    Shape::Primitive {
                        name: "u32",
                        wit_type: "u32",
                        rust_ty: "u32",
                        rust_literal: "3u32",
                        expected_debug: "3",
                    },
                ),
                (
                    "y",
                    Shape::Primitive {
                        name: "u32",
                        wit_type: "u32",
                        rust_ty: "u32",
                        rust_literal: "5u32",
                        expected_debug: "5",
                    },
                ),
            ],
        },
    ]);
    v
}

// ─── Arbitrary-driven generator (used by test_runtime_pipeline_fuzz) ─
//
// Generates `Shape` trees from an `arbitrary::Unstructured`. Records
// can't nest inside other records — WIT only declares record types at
// interface scope, so a field of type record would need its own top-
// level decl, which we don't emit. `allow_record=false` is threaded
// through when recursing into record fields.

fn gen_shape(
    u: &mut arbitrary::Unstructured<'_>,
    max_depth: u32,
    allow_record: bool,
) -> arbitrary::Result<Shape> {
    let can_recurse = max_depth > 0;
    // 0=primitive, 1=option, 2=list, 3=tuple, 4=record
    let max_kind: u8 = match (can_recurse, allow_record) {
        (false, _) => 0,
        (true, false) => 3,
        (true, true) => 4,
    };
    let kind: u8 = u.int_in_range(0..=max_kind)?;
    match kind {
        0 => pick_primitive(u),
        1 => Ok(Shape::Option(Box::new(gen_shape(
            u,
            max_depth - 1,
            allow_record,
        )?))),
        2 => Ok(Shape::List(Box::new(gen_shape(
            u,
            max_depth - 1,
            allow_record,
        )?))),
        3 => {
            let n: usize = u.int_in_range(2..=3)?;
            let parts: arbitrary::Result<Vec<Shape>> = (0..n)
                .map(|_| gen_shape(u, max_depth - 1, allow_record))
                .collect();
            Ok(Shape::Tuple(parts?))
        }
        4 => {
            const FIELD_NAMES: &[&str] = &["a", "b", "c"];
            let n: usize = u.int_in_range(1..=3)?;
            let fields: arbitrary::Result<Vec<(&'static str, Shape)>> = (0..n)
                .map(|i| {
                    let fshape = gen_shape(u, max_depth - 1, false)?;
                    Ok((FIELD_NAMES[i], fshape))
                })
                .collect();
            Ok(Shape::Record {
                wit_name: "rec",
                rust_name: "Rec",
                fields: fields?,
            })
        }
        _ => unreachable!(),
    }
}

fn pick_primitive(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Shape> {
    let atoms = primitive_atoms();
    let idx: usize = u.int_in_range(0..=atoms.len() - 1)?;
    Ok(atoms[idx].clone())
}

/// Deterministic LCG byte source so a failing iteration is replayable
/// via `SPLICER_FUZZ_SEED` + `SPLICER_FUZZ_ITERS`. Kept byte-for-byte
/// identical to `src/adapter/tests/fuzz.rs::fuzz_seeded_bytes` so the
/// two fuzz harnesses stay aligned.
fn fuzz_seeded_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 32) as u8
        })
        .collect()
}

// ─── Fixtures that don't vary per shape ────────────────────────────

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

const CONSUMER_CARGO_TOML: &str = r#"[package]
name = "consumer"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = { workspace = true }
"#;

// Consumer is shape-agnostic: it calls `api::foo()`, prints via
// `{r:?}` Debug formatting (which every primitive + every wit-bindgen-
// generated compound implements), and returns unit. Swapping shapes
// therefore only requires rewriting `my-shape` (the imported dep) and
// the provider crate — the consumer stays fixed.
const CONSUMER_WORLD_WIT: &str = r#"package my:svc@1.0.0;

interface app {
    run: func();
}

world consumer {
    export app;
    import my:shape/api@1.0.0;
}
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
    fn run() {
        println!("consumer: calling provider");
        let r = api::foo();
        println!("consumer: got {r:?}");
    }
}

bindings::export!(Consumer with_types_in bindings);
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
    async fn before_call(name: String) {
        println!("mdl: before {name}");
    }
}

impl AfterGuest for Mdl {
    async fn after_call(name: String) {
        println!("mdl: after {name}");
    }
}

bindings::export!(Mdl with_types_in bindings);
"#;

const MIDDLEWARE_WORLD_WIT: &str = r#"package my:middleware@1.0.0;

world mdl {
    export splicer:tier1/before@0.1.0;
    export splicer:tier1/after@0.1.0;
}
"#;

const MIDDLEWARE_TIER1_DEP_WIT: &str = include_str!("../wit/tier1/world.wit");

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

// ─── Per-shape emitters ────────────────────────────────────────────

/// Provider's world WIT — the only file that embeds the shape in its
/// interface signature.
fn provider_world_wit(shape: &Shape) -> String {
    format!(
        "package my:shape@1.0.0;\n\
         \n\
         interface api {{\n\
         {body}\
         }}\n\
         \n\
         world provider {{\n    \
             export api;\n\
         }}\n",
        body = api_interface_body(shape),
    )
}

/// The body shared between the provider's world WIT and the
/// consumer's copy of `my:shape` — the record/... decls (if any)
/// followed by the `foo` signature. Every line is 4-space-indented
/// so the caller can drop it straight inside `interface api { … }`.
fn api_interface_body(shape: &Shape) -> String {
    let mut body = String::new();
    let decls = shape.wit_decls();
    if !decls.is_empty() {
        for line in decls.lines() {
            body.push_str("    ");
            body.push_str(line);
            body.push('\n');
        }
        body.push('\n');
    }
    body.push_str(&format!("    foo: func() -> {};\n", shape.wit_type()));
    body
}

/// Provider's `src/lib.rs` — returns a literal of the shape's type
/// and prints it so the trace proves the provider was invoked.
fn provider_lib_rs(shape: &Shape) -> String {
    format!(
        r#"mod bindings {{
    wit_bindgen::generate!({{
        world: "provider",
        generate_all
    }});
}}

use bindings::exports::my::shape::api::Guest;

struct Provider;

impl Guest for Provider {{
    fn foo() -> {ty} {{
        let v: {ty} = {lit};
        println!("provider: returning {{v:?}}");
        v
    }}
}}

bindings::export!(Provider with_types_in bindings);
"#,
        ty = shape.rust_ty(),
        lit = shape.rust_literal(),
    )
}

/// Copy of `my:shape`'s interface, committed into the consumer's
/// `wit/deps/` so wit-bindgen can resolve the import.
fn consumer_shape_dep_wit(shape: &Shape) -> String {
    format!(
        "package my:shape@1.0.0;\n\
         \n\
         interface api {{\n\
         {body}\
         }}\n",
        body = api_interface_body(shape),
    )
}

// ─── Test ──────────────────────────────────────────────────────────

/// Loop the whole pipeline over the catalog of shapes, reusing the
/// cargo workspace for incremental compilation. Default set is
/// everything in `shape_catalog()`; override via
/// `SPLICER_RUNTIME_SHAPES=name1,name2`.
#[test]
#[ignore]
fn test_runtime_pipeline() {
    require_tool("cargo");
    require_tool("wasm-tools");
    require_tool("wac");

    let tmp = tempfile::tempdir().expect("mktempdir");
    let root = tmp.path();
    eprintln!("runtime_pipeline: work dir = {}", root.display());

    scaffold_common(root).expect("scaffold common");

    let shapes = select_shapes();
    assert!(
        !shapes.is_empty(),
        "SPLICER_RUNTIME_SHAPES selected no shapes; known: {}",
        shape_catalog()
            .iter()
            .map(Shape::name)
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut failures: Vec<(String, String)> = Vec::new();
    for shape in &shapes {
        let shape_name = shape.name();
        eprintln!("\n=== shape: {shape_name} ===");
        if let Err(e) = write_per_shape_files(root, shape) {
            failures.push((shape_name.clone(), format!("write_per_shape_files: {e}")));
            continue;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_pipeline_for_shape(root, shape)
        }));
        if let Err(panic) = result {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "<non-string panic>".into());
            failures.push((shape_name.clone(), msg));
            eprintln!(
                "shape `{shape_name}`: FAILED — {}",
                failures.last().unwrap().1
            );
        }
    }
    if !failures.is_empty() {
        eprintln!("\n=== failures ===");
        for (name, msg) in &failures {
            eprintln!("  {name}: {msg}");
        }
        panic!("{} of the shape pipelines failed", failures.len());
    }
}

/// Fuzz twin of `test_runtime_pipeline`: drives the same scaffold
/// with shapes generated by `gen_shape()`. Also `#[ignore]`'d — each
/// iteration rebuilds the provider crate, so a handful of iters is a
/// minute of work.
///
/// Env vars:
///   SPLICER_FUZZ_SEED   — base u64 seed (default: wall-clock nanos)
///   SPLICER_FUZZ_ITERS  — iterations to run (default: 8)
///   SPLICER_FUZZ_DEPTH  — max recursion depth per shape (default: 3)
///
/// Each iteration uses `base_seed.wrapping_add(iter_idx)` so any
/// failure can be replayed with `SPLICER_FUZZ_SEED=<iter_seed> \
/// SPLICER_FUZZ_ITERS=1`. Failures are printed as
/// `iter {i} seed {iter_seed} shape `{name}`: {msg}` so the seed to
/// replay is visible on every failing line.
#[test]
#[ignore]
fn test_runtime_pipeline_fuzz() {
    require_tool("cargo");
    require_tool("wasm-tools");
    require_tool("wac");

    let base_seed: u64 = std::env::var("SPLICER_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        });
    let iters: u32 = std::env::var("SPLICER_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let max_depth: u32 = std::env::var("SPLICER_FUZZ_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);

    eprintln!("runtime_pipeline_fuzz: iters={iters} base_seed={base_seed} max_depth={max_depth}");

    let tmp = tempfile::tempdir().expect("mktempdir");
    let root = tmp.path();
    eprintln!("runtime_pipeline_fuzz: work dir = {}", root.display());
    scaffold_common(root).expect("scaffold common");

    let mut failures: Vec<String> = Vec::new();

    for i in 0..iters {
        let iter_seed = base_seed.wrapping_add(i as u64);
        let buf = fuzz_seeded_bytes(iter_seed, 4096);
        let mut u = arbitrary::Unstructured::new(&buf);

        let shape = match gen_shape(&mut u, max_depth, true) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("iter {i} seed {iter_seed}: gen_shape: {e}"));
                continue;
            }
        };
        let shape_name = shape.name();
        eprintln!("\n=== iter {i} seed {iter_seed}: {shape_name} ===");

        if let Err(e) = write_per_shape_files(root, &shape) {
            failures.push(format!(
                "iter {i} seed {iter_seed} shape `{shape_name}`: write_per_shape_files: {e}"
            ));
            continue;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_pipeline_for_shape(root, &shape)
        }));
        if let Err(panic) = result {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "<non-string panic>".into());
            failures.push(format!(
                "iter {i} seed {iter_seed} shape `{shape_name}`: {msg}"
            ));
        }
    }

    eprintln!(
        "runtime_pipeline_fuzz: passed={} failures={}",
        iters as usize - failures.len(),
        failures.len()
    );
    if !failures.is_empty() {
        for f in failures.iter().take(20) {
            eprintln!("  {f}");
        }
        if failures.len() > 20 {
            eprintln!("  ... and {} more", failures.len() - 20);
        }
        panic!(
            "{} runtime fuzz iterations failed — replay a single case with \
             SPLICER_FUZZ_SEED=<iter_seed_from_output> SPLICER_FUZZ_ITERS=1",
            failures.len()
        );
    }
}

/// Pick which shapes to run. Without the env var, the full
/// `shape_catalog()`. With it, only shapes whose `name()` matches
/// one of the comma-separated entries.
fn select_shapes() -> Vec<Shape> {
    let all = shape_catalog();
    match std::env::var("SPLICER_RUNTIME_SHAPES").ok() {
        None => all,
        Some(csv) => {
            let wanted: Vec<String> = csv.split(',').map(|s| s.trim().to_string()).collect();
            all.into_iter()
                .filter(|s| wanted.iter().any(|w| *w == s.name()))
                .collect()
        }
    }
}

/// Drive the pipeline end-to-end for a single shape: build, wrap,
/// compose, splice, validate, invoke, assert on markers.
fn run_pipeline_for_shape(root: &Path, shape: &Shape) {
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
    // `wac compose` command that assembles the final .wasm.
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

    // Stage 2: splice the middleware in.
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
    // validator accepts them.
    let bytes = std::fs::read(&final_path).expect("read final.wasm");
    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    validator
        .validate_all(&bytes)
        .expect("final composed component must validate");
    eprintln!("runtime_pipeline: validated {} bytes", bytes.len());

    // Sanity check: invoke the UNSPLICED composition first so we can
    // tell "splice dropped the return value" apart from "the pipeline
    // was broken all along". Here we CAN assert on the value because
    // nothing between provider and consumer is manipulating it yet.
    let pre_splice_trace =
        invoke_run(&std::fs::read(&composed_path).unwrap()).expect("invoke composed (pre-splice)");
    eprintln!("runtime_pipeline: pre-splice trace:\n{pre_splice_trace}");
    let shape_name = shape.name();
    let expected_pre = format!("consumer: got {}", shape.expected_debug());
    assert!(
        pre_splice_trace.contains(&expected_pre),
        "even without the splice, consumer didn't see the expected value for shape `{shape_name}`.\n\
         --- expected substring ---\n{expected_pre}\n--- pre-splice trace ---\n{pre_splice_trace}",
    );

    // Run the spliced component. See the known-bug NOTE: the tier-1
    // adapter drops sync return values when wrapping a sync function
    // with async before/after hooks, so we only assert on the control-
    // flow markers, not the returned value.
    let captured = invoke_run(&bytes).expect("invoke run()");
    eprintln!("runtime_pipeline: post-splice trace:\n{captured}");
    for marker in [
        "consumer: calling provider",
        "mdl: before foo",
        "provider: returning ",
        "mdl: after foo",
        "consumer: got ", // value intentionally unchecked; see NOTE
    ] {
        assert!(
            captured.contains(marker),
            "post-splice trace missing marker `{marker}` for shape `{shape_name}`\n--- trace ---\n{captured}",
        );
    }
    eprintln!("runtime_pipeline: all control-flow markers fired for shape `{shape_name}`",);
}

/// Load the composed component, call `my:svc/app@1.0.0#run`, return
/// whatever the guest wrote to stdout. The spliced adapter uses
/// `task.return`, so the `component-model-async` feature + async
/// wasmtime config are required. `run` returns unit, so its typed
/// signature is `TypedFunc<(), ()>` regardless of the shape flowing
/// through `my:shape/api`.
fn invoke_run(bytes: &[u8]) -> anyhow::Result<String> {
    use wasmtime::component::{Component, Linker, ResourceTable, TypedFunc};
    use wasmtime::{Config, Engine, Store};
    use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
    use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

    struct Host {
        wasi: WasiCtx,
        table: ResourceTable,
    }
    impl WasiView for Host {
        fn ctx(&mut self) -> WasiCtxView<'_> {
            WasiCtxView {
                ctx: &mut self.wasi,
                table: &mut self.table,
            }
        }
    }

    let stdout_pipe = MemoryOutputPipe::new(1 << 20);
    let wasi = WasiCtxBuilder::new().stdout(stdout_pipe.clone()).build();

    let mut config = Config::new();
    config.async_support(true);
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;

    let component = Component::from_binary(&engine, bytes)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

    let mut store = Store::new(
        &engine,
        Host {
            wasi,
            table: ResourceTable::new(),
        },
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let app_idx = instance
            .get_export_index(&mut store, None, "my:svc/app@1.0.0")
            .context("component has no `my:svc/app@1.0.0` export")?;
        let run_idx = instance
            .get_export_index(&mut store, Some(&app_idx), "run")
            .context("`my:svc/app@1.0.0` has no `run` export")?;
        let run_func = instance
            .get_func(&mut store, run_idx)
            .context("run export is not a func")?;
        let typed: TypedFunc<(), ()> = run_func.typed(&store)?;
        typed.call_async(&mut store, ()).await?;
        typed.post_return_async(&mut store).await?;
        Ok::<_, anyhow::Error>(())
    })?;

    let bytes = stdout_pipe.contents();
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ─── Scaffolding helpers ───────────────────────────────────────────

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

/// One-time setup: workspace + shape-independent crates (consumer +
/// middleware). Per-shape `write_per_shape_files` then overwrites the
/// provider crate and the consumer's `deps/my-shape` copy on each
/// iteration.
fn scaffold_common(root: &Path) -> std::io::Result<()> {
    std::fs::write(root.join("Cargo.toml"), WORKSPACE_CARGO_TOML)?;

    write_crate(
        root,
        "provider",
        PROVIDER_CARGO_TOML,
        "// placeholder\n",
        &[],
    )?;
    write_crate(
        root,
        "consumer",
        CONSUMER_CARGO_TOML,
        CONSUMER_LIB_RS,
        &[("world.wit", CONSUMER_WORLD_WIT)],
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

/// Per-shape setup: rewrite the provider crate's source + WIT and the
/// consumer's `deps/my-shape` copy. Everything else (workspace,
/// middleware, consumer's own world) is stable across shapes.
fn write_per_shape_files(root: &Path, shape: &Shape) -> std::io::Result<()> {
    let provider_lib = provider_lib_rs(shape);
    let provider_world = provider_world_wit(shape);
    let dep_wit = consumer_shape_dep_wit(shape);

    std::fs::write(root.join("provider/src/lib.rs"), provider_lib)?;
    let provider_wit_dir = root.join("provider/wit");
    std::fs::create_dir_all(&provider_wit_dir)?;
    std::fs::write(provider_wit_dir.join("world.wit"), provider_world)?;

    let dep_dir = root.join("consumer/wit/deps/my-shape-1.0.0");
    std::fs::create_dir_all(&dep_dir)?;
    std::fs::write(dep_dir.join("package.wit"), dep_wit)?;
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
