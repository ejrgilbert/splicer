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

/// Wasm `ValType` for each flat slot consumed by `plan.cells`, in
/// flat-slot order. RecordOf / TupleOf cells contribute no slots.
/// Indexed by `flat_slot` rather than cell order — `Cell::Option`
/// allocates the disc before recursing into the child, so flat-slot
/// order can diverge from cell order.
fn plan_param_types(plan: &LiftPlan) -> Vec<ValType> {
    let mut by_slot: Vec<Option<ValType>> = vec![None; plan.flat_slot_count as usize];
    let mut put = |slot: u32, ty: ValType| by_slot[slot as usize] = Some(ty);
    for op in &plan.cells {
        match op {
            Cell::Bool { flat_slot }
            | Cell::IntegerSignExt { flat_slot }
            | Cell::IntegerZeroExt { flat_slot }
            | Cell::EnumCase { flat_slot, .. }
            | Cell::Flags { flat_slot, .. }
            | Cell::Char { flat_slot }
            | Cell::Handle { flat_slot, .. } => put(*flat_slot, ValType::I32),
            Cell::Integer64 { flat_slot } => put(*flat_slot, ValType::I64),
            Cell::FloatingF32 { flat_slot } => put(*flat_slot, ValType::F32),
            Cell::FloatingF64 { flat_slot } => put(*flat_slot, ValType::F64),
            Cell::Text { ptr_slot, len_slot } | Cell::Bytes { ptr_slot, len_slot } => {
                put(*ptr_slot, ValType::I32);
                put(*len_slot, ValType::I32);
            }
            Cell::Option { disc_slot, .. } => put(*disc_slot, ValType::I32),
            Cell::Result { disc_slot, .. } => put(*disc_slot, ValType::I32),
            Cell::Variant { disc_slot, .. } => put(*disc_slot, ValType::I32),
            Cell::RecordOf { .. } | Cell::TupleOf { .. } => {}
            Cell::ListOf => {
                unreachable!("un-wired Cell variant {op:?} should not appear in test plans")
            }
        }
    }
    by_slot
        .into_iter()
        .map(|t| t.expect("every flat slot must be claimed by some cell"))
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
                    entry_seg_off: 0, // not exercised by validate_emit_lift_plan
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
                    entry_seg_off: 0, // not exercised by validate_emit_lift_plan
                    id_addr: Some(id_addr as i32),
                };
                handle_idx += 1;
                CellSideData::Handle(Box::new(fill))
            }
            // Mirror the production `fold_cell_side_data` exhaustivity
            // contract — primitives and control-flow cells return
            // None, un-wired variants `unreachable!()`.
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
            | Cell::Result { .. } => CellSideData::None,
            Cell::ListOf => {
                unreachable!("auto_cell_side_data reached un-wired Cell variant {op:?}")
            }
        })
        .collect()
}

/// Round-trip a plan through `emit_lift_plan` and validate the
/// resulting wasm module. Plan flat slots map straight to wasm fn
/// params; the wrapper-locals extras sit above them.
fn validate_emit_lift_plan(plan: &LiftPlan) {
    let cell_layout = synth_cell_layout();
    let cell_side = auto_cell_side_data(plan);
    let param_types = plan_param_types(plan);
    let n = plan.flat_slot_count;
    let lcl = WrapperLocals {
        addr: n,
        st: 0,
        ws: 0,
        flags_addr: n + 1,
        flags_count: n + 2,
        char_len: n + 3,
        ext64: n + 4,
        ext_f64: n + 5,
        result: None,
        tr_addr: None,
        id_local: 0,
        task_return_loads: None,
        saved_bump: 0,
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
        (4u32, ValType::I32),
        (1u32, ValType::I64),
        (1u32, ValType::F64),
    ]);
    // Wasm function params occupy locals 0..flat_slot_count, so
    // `local_base = 0` aligns the plan's flat slots with the
    // params declared on the synth wasm fn.
    emit_lift_plan(
        &mut f,
        &cell_layout,
        0,
        plan,
        super::emit::CellSideRefs {
            cell_side: &cell_side,
        },
        0,
        &lcl,
    );
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
    ];
    for plan in &primitive_plans {
        validate_emit_lift_plan(plan);
    }
}
