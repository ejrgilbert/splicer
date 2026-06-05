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
use crate::adapter::fuzz_common::{run_structural_fuzz, FuzzOutcome};
use arbitrary::{Arbitrary, Unstructured};

/// Max recursion depth for generated `ValueType` trees.
const FUZZ_MAX_DEPTH: u32 = 2;

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

/// Record param `{ a..e: u32 }` flattens to 5 i32 slots → indirect-
/// params. Exercises `RecordLower` as a no-op 1→N decomposition;
/// the inner `u32` lifts drive the cursor.
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

/// Tuple param `tuple<u32, u64, f32, f64, bool>` flattens to 5 mixed
/// slots → indirect-params. Exercises `TupleLower` plus the inter-
/// field-alignment math from the mixed-primitive test applied
/// inside an aggregate.
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

/// Enum / flags / record-with-flags-field — aggregates whose leaves
/// are non-numeric primitives. Pins `EnumLower` and `FlagsLower`
/// emit shape end-to-end.
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

/// `list<T>` / `string` params in indirect-params position — both
/// flatten to (ptr, len) pairs; our wrapper passes them through
/// unchanged into the params record.
#[test]
fn test_adapter_async_string_list_indirect_params_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let string_id = arena.intern_val(ValueType::String);
    let list_u32 = arena.intern_val(ValueType::List(u32_id));
    // string(2) + list(2) + 3×u32 = 7 flat slots → indirect-params.
    let iface = make_iface(vec![(
        "many",
        sig(
            true,
            &["s", "l", "a", "b", "c"],
            vec![string_id, list_u32, u32_id, u32_id, u32_id],
            vec![u32_id],
        ),
    )]);
    let bytes = gen_adapter(
        "test:pkg/strlst-async@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// `list<u32, 4>` (fixed-length) flattens to 4 i32 slots inlined.
/// Exercises `FixedLengthListLowerToMemory`'s per-iter block replay
/// with cursor-shift rewrite.
#[test]
fn test_adapter_async_fixed_list_indirect_params_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let fsl = arena.intern_val(ValueType::FixedSizeList(u32_id, 4));
    // 4 fixed-list slots + 2 u32 = 6 flat slots → indirect-params.
    let iface = make_iface(vec![(
        "many",
        sig(
            true,
            &["fl", "a", "b"],
            vec![fsl, u32_id, u32_id],
            vec![u32_id],
        ),
    )]);
    let bytes = gen_adapter(
        "test:pkg/fl-async@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Variant / option / result params — the dispatch path. Each kind
/// exercises the disc-read + br_table + per-arm cursor rewrite.
#[test]
fn test_adapter_async_variant_option_result_indirect_params_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let u64_id = arena.intern_val(ValueType::U64);
    let opt_u32 = arena.intern_val(ValueType::Option(u32_id));
    let res = arena.intern_val(ValueType::Result {
        ok: Some(u32_id),
        err: Some(u64_id),
    });
    let either = arena.intern_val(ValueType::Variant(vec![
        ("left".into(), Some(u32_id)),
        ("right".into(), Some(u64_id)),
        ("neither".into(), None),
    ]));
    // option(2) + result(2 — 1 disc + 1 i64 joined) + variant(2) +
    // 2×u32 = 8 flat → indirect-params.
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([(
            "many".to_string(),
            sig(
                true,
                &["o", "r", "v", "a", "b"],
                vec![opt_u32, res, either, u32_id, u32_id],
                vec![u32_id],
            ),
        )]),
        type_exports: BTreeMap::from([("either".to_string(), either)]),
    });
    let bytes = gen_adapter(
        "test:pkg/disp-async@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

// ── Tier 1: sync indirect-params (function-boundary pointer-form) ──
//
// Sync funcs whose params overflow `MAX_FLAT_PARAMS` (16) collapse
// to a single `(i32) -> ...` signature: the host writes the lowered
// params into our linear memory and hands us the pointer. Both
// import and export flip together, so the wrapper just passes `local
// 0` straight to the handler (no static buffer, no re-lower). Result
// overflow (`MAX_FLAT_RESULTS = 1` on wasm32) similarly switches to
// a retptr on the import side (caller-allocates) + a retptr return
// on the export side (callee-returns). The doc this exercises is
// `docs/TODO/canonical-abi-gaps.md` (function-boundary pointer-form
// gap, sync half).

/// 17×u32 params: smallest shape that overflows MAX_FLAT_PARAMS.
/// No hooks → exercises the pure validation-gate flip + the
/// pass-through handler call.
#[test]
fn test_adapter_sync_17_u32_params_no_hooks_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let names: Vec<String> = (0..17).map(|i| format!("p{i}")).collect();
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let iface = make_iface(vec![(
        "wide",
        sig(false, &name_refs, vec![u32_id; 17], vec![]),
    )]);
    let bytes = gen_adapter(
        "test:pkg/sync-wide@1.0.0",
        &[],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// 17×u32 + before/after hooks. Tier-1 hooks pass only a call-id,
/// so this still exercises just the pass-through, but with hooks in
/// the wrapper body it confirms the bump-save/restore + hook calls
/// don't interfere with the params-pointer local.
#[test]
fn test_adapter_sync_17_u32_params_with_hooks_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let names: Vec<String> = (0..17).map(|i| format!("p{i}")).collect();
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let iface = make_iface(vec![(
        "wide",
        sig(false, &name_refs, vec![u32_id; 17], vec![]),
    )]);
    let bytes = gen_adapter(
        "test:pkg/sync-wide-hook@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Mixed widths overflowing MAX_FLAT_PARAMS — exercises inter-field
/// alignment padding inside the host-built params record. The
/// adapter never touches the contents (pure pass-through), so this
/// catches any misalignment in the supported-type predicate or in
/// the dispatch-module type section.
#[test]
fn test_adapter_sync_mixed_primitives_indirect_params_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let u64_id = arena.intern_val(ValueType::U64);
    let f32_id = arena.intern_val(ValueType::F32);
    let f64_id = arena.intern_val(ValueType::F64);
    let bool_id = arena.intern_val(ValueType::Bool);
    let char_id = arena.intern_val(ValueType::Char);
    // 9×u64 + 1×u32 + ... = 19 flat slots → indirect_params.
    let params = vec![
        u64_id, u64_id, u64_id, u64_id, u64_id, u64_id, u64_id, u64_id, u64_id, u32_id, f32_id,
        f64_id, bool_id, char_id,
    ];
    let names: Vec<String> = (0..params.len()).map(|i| format!("p{i}")).collect();
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let iface = make_iface(vec![("wide", sig(false, &name_refs, params, vec![]))]);
    let bytes = gen_adapter(
        "test:pkg/sync-mixed-wide@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Single record param with 17 u32 fields — aggregate in indirect-
/// params position. Same flat width as the 17×u32 case, but the
/// canonical-ABI lowering nests inside the record.
#[test]
fn test_adapter_sync_wide_record_param_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let fields: Vec<(String, ValueTypeId)> = (0..17).map(|i| (format!("f{i}"), u32_id)).collect();
    let record = arena.intern_val(ValueType::Record(fields));
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([("wide".to_string(), sig(false, &["r"], vec![record], vec![]))]),
        type_exports: BTreeMap::from([("rec17".to_string(), record)]),
    });
    let bytes = gen_adapter(
        "test:pkg/sync-rec17@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Result `record { a: u64, b: u64, c: u32 }` flattens to 5 i32 slots
/// → `export_sig.retptr = true`. Wrapper signature flips to
/// `() -> i32` (callee-returns retptr); handler is `(i32) -> ()`
/// (caller-allocates). Confirms the result-overflow half already
/// works without changes.
#[test]
fn test_adapter_sync_wide_result_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let u64_id = arena.intern_val(ValueType::U64);
    let record = arena.intern_val(ValueType::Record(vec![
        ("a".into(), u64_id),
        ("b".into(), u64_id),
        ("c".into(), u32_id),
    ]));
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([("wide".to_string(), sig(false, &[], vec![], vec![record]))]),
        type_exports: BTreeMap::from([("rec3".to_string(), record)]),
    });
    let bytes = gen_adapter(
        "test:pkg/sync-result-wide@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// Both overflows: 17×u32 params AND a wide record result. Pins the
/// combined handler shape `(i32, i32) -> ()` (params_ptr, retptr).
#[test]
fn test_adapter_sync_both_overflow_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let u64_id = arena.intern_val(ValueType::U64);
    let result_record = arena.intern_val(ValueType::Record(vec![
        ("a".into(), u64_id),
        ("b".into(), u64_id),
    ]));
    let names: Vec<String> = (0..17).map(|i| format!("p{i}")).collect();
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let iface = InterfaceType::Instance(InstanceInterface {
        functions: BTreeMap::from([(
            "wide".to_string(),
            sig(false, &name_refs, vec![u32_id; 17], vec![result_record]),
        )]),
        type_exports: BTreeMap::from([("rec2".to_string(), result_record)]),
    });
    let bytes = gen_adapter(
        "test:pkg/sync-both-wide@1.0.0",
        &["splicer:tier1/before", "splicer:tier1/after"],
        &iface,
        &arena,
        SplitKind::Consumer,
    );
    validate_component(&bytes);
}

/// 17×u32 params + blocking hook + void result. Tier-1 blocking is
/// only legal on void-returning functions; confirms the
/// `should-block` retptr scratch interleaves cleanly with the
/// params-pointer local.
#[test]
fn test_adapter_sync_indirect_params_with_blocking_hook_validates() {
    let mut arena = TypeArena::default();
    let u32_id = arena.intern_val(ValueType::U32);
    let names: Vec<String> = (0..17).map(|i| format!("p{i}")).collect();
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let iface = make_iface(vec![(
        "wide",
        sig(false, &name_refs, vec![u32_id; 17], vec![]),
    )]);
    let bytes = gen_adapter(
        "test:pkg/sync-wide-blocking@1.0.0",
        &["splicer:tier1/blocking"],
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

/// An error message matching one of these prefixes is an expected
/// bail — the adapter correctly refused a shape outside its current
/// support envelope. Anything else is a real failure.
///
/// Note: `"exceeds 16"` (sync indirect-params overflow) was removed
/// when sync function-boundary pointer-form landed — those shapes
/// now validate. `"flat representation"` covers the remaining async
/// per-param flat-fits-in-16 check inside
/// `require_indirect_params_supported_shape`.
fn fuzz_is_expected_bail(msg: &str) -> bool {
    msg.contains("flat parameter values")
        || msg.contains("flat representation")
        || msg.contains("results; only 0 or 1 results")
        || msg.contains("not yet implemented")
}

#[test]
fn fuzz_structural_shapes() {
    run_structural_fuzz("tier1-fuzz", |bytes| {
        let mut u = Unstructured::new(bytes);
        let mut arena = TypeArena::default();
        let mut need_export: Vec<ValueTypeId> = Vec::new();

        // Randomize is_async AND param count to cover all four
        // canonical-ABI corners: sync flat (≤16), sync indirect-
        // params (>16), async flat (≤4), async asymmetric-indirect
        // (5..16) plus the rare async-symmetric (>16). Param count
        // 0..=8 with up-to-depth-2 shapes typically lands 0..~24
        // flat slots — a healthy mix of all caps.
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
            None,
        );

        match gen {
            Ok(path) => {
                let bytes = std::fs::read(&path).map_err(|e| format!("read: {e}"))?;
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
