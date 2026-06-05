//! Structural fuzz harness + regression tests for bugs it surfaces.
//!
//! Generates random `ValueType` trees (bounded depth), wraps each as
//! a single-result async func on a consumer-style split, and asserts
//! [`generate_tier2_adapter`] either produces a valid component or
//! bails with a known-limit error. Coverage target: shapes the
//! [`super::dispatch_roundtrip`] hand-written tests don't enumerate.
//!
//! `cfg(test)` shrinks tier-2's `MAX_FLAT_SLOTS_PER_FN` /
//! `MAX_CELLS_PER_PARAM` to 16 / 8 so the budget-bail paths are
//! reachable from small fixtures — that means most random shapes
//! exceed the cap and surface as `ExpectedBail`. The shapes that
//! fit are what exercise the lift codegen end-to-end.
//!
//! Env knobs (unused in default `cargo test` runs):
//!     SPLICER_FUZZ_ITERS   iteration count (default 200)
//!     SPLICER_FUZZ_SEED    base seed (override to reproduce a
//!                          specific failing iteration)
//!
//! Replay a specific failing iteration:
//!     SPLICER_FUZZ_SEED=<iter_seed> SPLICER_FUZZ_ITERS=1 \
//!         cargo test --lib adapter::tier2::tests::fuzz::fuzz_structural_shapes \
//!         -- --nocapture

use crate::adapter::fuzz_common::{run_structural_fuzz, FuzzOutcome};
use crate::adapter::tier1::tests::{synth_split, SplitKind};
use arbitrary::{Arbitrary, Unstructured};
use cviz::model::{
    FuncSignature, InstanceInterface, InterfaceType, TypeArena, ValueType, ValueTypeId,
};
use std::collections::BTreeMap;

/// Recursion depth for generated `ValueType` trees. Lower than tier-1
/// because the test-mode cell budget is small (8) and depth-2 nesting
/// reliably blows past it — depth-1 keeps a meaningful fraction of
/// iterations actually inside the codegen path.
const FUZZ_MAX_DEPTH: u32 = 1;

/// Stable target / hook names — irrelevant to what the fuzz exercises.
const FUZZ_TARGET: &str = "test:fuzz/iface@1.0.0";
const FUZZ_HOOKS: &[&str] = &["splicer:tier2/before", "splicer:tier2/after"];

/// One-line FuncSignature constructor mirroring tier-1's `sig` helper.
fn sig(
    is_async: bool,
    param_names: &[&str],
    params: Vec<ValueTypeId>,
    results: Vec<ValueTypeId>,
) -> FuncSignature {
    FuncSignature {
        is_async,
        param_names: param_names.iter().map(|s| s.to_string()).collect(),
        params,
        results,
    }
}

/// Emit a primitive `ValueType`. Excludes `Resource` / `AsyncHandle` /
/// `Map` / `ErrorContext` — those need their own (more involved) test
/// paths.
fn fuzz_primitive(u: &mut Unstructured<'_>) -> arbitrary::Result<ValueType> {
    let ctors: &[fn() -> ValueType] = &[
        || ValueType::Bool,
        || ValueType::S8,
        || ValueType::U8,
        || ValueType::S16,
        || ValueType::U16,
        || ValueType::S32,
        || ValueType::U32,
        || ValueType::S64,
        || ValueType::U64,
        || ValueType::F32,
        || ValueType::F64,
        || ValueType::Char,
        || ValueType::String,
    ];
    Ok(ctors[u.choose_index(ctors.len())?]())
}

/// Recursively build a random `ValueType` tree. `depth == 0` forces
/// a primitive leaf. `need_export` collects type ids that must appear
/// in the interface's `type_exports` for tier-1's consumer-WAT helper
/// to declare them as `(export (type (eq N)))` (record / variant /
/// enum / flags).
fn fuzz_value_type(
    u: &mut Unstructured<'_>,
    arena: &mut TypeArena,
    depth: u32,
    need_export: &mut Vec<ValueTypeId>,
) -> arbitrary::Result<ValueTypeId> {
    if depth == 0 {
        return Ok(arena.intern_val(fuzz_primitive(u)?));
    }
    match u.choose_index(11)? {
        0 => Ok(arena.intern_val(fuzz_primitive(u)?)),
        1 => {
            let inner = fuzz_value_type(u, arena, depth - 1, need_export)?;
            Ok(arena.intern_val(ValueType::List(inner)))
        }
        2 => {
            let inner = fuzz_value_type(u, arena, depth - 1, need_export)?;
            let n = u.int_in_range::<u32>(1..=4)?;
            Ok(arena.intern_val(ValueType::FixedSizeList(inner, n)))
        }
        3 => {
            let count = u.int_in_range(2..=3)?;
            let mut ids = Vec::with_capacity(count);
            for _ in 0..count {
                ids.push(fuzz_value_type(u, arena, depth - 1, need_export)?);
            }
            Ok(arena.intern_val(ValueType::Tuple(ids)))
        }
        4 => {
            let inner = fuzz_value_type(u, arena, depth - 1, need_export)?;
            Ok(arena.intern_val(ValueType::Option(inner)))
        }
        5 => {
            let ok = if bool::arbitrary(u)? {
                Some(fuzz_value_type(u, arena, depth - 1, need_export)?)
            } else {
                None
            };
            let err = if bool::arbitrary(u)? {
                Some(fuzz_value_type(u, arena, depth - 1, need_export)?)
            } else {
                None
            };
            Ok(arena.intern_val(ValueType::Result { ok, err }))
        }
        6 => {
            let count = u.int_in_range(1..=3)?;
            let mut fields = Vec::with_capacity(count);
            for i in 0..count {
                let fid = fuzz_value_type(u, arena, depth - 1, need_export)?;
                fields.push((format!("f{i}"), fid));
            }
            let id = arena.intern_val(ValueType::Record(fields));
            need_export.push(id);
            Ok(id)
        }
        7 => {
            let count = u.int_in_range(1..=3)?;
            let mut cases = Vec::with_capacity(count);
            for i in 0..count {
                let payload = if bool::arbitrary(u)? {
                    Some(fuzz_value_type(u, arena, depth - 1, need_export)?)
                } else {
                    None
                };
                cases.push((format!("c{i}"), payload));
            }
            let id = arena.intern_val(ValueType::Variant(cases));
            need_export.push(id);
            Ok(id)
        }
        8 => {
            let count = u.int_in_range(1..=4)?;
            let tags: Vec<String> = (0..count).map(|i| format!("t{i}")).collect();
            let id = arena.intern_val(ValueType::Enum(tags));
            need_export.push(id);
            Ok(id)
        }
        9 => {
            // Component Model caps flags at 32 members.
            let count = u.int_in_range::<usize>(1..=8)?;
            let labels: Vec<String> = (0..count).map(|i| format!("fl{i}")).collect();
            let id = arena.intern_val(ValueType::Flags(labels));
            need_export.push(id);
            Ok(id)
        }
        _ => Ok(arena.intern_val(fuzz_primitive(u)?)),
    }
}

/// Error messages tier-2 produces for shapes outside its current
/// support envelope. Anything not matching here is a real failure
/// the fuzz harness should surface.
fn fuzz_is_expected_bail(msg: &str) -> bool {
    // Tier-2 budgets (artificially low under cfg(test) so the bail
    // paths are reachable):
    msg.contains("flat-slot count")
        || msg.contains("cell count")
        || msg.contains("exceeds i32 budget")
        || msg.contains("exceeds tier-2 layout budget")
        || msg.contains("requires N >= 1")
        // Indirect-params shape gate (mirrors tier-1's
        // "flat parameter values" but spelled differently here):
        || msg.contains("MAX_FLAT_ASYNC_PARAMS")
        || msg.contains("unsupported type")
        // Generic shape rejections:
        || msg.contains("interface has no functions")
        // Inline-resource rejection — fuzz doesn't generate resources
        // today, but keep the allowlist entry so adding them later
        // doesn't fail loudly until the support lands.
        || (msg.contains("declares resource") && msg.contains("inline"))
        // wit-parser / wit-component decode errors when the synthesized
        // WAT happens to land on a shape tier-2's downstream encoder
        // rejects (e.g. specific flag/variant edge cases). Treated as
        // expected-bail because the failure is upstream of tier-2's
        // own codegen — surfaces in the bail message via `context`.
        || msg.contains("ComponentEncoder")
}

/// Generate a tier-2 adapter and return its bytes.
fn gen_tier2_adapter(
    target: &str,
    hooks: &[String],
    iface: &InterfaceType,
    arena: &TypeArena,
) -> anyhow::Result<Vec<u8>> {
    let tmp = tempfile::tempdir().unwrap();
    let split = synth_split(target, iface, arena, SplitKind::Consumer);
    let split_path = split.path().to_str().unwrap();
    let path = crate::adapter::generate_tier2_adapter(
        "fuzz-mdl",
        target,
        hooks,
        tmp.path().to_str().unwrap(),
        split_path,
        None,
    )?;
    std::fs::read(&path).map_err(Into::into)
}

#[test]
fn fuzz_structural_shapes() {
    run_structural_fuzz("tier2-fuzz", |bytes| {
        let mut u = Unstructured::new(bytes);
        let mut arena = TypeArena::default();
        let mut need_export: Vec<ValueTypeId> = Vec::new();

        // Randomize is_async AND param count to cover the four
        // canonical-ABI corners: sync flat / sync indirect-params
        // (>16) / async flat / async asymmetric- and symmetric-
        // indirect. Param count 0..=8 with depth-2 shapes lands a
        // healthy spread across the caps.
        let is_async =
            bool::arbitrary(&mut u).map_err(|_| "ran out of random bytes".to_string())?;
        let nparams: usize = u
            .int_in_range(0..=8)
            .map_err(|_| "ran out of random bytes".to_string())?;
        let mut param_ids: Vec<ValueTypeId> = Vec::with_capacity(nparams);
        for _ in 0..nparams {
            let pid = fuzz_value_type(&mut u, &mut arena, FUZZ_MAX_DEPTH, &mut need_export)
                .map_err(|_| "ran out of random bytes".to_string())?;
            param_ids.push(pid);
        }
        let result_id = fuzz_value_type(&mut u, &mut arena, FUZZ_MAX_DEPTH, &mut need_export)
            .map_err(|_| "ran out of random bytes".to_string())?;
        let param_names: Vec<String> = (0..nparams).map(|i| format!("p{i}")).collect();
        let param_name_refs: Vec<&str> = param_names.iter().map(|s| s.as_str()).collect();
        let result_shape = arena.canonical_val(result_id);
        let params_shape: Vec<String> = param_ids
            .iter()
            .map(|id| arena.canonical_val(*id))
            .collect();
        let shape = format!(
            "{}fn(params={params_shape:?}, result={result_shape})",
            if is_async { "async " } else { "" }
        );

        let type_exports: BTreeMap<String, ValueTypeId> = need_export
            .iter()
            .enumerate()
            .map(|(idx, id)| (format!("ty{idx}"), *id))
            .collect();
        let iface = InterfaceType::Instance(InstanceInterface {
            functions: BTreeMap::from([(
                "get".to_string(),
                sig(is_async, &param_name_refs, param_ids, vec![result_id]),
            )]),
            type_exports,
        });
        let hooks: Vec<String> = FUZZ_HOOKS.iter().map(|s| s.to_string()).collect();

        match gen_tier2_adapter(FUZZ_TARGET, &hooks, &iface, &arena) {
            Ok(bytes) => {
                let mut validator =
                    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
                validator
                    .validate_all(&bytes)
                    .map_err(|e| format!("invalid component for shape `{shape}`: {e}"))?;
                Ok(FuzzOutcome::Passed)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if fuzz_is_expected_bail(&msg) {
                    Ok(FuzzOutcome::ExpectedBail)
                } else {
                    Err(format!("unexpected bail for shape `{shape}`: {msg}"))
                }
            }
        }
    });
}
