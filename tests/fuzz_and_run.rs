//! End-to-end fuzz + run harness for the splicer.
//!
//! Scaffolds three Rust crates (provider, consumer, middleware) plus
//! their WIT into a tempdir, runs the full splicer pipeline
//! (cargo build → wasm-tools component new → wac compose →
//! splicer splice → wac compose), loads the result under wasmtime,
//! invokes the composed component's `run` function, and asserts the
//! expected control-flow markers appear in stdout.
//!
//! Each test loops over a set of WIT shapes — primitives (`u32`,
//! `string`, …) and a first pass of compounds (`option<T>`,
//! `list<T>`, `tuple<…>`, `record`) — rewriting the provider's WIT +
//! Rust per shape and reusing the cargo workspace so incremental
//! compilation keeps the per-shape cost down. The consumer's outward
//! signature is shape-agnostic (`run: func()` with
//! `println!("… {r:?}")`), so only the provider + its copy of the
//! `my:shape` dep WIT change.
//!
//! `#[ignore]`'d because cargo-per-shape still adds up; run on demand:
//!     cargo test --test fuzz_and_run -- --ignored --nocapture
//!
//! Override the shape list via env var (comma-separated names):
//!     SPLICER_RUNTIME_SHAPES=u32,string cargo test --test fuzz_and_run \
//!         -- --ignored --nocapture

use anyhow::Context;
use arbitrary::Arbitrary;
use splicer::{compose, splice, ComponentInput, ComposeRequest, SpliceRequest};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Package name emitted at the top of the generated WAC source. Matches
/// the CLI's default so our test WAC is identical byte-for-byte to
/// what `splicer compose`/`splicer splice` write.
const WAC_PACKAGE_NAME: &str = "example:composition";

// ─── Tunables ──────────────────────────────────────────────────────
//
// All defaults and size limits that the fuzz / canned tests read.
// Keep named so a reader doesn't have to reverse-engineer a naked
// number. Env vars (SPLICER_FUZZ_SEED / _ITERS / _DEPTH) override
// the fuzz defaults below.

/// Pinned default seed — CI runs produce the same shape sequence
/// every time. Override with `SPLICER_FUZZ_SEED=<u64>` to explore.
const DEFAULT_FUZZ_SEED: u64 = 0xDEAD_BEEF;
/// Default iterations per fuzz run. Local runs take ~90s at this
/// setting; CI overrides via `SPLICER_FUZZ_ITERS` for heavier
/// coverage.
const DEFAULT_FUZZ_ITERS: u32 = 30;
/// Max recursion depth for generated shape trees.
const DEFAULT_FUZZ_DEPTH: u32 = 4;
/// Random bytes drawn per fuzz iteration. Large enough to sustain a
/// DEFAULT_FUZZ_DEPTH-deep shape tree without `int_in_range` running
/// short.
const FUZZ_BYTES_PER_ITER: usize = 4096;
/// Max failures echoed into the test output before truncating.
const MAX_FAILURES_SHOWN: usize = 20;

/// Arity bounds for generated tuples.
const TUPLE_ARITY: std::ops::RangeInclusive<usize> = 2..=3;
/// WIT field names for generated records.
const RECORD_FIELD_NAMES: &[&str] = &["a", "b", "c"];
/// Distinct WIT record names the generator cycles through; length
/// caps the number of records per shape tree (further records fall
/// back to a primitive to keep names unique).
const GEN_RECORD_WIT_NAMES: &[&str] = &["rec0", "rec1", "rec2", "rec3", "rec4", "rec5", "rec6"];
/// wit-bindgen-generated Rust names for `GEN_RECORD_WIT_NAMES`.
const GEN_RECORD_RUST_NAMES: &[&str] = &["Rec0", "Rec1", "Rec2", "Rec3", "Rec4", "Rec5", "Rec6"];
/// WIT variant names the generator cycles through; same name-pool
/// pattern as records.
const GEN_VARIANT_WIT_NAMES: &[&str] = &["tag0", "tag1", "tag2", "tag3", "tag4"];
/// wit-bindgen-generated Rust names for `GEN_VARIANT_WIT_NAMES`.
const GEN_VARIANT_RUST_NAMES: &[&str] = &["Tag0", "Tag1", "Tag2", "Tag3", "Tag4"];
/// Case names reused across generated variants. Size caps the number
/// of cases per variant.
const GEN_VARIANT_CASE_WIT_NAMES: &[&str] = &["ca", "cb", "cc", "cd"];
const GEN_VARIANT_CASE_RUST_NAMES: &[&str] = &["Ca", "Cb", "Cc", "Cd"];
/// WIT enum names for the generator.
const GEN_ENUM_WIT_NAMES: &[&str] = &["enm0", "enm1", "enm2", "enm3"];
const GEN_ENUM_RUST_NAMES: &[&str] = &["Enm0", "Enm1", "Enm2", "Enm3"];
/// Case names reused across generated enums.
const GEN_ENUM_CASE_WIT_NAMES: &[&str] = &["ea", "eb", "ec", "ed"];
const GEN_ENUM_CASE_RUST_NAMES: &[&str] = &["Ea", "Eb", "Ec", "Ed"];
/// WIT flags names for the generator.
const GEN_FLAGS_WIT_NAMES: &[&str] = &["fl0", "fl1", "fl2", "fl3"];
const GEN_FLAGS_RUST_NAMES: &[&str] = &["Fl0", "Fl1", "Fl2", "Fl3"];
/// Flag names reused across generated flags. Length caps flag count
/// per type — kept to 8 to stay within wit-bindgen's single-byte
/// flat-representation bucket for simplicity.
const GEN_FLAGS_WIT_FLAG_NAMES: &[&str] = &["fa", "fb", "fc", "fd", "fe", "ff", "fg", "fh"];
const GEN_FLAGS_RUST_FLAG_NAMES: &[&str] = &["FA", "FB", "FC", "FD", "FE", "FF", "FG", "FH"];
/// In-memory stdout buffer for captured guest output (1 MiB).
const STDOUT_CAPTURE_BYTES: usize = 1 << 20;

// ─── Shape definitions ────────────────────────────────────────────
//
// A `Shape` describes what varies per test iteration: the WIT type
// that `foo` returns, the matching Rust type in the provider, a
// concrete value to return, and what that value renders as in Debug
// output (used only by the pre-splice sanity check).
//
// Compounds recurse: Option/List/Tuple wrap another Shape and Record
// carries a named field list. `canned_shapes()` is the hardcoded
// deterministic coverage; `gen_shape()` drives the same enum from an
// `arbitrary::Unstructured` for the fuzz test below.

/// Which wit-bindgen side is constructing a shape literal. Only
/// matters for nominal types (record/variant/enum/flags): wit-bindgen
/// emits them under `bindings::exports::my::shape::api` on the
/// provider (exporting side) and `bindings::my::shape::api` on the
/// consumer (importing side). Structural types (option/list/tuple/
/// result) and primitives are path-neutral.
#[derive(Clone, Copy)]
enum BindingsSide {
    Provider,
    Consumer,
}

impl BindingsSide {
    fn path(self) -> &'static str {
        match self {
            BindingsSide::Provider => "bindings::exports::my::shape::api",
            BindingsSide::Consumer => "bindings::my::shape::api",
        }
    }
    /// Path to `my:shape/types` (where resources live under factored
    /// types).
    fn types_path(self) -> &'static str {
        match self {
            BindingsSide::Provider => "bindings::exports::my::shape::types",
            BindingsSide::Consumer => "bindings::my::shape::types",
        }
    }
}

// Shape covers value types plus resource handles (`own<T>` / `borrow<T>`)
// over a nullary-constructor resource. Resource methods, static funcs,
// and constructors-with-params are still out of scope — those are
// function kinds rather than value shapes and belong with tier-2
// coverage.
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
    Variant {
        /// Variant name in WIT.
        wit_name: &'static str,
        /// PascalCased wit-bindgen-generated Rust type name.
        rust_name: &'static str,
        cases: Vec<VariantCase>,
        /// Which case (0-indexed) `rust_literal`/`expected_debug`
        /// materializes. All cases still contribute to the WIT + Rust
        /// type definitions; only one is instantiated at runtime.
        selected: usize,
    },
    /// Named enum of unit tags — like a `Variant` where every case has
    /// no payload. Separate kind because the canonical-ABI layout is
    /// discriminant-only (no joined-flat payloads).
    Enum {
        wit_name: &'static str,
        rust_name: &'static str,
        /// `(wit_case, rust_case)` pairs.
        cases: Vec<(&'static str, &'static str)>,
        /// Which case (0-indexed) to materialize.
        selected: usize,
    },
    /// Named bitfield-set. wit-bindgen generates an opaque struct
    /// with `const` associated values plus bitor/etc.; the selected
    /// bitmask names which bits to set in the test value.
    Flags {
        wit_name: &'static str,
        rust_name: &'static str,
        /// `(wit_flag, rust_flag_const)` pairs — wit-bindgen emits
        /// each flag as `UPPER_SNAKE_CASE` associated const.
        flags: Vec<(&'static str, &'static str)>,
        /// Bitmask over `flags` (bit i set means `flags[i]` is included).
        selected: u32,
    },
    /// `result<ok?, err?>` — structural sum of Ok/Err branches, each
    /// with an optional payload.
    Result_ {
        ok: Option<Box<Shape>>,
        err: Option<Box<Shape>>,
        /// Which branch to materialize (`true` = Ok, `false` = Err).
        is_ok: bool,
    },
    // ResourceOwn and ResourceBorrow are wired through every Shape
    // method but not yet activated in `canned_shapes()` or the fuzz
    // generator — the consumer/provider scaffolds need resource codegen
    // first. `allow(dead_code)` until that lands.
    /// `own<T>` — owning handle to a nullary-constructor resource.
    /// Round-trips through the canonical ABI as an i32 with ownership
    /// transfer semantics.
    #[allow(dead_code)]
    ResourceOwn {
        /// Resource name in WIT (kebab-safe single word).
        wit_name: &'static str,
        /// PascalCased Rust type wit-bindgen generates.
        rust_name: &'static str,
    },
    /// `borrow<T>` — borrowed handle to the same kind of resource.
    /// Cannot appear in a function's return position, so the harness's
    /// echo-`foo(x: T) -> T` signature must specialize when this is the
    /// top-level shape (handled by the scaffold emitters).
    #[allow(dead_code)]
    ResourceBorrow {
        wit_name: &'static str,
        rust_name: &'static str,
    },
}

#[derive(Clone)]
struct VariantCase {
    /// Case name in WIT (kebab-case-safe, single word).
    wit_name: &'static str,
    /// wit-bindgen-generated PascalCase enum-variant ident.
    rust_name: &'static str,
    /// Optional payload type. `None` → unit variant; `Some(s)` →
    /// tuple-struct variant carrying a single `s` value.
    payload: Option<Shape>,
}

/// Turn a `String::from("…")` Rust source fragment into a bare
/// string literal (`"…"`) — used when the consumer's sync-mode
/// binding wants `&str` instead of `String`. The input is
/// always-generated-by-us so we don't need a real parser.
fn extract_string_literal(src: &str) -> String {
    // `String::from("hello")` → `"hello"`. Anything that doesn't
    // match falls back to the original string — this keeps the
    // helper robust to callers that accidentally pass a non-
    // `String::from` literal.
    let Some(rest) = src.strip_prefix("String::from(") else {
        return src.to_string();
    };
    let Some(inner) = rest.strip_suffix(')') else {
        return src.to_string();
    };
    inner.to_string()
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
            Shape::Variant { wit_name, .. } => format!("variant_{}", wit_name),
            Shape::Enum { wit_name, .. } => format!("enum_{}", wit_name),
            Shape::Flags { wit_name, .. } => format!("flags_{}", wit_name),
            Shape::Result_ { ok, err, .. } => {
                let ok_s = ok.as_ref().map(|s| s.name()).unwrap_or_else(|| "_".into());
                let err_s = err.as_ref().map(|s| s.name()).unwrap_or_else(|| "_".into());
                format!("result_{ok_s}_{err_s}")
            }
            Shape::ResourceOwn { wit_name, .. } => format!("own_{wit_name}"),
            Shape::ResourceBorrow { wit_name, .. } => format!("borrow_{wit_name}"),
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
            Shape::Variant { wit_name, .. } => (*wit_name).to_string(),
            Shape::Enum { wit_name, .. } => (*wit_name).to_string(),
            Shape::Flags { wit_name, .. } => (*wit_name).to_string(),
            Shape::Result_ { ok, err, .. } => match (ok.as_ref(), err.as_ref()) {
                (None, None) => "result".into(),
                (Some(o), None) => format!("result<{}>", o.wit_type()),
                (None, Some(e)) => format!("result<_, {}>", e.wit_type()),
                (Some(o), Some(e)) => format!("result<{}, {}>", o.wit_type(), e.wit_type()),
            },
            Shape::ResourceOwn { wit_name, .. } => format!("own<{wit_name}>"),
            Shape::ResourceBorrow { wit_name, .. } => format!("borrow<{wit_name}>"),
        }
    }

    /// Extra interface-level type declarations (e.g.
    /// `record point { ... }`) for every named compound at any depth
    /// of the shape tree. Empty for shapes made entirely of anonymous
    /// types.
    fn wit_decls(&self) -> String {
        let mut decls = String::new();
        let mut seen_resources = HashSet::new();
        self.collect_wit_decls(&mut decls, &mut seen_resources);
        decls
    }

    fn collect_wit_decls(&self, out: &mut String, seen_resources: &mut HashSet<&'static str>) {
        match self {
            Shape::Primitive { .. } => {}
            Shape::Option(inner) | Shape::List(inner) => {
                inner.collect_wit_decls(out, seen_resources)
            }
            Shape::Tuple(parts) => {
                for p in parts {
                    p.collect_wit_decls(out, seen_resources);
                }
            }
            Shape::Record {
                wit_name, fields, ..
            } => {
                for (_, fshape) in fields {
                    fshape.collect_wit_decls(out, seen_resources);
                }
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&format!("record {wit_name} {{\n"));
                for (fname, fshape) in fields {
                    out.push_str(&format!("    {fname}: {},\n", fshape.wit_type()));
                }
                out.push('}');
            }
            Shape::Variant {
                wit_name, cases, ..
            } => {
                for case in cases {
                    if let Some(p) = &case.payload {
                        p.collect_wit_decls(out, seen_resources);
                    }
                }
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&format!("variant {wit_name} {{\n"));
                for case in cases {
                    match &case.payload {
                        None => out.push_str(&format!("    {},\n", case.wit_name)),
                        Some(p) => {
                            out.push_str(&format!("    {}({}),\n", case.wit_name, p.wit_type()))
                        }
                    }
                }
                out.push('}');
            }
            Shape::Enum {
                wit_name, cases, ..
            } => {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&format!("enum {wit_name} {{\n"));
                for (wit_case, _) in cases {
                    out.push_str(&format!("    {wit_case},\n"));
                }
                out.push('}');
            }
            Shape::Flags {
                wit_name, flags, ..
            } => {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&format!("flags {wit_name} {{\n"));
                for (wit_flag, _) in flags {
                    out.push_str(&format!("    {wit_flag},\n"));
                }
                out.push('}');
            }
            Shape::Result_ { ok, err, .. } => {
                if let Some(o) = ok {
                    o.collect_wit_decls(out, seen_resources);
                }
                if let Some(e) = err {
                    e.collect_wit_decls(out, seen_resources);
                }
            }
            // Both own<X> and borrow<X> share the same `resource X`
            // declaration. Dedupe via `seen_resources` so a shape that
            // mixes `own<cat>` and `borrow<cat>` emits one resource decl
            // rather than two clashing ones.
            Shape::ResourceOwn { wit_name, .. } | Shape::ResourceBorrow { wit_name, .. } => {
                if seen_resources.insert(*wit_name) {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str(&format!("resource {wit_name} {{\n"));
                    out.push_str("    constructor();\n");
                    out.push('}');
                }
            }
        }
    }

    fn rust_ty(&self, side: BindingsSide) -> String {
        match self {
            Shape::Primitive { rust_ty, .. } => (*rust_ty).to_string(),
            Shape::Option(inner) => format!("Option<{}>", inner.rust_ty(side)),
            Shape::List(inner) => format!("Vec<{}>", inner.rust_ty(side)),
            Shape::Tuple(parts) => {
                let inside = parts
                    .iter()
                    .map(|p| p.rust_ty(side))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({inside})")
            }
            Shape::Record { rust_name, .. }
            | Shape::Variant { rust_name, .. }
            | Shape::Enum { rust_name, .. }
            | Shape::Flags { rust_name, .. } => {
                format!("{}::{rust_name}", side.path())
            }
            Shape::Result_ { ok, err, .. } => {
                let ok_ty = ok
                    .as_ref()
                    .map(|s| s.rust_ty(side))
                    .unwrap_or_else(|| "()".into());
                let err_ty = err
                    .as_ref()
                    .map(|s| s.rust_ty(side))
                    .unwrap_or_else(|| "()".into());
                format!("Result<{ok_ty}, {err_ty}>")
            }
            // Both own<X> and borrow<X> spell the type as the
            // wit-bindgen handle struct in `types` (factored). The
            // own-vs-borrow distinction shows up at the call site.
            Shape::ResourceOwn { rust_name, .. } | Shape::ResourceBorrow { rust_name, .. } => {
                format!("{}::{rust_name}", side.types_path())
            }
        }
    }

    fn rust_literal(&self, side: BindingsSide, mode: AsyncMode) -> String {
        match self {
            Shape::Primitive {
                rust_ty,
                rust_literal,
                ..
            } => {
                // wit-bindgen's sync imports take `&str` for string
                // params — including inside tuples (`(u32, &str)`),
                // `result<_, string>` (`Result<_, &str>`), variant
                // payloads, etc. `ownership: Owning` doesn't rewrite
                // those inner slots. Async imports use owned `String`
                // uniformly. So for the string primitive only: emit a
                // plain str literal on the consumer side in sync mode,
                // and an owned `String` everywhere else.
                let consumer_sync_string = *rust_ty == "String"
                    && matches!(side, BindingsSide::Consumer)
                    && matches!(mode, AsyncMode::Sync);
                if consumer_sync_string {
                    // rust_literal is `String::from("hello")`; strip
                    // to `"hello"`.
                    extract_string_literal(rust_literal)
                } else {
                    (*rust_literal).to_string()
                }
            }
            Shape::Option(inner) => format!("Some({})", inner.rust_literal(side, mode)),
            Shape::List(inner) => format!("vec![{}]", inner.rust_literal(side, mode)),
            Shape::Tuple(parts) => {
                let inside = parts
                    .iter()
                    .map(|p| p.rust_literal(side, mode))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({inside})")
            }
            Shape::Record {
                rust_name, fields, ..
            } => {
                // `ownership: Owning` forces record fields to owned
                // types on the consumer side even in sync mode — so
                // a `string` field is `String`, not `&str`. Recurse
                // with Async-mode so the field emitter uses
                // `String::from(...)` regardless of the outer mode.
                let inner_mode = AsyncMode::Async;
                let inits = fields
                    .iter()
                    .map(|(fname, fshape)| {
                        format!("{fname}: {}", fshape.rust_literal(side, inner_mode))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}::{rust_name} {{ {inits} }}", side.path())
            }
            Shape::Variant {
                rust_name,
                cases,
                selected,
                ..
            } => {
                // Same owned-payload rule as Record — variants with
                // `ownership: Owning` use owned payload types.
                let inner_mode = AsyncMode::Async;
                let case = &cases[*selected];
                let payload = case
                    .payload
                    .as_ref()
                    .map(|p| format!("({})", p.rust_literal(side, inner_mode)))
                    .unwrap_or_default();
                format!("{}::{rust_name}::{}{payload}", side.path(), case.rust_name)
            }
            Shape::Enum {
                rust_name,
                cases,
                selected,
                ..
            } => {
                let (_, rust_case) = cases[*selected];
                format!("{}::{rust_name}::{rust_case}", side.path())
            }
            Shape::Flags {
                rust_name,
                flags,
                selected,
                ..
            } => {
                // wit-bindgen emits each flag as a const on the opaque
                // struct and derives BitOr. An empty bitmask maps to
                // `Flags::empty()`; otherwise OR together the set bits.
                let base = format!("{}::{rust_name}", side.path());
                if *selected == 0 {
                    format!("{base}::empty()")
                } else {
                    flags
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| selected & (1u32 << i) != 0)
                        .map(|(_, (_, rust_flag))| format!("{base}::{rust_flag}"))
                        .collect::<Vec<_>>()
                        .join(" | ")
                }
            }
            Shape::Result_ { ok, err, is_ok } => {
                // The provider template binds the value to a local of
                // the full `Result<T, E>` type, so bare `Ok(...)` /
                // `Err(...)` infer the missing type parameter.
                if *is_ok {
                    match ok.as_ref() {
                        None => "Ok(())".into(),
                        Some(p) => format!("Ok({})", p.rust_literal(side, mode)),
                    }
                } else {
                    match err.as_ref() {
                        None => "Err(())".into(),
                        Some(p) => format!("Err({})", p.rust_literal(side, mode)),
                    }
                }
            }
            // For both own<X> and borrow<X>, the consumer constructs an
            // owned handle via the resource's nullary constructor; the
            // borrow case takes a reference at the call site
            // (consumer_pass_expr). In async mode wit-bindgen makes
            // imported resource constructors return futures, so the
            // literal needs `.await` to materialize the handle. The
            // provider-side use is symmetric — the bound argument has
            // the same Rust type — but the provider doesn't actually
            // construct one from this method.
            Shape::ResourceOwn { rust_name, .. } | Shape::ResourceBorrow { rust_name, .. } => {
                // Constructor lives in `my:shape/types` (factored).
                format!(
                    "{}::{rust_name}::new(){await_suffix}",
                    side.types_path(),
                    await_suffix = mode.await_suffix(),
                )
            }
        }
    }

    /// The expression the consumer passes as the argument to
    /// `api::foo(...)`. `v_ident` is the name of the local holding
    /// the constructed value. Async imports take everything by
    /// value; sync imports take some shapes by value and others by
    /// shared reference, so we prefix with `&` where wit-bindgen's
    /// sync-import signature demands a borrow.
    fn consumer_pass_expr(&self, v_ident: &str, mode: AsyncMode) -> String {
        // `borrow<T>` always passes a reference regardless of mode —
        // wit-bindgen emits `foo(x: &Cat)` for both sync and async
        // imports because the canonical-ABI rule (borrow doesn't
        // transfer ownership) is independent of async lifting.
        if matches!(self, Shape::ResourceBorrow { .. }) {
            return format!("&{v_ident}");
        }
        // `own<T>` is an ownership transfer — wit-bindgen takes the
        // handle by value in both sync and async, so the call site
        // hands over `v` directly without `&`. This must be checked
        // before the generic sync-Copy path below: handles aren't Copy
        // so `is_copy_in` would otherwise force a borrow.
        if matches!(self, Shape::ResourceOwn { .. }) {
            return v_ident.to_string();
        }
        if matches!(mode, AsyncMode::Async) {
            return v_ident.to_string();
        }
        // Sync import convention: wit-bindgen emits `foo(x: T)` when
        // the generated Rust type is `Copy`, and `foo(x: &T)`
        // otherwise. `is_copy_in(Sync)` approximates that — inside
        // structural containers (Tuple, Result, Option, List)
        // wit-bindgen substitutes `&str` for `string` (Copy-ish);
        // inside nominal containers (Record, Variant) the
        // `ownership: Owning` annotation keeps fields as owned
        // `String` (not Copy).
        if self.is_copy_in(AsyncMode::Sync) {
            v_ident.to_string()
        } else {
            format!("&{v_ident}")
        }
    }

    /// Mode-aware Copy-ness approximation for wit-bindgen's sync
    /// import rules. In Sync context, `string` is passed as `&str`
    /// (Copy); in Async context, as `String` (not). `Record` and
    /// `Variant` branches recurse with `Async` because
    /// `ownership: Owning` forces their internals to owned types
    /// regardless of the outer mode.
    fn is_copy_in(&self, mode: AsyncMode) -> bool {
        match self {
            Shape::Primitive { rust_ty, .. } => {
                *rust_ty != "String" || matches!(mode, AsyncMode::Sync)
            }
            Shape::Option(inner) => inner.is_copy_in(mode),
            Shape::List(_) => false,
            Shape::Tuple(parts) => parts.iter().all(|p| p.is_copy_in(mode)),
            Shape::Record { fields, .. } => {
                fields.iter().all(|(_, s)| s.is_copy_in(AsyncMode::Async))
            }
            Shape::Variant { cases, .. } => cases.iter().all(|c| {
                c.payload
                    .as_ref()
                    .is_none_or(|s| s.is_copy_in(AsyncMode::Async))
            }),
            Shape::Enum { .. } | Shape::Flags { .. } => true,
            Shape::Result_ { ok, err, .. } => {
                ok.as_ref().is_none_or(|s| s.is_copy_in(mode))
                    && err.as_ref().is_none_or(|s| s.is_copy_in(mode))
            }
            // Resource handles are not Copy — wit-bindgen generates a
            // `Drop` impl that releases the underlying handle.
            Shape::ResourceOwn { .. } | Shape::ResourceBorrow { .. } => false,
        }
    }

    /// True when this shape (anywhere in its tree) carries a resource
    /// handle. Scaffolds use this to switch from `{value:?}` interpolation
    /// (which doesn't work for opaque handles) to static print strings,
    /// and to gate the resource-impl boilerplate.
    fn contains_resource(&self) -> bool {
        match self {
            Shape::Primitive { .. } | Shape::Enum { .. } | Shape::Flags { .. } => false,
            Shape::Option(inner) | Shape::List(inner) => inner.contains_resource(),
            Shape::Tuple(parts) => parts.iter().any(Shape::contains_resource),
            Shape::Record { fields, .. } => fields.iter().any(|(_, s)| s.contains_resource()),
            Shape::Variant { cases, .. } => cases
                .iter()
                .any(|c| c.payload.as_ref().is_some_and(Shape::contains_resource)),
            Shape::Result_ { ok, err, .. } => {
                ok.as_ref().is_some_and(|s| s.contains_resource())
                    || err.as_ref().is_some_and(|s| s.contains_resource())
            }
            Shape::ResourceOwn { .. } | Shape::ResourceBorrow { .. } => true,
        }
    }

    /// Collect every resource (`wit_name`, `rust_name`) referenced in this
    /// shape, deduplicated by `wit_name`. Scaffolds emit one
    /// `impl GuestCat` per unique resource regardless of how many
    /// own/borrow occurrences appear.
    fn collect_resources(&self, out: &mut Vec<(&'static str, &'static str)>) {
        match self {
            Shape::Primitive { .. } | Shape::Enum { .. } | Shape::Flags { .. } => {}
            Shape::Option(inner) | Shape::List(inner) => inner.collect_resources(out),
            Shape::Tuple(parts) => {
                for p in parts {
                    p.collect_resources(out);
                }
            }
            Shape::Record { fields, .. } => {
                for (_, s) in fields {
                    s.collect_resources(out);
                }
            }
            Shape::Variant { cases, .. } => {
                for c in cases {
                    if let Some(p) = &c.payload {
                        p.collect_resources(out);
                    }
                }
            }
            Shape::Result_ { ok, err, .. } => {
                if let Some(o) = ok {
                    o.collect_resources(out);
                }
                if let Some(e) = err {
                    e.collect_resources(out);
                }
            }
            Shape::ResourceOwn {
                wit_name,
                rust_name,
            }
            | Shape::ResourceBorrow {
                wit_name,
                rust_name,
            } => {
                if !out.iter().any(|(w, _)| w == wit_name) {
                    out.push((*wit_name, *rust_name));
                }
            }
        }
    }

    fn expected_debug(&self) -> String {
        let mut err_enums = HashSet::new();
        // wit-bindgen error-styles an enum (implements `Error`, swaps
        // Debug to `{ code, name, message }`) only when the enum sits
        // directly at the err arg of a `result<_, EnumName>` that the
        // function signature returns (or accepts) at the TOP level.
        // Nested Results — `option<result<_, EnumName>>`,
        // `result<_, result<_, EnumName>>` — don't count: wit-bindgen
        // doesn't treat those enums as error types, so Debug stays
        // `EnumName::Case`. The fuzz harness's foo signature is
        // `func() -> T` (sync) or `func(x: T) -> T` (async), so T
        // itself is the top-level shape to inspect.
        if let Shape::Result_ { err: Some(e), .. } = self {
            if let Shape::Enum { rust_name, .. } = e.as_ref() {
                err_enums.insert(*rust_name);
            }
        }
        self.expected_debug_in(&err_enums)
    }

    fn expected_debug_in(&self, err_enums: &HashSet<&'static str>) -> String {
        match self {
            Shape::Primitive { expected_debug, .. } => (*expected_debug).to_string(),
            Shape::Option(inner) => format!("Some({})", inner.expected_debug_in(err_enums)),
            Shape::List(inner) => format!("[{}]", inner.expected_debug_in(err_enums)),
            Shape::Tuple(parts) => {
                let inside = parts
                    .iter()
                    .map(|p| p.expected_debug_in(err_enums))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({inside})")
            }
            Shape::Record {
                rust_name, fields, ..
            } => {
                let inits = fields
                    .iter()
                    .map(|(fname, fshape)| {
                        format!("{fname}: {}", fshape.expected_debug_in(err_enums))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{rust_name} {{ {inits} }}")
            }
            Shape::Variant {
                rust_name,
                cases,
                selected,
                ..
            } => {
                // wit-bindgen's derived Debug for variants prints as
                // `TypeName::CaseName(...)`, not the standard Rust
                // derive `CaseName(...)`.
                let case = &cases[*selected];
                let payload = case
                    .payload
                    .as_ref()
                    .map(|p| format!("({})", p.expected_debug_in(err_enums)))
                    .unwrap_or_default();
                format!("{rust_name}::{}{payload}", case.rust_name)
            }
            Shape::Enum {
                rust_name,
                cases,
                selected,
                ..
            } => {
                // Enums used as error types get wit-bindgen's
                // error-shaped Debug (`TypeName { code, name, message }`);
                // enums not in an error position keep the standard
                // `TypeName::Case` Debug.
                if err_enums.contains(*rust_name) {
                    let (wit_case, _) = cases[*selected];
                    format!(
                        r#"{rust_name} {{ code: {selected}, name: "{wit_case}", message: "" }}"#
                    )
                } else {
                    let (_, rust_case) = cases[*selected];
                    format!("{rust_name}::{rust_case}")
                }
            }
            Shape::Flags {
                rust_name,
                flags,
                selected,
                ..
            } => {
                // wit-bindgen's Debug prints set flags joined by
                // ` | ` in decl order, wrapped in `TypeName(...)`.
                // Empty mask is `TypeName(0x0)`.
                if *selected == 0 {
                    format!("{rust_name}(0x0)")
                } else {
                    let joined = flags
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| selected & (1u32 << i) != 0)
                        .map(|(_, (_, rust_flag))| *rust_flag)
                        .collect::<Vec<_>>()
                        .join(" | ");
                    format!("{rust_name}({joined})")
                }
            }
            Shape::Result_ { ok, err, is_ok } => {
                if *is_ok {
                    match ok.as_ref() {
                        None => "Ok(())".into(),
                        Some(p) => format!("Ok({})", p.expected_debug_in(err_enums)),
                    }
                } else {
                    match err.as_ref() {
                        None => "Err(())".into(),
                        Some(p) => format!("Err({})", p.expected_debug_in(err_enums)),
                    }
                }
            }
            // Resource handle Debug is opaque — wit-bindgen doesn't
            // expose the internal handle ID stably, so the harness's
            // value-round-trip assertion can't pin a string. Scaffold
            // emitters using resource shapes need a different
            // verification path (e.g. side-effect markers from the
            // resource's constructor or destructor) rather than
            // matching this placeholder.
            Shape::ResourceOwn { rust_name, .. } | Shape::ResourceBorrow { rust_name, .. } => {
                format!("<{rust_name} handle>")
            }
        }
    }
}

/// The primitive shapes that both the canned list and the fuzz
/// generator draw from. Kept as a function (not a const) because
/// `Shape::Primitive` is not const-constructible with nested lifetimes.
fn primitive_atoms() -> Vec<Shape> {
    vec![
        Shape::Primitive {
            name: "u8",
            wit_type: "u8",
            rust_ty: "u8",
            rust_literal: "7u8",
            expected_debug: "7",
        },
        Shape::Primitive {
            name: "s8",
            wit_type: "s8",
            rust_ty: "i8",
            rust_literal: "-7i8",
            expected_debug: "-7",
        },
        Shape::Primitive {
            name: "u16",
            wit_type: "u16",
            rust_ty: "u16",
            rust_literal: "500u16",
            expected_debug: "500",
        },
        Shape::Primitive {
            name: "s16",
            wit_type: "s16",
            rust_ty: "i16",
            rust_literal: "-500i16",
            expected_debug: "-500",
        },
        Shape::Primitive {
            name: "u32",
            wit_type: "u32",
            rust_ty: "u32",
            rust_literal: "42u32",
            expected_debug: "42",
        },
        Shape::Primitive {
            name: "s32",
            wit_type: "s32",
            rust_ty: "i32",
            rust_literal: "-42i32",
            expected_debug: "-42",
        },
        Shape::Primitive {
            name: "u64",
            wit_type: "u64",
            rust_ty: "u64",
            rust_literal: "9000u64",
            expected_debug: "9000",
        },
        Shape::Primitive {
            name: "s64",
            wit_type: "s64",
            rust_ty: "i64",
            rust_literal: "-42i64",
            expected_debug: "-42",
        },
        Shape::Primitive {
            name: "f32",
            wit_type: "f32",
            rust_ty: "f32",
            rust_literal: "1.5f32",
            expected_debug: "1.5",
        },
        Shape::Primitive {
            name: "f64",
            wit_type: "f64",
            rust_ty: "f64",
            rust_literal: "2.5f64",
            expected_debug: "2.5",
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

fn canned_shapes() -> Vec<Shape> {
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
        // Variant with a mix of unit and payload-carrying cases.
        // `selected: 1` materializes the payload-carrying `msg` case
        // so the per-case lift/lower path is exercised end-to-end.
        Shape::Variant {
            wit_name: "tag",
            rust_name: "Tag",
            cases: vec![
                VariantCase {
                    wit_name: "empty",
                    rust_name: "Empty",
                    payload: None,
                },
                VariantCase {
                    wit_name: "msg",
                    rust_name: "Msg",
                    payload: Some(Shape::Primitive {
                        name: "string",
                        wit_type: "string",
                        rust_ty: "String",
                        rust_literal: r#"String::from("variant-hi")"#,
                        expected_debug: r#""variant-hi""#,
                    }),
                },
                VariantCase {
                    wit_name: "num",
                    rust_name: "Num",
                    payload: Some(Shape::Primitive {
                        name: "s64",
                        wit_type: "s64",
                        rust_ty: "i64",
                        rust_literal: "-1i64",
                        expected_debug: "-1",
                    }),
                },
            ],
            selected: 1,
        },
        Shape::Enum {
            wit_name: "color",
            rust_name: "Color",
            cases: vec![("red", "Red"), ("green", "Green"), ("blue", "Blue")],
            selected: 1,
        },
        Shape::Flags {
            wit_name: "perms",
            rust_name: "Perms",
            flags: vec![("read", "READ"), ("write", "WRITE"), ("execute", "EXECUTE")],
            // READ | EXECUTE — bits 0 and 2 set.
            selected: 0b101,
        },
        // result<u32, string> constructed as the Ok branch.
        Shape::Result_ {
            ok: Some(Box::new(Shape::Primitive {
                name: "u32",
                wit_type: "u32",
                rust_ty: "u32",
                rust_literal: "11u32",
                expected_debug: "11",
            })),
            err: Some(Box::new(Shape::Primitive {
                name: "string",
                wit_type: "string",
                rust_ty: "String",
                rust_literal: r#"String::from("boom")"#,
                expected_debug: r#""boom""#,
            })),
            is_ok: true,
        },
        // result<_, string> constructed as the Err branch — exercises
        // the one-sided payload path.
        Shape::Result_ {
            ok: None,
            err: Some(Box::new(Shape::Primitive {
                name: "string",
                wit_type: "string",
                rust_ty: "String",
                rust_literal: r#"String::from("nope")"#,
                expected_debug: r#""nope""#,
            })),
            is_ok: false,
        },
        // Bare `own<cat>` — exercises the canon-ABI handle-transfer
        // path with a nullary-constructor resource. Echo signature
        // works (`foo(x: own<cat>) -> own<cat>`); the consumer
        // constructs, transfers ownership, receives the handle back.
        Shape::ResourceOwn {
            wit_name: "cat",
            rust_name: "Cat",
        },
        // Bare `borrow<cat>` — exercises the borrow path. Function
        // signature drops the result clause (borrow can't be returned),
        // so this also smoke-tests the void-return scaffold branch.
        Shape::ResourceBorrow {
            wit_name: "cat",
            rust_name: "Cat",
        },
    ]);
    v
}

// ─── Arbitrary-driven generator (used by test_fuzz) ─
//
// Generates `Shape` trees from an `arbitrary::Unstructured`. Records
// can't nest inside other records — WIT only declares record types at
// interface scope, so a field of type record would need its own top-
// level decl, which we don't emit. `allow_record=false` is threaded
// through when recursing into record fields.

/// Per-tree counters for nominal types. Each counter indexes into
/// the corresponding `GEN_*_NAMES` pool; when a counter hits the
/// pool length, that nominal kind is considered exhausted and
/// `gen_nominal` picks another one (or falls back to a primitive).
#[derive(Default)]
struct NominalCounters {
    records: usize,
    variants: usize,
    enums: usize,
    flags: usize,
}

impl NominalCounters {
    fn available_nominals(&self) -> Vec<NominalKind> {
        let mut v = Vec::new();
        if self.records < GEN_RECORD_WIT_NAMES.len() {
            v.push(NominalKind::Record);
        }
        if self.variants < GEN_VARIANT_WIT_NAMES.len() {
            v.push(NominalKind::Variant);
        }
        if self.enums < GEN_ENUM_WIT_NAMES.len() {
            v.push(NominalKind::Enum);
        }
        if self.flags < GEN_FLAGS_WIT_NAMES.len() {
            v.push(NominalKind::Flags);
        }
        v
    }
}

#[derive(Clone, Copy)]
enum NominalKind {
    Record,
    Variant,
    Enum,
    Flags,
}

fn gen_shape(
    u: &mut arbitrary::Unstructured<'_>,
    max_depth: u32,
    allow_nominal: bool,
    counters: &mut NominalCounters,
) -> arbitrary::Result<Shape> {
    let can_recurse = max_depth > 0;
    let any_nominal_left = !counters.available_nominals().is_empty();
    // 0=primitive, 1=option, 2=list, 3=tuple, 4=result, 5=nominal
    // (Result is structural — allowed even in nominal-banned contexts —
    // but its payloads propagate `allow_nominal=false`.)
    let max_kind: u8 = match (can_recurse, allow_nominal, any_nominal_left) {
        (false, _, _) => 0,
        (true, false, _) => 4,
        (true, true, false) => 4,
        (true, true, true) => 5,
    };
    let kind: u8 = u.int_in_range(0..=max_kind)?;
    match kind {
        0 => pick_primitive(u),
        1 => Ok(Shape::Option(Box::new(gen_shape(
            u,
            max_depth - 1,
            allow_nominal,
            counters,
        )?))),
        2 => Ok(Shape::List(Box::new(gen_shape(
            u,
            max_depth - 1,
            allow_nominal,
            counters,
        )?))),
        3 => {
            let n: usize = u.int_in_range(TUPLE_ARITY)?;
            let parts: arbitrary::Result<Vec<Shape>> = (0..n)
                .map(|_| gen_shape(u, max_depth - 1, allow_nominal, counters))
                .collect();
            Ok(Shape::Tuple(parts?))
        }
        4 => gen_result(u, max_depth, allow_nominal, counters),
        5 => gen_nominal(u, max_depth, counters),
        _ => unreachable!(),
    }
}

/// Pick a nominal kind uniformly from the ones whose name pool hasn't
/// been exhausted and delegate. Caller must guarantee at least one is
/// available (checked via `counters.available_nominals()`).
fn gen_nominal(
    u: &mut arbitrary::Unstructured<'_>,
    max_depth: u32,
    counters: &mut NominalCounters,
) -> arbitrary::Result<Shape> {
    let available = counters.available_nominals();
    debug_assert!(!available.is_empty());
    let pick = available[u.int_in_range(0..=available.len() - 1)?];
    match pick {
        NominalKind::Record => gen_record(u, max_depth, counters),
        NominalKind::Variant => gen_variant(u, max_depth, counters),
        NominalKind::Enum => gen_enum(u, counters),
        NominalKind::Flags => gen_flags(u, counters),
    }
}

fn gen_record(
    u: &mut arbitrary::Unstructured<'_>,
    max_depth: u32,
    counters: &mut NominalCounters,
) -> arbitrary::Result<Shape> {
    let idx = counters.records;
    counters.records += 1;
    let n: usize = u.int_in_range(1..=RECORD_FIELD_NAMES.len())?;
    let fields: arbitrary::Result<Vec<(&'static str, Shape)>> = (0..n)
        .map(|i| {
            let fshape = gen_shape(u, max_depth - 1, false, counters)?;
            Ok((RECORD_FIELD_NAMES[i], fshape))
        })
        .collect();
    Ok(Shape::Record {
        wit_name: GEN_RECORD_WIT_NAMES[idx],
        rust_name: GEN_RECORD_RUST_NAMES[idx],
        fields: fields?,
    })
}

/// Build a variant with 1..=N cases (N capped by the shared case-name
/// pool). Each case is independently unit or payload-bearing; the
/// selected case is what `rust_literal` / `expected_debug` will
/// materialize at runtime.
fn gen_variant(
    u: &mut arbitrary::Unstructured<'_>,
    max_depth: u32,
    counters: &mut NominalCounters,
) -> arbitrary::Result<Shape> {
    let idx = counters.variants;
    counters.variants += 1;
    let n: usize = u.int_in_range(1..=GEN_VARIANT_CASE_WIT_NAMES.len())?;
    let mut cases: Vec<VariantCase> = Vec::with_capacity(n);
    for i in 0..n {
        let payload = if bool::arbitrary(u)? {
            Some(gen_shape(u, max_depth - 1, false, counters)?)
        } else {
            None
        };
        cases.push(VariantCase {
            wit_name: GEN_VARIANT_CASE_WIT_NAMES[i],
            rust_name: GEN_VARIANT_CASE_RUST_NAMES[i],
            payload,
        });
    }
    let selected: usize = u.int_in_range(0..=cases.len() - 1)?;
    Ok(Shape::Variant {
        wit_name: GEN_VARIANT_WIT_NAMES[idx],
        rust_name: GEN_VARIANT_RUST_NAMES[idx],
        cases,
        selected,
    })
}

fn gen_enum(
    u: &mut arbitrary::Unstructured<'_>,
    counters: &mut NominalCounters,
) -> arbitrary::Result<Shape> {
    let idx = counters.enums;
    counters.enums += 1;
    let n: usize = u.int_in_range(1..=GEN_ENUM_CASE_WIT_NAMES.len())?;
    let cases: Vec<(&'static str, &'static str)> = (0..n)
        .map(|i| (GEN_ENUM_CASE_WIT_NAMES[i], GEN_ENUM_CASE_RUST_NAMES[i]))
        .collect();
    let selected: usize = u.int_in_range(0..=cases.len() - 1)?;
    Ok(Shape::Enum {
        wit_name: GEN_ENUM_WIT_NAMES[idx],
        rust_name: GEN_ENUM_RUST_NAMES[idx],
        cases,
        selected,
    })
}

fn gen_flags(
    u: &mut arbitrary::Unstructured<'_>,
    counters: &mut NominalCounters,
) -> arbitrary::Result<Shape> {
    let idx = counters.flags;
    counters.flags += 1;
    let n: usize = u.int_in_range(1..=GEN_FLAGS_WIT_FLAG_NAMES.len())?;
    let flags: Vec<(&'static str, &'static str)> = (0..n)
        .map(|i| (GEN_FLAGS_WIT_FLAG_NAMES[i], GEN_FLAGS_RUST_FLAG_NAMES[i]))
        .collect();
    // Random bitmask over the n flags. u32 comfortably fits up to 32.
    let full_mask: u32 = if n >= 32 { u32::MAX } else { (1u32 << n) - 1 };
    let selected = u.int_in_range(0..=full_mask)?;
    Ok(Shape::Flags {
        wit_name: GEN_FLAGS_WIT_NAMES[idx],
        rust_name: GEN_FLAGS_RUST_NAMES[idx],
        flags,
        selected,
    })
}

fn gen_result(
    u: &mut arbitrary::Unstructured<'_>,
    max_depth: u32,
    allow_nominal: bool,
    counters: &mut NominalCounters,
) -> arbitrary::Result<Shape> {
    // Each side is independently present or absent. If both absent,
    // that's a bare `result` with no payloads — legal WIT.
    let ok = if bool::arbitrary(u)? {
        Some(Box::new(gen_shape(
            u,
            max_depth - 1,
            allow_nominal,
            counters,
        )?))
    } else {
        None
    };
    let err = if bool::arbitrary(u)? {
        Some(Box::new(gen_shape(
            u,
            max_depth - 1,
            allow_nominal,
            counters,
        )?))
    } else {
        None
    };
    let is_ok = bool::arbitrary(u)?;
    Ok(Shape::Result_ { ok, err, is_ok })
}

fn pick_primitive(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Shape> {
    let atoms = primitive_atoms();
    let idx: usize = u.int_in_range(0..=atoms.len() - 1)?;
    Ok(atoms[idx].clone())
}

/// Deterministic LCG byte source so a failing iteration is replayable
/// via `SPLICER_FUZZ_SEED` + `SPLICER_FUZZ_ITERS`. Kept identical to
/// `src/adapter/tests/fuzz.rs::fuzz_seeded_bytes` so the two fuzz
/// harnesses stay aligned.
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
/// Which async axis the per-iteration Rust + WIT targets. `Sync` keeps
/// the provider's `foo` and the consumer's `run` as plain WIT funcs;
/// `Async` marks both with `async` so the adapter exercises its
/// canon-lower-async handler path and `task.return` loads instead of
/// the sync retptr pattern.
#[derive(Clone, Copy)]
enum AsyncMode {
    Sync,
    Async,
}

impl AsyncMode {
    fn tag(self) -> &'static str {
        match self {
            AsyncMode::Sync => "sync",
            AsyncMode::Async => "async",
        }
    }
    /// The `async ` marker to insert between `:` and `func` in a WIT
    /// method declaration (`foo: async func(...)`), or empty for
    /// sync. Note the placement: `async` comes *after* the colon in
    /// WIT, not before the method name.
    fn wit_async_marker(self) -> &'static str {
        match self {
            AsyncMode::Sync => "",
            AsyncMode::Async => "async ",
        }
    }
    /// The `async ` prefix for a Rust `fn` item, or empty for sync.
    fn rust_prefix(self) -> &'static str {
        match self {
            AsyncMode::Sync => "",
            AsyncMode::Async => "async ",
        }
    }
    /// Extra `generate!` options to enable async codegen; empty in
    /// sync mode.
    fn generate_opts(self) -> &'static str {
        match self {
            AsyncMode::Sync => "",
            AsyncMode::Async => "        async: true,\n",
        }
    }
    /// `.await` suffix after the consumer's `api::foo()` call in
    /// async mode, empty in sync.
    fn await_suffix(self) -> &'static str {
        match self {
            AsyncMode::Sync => "",
            AsyncMode::Async => ".await",
        }
    }
}

fn consumer_world_wit(shape: &Shape, mode: AsyncMode) -> String {
    let mut wit = format!(
        "package my:svc@1.0.0;\n\
         \n\
         interface app {{\n    \
             run: {marker}func();\n\
         }}\n\
         \n\
         world consumer {{\n    \
             export app;\n",
        marker = mode.wit_async_marker(),
    );
    if shape.contains_resource() {
        // Factored-types: import types separately so wac can unify
        // resource identity with the provider's types instance.
        wit.push_str(&format!("    import {TARGET_TYPES_INTERFACE};\n"));
    }
    wit.push_str(&format!("    import {TARGET_INTERFACE};\n"));
    wit.push_str("}\n");
    wit
}

fn consumer_lib_rs(shape: &Shape, mode: AsyncMode) -> String {
    // Echo pattern: consumer constructs a value, sends it, prints
    // what the provider echoed back. `pass_expr` is the expression
    // the consumer actually passes to `api::foo(...)` — wit-bindgen's
    // sync imports take some shape kinds by value and others by
    // shared reference, so the expression varies per shape.
    //
    // Resource shapes diverge: the value can't be `{:?}`-printed (the
    // handle isn't reliably Debug-printable), and a top-level
    // `borrow<T>` shape's `foo` returns `()` rather than echoing the
    // handle, so the call site has no result to bind. Both differences
    // are dispatched through the precomputed `expected_debug` strings
    // and the `is_top_borrow` branch below.
    let literal = shape.rust_literal(BindingsSide::Consumer, mode);
    let pass_expr = shape.consumer_pass_expr("v", mode);

    let has_resource = shape.contains_resource();
    let is_top_borrow = matches!(shape, Shape::ResourceBorrow { .. });
    let expected = shape.expected_debug();

    let send_print = if has_resource {
        format!(r#"println!("consumer: sending {expected}");"#)
    } else {
        r#"println!("consumer: sending {v:?}");"#.to_string()
    };

    let call_and_got = if is_top_borrow {
        format!(
            "api::foo({pass_expr}){await_suffix};\n        \
             println!(\"consumer: got {expected}\");",
            await_suffix = mode.await_suffix(),
        )
    } else if has_resource {
        format!(
            "let r = api::foo({pass_expr}){await_suffix};\n        \
             let _ = r;\n        \
             println!(\"consumer: got {expected}\");",
            await_suffix = mode.await_suffix(),
        )
    } else {
        format!(
            "let r = api::foo({pass_expr}){await_suffix};\n        \
             println!(\"consumer: got {{r:?}}\");",
            await_suffix = mode.await_suffix(),
        )
    };

    format!(
        r#"mod bindings {{
    wit_bindgen::generate!({{
        world: "consumer",
        ownership: Owning,
{opts}        generate_all
    }});
}}

use bindings::exports::my::svc::app::Guest;
use bindings::my::shape::api;

struct Consumer;

impl Guest for Consumer {{
    {rust_prefix}fn run() {{
        let v = {literal};
        {send_print}
        {call_and_got}
    }}
}}

bindings::export!(Consumer with_types_in bindings);
"#,
        opts = mode.generate_opts(),
        rust_prefix = mode.rust_prefix(),
    )
}

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

/// Placeholder replaced at call time with the absolute path of the
/// middleware component. The YAML must reference an absolute path
/// because `splicer::splice` resolves `inject.path` against the
/// process cwd — integration tests run from the project root, not
/// the scaffold tempdir, so a relative path wouldn't resolve.
const MIDDLEWARE_PATH_PLACEHOLDER: &str = "{MIDDLEWARE_PATH}";
const TARGET_INTERFACE: &str = "my:shape/api@1.0.0";
const TARGET_TYPES_INTERFACE: &str = "my:shape/types@1.0.0";

fn splice_yaml_between_tmpl() -> String {
    format!(
        r#"version: 1

rules:
  - between:
      interface: "{TARGET_INTERFACE}"
      inner:
        name: provider-comp
      outer:
        name: consumer-comp
    inject:
      - name: mdl
        path: "{MIDDLEWARE_PATH_PLACEHOLDER}"
"#
    )
}

/// Before-rule variant: insert middleware in front of the provider's
/// export, BEFORE the provider is composed with the consumer.
///
/// Uses `provider.alias: prov` because the auto-derived shim-instance
/// variable name (`shape-api@1-0-0-shim-instance`) would contain `@`,
/// which WAC rejects as an identifier.
fn splice_yaml_before_tmpl() -> String {
    format!(
        r#"version: 1

rules:
  - before:
      interface: "{TARGET_INTERFACE}"
      provider:
        alias: "prov"
    inject:
      - name: mdl
        path: "{MIDDLEWARE_PATH_PLACEHOLDER}"
"#
    )
}

fn splice_yaml(tmpl: String, middleware_path: &Path) -> String {
    tmpl.replace(
        MIDDLEWARE_PATH_PLACEHOLDER,
        middleware_path.to_str().expect("middleware path utf8"),
    )
}

/// Which splicer rule kind the pipeline should exercise.
#[derive(Clone, Copy)]
enum PipelineKind {
    /// `between` rule: compose provider+consumer first, then splice
    /// middleware into the inner/outer interface boundary.
    Between,
    /// `before` rule: splice middleware in front of the provider's
    /// export first, then compose the spliced provider with the
    /// consumer.
    Before,
}

impl PipelineKind {
    /// Short lowercase label used in log prefixes and error messages.
    fn tag(self) -> &'static str {
        match self {
            PipelineKind::Between => "between",
            PipelineKind::Before => "before",
        }
    }
}

// ─── Per-shape emitters ────────────────────────────────────────────

/// Provider's world WIT. Resource-bearing shapes use the factored-types
/// pattern (resource lives in `types`, `api` `use`s it) — required for
/// splicer's wrapper to preserve resource type identity. See memory:
/// project_resource_wrapper_pattern.md.
fn provider_world_wit(shape: &Shape, mode: AsyncMode) -> String {
    let has_resources = shape.contains_resource();
    let mut wit = String::from("package my:shape@1.0.0;\n\n");
    if has_resources {
        wit.push_str(&types_interface_block(shape));
        wit.push('\n');
    }
    wit.push_str("interface api {\n");
    wit.push_str(&api_interface_body(shape, mode));
    wit.push_str("}\n\n");
    wit.push_str("world provider {\n");
    if has_resources {
        wit.push_str("    export types;\n");
    }
    wit.push_str("    export api;\n");
    wit.push_str("}\n");
    wit
}

/// `interface types { … }` block holding the shape's resources, or
/// empty for non-resource shapes.
fn types_interface_block(shape: &Shape) -> String {
    let decls = shape.wit_decls();
    if decls.is_empty() {
        return String::new();
    }
    let mut block = String::from("interface types {\n");
    for line in decls.lines() {
        block.push_str("    ");
        block.push_str(line);
        block.push('\n');
    }
    block.push_str("}\n");
    block
}

/// Body of `interface api`. Resource shapes emit `use types.{...};`;
/// non-resource shapes emit compound decls (record/variant) inline.
/// Always ends with `foo`. (Mixed shapes — records-of-resources — not
/// handled; not in canned set.)
fn api_interface_body(shape: &Shape, mode: AsyncMode) -> String {
    let mut body = String::new();
    let mut resources = Vec::new();
    shape.collect_resources(&mut resources);
    if !resources.is_empty() {
        let names: Vec<&str> = resources.iter().map(|(n, _)| *n).collect();
        body.push_str(&format!("    use types.{{{}}};\n\n", names.join(", ")));
    } else {
        // Non-resource decls go inline in api.
        let decls = shape.wit_decls();
        if !decls.is_empty() {
            for line in decls.lines() {
                body.push_str("    ");
                body.push_str(line);
                body.push('\n');
            }
            body.push('\n');
        }
    }
    // Echo pattern in both modes: consumer sends a value, provider
    // echoes back. Exercises both canon-ABI directions (param + result)
    // in sync and async. The exception is a top-level `borrow<T>` —
    // borrows can't appear in return position per the component model,
    // so the signature drops the result clause.
    let result_clause = if matches!(shape, Shape::ResourceBorrow { .. }) {
        String::new()
    } else {
        format!(" -> {ty}", ty = shape.wit_type())
    };
    body.push_str(&format!(
        "    foo: {marker}func(x: {ty}){result_clause};\n",
        marker = mode.wit_async_marker(),
        ty = shape.wit_type()
    ));
    body
}

/// Provider's `src/lib.rs` — receives a value, prints it, and echoes
/// it back. Exercises both canon-ABI directions: consumer lowers the
/// param and provider lifts it on the way in; provider lowers the
/// result and consumer lifts it on the way out.
///
/// Resource shapes diverge from the value-type template: the provider
/// must `impl Guest{Rust}` for each exported resource, declare the
/// `type Rust = …;` association inside `impl Guest`, and switch from
/// `{value:?}` interpolation to a static print string (handles aren't
/// reliably Debug-printable). A top-level `borrow<T>` shape further
/// drops the return clause since borrow can't appear in a result.
fn provider_lib_rs(shape: &Shape, mode: AsyncMode) -> String {
    if shape.contains_resource() {
        return provider_lib_rs_resource(shape, mode);
    }
    format!(
        r#"mod bindings {{
    wit_bindgen::generate!({{
        world: "provider",
{opts}        generate_all
    }});
}}

use bindings::exports::my::shape::api::Guest;

struct Provider;

impl Guest for Provider {{
    {rust_prefix}fn foo(x: {ty}) -> {ty} {{
        println!("provider: received {{x:?}}");
        x
    }}
}}

bindings::export!(Provider with_types_in bindings);
"#,
        opts = mode.generate_opts(),
        rust_prefix = mode.rust_prefix(),
        ty = shape.rust_ty(BindingsSide::Provider),
    )
}

/// Resource-aware provider scaffold. Emits per-resource `Guest{Rust}`
/// impls (each backed by an empty unit struct from `new()`), the
/// associated-type lines inside `impl Guest`, and a `foo` body that
/// uses the precomputed `expected_debug()` string instead of a `:?`
/// formatter the resource handle wouldn't satisfy.
fn provider_lib_rs_resource(shape: &Shape, mode: AsyncMode) -> String {
    let mut resources = Vec::new();
    shape.collect_resources(&mut resources);

    // Factored-types: resource impls + associated-types go on
    // types::Guest; `foo` goes on api::Guest.
    let prefix = mode.rust_prefix();
    let resource_structs = resources
        .iter()
        .map(|(_, r)| {
            format!(
                "struct {r}Impl;\n\nimpl GuestCat_ for {r}Impl {{\n    \
                 {prefix}fn new() -> Self {{ {r}Impl }}\n}}\n"
            )
            .replace("GuestCat_", &format!("Guest{r}"))
        })
        .collect::<Vec<_>>()
        .join("\n");

    let assoc_types = resources
        .iter()
        .map(|(_, r)| format!("    type {r} = {r}Impl;\n"))
        .collect::<Vec<_>>()
        .join("");

    let received_print = format!(
        r#"println!("provider: received {}");"#,
        shape.expected_debug()
    );

    let foo_block = if let Shape::ResourceBorrow { rust_name, .. } = shape {
        // wit-bindgen names export-side borrow params `<Rust>Borrow<'_>`
        // (distinct generated type, not a plain `&Rust`); the borrow
        // type lives in `types` under the factored pattern.
        let path = BindingsSide::Provider.types_path();
        format!(
            "{prefix}fn foo(x: {path}::{rust_name}Borrow<'_>) {{\n        \
             let _ = x;\n        \
             {received}\n    \
             }}",
            prefix = mode.rust_prefix(),
            received = received_print,
        )
    } else {
        let ty = shape.rust_ty(BindingsSide::Provider);
        format!(
            "{prefix}fn foo(x: {ty}) -> {ty} {{\n        \
             {received}\n        \
             x\n    \
             }}",
            prefix = mode.rust_prefix(),
            received = received_print,
        )
    };

    let guest_resource_traits = resources
        .iter()
        .map(|(_, r)| format!("Guest{r}"))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#"mod bindings {{
    wit_bindgen::generate!({{
        world: "provider",
{opts}        generate_all
    }});
}}

use bindings::exports::my::shape::types::{{Guest as TypesGuest, {guest_resource_traits}}};
use bindings::exports::my::shape::api::Guest as ApiGuest;

struct Provider;

{resource_structs}
impl TypesGuest for Provider {{
{assoc_types}}}

impl ApiGuest for Provider {{
    {foo_block}
}}

bindings::export!(Provider with_types_in bindings);
"#,
        opts = mode.generate_opts(),
    )
}

/// Consumer's `wit/deps/` copy of `my:shape`. Mirrors provider-side
/// structure (factored types for resource shapes).
fn consumer_shape_dep_wit(shape: &Shape, mode: AsyncMode) -> String {
    let mut wit = String::from("package my:shape@1.0.0;\n\n");
    if shape.contains_resource() {
        wit.push_str(&types_interface_block(shape));
        wit.push('\n');
    }
    wit.push_str("interface api {\n");
    wit.push_str(&api_interface_body(shape, mode));
    wit.push_str("}\n");
    wit
}

// ─── Test ──────────────────────────────────────────────────────────

/// Loop the whole pipeline over the canned shape list, reusing the
/// cargo workspace for incremental compilation. Default set is
/// everything in `canned_shapes()`; override via
/// `SPLICER_RUNTIME_SHAPES=name1,name2`.
#[test]
#[ignore]
fn test_canned() {
    require_splicer_toolchain();

    let tmp = tempfile::tempdir().expect("mktempdir");
    let root_buf = tmp.path().to_path_buf();
    // SPLICER_KEEP_TMPDIR=1 disables auto-cleanup so post-mortem
    // inspection (wasm-tools print, cat *.wac) is possible.
    if std::env::var("SPLICER_KEEP_TMPDIR").is_ok() {
        eprintln!("(keeping tmpdir: {})", root_buf.display());
        std::mem::forget(tmp);
    }
    let root = root_buf.as_path();
    eprintln!("canned: work dir = {}", root.display());

    let shapes = select_shapes();
    assert!(
        !shapes.is_empty(),
        "SPLICER_RUNTIME_SHAPES selected no shapes; known: {}",
        canned_shapes()
            .iter()
            .map(Shape::name)
            .collect::<Vec<_>>()
            .join(",")
    );
    // Mode is the outer loop so each mode transition only rebuilds
    // consumer (+ dep) once, not once per shape. Inside a single mode
    // the provider is the only crate whose source changes per shape,
    // which keeps incremental cargo builds cheap.
    let mut failures: Vec<(String, String)> = Vec::new();
    let total_shapes = shapes.len() * ALL_ASYNC_MODES.len();
    let mut shape_idx = 0usize;
    for &mode in ALL_ASYNC_MODES {
        let mode_tag = mode.tag();
        eprintln!("\n### mode: {mode_tag} ###");
        scaffold_common(root, mode).expect("scaffold common");
        for shape in &shapes {
            shape_idx += 1;
            let shape_name = shape.name();
            let label = format!("{shape_name}/{mode_tag}");
            eprintln!("\n=== [{shape_idx}/{total_shapes}] shape: {label} ===");
            if let Err(e) = write_per_shape_files(root, shape, mode) {
                failures.push((label.clone(), format!("write_per_shape_files: {e}")));
                continue;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_pipeline_for_shape(root, shape, ALL_PIPELINE_KINDS)
            }));
            if let Err(panic) = result {
                let msg = panic_msg(&*panic);
                failures.push((label.clone(), msg.clone()));
                eprintln!("shape `{label}`: FAILED — {msg}");
            }
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

/// Async modes every canned shape and every fuzz iteration runs
/// through. Sync goes first so if async breaks, you've already seen
/// the sync-mode failure output before async layer piles on top.
const ALL_ASYNC_MODES: &[AsyncMode] = &[AsyncMode::Sync, AsyncMode::Async];

/// Fuzz twin of `test_canned`: drives the same scaffold
/// with shapes generated by `gen_shape()`. Also `#[ignore]`'d — each
/// iteration rebuilds the provider crate, so a handful of iters is a
/// minute of work.
///
/// Env vars override the `DEFAULT_FUZZ_*` constants:
///   SPLICER_FUZZ_SEED   — base u64 seed (default: `DEFAULT_FUZZ_SEED`)
///   SPLICER_FUZZ_ITERS  — iterations to run
///   SPLICER_FUZZ_DEPTH  — max recursion depth per shape
///
/// Each iteration uses `base_seed.wrapping_add(iter_idx)` so any
/// failure can be replayed with `SPLICER_FUZZ_SEED=<iter_seed> \
/// SPLICER_FUZZ_ITERS=1`. Failures are printed as
/// `iter {i} seed {iter_seed} shape `{name}`: {msg}` so the seed to
/// replay is visible on every failing line.
#[test]
#[ignore]
fn test_fuzz() {
    require_splicer_toolchain();

    let base_seed: u64 = env_or("SPLICER_FUZZ_SEED", DEFAULT_FUZZ_SEED);
    let iters: u32 = env_or("SPLICER_FUZZ_ITERS", DEFAULT_FUZZ_ITERS);
    let max_depth: u32 = env_or("SPLICER_FUZZ_DEPTH", DEFAULT_FUZZ_DEPTH);

    eprintln!("fuzz: iters={iters} base_seed={base_seed} max_depth={max_depth}");

    let tmp = tempfile::tempdir().expect("mktempdir");
    let root_buf = tmp.path().to_path_buf();
    // SPLICER_KEEP_TMPDIR=1 disables auto-cleanup so post-mortem
    // inspection (wasm-tools print, cat *.wac) is possible.
    if std::env::var("SPLICER_KEEP_TMPDIR").is_ok() {
        eprintln!("(keeping tmpdir: {})", root_buf.display());
        std::mem::forget(tmp);
    }
    let root = root_buf.as_path();
    eprintln!("fuzz: work dir = {}", root.display());

    let mut failures: Vec<String> = Vec::new();
    let mut expected_bails = 0usize;
    let mut harness_bails = 0usize;
    let mut total_runs: usize = 0;

    // Mode is the outer loop (see `test_canned` for the rationale).
    // Each iter's generator state is seeded independently of mode, so
    // both modes see the same shape sequence — a failure in one mode
    // but not the other isolates mode as the cause.
    let total_iters_all_modes = (iters as usize) * ALL_ASYNC_MODES.len();
    let mut run_idx = 0usize;
    for &mode in ALL_ASYNC_MODES {
        let mode_tag = mode.tag();
        eprintln!("\n### mode: {mode_tag} ###");
        scaffold_common(root, mode).expect("scaffold common");

        for i in 0..iters {
            total_runs += 1;
            run_idx += 1;
            let iter_seed = base_seed.wrapping_add(i as u64);
            let buf = fuzz_seeded_bytes(iter_seed, FUZZ_BYTES_PER_ITER);
            let mut u = arbitrary::Unstructured::new(&buf);

            let mut counters = NominalCounters::default();
            let shape = match gen_shape(&mut u, max_depth, true, &mut counters) {
                Ok(s) => s,
                Err(e) => {
                    failures.push(format!(
                        "iter {i} seed {iter_seed} mode {mode_tag}: gen_shape: {e}"
                    ));
                    continue;
                }
            };
            let shape_name = shape.name();
            eprintln!(
                "\n=== [{run_idx}/{total_iters_all_modes}] iter {i} seed {iter_seed} mode {mode_tag}: {shape_name} ==="
            );

            if let Err(e) = write_per_shape_files(root, &shape, mode) {
                failures.push(format!(
                    "iter {i} seed {iter_seed} mode {mode_tag} shape `{shape_name}`: write_per_shape_files: {e}"
                ));
                continue;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_pipeline_for_shape(root, &shape, ALL_PIPELINE_KINDS)
            }));
            if let Err(panic) = result {
                let msg = panic_msg(&*panic);
                if is_expected_bail(&msg) {
                    expected_bails += 1;
                    eprintln!(
                        "iter {i} seed {iter_seed} mode {mode_tag} shape `{shape_name}`: expected-bail ({})",
                        msg.lines().next().unwrap_or(&msg)
                    );
                } else if is_harness_bail(&msg, mode) {
                    harness_bails += 1;
                    eprintln!(
                        "iter {i} seed {iter_seed} mode {mode_tag} shape `{shape_name}`: harness-bail ({})",
                        msg.lines().next().unwrap_or(&msg)
                    );
                } else {
                    failures.push(format!(
                        "iter {i} seed {iter_seed} mode {mode_tag} shape `{shape_name}`: {msg}"
                    ));
                }
            }
        }
    }

    eprintln!(
        "fuzz: passed={} expected_bails={expected_bails} harness_bails={harness_bails} failures={}",
        total_runs - failures.len() - expected_bails - harness_bails,
        failures.len()
    );
    if !failures.is_empty() {
        for f in failures.iter().take(MAX_FAILURES_SHOWN) {
            eprintln!("  {f}");
        }
        if failures.len() > MAX_FAILURES_SHOWN {
            eprintln!("  ... and {} more", failures.len() - MAX_FAILURES_SHOWN);
        }
        panic!(
            "{} fuzz iterations failed — replay a single case with \
             SPLICER_FUZZ_SEED=<iter_seed_from_output> SPLICER_FUZZ_ITERS=1",
            failures.len()
        );
    }
}

/// Pick which shapes to run. Without the env var, the full
/// `canned_shapes()`. With it, only shapes whose `name()` matches
/// one of the comma-separated entries.
fn select_shapes() -> Vec<Shape> {
    let all = canned_shapes();
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

/// Drive the pipeline end-to-end for a single shape across every
/// requested `PipelineKind`. Shared work (cargo build, component
/// wrapping, provider+consumer compose, pre-splice sanity check) runs
/// once; the splice + post-splice invocation runs per kind.
fn run_pipeline_for_shape(root: &Path, shape: &Shape, kinds: &[PipelineKind]) {
    // Harness cargo builds fail whenever the generated consumer /
    // provider doesn't line up with wit-bindgen's expected borrow /
    // signature shape for the current Shape — pure harness noise,
    // not a splicer bug. Suppress the build output entirely; the
    // panic message is just "cargo build: exit Some(N)" so
    // `is_harness_bail` still classifies it correctly. To inspect
    // the real rustc output, rerun with `SPLICER_KEEP_TMPDIR=1`
    // and run cargo build in the preserved tmpdir.
    run_quiet(
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
    let middleware_comp = wrap_component(root, "middleware", &adapter);

    // Pre-splice composition of provider+consumer. Used as the
    // baseline we invoke to prove the pipeline threads the value
    // through without the middleware, and (for `between`) as the
    // splice input.
    let composed_path = compose_provider_consumer(root, &provider_comp, &consumer_comp);
    let shape_name = shape.name();

    // Sanity check: invoke the UNSPLICED composition so we can tell
    // "splice dropped the return value" apart from "the pipeline was
    // broken all along". Runs once; shared by every kind.
    let pre_splice_trace =
        invoke_run(&std::fs::read(&composed_path).unwrap()).expect("invoke composed (pre-splice)");
    eprintln!("pipeline: pre-splice trace:\n{pre_splice_trace}");
    let expected_pre = format!("consumer: got {}", shape.expected_debug());
    assert!(
        pre_splice_trace.contains(&expected_pre),
        "even without the splice, consumer didn't see the expected value for shape `{shape_name}`.\n\
         --- expected substring ---\n{expected_pre}\n--- pre-splice trace ---\n{pre_splice_trace}",
    );

    for &kind in kinds {
        let kind_tag = kind.tag();
        eprintln!("pipeline[{kind_tag}]: splicing shape `{shape_name}`");

        let final_path = match kind {
            PipelineKind::Between => splice_between(root, &composed_path, &middleware_comp),
            PipelineKind::Before => {
                splice_before_and_compose(root, &provider_comp, &consumer_comp, &middleware_comp)
            }
        };

        // Validate: parse the final bytes and check the component-
        // model validator accepts them.
        let bytes = std::fs::read(&final_path).expect("read final.wasm");
        let mut validator =
            wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
        validator
            .validate_all(&bytes)
            .unwrap_or_else(|e| panic!("[{kind_tag}] final component failed validation: {e}"));
        eprintln!("pipeline[{kind_tag}]: validated {} bytes", bytes.len());

        let captured = invoke_run(&bytes).expect("invoke run()");
        eprintln!("pipeline[{kind_tag}]: post-splice trace:\n{captured}");
        let expected_sent = format!("consumer: sending {}", shape.expected_debug());
        let expected_received = format!("provider: received {}", shape.expected_debug());
        let expected_got = format!("consumer: got {}", shape.expected_debug());
        let expected_before = format!("mdl: before {TARGET_INTERFACE}#foo");
        let expected_after = format!("mdl: after {TARGET_INTERFACE}#foo");
        for marker in [
            expected_sent.as_str(),
            expected_before.as_str(),
            expected_received.as_str(),
            expected_after.as_str(),
            expected_got.as_str(),
        ] {
            assert!(
                captured.contains(marker),
                "[{kind_tag}] post-splice trace missing marker `{marker}` for shape `{shape_name}`\n--- trace ---\n{captured}",
            );
        }
        eprintln!("pipeline[{kind_tag}]: all control-flow markers fired for shape `{shape_name}`");
    }
}

/// Every kind that `run_pipeline_for_shape` should exercise on each
/// shape. Callers that want to run one kind in isolation (e.g. to
/// narrow a reproduction) can pass a shorter slice directly.
const ALL_PIPELINE_KINDS: &[PipelineKind] = &[PipelineKind::Between, PipelineKind::Before];

/// Run `splicer compose` + `wac compose` on provider + consumer to
/// produce the pre-splice composed.wasm.
fn compose_provider_consumer(root: &Path, provider_comp: &Path, consumer_comp: &Path) -> PathBuf {
    let compose_wac = root.join("compose.wac");
    let composed_path = root.join("composed.wasm");
    let out = compose(ComposeRequest {
        components: vec![
            ComponentInput {
                alias: None,
                path: provider_comp.to_path_buf(),
            },
            ComponentInput {
                alias: None,
                path: consumer_comp.to_path_buf(),
            },
        ],
        package_name: WAC_PACKAGE_NAME.to_string(),
    })
    .expect("splicer::compose");
    std::fs::write(&compose_wac, &out.wac).expect("write compose.wac");
    run_wac_command(
        &out.wac_compose_cmd(compose_wac.to_str().unwrap()),
        &composed_path,
        root,
        "wac compose (provider+consumer)",
    );
    composed_path
}

/// Splice middleware into the interface boundary of an already-
/// composed provider+consumer binary. Returns final.wasm.
fn splice_between(root: &Path, composed_path: &Path, middleware_comp: &Path) -> PathBuf {
    let spliced_wac = root.join("spliced.wac");
    let splits_dir = root.join("splits");
    std::fs::create_dir_all(&splits_dir).unwrap();

    let out = splice(SpliceRequest {
        composition_wasm: composed_path.to_path_buf(),
        rules_yaml: splice_yaml(splice_yaml_between_tmpl(), middleware_comp),
        package_name: WAC_PACKAGE_NAME.to_string(),
        splits_dir: splits_dir.clone(),
        skip_type_check: false,
    })
    .expect("splicer::splice (between)");
    std::fs::write(&spliced_wac, &out.wac).expect("write spliced.wac");
    let final_path = root.join("final.wasm");
    run_wac_command(
        &out.wac_compose_cmd(spliced_wac.to_str().unwrap()),
        &final_path,
        root,
        "wac compose (post-between-splice)",
    );
    final_path
}

/// Splice middleware in front of a lone provider first, then compose
/// that spliced-provider with the consumer. Returns final.wasm.
fn splice_before_and_compose(
    root: &Path,
    provider_comp: &Path,
    consumer_comp: &Path,
    middleware_comp: &Path,
) -> PathBuf {
    // Step 1: splice against the lone provider.
    let before_wac = root.join("before_splice.wac");
    let splits_dir = root.join("splits_before");
    std::fs::create_dir_all(&splits_dir).unwrap();

    let splice_out = splice(SpliceRequest {
        composition_wasm: provider_comp.to_path_buf(),
        rules_yaml: splice_yaml(splice_yaml_before_tmpl(), middleware_comp),
        package_name: WAC_PACKAGE_NAME.to_string(),
        splits_dir: splits_dir.clone(),
        skip_type_check: false,
    })
    .expect("splicer::splice (before)");
    std::fs::write(&before_wac, &splice_out.wac).expect("write before_splice.wac");
    let spliced_provider = root.join("spliced_provider.wasm");
    run_wac_command(
        &splice_out.wac_compose_cmd(before_wac.to_str().unwrap()),
        &spliced_provider,
        root,
        "wac compose (before-splice provider)",
    );

    // Step 2: compose the spliced provider with the consumer.
    let compose_wac = root.join("final_compose.wac");
    let compose_out = compose(ComposeRequest {
        components: vec![
            ComponentInput {
                alias: None,
                path: spliced_provider.clone(),
            },
            ComponentInput {
                alias: None,
                path: consumer_comp.to_path_buf(),
            },
        ],
        package_name: WAC_PACKAGE_NAME.to_string(),
    })
    .expect("splicer::compose (spliced_provider+consumer)");
    std::fs::write(&compose_wac, &compose_out.wac).expect("write final_compose.wac");
    let final_path = root.join("final.wasm");
    run_wac_command(
        &compose_out.wac_compose_cmd(compose_wac.to_str().unwrap()),
        &final_path,
        root,
        "wac compose (spliced_provider+consumer)",
    );
    final_path
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

    let stdout_pipe = MemoryOutputPipe::new(STDOUT_CAPTURE_BYTES);
    let wasi = WasiCtxBuilder::new().stdout(stdout_pipe.clone()).build();

    let mut config = Config::new();
    config.wasm_component_model_async(true);
    // Required for async-lifted exports (e.g. consumer `run: async
    // func()` in AsyncMode::Async). Harmless when only sync-lifted
    // exports are in use.
    config.wasm_component_model_async_stackful(true);
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

/// Tools every test in this file shells out to. Factored so adding a
/// new one touches one place.
fn require_splicer_toolchain() {
    require_tool("cargo");
    require_tool("wasm-tools");
    require_tool("wac");
}

/// Read an env var, parse it, fall back to `default` on any failure.
/// The common shape for the `SPLICER_FUZZ_*` knobs.
fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Extract a displayable message from a `catch_unwind` payload.
fn panic_msg(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "<non-string panic>".into())
}

/// Panic messages that indicate splicer correctly refused a shape
/// outside its declared support envelope. Mirrors the structural
/// fuzz test's `fuzz_is_expected_bail`.
fn is_expected_bail(msg: &str) -> bool {
    msg.contains("flat parameter values")
        || msg.contains("flat representation")
        || msg.contains("exceeds 16")
        || msg.contains("results; only 0 or 1 results")
        || msg.contains("not yet implemented")
}

/// Panic messages that indicate a harness limitation, not a splicer
/// bug. Consumer/provider cargo-build failures always fall here —
/// splicer runs after the components are built, so any rustc error
/// is a wit-bindgen/harness codegen mismatch (e.g. the consumer's
/// `api::foo(&v)` not matching wit-bindgen's per-element borrow
/// convention for structural containers with nominal elements).
fn is_harness_bail(msg: &str, _mode: AsyncMode) -> bool {
    msg.contains("cargo build: exit")
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

/// Like `run`, but drops captured stdout/stderr from the panic
/// message on failure — used for commands whose failure output is
/// expected noise (currently just `cargo build` on harness-invalid
/// shapes). The panic still mentions the label + exit code so
/// `is_harness_bail` can classify.
fn run_quiet(cmd: &mut Command, label: &str) {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{label}: spawn failed: {e}"));
    if !out.status.success() {
        // Print stderr/stdout for diagnosis when SPLICER_DEBUG_BUILD=1
        // is set; by default the test stays quiet so harness-noise
        // failures (Shape doesn't fit wit-bindgen's borrow rules) don't
        // pollute the run. Set the env var when iterating on a specific
        // shape and inspect rustc's actual error output.
        if std::env::var("SPLICER_DEBUG_BUILD").is_ok() {
            eprintln!(
                "--- {label} stderr ---\n{}\n--- {label} stdout ---\n{}",
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout),
            );
        }
        panic!("{label}: exit {:?}", out.status.code());
    }
}

/// One-time-per-mode setup: workspace + shape-independent crates
/// (consumer + middleware). The consumer's world WIT + lib.rs depend
/// on `AsyncMode` (its `run` export and the `.await` on `api::foo()`
/// differ), so this needs to be re-run when toggling modes.
/// `write_per_shape_files` then overwrites the provider crate and
/// the consumer's `deps/my-shape` copy per shape.
fn scaffold_common(root: &Path, _mode: AsyncMode) -> std::io::Result<()> {
    std::fs::write(root.join("Cargo.toml"), WORKSPACE_CARGO_TOML)?;

    write_crate(
        root,
        "provider",
        PROVIDER_CARGO_TOML,
        "// placeholder\n",
        &[],
    )?;
    // Consumer's lib.rs and world.wit are both per-shape — written
    // by `write_per_shape_files`. Scaffold with a placeholder.
    write_crate(
        root,
        "consumer",
        CONSUMER_CARGO_TOML,
        "// placeholder\n",
        &[],
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
/// middleware, consumer's own world) is stable across shapes *within
/// a single AsyncMode iteration*.
fn write_per_shape_files(root: &Path, shape: &Shape, mode: AsyncMode) -> std::io::Result<()> {
    let provider_lib = provider_lib_rs(shape, mode);
    let provider_world = provider_world_wit(shape, mode);
    let consumer_lib = consumer_lib_rs(shape, mode);
    let consumer_world = consumer_world_wit(shape, mode);
    let dep_wit = consumer_shape_dep_wit(shape, mode);

    std::fs::write(root.join("provider/src/lib.rs"), provider_lib)?;
    let provider_wit_dir = root.join("provider/wit");
    std::fs::create_dir_all(&provider_wit_dir)?;
    std::fs::write(provider_wit_dir.join("world.wit"), provider_world)?;

    std::fs::write(root.join("consumer/src/lib.rs"), consumer_lib)?;
    let consumer_wit_dir = root.join("consumer/wit");
    std::fs::create_dir_all(&consumer_wit_dir)?;
    std::fs::write(consumer_wit_dir.join("world.wit"), consumer_world)?;
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
