//! Each test should be a few lines: build a minimal config (a WIT
//! type, a list of function-param-type names), call the helper,
//! `assert_eq!` against an expected value. New cases are mostly
//! one-liners that delegate to a helper.

use wasm_encoder::{
    CodeSection, EntityType, Function, FunctionSection, ImportSection, MemoryType, Module,
    TypeSection, ValType,
};
use wit_parser::abi::WasmSignature;
use wit_parser::{Function as WitFunction, Resolve, SizeAlign, Type};

use super::super::super::abi::emit::{BlobSlice, RecordLayout};
use super::super::blob::NameInterner;
use super::super::cells::CellLayout;
use super::super::schema::{RECORD_FIELD_TUPLE_IDX, RECORD_FIELD_TUPLE_NAME, RECORD_INFO_FIELDS};
use super::super::{FuncClassified, FuncShape};
use super::plan::{Cell, LiftPlan, NamedListInfo};
use super::*;

// ─── Fixture WIT + Resolve helpers ────────────────────────────

/// Single-interface fixture WIT. New tests pull types/functions
/// from `test:lift/t` via [`type_named`] / [`func_named`].
const TEST_WIT: &str = r#"
    package test:lift@0.0.1;
    interface t {
        enum color { red, green, blue }
        record point { x: u32, y: s32 }
        record nested { p: point, c: color }
        record pair { a: u8, b: u8 }
        f-mixed: func(a: bool, s: string, b: list<u8>, x: s64);
        f-color: func(c: color);
        f-point: func(p: point);
        f-mix-records: func(p: point, n: nested);
    }
"#;

fn test_resolve() -> Resolve {
    let mut r = Resolve::new();
    r.push_str("test.wit", TEST_WIT)
        .expect("test WIT must parse");
    r
}

fn iface_id(resolve: &Resolve) -> wit_parser::InterfaceId {
    resolve
        .interfaces
        .iter()
        .find_map(|(id, _)| {
            let qname = resolve.id_of(id)?;
            let unversioned = qname.split('@').next().unwrap_or(&qname);
            (unversioned == "test:lift/t").then_some(id)
        })
        .expect("test:lift/t interface not found in fixture")
}

fn type_named(resolve: &Resolve, name: &str) -> Type {
    Type::Id(
        resolve.interfaces[iface_id(resolve)]
            .types
            .get(name)
            .copied()
            .unwrap_or_else(|| panic!("type `{name}` not found in fixture")),
    )
}

fn func_named<'a>(resolve: &'a Resolve, name: &str) -> &'a WitFunction {
    resolve.interfaces[iface_id(resolve)]
        .functions
        .get(name)
        .unwrap_or_else(|| panic!("function `{name}` not found in fixture"))
}

// ─── Plan-builder + assertion fixture constructors ────────────

/// Thin alias for [`LiftPlan::for_type`] — keeps the in-test call
/// sites short. Tests that don't compare against a [`Cell::RecordOf`]
/// fixture pass a fresh interner; tests that do thread the same one
/// through both the plan-builder and [`record_of`] so the
/// pre-interned [`BlobSlice`]s match (the interner dedupes).
fn plan_for(ty: &Type, resolve: &Resolve, names: &mut NameInterner) -> LiftPlan {
    LiftPlan::for_type(ty, resolve, names)
}

fn plan_for_named(name: &str, resolve: &Resolve, names: &mut NameInterner) -> LiftPlan {
    plan_for(&type_named(resolve, name), resolve, names)
}

/// `NamedListInfo { type_name, item_names }` shorthand for fixtures.
fn enum_info(type_name: &str, items: &[&str]) -> NamedListInfo {
    NamedListInfo {
        type_name: type_name.into(),
        item_names: items.iter().map(|s| (*s).to_string()).collect(),
    }
}

/// `Cell::RecordOf` shorthand for fixtures. Interns `type_name` and
/// each field name into `names`; pass the same interner that built
/// the actual plan and the dedup keeps the [`BlobSlice`]s aligned
/// regardless of which side ran first.
fn record_of(names: &mut NameInterner, type_name: &str, fields: &[(&str, u32)]) -> Cell {
    let type_name = names.intern(type_name);
    let fields = fields.iter().map(|(n, i)| (names.intern(n), *i)).collect();
    Cell::RecordOf { type_name, fields }
}

// ─── FuncClassified fixtures ──────────────────────────────────

fn dummy_sig() -> WasmSignature {
    WasmSignature {
        params: Vec::new(),
        results: Vec::new(),
        indirect_params: false,
        retptr: false,
    }
}

fn make_param(ty: &Type, resolve: &Resolve, names: &mut NameInterner) -> ParamLift {
    ParamLift {
        name: BlobSlice::EMPTY,
        plan: plan_for(ty, resolve, names),
    }
}

/// Build a [`FuncClassified`] whose params are the WIT-named types
/// in `param_names`. Plans are plan-relative — no cumulative cursor
/// to thread. Other fields are dummies — the side-table builders
/// only read `params` / `result_lift`.
fn func_with_params(
    resolve: &Resolve,
    names: &mut NameInterner,
    param_names: &[&str],
) -> FuncClassified {
    let params = param_names
        .iter()
        .map(|n| make_param(&type_named(resolve, n), resolve, names))
        .collect();
    FuncClassified {
        shape: FuncShape::Sync,
        result_ty: None,
        import_module: String::new(),
        import_field: String::new(),
        export_name: String::new(),
        export_sig: dummy_sig(),
        import_sig: dummy_sig(),
        needs_cabi_post: false,
        fn_name_offset: 0,
        fn_name_len: 0,
        params,
        result_lift: None,
    }
}

/// Synthesize the two `RecordLayout`s `build_record_info_blob`
/// reads. The builder doesn't care that the layouts come from
/// hand-rolled `for_named_fields` rather than the live splicer
/// `record-info` typedef — it only reads field offsets / sizes.
/// `list<tuple<...>>` flattens to (ptr, len), the same canonical-
/// ABI shape as `string`, so we use `Type::String` for the
/// `fields` slot.
fn synth_record_info_layouts(resolve: &Resolve) -> (RecordLayout, RecordLayout) {
    let mut sizes = SizeAlign::default();
    sizes.fill(resolve);
    let entry = RecordLayout::for_named_fields(
        &sizes,
        &[
            ("type-name".into(), Type::String),
            (RECORD_INFO_FIELDS.into(), Type::String),
        ],
    );
    let tuple = RecordLayout::for_named_fields(
        &sizes,
        &[
            (RECORD_FIELD_TUPLE_NAME.into(), Type::String),
            (RECORD_FIELD_TUPLE_IDX.into(), Type::U32),
        ],
    );
    (entry, tuple)
}

// ─── emit_lift_plan validate harness ──────────────────────────

/// Synthesize the live `cell` variant layout from
/// `wit/common/world.wit`. Pinning to the live WIT ensures disc
/// numbering matches production codegen.
fn synth_cell_layout() -> CellLayout {
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let mut resolve = Resolve::new();
    resolve
        .push_str("common.wit", common_wit)
        .expect("wit/common/world.wit must parse");
    let cell_id = resolve
        .interfaces
        .iter()
        .find_map(|(id, _)| {
            let qname = resolve.id_of(id)?;
            let unversioned = qname.split('@').next().unwrap_or(&qname);
            (unversioned == "splicer:common/types").then_some(id)
        })
        .and_then(|id| resolve.interfaces[id].types.get("cell").copied())
        .expect("splicer:common/types must export `cell`");
    let mut sizes = SizeAlign::default();
    sizes.fill(&resolve);
    CellLayout::from_resolve(&sizes, &resolve, cell_id)
}

/// Wasm `ValType` for each flat slot consumed by `plan.cells`, in
/// flat-slot order. RecordOf cells contribute no slots.
fn plan_param_types(plan: &LiftPlan) -> Vec<ValType> {
    let mut out = Vec::new();
    for op in &plan.cells {
        match op {
            Cell::Bool { .. }
            | Cell::IntegerSignExt { .. }
            | Cell::IntegerZeroExt { .. }
            | Cell::EnumCase { .. } => out.push(ValType::I32),
            Cell::Integer64 { .. } => out.push(ValType::I64),
            Cell::FloatingF32 { .. } => out.push(ValType::F32),
            Cell::FloatingF64 { .. } => out.push(ValType::F64),
            Cell::Text { .. } | Cell::Bytes { .. } => {
                out.push(ValType::I32);
                out.push(ValType::I32);
            }
            Cell::RecordOf { .. } => {}
            Cell::Char
            | Cell::ListOf
            | Cell::TupleOf
            | Cell::Option
            | Cell::Result
            | Cell::Flags
            | Cell::Variant
            | Cell::Handle
            | Cell::Future
            | Cell::Stream
            | Cell::ErrorContext => {
                unreachable!("un-wired Cell variant {op:?} should not appear in test plans")
            }
        }
    }
    out
}

/// Side-table indices a single-plan run of `build_record_info_blob`
/// would assign — `Some(i)` for the i'th RecordOf cell in plan
/// order, `None` for non-RecordOf cells.
fn auto_record_info_indices(plan: &LiftPlan) -> Vec<Option<u32>> {
    let mut idx = 0u32;
    plan.cells
        .iter()
        .map(|op| match op {
            Cell::RecordOf { .. } => {
                let i = idx;
                idx += 1;
                Some(i)
            }
            _ => None,
        })
        .collect()
}

/// Round-trip a plan through `emit_lift_plan` and validate the
/// resulting wasm module. Wasm function params come straight from
/// the plan's flat slots; `WrapperLocals` extras (addr/ext64/
/// ext_f64) sit in three locals declared above the params.
fn validate_emit_lift_plan(plan: &LiftPlan) {
    let cell_layout = synth_cell_layout();
    let record_info = auto_record_info_indices(plan);
    let param_types = plan_param_types(plan);
    let n = plan.flat_slot_count;
    let lcl = WrapperLocals {
        addr: n,
        st: 0,
        ws: 0,
        ext64: n + 1,
        ext_f64: n + 2,
        result: None,
        tr_addr: None,
    };

    let mut module = Module::new();
    let mut types = TypeSection::new();
    types.ty().function(param_types.iter().copied(), []);
    module.section(&types);
    let mut imports = ImportSection::new();
    imports.import(
        "env",
        "memory",
        EntityType::Memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        }),
    );
    module.section(&imports);
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    module.section(&funcs);
    let mut code = CodeSection::new();
    let mut f = Function::new([
        (1u32, ValType::I32),
        (1u32, ValType::I64),
        (1u32, ValType::F64),
    ]);
    // Wasm function params occupy locals 0..flat_slot_count, so
    // `local_base = 0` aligns the plan's flat slots with the
    // params declared on the synth wasm fn.
    emit_lift_plan(&mut f, &cell_layout, 0, plan, &record_info, 0, &lcl);
    f.instructions().end();
    code.function(&f);
    module.section(&code);

    wasmparser::Validator::new()
        .validate_all(&module.finish())
        .expect("emit_lift_plan output must validate");
}

// ─── LiftPlanBuilder shape ───────────────────────────────────

#[test]
fn primitives_assign_one_cell_one_slot() {
    let r = Resolve::new();
    let mut names = NameInterner::new();
    let cases: &[(Type, Cell)] = &[
        (Type::Bool, Cell::Bool { flat_slot: 0 }),
        (Type::S32, Cell::IntegerSignExt { flat_slot: 0 }),
        (Type::U32, Cell::IntegerZeroExt { flat_slot: 0 }),
        (Type::S64, Cell::Integer64 { flat_slot: 0 }),
        (Type::F32, Cell::FloatingF32 { flat_slot: 0 }),
        (Type::F64, Cell::FloatingF64 { flat_slot: 0 }),
    ];
    for (ty, expected) in cases {
        let plan = plan_for(ty, &r, &mut names);
        assert_eq!(plan.cells, vec![expected.clone()], "{ty:?}");
        assert_eq!(plan.flat_slot_count, 1, "{ty:?}");
    }
}

#[test]
fn string_takes_two_flat_slots() {
    let mut names = NameInterner::new();
    let plan = plan_for(&Type::String, &Resolve::new(), &mut names);
    assert_eq!(
        plan.cells,
        vec![Cell::Text {
            ptr_slot: 0,
            len_slot: 1
        }]
    );
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn list_u8_classifies_as_bytes_cell() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let bytes_ty = func_named(&r, "f-mixed").params[2].ty;
    let plan = plan_for(&bytes_ty, &r, &mut names);
    assert_eq!(
        plan.cells,
        vec![Cell::Bytes {
            ptr_slot: 0,
            len_slot: 1
        }]
    );
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn enum_carries_named_list_info() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    assert_eq!(
        plan_for_named("color", &r, &mut names).cells,
        vec![Cell::EnumCase {
            flat_slot: 0,
            info: enum_info("color", &["red", "green", "blue"]),
        }],
    );
}

#[test]
fn record_lays_parent_before_children() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("point", &r, &mut names);
    assert_eq!(
        plan.cells,
        vec![
            record_of(&mut names, "point", &[("x", 1), ("y", 2)]),
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
        ],
    );
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn nested_record_walks_depth_first() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("nested", &r, &mut names);
    assert_eq!(
        plan.cells,
        vec![
            record_of(&mut names, "nested", &[("p", 1), ("c", 4)]),
            record_of(&mut names, "point", &[("x", 2), ("y", 3)]),
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            Cell::EnumCase {
                flat_slot: 2,
                info: enum_info("color", &["red", "green", "blue"]),
            },
        ],
    );
    assert_eq!(plan.flat_slot_count, 3);
}

#[test]
fn classify_func_params_yields_plan_relative_slots() {
    // f-mixed(a: bool, s: string, b: list<u8>, x: s64): each
    // param's plan is plan-relative, not threaded with cumulative
    // cursor. b's bytes cell holds slots (0, 1) regardless of its
    // absolute wasm-local position (3, 4) in the wrapper. Pins
    // the local-base-independence invariant.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let params = classify_func_params(&r, func_named(&r, "f-mixed"), &mut names);
    assert_eq!(
        params[2].plan.cells,
        vec![Cell::Bytes {
            ptr_slot: 0,
            len_slot: 1
        }],
    );
    // Same WIT type → same cells whether built standalone or as
    // a non-zero-indexed param.
    let bytes_ty = func_named(&r, "f-mixed").params[2].ty;
    assert_eq!(
        params[2].plan.cells,
        plan_for(&bytes_ty, &r, &mut names).cells,
    );
}

#[test]
fn param_plan_flat_slot_counts_compose_for_emit_local_base() {
    // Classify outputs plan-relative slots; the emit phase chains
    // per-param `flat_slot_count` into the cumulative `local_base`
    // it passes to `emit_lift_plan`. f-mixed(a: bool, s: string,
    // b: list<u8>, x: s64) → cumulative starts 0, 1, 3, 5; total 6.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let params = classify_func_params(&r, func_named(&r, "f-mixed"), &mut names);
    let starts: Vec<u32> = params
        .iter()
        .scan(0u32, |acc, p| {
            let s = *acc;
            *acc += p.plan.flat_slot_count;
            Some(s)
        })
        .collect();
    assert_eq!(starts, vec![0, 1, 3, 5]);
    assert_eq!(params.last().unwrap().plan.flat_slot_count, 1);
}

// ─── Side-table dedup ─────────────────────────────────────────

#[test]
fn enum_strings_dedup_across_funcs() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let funcs = vec![
        func_with_params(&r, &mut names, &["color"]),
        func_with_params(&r, &mut names, &["color"]),
    ];
    let table = register_enum_strings(&funcs, &mut names);
    assert_eq!(table.len(), 1);
    assert_eq!(names.into_bytes(), b"colorredgreenblue");
}

#[test]
fn name_interner_dedupes_record_strings_across_plans() {
    // f-point shares `point` with f-mix-records, and the `nested`
    // record contains a `point` field — the plan-builder interns
    // type-names + field-names directly, and the [`NameInterner`]
    // dedup folds repeats into one copy. Pins the property the old
    // `register_record_strings` test was actually asserting (one
    // string per type-name across the whole interface).
    let r = test_resolve();
    let mut names = NameInterner::new();
    let _ = vec![
        func_with_params(&r, &mut names, &["point"]),
        func_with_params(&r, &mut names, &["point", "nested"]),
    ];
    let bytes = names.into_bytes();
    let count = |needle: &str| {
        let n = needle.as_bytes();
        bytes.windows(n.len()).filter(|w| *w == n).count()
    };
    // Each name appears exactly once in the blob: the plan-builder
    // walks `point` three times (standalone + nested + as a field
    // type) but the interner dedupes it down to one.
    assert_eq!(count("point"), 1);
    assert_eq!(count("nested"), 1);
    assert_eq!(count("x"), 1);
    assert_eq!(count("y"), 1);
}

// ─── Record-info side-table layout ────────────────────────────

#[test]
fn build_record_info_blob_assigns_per_param_ranges_and_cell_idx() {
    // 2 funcs, 3 params total — exactly the audit's request.
    // f-point(p: point):                 1 RecordOf cell
    // f-mix-records(p: point, n: nested): 1 + 2 RecordOf cells
    let r = test_resolve();
    let mut names = NameInterner::new();
    let funcs = vec![
        func_with_params(&r, &mut names, &["point"]),
        func_with_params(&r, &mut names, &["point", "nested"]),
    ];
    let (entry, tuple) = synth_record_info_layouts(&r);
    let blobs = build_record_info_blob(&funcs, &entry, &tuple, 0, 1);

    // Range lengths per (fn, param). New cases drop in here.
    let lens: Vec<Vec<u32>> = blobs
        .per_param_range
        .iter()
        .map(|fns| fns.iter().map(|sr| sr.map_or(0, |s| s.len)).collect())
        .collect();
    assert_eq!(lens, vec![vec![1], vec![1, 2]]);

    // Cell-idx maps reset per range — index counts up only inside
    // one (fn, param), not across them.
    let expected: Vec<Vec<&[Option<u32>]>> = vec![
        vec![&[Some(0), None, None]],
        vec![
            &[Some(0), None, None],
            &[Some(0), Some(1), None, None, None],
        ],
    ];
    for (fn_idx, fn_expected) in expected.iter().enumerate() {
        for (param_idx, param_expected) in fn_expected.iter().enumerate() {
            assert_eq!(
                blobs.per_param_cell_idx.for_param(fn_idx, param_idx),
                *param_expected,
                "fn {fn_idx} param {param_idx}",
            );
        }
    }

    // 4 record entries → 4 relocs into the tuples segment.
    assert_eq!(blobs.entries.relocs.len(), 4);
}

// ─── emit_lift_plan round-trip through validator ──────────────

#[test]
fn emit_lift_plan_validates_every_classify_built_shape() {
    // Every wired Cell variant: classify a fixture WIT type, emit,
    // validate. Adding a new kind = adding a row.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let primitive_plans = [
        plan_for(&Type::Bool, &r, &mut names),
        plan_for(&Type::S32, &r, &mut names),
        plan_for(&Type::U32, &r, &mut names),
        plan_for(&Type::S64, &r, &mut names),
        plan_for(&Type::F32, &r, &mut names),
        plan_for(&Type::F64, &r, &mut names),
        plan_for(&Type::String, &r, &mut names),
        plan_for(&func_named(&r, "f-mixed").params[2].ty, &r, &mut names), // list<u8>
        plan_for_named("color", &r, &mut names),
        plan_for_named("point", &r, &mut names),
        plan_for_named("nested", &r, &mut names),
    ];
    for plan in &primitive_plans {
        validate_emit_lift_plan(plan);
    }
}
