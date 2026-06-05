//! Layout-phase tests: build a `LayoutEnv`, assert against its
//! dispatches / plan / schema.

use anyhow::Result;
use wit_parser::{Function as WitFunction, Resolve};

use super::super::blob::NameInterner;
use super::super::layout::{
    lay_out_static_memory, StaticDataPlan, MAX_CELLS_PER_PARAM, MAX_FLAT_SLOTS_PER_FN,
};
use super::super::schema::{compute_schema, SchemaLayouts};
use super::super::{build_per_func_classified, synthesize_adapter_world_wit, FuncDispatch};

const TARGET_IFACE: &str = "test:layout-fixture/t@0.0.1";
const TARGET_WIT: &str = r#"
    package test:layout-fixture@0.0.1;
    interface t {
        record point { x: u32, y: s32 }
        f-noargs: func();
        f-pair-u32: func(a: u32, b: u32) -> u32;
        f-string: func(s: string);
        f-string-result: func(x: u32) -> string;
        f-record: func(p: point) -> bool;
    }
"#;

/// Fully-laid-out dispatch list paired with its schema + plan.
struct LayoutEnv {
    dispatches: Vec<FuncDispatch>,
    plan: StaticDataPlan,
    schema: SchemaLayouts,
}

impl LayoutEnv {
    /// Find a dispatch by export-name substring (e.g. the WIT fn name).
    fn dispatch(&self, name: &str) -> &FuncDispatch {
        self.dispatches
            .iter()
            .find(|fd| fd.export_name.contains(name))
            .unwrap_or_else(|| panic!("no dispatch matching `{name}`"))
    }
}

fn env() -> LayoutEnv {
    env_with(true, true)
}

fn env_with(has_before: bool, has_after: bool) -> LayoutEnv {
    use crate::contract::{versioned_interface, TIER2_AFTER, TIER2_BEFORE, TIER2_VERSION};
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let mut resolve = Resolve::new();
    resolve.push_str("test.wit", TARGET_WIT).unwrap();
    resolve.push_str("common.wit", common_wit).unwrap();
    resolve.push_str("tier2.wit", tier2_wit).unwrap();
    let mut hook_ifaces: Vec<String> = Vec::new();
    if has_before {
        hook_ifaces.push(versioned_interface(TIER2_BEFORE, TIER2_VERSION));
    }
    if has_after {
        hook_ifaces.push(versioned_interface(TIER2_AFTER, TIER2_VERSION));
    }
    let world_wit = synthesize_adapter_world_wit(
        "test:layout-fixture-adapter",
        "adapter",
        TARGET_IFACE,
        TARGET_IFACE,
        &hook_ifaces,
    );
    let world_pkg = resolve.push_str("world.wit", &world_wit).unwrap();
    let world_id = resolve.select_world(&[world_pkg], Some("adapter")).unwrap();
    let map_aliases = super::super::lift::desugar_map_aliases(&mut resolve);
    let target_iface =
        super::super::test_utils::iface_by_unversioned_qname(&resolve, "test:layout-fixture/t");
    let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
        .functions
        .values()
        .collect();
    let schema = compute_schema(&resolve, world_id, has_before, has_after, false).unwrap();
    let mut names = NameInterner::new();
    let iface_name = names.intern(TARGET_IFACE);
    let classified =
        build_per_func_classified(&resolve, target_iface, &funcs, &mut names, &map_aliases)
            .unwrap();
    let (dispatches, plan) =
        lay_out_static_memory(classified, &funcs, &schema, names, iface_name).unwrap();
    LayoutEnv {
        dispatches,
        plan,
        schema,
    }
}

// ─── Fields-blob placement ────────────────────────────────────

#[test]
fn fields_buf_offsets_per_func_are_contiguous() {
    let env = env();
    let fs = env.schema.field_layout.size;
    assert!(env.dispatches.windows(2).all(|w| {
        w[0].fields_buf_offset + (w[0].params.len() as u32) * fs == w[1].fields_buf_offset
    }));
}

// ─── After-hook wiring ────────────────────────────────────────

#[test]
fn after_setup_absent_when_after_hook_off() {
    assert!(env_with(true, false)
        .dispatches
        .iter()
        .all(|fd| fd.after.is_none()));
}

#[test]
fn after_setup_present_when_after_hook_on() {
    assert!(env_with(true, true)
        .dispatches
        .iter()
        .all(|fd| fd.after.is_some()));
}

// ─── Retptr scratch ───────────────────────────────────────────

#[test]
fn retptr_offset_set_iff_sig_uses_retptr() {
    let env = env();
    for fd in &env.dispatches {
        assert_eq!(
            fd.retptr_offset.is_some(),
            fd.export_sig.retptr || fd.import_sig.retptr,
        );
    }
}

#[test]
fn fixture_covers_both_retptr_polarities() {
    // Guards [`retptr_offset_set_iff_sig_uses_retptr`] from
    // becoming vacuous if the fixture WIT loses one shape.
    let env = env();
    assert!(env.dispatches.iter().any(|fd| fd.retptr_offset.is_some()));
    assert!(env.dispatches.iter().any(|fd| fd.retptr_offset.is_none()));
}

// ─── Post-layout shape ────────────────────────────────────────

#[test]
fn dispatch_param_count_matches_wit_param_count() {
    let env = env();
    let counts: Vec<usize> = env.dispatches.iter().map(|fd| fd.params.len()).collect();
    // f-noargs(0), f-pair-u32(2), f-string(1), f-string-result(1), f-record(1)
    assert_eq!(counts, vec![0, 2, 1, 1, 1]);
}

// ─── Bump-allocator base ──────────────────────────────────────

#[test]
fn bump_start_aligned_to_cell_align() {
    let env = env();
    assert_eq!(env.plan.bump_start % env.schema.cell_layout.align, 0);
}

#[test]
fn bump_start_within_i32_budget() {
    assert!(env().plan.bump_start <= i32::MAX as u32);
}

#[test]
fn data_segments_sit_below_bump_start() {
    let env = env();
    assert!(env
        .plan
        .data_segments
        .iter()
        .all(|(off, bytes)| off + bytes.len() as u32 <= env.plan.bump_start));
}

// ─── Fixture sanity (guards the property tests from running on
// a degenerate WIT) ──────────────────────────────────────────

#[test]
fn fixture_includes_void_and_non_void_funcs() {
    let env = env();
    assert!(env.dispatch("f-noargs").result_lift.is_none());
    assert!(env.dispatch("f-pair-u32").result_lift.is_some());
}

// ─── Layout-budget bails ──────────────────────────────────────

/// Drive the same pipeline as [`env_with`] but for an arbitrary
/// target WIT, returning the `lay_out_static_memory` result so
/// the budget tests can assert on its `Err`. `target_iface` is
/// the unversioned qname (`pkg:ns/iface`); the fixture WIT must
/// declare exactly one matching package + interface.
fn try_lay_out(target_wit: &str, target_iface_qname: &str) -> Result<()> {
    use crate::contract::{versioned_interface, TIER2_AFTER, TIER2_BEFORE, TIER2_VERSION};
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let tier2_wit = include_str!("../../../../wit/tier2/world.wit");
    let mut resolve = Resolve::new();
    resolve.push_str("test.wit", target_wit).unwrap();
    resolve.push_str("common.wit", common_wit).unwrap();
    resolve.push_str("tier2.wit", tier2_wit).unwrap();
    let hook_ifaces = vec![
        versioned_interface(TIER2_BEFORE, TIER2_VERSION),
        versioned_interface(TIER2_AFTER, TIER2_VERSION),
    ];
    let target_versioned = format!("{target_iface_qname}@0.0.1");
    let world_wit = synthesize_adapter_world_wit(
        "test:budget-fixture-adapter",
        "adapter",
        &target_versioned,
        &target_versioned,
        &hook_ifaces,
    );
    let world_pkg = resolve.push_str("world.wit", &world_wit).unwrap();
    let world_id = resolve.select_world(&[world_pkg], Some("adapter")).unwrap();
    let map_aliases = super::super::lift::desugar_map_aliases(&mut resolve);
    let target_iface =
        super::super::test_utils::iface_by_unversioned_qname(&resolve, target_iface_qname);
    let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
        .functions
        .values()
        .collect();
    let schema = compute_schema(&resolve, world_id, true, true, false).unwrap();
    let mut names = NameInterner::new();
    let iface_name = names.intern(&target_versioned);
    let classified =
        build_per_func_classified(&resolve, target_iface, &funcs, &mut names, &map_aliases)?;
    lay_out_static_memory(classified, &funcs, &schema, names, iface_name).map(|_| ())
}

#[test]
fn flat_slot_budget_bails_when_param_flatten_exceeds_cap() {
    // `flat_slot_count` is per-param: a record param flattens to
    // one slot per leaf primitive field. `MAX_FLAT_SLOTS_PER_FN
    // + 1` u32 fields pushes one record param over the cap.
    // (The cell-budget check runs after the flat-slot check, so
    // the flat-slot bail fires first even though this shape also
    // exceeds `MAX_CELLS_PER_PARAM`.)
    let n = MAX_FLAT_SLOTS_PER_FN + 1;
    let fields = (0..n)
        .map(|i| format!("f{i}: u32"))
        .collect::<Vec<_>>()
        .join(", ");
    let wit = format!(
        "package test:budget-flat@0.0.1;\n\
         interface t {{\n\
             record big {{ {fields} }}\n\
             bloat: func(b: big);\n\
         }}\n"
    );
    let err = try_lay_out(&wit, "test:budget-flat/t")
        .expect_err("flat-slot budget should bail at MAX_FLAT_SLOTS_PER_FN + 1");
    let msg = err.to_string();
    assert!(
        msg.contains("flat-slot count") && msg.contains(&MAX_FLAT_SLOTS_PER_FN.to_string()),
        "bail should name the budget, got: {msg}"
    );
}

#[test]
fn cell_budget_bails_when_record_param_exceeds_cap() {
    // Each leaf field contributes one cell, plus one `RecordOf`
    // for the parent. `MAX_CELLS_PER_PARAM` leaf u32 fields gives
    // `MAX_CELLS_PER_PARAM + 1` cells — one over.
    let n = MAX_CELLS_PER_PARAM;
    let fields = (0..n)
        .map(|i| format!("f{i}: u32"))
        .collect::<Vec<_>>()
        .join(", ");
    let wit = format!(
        "package test:budget-cells@0.0.1;\n\
         interface t {{\n\
             record big {{ {fields} }}\n\
             bloat: func(b: big);\n\
         }}\n"
    );
    let err = try_lay_out(&wit, "test:budget-cells/t")
        .expect_err("cell budget should bail at MAX_CELLS_PER_PARAM + 1");
    let msg = err.to_string();
    assert!(
        msg.contains("cell count") && msg.contains(&MAX_CELLS_PER_PARAM.to_string()),
        "bail should name the budget, got: {msg}"
    );
}
