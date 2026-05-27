//! Structural fuzz harness for the typed (tier-3/4) codegen.
//!
//! Generates random WIT shapes within typed's value-typed scope (no
//! resources / handles / futures / streams / error-context / maps),
//! runs each through [`generate_wrapper_crate`] for both
//! [`Behavior::Transform`] and [`Behavior::Virtualize`], and asserts:
//! (1) the returned `lib.rs` parses as Rust syntax via `syn`;
//! (2) Transform output references the target interface's bindings
//!     path; Virtualize output does not.
//!
//! Today the typed codegen emits `async fn` guest method bodies
//! unconditionally; this harness only generates `async func` WIT to
//! stay inside what the codegen can legally produce. When sync
//! wrapping lands the generator should mix the two.
//!
//! Env knobs (unused in default `cargo test` runs):
//!     SPLICER_FUZZ_ITERS   iteration count (default 200)
//!     SPLICER_FUZZ_SEED    base seed (override to reproduce a
//!                          specific failing iteration)

use crate::adapter::fuzz_common::{run_structural_fuzz, FuzzOutcome};
use crate::adapter::typed::{generate_wrapper_crate, Behavior, GenerateWrapperInput, WrapperCrate};
use arbitrary::{Arbitrary, Unstructured};
use std::fmt::Write;

/// Depth limit for generated type trees. Typed's IR has more
/// constructor variants per level (record fields, variant arms,
/// multi-interface worlds) than tier-1/2; depth-1 keeps the WIT
/// text small enough that wit-bindgen + IR build run quickly.
const FUZZ_MAX_DEPTH: u32 = 1;

const FUZZ_PACKAGE: &str = "fuzz:gen@0.1.0";
const FUZZ_INTERFACE_QNAME: &str = "fuzz:gen/ops@0.1.0";
const FUZZ_WORLD: &str = "w";

/// One generator output: WIT source text plus the bindings-path
/// substring the wrapper's emitted `lib.rs` should reference under
/// [`Behavior::Transform`] (and avoid under [`Behavior::Virtualize`]).
struct FuzzWit {
    text: String,
    /// e.g. `bindings::fuzz::gen::ops::get` — wit-bindgen's snake-case
    /// transform of the package + interface + function path.
    expected_call_path: String,
}

/// Rendered value type: an inline WIT type reference (e.g. `u32`,
/// `list<u32>`, `option<r0>`).
struct WitRef(String);

/// Top-level type declarations to append to the interface body
/// (records, variants, enums, flags — typed requires these be named).
#[derive(Default)]
struct TypeDecls {
    decls: String,
    next_record: u32,
    next_variant: u32,
    next_enum: u32,
    next_flags: u32,
}

impl TypeDecls {
    fn fresh_record(&mut self) -> String {
        let n = self.next_record;
        self.next_record += 1;
        format!("r{n}")
    }
    fn fresh_variant(&mut self) -> String {
        let n = self.next_variant;
        self.next_variant += 1;
        format!("v{n}")
    }
    fn fresh_enum(&mut self) -> String {
        let n = self.next_enum;
        self.next_enum += 1;
        format!("e{n}")
    }
    fn fresh_flags(&mut self) -> String {
        let n = self.next_flags;
        self.next_flags += 1;
        format!("fl{n}")
    }
    fn push(&mut self, decl: &str) {
        writeln!(&mut self.decls, "    {decl}").unwrap();
    }
}

/// Pick a primitive `WitRef`. Excludes anything typed rejects
/// (`error-context`) plus shapes that need named declarations.
fn fuzz_primitive(u: &mut Unstructured<'_>) -> arbitrary::Result<WitRef> {
    const PRIMS: &[&str] = &[
        "bool", "s8", "u8", "s16", "u16", "s32", "u32", "s64", "u64", "f32", "f64", "char",
        "string",
    ];
    Ok(WitRef(PRIMS[u.choose_index(PRIMS.len())?].to_string()))
}

/// Build a random value type. Compounds that WIT requires be named
/// (record / variant / enum / flags) get a fresh declaration appended
/// to `decls`; the returned `WitRef` is the type-name reference.
fn fuzz_value_type(
    u: &mut Unstructured<'_>,
    depth: u32,
    decls: &mut TypeDecls,
) -> arbitrary::Result<WitRef> {
    if depth == 0 {
        return fuzz_primitive(u);
    }
    // 11 buckets: one "another primitive" leaf + 10 compound shapes.
    match u.choose_index(11)? {
        0 => fuzz_primitive(u),
        1 => {
            let inner = fuzz_value_type(u, depth - 1, decls)?;
            Ok(WitRef(format!("list<{}>", inner.0)))
        }
        2 => {
            let inner = fuzz_value_type(u, depth - 1, decls)?;
            Ok(WitRef(format!("option<{}>", inner.0)))
        }
        3 => {
            let ok = if bool::arbitrary(u)? {
                Some(fuzz_value_type(u, depth - 1, decls)?)
            } else {
                None
            };
            let err = if bool::arbitrary(u)? {
                Some(fuzz_value_type(u, depth - 1, decls)?)
            } else {
                None
            };
            Ok(WitRef(match (ok, err) {
                (Some(o), Some(e)) => format!("result<{}, {}>", o.0, e.0),
                (Some(o), None) => format!("result<{}>", o.0),
                (None, Some(e)) => format!("result<_, {}>", e.0),
                (None, None) => "result".to_string(),
            }))
        }
        4 => {
            let count = u.int_in_range(2..=4)?;
            let mut parts = Vec::with_capacity(count);
            for _ in 0..count {
                parts.push(fuzz_value_type(u, depth - 1, decls)?.0);
            }
            Ok(WitRef(format!("tuple<{}>", parts.join(", "))))
        }
        5 => {
            let count = u.int_in_range(1..=3)?;
            let mut fields = Vec::with_capacity(count);
            for i in 0..count {
                let ty = fuzz_value_type(u, depth - 1, decls)?;
                fields.push(format!("f{i}: {}", ty.0));
            }
            let name = decls.fresh_record();
            decls.push(&format!("record {name} {{ {} }}", fields.join(", ")));
            Ok(WitRef(name))
        }
        6 => {
            let count = u.int_in_range(1..=3)?;
            let mut cases = Vec::with_capacity(count);
            for i in 0..count {
                let payload = if bool::arbitrary(u)? {
                    Some(fuzz_value_type(u, depth - 1, decls)?)
                } else {
                    None
                };
                cases.push(match payload {
                    Some(t) => format!("c{i}({})", t.0),
                    None => format!("c{i}"),
                });
            }
            let name = decls.fresh_variant();
            decls.push(&format!("variant {name} {{ {} }}", cases.join(", ")));
            Ok(WitRef(name))
        }
        7 => {
            let count = u.int_in_range(1..=4)?;
            let tags: Vec<String> = (0..count).map(|i| format!("t{i}")).collect();
            let name = decls.fresh_enum();
            decls.push(&format!("enum {name} {{ {} }}", tags.join(", ")));
            Ok(WitRef(name))
        }
        8 => {
            // Component-model caps flags at 32 members.
            let count = u.int_in_range::<usize>(1..=8)?;
            let labels: Vec<String> = (0..count).map(|i| format!("l{i}")).collect();
            let name = decls.fresh_flags();
            decls.push(&format!("flags {name} {{ {} }}", labels.join(", ")));
            Ok(WitRef(name))
        }
        _ => fuzz_primitive(u),
    }
}

/// Render a WIT source text exporting a single async function
/// returning a random value type.
fn fuzz_wit(u: &mut Unstructured<'_>) -> arbitrary::Result<FuzzWit> {
    let mut decls = TypeDecls::default();
    let ret = fuzz_value_type(u, FUZZ_MAX_DEPTH, &mut decls)?;
    let text = format!(
        "package {pkg};\n\
         interface ops {{\n\
         {decls}    get: async func() -> {ret};\n\
         }}\n\
         world {world} {{ export ops; }}\n",
        pkg = FUZZ_PACKAGE,
        decls = decls.decls,
        ret = ret.0,
        world = FUZZ_WORLD,
    );
    Ok(FuzzWit {
        text,
        expected_call_path: "bindings::fuzz::gen::ops::get".to_string(),
    })
}

/// Error messages typed's codegen produces for shapes outside its
/// support envelope. The current generator avoids them, so anything
/// here only fires if the generator drifts.
fn fuzz_is_expected_bail(msg: &str) -> bool {
    msg.contains("not supported")
        || msg.contains("not yet implemented")
        || msg.contains("unresolved WIT type")
}

/// Run `generate_wrapper_crate` for one `Behavior` and validate the
/// output's lib.rs parses as Rust and matches the expected call-site
/// presence/absence convention.
fn check_one(wit: &FuzzWit, behavior: Behavior) -> Result<(), String> {
    let crate_out: WrapperCrate = generate_wrapper_crate(&GenerateWrapperInput {
        target_wit: &wit.text,
        world_name: Some(FUZZ_WORLD),
        interface_qualified_name: FUZZ_INTERFACE_QNAME,
        behavior,
        strategy_crate_name: "fuzz-strategy",
        strategy_crate_path: "/abs/path/to/fuzz-strategy",
        strategy_type: "FuzzStrategy",
        splicer_tool_sdk_version: crate::test_consts::SDK_TEST_VERSION,
    })
    .map_err(|e| format!("{e:#}"))?;

    syn::parse_file(&crate_out.lib_rs).map_err(|e| {
        format!(
            "lib.rs not valid Rust ({behavior:?}): {e}\nWIT:\n{}",
            wit.text
        )
    })?;

    match behavior {
        Behavior::Transform => {
            if !crate_out.lib_rs.contains(&wit.expected_call_path) {
                return Err(format!(
                    "transform: expected call site `{}` missing from lib.rs\nWIT:\n{}",
                    wit.expected_call_path, wit.text
                ));
            }
        }
        Behavior::Virtualize => {
            if crate_out.lib_rs.contains(&wit.expected_call_path) {
                return Err(format!(
                    "virtualize: unexpected call site `{}` present in lib.rs\nWIT:\n{}",
                    wit.expected_call_path, wit.text
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn fuzz_structural_shapes() {
    run_structural_fuzz("typed-fuzz", |bytes| {
        let mut u = Unstructured::new(bytes);
        let wit = fuzz_wit(&mut u).map_err(|_| "ran out of random bytes".to_string())?;

        for behavior in [Behavior::Transform, Behavior::Virtualize] {
            if let Err(msg) = check_one(&wit, behavior) {
                return if fuzz_is_expected_bail(&msg) {
                    Ok(FuzzOutcome::ExpectedBail)
                } else {
                    Err(msg)
                };
            }
        }
        Ok(FuzzOutcome::Passed)
    });
}
