//! Each test should be a few lines: build a minimal config (a WIT
//! type, a list of function-param-type names), call the helper,
//! `assert_eq!` against an expected value. New cases are mostly
//! one-liners that delegate to a helper.

use wasm_encoder::{
    CodeSection, EntityType, Function, FunctionSection, ImportSection, MemoryType, Module,
    TypeSection, ValType,
};
use wit_parser::abi::{WasmSignature, WasmType};
use wit_parser::{Function as WitFunction, Resolve, SizeAlign, Type};

use super::super::super::abi::emit::{BlobSlice, RecordLayout, MAX_UTF8_LEN, STRING_FLAT_BYTES};
use super::super::blob::NameInterner;
use super::super::cells::CellLayout;
use super::super::schema::{RECORD_FIELD_TUPLE_IDX, RECORD_FIELD_TUPLE_NAME, RECORD_INFO_FIELDS};
use super::super::{FuncClassified, FuncShape};
use super::plan::{Cell, HandleKind, LiftPlan, NamedListInfo};
use super::sidetable::flags_info::FlagsRuntimeFill;
use super::sidetable::handle_info::HandleRuntimeFill;
use super::sidetable::variant_info::VariantRuntimeFill;
use super::sidetable::CellSideData;
use super::*;

// ─── Fixture WIT + Resolve helpers ────────────────────────────

/// Single-interface fixture WIT. New tests pull types/functions
/// from `test:lift/t` via [`type_named`] / [`func_named`].
const TEST_WIT: &str = r#"
    package test:lift@0.0.1;
    interface t {
        enum color { red, green, blue }
        flags fperms { read, write, exec }
        variant shape { circle, sq(u32), tri(u32) }
        record point { x: u32, y: s32 }
        record nested { p: point, c: color }
        record pair { a: u8, b: u8 }
        record point-and-tuple { p: point, t: tuple<u8, s32> }
        record perms-pair { primary: fperms, secondary: fperms }
        record shape-pair { lhs: shape, rhs: shape }
        f-mixed: func(a: bool, s: string, b: list<u8>, x: s64);
        f-color: func(c: color);
        f-flags: func(p: fperms);
        f-point: func(p: point);
        f-mix-records: func(p: point, n: nested);
        f-tuple: func(t: tuple<u8, s32>);
        f-tuple-of-tuple: func(t: tuple<u8, tuple<s32, s32>>);
        f-record-with-tuple: func(rt: point-and-tuple);
        f-record-with-flags: func(rwf: perms-pair);
        f-perms-result: func() -> perms-pair;
        f-variant-shape: func(s: shape);
        f-record-with-variant: func(rwv: shape-pair);
        f-option-u32: func(o: option<u32>);
        f-option-string: func(o: option<string>);
        f-option-option: func(o: option<option<u32>>);
        record point-and-option { p: point, o: option<u32> }
        f-record-with-option: func(rwo: point-and-option);
        f-result-u32-string: func(r: result<u32, string>);
        f-result-unit-err: func(r: result<_, string>);
        f-result-ok-unit: func(r: result<u32>);
        f-result-both-unit: func(r: result);
        // Joined-flat widening: ok=u32 → [I32], err=u64 → [I64];
        // joined slot 1 = I64. Ok arm's Cell::IntegerZeroExt reads
        // slot 1 as I32 → emit bitcast I64→I32.
        f-result-u32-u64: func(r: result<u32, u64>);
        // Three-arm widening: a(u32) → [I32], b(u64) → [I64],
        // c(f64) → [F64]; joined slot 1 = I64. a + c arms widen,
        // b matches.
        variant tri-arm { a(u32), b(u64), c(f64) }
        f-variant-tri-arm: func(v: tri-arm);
        resource my-res;
        record handle-pair { primary: own<my-res>, secondary: borrow<my-res> }
        f-handle-own: func(h: own<my-res>);
        f-handle-borrow: func(h: borrow<my-res>);
        f-record-with-handle: func(hp: handle-pair);
        f-stream-u32: async func(s: stream<u32>);
        f-future-string: async func(fut: future<string>);
        f-stream-of-res: async func(s: stream<my-res>);
        record stream-pair { events: stream<u32>, ack: future<u32> }
        f-record-with-stream: async func(rs: stream-pair);
        f-error-context: func(e: error-context);
        f-result-with-err-ctx: func(r: result<s32, error-context>);
        f-list-u32: func(xs: list<u32>);
        f-list-string: func(xs: list<string>);
        f-result-list-u32: func() -> list<u32>;
        f-list-of-list: func(xs: list<list<u32>>);
        record list-pair { items: list<string>, scores: list<u32> }
        f-list-of-record: func(xs: list<point>);
        f-result-list-list: func(r: result<list<u32>, list<u32>>);
        variant list-or-int { with-list(list<u32>), plain(u32) }
        f-variant-list-arm: func(v: list-or-int);
        f-option-list: func(o: option<list<u32>>);
    }
"#;

fn test_resolve() -> Resolve {
    let mut r = Resolve::new();
    r.push_str("test.wit", TEST_WIT)
        .expect("test WIT must parse");
    r
}

fn iface_id(resolve: &Resolve) -> wit_parser::InterfaceId {
    super::super::test_utils::iface_by_unversioned_qname(resolve, "test:lift/t")
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
/// Unwraps the [`Result`] so positive tests don't have to repeat the
/// `.expect(...)`. Negative cases call `LiftPlan::for_type` directly.
fn plan_for(ty: &Type, resolve: &Resolve, names: &mut NameInterner) -> LiftPlan {
    LiftPlan::for_type(ty, resolve, names).expect("test fixture must classify")
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
        borrow_drops: Vec::new(),
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
    let common_id =
        super::super::test_utils::iface_by_unversioned_qname(&resolve, "splicer:common/types");
    let cell_id = resolve.interfaces[common_id]
        .types
        .get("cell")
        .copied()
        .expect("splicer:common/types must export `cell`");
    let mut sizes = SizeAlign::default();
    sizes.fill(&resolve);
    CellLayout::from_resolve(&sizes, &resolve, cell_id)
}

/// Wasm `ValType` per flat slot — sourced from the canonical-ABI
/// `flat_types(plan.source_ty)`, the same computation canon-lower runs
/// to produce the wrapper's flat-param signature. Joined-flat widening
/// (from `result` / `variant` arms) falls out naturally — `flat_types`
/// returns the joined types for those slots. Pinning the test's
/// declared params to this single source means a drift between
/// emit_cell_op and the cell's expected wasm type surfaces as a
/// validation error rather than two-wrongs-cancel.
fn plan_param_types(plan: &LiftPlan, resolve: &Resolve) -> Vec<ValType> {
    use super::super::super::abi::emit::wasm_type_to_val;
    use super::super::super::abi::flat_types;
    flat_types(resolve, &plan.source_ty, None)
        .expect("plan source_ty must flatten within MAX_FLAT_PARAMS")
        .into_iter()
        .map(wasm_type_to_val)
        .collect()
}

/// Synthesize the [`CellSideData`] sequence a real layout phase
/// would attach to `plan.cells` — record/tuple/flags entries get
/// stub addresses (just need to be in-memory for wasm validation);
/// runtime value-correctness is the canned-shape harness's job.
fn auto_cell_side_data(plan: &LiftPlan) -> Vec<CellSideData> {
    /// Bytes per child-index in `tuple-indices` (canonical-ABI u32).
    const U32_BYTES: u32 = 4;
    /// Mid-page cursor for the synth flags-scratch buffer — anywhere
    /// in linear memory works; sitting away from page 0 keeps stub
    /// addresses clearly distinct from null.
    const FLAGS_SCRATCH_BASE: u32 = 0x1000;
    /// Stride between stub flag-name `(off, len)` slices.
    const STUB_FLAG_NAME_STRIDE: u32 = 16;
    /// Stub flag-name length (any non-zero u32 works).
    const STUB_FLAG_NAME_LEN: u32 = 4;

    let mut record_idx: u32 = 0;
    let mut tuple_cursor: u32 = 0;
    let mut flags_cursor: u32 = FLAGS_SCRATCH_BASE;
    let mut flags_idx: u32 = 0;
    let mut variant_idx: u32 = 0;
    let mut char_cursor: u32 = 0x3000;
    let mut handle_idx: u32 = 0;
    let mut handle_id_cursor: u32 = 0x4000;
    plan.cells
        .iter()
        .map(|op| match op {
            Cell::RecordOf { .. } => {
                let idx = record_idx;
                record_idx += 1;
                CellSideData::Record { idx }
            }
            Cell::TupleOf { children } => {
                let off = tuple_cursor;
                let len = children.len() as u32;
                tuple_cursor += len * U32_BYTES;
                CellSideData::Tuple {
                    slice: BlobSlice { off, len },
                }
            }
            Cell::Flags { info, .. } => {
                let scratch_addr = flags_cursor;
                flags_cursor += info.item_names.len() as u32 * STRING_FLAT_BYTES;
                let set_flags_len_addr = flags_cursor;
                flags_cursor += U32_BYTES;
                let fill = FlagsRuntimeFill {
                    side_table_idx: flags_idx,
                    entry_seg_off: 0, // not exercised by the validator fixture
                    set_flags_len_addr: Some(set_flags_len_addr as i32),
                    scratch_addr: scratch_addr as i32,
                    flag_names: info
                        .item_names
                        .iter()
                        .enumerate()
                        .map(|(i, _)| BlobSlice {
                            off: i as u32 * STUB_FLAG_NAME_STRIDE,
                            len: STUB_FLAG_NAME_LEN,
                        })
                        .collect(),
                };
                flags_idx += 1;
                CellSideData::Flags(Box::new(fill))
            }
            Cell::Variant {
                info,
                per_case_payload,
                ..
            } => {
                /// Synth offsets for variant entry-slot stubs — anywhere
                /// in linear memory works.
                const VARIANT_BASE: u32 = 0x2000;
                let case_name_addr = VARIANT_BASE + variant_idx * 32;
                let payload_disc_addr = case_name_addr + 16;
                let payload_value_addr = case_name_addr + 20;
                let fill = VariantRuntimeFill {
                    side_table_idx: variant_idx,
                    entry_seg_off: 0,
                    case_name_addr: Some(case_name_addr as i32),
                    payload_disc_addr: Some(payload_disc_addr as i32),
                    payload_value_addr: Some(payload_value_addr as i32),
                    case_names: info
                        .item_names
                        .iter()
                        .enumerate()
                        .map(|(i, _)| BlobSlice {
                            off: i as u32 * STUB_FLAG_NAME_STRIDE,
                            len: STUB_FLAG_NAME_LEN,
                        })
                        .collect(),
                    per_case_payload: per_case_payload.clone(),
                };
                variant_idx += 1;
                CellSideData::Variant(Box::new(fill))
            }
            Cell::Char { .. } => {
                let scratch_addr = char_cursor;
                char_cursor += MAX_UTF8_LEN;
                CellSideData::Char {
                    scratch_addr: scratch_addr as i32,
                }
            }
            Cell::Handle { .. } => {
                /// Bytes per `handle-info.id` (u64, 8-aligned).
                const U64_BYTES: u32 = 8;
                let id_addr = handle_id_cursor;
                handle_id_cursor += U64_BYTES;
                let fill = HandleRuntimeFill {
                    side_table_idx: handle_idx,
                    entry_seg_off: 0, // not exercised by the validator fixture
                    id_addr: Some(id_addr as i32),
                };
                handle_idx += 1;
                CellSideData::Handle(Box::new(fill))
            }
            // No side-table contribution — flat-only or control-flow.
            Cell::Bool { .. }
            | Cell::IntegerSignExt { .. }
            | Cell::IntegerZeroExt { .. }
            | Cell::Integer64 { .. }
            | Cell::FloatingF32 { .. }
            | Cell::FloatingF64 { .. }
            | Cell::Text { .. }
            | Cell::Bytes { .. }
            | Cell::EnumCase { .. }
            | Cell::Option { .. }
            | Cell::Result { .. }
            | Cell::ListOf { .. } => CellSideData::None,
        })
        .collect()
}

/// Round-trip a plan through `emit_lift_plan` and validate the
/// resulting wasm module. Allocates per-list emit locals via the
/// production helper and imports a stub `cabi_realloc` so the
/// validator can resolve the calls the list-of arm emits. The
/// function is never invoked, only validated.
fn validate_emit_lift_plan(plan: &LiftPlan, resolve: &Resolve) {
    use super::super::super::indices::LocalsBuilder;
    use crate::adapter::indices::FrozenLocals;

    let mut sizes = SizeAlign::default();
    sizes.fill(resolve);
    let cell_layout = synth_cell_layout();
    let cell_side = auto_cell_side_data(plan);
    let param_types = plan_param_types(plan, resolve);
    let n = plan.flat_slot_count;

    // Match alloc_wrapper_locals' allocation order so the indices
    // line up against the frozen locals list.
    let mut builder = LocalsBuilder::new(n);
    let addr = builder.alloc_local(ValType::I32);
    let st = builder.alloc_local(ValType::I32);
    let ws = builder.alloc_local(ValType::I32);
    let ext64 = builder.alloc_local(ValType::I64);
    let ext_f64 = builder.alloc_local(ValType::F64);
    let widen_i32_a = builder.alloc_local(ValType::I32);
    let widen_i32_b = builder.alloc_local(ValType::I32);
    let flags_addr = builder.alloc_local(ValType::I32);
    let flags_count = builder.alloc_local(ValType::I32);
    let char_len = builder.alloc_local(ValType::I32);
    let id_local = builder.alloc_local(ValType::I64);
    let saved_bump = builder.alloc_local(ValType::I32);
    let cells_base = builder.alloc_local(ValType::I32);
    let next_cell_idx = builder.alloc_local(ValType::I32);
    let list_locals = super::emit::alloc_list_emit_locals(plan, resolve, &sizes, &mut builder);
    let FrozenLocals { locals } = builder.freeze();

    let lcl = WrapperLocals {
        addr,
        st,
        ws,
        ext64,
        ext_f64,
        widen_i32_a,
        widen_i32_b,
        flags_addr,
        flags_count,
        char_len,
        cells_base,
        next_cell_idx,
        result: None,
        tr_addr: None,
        id_local,
        task_return_loads: None,
        params_lower_seq: None,
        saved_bump,
        param_list_locals: Vec::new(),
    };

    // Stub `cabi_realloc` import — signature `(i32, i32, i32, i32)
    // -> i32`. Index 0 in the func index space (no other imports
    // ahead of it).
    let mut module = Module::new();
    let mut types = TypeSection::new();
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        [ValType::I32],
    );
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
    imports.import("env", "cabi_realloc", EntityType::Function(0));
    module.section(&imports);

    let mut funcs = FunctionSection::new();
    funcs.function(1);
    module.section(&funcs);

    let mut code = CodeSection::new();
    let mut f = Function::new_with_locals_types(locals);
    f.instructions().i32_const(0);
    f.instructions().local_set(lcl.cells_base);
    let lift_ctx = super::emit::LiftEmitCtx {
        cell_layout: &cell_layout,
        cabi_realloc_idx: 0,
    };
    emit_lift_plan(
        &mut f,
        &lift_ctx,
        plan,
        super::emit::CellSideRefs {
            cell_side: &cell_side,
        },
        0,
        &lcl,
        &list_locals,
    );
    f.instructions().end();
    code.function(&f);
    module.section(&code);

    wasmparser::Validator::new()
        .validate_all(&module.finish())
        .expect("emit_lift_plan output must validate (list path)");
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
fn char_assigns_one_cell_one_slot() {
    let r = Resolve::new();
    let mut names = NameInterner::new();
    let plan = plan_for(&Type::Char, &r, &mut names);
    assert_eq!(plan.cells, vec![Cell::Char { flat_slot: 0 }]);
    assert_eq!(plan.flat_slot_count, 1);
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
fn flags_assigns_one_cell_one_slot() {
    // `fperms` has 3 flags; canonical-ABI lowers them all into a
    // single i32 (caps at 32 bits). Plan is one Flags cell at
    // flat_slot 0 carrying the full NamedListInfo.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("fperms", &r, &mut names);
    assert_eq!(
        plan.cells,
        vec![Cell::Flags {
            flat_slot: 0,
            info: enum_info("fperms", &["read", "write", "exec"]),
        }],
    );
    assert_eq!(plan.flat_slot_count, 1);
}

#[test]
fn variant_lays_disc_first_then_arms_share_slots() {
    // shape { circle, sq(u32), tri(u32) }: 3 cases, 2 with payload.
    // Joined flat = [i32 disc, i32 (joined u32/u32)]. Cell order:
    //   sq's u32   → cell 0 (slot 1)
    //   tri's u32  → cell 1 (slot 1, shares with sq's slot)
    //   Variant    → cell 2 (disc=0, per_case_payload=[None, Some(0), Some(1)])
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("shape", &r, &mut names);
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::Variant {
                disc_slot: 0,
                per_case_payload: vec![None, Some(0), Some(1)],
                info: enum_info("shape", &["circle", "sq", "tri"]),
            },
        ],
    );
    assert_eq!(plan.root(), 2);
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn record_with_variant_field_recurses_into_variant() {
    // shape-pair { lhs: shape, rhs: shape }: each variant claims one
    // disc slot + one shared-payload slot; arms share inside each
    // variant but lhs and rhs occupy independent slots.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("shape-pair", &r, &mut names);
    let shape_info = enum_info("shape", &["circle", "sq", "tri"]);
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::Variant {
                disc_slot: 0,
                per_case_payload: vec![None, Some(0), Some(1)],
                info: shape_info.clone(),
            },
            Cell::IntegerZeroExt { flat_slot: 3 },
            Cell::IntegerZeroExt { flat_slot: 3 },
            Cell::Variant {
                disc_slot: 2,
                per_case_payload: vec![None, Some(3), Some(4)],
                info: shape_info,
            },
            record_of(&mut names, "shape-pair", &[("lhs", 2), ("rhs", 5)]),
        ],
    );
    assert_eq!(plan.root(), 6);
    assert_eq!(plan.flat_slot_count, 4);
}

#[test]
fn handle_assigns_one_cell_one_slot() {
    // own<my-res>: a single i32 (the canonical-ABI handle) → one
    // Cell::Handle with the resource's pre-interned type-name. The
    // interner dedupes, so re-interning "my-res" off the same
    // `names` returns the BlobSlice already on the cell.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(&func_named(&r, "f-handle-own").params[0].ty, &r, &mut names);
    let res_name = names.intern("my-res");
    assert_eq!(
        plan.cells,
        vec![Cell::Handle {
            flat_slot: 0,
            type_name: res_name,
            kind: HandleKind::Resource,
        }],
    );
    assert_eq!(plan.root(), 0);
    assert_eq!(plan.flat_slot_count, 1);
}

#[test]
fn borrow_handle_takes_same_shape_as_own() {
    // borrow<R> and own<R> both flatten to a single i32 (the canonical-
    // ABI handle); the lift codegen treats them identically. The
    // ownership distinction is the adapter's job, not the lift's.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let own_plan = plan_for(&func_named(&r, "f-handle-own").params[0].ty, &r, &mut names);
    let borrow_plan = plan_for(
        &func_named(&r, "f-handle-borrow").params[0].ty,
        &r,
        &mut names,
    );
    assert_eq!(own_plan.cells, borrow_plan.cells);
    assert_eq!(own_plan.flat_slot_count, borrow_plan.flat_slot_count);
}

#[test]
fn record_with_handle_field_recurses_into_handle() {
    // handle-pair { primary: own<my-res>, secondary: borrow<my-res> }
    //   primary    → cell 0 (Handle slot 0)
    //   secondary  → cell 1 (Handle slot 1)
    //   hp         → cell 2 (RecordOf primary=0, secondary=1)
    // Both fields point at the same `my-res` resource, so the
    // pre-interned type-name BlobSlice is shared (interner dedupes).
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("handle-pair", &r, &mut names);
    let res_name = names.intern("my-res");
    assert_eq!(
        plan.cells,
        vec![
            Cell::Handle {
                flat_slot: 0,
                type_name: res_name,
                kind: HandleKind::Resource,
            },
            Cell::Handle {
                flat_slot: 1,
                type_name: res_name,
                kind: HandleKind::Resource,
            },
            record_of(
                &mut names,
                "handle-pair",
                &[("primary", 0), ("secondary", 1)],
            ),
        ],
    );
    assert_eq!(plan.root(), 2);
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn stream_handle_assigns_one_cell_one_slot() {
    // stream<u32>: single i32 (canonical-ABI handle); type-name
    // empty (anonymous element type for primitives).
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(&func_named(&r, "f-stream-u32").params[0].ty, &r, &mut names);
    let empty = names.intern("");
    assert_eq!(
        plan.cells,
        vec![Cell::Handle {
            flat_slot: 0,
            type_name: empty,
            kind: HandleKind::Stream,
        }],
    );
    assert_eq!(plan.flat_slot_count, 1);
}

#[test]
fn future_handle_takes_same_shape_as_stream() {
    // future<T> and stream<T> share `Cell::Handle` — only the
    // `kind` differs.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-future-string").params[0].ty,
        &r,
        &mut names,
    );
    let empty = names.intern("");
    assert_eq!(
        plan.cells,
        vec![Cell::Handle {
            flat_slot: 0,
            type_name: empty,
            kind: HandleKind::Future,
        }],
    );
    assert_eq!(plan.flat_slot_count, 1);
}

#[test]
fn stream_with_named_element_carries_element_type_name() {
    // stream<my-res>: type-name = "my-res" (named element type).
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-stream-of-res").params[0].ty,
        &r,
        &mut names,
    );
    let res_name = names.intern("my-res");
    assert_eq!(
        plan.cells,
        vec![Cell::Handle {
            flat_slot: 0,
            type_name: res_name,
            kind: HandleKind::Stream,
        }],
    );
}

#[test]
fn record_with_stream_and_future_fields_recurses_into_handle() {
    // stream-pair { events: stream<u32>, ack: future<u32> }
    //   events → cell 0 (Handle slot 0, kind=Stream)
    //   ack    → cell 1 (Handle slot 1, kind=Future)
    //   sp     → cell 2 (RecordOf events=0, ack=1)
    // Pins that the same recursion machinery as resource handles
    // works for stream/future fields, with `kind` plumbed through.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("stream-pair", &r, &mut names);
    let empty = names.intern("");
    assert_eq!(
        plan.cells,
        vec![
            Cell::Handle {
                flat_slot: 0,
                type_name: empty,
                kind: HandleKind::Stream,
            },
            Cell::Handle {
                flat_slot: 1,
                type_name: empty,
                kind: HandleKind::Future,
            },
            record_of(&mut names, "stream-pair", &[("events", 0), ("ack", 1)],),
        ],
    );
    assert_eq!(plan.root(), 2);
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn error_context_assigns_one_cell_one_slot() {
    // `error-context`: a single i32 (canonical-ABI handle); type-name
    // empty (no nested type to surface — the cell-disc names the kind).
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-error-context").params[0].ty,
        &r,
        &mut names,
    );
    let empty = names.intern("");
    assert_eq!(
        plan.cells,
        vec![Cell::Handle {
            flat_slot: 0,
            type_name: empty,
            kind: HandleKind::ErrorContext,
        }],
    );
    assert_eq!(plan.root(), 0);
    assert_eq!(plan.flat_slot_count, 1);
}

#[test]
fn result_with_error_context_err_arm_recurses_into_handle() {
    // result<s32, error-context>: the typical error-context usage
    // shape. flat layout = [i32 disc, i32 (joined s32/error-context)]
    //   ok payload → cell 0 (IntegerSignExt slot 1, s32)
    //   err payload → cell 1 (Handle slot 1, kind=ErrorContext)
    //   parent      → cell 2 (Result disc=0, ok=Some(0), err=Some(1))
    // The err-arm Handle cell shares flat slot 1 with the ok-arm
    // payload cell — joined result layout.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-result-with-err-ctx").params[0].ty,
        &r,
        &mut names,
    );
    let empty = names.intern("");
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerSignExt { flat_slot: 1 },
            Cell::Handle {
                flat_slot: 1,
                type_name: empty,
                kind: HandleKind::ErrorContext,
            },
            Cell::Result {
                disc_slot: 0,
                ok_idx: Some(0),
                err_idx: Some(1),
            },
        ],
    );
    assert_eq!(plan.root(), 2);
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn list_of_primitive_carries_element_plan() {
    // list<u32>: parent (ptr, len) → 2 i32 slots, 1 cell. Element
    // plan has its own flat slots — independent from the parent.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(&func_named(&r, "f-list-u32").params[0].ty, &r, &mut names);
    assert_eq!(plan.cells.len(), 1);
    let Cell::ListOf {
        ptr_slot,
        len_slot,
        element_plan,
        ..
    } = &plan.cells[0]
    else {
        panic!("expected Cell::ListOf, got {:?}", plan.cells[0]);
    };
    assert_eq!(*ptr_slot, 0);
    assert_eq!(*len_slot, 1);
    assert_eq!(plan.flat_slot_count, 2);
    assert_eq!(plan.root(), 0);
    // Element plan: u32 → one IntegerZeroExt cell, one flat slot.
    assert_eq!(
        element_plan.cells,
        vec![Cell::IntegerZeroExt { flat_slot: 0 }],
    );
    assert_eq!(element_plan.flat_slot_count, 1);
    assert_eq!(element_plan.root(), 0);
}

#[test]
fn list_of_string_element_plan_uses_two_local_slots() {
    // list<string>: element string is (ptr, len) → 2 flat slots,
    // local to the element plan. Parent still 2 slots + 1 cell.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-list-string").params[0].ty,
        &r,
        &mut names,
    );
    assert_eq!(plan.flat_slot_count, 2);
    let Cell::ListOf { element_plan, .. } = &plan.cells[0] else {
        panic!("expected Cell::ListOf");
    };
    assert_eq!(
        element_plan.cells,
        vec![Cell::Text {
            ptr_slot: 0,
            len_slot: 1,
        }],
    );
    assert_eq!(element_plan.flat_slot_count, 2);
}

#[test]
fn nested_list_bails_at_plan_build() {
    // list<list<u32>>: nested lists aren't a supported element shape;
    // plan-build surfaces the inner failure to the outer caller.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let err = LiftPlan::for_type(
        &func_named(&r, "f-list-of-list").params[0].ty,
        &r,
        &mut names,
    )
    .expect_err("nested list must bail at plan build");
    let msg = err.to_string();
    assert!(msg.contains("`list<T>` element type"));
    assert!(msg.contains("github.com/ejrgilbert/splicer/issues"));
}

#[test]
fn list_of_compound_element_bails_at_plan_build() {
    // list<point>: record element types aren't a supported list element
    // shape today (compound element types defer to a later landing).
    let r = test_resolve();
    let mut names = NameInterner::new();
    let err = LiftPlan::for_type(
        &func_named(&r, "f-list-of-record").params[0].ty,
        &r,
        &mut names,
    )
    .expect_err("list<record> must bail at plan build");
    let msg = err.to_string();
    assert!(msg.contains("`list<T>` element type"));
    assert!(msg.contains("github.com/ejrgilbert/splicer/issues"));
}

#[test]
fn list_inside_result_arm_bails_at_plan_build() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let err = LiftPlan::for_type(
        &func_named(&r, "f-result-list-list").params[0].ty,
        &r,
        &mut names,
    )
    .expect_err("list inside a result arm must bail at plan build");
    let msg = err.to_string();
    assert!(msg.contains("`list<T>` inside a `result` / `variant` arm"));
    assert!(msg.contains("github.com/ejrgilbert/splicer/issues"));
}

#[test]
fn list_inside_variant_arm_bails_at_plan_build() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let err = LiftPlan::for_type(
        &func_named(&r, "f-variant-list-arm").params[0].ty,
        &r,
        &mut names,
    )
    .expect_err("list inside a variant arm must bail at plan build");
    let msg = err.to_string();
    assert!(msg.contains("`list<T>` inside a `result` / `variant` arm"));
    assert!(msg.contains("github.com/ejrgilbert/splicer/issues"));
}

#[test]
fn list_inside_option_is_allowed() {
    // Option's payload slots are dedicated, not joined — guard must
    // not fire here.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-option-list").params[0].ty,
        &r,
        &mut names,
    );
    assert_eq!(plan.cells.len(), 2);
    assert!(matches!(plan.cells[0], Cell::ListOf { .. }));
    assert!(matches!(plan.cells[1], Cell::Option { .. }));
}

#[test]
fn record_with_list_field_recurses_into_list() {
    // list-pair { items: list<string>, scores: list<u32> }
    //   items   → cell 0 (ListOf, slots 0..1)
    //   scores  → cell 1 (ListOf, slots 2..3)
    //   parent  → cell 2 (RecordOf items=0, scores=1)
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("list-pair", &r, &mut names);
    assert_eq!(plan.cells.len(), 3);
    assert!(matches!(plan.cells[0], Cell::ListOf { .. }));
    assert!(matches!(plan.cells[1], Cell::ListOf { .. }));
    assert_eq!(
        plan.cells[2],
        record_of(&mut names, "list-pair", &[("items", 0), ("scores", 1)],),
    );
    assert_eq!(plan.flat_slot_count, 4);
    assert_eq!(plan.root(), 2);
}

#[test]
fn record_with_flags_field_recurses_into_flags() {
    // perms-pair { primary: fperms, secondary: fperms }
    //   primary    → cell 0 (Flags slot 0)
    //   secondary  → cell 1 (Flags slot 1)
    //   pp         → cell 2 (RecordOf primary=0, secondary=1)
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("perms-pair", &r, &mut names);
    assert_eq!(
        plan.cells,
        vec![
            Cell::Flags {
                flat_slot: 0,
                info: enum_info("fperms", &["read", "write", "exec"]),
            },
            Cell::Flags {
                flat_slot: 1,
                info: enum_info("fperms", &["read", "write", "exec"]),
            },
            record_of(
                &mut names,
                "perms-pair",
                &[("primary", 0), ("secondary", 1)],
            ),
        ],
    );
    assert_eq!(plan.root(), 2);
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn record_lays_children_before_parent() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("point", &r, &mut names);
    // Children-first: u32 + s32 land at indices 0 and 1, the parent
    // RecordOf is appended last and references them. Plan.root
    // points at the parent's cell index (2), not at cells[0].
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            record_of(&mut names, "point", &[("x", 0), ("y", 1)]),
        ],
    );
    assert_eq!(plan.root(), 2);
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn nested_record_walks_depth_first() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("nested", &r, &mut names);
    // Depth-first, children-before-parent: the inner `point`'s
    // primitive children land at 0/1, then `point`'s parent at 2,
    // then the `color` enum at 3, then the outer `nested` parent at
    // 4. plan.root() is the outer parent.
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            record_of(&mut names, "point", &[("x", 0), ("y", 1)]),
            Cell::EnumCase {
                flat_slot: 2,
                info: enum_info("color", &["red", "green", "blue"]),
            },
            record_of(&mut names, "nested", &[("p", 2), ("c", 3)]),
        ],
    );
    assert_eq!(plan.root(), 4);
    assert_eq!(plan.flat_slot_count, 3);
}

#[test]
fn tuple_lays_children_before_parent() {
    // tuple<u8, s32>: u8 → cell 0, s32 → cell 1, TupleOf parent → cell 2.
    // Plan-relative flat slots: u8 slot 0, s32 slot 1, parent consumes none.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let f = func_named(&r, "f-tuple");
    let plan = plan_for(&f.params[0].ty, &r, &mut names);
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            Cell::TupleOf {
                children: vec![0, 1]
            },
        ],
    );
    assert_eq!(plan.root(), 2);
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn nested_tuple_walks_depth_first() {
    // tuple<u8, tuple<s32, s32>>:
    //   u8     → cell 0 (slot 0)
    //   s32    → cell 1 (slot 1)
    //   s32    → cell 2 (slot 2)
    //   inner  → cell 3 (children=[1, 2])
    //   outer  → cell 4 (children=[0, 3])
    let r = test_resolve();
    let mut names = NameInterner::new();
    let f = func_named(&r, "f-tuple-of-tuple");
    let plan = plan_for(&f.params[0].ty, &r, &mut names);
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            Cell::IntegerSignExt { flat_slot: 2 },
            Cell::TupleOf {
                children: vec![1, 2]
            },
            Cell::TupleOf {
                children: vec![0, 3]
            },
        ],
    );
    assert_eq!(plan.root(), 4);
    assert_eq!(plan.flat_slot_count, 3);
}

#[test]
fn record_with_tuple_field_recurses_into_tuple() {
    // point-and-tuple { p: point, t: tuple<u8, s32> }
    //   p.x   → cell 0 (slot 0, u32)
    //   p.y   → cell 1 (slot 1, s32)
    //   point → cell 2 (RecordOf)
    //   t.0   → cell 3 (slot 2, u8)
    //   t.1   → cell 4 (slot 3, s32)
    //   t     → cell 5 (TupleOf children=[3, 4])
    //   pat   → cell 6 (RecordOf p=2, t=5)
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("point-and-tuple", &r, &mut names);
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            record_of(&mut names, "point", &[("x", 0), ("y", 1)]),
            Cell::IntegerZeroExt { flat_slot: 2 },
            Cell::IntegerSignExt { flat_slot: 3 },
            Cell::TupleOf {
                children: vec![3, 4]
            },
            record_of(&mut names, "point-and-tuple", &[("p", 2), ("t", 5)]),
        ],
    );
    assert_eq!(plan.root(), 6);
    assert_eq!(plan.flat_slot_count, 4);
}

#[test]
fn option_allocates_disc_before_inner() {
    // option<u32>: disc i32 → slot 0, inner u32 → slot 1.
    // Cell order is children-before-parent, so the IntegerZeroExt for
    // the inner u32 lands at cell 0 (with flat_slot=1) and the Option
    // parent at cell 1 (with disc_slot=0).
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(&func_named(&r, "f-option-u32").params[0].ty, &r, &mut names);
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::Option {
                disc_slot: 0,
                child_idx: 0,
            },
        ],
    );
    assert_eq!(plan.root(), 1);
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn option_of_string_keeps_canonical_disc_first() {
    // option<string>: [disc i32, ptr i32, len i32] in canonical-ABI
    // order. Plan-builder bumps disc first (slot 0), then string's
    // (ptr=1, len=2). Cell ordering still places the leaf first.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-option-string").params[0].ty,
        &r,
        &mut names,
    );
    assert_eq!(
        plan.cells,
        vec![
            Cell::Text {
                ptr_slot: 1,
                len_slot: 2,
            },
            Cell::Option {
                disc_slot: 0,
                child_idx: 0,
            },
        ],
    );
    assert_eq!(plan.flat_slot_count, 3);
}

#[test]
fn nested_option_walks_disc_per_layer() {
    // option<option<u32>>: outer disc → slot 0, inner disc → slot 1,
    // u32 → slot 2. Cell order: leaf u32 (cell 0), inner Option (cell
    // 1, disc=1, child=0), outer Option (cell 2, disc=0, child=1).
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-option-option").params[0].ty,
        &r,
        &mut names,
    );
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 2 },
            Cell::Option {
                disc_slot: 1,
                child_idx: 0,
            },
            Cell::Option {
                disc_slot: 0,
                child_idx: 1,
            },
        ],
    );
    assert_eq!(plan.root(), 2);
    assert_eq!(plan.flat_slot_count, 3);
}

#[test]
fn record_with_option_field_recurses_into_option() {
    // point-and-option { p: point, o: option<u32> }
    //   p.x  → cell 0 (slot 0, u32)
    //   p.y  → cell 1 (slot 1, s32)
    //   p    → cell 2 (RecordOf)
    //   o.inner → cell 3 (slot 3, u32)  -- disc bumped first → slot 2
    //   o    → cell 4 (Option { disc:2, child:3 })
    //   pao  → cell 5 (RecordOf p=2, o=4)
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("point-and-option", &r, &mut names);
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            record_of(&mut names, "point", &[("x", 0), ("y", 1)]),
            Cell::IntegerZeroExt { flat_slot: 3 },
            Cell::Option {
                disc_slot: 2,
                child_idx: 3,
            },
            record_of(&mut names, "point-and-option", &[("p", 2), ("o", 4)]),
        ],
    );
    assert_eq!(plan.root(), 5);
    assert_eq!(plan.flat_slot_count, 4);
}

#[test]
fn result_u32_string_shares_arms_flat_slots() {
    // result<u32, string>: joined flat = [i32 disc, i32, i32].
    // Ok=u32 claims slot 1; Err=string claims slots 1, 2 (sharing
    // slot 1 via the save-and-restore cursor). Cell order:
    //   IntegerZeroExt {flat_slot:1}  // ok arm leaf
    //   Text {ptr:1, len:2}           // err arm leaf
    //   Result { disc:0, ok:Some(0), err:Some(1) }
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-result-u32-string").params[0].ty,
        &r,
        &mut names,
    );
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::Text {
                ptr_slot: 1,
                len_slot: 2,
            },
            Cell::Result {
                disc_slot: 0,
                ok_idx: Some(0),
                err_idx: Some(1),
            },
        ],
    );
    assert_eq!(plan.root(), 2);
    assert_eq!(plan.flat_slot_count, 3);
}

#[test]
fn result_u32_u64_records_joined_flat_widening() {
    // result<u32, u64>: ok flat = [I32], err flat = [I64]; joined =
    // [I32 disc, I64 (= max width)]. Ok arm's leaf reads slot 1
    // expecting I32 — emit must bitcast I64→I32. Err arm matches
    // joined; no bitcast.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-result-u32-u64").params[0].ty,
        &r,
        &mut names,
    );
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::Integer64 { flat_slot: 1 },
            Cell::Result {
                disc_slot: 0,
                ok_idx: Some(0),
                err_idx: Some(1),
            },
        ],
    );
    assert_eq!(plan.flat_slot_count, 2);
    // Disc never widens (joined position 0 is always I32).
    assert!(plan.widening_for(0).is_none());
    assert_eq!(plan.widening_for(1), Some(WasmType::I64));
}

#[test]
fn variant_tri_arm_records_joined_flat_widening() {
    // variant tri-arm { a(u32), b(u64), c(f64) }:
    // a → [I32], b → [I64], c → [F64]; joined = [I32 disc, I64
    // (max width across arms)]. a + c widen (I32 / F64 vs I64),
    // b matches — slot_widening[1] is recorded once (idempotent
    // across arms; joined is structural).
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-variant-tri-arm").params[0].ty,
        &r,
        &mut names,
    );
    assert_eq!(plan.flat_slot_count, 2);
    assert!(plan.widening_for(0).is_none());
    assert_eq!(plan.widening_for(1), Some(WasmType::I64));
}

#[test]
fn result_u32_string_records_no_widening() {
    // result<u32, string>: ok flat = [I32], err flat = [I32, I32];
    // joined = [I32, I32, I32]. Every arm position matches the
    // joined wasm type → no widening recorded.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-result-u32-string").params[0].ty,
        &r,
        &mut names,
    );
    for slot in 0..plan.flat_slot_count {
        assert!(
            plan.widening_for(slot).is_none(),
            "result<u32, string> slot {slot} should not widen",
        );
    }
}

#[test]
fn result_unit_ok_skips_ok_child() {
    // result<_, string>: Ok arm is unit, no child cell. Err=string
    // claims slots 1, 2. ok_idx=None, err_idx=Some(0).
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-result-unit-err").params[0].ty,
        &r,
        &mut names,
    );
    assert_eq!(
        plan.cells,
        vec![
            Cell::Text {
                ptr_slot: 1,
                len_slot: 2,
            },
            Cell::Result {
                disc_slot: 0,
                ok_idx: None,
                err_idx: Some(0),
            },
        ],
    );
    assert_eq!(plan.flat_slot_count, 3);
}

#[test]
fn result_unit_err_skips_err_child() {
    // result<u32>: only Ok arm has a payload. ok_idx=Some(0),
    // err_idx=None. Total 2 slots (disc + u32).
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-result-ok-unit").params[0].ty,
        &r,
        &mut names,
    );
    assert_eq!(
        plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::Result {
                disc_slot: 0,
                ok_idx: Some(0),
                err_idx: None,
            },
        ],
    );
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn result_both_unit_is_disc_only() {
    // result<_, _>: both arms unit. Just the disc, no children.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-result-both-unit").params[0].ty,
        &r,
        &mut names,
    );
    assert_eq!(
        plan.cells,
        vec![Cell::Result {
            disc_slot: 0,
            ok_idx: None,
            err_idx: None,
        }],
    );
    assert_eq!(plan.root(), 0);
    assert_eq!(plan.flat_slot_count, 1);
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
    let params = classify_func_params(&r, func_named(&r, "f-mixed"), &mut names)
        .expect("f-mixed params must classify");
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
    let params = classify_func_params(&r, func_named(&r, "f-mixed"), &mut names)
        .expect("f-mixed params must classify");
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

// ─── Side-table scratch sizing parity ─────────────────────────

#[test]
fn char_scratch_sizes_count_single_cell_char_result() {
    // Regression: `char_scratch_sizes` must pick up a single-cell
    // char result by checking the classified `Cell::Char` (not the
    // raw `result_ty`), so a `type my-char = char` alias works.
    use super::classify::SideTableInfo;
    use super::sidetable::char_info::char_scratch_sizes;
    let r = test_resolve();
    let mut names = NameInterner::new();
    let mut fd = func_with_params(&r, &mut names, &[]);
    fd.result_ty = Some(Type::Char);
    fd.result_lift = Some(ResultLift {
        source: ResultSource::Direct(Cell::Char { flat_slot: 0 }),
        side_table: SideTableInfo::default(),
    });
    // 1 char-result × 4 bytes scratch.
    assert_eq!(char_scratch_sizes(&[fd]), vec![MAX_UTF8_LEN]);
}

#[test]
fn flags_scratch_sizes_count_both_param_and_result_cells() {
    // Regression: `flags_scratch_sizes` must walk per-fn params AND
    // the compound result plan, in the order `build_flags_info_blob`
    // consumes addresses — otherwise a record-result-with-flags
    // crashes the builder's `scratch_addrs.next()` expect.
    use super::classify::{CompoundResult, SideTableInfo};
    use super::sidetable::flags_info::flags_scratch_sizes;
    let r = test_resolve();
    let mut names = NameInterner::new();
    let fd_param = func_with_params(&r, &mut names, &["fperms"]);
    let mut fd_result = func_with_params(&r, &mut names, &[]);
    fd_result.result_lift = Some(ResultLift {
        source: ResultSource::Compound(CompoundResult {
            ty: type_named(&r, "perms-pair"),
            plan: plan_for_named("perms-pair", &r, &mut names),
        }),
        side_table: SideTableInfo::default(),
    });
    // 1 flags param + 2 flags inside the record result → 3 slabs of
    // 3 flags × 8 bytes each.
    assert_eq!(
        flags_scratch_sizes(&[fd_param, fd_result]),
        vec![24, 24, 24]
    );
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
    // one (fn, param), not across them. Children-first plan order
    // puts each RecordOf cell after its descendants, so the
    // `Some(_)` slots land at the *end* of each map (and, for
    // `nested`, the inner `point` parent picks up side-table idx 0
    // before the outer `nested` parent picks up idx 1).
    let expected: Vec<Vec<&[Option<u32>]>> = vec![
        vec![&[None, None, Some(0)]],
        vec![
            &[None, None, Some(0)],
            &[None, None, Some(0), None, Some(1)],
        ],
    ];
    for (fn_idx, fn_expected) in expected.iter().enumerate() {
        for (param_idx, param_expected) in fn_expected.iter().enumerate() {
            assert_eq!(
                blobs.per_cell_idx.for_param(fn_idx, param_idx),
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
        plan_for(&Type::Char, &r, &mut names),
        plan_for(&func_named(&r, "f-mixed").params[2].ty, &r, &mut names), // list<u8>
        plan_for_named("color", &r, &mut names),
        plan_for_named("fperms", &r, &mut names),
        plan_for_named("shape", &r, &mut names),
        plan_for_named("point", &r, &mut names),
        plan_for_named("nested", &r, &mut names),
        plan_for_named("perms-pair", &r, &mut names),
        plan_for_named("shape-pair", &r, &mut names),
        plan_for(&func_named(&r, "f-tuple").params[0].ty, &r, &mut names),
        plan_for(
            &func_named(&r, "f-tuple-of-tuple").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for_named("point-and-tuple", &r, &mut names),
        plan_for(&func_named(&r, "f-option-u32").params[0].ty, &r, &mut names),
        plan_for(
            &func_named(&r, "f-option-string").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for(
            &func_named(&r, "f-option-option").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for_named("point-and-option", &r, &mut names),
        plan_for(
            &func_named(&r, "f-result-u32-string").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for(
            &func_named(&r, "f-result-unit-err").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for(
            &func_named(&r, "f-result-ok-unit").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for(
            &func_named(&r, "f-result-both-unit").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for(
            &func_named(&r, "f-result-u32-u64").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for(
            &func_named(&r, "f-variant-tri-arm").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for(&func_named(&r, "f-handle-own").params[0].ty, &r, &mut names),
        plan_for(
            &func_named(&r, "f-handle-borrow").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for_named("handle-pair", &r, &mut names),
        plan_for(&func_named(&r, "f-stream-u32").params[0].ty, &r, &mut names),
        plan_for(
            &func_named(&r, "f-future-string").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for(
            &func_named(&r, "f-stream-of-res").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for_named("stream-pair", &r, &mut names),
        plan_for(
            &func_named(&r, "f-error-context").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for(
            &func_named(&r, "f-result-with-err-ctx").params[0].ty,
            &r,
            &mut names,
        ),
        // Scalar-element lists only (compound elements still todo!()
        // in `emit_list_of_arm`).
        plan_for(&func_named(&r, "f-list-u32").params[0].ty, &r, &mut names),
        plan_for(
            &func_named(&r, "f-list-string").params[0].ty,
            &r,
            &mut names,
        ),
        plan_for_named("list-pair", &r, &mut names),
    ];
    for plan in &primitive_plans {
        validate_emit_lift_plan(plan, &r);
    }
}

// ─── List-of emit codegen (scalar elements) ──────────────────

#[test]
fn list_of_u32_emits_valid_wasm() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(&func_named(&r, "f-list-u32").params[0].ty, &r, &mut names);
    validate_emit_lift_plan(&plan, &r);
}

#[test]
fn list_of_string_emits_valid_wasm() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-list-string").params[0].ty,
        &r,
        &mut names,
    );
    validate_emit_lift_plan(&plan, &r);
}

#[test]
fn list_of_enum_emits_valid_wasm() {
    // Enum elements are scalar — single i32, no per-element scratch.
    let r = Resolve::new();
    // Reuse TEST_WIT but with a list<color> shape — already there
    // implicitly via the lift fixture; we cons up the type here.
    let mut r = r;
    r.push_str(
        "list-of-enum.wit",
        r#"
        package test:list-enum@0.0.1;
        interface t {
            enum color { red, green, blue }
            f: func(xs: list<color>);
        }
        "#,
    )
    .unwrap();
    let iface = super::super::test_utils::iface_by_unversioned_qname(&r, "test:list-enum/t");
    let func_id = r.interfaces[iface]
        .functions
        .keys()
        .find(|n| *n == "f")
        .unwrap()
        .clone();
    let func = &r.interfaces[iface].functions[&func_id];
    let mut names = NameInterner::new();
    let plan =
        LiftPlan::for_type(&func.params[0].ty, &r, &mut names).expect("list<color> must classify");
    validate_emit_lift_plan(&plan, &r);
}

#[test]
fn list_result_classifies_as_compound() {
    // `list<T>` (non-u8) results route through retptr + Compound; the
    // produced plan has the same structural shape as a param-side
    // `list<T>` plan (validate-emit coverage already pins emit shape).
    let r = test_resolve();
    let mut names = NameInterner::new();
    let func = func_named(&r, "f-result-list-u32");
    let result_lift = classify_result_lift(&r, func, true, &mut names)
        .expect("list<u32> result must classify")
        .expect("list<u32> result must produce a ResultLift");
    let compound = result_lift
        .compound()
        .expect("list<u32> result must route through Compound");
    assert!(
        matches!(compound.plan.cells[0], Cell::ListOf { .. }),
        "Compound result plan's root cell must be ListOf, got {:?}",
        compound.plan.cells[0],
    );
}

#[test]
fn record_with_list_field_emits_valid_wasm() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("list-pair", &r, &mut names);
    validate_emit_lift_plan(&plan, &r);
}
