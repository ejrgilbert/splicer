//! Cell-emit smoke + WIT-discriminant pinning. Each helper emits a
//! one-function wasm module and round-trips it through wasmparser to
//! confirm the bytecode parses and validates.

use super::super::cells::{
    CellLayout, PayloadSource, EXPECTED_CELL_CASES,
};
use wasm_encoder::{
    CodeSection, EntityType, Function, FunctionSection, ImportSection, MemoryType, Module,
    TypeSection, ValType,
};
use wit_parser::{Resolve, SizeAlign};

/// Build a minimal wasm module with one function body from
/// `emit_body` and round-trip it through wasmparser. Structural
/// smoke test only — runtime correctness is the fuzz harness's job.
fn build_and_validate(param_types: &[ValType], emit_body: impl FnOnce(&mut Function)) {
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
    let mut f = Function::new([]);
    emit_body(&mut f);
    f.instructions().end();
    code.function(&f);
    module.section(&code);

    let bytes = module.finish();
    wasmparser::Validator::new()
        .validate_all(&bytes)
        .expect("emitted module should validate");
}

/// Synthetic `CellLayout` (size=16, align=8, payload_offset=8)
/// for structural tests that lack a `Resolve`.
fn synth_cell_layout() -> CellLayout {
    CellLayout {
        size: 16,
        align: 8,
        payload_offset: 8,
        discs: EXPECTED_CELL_CASES
            .iter()
            .enumerate()
            .map(|(i, name)| ((*name).to_string(), i as u8))
            .collect(),
    }
}

#[test]
fn bool_cell_emits_valid_wasm() {
    // params: (addr_local: i32, payload_local: i32)
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32, ValType::I32], |f| cl.emit_bool(f, 0, 1));
}

#[test]
fn integer_cell_emits_valid_wasm() {
    // params: (addr_local: i32, payload_local: i64)
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32, ValType::I64], |f| cl.emit_integer(f, 0, 1));
}

#[test]
fn floating_cell_emits_valid_wasm() {
    // params: (addr_local: i32, payload_local: f64)
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32, ValType::F64], |f| cl.emit_floating(f, 0, 1));
}

#[test]
fn text_cell_emits_valid_wasm() {
    // params: (addr_local: i32, ptr_local: i32, len_local: i32)
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32, ValType::I32, ValType::I32], |f| {
        cl.emit_text(f, 0, 1, 2)
    });
}

#[test]
fn bytes_cell_emits_valid_wasm() {
    // params: (addr_local: i32, ptr_local: i32, len_local: i32)
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32, ValType::I32, ValType::I32], |f| {
        cl.emit_bytes(f, 0, 1, 2)
    });
}

#[test]
fn tuple_of_cell_emits_valid_wasm() {
    // Static path: indices off is an i32.const, no locals beyond addr.
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32], |f| {
        cl.emit_tuple_of(f, 0, PayloadSource::ConstI32(0x100), 3)
    });
}

#[test]
fn tuple_of_cell_with_local_off_emits_valid_wasm() {
    // List-element path: caller stages per-iteration slot ptr into
    // a local and passes PayloadSource::Local.
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32, ValType::I32], |f| {
        cl.emit_tuple_of(f, 0, PayloadSource::Local(1), 3)
    });
}

#[test]
fn option_some_cell_emits_valid_wasm() {
    // params: (addr_local: i32). inner_idx is an i32.const.
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32], |f| {
        cl.emit_option_some(f, 0, PayloadSource::ConstI32(7))
    });
}

#[test]
fn option_some_cell_with_local_idx_emits_valid_wasm() {
    // List-element shape: caller stages runtime idx into a local
    // and passes PayloadSource::Local.
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32, ValType::I32], |f| {
        cl.emit_option_some(f, 0, PayloadSource::Local(1))
    });
}

#[test]
fn option_none_cell_emits_valid_wasm() {
    // params: (addr_local: i32). disc-only, no payload writes.
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32], |f| cl.emit_option_none(f, 0));
}

#[test]
fn result_ok_with_payload_emits_valid_wasm() {
    // params: (addr_local: i32). option<u32> payload (disc=1, idx).
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32], |f| {
        cl.emit_result_ok(f, 0, true, PayloadSource::ConstI32(5))
    });
}

#[test]
fn result_ok_unit_emits_valid_wasm() {
    // params: (addr_local: i32). option<u32> payload (disc=0).
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32], |f| {
        cl.emit_result_ok(f, 0, false, PayloadSource::ConstI32(0))
    });
}

#[test]
fn result_err_with_payload_emits_valid_wasm() {
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32], |f| {
        cl.emit_result_err(f, 0, true, PayloadSource::ConstI32(7))
    });
}

#[test]
fn result_err_unit_emits_valid_wasm() {
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32], |f| {
        cl.emit_result_err(f, 0, false, PayloadSource::ConstI32(0))
    });
}

#[test]
fn flags_set_cell_emits_valid_wasm() {
    // params: (addr_local: i32). side_table_idx is i32.const, no
    // additional locals.
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32], |f| {
        cl.emit_flags_set(f, 0, PayloadSource::ConstI32(11))
    });
}

#[test]
fn variant_case_cell_emits_valid_wasm() {
    // params: (addr_local: i32). side_table_idx is i32.const.
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32], |f| {
        cl.emit_variant_case(f, 0, PayloadSource::ConstI32(5))
    });
}

#[test]
fn char_cell_emits_valid_wasm() {
    // params: (addr_local: i32, code_point: i32, len_local: i32,
    // scratch_addr_local: i32). Caller seeds the scratch local
    // with a mid-page constant before the helper runs so the utf-8
    // stores land in valid memory.
    let cl = synth_cell_layout();
    build_and_validate(
        &[ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        |f| {
            f.instructions().i32_const(0x1000);
            f.instructions().local_set(3);
            cl.emit_char(f, 0, 1, 3, 2)
        },
    );
}

#[test]
fn handle_cells_emit_valid_wasm() {
    // Static path: side_table_idx is an i32.const. All four
    // handle disc-cases share the same body — exercise each so
    // a disc-name typo surfaces here.
    let cl = synth_cell_layout();
    for disc_case in [
        "resource-handle",
        "stream-handle",
        "future-handle",
        "error-context-handle",
    ] {
        build_and_validate(&[ValType::I32], |f| {
            cl.emit_handle_cell(f, 0, disc_case, PayloadSource::ConstI32(9))
        });
    }
}

#[test]
fn handle_cell_with_local_idx_emits_valid_wasm() {
    // List-element path: caller stages runtime idx into a local.
    let cl = synth_cell_layout();
    build_and_validate(&[ValType::I32, ValType::I32], |f| {
        cl.emit_handle_cell(f, 0, "resource-handle", PayloadSource::Local(1))
    });
}

#[test]
fn enum_case_with_zero_offset_skips_add() {
    // Sole enum in its range → `entry_offset == 0`; the
    // `LocalPlusConst` short-circuit elides `i32.const 0 + i32.add`.
    let cl = synth_cell_layout();
    let baseline = capture_function_body(&[ValType::I32, ValType::I32], |f| {
        cl.emit_enum_case(f, 0, 1, 0);
    });
    // Validate too.
    build_and_validate(&[ValType::I32, ValType::I32], |f| {
        cl.emit_enum_case(f, 0, 1, 0)
    });
    assert!(
        !baseline.iter().any(|i| matches!(i, OpKind::I32Add)),
        "offset=0 must not emit i32.add, found in {baseline:?}",
    );
}

#[test]
fn enum_case_with_nonzero_offset_emits_add() {
    // Non-first enum in a multi-enum range → `disc + entry_offset`
    // must materialize as `i32.const offset; i32.add` before the store.
    let cl = synth_cell_layout();
    let body = capture_function_body(&[ValType::I32, ValType::I32], |f| {
        cl.emit_enum_case(f, 0, 1, 5);
    });
    build_and_validate(&[ValType::I32, ValType::I32], |f| {
        cl.emit_enum_case(f, 0, 1, 5)
    });
    let consts: Vec<i32> = body
        .iter()
        .filter_map(|i| match i {
            OpKind::I32Const(v) => Some(*v),
            _ => None,
        })
        .collect();
    assert!(consts.contains(&5), "expected i32.const 5 in {body:?}");
    assert_eq!(
        body.iter().filter(|i| matches!(i, OpKind::I32Add)).count(),
        1,
        "expected exactly one i32.add in {body:?}",
    );
}

/// Minimal operator tag for inspecting emitted bytes. Tracks only
/// the ops the enum-case tests assert on; other ops collapse into
/// `Other` to keep the matcher tiny.
#[derive(Debug, PartialEq, Eq)]
enum OpKind {
    I32Const(i32),
    I32Add,
    Other,
}

/// Encode a one-function module via `build_and_validate`'s pipeline
/// but, instead of validating, decode the code section to the tag
/// list above. Lets tests assert on emitted instructions without
/// pulling in a full disassembler.
fn capture_function_body(
    param_types: &[ValType],
    emit_body: impl FnOnce(&mut Function),
) -> Vec<OpKind> {
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
    let mut f = Function::new([]);
    emit_body(&mut f);
    f.instructions().end();
    code.function(&f);
    module.section(&code);

    let bytes = module.finish();
    let mut ops = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&bytes) {
        if let wasmparser::Payload::CodeSectionEntry(body) =
            payload.expect("emitted module parses")
        {
            for op in body
                .get_operators_reader()
                .expect("body has an operators reader")
            {
                let op = op.expect("operator decodes");
                ops.push(match op {
                    wasmparser::Operator::I32Const { value } => OpKind::I32Const(value),
                    wasmparser::Operator::I32Add => OpKind::I32Add,
                    _ => OpKind::Other,
                });
            }
        }
    }
    ops
}

/// Structural fuzz over primitive cell-emit helpers — picks a
/// helper at random per iteration and validates the bytecode.
#[test]
fn primitive_cells_structural_fuzz() {
    let seed: u64 = std::env::var("SPLICER_TIER2_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xC0FF_EE00_DEAD_BEEF);

    let cl = synth_cell_layout();
    for iter in 0..100u64 {
        let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(iter);
        match mixed % 5 {
            0 => build_and_validate(&[ValType::I32, ValType::I32], |f| cl.emit_bool(f, 0, 1)),
            1 => build_and_validate(&[ValType::I32, ValType::I64], |f| cl.emit_integer(f, 0, 1)),
            2 => build_and_validate(&[ValType::I32, ValType::F64], |f| cl.emit_floating(f, 0, 1)),
            3 => build_and_validate(&[ValType::I32, ValType::I32, ValType::I32], |f| {
                cl.emit_text(f, 0, 1, 2)
            }),
            4 => build_and_validate(&[ValType::I32, ValType::I32, ValType::I32], |f| {
                cl.emit_bytes(f, 0, 1, 2)
            }),
            _ => unreachable!(),
        }
    }
}

#[test]
fn cell_discriminants_match_wit_declaration_order() {
    // Pin disc numbering against live WIT so a reorder/rename/remove
    // fires here before lift codegen miscompiles to a wrong case.
    let common_wit = include_str!("../../../../wit/common/world.wit");
    let mut resolve = Resolve::new();
    resolve
        .push_str("common.wit", common_wit)
        .expect("wit/common/world.wit must parse");
    let iface_id =
        super::super::test_utils::iface_by_unversioned_qname(&resolve, "splicer:common/types");
    let cell_id = resolve.interfaces[iface_id]
        .types
        .get("cell")
        .copied()
        .expect("splicer:common/types must export `cell` typedef");
    let mut sizes = SizeAlign::default();
    sizes.fill(&resolve);
    let layout = CellLayout::from_resolve(&sizes, &resolve, cell_id);

    for (expected_disc, name) in EXPECTED_CELL_CASES.iter().enumerate() {
        assert_eq!(
            layout.disc_of(name),
            expected_disc as u8,
            "WIT case `{name}` no longer has disc {expected_disc} — \
             reorder/rename detected"
        );
    }
}
