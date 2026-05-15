//! Tests: build a minimal config, call the helper, assert_eq against
//! an expected. New cases are mostly one-liners over the helpers.

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
use super::plan::{desugar_map_aliases, ArmGuard, Cell, HandleKind, LiftPlan, MapAliases};
use super::sidetable::flags_info::FlagsRuntimeFill;
use super::sidetable::handle_info::HandleRuntimeFill;
use super::sidetable::record_info::RecordRuntimeFill;
use super::sidetable::variant_info::VariantRuntimeFill;
use super::sidetable::{CellSideData, CharScratch, TupleIdxSource};
use super::*;

// ─── Fixture WIT + Resolve helpers ────────────────────────────

/// Single-interface fixture WIT used by every test.
const TEST_WIT: &str = r#"
    package test:lift@0.0.1;
    interface t {
        enum color { red, green, blue }
        enum mood { happy, sad }
        flags fperms { read, write, exec }
        flags fcaps { net, fs, time, env, rand }
        variant shape { circle, sq(u32), tri(u32) }
        record point { x: u32, y: s32 }
        record solo { v: u32 }
        record nested { p: point, c: color }
        record two-enums { c: color, m: mood }
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
        // F32 leaf in a widened slot: a(f32) → [F32], b(u64) → [I64];
        // joined slot 1 = I64. a's f32 leaf reads slot 1 expecting
        // F32 → emit bitcast I64→F32 via `lcl.widen_f32`.
        variant f32-widen { a(f32), b(u64) }
        f-variant-f32-widen: func(v: f32-widen);
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
        f-list-char: func(xs: list<char>);
        f-list-option-u32: func(xs: list<option<u32>>);
        f-list-option-string: func(xs: list<option<string>>);
        f-list-option-option-u32: func(xs: list<option<option<u32>>>);
        f-list-result-u32-string: func(xs: list<result<u32, string>>);
        f-list-result-unit-string: func(xs: list<result<_, string>>);
        f-list-tuple-u32-u32: func(xs: list<tuple<u32, u32>>);
        f-list-tuple-u32-string: func(xs: list<tuple<u32, string>>);
        f-list-tuple-of-tuple: func(xs: list<tuple<u32, tuple<s32, s32>>>);
        f-list-handle-own: func(xs: list<own<my-res>>);
        f-list-handle-borrow: func(xs: list<borrow<my-res>>);
        f-list-error-context: func(xs: list<error-context>);
        f-list-flags: func(xs: list<fperms>);
        f-list-tuple-of-flags: func(xs: list<tuple<fperms, fperms>>);
        // Mixed-width tuple element: fperms (3 bits) + fcaps (5 bits)
        // — cumulative `scratch_offset_in_elem` for the second cell
        // must equal `3 * STRING_FLAT_BYTES`, not 5x. Catches a
        // regression where the offset is computed from a uniform
        // per-cell stride.
        f-list-tuple-mixed-flags: func(xs: list<tuple<fperms, fcaps>>);
        f-list-tuple-of-handles:
            func(xs: list<tuple<own<my-res>, borrow<my-res>>>);
        record handle-and-stuff { h: own<my-res>, x: u32 }
        f-handles-mixed-with-list:
            func(top: own<my-res>, xs: list<own<my-res>>);
        record list-char-pair { items: list<char>, scores: list<u32> }
        f-record-with-list-char: func(rcp: list-char-pair);
        f-result-list-u32: func() -> list<u32>;
        f-result-list-char: func() -> list<char>;
        f-list-of-list: func(xs: list<list<u32>>);
        // `list<list<T>>` per inner-T kind in the depth-2 nested-list
        // scope (scalar / char / option / result / tuple / enum). Each
        // exercises the inner element class through the nested
        // `emit_list_of_arm` recursion.
        f-list-of-list-string: func(xs: list<list<string>>);
        f-list-of-list-char: func(xs: list<list<char>>);
        f-list-of-list-option-u32: func(xs: list<list<option<u32>>>);
        f-list-of-list-result-u32-string:
            func(xs: list<list<result<u32, string>>>);
        f-list-of-list-tuple-u32-u32: func(xs: list<list<tuple<u32, u32>>>);
        f-list-of-list-color: func(xs: list<list<color>>);
        // Nested-list at result position — retptr-loaded compound.
        f-result-list-of-list-u32: func() -> list<list<u32>>;
        // Depth-2 cap: a `list<…>` element-cell may only appear as
        // the SOLE cell of its parent list's element plan. Wrapping
        // a `list<T>` in option/tuple/record/variant/result inside
        // another list violates the cap — emit's `nested_inner` lives
        // on a single element-plan position and would panic without
        // the gate. These fixtures pin the plan-build rejection.
        f-list-option-list-u32: func(xs: list<option<list<u32>>>);
        f-list-tuple-with-list-u32: func(xs: list<tuple<u32, list<u32>>>);
        record list-field-record { ys: list<u32> }
        f-list-record-with-list: func(xs: list<list-field-record>);
        variant list-arm-variant { v(list<u32>), empty }
        f-list-variant-list-arm: func(xs: list<list-arm-variant>);
        f-list-result-with-list-ok: func(xs: list<result<list<u32>, u32>>);
        record list-pair { items: list<string>, scores: list<u32> }
        f-list-of-record: func(xs: list<point>);
        // `list<T, N>` (canon-ABI fixed-length list) flattens to
        // `N × flat(T)` inlined — same shape as `tuple<T;N>`. Lift
        // desugars at plan-build into `Cell::TupleOf` with N children.
        f-fixed-list-u32: func(xs: list<u32, 3>);
        // N=1: WIT forbids 1-tuples, so single-child TupleOf is only
        // reachable via FixedLengthList.
        f-fixed-list-u32-one: func(xs: list<u32, 1>);
        // N=0: WIT grammar parses it, plan-build bails with a clear
        // error rather than constructing an empty TupleOf.
        f-fixed-list-u32-zero: func(xs: list<u32, 0>);
        // N over the layout budget — pre-bail at plan-build so we
        // can't overflow `bump_flat_slot`'s u32 counter.
        f-fixed-list-u32-huge: func(xs: list<u32, 17>);
        record point-and-fixed-list { p: point, xs: list<u32, 3> }
        f-record-with-fixed-list: func(rwf: point-and-fixed-list);
        // `list<list<u32, 3>>`: outer dynamic list, each element is a
        // 3-wide TupleOf — exercises `Cell::TupleOf` as a list element
        // via `PrestagedTupleIndices`.
        f-list-of-fixed-list: func(xs: list<list<u32, 3>>);
        // FixedLengthList nested in FixedLengthList — outer TupleOf
        // wraps inner TupleOf children. Mirrors `nested_tuple_…` but
        // through the desugar path.
        f-fixed-list-of-fixed-list: func(xs: list<list<u32, 2>, 3>);
        // Result-side coverage: `is_compound_result` must accept
        // FixedLengthList so the wrapper retptr-loads it correctly.
        f-result-fixed-list-u32: func() -> list<u32, 3>;
        // `map<K, V>` is canonical-ABI shorthand for `list<tuple<K, V>>`.
        // Lift desugars via a synthetic tuple typedef, then reuses the
        // existing `Cell::ListOf` + `Cell::TupleOf` path.
        f-map-string-u32: func(m: map<string, u32>);
        record map-pair { primary: map<string, u32>, secondary: map<u32, string> }
        f-record-with-map: func(rwm: map-pair);
        f-result-map: func() -> map<string, u32>;
        // Nested-list value still gated — map<K, list<list<T>>> must
        // bail at plan-build with the same message as `list<list<T>>`.
        f-map-of-list-of-list: func(m: map<string, list<list<u32>>>);
        // Type-alias chain over Map: `desugar_map_aliases` must register
        // the underlying Map typedef even when the function references
        // it through an alias. Push then peels the alias and lands on
        // the same Map typeid — alias map lookup succeeds.
        type aliased-map = map<string, u32>;
        f-aliased-map: func(m: aliased-map);
        // `list<variant>` is still gated — covers the bail path that
        // the deleted `list_of_compound_element_bails_at_plan_build`
        // generic-test used to assert. New still-gated kinds drop in
        // here as one-liners.
        f-list-of-variant: func(xs: list<shape>);
        // Multi-variant element — pins `variants_per_elem > 1` +
        // cumulative `entry_offset_in_elem` so a uniform-stride bug
        // would compute different cell-array indices than the
        // cumulative-count formula.
        f-list-tuple-of-variants: func(xs: list<tuple<shape, shape>>);
        // Multi-record element with mismatched field counts (point=2,
        // solo=1) — pins literal `tuples_offset_in_elem` so a
        // uniform-stride bug (e.g. records_per_elem * max_fields *
        // tuple_size) doesn't silently match the cumulative formula.
        f-list-tuple-record-mixed: func(xs: list<tuple<point, solo>>);
        f-result-list-list: func(r: result<list<u32>, list<u32>>);
        variant list-or-int { with-list(list<u32>), plain(u32) }
        f-variant-list-arm: func(v: list-or-int);
        f-option-list: func(o: option<list<u32>>);
        // Depth-2 nesting: outer result, inner variant, list in
        // case 0 of the inner variant. The list-of cell must carry
        // both `(outer disc, ok=0)` and `(inner disc, case=0)`.
        f-result-of-variant-with-list:
            func(v: result<list-or-int, u32>);
    }
"#;

/// Test resolve bundled with its `MapAliases` sidecar. Fields are
/// private + accessed via [`Self::resolve`] / [`Self::aliases`] so
/// call sites can't drift into reading the bare `Resolve` without
/// also having the matching alias table (a `Deref`-to-`Resolve`
/// earlier draft compounded with two factory flavors and made it
/// easy to forget which sites needed the aliases).
pub(super) struct TestResolve {
    inner: Resolve,
    aliases: MapAliases,
}

impl TestResolve {
    fn resolve(&self) -> &Resolve {
        &self.inner
    }

    fn aliases(&self) -> &MapAliases {
        &self.aliases
    }
}

fn test_resolve() -> TestResolve {
    let mut inner = Resolve::new();
    inner
        .push_str("test.wit", TEST_WIT)
        .expect("test WIT must parse");
    let aliases = desugar_map_aliases(&mut inner);
    TestResolve { inner, aliases }
}

/// Pair the standard test resolve with a fresh interner.
fn setup() -> (TestResolve, NameInterner) {
    (test_resolve(), NameInterner::new())
}

fn iface_id(resolve: &TestResolve) -> wit_parser::InterfaceId {
    super::super::test_utils::iface_by_unversioned_qname(resolve.resolve(), "test:lift/t")
}

fn type_named(resolve: &TestResolve, name: &str) -> Type {
    Type::Id(
        resolve.resolve().interfaces[iface_id(resolve)]
            .types
            .get(name)
            .copied()
            .unwrap_or_else(|| panic!("type `{name}` not found in fixture")),
    )
}

fn func_named<'a>(resolve: &'a TestResolve, name: &str) -> &'a WitFunction {
    resolve.resolve().interfaces[iface_id(resolve)]
        .functions
        .get(name)
        .unwrap_or_else(|| panic!("function `{name}` not found in fixture"))
}

// ─── Plan-builder + assertion fixture constructors ────────────

/// Thin alias for `LiftPlan::for_type`. Unwraps; negative cases call
/// `LiftPlan::for_type` directly.
fn plan_for(ty: &Type, resolve: &TestResolve, names: &mut NameInterner) -> LiftPlan {
    LiftPlan::for_type(ty, resolve.resolve(), names, resolve.aliases())
        .expect("test fixture must classify")
}

fn plan_for_named(name: &str, resolve: &TestResolve, names: &mut NameInterner) -> LiftPlan {
    plan_for(&type_named(resolve, name), resolve, names)
}

/// Plan for the first param of `func_name` in the fixture WIT.
fn plan_for_param(func_name: &str, resolve: &TestResolve, names: &mut NameInterner) -> LiftPlan {
    plan_for(&func_named(resolve, func_name).params[0].ty, resolve, names)
}

/// Pin a plan's full shape (cells, root, flat-slot count) in one call.
#[track_caller]
fn assert_plan(plan: &LiftPlan, cells: Vec<Cell>, root: u32, slot_count: u32) {
    assert_eq!(plan.cells, cells);
    assert_eq!(plan.root(), root);
    assert_eq!(plan.flat_slot_count, slot_count);
}

/// `assert_plan` minus `root` for tests where cell layout already pins it.
#[track_caller]
fn assert_plan_no_root(plan: &LiftPlan, cells: Vec<Cell>, slot_count: u32) {
    assert_eq!(plan.cells, cells);
    assert_eq!(plan.flat_slot_count, slot_count);
}

/// `Cell::Flags` shorthand. Interns through `names` so BlobSlices match.
fn flags_cell(names: &mut NameInterner, flat_slot: u32, type_name: &str, items: &[&str]) -> Cell {
    let type_name = names.intern(type_name);
    let flag_names = items.iter().map(|n| names.intern(n)).collect();
    Cell::Flags {
        flat_slot,
        type_name,
        flag_names,
    }
}

/// `Cell::EnumCase` shorthand. Same shape as `flags_cell`.
fn enum_cell(names: &mut NameInterner, flat_slot: u32, type_name: &str, cases: &[&str]) -> Cell {
    let type_name = names.intern(type_name);
    let case_names = cases.iter().map(|n| names.intern(n)).collect();
    Cell::EnumCase {
        flat_slot,
        type_name,
        case_names,
    }
}

/// `Cell::Variant` shorthand. Carries disc slot + per-case payloads.
fn variant_cell(
    names: &mut NameInterner,
    disc_slot: u32,
    type_name: &str,
    cases: &[&str],
    per_case_payload: Vec<Option<u32>>,
) -> Cell {
    let type_name = names.intern(type_name);
    let case_names = cases.iter().map(|n| names.intern(n)).collect();
    Cell::Variant {
        disc_slot,
        per_case_payload,
        type_name,
        case_names,
    }
}

/// `Cell::RecordOf` shorthand. Pass the same interner used for the plan.
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

fn make_param(ty: &Type, resolve: &TestResolve, names: &mut NameInterner) -> ParamLift {
    ParamLift {
        name: BlobSlice::EMPTY,
        plan: plan_for(ty, resolve, names),
    }
}

/// Build a `FuncClassified` with the given WIT param types; other
/// fields are dummies (side-table builders only read params/result).
fn func_with_params(
    resolve: &TestResolve,
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

/// Synthesize a per-call info-record `RecordLayout` (handle-info or
/// flags-info) from the live common WIT — same ergonomics as
/// [`synth_cell_layout`]. Drives the validator fixture's
/// `LiftEmitCtx.{handle_info, flags_info}`.
fn synth_info_layout(record_name: &str) -> RecordLayout {
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let mut resolve = Resolve::new();
    resolve
        .push_str("common.wit", common_wit)
        .expect("wit/common/world.wit must parse");
    let common_id =
        super::super::test_utils::iface_by_unversioned_qname(&resolve, "splicer:common/types");
    let record_id = resolve.interfaces[common_id]
        .types
        .get(record_name)
        .copied()
        .unwrap_or_else(|| panic!("splicer:common/types must export `{record_name}`"));
    let mut sizes = SizeAlign::default();
    sizes.fill(&resolve);
    RecordLayout::for_record_typedef(&sizes, &resolve, record_id)
}

/// Wasm `ValType` per flat slot from canonical-ABI `flat_types` —
/// pinning the test's declared params to this single source surfaces
/// emit_cell_op/expected drift as a validation error.
fn plan_param_types(plan: &LiftPlan, resolve: &Resolve) -> Vec<ValType> {
    use super::super::super::abi::emit::wasm_type_to_val;
    use super::super::super::abi::flat_types;
    flat_types(resolve, &plan.source_ty, None)
        .expect("plan source_ty must flatten within MAX_FLAT_PARAMS")
        .into_iter()
        .map(wasm_type_to_val)
        .collect()
}

/// Synthesize `CellSideData` with stub addresses; runtime value
/// correctness is the canned-shape harness's job.
fn auto_cell_side_data(plan: &LiftPlan) -> Vec<CellSideData> {
    const U32_BYTES: u32 = 4;
    /// Mid-page so stub addresses don't alias null.
    const FLAGS_SCRATCH_BASE: u32 = 0x1000;
    const STUB_FLAG_NAME_STRIDE: u32 = 16;
    const STUB_FLAG_NAME_LEN: u32 = 4;

    const RECORD_TUPLES_BASE: u32 = 0x4000;
    /// Each record slot is `STRING_FLAT_BYTES + 4`; 16 covers it for
    /// the validator (which only checks address shape).
    const RECORD_TUPLES_STRIDE: u32 = 16;
    const STUB_RECORD_NAME_LEN: u32 = 4;

    let mut record_idx: u32 = 0;
    let mut tuple_cursor: u32 = 0;
    let mut flags_cursor: u32 = FLAGS_SCRATCH_BASE;
    let mut flags_idx: u32 = 0;
    let mut variant_idx: u32 = 0;
    let mut char_cursor: u32 = 0x3000;
    let mut handle_idx: u32 = 0;
    plan.cells
        .iter()
        .map(|op| match op {
            Cell::RecordOf { fields, .. } => {
                use super::sidetable::record_info::RecordSlotSource;
                let entry_idx = record_idx;
                let fields_ptr = (RECORD_TUPLES_BASE + entry_idx * RECORD_TUPLES_STRIDE) as i32;
                let fill = RecordRuntimeFill {
                    slot_source: RecordSlotSource::Static {
                        entry_idx,
                        fields_ptr,
                    },
                    type_name: BlobSlice {
                        off: 0,
                        len: STUB_RECORD_NAME_LEN,
                    },
                    fields_len: fields.len() as u32,
                };
                record_idx += 1;
                CellSideData::Record(Box::new(fill))
            }
            Cell::TupleOf { children } => {
                let off = tuple_cursor;
                let len = children.len() as u32;
                tuple_cursor += len * U32_BYTES;
                CellSideData::Tuple {
                    source: TupleIdxSource::Static(BlobSlice { off, len }),
                }
            }
            Cell::Flags { flag_names, .. } => {
                use super::sidetable::flags_info::FlagsSlotSource;
                let scratch_addr = flags_cursor;
                flags_cursor += flag_names.len() as u32 * STRING_FLAT_BYTES;
                let fill = FlagsRuntimeFill {
                    slot_source: FlagsSlotSource::Static {
                        entry_idx: flags_idx,
                        scratch_addr: scratch_addr as i32,
                    },
                    type_name: BlobSlice {
                        off: 0,
                        len: STUB_FLAG_NAME_LEN,
                    },
                    flag_names: (0..flag_names.len() as u32)
                        .map(|i| BlobSlice {
                            off: i * STUB_FLAG_NAME_STRIDE,
                            len: STUB_FLAG_NAME_LEN,
                        })
                        .collect(),
                };
                flags_idx += 1;
                CellSideData::Flags(Box::new(fill))
            }
            Cell::Variant {
                case_names,
                per_case_payload,
                ..
            } => {
                use super::sidetable::variant_info::VariantSlotSource;
                let fill = VariantRuntimeFill {
                    slot_source: VariantSlotSource::Static {
                        entry_idx: variant_idx,
                    },
                    type_name: BlobSlice {
                        off: 0,
                        len: STUB_FLAG_NAME_LEN,
                    },
                    case_names: (0..case_names.len() as u32)
                        .map(|i| BlobSlice {
                            off: i * STUB_FLAG_NAME_STRIDE,
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
                    scratch: CharScratch::Static {
                        scratch_addr: scratch_addr as i32,
                    },
                }
            }
            Cell::Handle { type_name, .. } => {
                use super::sidetable::handle_info::HandleSlotSource;
                let fill = HandleRuntimeFill {
                    slot_source: HandleSlotSource::Static(handle_idx),
                    type_name: *type_name,
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

/// Run `emit_lift_plan` and validate the wasm. Imports a stub
/// `cabi_realloc` so list-of arm calls resolve; never invokes.
fn validate_emit_lift_plan(plan: &LiftPlan, resolve: &Resolve) {
    use super::super::super::indices::LocalsBuilder;
    use crate::adapter::indices::FrozenLocals;

    let mut sizes = SizeAlign::default();
    sizes.fill(resolve);
    let cell_layout = synth_cell_layout();
    let handle_info_layout = synth_info_layout("handle-info");
    let flags_info_layout = synth_info_layout("flags-info");
    let record_info_layout = synth_info_layout("record-info");
    let variant_info_layout = synth_info_layout("variant-info");
    let (_, record_field_tuple_layout) = synth_record_info_layouts(resolve);
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
    let widen_f32 = builder.alloc_local(ValType::F32);
    let flags_addr = builder.alloc_local(ValType::I32);
    let flags_count = builder.alloc_local(ValType::I32);
    let char_len = builder.alloc_local(ValType::I32);
    let char_scratch_addr = builder.alloc_local(ValType::I32);
    let list_elem_child_idx = builder.alloc_local(ValType::I32);
    let tuple_slot_ptr = builder.alloc_local(ValType::I32);
    let id_local = builder.alloc_local(ValType::I64);
    let saved_bump = builder.alloc_local(ValType::I32);
    let cells_base = builder.alloc_local(ValType::I32);
    let next_cell_idx = builder.alloc_local(ValType::I32);
    let handle_info_base = builder.alloc_local(ValType::I32);
    let flags_info_base = builder.alloc_local(ValType::I32);
    let record_info_base = builder.alloc_local(ValType::I32);
    let variant_info_base = builder.alloc_local(ValType::I32);
    let next_handle_idx = builder.alloc_local(ValType::I32);
    let next_flags_idx = builder.alloc_local(ValType::I32);
    let next_record_idx = builder.alloc_local(ValType::I32);
    let next_variant_idx = builder.alloc_local(ValType::I32);
    let list_elem_handle_base = builder.alloc_local(ValType::I32);
    let handle_slot_addr = builder.alloc_local(ValType::I32);
    let handle_payload_idx = builder.alloc_local(ValType::I32);
    let list_elem_flags_base = builder.alloc_local(ValType::I32);
    let list_elem_flags_scratch_base = builder.alloc_local(ValType::I32);
    let flags_slot_addr = builder.alloc_local(ValType::I32);
    let flags_payload_idx = builder.alloc_local(ValType::I32);
    let list_elem_record_base = builder.alloc_local(ValType::I32);
    let list_elem_record_tuples_base = builder.alloc_local(ValType::I32);
    let record_slot_addr = builder.alloc_local(ValType::I32);
    let record_payload_idx = builder.alloc_local(ValType::I32);
    let record_tuples_slice_addr = builder.alloc_local(ValType::I32);
    let list_elem_variant_base = builder.alloc_local(ValType::I32);
    let variant_slot_addr = builder.alloc_local(ValType::I32);
    let variant_payload_idx = builder.alloc_local(ValType::I32);
    let list_locals = super::emit::alloc_list_emit_locals(
        plan,
        resolve,
        &sizes,
        record_field_tuple_layout.size,
        &mut builder,
    );
    let FrozenLocals { locals } = builder.freeze();

    let lcl = WrapperLocals {
        addr,
        st,
        ws,
        ext64,
        ext_f64,
        widen_i32_a,
        widen_i32_b,
        widen_f32,
        flags_addr,
        flags_count,
        char_len: Some(char_len),
        char_scratch_addr: Some(char_scratch_addr),
        list_elem_child_idx: Some(list_elem_child_idx),
        tuple_slot_ptr: Some(tuple_slot_ptr),
        cells_base,
        next_cell_idx,
        handle_info_base: Some(handle_info_base),
        flags_info_base: Some(flags_info_base),
        record_info_base: Some(record_info_base),
        variant_info_base: Some(variant_info_base),
        next_handle_idx: Some(next_handle_idx),
        next_flags_idx: Some(next_flags_idx),
        next_record_idx: Some(next_record_idx),
        next_variant_idx: Some(next_variant_idx),
        list_elem_handle_base: Some(list_elem_handle_base),
        handle_slot_addr: Some(handle_slot_addr),
        handle_payload_idx: Some(handle_payload_idx),
        list_elem_flags_base: Some(list_elem_flags_base),
        list_elem_flags_scratch_base: Some(list_elem_flags_scratch_base),
        flags_slot_addr: Some(flags_slot_addr),
        flags_payload_idx: Some(flags_payload_idx),
        list_elem_record_base: Some(list_elem_record_base),
        list_elem_record_tuples_base: Some(list_elem_record_tuples_base),
        record_slot_addr: Some(record_slot_addr),
        record_payload_idx: Some(record_payload_idx),
        record_tuples_slice_addr: Some(record_tuples_slice_addr),
        list_elem_variant_base: Some(list_elem_variant_base),
        variant_slot_addr: Some(variant_slot_addr),
        variant_payload_idx: Some(variant_payload_idx),
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
        handle_info: super::emit::HandleInfoOffsets::from_layout(&handle_info_layout),
        flags_info: super::emit::FlagsInfoOffsets::from_layout(&flags_info_layout),
        record_info: super::emit::RecordInfoOffsets::from_layout(
            &record_info_layout,
            &record_field_tuple_layout,
        ),
        variant_info: super::emit::VariantInfoOffsets::from_layout(
            &variant_info_layout,
            super::super::super::abi::emit::option_payload_offset(&sizes, &Type::U32),
        ),
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
    let r = test_resolve();
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
    let plan = plan_for(&Type::String, &test_resolve(), &mut names);
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
    let (r, mut names) = setup();
    let bytes_ty = func_named(&r, "f-mixed").params[2].ty;
    let plan = plan_for(&bytes_ty, &r, &mut names);
    assert_plan_no_root(
        &plan,
        vec![Cell::Bytes {
            ptr_slot: 0,
            len_slot: 1,
        }],
        2,
    );
}

#[test]
fn char_assigns_one_cell_one_slot() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(&Type::Char, &r, &mut names);
    assert_eq!(plan.cells, vec![Cell::Char { flat_slot: 0 }]);
    assert_eq!(plan.flat_slot_count, 1);
}

#[test]
fn enum_carries_named_list_info() {
    let (r, mut names) = setup();
    assert_eq!(
        plan_for_named("color", &r, &mut names).cells,
        vec![enum_cell(&mut names, 0, "color", &["red", "green", "blue"])],
    );
}

#[test]
fn flags_assigns_one_cell_one_slot() {
    // `fperms` has 3 flags; canonical-ABI lowers them all into a
    // single i32 (caps at 32 bits). Plan is one Flags cell at
    // flat_slot 0 carrying the full NamedListInfo.
    let (r, mut names) = setup();
    let plan = plan_for_named("fperms", &r, &mut names);
    assert_plan_no_root(
        &plan,
        vec![flags_cell(
            &mut names,
            0,
            "fperms",
            &["read", "write", "exec"],
        )],
        1,
    );
}

#[test]
fn variant_lays_disc_first_then_arms_share_slots() {
    // shape { circle, sq(u32), tri(u32) }: 3 cases, 2 with payload.
    // Joined flat = [i32 disc, i32 (joined u32/u32)]. Cell order:
    //   sq's u32   → cell 0 (slot 1)
    //   tri's u32  → cell 1 (slot 1, shares with sq's slot)
    //   Variant    → cell 2 (disc=0, per_case_payload=[None, Some(0), Some(1)])
    let (r, mut names) = setup();
    let plan = plan_for_named("shape", &r, &mut names);
    assert_plan(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::IntegerZeroExt { flat_slot: 1 },
            variant_cell(
                &mut names,
                0,
                "shape",
                &["circle", "sq", "tri"],
                vec![None, Some(0), Some(1)],
            ),
        ],
        2,
        2,
    );
}

#[test]
fn record_with_variant_field_recurses_into_variant() {
    // shape-pair { lhs: shape, rhs: shape }: each variant claims one
    // disc slot + one shared-payload slot; arms share inside each
    // variant but lhs and rhs occupy independent slots.
    let (r, mut names) = setup();
    let plan = plan_for_named("shape-pair", &r, &mut names);
    let shape_cases: &[&str] = &["circle", "sq", "tri"];
    assert_plan(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::IntegerZeroExt { flat_slot: 1 },
            variant_cell(
                &mut names,
                0,
                "shape",
                shape_cases,
                vec![None, Some(0), Some(1)],
            ),
            Cell::IntegerZeroExt { flat_slot: 3 },
            Cell::IntegerZeroExt { flat_slot: 3 },
            variant_cell(
                &mut names,
                2,
                "shape",
                shape_cases,
                vec![None, Some(3), Some(4)],
            ),
            record_of(&mut names, "shape-pair", &[("lhs", 2), ("rhs", 5)]),
        ],
        6,
        4,
    );
}

#[test]
fn handle_assigns_one_cell_one_slot() {
    // own<my-res>: a single i32 (the canonical-ABI handle) → one
    // Cell::Handle with the resource's pre-interned type-name. The
    // interner dedupes, so re-interning "my-res" off the same
    // `names` returns the BlobSlice already on the cell.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-handle-own", &r, &mut names);
    let res_name = names.intern("my-res");
    assert_plan(
        &plan,
        vec![Cell::Handle {
            flat_slot: 0,
            type_name: res_name,
            kind: HandleKind::Resource,
        }],
        0,
        1,
    );
}

#[test]
fn borrow_handle_takes_same_shape_as_own() {
    // borrow<R> and own<R> both flatten to a single i32 (the canonical-
    // ABI handle); the lift codegen treats them identically. The
    // ownership distinction is the adapter's job, not the lift's.
    let (r, mut names) = setup();
    let own_plan = plan_for_param("f-handle-own", &r, &mut names);
    let borrow_plan = plan_for_param("f-handle-borrow", &r, &mut names);
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
    let (r, mut names) = setup();
    let plan = plan_for_named("handle-pair", &r, &mut names);
    let res_name = names.intern("my-res");
    assert_plan(
        &plan,
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
        2,
        2,
    );
}

#[test]
fn stream_handle_assigns_one_cell_one_slot() {
    // stream<u32>: single i32 (canonical-ABI handle); type-name
    // empty (anonymous element type for primitives).
    let (r, mut names) = setup();
    let plan = plan_for_param("f-stream-u32", &r, &mut names);
    let empty = names.intern("");
    assert_plan_no_root(
        &plan,
        vec![Cell::Handle {
            flat_slot: 0,
            type_name: empty,
            kind: HandleKind::Stream,
        }],
        1,
    );
}

#[test]
fn future_handle_takes_same_shape_as_stream() {
    // future<T> and stream<T> share `Cell::Handle` — only the
    // `kind` differs.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-future-string", &r, &mut names);
    let empty = names.intern("");
    assert_plan_no_root(
        &plan,
        vec![Cell::Handle {
            flat_slot: 0,
            type_name: empty,
            kind: HandleKind::Future,
        }],
        1,
    );
}

#[test]
fn stream_with_named_element_carries_element_type_name() {
    // stream<my-res>: type-name = "my-res" (named element type).
    let (r, mut names) = setup();
    let plan = plan_for_param("f-stream-of-res", &r, &mut names);
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
    let (r, mut names) = setup();
    let plan = plan_for_named("stream-pair", &r, &mut names);
    let empty = names.intern("");
    assert_plan(
        &plan,
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
            record_of(&mut names, "stream-pair", &[("events", 0), ("ack", 1)]),
        ],
        2,
        2,
    );
}

#[test]
fn error_context_assigns_one_cell_one_slot() {
    // `error-context`: a single i32 (canonical-ABI handle); type-name
    // empty (no nested type to surface — the cell-disc names the kind).
    let (r, mut names) = setup();
    let plan = plan_for_param("f-error-context", &r, &mut names);
    let empty = names.intern("");
    assert_plan(
        &plan,
        vec![Cell::Handle {
            flat_slot: 0,
            type_name: empty,
            kind: HandleKind::ErrorContext,
        }],
        0,
        1,
    );
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
    let (r, mut names) = setup();
    let plan = plan_for_param("f-result-with-err-ctx", &r, &mut names);
    let empty = names.intern("");
    assert_plan(
        &plan,
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
        2,
        2,
    );
}

#[test]
fn list_of_primitive_carries_element_plan() {
    // list<u32>: parent (ptr, len) → 2 i32 slots, 1 cell. Element
    // plan has its own flat slots — independent from the parent.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-u32", &r, &mut names);
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
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-string", &r, &mut names);
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
fn list_of_char_element_plan_uses_one_local_slot() {
    // list<char>: element is a single i32 code point. The per-call
    // utf-8 scratch buffer lives on `ListEmitLocals.char_scratch_base`
    // (cabi_realloc'd at emit time), not in the plan's flat slots.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(&func_named(&r, "f-list-char").params[0].ty, &r, &mut names);
    assert_eq!(plan.flat_slot_count, 2);
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
    assert_eq!(plan.root(), 0);
    assert_eq!(element_plan.cells, vec![Cell::Char { flat_slot: 0 }]);
    assert_eq!(element_plan.flat_slot_count, 1);
    assert_eq!(element_plan.root(), 0);
}

#[test]
fn list_of_option_u32_element_plan_is_two_cells() {
    // option<u32>: [IntegerZeroExt(slot 1), Option(disc=0, child_idx=0)].
    // Multi-cell list element — exercises the lifted gate +
    // PrestagedChildIdx class.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-list-option-u32").params[0].ty,
        &r,
        &mut names,
    );
    let Cell::ListOf { element_plan, .. } = &plan.cells[0] else {
        panic!("expected ListOf, got {:?}", plan.cells[0]);
    };
    assert_eq!(
        element_plan.cells,
        vec![
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::Option {
                disc_slot: 0,
                child_idx: 0,
            },
        ],
    );
    assert_eq!(element_plan.flat_slot_count, 2);
    assert_eq!(element_plan.root(), 1);
}

#[test]
fn list_of_result_u32_string_element_plan_shape() {
    // result<u32, string>: [Int(u32 arm), Text(err arm joined slots),
    //   Result(disc=0, ok_idx=0, err_idx=1)].
    // Each arm reads its own joined slot range; the result cell
    // carries both ok and err relative indices into the element plan.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-list-result-u32-string").params[0].ty,
        &r,
        &mut names,
    );
    let Cell::ListOf { element_plan, .. } = &plan.cells[0] else {
        panic!("expected ListOf, got {:?}", plan.cells[0]);
    };
    assert_eq!(element_plan.cells.len(), 3);
    let Cell::Result {
        disc_slot,
        ok_idx,
        err_idx,
    } = element_plan.cells[2]
    else {
        panic!(
            "expected Result at element-plan cell 2, got {:?}",
            element_plan.cells[2]
        );
    };
    assert_eq!(disc_slot, 0);
    assert_eq!(ok_idx, Some(0));
    assert_eq!(err_idx, Some(1));
}

#[test]
fn list_of_tuple_u32_string_element_plan_shape() {
    // tuple<u32, string>: [Int(slot 0), Text(slots 1..2),
    //   TupleOf(children=[0, 1])]. Each iteration writes the
    //   resolved cell-array indices into the per-call tuple-idx
    //   buffer slot; the cell payload reads back the staged ptr.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-tuple-u32-string", &r, &mut names);
    let Cell::ListOf { element_plan, .. } = &plan.cells[0] else {
        panic!("expected ListOf, got {:?}", plan.cells[0]);
    };
    assert_eq!(element_plan.cells.len(), 3);
    let Cell::TupleOf { children } = &element_plan.cells[2] else {
        panic!(
            "expected TupleOf at element-plan cell 2, got {:?}",
            element_plan.cells[2]
        );
    };
    assert_eq!(children.as_slice(), &[0, 1]);
    assert_eq!(element_plan.root(), 2);
}

/// Pull the element_plan out of a top-level `Cell::ListOf`.
fn list_element_plan(plan: &LiftPlan, cell_idx: usize) -> &LiftPlan {
    match &plan.cells[cell_idx] {
        Cell::ListOf { element_plan, .. } => element_plan,
        other => panic!("expected ListOf at cell {cell_idx}, got {other:?}"),
    }
}

#[test]
fn walk_element_plan_counts_match_side_data_lockstep() {
    // Drift between counts (which size the per-list buffers) and
    // side-data offsets (which index into them) is the failure mode
    // #5 flagged. Pin: counts == Σ(per-cell offsets) for every kind
    // that contributes both a count AND a build-time-relative
    // offset_in_elem (TupleOf children, Handle slots).
    use super::sidetable::flags_info::FlagsSlotSource;
    use super::sidetable::handle_info::HandleSlotSource;
    use super::sidetable::record_info::RecordSlotSource;
    use super::sidetable::variant_info::VariantSlotSource;
    let (r, mut names) = setup();
    let (_, record_tuple_layout) = synth_record_info_layouts(r.resolve());
    let record_tuple_size = record_tuple_layout.size;
    for fn_name in [
        "f-list-tuple-u32-string",
        "f-list-tuple-of-tuple",
        "f-list-tuple-u32-u32",
        "f-list-handle-own",
        "f-list-tuple-of-handles",
        "f-list-error-context",
        "f-list-flags",
        "f-list-tuple-of-flags",
        "f-list-tuple-mixed-flags",
        "f-list-of-record",
        "f-list-tuple-record-mixed",
        "f-list-of-variant",
        "f-list-tuple-of-variants",
    ] {
        let plan = plan_for_param(fn_name, &r, &mut names);
        let elem_plan = list_element_plan(&plan, 0);
        let (side, counts) = super::emit::walk_element_plan(elem_plan, record_tuple_size);
        assert_eq!(side.len(), elem_plan.cells.len());
        let mut expected_chars = 0u32;
        let mut expected_tuple_slots = 0u32;
        let mut expected_handles = 0u32;
        let mut expected_flags = 0u32;
        let mut expected_flags_scratch = 0u32;
        let mut expected_records = 0u32;
        let mut expected_record_tuples_bytes = 0u32;
        let mut expected_variants = 0u32;
        let mut running_tuple_off = 0u32;
        for (cell, sd) in elem_plan.cells.iter().zip(side.iter()) {
            match cell {
                Cell::Char { .. } => {
                    assert!(matches!(sd, CellSideData::Char { .. }));
                    expected_chars += 1;
                }
                Cell::TupleOf { children } => {
                    let CellSideData::Tuple {
                        source: TupleIdxSource::PerIteration { offset_in_elem },
                    } = sd
                    else {
                        panic!("expected PerIteration TupleOf side data, got {sd:?}");
                    };
                    assert_eq!(
                        *offset_in_elem, running_tuple_off,
                        "TupleOf offsets must be cumulative across element_plan",
                    );
                    running_tuple_off += children.len() as u32 * 4;
                    expected_tuple_slots += children.len() as u32;
                }
                Cell::Handle { .. } => {
                    let CellSideData::Handle(fill) = sd else {
                        panic!("expected Handle side data, got {sd:?}");
                    };
                    let HandleSlotSource::PerIteration { offset_in_elem } = fill.slot_source else {
                        panic!(
                            "list-element Handle must carry PerIteration slot source, \
                             got {:?}",
                            fill.slot_source,
                        );
                    };
                    assert_eq!(
                        offset_in_elem, expected_handles,
                        "Handle offset_in_elem must be the cumulative count",
                    );
                    expected_handles += 1;
                }
                Cell::Flags { flag_names, .. } => {
                    let CellSideData::Flags(fill) = sd else {
                        panic!("expected Flags side data, got {sd:?}");
                    };
                    let FlagsSlotSource::PerIteration {
                        entry_offset_in_elem,
                        scratch_offset_in_elem,
                    } = fill.slot_source
                    else {
                        panic!(
                            "list-element Flags must carry PerIteration slot source, \
                             got {:?}",
                            fill.slot_source,
                        );
                    };
                    assert_eq!(
                        entry_offset_in_elem, expected_flags,
                        "Flags entry_offset_in_elem must be the cumulative count",
                    );
                    assert_eq!(
                        scratch_offset_in_elem, expected_flags_scratch,
                        "Flags scratch_offset_in_elem must be cumulative \
                         (sum of prior cells' set-flags scratch sizes)",
                    );
                    expected_flags += 1;
                    expected_flags_scratch += flag_names.len() as u32 * STRING_FLAT_BYTES;
                }
                Cell::RecordOf { fields, .. } => {
                    let CellSideData::Record(fill) = sd else {
                        panic!("expected Record side data, got {sd:?}");
                    };
                    let RecordSlotSource::PerIteration {
                        entry_offset_in_elem,
                        tuples_offset_in_elem,
                    } = fill.slot_source
                    else {
                        panic!(
                            "list-element Record must carry PerIteration slot source, \
                             got {:?}",
                            fill.slot_source,
                        );
                    };
                    assert_eq!(
                        entry_offset_in_elem, expected_records,
                        "Record entry_offset_in_elem must be the cumulative count",
                    );
                    assert_eq!(
                        tuples_offset_in_elem, expected_record_tuples_bytes,
                        "Record tuples_offset_in_elem must be cumulative \
                         (sum of prior cells' field-tuples sizes)",
                    );
                    expected_records += 1;
                    expected_record_tuples_bytes += fields.len() as u32 * record_tuple_size;
                }
                Cell::Variant { .. } => {
                    let CellSideData::Variant(fill) = sd else {
                        panic!("expected Variant side data, got {sd:?}");
                    };
                    let VariantSlotSource::PerIteration {
                        entry_offset_in_elem,
                    } = fill.slot_source
                    else {
                        panic!(
                            "list-element Variant must carry PerIteration slot source, \
                             got {:?}",
                            fill.slot_source,
                        );
                    };
                    assert_eq!(
                        entry_offset_in_elem, expected_variants,
                        "Variant entry_offset_in_elem must be the cumulative count",
                    );
                    expected_variants += 1;
                }
                _ => assert!(matches!(sd, CellSideData::None)),
            }
        }
        assert_eq!(
            counts.chars, expected_chars,
            "counts.chars drift in {fn_name}",
        );
        assert_eq!(
            counts.tuple_idx_slots, expected_tuple_slots,
            "counts.tuple_idx_slots drift in {fn_name}",
        );
        assert_eq!(
            counts.handles, expected_handles,
            "counts.handles drift in {fn_name}",
        );
        assert_eq!(
            counts.flags, expected_flags,
            "counts.flags drift in {fn_name}",
        );
        assert_eq!(
            counts.flags_scratch_bytes, expected_flags_scratch,
            "counts.flags_scratch_bytes drift in {fn_name}",
        );
        assert_eq!(
            counts.records, expected_records,
            "counts.records drift in {fn_name}",
        );
        assert_eq!(
            counts.record_tuples_bytes, expected_record_tuples_bytes,
            "counts.record_tuples_bytes drift in {fn_name}",
        );
        assert_eq!(
            counts.variants, expected_variants,
            "counts.variants drift in {fn_name}",
        );
    }
}

#[test]
fn walk_element_plan_pins_mixed_flags_scratch_bytes_literal() {
    // Cross-check the cumulative `scratch_offset_in_elem` formula at
    // a literal value: `f-list-tuple-mixed-flags` = list<tuple<fperms,
    // fcaps>>, fperms = 3 bits, fcaps = 5 bits. Per-element scratch is
    // (3 + 5) * STRING_FLAT_BYTES; cell 0 starts at offset 0, cell 1
    // at 3 * STRING_FLAT_BYTES. A regression that uses a uniform stride
    // (max-bits or first-cell-bits) would compute different values and
    // be caught here in a way that the lockstep test (which derives
    // expected values from the same per-cell formula) wouldn't.
    use super::sidetable::flags_info::FlagsSlotSource;
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-tuple-mixed-flags", &r, &mut names);
    let elem_plan = list_element_plan(&plan, 0);
    let (_, record_tuple_layout) = synth_record_info_layouts(r.resolve());
    let (side, counts) = super::emit::walk_element_plan(elem_plan, record_tuple_layout.size);
    let flags_offsets: Vec<(u32, u32)> = side
        .iter()
        .filter_map(|sd| match sd {
            CellSideData::Flags(fill) => match fill.slot_source {
                FlagsSlotSource::PerIteration {
                    entry_offset_in_elem,
                    scratch_offset_in_elem,
                } => Some((entry_offset_in_elem, scratch_offset_in_elem)),
                _ => panic!("list-element Flags must be PerIteration"),
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        flags_offsets,
        vec![(0, 0), (1, 3 * STRING_FLAT_BYTES)],
        "tuple<fperms, fcaps> must give cell 0 (entry 0, scratch 0) \
         and cell 1 (entry 1, scratch 3 * STRING_FLAT_BYTES)",
    );
    assert_eq!(counts.flags, 2);
    assert_eq!(counts.flags_scratch_bytes, (3 + 5) * STRING_FLAT_BYTES);
}

#[test]
fn walk_element_plan_pins_mixed_record_tuples_bytes_literal() {
    // Twin of [`walk_element_plan_pins_mixed_flags_scratch_bytes_literal`]
    // for record tuples. `f-list-tuple-record-mixed` =
    // list<tuple<point, solo>>, point = 2 fields, solo = 1 field. The
    // element-plan record-cells land at cumulative
    // `tuples_offset_in_elem`s of (0, 2 * tuple_size); a regression
    // that uses a uniform per-record stride (e.g. max-fields or
    // records_per_elem-driven) would compute different values and be
    // caught here in a way that the lockstep test (which derives
    // expected values from the same per-cell formula) wouldn't.
    use super::sidetable::record_info::RecordSlotSource;
    let (r, mut names) = setup();
    let (_, record_tuple_layout) = synth_record_info_layouts(r.resolve());
    let tuple_size = record_tuple_layout.size;
    let plan = plan_for_param("f-list-tuple-record-mixed", &r, &mut names);
    let elem_plan = list_element_plan(&plan, 0);
    let (side, counts) = super::emit::walk_element_plan(elem_plan, tuple_size);
    let record_offsets: Vec<(u32, u32)> = side
        .iter()
        .filter_map(|sd| match sd {
            CellSideData::Record(fill) => match fill.slot_source {
                RecordSlotSource::PerIteration {
                    entry_offset_in_elem,
                    tuples_offset_in_elem,
                } => Some((entry_offset_in_elem, tuples_offset_in_elem)),
                _ => panic!("list-element Record must be PerIteration"),
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        record_offsets,
        vec![(0, 0), (1, 2 * tuple_size)],
        "tuple<point, solo> must give point (entry 0, tuples 0) \
         and solo (entry 1, tuples 2 * tuple_size)",
    );
    assert_eq!(counts.records, 2);
    assert_eq!(counts.record_tuples_bytes, (2 + 1) * tuple_size);
}

#[test]
fn walk_element_plan_zero_counts_for_scalar_only() {
    // Scalar-only element plans must produce zero-counts so the
    // per-list buffer allocations stay gated off.
    let (r, mut names) = setup();
    let (_, record_tuple_layout) = synth_record_info_layouts(r.resolve());
    for fn_name in ["f-list-u32", "f-list-string"] {
        let plan = plan_for_param(fn_name, &r, &mut names);
        let elem_plan = list_element_plan(&plan, 0);
        let (_, counts) = super::emit::walk_element_plan(elem_plan, record_tuple_layout.size);
        assert_eq!(counts.chars, 0, "{fn_name} should have no char cells");
        assert_eq!(
            counts.tuple_idx_slots, 0,
            "{fn_name} should have no tuple-idx slots",
        );
        assert_eq!(counts.handles, 0, "{fn_name} should have no handle cells");
        assert_eq!(counts.flags, 0, "{fn_name} should have no flags cells");
        assert_eq!(counts.records, 0, "{fn_name} should have no record cells");
        assert_eq!(counts.variants, 0, "{fn_name} should have no variant cells");
    }
}

#[test]
fn record_with_list_char_field_recurses_into_list() {
    // list-char-pair { items: list<char>, scores: list<u32> }
    //   items   → cell 0 (ListOf, slots 0..1, char element)
    //   scores  → cell 1 (ListOf, slots 2..3, u32 element)
    //   parent  → cell 2 (RecordOf items=0, scores=1)
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for_named("list-char-pair", &r, &mut names);
    assert_eq!(plan.cells.len(), 3);
    let Cell::ListOf { element_plan, .. } = &plan.cells[0] else {
        panic!("expected ListOf at cell 0, got {:?}", plan.cells[0]);
    };
    assert_eq!(element_plan.cells, vec![Cell::Char { flat_slot: 0 }]);
    assert!(matches!(plan.cells[1], Cell::ListOf { .. }));
    assert_eq!(
        plan.cells[2],
        record_of(&mut names, "list-char-pair", &[("items", 0), ("scores", 1)],),
    );
    assert_eq!(plan.flat_slot_count, 4);
    assert_eq!(plan.root(), 2);
}

#[test]
fn nested_list_inner_with_option_pins_child_idx_class() {
    // `list<list<option<u32>>>`: inner element plan = [disc, payload,
    // Option]. The inner ListOf carries class `PrestagedNestedList`,
    // its element plan's Option cell carries `PrestagedChildIdx` —
    // gates inner-element wrapper-locals via `any_list_element_has_class`.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-of-list-option-u32", &r, &mut names);
    let [Cell::ListOf { element_plan, .. }] = plan.cells.as_slice() else {
        panic!("expected one outer ListOf, got {:?}", plan.cells);
    };
    let [Cell::ListOf {
        element_plan: inner_plan,
        ..
    }] = element_plan.cells.as_slice()
    else {
        panic!("expected one inner ListOf, got {:?}", element_plan.cells);
    };
    // Inner: IntegerZeroExt payload + Option parent (disc is a slot,
    // not a cell — `push_option` only pushes one cell).
    assert_eq!(inner_plan.cells.len(), 2);
    assert!(matches!(inner_plan.cells[1], Cell::Option { .. }));
    // Recursive gate-walker reaches the inner Option.
    assert!(plan.any_list_element_has_class(super::plan::ListElementClass::PrestagedChildIdx));
    assert!(plan.any_list_element_has_class(super::plan::ListElementClass::PrestagedNestedList));
}

#[test]
fn nested_list_builds_with_listof_element() {
    // `list<list<u32>>`: outer element plan = [inner ListOf]; inner
    // element plan = [IntegerZeroExt]. Pins that the
    // `PrestagedNestedList` class accepts a scalar inner element.
    let (r, mut names) = setup();
    let plan = LiftPlan::for_type(
        &func_named(&r, "f-list-of-list").params[0].ty,
        r.resolve(),
        &mut names,
        r.aliases(),
    )
    .expect("list<list<u32>> plan-build must succeed");
    let [Cell::ListOf { element_plan, .. }] = plan.cells.as_slice() else {
        panic!("expected one outer ListOf, got {:?}", plan.cells);
    };
    let [Cell::ListOf {
        element_plan: inner_plan,
        ..
    }] = element_plan.cells.as_slice()
    else {
        panic!(
            "expected one inner ListOf in outer element plan, got {:?}",
            element_plan.cells
        );
    };
    assert!(matches!(
        inner_plan.cells.as_slice(),
        [Cell::IntegerZeroExt { flat_slot: 0 }]
    ));
}

#[test]
fn nested_list_under_wrapper_bails_at_plan_build() {
    // Depth-2 cap: a `list<…>` element-cell may only appear as the
    // sole cell of its parent list's element plan. These shapes wrap
    // a `list<T>` in option / tuple / record / variant / result
    // inside another list — must all bail at plan-build so emit's
    // single-position `nested_inner` invariant holds.
    let (r, mut names) = setup();
    for fixture in [
        "f-list-option-list-u32",
        "f-list-tuple-with-list-u32",
        "f-list-record-with-list",
        "f-list-variant-list-arm",
        "f-list-result-with-list-ok",
    ] {
        let err = LiftPlan::for_type(
            &func_named(&r, fixture).params[0].ty,
            r.resolve(),
            &mut names,
            r.aliases(),
        )
        .expect_err(&format!("`{fixture}` must bail at plan-build"));
        let msg = err.to_string();
        assert!(
            msg.contains("`list<T>` element type"),
            "`{fixture}`: expected list-element gate phrase, got: {msg}"
        );
    }
}

#[test]
fn map_classifies_as_list_of_tuple() {
    // `map<string, u32>` desugars to `list<tuple<string, u32>>`.
    // Element plan: Text(slots 0..2) + IntegerZeroExt(slot 2)
    // + TupleOf(children=[0, 1]). Outer: ListOf(ptr/len). Same
    // shape as `f-list-tuple-u32-string` mod K/V order.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-map-string-u32", &r, &mut names);
    let Cell::ListOf { element_plan, .. } = &plan.cells[0] else {
        panic!("expected ListOf at cell 0, got {:?}", plan.cells[0]);
    };
    assert_eq!(element_plan.cells.len(), 3);
    assert!(matches!(
        element_plan.cells[0],
        Cell::Text {
            ptr_slot: 0,
            len_slot: 1
        }
    ));
    assert!(matches!(
        element_plan.cells[1],
        Cell::IntegerZeroExt { flat_slot: 2 }
    ));
    let Cell::TupleOf { children } = &element_plan.cells[2] else {
        panic!(
            "expected TupleOf at element-plan cell 2, got {:?}",
            element_plan.cells[2]
        );
    };
    assert_eq!(children.as_slice(), &[0, 1]);
    // Outer flat: (ptr, len) — same as `list<T>`.
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn map_in_record_classifies_with_inner_list() {
    // record map-pair { primary: map<string, u32>, secondary: map<u32, string> }
    // Each field expands to its own ListOf with an internal TupleOf.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-record-with-map", &r, &mut names);
    assert!(matches!(plan.cells[0], Cell::ListOf { .. }));
    assert!(matches!(plan.cells[1], Cell::ListOf { .. }));
    let Cell::RecordOf { fields, .. } = &plan.cells[2] else {
        panic!("expected RecordOf at cell 2, got {:?}", plan.cells[2]);
    };
    assert_eq!(
        fields.iter().map(|(_, idx)| *idx).collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[test]
fn map_with_nested_list_value_bails() {
    // `map<string, list<list<u32>>>` desugars to
    // `list<tuple<string, list<list<u32>>>>`. The inner `list<list<u32>>`
    // can plan on its own (scope of this commit), but once it sits as
    // a tuple field, the tuple becomes a list element whose nested
    // sub-list is rejected by the depth-2 cap (the outer `Cell::ListOf`
    // inside the tuple has class `None` because its inner cell is
    // itself `PrestagedNestedList`).
    let (r, mut names) = setup();
    let err = LiftPlan::for_type(
        &func_named(&r, "f-map-of-list-of-list").params[0].ty,
        r.resolve(),
        &mut names,
        r.aliases(),
    )
    .expect_err("map<_, list<list<T>>> must bail at plan build");
    let msg = err.to_string();
    assert!(
        msg.contains("`list<T>` element type"),
        "expected list-element gate message, got: {msg}"
    );
    assert!(
        msg.contains("further nesting"),
        "expected the depth-cap phrase from the new gate, got: {msg}"
    );
}

#[test]
fn map_through_type_alias_classifies() {
    // `type aliased-map = map<string, u32>;` + `func(m: aliased-map)`:
    // `push` peels the alias via `TypeDefKind::Type(t)` and lands on
    // the underlying Map typeid, which `desugar_map_aliases` has
    // already registered. Pins that aliasing doesn't bypass the
    // alias-map registration.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-aliased-map", &r, &mut names);
    let Cell::ListOf { element_plan, .. } = &plan.cells[0] else {
        panic!(
            "aliased map must classify as ListOf at cell 0, got {:?}",
            plan.cells[0]
        );
    };
    assert!(matches!(
        element_plan.cells.last(),
        Some(Cell::TupleOf { .. })
    ));
}

#[test]
fn list_of_variant_classifies_with_perivariant_element() {
    // list<shape>: variant elements are now supported. The element
    // plan carries one `Cell::Variant` after its primitive disc/payload
    // cells (children-first), and `has_list_elem_variant()` flips
    // true so the wrapper alloc routes through the runtime-sized
    // variant-info branch.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-of-variant", &r, &mut names);
    let Cell::ListOf { element_plan, .. } = &plan.cells[0] else {
        panic!("expected ListOf at cell 0, got {:?}", plan.cells[0]);
    };
    assert!(matches!(
        element_plan.cells.last(),
        Some(Cell::Variant { .. })
    ));
    assert!(plan.has_list_elem_variant());
}

#[test]
fn list_of_record_classifies_with_perirecord_element() {
    // list<point>: record elements are now supported. The element
    // plan carries one `Cell::RecordOf` after its primitive field
    // cells (children-first), and `has_list_elem_record()` flips
    // true so the wrapper alloc routes through the runtime-sized
    // record-info branch.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-of-record", &r, &mut names);
    let Cell::ListOf { element_plan, .. } = &plan.cells[0] else {
        panic!("expected ListOf at cell 0, got {:?}", plan.cells[0]);
    };
    assert!(matches!(
        element_plan.cells.last(),
        Some(Cell::RecordOf { .. })
    ));
    assert!(plan.has_list_elem_record());
}

#[test]
fn list_inside_result_arm_carries_disc_guards() {
    // result<list<u32>, list<u32>>: each arm's list snapshots the
    // outer disc — ok-arm guard expects 0, err-arm 1.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-result-list-list", &r, &mut names);
    assert_eq!(plan.cells.len(), 3);
    let Cell::ListOf {
        arm_guards: ok_guards,
        ..
    } = &plan.cells[0]
    else {
        panic!("expected ok arm Cell::ListOf at 0");
    };
    let Cell::ListOf {
        arm_guards: err_guards,
        ..
    } = &plan.cells[1]
    else {
        panic!("expected err arm Cell::ListOf at 1");
    };
    assert!(matches!(plan.cells[2], Cell::Result { .. }));
    assert_eq!(
        ok_guards.as_slice(),
        &[ArmGuard {
            disc_slot: 0,
            expected_disc: 0,
        }]
    );
    assert_eq!(
        err_guards.as_slice(),
        &[ArmGuard {
            disc_slot: 0,
            expected_disc: 1,
        }]
    );
}

#[test]
fn list_inside_variant_arm_carries_case_disc_guard() {
    // variant list-or-int { with-list(list<u32>), plain(u32) }:
    // case 0 carries a guard expecting disc=0; case 1 has no list.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-variant-list-arm", &r, &mut names);
    let Cell::ListOf { arm_guards, .. } = &plan.cells[0] else {
        panic!("expected Cell::ListOf at 0");
    };
    assert_eq!(
        arm_guards.as_slice(),
        &[ArmGuard {
            disc_slot: 0,
            expected_disc: 0,
        }]
    );
}

#[test]
fn list_inside_nested_arms_stacks_guards_outer_to_inner() {
    // result<variant { with-list(list<u32>), plain(u32) }, u32>:
    // outer disc at slot 0, inner disc inside the ok arm at slot 1,
    // list-of inside `with-list` payload. Stack must be outer→inner.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-result-of-variant-with-list", &r, &mut names);
    let Cell::ListOf { arm_guards, .. } = &plan.cells[0] else {
        panic!("expected Cell::ListOf at 0, got {:?}", plan.cells[0]);
    };
    assert_eq!(
        arm_guards.as_slice(),
        &[
            ArmGuard {
                disc_slot: 0,
                expected_disc: 0,
            },
            ArmGuard {
                disc_slot: 1,
                expected_disc: 0,
            },
        ],
    );
}

/// Assert each `Cell::ListOf` carries exactly one `ArmGuard` per
/// `Result` / `Variant` ancestor (option/record/tuple don't count —
/// their slots aren't joined). Pins the joined-arm rule.
fn assert_arm_guards_match_joined_ancestry(plan: &LiftPlan) {
    fn walk(plan: &LiftPlan, idx: u32, depth: usize) {
        match &plan.cells[idx as usize] {
            Cell::Result {
                ok_idx, err_idx, ..
            } => {
                if let Some(i) = ok_idx {
                    walk(plan, *i, depth + 1);
                }
                if let Some(i) = err_idx {
                    walk(plan, *i, depth + 1);
                }
            }
            Cell::Variant {
                per_case_payload, ..
            } => {
                for slot in per_case_payload.iter().flatten() {
                    walk(plan, *slot, depth + 1);
                }
            }
            Cell::Option { child_idx, .. } => walk(plan, *child_idx, depth),
            Cell::RecordOf { fields, .. } => {
                for (_, i) in fields {
                    walk(plan, *i, depth);
                }
            }
            Cell::TupleOf { children } => {
                for i in children {
                    walk(plan, *i, depth);
                }
            }
            Cell::ListOf {
                arm_guards,
                element_plan,
                ..
            } => {
                assert_eq!(
                    arm_guards.len(),
                    depth,
                    "ListOf at cell {idx} has {} guards, expected {depth} for joined-arm ancestry",
                    arm_guards.len(),
                );
                assert_arm_guards_match_joined_ancestry(element_plan);
            }
            Cell::Bool { .. }
            | Cell::IntegerSignExt { .. }
            | Cell::IntegerZeroExt { .. }
            | Cell::Integer64 { .. }
            | Cell::FloatingF32 { .. }
            | Cell::FloatingF64 { .. }
            | Cell::Text { .. }
            | Cell::Bytes { .. }
            | Cell::Char { .. }
            | Cell::EnumCase { .. }
            | Cell::Flags { .. }
            | Cell::Handle { .. } => {}
        }
    }
    walk(plan, plan.root(), 0);
}

#[test]
fn arm_guards_match_joined_arm_ancestry_for_every_list_fixture() {
    // Structural invariant on every fixture that puts a `list<T>`
    // somewhere reachable: guard count == joined-arm ancestor count.
    // Catches future regressions in `push_arm` / `push_list_of` /
    // `push_result` / `push_variant`.
    let (r, mut names) = setup();
    let plans = [
        plan_for_param("f-list-u32", &r, &mut names),
        plan_for_param("f-list-string", &r, &mut names),
        plan_for_param("f-list-char", &r, &mut names),
        plan_for_param("f-list-option-u32", &r, &mut names),
        plan_for_param("f-list-result-u32-string", &r, &mut names),
        plan_for_param("f-list-tuple-u32-string", &r, &mut names),
        plan_for_param("f-list-tuple-of-tuple", &r, &mut names),
        plan_for_param("f-list-handle-own", &r, &mut names),
        plan_for_param("f-list-handle-borrow", &r, &mut names),
        plan_for_param("f-list-error-context", &r, &mut names),
        plan_for_param("f-list-flags", &r, &mut names),
        plan_for_param("f-list-tuple-of-flags", &r, &mut names),
        plan_for_param("f-list-tuple-mixed-flags", &r, &mut names),
        plan_for_param("f-list-tuple-of-handles", &r, &mut names),
        plan_for_named("list-pair", &r, &mut names),
        plan_for_named("list-char-pair", &r, &mut names),
        plan_for_param("f-option-list", &r, &mut names),
        plan_for_param("f-result-list-list", &r, &mut names),
        plan_for_param("f-variant-list-arm", &r, &mut names),
        plan_for_param("f-result-of-variant-with-list", &r, &mut names),
        // Mixed: `func(top: own<R>, xs: list<own<R>>)`. params[0] is
        // a static handle (no list, no guards); params[1] is the
        // list<own<R>> we actually want to pin.
        plan_for(
            &func_named(&r, "f-handles-mixed-with-list").params[1].ty,
            &r,
            &mut names,
        ),
    ];
    for plan in &plans {
        assert_arm_guards_match_joined_ancestry(plan);
    }
}

#[test]
fn list_inside_option_is_allowed() {
    // Option's payload slots are dedicated, not joined — guard must
    // not fire here.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-option-list", &r, &mut names);
    assert_eq!(plan.cells.len(), 2);
    let Cell::ListOf { arm_guards, .. } = &plan.cells[0] else {
        panic!("expected Cell::ListOf at 0");
    };
    assert!(
        arm_guards.is_empty(),
        "option's payload slots aren't joined; ListOf should carry no guards"
    );
    assert!(matches!(plan.cells[1], Cell::Option { .. }));
}

#[test]
fn record_with_list_field_recurses_into_list() {
    // list-pair { items: list<string>, scores: list<u32> }
    //   items   → cell 0 (ListOf, slots 0..1)
    //   scores  → cell 1 (ListOf, slots 2..3)
    //   parent  → cell 2 (RecordOf items=0, scores=1)
    let (r, mut names) = setup();
    let plan = plan_for_named("list-pair", &r, &mut names);
    assert_eq!(plan.cells.len(), 3);
    assert!(matches!(plan.cells[0], Cell::ListOf { .. }));
    assert!(matches!(plan.cells[1], Cell::ListOf { .. }));
    assert_eq!(
        plan.cells[2],
        record_of(&mut names, "list-pair", &[("items", 0), ("scores", 1)]),
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
    let (r, mut names) = setup();
    let plan = plan_for_named("perms-pair", &r, &mut names);
    assert_plan(
        &plan,
        vec![
            flags_cell(&mut names, 0, "fperms", &["read", "write", "exec"]),
            flags_cell(&mut names, 1, "fperms", &["read", "write", "exec"]),
            record_of(
                &mut names,
                "perms-pair",
                &[("primary", 0), ("secondary", 1)],
            ),
        ],
        2,
        2,
    );
}

#[test]
fn record_lays_children_before_parent() {
    let (r, mut names) = setup();
    let plan = plan_for_named("point", &r, &mut names);
    // Children-first: u32 + s32 land at indices 0 and 1, the parent
    // RecordOf is appended last and references them. Plan.root
    // points at the parent's cell index (2), not at cells[0].
    assert_plan(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            record_of(&mut names, "point", &[("x", 0), ("y", 1)]),
        ],
        2,
        2,
    );
}

#[test]
fn nested_record_walks_depth_first() {
    let (r, mut names) = setup();
    let plan = plan_for_named("nested", &r, &mut names);
    // Depth-first, children-before-parent: the inner `point`'s
    // primitive children land at 0/1, then `point`'s parent at 2,
    // then the `color` enum at 3, then the outer `nested` parent at
    // 4. plan.root() is the outer parent.
    assert_plan(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            record_of(&mut names, "point", &[("x", 0), ("y", 1)]),
            enum_cell(&mut names, 2, "color", &["red", "green", "blue"]),
            record_of(&mut names, "nested", &[("p", 2), ("c", 3)]),
        ],
        4,
        3,
    );
}

#[test]
fn tuple_lays_children_before_parent() {
    // tuple<u8, s32>: u8 → cell 0, s32 → cell 1, TupleOf parent → cell 2.
    // Plan-relative flat slots: u8 slot 0, s32 slot 1, parent consumes none.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-tuple", &r, &mut names);
    assert_plan(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            Cell::TupleOf {
                children: vec![0, 1],
            },
        ],
        2,
        2,
    );
}

#[test]
fn nested_tuple_walks_depth_first() {
    // tuple<u8, tuple<s32, s32>>:
    //   u8     → cell 0 (slot 0)
    //   s32    → cell 1 (slot 1)
    //   s32    → cell 2 (slot 2)
    //   inner  → cell 3 (children=[1, 2])
    //   outer  → cell 4 (children=[0, 3])
    let (r, mut names) = setup();
    let plan = plan_for_param("f-tuple-of-tuple", &r, &mut names);
    assert_plan(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            Cell::IntegerSignExt { flat_slot: 2 },
            Cell::TupleOf {
                children: vec![1, 2],
            },
            Cell::TupleOf {
                children: vec![0, 3],
            },
        ],
        4,
        3,
    );
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
    let (r, mut names) = setup();
    let plan = plan_for_named("point-and-tuple", &r, &mut names);
    assert_plan(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            record_of(&mut names, "point", &[("x", 0), ("y", 1)]),
            Cell::IntegerZeroExt { flat_slot: 2 },
            Cell::IntegerSignExt { flat_slot: 3 },
            Cell::TupleOf {
                children: vec![3, 4],
            },
            record_of(&mut names, "point-and-tuple", &[("p", 2), ("t", 5)]),
        ],
        6,
        4,
    );
}

#[test]
fn fixed_length_list_desugars_to_tuple_of() {
    // list<u32, 3>: three u32 leaves at slots 0..3, TupleOf parent at
    // cell 3. Same plan shape as `tuple<u32, u32, u32>`.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-fixed-list-u32", &r, &mut names);
    assert_plan(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::IntegerZeroExt { flat_slot: 2 },
            Cell::TupleOf {
                children: vec![0, 1, 2],
            },
        ],
        3,
        3,
    );
}

#[test]
fn record_with_fixed_length_list_field_recurses_into_tuple() {
    // point-and-fixed-list { p: point, xs: list<u32, 3> }
    //   p.x → cell 0 (slot 0), p.y → cell 1 (slot 1), point → cell 2
    //   xs.0 → cell 3 (slot 2), xs.1 → cell 4 (slot 3),
    //   xs.2 → cell 5 (slot 4), xs (TupleOf) → cell 6
    //   record-of → cell 7
    let (r, mut names) = setup();
    let plan = plan_for_named("point-and-fixed-list", &r, &mut names);
    assert_plan(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerSignExt { flat_slot: 1 },
            record_of(&mut names, "point", &[("x", 0), ("y", 1)]),
            Cell::IntegerZeroExt { flat_slot: 2 },
            Cell::IntegerZeroExt { flat_slot: 3 },
            Cell::IntegerZeroExt { flat_slot: 4 },
            Cell::TupleOf {
                children: vec![3, 4, 5],
            },
            record_of(&mut names, "point-and-fixed-list", &[("p", 2), ("xs", 6)]),
        ],
        7,
        5,
    );
}

#[test]
fn list_of_fixed_length_list_routes_through_list_element_tuple() {
    // list<list<u32, 3>>: outer ListOf with element_plan = [u32, u32,
    // u32, TupleOf]. The element-plan TupleOf is allowed_as_list_element
    // via the existing `PrestagedTupleIndices` class — no new arm
    // needed.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-of-fixed-list", &r, &mut names);
    let Cell::ListOf { element_plan, .. } = &plan.cells[0] else {
        panic!("expected ListOf at cell 0, got {:?}", plan.cells[0]);
    };
    assert_eq!(element_plan.cells.len(), 4);
    assert!(matches!(
        element_plan.cells[0..3],
        [
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::IntegerZeroExt { flat_slot: 2 },
        ],
    ));
    let Cell::TupleOf { children } = &element_plan.cells[3] else {
        panic!(
            "expected TupleOf at element-plan cell 3, got {:?}",
            element_plan.cells[3]
        );
    };
    assert_eq!(children.as_slice(), &[0, 1, 2]);
    // Outer flat: (ptr, len) — same as `list<T>`.
    assert_eq!(plan.flat_slot_count, 2);
}

#[test]
fn fixed_length_list_n_one_builds_single_child_tuple_of() {
    // list<u32, 1>: WIT forbids tuple<T> (1-tuples), so a single-child
    // TupleOf is unique to FixedLengthList. Pins that the desugar path
    // doesn't accidentally peel through the parent when N==1.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-fixed-list-u32-one", &r, &mut names);
    assert_plan(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::TupleOf { children: vec![0] },
        ],
        1,
        1,
    );
}

#[test]
fn fixed_length_list_n_zero_bails_at_plan_build() {
    // WIT grammar parses `list<u32, 0>`; canon-ABI gives it no meaning.
    // Plan-builder bails rather than building an empty `Cell::TupleOf`
    // that the existing `push_tuple` invariant would reject.
    let (r, mut names) = setup();
    let err = LiftPlan::for_type(
        &func_named(&r, "f-fixed-list-u32-zero").params[0].ty,
        r.resolve(),
        &mut names,
        r.aliases(),
    )
    .expect_err("N == 0 must bail");
    let msg = err.to_string();
    assert!(msg.contains("requires N >= 1"), "got: {msg}");
    assert!(msg.contains("github.com/ejrgilbert/splicer/issues"));
}

#[test]
fn fixed_length_list_n_over_budget_bails_at_plan_build() {
    // Pre-bail catches N over `MAX_FLAT_SLOTS_PER_FN` / `MAX_CELLS_PER_PARAM`
    // before `bump_flat_slot`'s checked_add can overflow. Test fixture
    // uses N=17 which exceeds the cfg(test) budget of 16.
    let (r, mut names) = setup();
    let err = LiftPlan::for_type(
        &func_named(&r, "f-fixed-list-u32-huge").params[0].ty,
        r.resolve(),
        &mut names,
        r.aliases(),
    )
    .expect_err("N over budget must bail");
    let msg = err.to_string();
    assert!(msg.contains("exceeds tier-2 layout budget"), "got: {msg}");
    assert!(msg.contains("github.com/ejrgilbert/splicer/issues"));
}

#[test]
fn fixed_length_list_of_fixed_length_list_walks_depth_first() {
    // list<list<u32, 2>, 3>: inner list<u32, 2> → 2 u32 + TupleOf
    // (3 cells, 2 flat slots). Outer wraps 3 inners + parent TupleOf:
    //   inner[0] children → cells 0..2, TupleOf → cell 2
    //   inner[1] children → cells 3..5, TupleOf → cell 5
    //   inner[2] children → cells 6..8, TupleOf → cell 8
    //   outer TupleOf      → cell 9 (children=[2, 5, 8])
    let (r, mut names) = setup();
    let plan = plan_for_param("f-fixed-list-of-fixed-list", &r, &mut names);
    assert_plan(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 0 },
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::TupleOf {
                children: vec![0, 1],
            },
            Cell::IntegerZeroExt { flat_slot: 2 },
            Cell::IntegerZeroExt { flat_slot: 3 },
            Cell::TupleOf {
                children: vec![3, 4],
            },
            Cell::IntegerZeroExt { flat_slot: 4 },
            Cell::IntegerZeroExt { flat_slot: 5 },
            Cell::TupleOf {
                children: vec![6, 7],
            },
            Cell::TupleOf {
                children: vec![2, 5, 8],
            },
        ],
        9,
        6,
    );
}

#[test]
fn option_allocates_disc_before_inner() {
    // option<u32>: disc i32 → slot 0, inner u32 → slot 1.
    // Cell order is children-before-parent, so the IntegerZeroExt for
    // the inner u32 lands at cell 0 (with flat_slot=1) and the Option
    // parent at cell 1 (with disc_slot=0).
    let (r, mut names) = setup();
    let plan = plan_for_param("f-option-u32", &r, &mut names);
    assert_plan(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::Option {
                disc_slot: 0,
                child_idx: 0,
            },
        ],
        1,
        2,
    );
}

#[test]
fn option_of_string_keeps_canonical_disc_first() {
    // option<string>: [disc i32, ptr i32, len i32] in canonical-ABI
    // order. Plan-builder bumps disc first (slot 0), then string's
    // (ptr=1, len=2). Cell ordering still places the leaf first.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-option-string", &r, &mut names);
    assert_plan_no_root(
        &plan,
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
        3,
    );
}

#[test]
fn nested_option_walks_disc_per_layer() {
    // option<option<u32>>: outer disc → slot 0, inner disc → slot 1,
    // u32 → slot 2. Cell order: leaf u32 (cell 0), inner Option (cell
    // 1, disc=1, child=0), outer Option (cell 2, disc=0, child=1).
    let (r, mut names) = setup();
    let plan = plan_for_param("f-option-option", &r, &mut names);
    assert_plan(
        &plan,
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
        2,
        3,
    );
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
    let (r, mut names) = setup();
    let plan = plan_for_named("point-and-option", &r, &mut names);
    assert_plan(
        &plan,
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
        5,
        4,
    );
}

#[test]
fn result_u32_string_shares_arms_flat_slots() {
    // result<u32, string>: joined flat = [i32 disc, i32, i32].
    // Ok=u32 claims slot 1; Err=string claims slots 1, 2 (sharing
    // slot 1 via the save-and-restore cursor). Cell order:
    //   IntegerZeroExt {flat_slot:1}  // ok arm leaf
    //   Text {ptr:1, len:2}           // err arm leaf
    //   Result { disc:0, ok:Some(0), err:Some(1) }
    let (r, mut names) = setup();
    let plan = plan_for_param("f-result-u32-string", &r, &mut names);
    assert_plan(
        &plan,
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
        2,
        3,
    );
}

#[test]
fn result_u32_u64_records_joined_flat_widening() {
    // result<u32, u64>: ok flat = [I32], err flat = [I64]; joined =
    // [I32 disc, I64 (= max width)]. Ok arm's leaf reads slot 1
    // expecting I32 — emit must bitcast I64→I32. Err arm matches
    // joined; no bitcast.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-result-u32-u64", &r, &mut names);
    assert_plan_no_root(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::Integer64 { flat_slot: 1 },
            Cell::Result {
                disc_slot: 0,
                ok_idx: Some(0),
                err_idx: Some(1),
            },
        ],
        2,
    );
    // Disc never widens (joined position 0 is always I32).
    assert!(plan.widening_for(0).is_none());
    assert_eq!(plan.widening_for(1), Some(WasmType::I64));
}

#[test]
fn variant_f32_widen_records_f32_arm_widening() {
    // Regression: pre-refactor, F32-arm widening panicked in
    // pin_leaf_flat ("F32 widening must use push_widened_get inline")
    // because there was no F32 scratch local. `lcl.widen_f32` lifts
    // that. variant { a(f32), b(u64) }: joined = [I32 disc, I64];
    // arm a's F32 leaf at slot 1 widens, arm b matches.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-variant-f32-widen", &r, &mut names);
    assert_eq!(plan.flat_slot_count, 2);
    assert!(plan.widening_for(0).is_none());
    assert_eq!(plan.widening_for(1), Some(WasmType::I64));
    // Emit must not panic — exercised by
    // `emit_lift_plan_validates_every_classify_built_shape`.
}

#[test]
fn variant_tri_arm_records_joined_flat_widening() {
    // variant tri-arm { a(u32), b(u64), c(f64) }:
    // a → [I32], b → [I64], c → [F64]; joined = [I32 disc, I64
    // (max width across arms)]. a + c widen (I32 / F64 vs I64),
    // b matches — slot_widening[1] is recorded once (idempotent
    // across arms; joined is structural).
    let (r, mut names) = setup();
    let plan = plan_for_param("f-variant-tri-arm", &r, &mut names);
    assert_eq!(plan.flat_slot_count, 2);
    assert!(plan.widening_for(0).is_none());
    assert_eq!(plan.widening_for(1), Some(WasmType::I64));
}

#[test]
fn result_u32_string_records_no_widening() {
    // result<u32, string>: ok flat = [I32], err flat = [I32, I32];
    // joined = [I32, I32, I32]. Every arm position matches the
    // joined wasm type → no widening recorded.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-result-u32-string", &r, &mut names);
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
    let (r, mut names) = setup();
    let plan = plan_for_param("f-result-unit-err", &r, &mut names);
    assert_plan_no_root(
        &plan,
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
        3,
    );
}

#[test]
fn result_unit_err_skips_err_child() {
    // result<u32>: only Ok arm has a payload. ok_idx=Some(0),
    // err_idx=None. Total 2 slots (disc + u32).
    let (r, mut names) = setup();
    let plan = plan_for_param("f-result-ok-unit", &r, &mut names);
    assert_plan_no_root(
        &plan,
        vec![
            Cell::IntegerZeroExt { flat_slot: 1 },
            Cell::Result {
                disc_slot: 0,
                ok_idx: Some(0),
                err_idx: None,
            },
        ],
        2,
    );
}

#[test]
fn result_both_unit_is_disc_only() {
    // result<_, _>: both arms unit. Just the disc, no children.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-result-both-unit", &r, &mut names);
    assert_plan(
        &plan,
        vec![Cell::Result {
            disc_slot: 0,
            ok_idx: None,
            err_idx: None,
        }],
        0,
        1,
    );
}

#[test]
fn classify_func_params_yields_plan_relative_slots() {
    // f-mixed(a: bool, s: string, b: list<u8>, x: s64): each
    // param's plan is plan-relative, not threaded with cumulative
    // cursor. b's bytes cell holds slots (0, 1) regardless of its
    // absolute wasm-local position (3, 4) in the wrapper. Pins
    // the local-base-independence invariant.
    let (r, mut names) = setup();
    let params = classify_func_params(
        r.resolve(),
        func_named(&r, "f-mixed"),
        &mut names,
        r.aliases(),
    )
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
    let (r, mut names) = setup();
    let params = classify_func_params(
        r.resolve(),
        func_named(&r, "f-mixed"),
        &mut names,
        r.aliases(),
    )
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
    use super::sidetable::char_info::char_scratch_sizes;
    let (r, mut names) = setup();
    let mut fd = func_with_params(&r, &mut names, &[]);
    fd.result_ty = Some(Type::Char);
    fd.result_lift = Some(ResultLift {
        source: ResultSource::Direct(Cell::Char { flat_slot: 0 }),
    });
    // 1 char-result × 4 bytes scratch.
    assert_eq!(char_scratch_sizes(&[fd]), vec![MAX_UTF8_LEN]);
}

#[test]
fn flags_scratch_sizes_count_both_param_and_result_cells() {
    // Regression: `flags_scratch_sizes` must walk per-fn params AND
    // the compound result plan, in the order `build_flags_info_maps`
    // consumes addresses — otherwise a record-result-with-flags
    // crashes the builder's `scratch_addrs.next()` expect.
    use super::classify::CompoundResult;
    use super::sidetable::flags_info::flags_scratch_sizes;
    let (r, mut names) = setup();
    let fd_param = func_with_params(&r, &mut names, &["fperms"]);
    let mut fd_result = func_with_params(&r, &mut names, &[]);
    fd_result.result_lift = Some(ResultLift {
        source: ResultSource::Compound(CompoundResult {
            ty: type_named(&r, "perms-pair"),
            plan: plan_for_named("perms-pair", &r, &mut names),
        }),
    });
    // 1 flags param + 2 flags inside the record result → 3 slabs of
    // 3 flags × 8 bytes each.
    assert_eq!(
        flags_scratch_sizes(&[fd_param, fd_result]),
        vec![24, 24, 24]
    );
}

// ─── Side-table N=1 invariant ─────────────────────────────────

#[test]
#[should_panic(expected = "≤1 EnumCase per range")]
fn build_enum_info_blob_panics_on_multi_enum_plan() {
    // Pin the structural N=1 invariant in `build_enum_info_blob`:
    // a record with two distinct enum fields (`two-enums { c: color,
    // m: mood }`) produces two `Cell::EnumCase` cells in one plan,
    // which the per-cell `cell::enum-case(disc)` payload encoding
    // can't disambiguate within a single per-(fn, param) range.
    // Documented in build_enum_info_blob's doc; this test fails the
    // build the moment a future change makes the plan-builder
    // produce multi-enum plans without first introducing a per-cell
    // side-table-idx scheme.
    use super::sidetable::enum_info::build_enum_info_blob;
    let (r, mut names) = setup();
    let fd = func_with_params(&r, &mut names, &["two-enums"]);
    let entry_layout = synth_info_layout("enum-info");
    let segment_id = super::super::blob::SymbolBases::new().alloc();
    let _ = build_enum_info_blob(&[fd], &entry_layout, segment_id);
}

// ─── Side-table dedup ─────────────────────────────────────────

#[test]
fn enum_strings_dedup_across_funcs() {
    // Enum type-name + case-names are interned at plan-build via the
    // shared NameInterner. Two funcs sharing the same enum type
    // produce one copy of every string — dedup is the interner's job.
    // Plan-build interns in WIT order: type-name first, then cases in
    // declaration order. A single `color` enum used twice → exactly
    // `colorredgreenblue` (substring matching would false-positive on
    // a future case named e.g. "reduce" containing "red").
    let (r, mut names) = setup();
    func_with_params(&r, &mut names, &["color"]);
    func_with_params(&r, &mut names, &["color"]);
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
    let (r, mut names) = setup();
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
fn build_record_info_maps_assigns_per_param_counts_and_cell_idx() {
    // 2 funcs, 3 params total — exactly the audit's request.
    // f-point(p: point):                 1 RecordOf cell
    // f-mix-records(p: point, n: nested): 1 + 2 RecordOf cells
    use super::sidetable::record_info::RecordSlotSource;
    let (r, mut names) = setup();
    let funcs = vec![
        func_with_params(&r, &mut names, &["point"]),
        func_with_params(&r, &mut names, &["point", "nested"]),
    ];
    let (_, tuple) = synth_record_info_layouts(r.resolve());
    let maps = build_record_info_maps(&funcs, &tuple, 0);

    // Per-(fn, param) counts. New cases drop in here.
    assert_eq!(maps.per_param_count, vec![vec![1], vec![1, 2]]);

    // Cell-idx maps reset per range — entry_idx counts up only inside
    // one (fn, param), not across them. Children-first plan order
    // puts each RecordOf cell after its descendants, so the
    // `Some(_)` slots land at the *end* of each map (and, for
    // `nested`, the inner `point` parent picks up entry_idx 0
    // before the outer `nested` parent picks up entry_idx 1).
    let expected: Vec<Vec<&[Option<u32>]>> = vec![
        vec![&[None, None, Some(0)]],
        vec![
            &[None, None, Some(0)],
            &[None, None, Some(0), None, Some(1)],
        ],
    ];
    for (fn_idx, fn_expected) in expected.iter().enumerate() {
        for (param_idx, param_expected) in fn_expected.iter().enumerate() {
            let actual: Vec<Option<u32>> = maps
                .per_cell_fill
                .for_param(fn_idx, param_idx)
                .iter()
                .map(|f| {
                    f.as_ref().map(|fill| match fill.slot_source {
                        RecordSlotSource::Static { entry_idx, .. } => entry_idx,
                        RecordSlotSource::PerIteration { .. } => {
                            unreachable!("outer-plan walk produces only Static fills")
                        }
                    })
                })
                .collect();
            assert_eq!(
                actual.as_slice(),
                *param_expected,
                "fn {fn_idx} param {param_idx}",
            );
        }
    }

    // Pin the tuples-segment size — a `record_field_tuple_layout` drift
    // (e.g. struct-field reorder, or string-flat-bytes change) shows up
    // as a wrong segment size here.
    //
    // 4 RecordOf cells × 2 fields each = 8 `(name, idx)` tuples:
    //   fn-0 p: point          → 1 record × 2 fields = 2
    //   fn-1 p: point          → 1 record × 2 fields = 2
    //   fn-1 n: nested         → 2 records × 2 fields = 4 (inner point + outer nested)
    assert_eq!(maps.tuples.bytes.len() as u32, 8 * tuple.size);
}

// ─── emit_lift_plan round-trip through validator ──────────────

#[test]
fn emit_lift_plan_validates_every_classify_built_shape() {
    // Every wired Cell variant: classify a fixture WIT type, emit,
    // validate. Adding a new kind = adding a row.
    let (r, mut names) = setup();
    let plans = [
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
        plan_for_param("f-tuple", &r, &mut names),
        plan_for_param("f-tuple-of-tuple", &r, &mut names),
        plan_for_named("point-and-tuple", &r, &mut names),
        plan_for_param("f-option-u32", &r, &mut names),
        plan_for_param("f-option-string", &r, &mut names),
        plan_for_param("f-option-option", &r, &mut names),
        plan_for_named("point-and-option", &r, &mut names),
        plan_for_param("f-result-u32-string", &r, &mut names),
        plan_for_param("f-result-unit-err", &r, &mut names),
        plan_for_param("f-result-ok-unit", &r, &mut names),
        plan_for_param("f-result-both-unit", &r, &mut names),
        plan_for_param("f-result-u32-u64", &r, &mut names),
        plan_for_param("f-variant-tri-arm", &r, &mut names),
        // F32-arm widening: drives `lcl.widen_f32` through emit; a
        // regression that re-introduces the F32 panic surfaces here.
        plan_for_param("f-variant-f32-widen", &r, &mut names),
        plan_for_param("f-handle-own", &r, &mut names),
        plan_for_param("f-handle-borrow", &r, &mut names),
        plan_for_named("handle-pair", &r, &mut names),
        plan_for_param("f-stream-u32", &r, &mut names),
        plan_for_param("f-future-string", &r, &mut names),
        plan_for_param("f-stream-of-res", &r, &mut names),
        plan_for_named("stream-pair", &r, &mut names),
        plan_for_param("f-error-context", &r, &mut names),
        plan_for_param("f-result-with-err-ctx", &r, &mut names),
        // Scalar-element lists + char (per-iteration utf-8 scratch) +
        // option/result (cell-payload runtime child idx) + tuple
        // (per-iteration tuple-idx buffer). Element kinds with
        // static side-table records still gated.
        plan_for_param("f-list-u32", &r, &mut names),
        plan_for_param("f-list-string", &r, &mut names),
        plan_for_param("f-list-char", &r, &mut names),
        plan_for_param("f-list-option-u32", &r, &mut names),
        plan_for_param("f-list-option-string", &r, &mut names),
        // Nested option in a list element: two cells reuse the
        // shared `list_elem_child_idx` staging local sequentially.
        plan_for_param("f-list-option-option-u32", &r, &mut names),
        plan_for_param("f-list-result-u32-string", &r, &mut names),
        plan_for_param("f-list-result-unit-string", &r, &mut names),
        plan_for_param("f-list-tuple-u32-u32", &r, &mut names),
        plan_for_param("f-list-tuple-u32-string", &r, &mut names),
        // Nested tuple element: two TupleOf cells in element_plan,
        // their per-iteration slots living adjacent in the per-call
        // tuple-idx buffer. Pins multi-tuple per-element offset math.
        plan_for_param("f-list-tuple-of-tuple", &r, &mut names),
        // List-element handles: per-call handle-info buffer grown
        // at runtime; per-iteration `list_elem_handle_base` stages
        // the slot index. Covers all 5 handle kinds via the shared
        // codegen — error-context routes through the same path.
        plan_for_param("f-list-handle-own", &r, &mut names),
        plan_for_param("f-list-handle-borrow", &r, &mut names),
        plan_for_param("f-list-error-context", &r, &mut names),
        // Multi-handle element: tuple<own, borrow> — exercises
        // `handles_per_elem > 1` and `offset_in_elem` math in both
        // `emit_handle_runtime_fill` and `stage_handle_cell_payload`.
        plan_for_param("f-list-tuple-of-handles", &r, &mut names),
        // List-element flags: per-call flags-info buffer + variable-
        // stride per-list scratch slab. `f-list-tuple-of-flags`
        // additionally pins `flags_per_elem > 1`; `mixed-flags`
        // (fperms 3 bits + fcaps 5 bits) pins cumulative
        // `scratch_offset_in_elem` math against a uniform-stride
        // regression.
        plan_for_param("f-list-flags", &r, &mut names),
        plan_for_param("f-list-tuple-of-flags", &r, &mut names),
        plan_for_param("f-list-tuple-mixed-flags", &r, &mut names),
        // List-element records: per-call record-info buffer + per-call
        // field-tuples buffer (cell-idxs runtime-resolved off
        // `elem_cell_base`). Pins emit_record_runtime_fill's
        // PerIteration arm + the cell::record-of payload staging.
        plan_for_param("f-list-of-record", &r, &mut names),
        // Mixed-field-count record tuple element — pins
        // `records_per_elem > 1` + cumulative `tuples_offset_in_elem`
        // for the multi-record case (point=2 fields, solo=1 field).
        plan_for_param("f-list-tuple-record-mixed", &r, &mut names),
        // List-element variants: per-call variant-info buffer; the
        // N-way disc dispatch writes case-name + payload using the
        // per-iter staged slot address; payload `child_idx` resolves
        // to `elem_cell_base + child_pos` at runtime.
        plan_for_param("f-list-of-variant", &r, &mut names),
        // Multi-variant element — pins `variants_per_elem > 1` +
        // cumulative `entry_offset_in_elem`.
        plan_for_param("f-list-tuple-of-variants", &r, &mut names),
        // `func(top: own<R>, xs: list<own<R>>)` params[1] — mixed
        // wrapper with both a static handle (top) and a
        // list-element handle (xs). Pins the
        // `static_count → next_handle_idx` seed interaction.
        plan_for(
            &func_named(&r, "f-handles-mixed-with-list").params[1].ty,
            &r,
            &mut names,
        ),
        plan_for_named("list-pair", &r, &mut names),
        plan_for_named("list-char-pair", &r, &mut names),
        // `map<K, V>` desugars to `list<tuple<K, V>>`; this pins that
        // the emit path inherited from list-of-tuple stays wired when
        // the element type comes from a synthetic tuple typedef
        // (alloced by `desugar_map_aliases`, not an original WIT type).
        plan_for_param("f-map-string-u32", &r, &mut names),
        plan_for_named("map-pair", &r, &mut names),
        // Lists nested in joined arms — guards on the bump pre-pass
        // and on `emit_list_of_arm` body. Last entry stacks to depth 2.
        plan_for_param("f-result-list-list", &r, &mut names),
        plan_for_param("f-variant-list-arm", &r, &mut names),
        plan_for_param("f-result-of-variant-with-list", &r, &mut names),
        // `list<list<T>>` (depth-2 nested-list) — inner T per supported
        // class. Drives the nested pre-pass memory walk + the
        // recursive `emit_list_of_arm` per outer iteration.
        plan_for_param("f-list-of-list", &r, &mut names),
        plan_for_param("f-list-of-list-string", &r, &mut names),
        plan_for_param("f-list-of-list-char", &r, &mut names),
        plan_for_param("f-list-of-list-option-u32", &r, &mut names),
        plan_for_param("f-list-of-list-result-u32-string", &r, &mut names),
        plan_for_param("f-list-of-list-tuple-u32-u32", &r, &mut names),
        plan_for_param("f-list-of-list-color", &r, &mut names),
    ];
    for plan in &plans {
        validate_emit_lift_plan(plan, r.resolve());
    }
}

// ─── List-of emit codegen (scalar elements) ──────────────────

#[test]
fn list_of_u32_emits_valid_wasm() {
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-u32", &r, &mut names);
    validate_emit_lift_plan(&plan, r.resolve());
}

#[test]
fn list_of_string_emits_valid_wasm() {
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-string", &r, &mut names);
    validate_emit_lift_plan(&plan, r.resolve());
}

#[test]
fn list_of_option_u32_emits_valid_wasm() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-list-option-u32").params[0].ty,
        &r,
        &mut names,
    );
    validate_emit_lift_plan(&plan, r.resolve());
}

#[test]
fn list_of_result_u32_string_emits_valid_wasm() {
    let r = test_resolve();
    let mut names = NameInterner::new();
    let plan = plan_for(
        &func_named(&r, "f-list-result-u32-string").params[0].ty,
        &r,
        &mut names,
    );
    validate_emit_lift_plan(&plan, r.resolve());
}

#[test]
fn list_of_tuple_u32_string_emits_valid_wasm() {
    // Multi-cell tuple element with mixed flat shapes (Int + Text).
    // Exercises the per-list tuple-idx buffer realloc + per-iter
    // slot staging + the `PerIteration` tuple-of branch.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-tuple-u32-string", &r, &mut names);
    validate_emit_lift_plan(&plan, r.resolve());
}

#[test]
fn list_of_tuple_of_tuple_emits_valid_wasm() {
    // Nested tuple in a list element: two TupleOf cells per
    // iteration, distinct `offset_in_elem` slots in the per-call
    // tuple-idx buffer. Pins the multi-tuple offset accounting.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-tuple-of-tuple", &r, &mut names);
    validate_emit_lift_plan(&plan, r.resolve());
}

#[test]
fn list_of_char_emits_valid_wasm() {
    // Single-cell `Cell::Char` element — exercises the per-list
    // utf-8 scratch realloc, the per-iteration scratch-addr stage,
    // and the `Prestaged` CellSideData::Char branch.
    let (r, mut names) = setup();
    let plan = plan_for_param("f-list-char", &r, &mut names);
    validate_emit_lift_plan(&plan, r.resolve());
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
    // Empty map-alias map is produced via the standard desugar pass
    // (no Map typedefs in this fixture) — keeps `MapAliases` un-
    // constructible without going through the canonical route.
    let aliases = desugar_map_aliases(&mut r);
    let iface = super::super::test_utils::iface_by_unversioned_qname(&r, "test:list-enum/t");
    let func_id = r.interfaces[iface]
        .functions
        .keys()
        .find(|n| *n == "f")
        .unwrap()
        .clone();
    let func = &r.interfaces[iface].functions[&func_id];
    let mut names = NameInterner::new();
    let plan = LiftPlan::for_type(&func.params[0].ty, &r, &mut names, &aliases)
        .expect("list<color> must classify");
    validate_emit_lift_plan(&plan, &r);
}

#[test]
fn list_result_classifies_as_compound() {
    // `list<T>` (non-u8) results route through retptr + Compound; the
    // produced plan has the same structural shape as a param-side
    // `list<T>` plan (validate-emit coverage already pins emit shape).
    let (r, mut names) = setup();
    let func = func_named(&r, "f-result-list-u32");
    let result_lift = classify_result_lift(r.resolve(), func, true, &mut names, r.aliases())
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
fn map_result_classifies_as_compound() {
    // `map<K, V>` results route through retptr + Compound; the
    // desugared element plan exposes the same `Cell::ListOf` shape
    // as a `list<tuple<K, V>>` result. Pins `is_compound_result`
    // accepting the Map kind.
    let (r, mut names) = setup();
    let func = func_named(&r, "f-result-map");
    let result_lift = classify_result_lift(r.resolve(), func, true, &mut names, r.aliases())
        .expect("map<string,u32> result must classify")
        .expect("map<string,u32> result must produce a ResultLift");
    let compound = result_lift
        .compound()
        .expect("map<string,u32> result must route through Compound");
    let Cell::ListOf { element_plan, .. } = &compound.plan.cells[0] else {
        panic!(
            "Compound map-result root must be ListOf, got {:?}",
            compound.plan.cells[0]
        );
    };
    assert!(matches!(
        element_plan.cells.last(),
        Some(Cell::TupleOf { .. })
    ));
}

#[test]
fn fixed_length_list_result_classifies_as_compound() {
    // `list<T, N>` results route through retptr + Compound; the
    // desugared plan exposes a `Cell::TupleOf` root. Pins that
    // `is_compound_result` accepts the FixedLengthList kind — without
    // this arm the wrapper would silently drop the result.
    let (r, mut names) = setup();
    let func = func_named(&r, "f-result-fixed-list-u32");
    let result_lift = classify_result_lift(r.resolve(), func, true, &mut names, r.aliases())
        .expect("list<u32, 3> result must classify")
        .expect("list<u32, 3> result must produce a ResultLift");
    let compound = result_lift
        .compound()
        .expect("list<u32, 3> result must route through Compound");
    let Cell::TupleOf { children } = compound.plan.cells.last().expect("non-empty plan") else {
        panic!(
            "Compound fixed-list-result root must be TupleOf, got {:?}",
            compound.plan.cells.last()
        );
    };
    assert_eq!(children.as_slice(), &[0, 1, 2]);
}

#[test]
fn list_char_result_classifies_as_compound() {
    // `list<char>` results take the same Compound route as `list<u32>`;
    // the result-side emit shares `emit_list_of_arm` with params, so
    // per-iteration utf-8 scratch + Prestaged CharScratch wire up
    // identically. Pin the shape so a future result-side regression
    // surfaces here.
    let r = test_resolve();
    let mut names = NameInterner::new();
    let func = func_named(&r, "f-result-list-char");
    let result_lift = classify_result_lift(r.resolve(), func, true, &mut names, r.aliases())
        .expect("list<char> result must classify")
        .expect("list<char> result must produce a ResultLift");
    let compound = result_lift
        .compound()
        .expect("list<char> result must route through Compound");
    let Cell::ListOf { element_plan, .. } = &compound.plan.cells[0] else {
        panic!(
            "Compound result root must be ListOf, got {:?}",
            compound.plan.cells[0]
        );
    };
    assert_eq!(element_plan.cells, vec![Cell::Char { flat_slot: 0 }]);
}

#[test]
fn record_with_list_field_emits_valid_wasm() {
    let (r, mut names) = setup();
    let plan = plan_for_named("list-pair", &r, &mut names);
    validate_emit_lift_plan(&plan, r.resolve());
}
