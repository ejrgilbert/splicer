//! Structural fuzz harness + regression tests for bugs it surfaced.
//!
//! The fuzz test generates random `ValueType` trees (bounded depth),
//! wraps each as a single-result async func, and asserts the adapter
//! generator either produces a valid component or bails with a known-
//! limit error. The point is structural coverage of shapes the hand-
//! written tests have never seen — combinations of record fields,
//! variant arms, and nested compounds that would be tedious to
//! enumerate by hand.
//!
//! Env knobs for replay / tuning (unused in default `cargo test` runs):
//!     SPLICER_FUZZ_ITERS   iteration count (default 200)
//!     SPLICER_FUZZ_SEED    base seed (default time-based; override to
//!                          reproduce a specific failing iteration)
//!
//! To replay a specific failing iteration after it's reported by a run:
//!     SPLICER_FUZZ_SEED=<iter_seed> SPLICER_FUZZ_ITERS=1 \
//!         cargo test --lib fuzz_structural_shapes -- --nocapture

use super::*;
use arbitrary::{Arbitrary, Unstructured};

/// Pinned default seed — overrideable with `SPLICER_FUZZ_SEED`.
const DEFAULT_FUZZ_SEED: u64 = 0xDEAD_BEEF;
/// Default iteration count for the structural fuzz loop.
const DEFAULT_FUZZ_ITERS: usize = 200;
/// Random bytes drawn per fuzz iteration.
const FUZZ_BYTES_PER_ITER: usize = 256;
/// Max recursion depth for generated `ValueType` trees.
const FUZZ_MAX_DEPTH: u32 = 2;
/// Max failures echoed into the test output before truncating.
const MAX_FAILURES_SHOWN: usize = 20;

// ── Tier 1: async indirect-params (lower_to_memory) ──────────────────
//
// Async funcs whose params flatten past `MAX_FLAT_ASYNC_PARAMS` (4)
// canon-lower with `indirect_params = true` — the import takes a
// single params-pointer, so the wrapper must lower its flat function
// params into a memory-resident params record before the handler call.
// See `docs/TODO/tier2-async-target-indirect-params.md` for the full
// rationale; same fix applies to both tiers.
//
// Until primitive `lower_to_memory` lands these tests fail with the
// existing bail. They define the goal: the all-u32 shape pins the
// minimal indirect-params path; the mixed-primitives shape pins
// store-width + canonical-ABI inter-field alignment math.

/// Five `u32` params — flattens to 5 i32 slots → `indirect_params=true`
/// on canon-lower-async. Smallest shape that forces the lowering.
#[test]
fn test_adapter_async_5_u32_params_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let iface = make_iface(vec![(
        "many",
        sig(
            true,
            &["a", "b", "c", "d", "e"],
            vec![u32_id; 5], // 5 > MAX_FLAT_ASYNC_PARAMS=4 → indirect_params
            vec![u32_id],
        ),
    )]);
    let bytes = gen_adapter(
        "test:pkg/many@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Mixed primitive widths in indirect-params position — exercises
/// `i32.store` / `i64.store` / `f32.store` / `f64.store` /
/// `i32.store8` plus inter-field padding (`u32`→`u64` and `bool`→`char`
/// transitions force alignment bumps).
#[test]
fn test_adapter_async_mixed_primitives_indirect_params_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let u64_id = arena.intern_val(ValueType::U64);
    let f32_id = arena.intern_val(ValueType::F32);
    let f64_id = arena.intern_val(ValueType::F64);
    let bool_id = arena.intern_val(ValueType::Bool);
    let char_id = arena.intern_val(ValueType::Char);
    let iface = make_iface(vec![(
        "mixed",
        sig(
            true,
            &["a", "b", "c", "d", "e", "f"],
            vec![u32_id, u64_id, f32_id, f64_id, bool_id, char_id], // 6 slots
            vec![u32_id],
        ),
    )]);
    let bytes = gen_adapter(
        "test:pkg/mixed-async@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Phase-2 record param — `record { a..e: u32 }` flattens to 5 i32
/// slots → indirect-params. Exercises `RecordLower` as a no-op
/// 1→N decomposition; the inner `u32` lifts then drive the cursor.
#[test]
fn test_adapter_async_record_param_indirect_params_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let record = arena.intern_val(ValueType::Record(vec![
        ("a".into(), u32_id),
        ("b".into(), u32_id),
        ("c".into(), u32_id),
        ("d".into(), u32_id),
        ("e".into(), u32_id),
    ]));
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([(
            "many".to_string(),
            sig(true, &["r"], vec![record], vec![u32_id]),
        )]),
        type_exports: BTreeMap::from([("rec5".to_string(), record)]),
    });
    let bytes = gen_adapter(
        "test:pkg/rec5-async@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Phase-2 tuple param — `tuple<u32, u64, f32, f64, bool>` flattens
/// to 5 mixed slots → indirect-params. Exercises `TupleLower` plus
/// the inter-field-alignment math from the mixed-primitive test
/// applied inside an aggregate.
#[test]
fn test_adapter_async_tuple_param_indirect_params_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let u64_id = arena.intern_val(ValueType::U64);
    let f32_id = arena.intern_val(ValueType::F32);
    let f64_id = arena.intern_val(ValueType::F64);
    let bool_id = arena.intern_val(ValueType::Bool);
    let tup = arena.intern_val(ValueType::Tuple(vec![
        u32_id, u64_id, f32_id, f64_id, bool_id,
    ]));
    let iface = make_iface(vec![("many", sig(true, &["t"], vec![tup], vec![u32_id]))]);
    let bytes = gen_adapter(
        "test:pkg/tup5-async@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Phase-2 enum / flags / record-with-flags-field — aggregates whose
/// leaves are non-numeric primitives. Pins `EnumLower` and
/// `FlagsLower` emit shape end-to-end.
#[test]
fn test_adapter_async_enum_flags_record_indirect_params_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let color = arena.intern_val(ValueType::Enum(vec![
        "red".into(),
        "green".into(),
        "blue".into(),
    ]));
    let perms = arena.intern_val(ValueType::Flags(vec![
        "read".into(),
        "write".into(),
        "exec".into(),
    ]));
    // Record with mixed leaf kinds; flat = enum(i32) + flags(i32) +
    // u32 + u32 + u32 = 5 i32 slots → indirect-params.
    let record = arena.intern_val(ValueType::Record(vec![
        ("c".into(), color),
        ("f".into(), perms),
        ("a".into(), u32_id),
        ("b".into(), u32_id),
        ("d".into(), u32_id),
    ]));
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([(
            "many".to_string(),
            sig(true, &["r"], vec![record], vec![u32_id]),
        )]),
        type_exports: BTreeMap::from([
            ("color".to_string(), color),
            ("perms".to_string(), perms),
            ("rec5".to_string(), record),
        ]),
    });
    let bytes = gen_adapter(
        "test:pkg/cfr-async@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

#[test]
fn test_adapter_record_with_list_field_repro() {
    let mut arena = TypeArena::default();
    let char_id = arena.intern_val(ValueType::Char);
    let list_id = arena.intern_val(ValueType::List(char_id));
    let record_id = arena.intern_val(ValueType::Record(vec![("f0".into(), list_id)]));
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([("get".to_string(), sig(true, &[], vec![], vec![record_id]))]),
        type_exports: BTreeMap::from([("rec".to_string(), record_id)]),
    });
    let bytes = gen_adapter(
        "test:repro/rec@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Emit a primitive `ValueType`. Excludes `Resource` / `AsyncHandle` /
/// `Map` / `ErrorContext` — the synth-split WAT helper panics on those
/// and they need their own (more involved) test paths.
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
/// in the interface's `type_exports` for the adapter to reference
/// them (record / variant / enum / flags — matches the convention of
/// the hand-written tests).
fn fuzz_value_type(
    u: &mut Unstructured<'_>,
    arena: &mut TypeArena,
    depth: u32,
    need_export: &mut Vec<ValueTypeId>,
) -> arbitrary::Result<ValueTypeId> {
    if depth == 0 {
        return Ok(arena.intern_val(fuzz_primitive(u)?));
    }

    // 11 shape constructors — one is "another primitive" so leaves
    // keep showing up even at higher depths.
    match u.choose_index(11)? {
        0 => Ok(arena.intern_val(fuzz_primitive(u)?)),
        1 => {
            let inner = fuzz_value_type(u, arena, depth - 1, need_export)?;
            Ok(arena.intern_val(ValueType::List(inner)))
        }
        2 => {
            let inner = fuzz_value_type(u, arena, depth - 1, need_export)?;
            let n = u.int_in_range::<u32>(1..=8)?;
            Ok(arena.intern_val(ValueType::FixedSizeList(inner, n)))
        }
        3 => {
            let count = u.int_in_range(2..=4)?;
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
            let count = u.int_in_range(1..=4)?;
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
            let count = u.int_in_range(1..=4)?;
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
            let count = u.int_in_range::<usize>(1..=32)?;
            let labels: Vec<String> = (0..count).map(|i| format!("fl{i}")).collect();
            let id = arena.intern_val(ValueType::Flags(labels));
            need_export.push(id);
            Ok(id)
        }
        _ => Ok(arena.intern_val(fuzz_primitive(u)?)),
    }
}

/// Deterministic LCG byte source so a failing iteration is replayable
/// via `SPLICER_FUZZ_SEED` + `SPLICER_FUZZ_ITERS`. Intentionally
/// avoids bringing in `rand` just for this harness.
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

/// An error message matching one of these prefixes is an expected
/// bail — the adapter correctly refused a shape outside its current
/// support envelope. Anything else is a real failure.
fn fuzz_is_expected_bail(msg: &str) -> bool {
    msg.contains("flat parameter values")
        || msg.contains("flat representation")
        || msg.contains("exceeds 16") // "flattens to N core values (exceeds 16..."
        || msg.contains("results; only 0 or 1 results")
        || msg.contains("not yet implemented")
}

#[test]
fn fuzz_structural_shapes() {
    let iters: usize = std::env::var("SPLICER_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_FUZZ_ITERS);
    let base_seed: u64 = std::env::var("SPLICER_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_FUZZ_SEED);
    eprintln!("fuzz: iters={iters} base_seed={base_seed}");

    let mut passed = 0usize;
    let mut expected_bails = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for i in 0..iters {
        let iter_seed = base_seed.wrapping_add(i as u64);
        let bytes = fuzz_seeded_bytes(iter_seed, FUZZ_BYTES_PER_ITER);

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut u = Unstructured::new(&bytes);
            let mut arena = TypeArena::default();
            let mut need_export: Vec<ValueTypeId> = Vec::new();

            let result_id = fuzz_value_type(&mut u, &mut arena, FUZZ_MAX_DEPTH, &mut need_export)
                .map_err(|_| "ran out of random bytes".to_string())?;
            let shape = arena.canonical_val(result_id);

            let type_exports: BTreeMap<String, ValueTypeId> = need_export
                .iter()
                .enumerate()
                .map(|(idx, id)| (format!("ty{idx}"), *id))
                .collect();
            let iface = InterfaceType::Instance(InstanceInterface {
                functions: BTreeMap::from([(
                    "get".to_string(),
                    sig(true, &[], vec![], vec![result_id]),
                )]),
                type_exports,
            });

            let tmp = tempfile::tempdir().unwrap();
            let hooks = [
                "splicer:tier1/before".to_string(),
                "splicer:tier1/after".to_string(),
            ];
            let split = synth_split("test:fuzz/iface@1.0.0", &iface, &arena, SplitKind::Consumer);
            let split_path = split.path().to_str().unwrap();

            let gen = crate::adapter::generate_tier1_adapter(
                "fuzz-mdl",
                "test:fuzz/iface@1.0.0",
                &hooks,
                tmp.path().to_str().unwrap(),
                split_path,
            );

            match gen {
                Ok(path) => {
                    let bytes = std::fs::read(&path).map_err(|e| format!("read: {e}"))?;
                    let mut validator =
                        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
                    validator
                        .validate_all(&bytes)
                        .map_err(|e| format!("invalid component for shape `{shape}`: {e}"))?;
                    Ok::<String, String>("passed".to_string())
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    if fuzz_is_expected_bail(&msg) {
                        Ok("expected-bail".to_string())
                    } else {
                        Err(format!("unexpected bail for shape `{shape}`: {msg}"))
                    }
                }
            }
        }));

        match outcome {
            Ok(Ok(tag)) if tag == "passed" => passed += 1,
            Ok(Ok(_)) => expected_bails += 1,
            Ok(Err(msg)) => failures.push(format!("iter {i} seed {iter_seed}: {msg}")),
            Err(_) => failures.push(format!("iter {i} seed {iter_seed}: PANIC")),
        }
    }

    eprintln!(
        "fuzz: passed={passed} expected_bails={expected_bails} failures={}",
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
            "{} structural fuzz iterations failed — replay a single case with \
             SPLICER_FUZZ_SEED=<iter_seed_from_output> SPLICER_FUZZ_ITERS=1",
            failures.len()
        );
    }
}
