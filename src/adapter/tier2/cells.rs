//! Cell-construction helpers: emit canonical-ABI wasm that writes one
//! `cell` variant case (per `splicer:common/types`) into linear memory
//! at a caller-supplied address. Discriminant numbering must match the
//! `variant cell` declaration in `wit/common/world.wit` — pinned by
//! `cell_discriminants_match_wit_declaration_order` in [`tests`].

use std::collections::HashMap;

use wasm_encoder::{Function, MemArg};
use wit_parser::{Resolve, SizeAlign, Type, TypeId};

use super::super::abi::emit::{I8_STORE_LOG2_ALIGN, SLICE_LEN_OFFSET, SLICE_PTR_OFFSET};

/// `cell` variant case names in WIT-declaration order. Validates the
/// WIT and codegen agree on the case set.
const EXPECTED_CELL_CASES: &[&str] = &[
    "bool",
    "integer",
    "floating",
    "text",
    "bytes",
    "list-of",
    "tuple-of",
    "option-some",
    "option-none",
    "result-ok",
    "result-err",
    "record-of",
    "flags-set",
    "enum-case",
    "variant-case",
    "resource-handle",
    "stream-handle",
    "future-handle",
    "error-context-handle",
];

/// Schema-derived layout of the `cell` variant. Emit helpers hang off
/// this so canonical-ABI numbers (including disc ordering) are read
/// from the live WIT once and never duplicated.
pub(crate) struct CellLayout {
    pub size: u32,
    pub align: u32,
    payload_offset: u64,
    discs: HashMap<String, u8>,
}

impl CellLayout {
    /// Compute layout from `splicer:common/types.cell`.
    pub(crate) fn from_resolve(sizes: &SizeAlign, resolve: &Resolve, cell_id: TypeId) -> Self {
        use wit_parser::TypeDefKind;
        let typedef = &resolve.types[cell_id];
        let TypeDefKind::Variant(v) = &typedef.kind else {
            panic!("CellLayout::from_resolve: `cell` typedef is not a variant");
        };
        let size = sizes.size(&Type::Id(cell_id)).size_wasm32() as u32;
        let align = sizes.align(&Type::Id(cell_id)).align_wasm32() as u32;
        let payload_offset = sizes
            .payload_offset(v.tag(), v.cases.iter().map(|c| c.ty.as_ref()))
            .size_wasm32() as u64;
        let discs: HashMap<String, u8> = v
            .cases
            .iter()
            .enumerate()
            .map(|(i, c)| {
                assert!(
                    i < u8::MAX as usize,
                    "CellLayout::from_resolve: `cell` variant has more than 255 cases"
                );
                (c.name.clone(), i as u8)
            })
            .collect();
        for expected in EXPECTED_CELL_CASES {
            assert!(
                discs.contains_key(*expected),
                "CellLayout::from_resolve: `cell` variant in WIT is missing case `{expected}` \
                 (the codegen needs every case in EXPECTED_CELL_CASES — update both together)"
            );
        }
        assert_eq!(
            discs.len(),
            EXPECTED_CELL_CASES.len(),
            "CellLayout::from_resolve: `cell` variant has {} cases, codegen expects {}",
            discs.len(),
            EXPECTED_CELL_CASES.len()
        );
        Self {
            size,
            align,
            payload_offset,
            discs,
        }
    }

    /// Discriminant value for a `cell` case by kebab-case name.
    /// Panics on unknown name (implies an emit-side typo —
    /// `from_resolve` validates WIT against `EXPECTED_CELL_CASES`).
    fn disc_of(&self, name: &str) -> u8 {
        *self
            .discs
            .get(name)
            .unwrap_or_else(|| panic!("CellLayout::disc_of: no `cell` case named `{name}`"))
    }
}

// ─── Primitive cell-emit helpers ──────────────────────────────────
//
// Each `emit_<kind>_cell` writes one cell at `addr_local`. Helpers
// are one-liners over `emit_cell` (disc-write + per-part payload-
// write loop). Callers own locals + cell-cursor advancement.
// Canonical-ABI doesn't require padding / unused payload bytes to be
// zeroed — readers gate on the discriminant.

/// Width of a single payload-part store; one wasm-encoder store insn.
#[derive(Clone, Copy)]
enum StoreKind {
    /// `i32.store8` — 1 byte.
    I8,
    /// `i32.store` — 4 bytes (4-aligned).
    I32,
    /// `i64.store` — 8 bytes (8-aligned).
    I64,
    /// `f64.store` — 8 bytes (8-aligned).
    F64,
}

impl StoreKind {
    /// Log2 alignment (so `2` means 4-byte).
    fn natural_align(self) -> u32 {
        match self {
            StoreKind::I8 => 0,
            StoreKind::I32 => 2,
            StoreKind::I64 | StoreKind::F64 => 3,
        }
    }
}

/// Where one payload word's value comes from. `Local` for runtime-
/// lifted values; `ConstI32` for build-time-known indices (record-of,
/// static option/result child idx) — list-element emit reuses `Local`
/// for the per-iteration `elem_cell_base + relative_idx`.
/// `LocalPlusConst` adds a build-time offset to a runtime local
/// (enum-case shifts the raw disc by its `entry_offset`).
#[derive(Clone, Copy)]
pub(crate) enum PayloadSource {
    Local(u32),
    ConstI32(i32),
    LocalPlusConst { local: u32, offset: i32 },
}

/// One value to write into a cell's payload area. `offset` is
/// relative to the payload-area start.
#[derive(Clone, Copy)]
struct PayloadPart {
    source: PayloadSource,
    kind: StoreKind,
    offset: u32,
}

impl CellLayout {
    /// Emit wasm that writes one cell at `addr_local`: 1-byte disc at
    /// offset 0 + each `parts[i]` at its payload sub-offset.
    fn emit_cell(&self, f: &mut Function, addr_local: u32, disc: u8, parts: &[PayloadPart]) {
        f.instructions().local_get(addr_local);
        f.instructions().i32_const(disc as i32);
        f.instructions().i32_store8(MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        });
        for part in parts {
            f.instructions().local_get(addr_local);
            match part.source {
                PayloadSource::Local(l) => {
                    f.instructions().local_get(l);
                }
                PayloadSource::ConstI32(c) => {
                    f.instructions().i32_const(c);
                }
                PayloadSource::LocalPlusConst { local, offset } => {
                    f.instructions().local_get(local);
                    if offset != 0 {
                        f.instructions().i32_const(offset);
                        f.instructions().i32_add();
                    }
                }
            }
            let mem = MemArg {
                offset: self.payload_offset + part.offset as u64,
                align: part.kind.natural_align(),
                memory_index: 0,
            };
            match part.kind {
                StoreKind::I8 => f.instructions().i32_store8(mem),
                StoreKind::I32 => f.instructions().i32_store(mem),
                StoreKind::I64 => f.instructions().i64_store(mem),
                StoreKind::F64 => f.instructions().f64_store(mem),
            };
        }
    }

    /// Single-payload primitive cell (bool / integer / floating).
    fn emit_single_payload(
        &self,
        f: &mut Function,
        addr_local: u32,
        case: &str,
        kind: StoreKind,
        payload_local: u32,
    ) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of(case),
            &[PayloadPart {
                source: PayloadSource::Local(payload_local),
                kind,
                offset: 0,
            }],
        );
    }

    /// `cell::bool(bool)` — payload is i32 flat form (0 or 1).
    pub(crate) fn emit_bool(&self, f: &mut Function, addr_local: u32, payload_local: u32) {
        self.emit_single_payload(f, addr_local, "bool", StoreKind::I8, payload_local);
    }

    /// `cell::integer(s64)` — caller has widened narrower integers.
    pub(crate) fn emit_integer(&self, f: &mut Function, addr_local: u32, payload_local: u32) {
        self.emit_single_payload(f, addr_local, "integer", StoreKind::I64, payload_local);
    }

    /// `cell::floating(f64)` — caller has promoted f32.
    pub(crate) fn emit_floating(&self, f: &mut Function, addr_local: u32, payload_local: u32) {
        self.emit_single_payload(f, addr_local, "floating", StoreKind::F64, payload_local);
    }

    /// `cell::text(string)` — `(ptr, len)` of utf-8.
    pub(crate) fn emit_text(
        &self,
        f: &mut Function,
        addr_local: u32,
        ptr_local: u32,
        len_local: u32,
    ) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of("text"),
            &ptr_len_parts(ptr_local, len_local),
        );
    }

    /// `cell::bytes(list<u8>)` — same flat shape as text.
    pub(crate) fn emit_bytes(
        &self,
        f: &mut Function,
        addr_local: u32,
        ptr_local: u32,
        len_local: u32,
    ) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of("bytes"),
            &ptr_len_parts(ptr_local, len_local),
        );
    }

    // ─── Compound / structural cell emitters ───────────────────────

    /// `cell::text` for a `char` source. UTF-8 encodes the code point
    /// into the scratch buffer at `scratch_addr_local` (1–4 bytes),
    /// sets `len_local`, then writes `cell::text(scratch, len)`.
    pub(crate) fn emit_char(
        &self,
        f: &mut Function,
        addr_local: u32,
        code_point_local: u32,
        scratch_addr_local: u32,
        len_local: u32,
    ) {
        emit_utf8_encode(f, code_point_local, scratch_addr_local, len_local);
        self.emit_cell(
            f,
            addr_local,
            self.disc_of("text"),
            &[
                PayloadPart {
                    source: PayloadSource::Local(scratch_addr_local),
                    kind: StoreKind::I32,
                    offset: SLICE_PTR_OFFSET,
                },
                PayloadPart {
                    source: PayloadSource::Local(len_local),
                    kind: StoreKind::I32,
                    offset: SLICE_LEN_OFFSET,
                },
            ],
        );
    }

    /// `cell::list-of(list<u32>)` — `(ptr, len)` of a runtime child-idx array.
    pub(crate) fn emit_list_of(
        &self,
        f: &mut Function,
        addr_local: u32,
        idx_array_ptr: u32,
        idx_array_len: u32,
    ) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of("list-of"),
            &ptr_len_parts(idx_array_ptr, idx_array_len),
        );
    }

    /// `cell::tuple-of(list<u32>)` — `(ptr, len)` of a child-idx array.
    /// `indices_off_source`: `ConstI32` (static) or `Local` (list-element).
    pub(crate) fn emit_tuple_of(
        &self,
        f: &mut Function,
        addr_local: u32,
        indices_off_source: PayloadSource,
        indices_len: u32,
    ) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of("tuple-of"),
            &[
                PayloadPart {
                    source: indices_off_source,
                    kind: StoreKind::I32,
                    offset: 0,
                },
                PayloadPart {
                    source: PayloadSource::ConstI32(indices_len as i32),
                    kind: StoreKind::I32,
                    offset: 4,
                },
            ],
        );
    }

    /// `cell::option-some(u32)` — inner cell-array index.
    pub(crate) fn emit_option_some(
        &self,
        f: &mut Function,
        addr_local: u32,
        inner_idx_source: PayloadSource,
    ) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of("option-some"),
            &[PayloadPart {
                source: inner_idx_source,
                kind: StoreKind::I32,
                offset: 0,
            }],
        );
    }

    /// `cell::option-none` — disc only.
    pub(crate) fn emit_option_none(&self, f: &mut Function, addr_local: u32) {
        self.emit_cell(f, addr_local, self.disc_of("option-none"), &[]);
    }

    /// `cell::result-ok(option<u32>)`. `inner_idx_source` ignored when
    /// `has_payload` is false.
    pub(crate) fn emit_result_ok(
        &self,
        f: &mut Function,
        addr_local: u32,
        has_payload: bool,
        inner_idx_source: PayloadSource,
    ) {
        self.emit_result_arm(f, addr_local, "result-ok", has_payload, inner_idx_source);
    }

    /// `cell::result-err(option<u32>)`.
    pub(crate) fn emit_result_err(
        &self,
        f: &mut Function,
        addr_local: u32,
        has_payload: bool,
        inner_idx_source: PayloadSource,
    ) {
        self.emit_result_arm(f, addr_local, "result-err", has_payload, inner_idx_source);
    }

    /// Shared body for both result arms: cell disc + inline
    /// `option<u32>` (option-disc at +0, inner idx at +4 when payload).
    fn emit_result_arm(
        &self,
        f: &mut Function,
        addr_local: u32,
        case: &str,
        has_payload: bool,
        inner_idx_source: PayloadSource,
    ) {
        let mut parts = vec![PayloadPart {
            source: PayloadSource::ConstI32(if has_payload { 1 } else { 0 }),
            kind: StoreKind::I32,
            offset: 0,
        }];
        if has_payload {
            parts.push(PayloadPart {
                source: inner_idx_source,
                kind: StoreKind::I32,
                offset: 4,
            });
        }
        self.emit_cell(f, addr_local, self.disc_of(case), &parts);
    }

    /// `cell::record-of(u32)` — index into `field-tree.record-infos`.
    pub(crate) fn emit_record_of(&self, f: &mut Function, addr_local: u32, payload: PayloadSource) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of("record-of"),
            &[PayloadPart {
                source: payload,
                kind: StoreKind::I32,
                offset: 0,
            }],
        );
    }

    /// `cell::flags-set(u32)` — index into `field-tree.flags-infos`.
    pub(crate) fn emit_flags_set(&self, f: &mut Function, addr_local: u32, payload: PayloadSource) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of("flags-set"),
            &[PayloadPart {
                source: payload,
                kind: StoreKind::I32,
                offset: 0,
            }],
        );
    }

    /// `cell::enum-case(u32)` — index into `field-tree.enum-infos`.
    /// Payload is `disc + entry_offset`: `entry_offset` shifts past
    /// any earlier enums' case-name ranges in the per-(fn, param)
    /// slice. See [`super::lift::plan::Cell::EnumCase::entry_offset`].
    pub(crate) fn emit_enum_case(
        &self,
        f: &mut Function,
        addr_local: u32,
        source_local: u32,
        entry_offset: u32,
    ) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of("enum-case"),
            &[PayloadPart {
                source: PayloadSource::LocalPlusConst {
                    local: source_local,
                    offset: entry_offset as i32,
                },
                kind: StoreKind::I32,
                offset: 0,
            }],
        );
    }

    /// `cell::variant-case(u32)` — index into `field-tree.variant-infos`.
    /// The entry's `case-name` + `payload` are patched at runtime.
    pub(crate) fn emit_variant_case(
        &self,
        f: &mut Function,
        addr_local: u32,
        payload: PayloadSource,
    ) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of("variant-case"),
            &[PayloadPart {
                source: payload,
                kind: StoreKind::I32,
                offset: 0,
            }],
        );
    }

    /// `cell::{resource,stream,future,error-context}-handle(u32)` —
    /// index into `field-tree.handle-infos`. The entry's `id` (and
    /// possibly `type-name`) is filled by the wrapper.
    pub(crate) fn emit_handle_cell(
        &self,
        f: &mut Function,
        addr_local: u32,
        disc_case: &str,
        side_table_idx_source: PayloadSource,
    ) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of(disc_case),
            &[PayloadPart {
                source: side_table_idx_source,
                kind: StoreKind::I32,
                offset: 0,
            }],
        );
    }
}

/// Shared `(ptr, len)` payload layout for `text` / `bytes` / `list-of`.
fn ptr_len_parts(ptr_local: u32, len_local: u32) -> [PayloadPart; 2] {
    [
        PayloadPart {
            source: PayloadSource::Local(ptr_local),
            kind: StoreKind::I32,
            offset: 0,
        },
        PayloadPart {
            source: PayloadSource::Local(len_local),
            kind: StoreKind::I32,
            offset: 4,
        },
    ]
}

/// UTF-8 encode the code point at `code_point_local` into the
/// scratch buffer based at `scratch_addr_local`, storing the 1–4 byte
/// length in `len_local`. Caller reserves 4 bytes of scratch.
fn emit_utf8_encode(
    f: &mut Function,
    code_point_local: u32,
    scratch_addr_local: u32,
    len_local: u32,
) {
    use wasm_encoder::BlockType;
    let store_i8 = |off: u32| MemArg {
        offset: off as u64,
        align: I8_STORE_LOG2_ALIGN,
        memory_index: 0,
    };

    // 1B branch: cp < 0x80 → buf[0] = cp; len = 1.
    f.instructions().local_get(code_point_local);
    f.instructions().i32_const(0x80);
    f.instructions().i32_lt_u();
    f.instructions().if_(BlockType::Empty);
    f.instructions().local_get(scratch_addr_local);
    f.instructions().local_get(code_point_local);
    f.instructions().i32_store8(store_i8(0));
    f.instructions().i32_const(1);
    f.instructions().local_set(len_local);
    f.instructions().else_();

    // 2B branch: cp < 0x800.
    f.instructions().local_get(code_point_local);
    f.instructions().i32_const(0x800);
    f.instructions().i32_lt_u();
    f.instructions().if_(BlockType::Empty);
    // buf[0] = 0xC0 | (cp >> 6)
    f.instructions().local_get(scratch_addr_local);
    f.instructions().local_get(code_point_local);
    f.instructions().i32_const(6);
    f.instructions().i32_shr_u();
    f.instructions().i32_const(0xC0);
    f.instructions().i32_or();
    f.instructions().i32_store8(store_i8(0));
    // buf[1] = 0x80 | (cp & 0x3F)
    f.instructions().local_get(scratch_addr_local);
    f.instructions().local_get(code_point_local);
    f.instructions().i32_const(0x3F);
    f.instructions().i32_and();
    f.instructions().i32_const(0x80);
    f.instructions().i32_or();
    f.instructions().i32_store8(store_i8(1));
    f.instructions().i32_const(2);
    f.instructions().local_set(len_local);
    f.instructions().else_();

    // 3B branch: cp < 0x10000.
    f.instructions().local_get(code_point_local);
    f.instructions().i32_const(0x10000);
    f.instructions().i32_lt_u();
    f.instructions().if_(BlockType::Empty);
    // buf[0] = 0xE0 | (cp >> 12)
    f.instructions().local_get(scratch_addr_local);
    f.instructions().local_get(code_point_local);
    f.instructions().i32_const(12);
    f.instructions().i32_shr_u();
    f.instructions().i32_const(0xE0);
    f.instructions().i32_or();
    f.instructions().i32_store8(store_i8(0));
    // buf[1] = 0x80 | ((cp >> 6) & 0x3F)
    f.instructions().local_get(scratch_addr_local);
    f.instructions().local_get(code_point_local);
    f.instructions().i32_const(6);
    f.instructions().i32_shr_u();
    f.instructions().i32_const(0x3F);
    f.instructions().i32_and();
    f.instructions().i32_const(0x80);
    f.instructions().i32_or();
    f.instructions().i32_store8(store_i8(1));
    // buf[2] = 0x80 | (cp & 0x3F)
    f.instructions().local_get(scratch_addr_local);
    f.instructions().local_get(code_point_local);
    f.instructions().i32_const(0x3F);
    f.instructions().i32_and();
    f.instructions().i32_const(0x80);
    f.instructions().i32_or();
    f.instructions().i32_store8(store_i8(2));
    f.instructions().i32_const(3);
    f.instructions().local_set(len_local);
    f.instructions().else_();

    // 4B branch: cp ∈ 0x10000..=0x10FFFF (canonical-ABI guarantees
    // valid scalar, so no surrogate / out-of-range handling).
    // buf[0] = 0xF0 | (cp >> 18)
    f.instructions().local_get(scratch_addr_local);
    f.instructions().local_get(code_point_local);
    f.instructions().i32_const(18);
    f.instructions().i32_shr_u();
    f.instructions().i32_const(0xF0);
    f.instructions().i32_or();
    f.instructions().i32_store8(store_i8(0));
    // buf[1] = 0x80 | ((cp >> 12) & 0x3F)
    f.instructions().local_get(scratch_addr_local);
    f.instructions().local_get(code_point_local);
    f.instructions().i32_const(12);
    f.instructions().i32_shr_u();
    f.instructions().i32_const(0x3F);
    f.instructions().i32_and();
    f.instructions().i32_const(0x80);
    f.instructions().i32_or();
    f.instructions().i32_store8(store_i8(1));
    // buf[2] = 0x80 | ((cp >> 6) & 0x3F)
    f.instructions().local_get(scratch_addr_local);
    f.instructions().local_get(code_point_local);
    f.instructions().i32_const(6);
    f.instructions().i32_shr_u();
    f.instructions().i32_const(0x3F);
    f.instructions().i32_and();
    f.instructions().i32_const(0x80);
    f.instructions().i32_or();
    f.instructions().i32_store8(store_i8(2));
    // buf[3] = 0x80 | (cp & 0x3F)
    f.instructions().local_get(scratch_addr_local);
    f.instructions().local_get(code_point_local);
    f.instructions().i32_const(0x3F);
    f.instructions().i32_and();
    f.instructions().i32_const(0x80);
    f.instructions().i32_or();
    f.instructions().i32_store8(store_i8(3));
    f.instructions().i32_const(4);
    f.instructions().local_set(len_local);

    // Close 3 nested if/else blocks.
    f.instructions().end();
    f.instructions().end();
    f.instructions().end();
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_encoder::{
        CodeSection, EntityType, FunctionSection, ImportSection, MemoryType, Module, TypeSection,
        ValType,
    };

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
                1 => {
                    build_and_validate(&[ValType::I32, ValType::I64], |f| cl.emit_integer(f, 0, 1))
                }
                2 => {
                    build_and_validate(&[ValType::I32, ValType::F64], |f| cl.emit_floating(f, 0, 1))
                }
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
        let common_wit = include_str!("../../../wit/common/world.wit");
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
}
