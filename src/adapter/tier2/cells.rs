//! Cell-construction helpers for tier-2's lifted-value representation.
//!
//! Each primitive WIT type maps to one `cell` variant case (see
//! [`splicer:common/types`](../../../wit/common/world.wit) — the
//! `variant cell { ... }` declaration). This module emits the
//! canonical-ABI wasm that writes a single cell into linear memory at
//! a caller-supplied address.
//!
//! ## Memory layout (canonical ABI, computed by `wit-parser::SizeAlign`)
//!
//! A `cell` is a variant with 18 cases. The discriminant fits in a
//! `u8` (≤256 cases). Variant alignment = max alignment of all
//! payloads — `integer(s64)` forces alignment 8. Total cell size is
//! 8 (disc + padding) + max-payload-size (padded to 8).
//!
//! Every nominal-typed case (record-of, variant-case, handle, etc.)
//! is a `u32` index into a per-kind side table on `field-tree`, not
//! an inline payload. This caps the variant's max payload at 8 bytes
//! (`s64`), so `CELL_SIZE = 8 + 8 = 16` bytes. See
//! `docs/tiers/tier-2.md` for the design rationale (memory savings
//! of ~50% on primitive-dominated trees vs. an inline-metadata
//! layout). If a future cell case widens the max payload past 8
//! bytes, [`CELL_SIZE`] must update.
//!
//! ## Discriminant ordering
//!
//! The numeric discriminants below MUST stay in lockstep with the
//! `variant cell { ... }` declaration in `wit/common/world.wit`.
//! A test in [`tests`] pins this by re-decoding the published WIT
//! and asserting the case ordering matches.
//!
//! ## Future optimization
//!
//! This file emits cells one-at-a-time into a `cabi_realloc`-grown
//! buffer for simplicity (see `docs/tiers/tier-2.md`). A two-pass
//! mode (pre-count + bulk-allocate) would avoid per-cell realloc
//! traffic; defer until benchmarks show it matters.

use std::collections::HashMap;

use wasm_encoder::{Function, MemArg};
use wit_parser::{Resolve, SizeAlign, Type, TypeId};

/// `cell` variant case names that the codegen knows how to emit, in
/// the order they appear in `wit/common/world.wit`. Used by
/// [`CellLayout::from_resolve`] to validate the WIT and the codegen
/// agree on the case set — a removal or rename in the WIT fires
/// loudly here rather than producing wasm that lies about disc values.
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
];

/// Schema-derived layout of the `cell` variant: total size,
/// alignment, the byte offset where each case's payload starts
/// (variants put all payloads at the same offset), and a map from WIT
/// case name to discriminant value. All emit helpers hang off this
/// struct so the canonical-ABI numbers — including the discriminant
/// ordering — are read from the live WIT once and never duplicated.
pub(crate) struct CellLayout {
    pub size: u32,
    pub align: u32,
    payload_offset: u64,
    discs: HashMap<String, u8>,
}

impl CellLayout {
    /// Compute the layout from `splicer:common/types.cell`. `cell_id`
    /// must point at the variant typedef.
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

    /// Look up the discriminant value for a `cell` case by its WIT
    /// case name (kebab-case, exactly as declared in
    /// `wit/common/world.wit`). Panics if `name` isn't a case;
    /// `from_resolve` validates the WIT against `EXPECTED_CELL_CASES`,
    /// so reaching the panic implies an emit-side typo.
    fn disc_of(&self, name: &str) -> u8 {
        *self
            .discs
            .get(name)
            .unwrap_or_else(|| panic!("CellLayout::disc_of: no `cell` case named `{name}`"))
    }
}

// ─── Primitive cell-emit helpers ──────────────────────────────────
//
// Each `emit_<kind>_cell` helper writes one cell into linear memory
// at the address held in `addr_local`, with case-specific payload
// values supplied in additional locals. After the helper returns,
// the cell has been written; the wasm value stack is unchanged.
//
// Helpers are one-liners over [`emit_cell`], which factors the
// disc-write + per-part payload-write loop. Each helper's only job
// is to declare its discriminant + a slice of [`PayloadPart`]s
// describing where each value goes inside the payload area.
//
// All locals must be allocated by the caller; helpers don't allocate.
// Callers also own cell-cursor advancement (incrementing the
// cells-array count + recomputing the next cell's address).
//
// Canonical-ABI doesn't require padding bytes between disc and
// payload (or unused payload bytes for narrow cases like `bool`)
// to be zeroed — readers gate on the discriminant.

/// Width of a single payload-part store. Each variant maps to
/// exactly one `wasm-encoder` store instruction; `natural_align`
/// returns the log2 alignment that store implicitly requires.
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
    /// Log2 alignment that the store requires. `MemArg::align` is in
    /// log2 form (so `2` means 4-byte alignment).
    fn natural_align(self) -> u32 {
        match self {
            StoreKind::I8 => 0,
            StoreKind::I32 => 2,
            StoreKind::I64 | StoreKind::F64 => 3,
        }
    }
}

/// Where one payload word's value comes from.
///
/// Most cells source from a wasm local holding the runtime-lifted
/// value (`Local`). A few cells — notably `record-of`, where the
/// side-table index is computed at adapter-build time — source from
/// an `i32.const` (`ConstI32`); pre-materializing the constant into
/// a wasm local first would just be wasted instructions.
#[derive(Clone, Copy)]
enum PayloadSource {
    Local(u32),
    ConstI32(i32),
}

/// One value to write into a cell's payload area.
///
/// Callers describe each cell as a list of these — the loop in
/// [`emit_cell`] does the rest. `offset` is relative to the start of
/// the payload area (i.e. the actual store happens at
/// `addr + PAYLOAD_OFFSET + offset`).
#[derive(Clone, Copy)]
struct PayloadPart {
    /// Source for the value being stored. Must already match the
    /// type `kind` expects — caller is responsible for any narrowing
    /// or extension before reaching here.
    source: PayloadSource,
    kind: StoreKind,
    /// Byte offset within the payload area.
    offset: u32,
}

impl CellLayout {
    /// Emit wasm that writes one cell at `addr_local`: a 1-byte
    /// discriminant at offset 0 followed by each `parts[i]` written
    /// into the payload area at its declared sub-offset.
    fn emit_cell(&self, f: &mut Function, addr_local: u32, disc: u8, parts: &[PayloadPart]) {
        // Discriminant byte at offset 0.
        f.instructions().local_get(addr_local);
        f.instructions().i32_const(disc as i32);
        f.instructions().i32_store8(MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        });
        // Payload parts.
        for part in parts {
            f.instructions().local_get(addr_local);
            match part.source {
                PayloadSource::Local(l) => {
                    f.instructions().local_get(l);
                }
                PayloadSource::ConstI32(c) => {
                    f.instructions().i32_const(c);
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

    /// Emit a single-payload primitive cell — body is identical
    /// across `bool` / `integer` / `floating`, only the case name and
    /// store width differ.
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

    /// `cell::bool(bool)` — `payload_local` is the i32 flat form (0 or 1).
    pub(crate) fn emit_bool(&self, f: &mut Function, addr_local: u32, payload_local: u32) {
        self.emit_single_payload(f, addr_local, "bool", StoreKind::I8, payload_local);
    }

    /// `cell::integer(s64)` — `payload_local` is i64, already widened
    /// from any narrower integer.
    pub(crate) fn emit_integer(&self, f: &mut Function, addr_local: u32, payload_local: u32) {
        self.emit_single_payload(f, addr_local, "integer", StoreKind::I64, payload_local);
    }

    /// `cell::floating(f64)` — `payload_local` is f64, already
    /// promoted from f32 if necessary.
    pub(crate) fn emit_floating(&self, f: &mut Function, addr_local: u32, payload_local: u32) {
        self.emit_single_payload(f, addr_local, "floating", StoreKind::F64, payload_local);
    }

    /// `cell::text(string)` — `(ptr, len)` pair pointing at utf-8.
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

    // ─── Un-wired stubs — codegen lives here when each kind lands ──
    //
    // Each stub names the cell variant + payload shape it'll produce
    // when implemented. They're unreachable until `tier2/lift::Cell`
    // for the matching compound type wires its `emit_cell_op` arm to
    // call them. Keeping the stubs in cells.rs documents the lowest-level contract:
    // "to lift a record, you need `cell_layout.emit_record_of(addr,
    // side_table_idx)` to write disc 11 + i32 at payload+0".

    /// `cell::char` — char's utf-8 encoding. char isn't a cell variant
    /// of its own; we encode the i32 code point as utf-8 bytes (1–4 of
    /// them) and emit the result as `cell::text`.
    #[allow(dead_code)]
    pub(crate) fn emit_char(&self, f: &mut Function, addr_local: u32, code_point_local: u32) {
        let _ = (f, addr_local, code_point_local);
        todo!(
            "utf-8-encode the i32 code point into a cabi_realloc'd \
             buffer, then emit_text(ptr, len)"
        );
    }

    /// `cell::list-of(list<u32>)` — payload is `(ptr, len)` of a
    /// child-cell-index array allocated upstream.
    #[allow(dead_code)]
    pub(crate) fn emit_list_of(
        &self,
        f: &mut Function,
        addr_local: u32,
        idx_array_ptr: u32,
        idx_array_len: u32,
    ) {
        let _ = (f, addr_local, idx_array_ptr, idx_array_len);
        todo!("cell::list-of — disc 5 + (ptr, len) at payload");
    }

    /// `cell::tuple-of(list<u32>)` — same flat shape as list-of.
    #[allow(dead_code)]
    pub(crate) fn emit_tuple_of(
        &self,
        f: &mut Function,
        addr_local: u32,
        idx_array_ptr: u32,
        idx_array_len: u32,
    ) {
        let _ = (f, addr_local, idx_array_ptr, idx_array_len);
        todo!("cell::tuple-of — disc 6 + (ptr, len) at payload");
    }

    /// `cell::option-some(u32)` — single inner cell index.
    #[allow(dead_code)]
    pub(crate) fn emit_option_some(&self, f: &mut Function, addr_local: u32, inner_idx: u32) {
        let _ = (f, addr_local, inner_idx);
        todo!("cell::option-some — disc 7 + i32 inner cell index at payload+0");
    }

    /// `cell::option-none` — no payload, just the discriminant.
    #[allow(dead_code)]
    pub(crate) fn emit_option_none(&self, f: &mut Function, addr_local: u32) {
        let _ = (f, addr_local);
        todo!("cell::option-none — disc 8, no payload");
    }

    /// `cell::result-ok(option<u32>)` — disc 9 + option<inner>.
    #[allow(dead_code)]
    pub(crate) fn emit_result_ok(
        &self,
        f: &mut Function,
        addr_local: u32,
        has_payload: bool,
        inner_idx: u32,
    ) {
        let _ = (f, addr_local, has_payload, inner_idx);
        todo!("cell::result-ok(option<u32>) — disc 9 + option<u32> payload");
    }

    /// `cell::result-err(option<u32>)` — disc 10 + option<inner>.
    #[allow(dead_code)]
    pub(crate) fn emit_result_err(
        &self,
        f: &mut Function,
        addr_local: u32,
        has_payload: bool,
        inner_idx: u32,
    ) {
        let _ = (f, addr_local, has_payload, inner_idx);
        todo!("cell::result-err(option<u32>) — disc 10 + option<u32> payload");
    }

    /// `cell::record-of(u32)` — index into `field-tree.record-infos`.
    /// The side-table index is adapter-build-time-known, so we emit it
    /// as an `i32.const` rather than a local-load.
    pub(crate) fn emit_record_of(&self, f: &mut Function, addr_local: u32, side_table_idx: u32) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of("record-of"),
            &[PayloadPart {
                source: PayloadSource::ConstI32(side_table_idx as i32),
                kind: StoreKind::I32,
                offset: 0,
            }],
        );
    }

    /// `cell::flags-set(u32)` — index into `field-tree.flags-infos`.
    #[allow(dead_code)]
    pub(crate) fn emit_flags_set(&self, f: &mut Function, addr_local: u32, side_table_idx: u32) {
        let _ = (f, addr_local, side_table_idx);
        todo!("cell::flags-set — disc 12 + i32 flags-info side-table index");
    }

    /// `cell::enum-case(u32)` — index into `field-tree.enum-infos`.
    /// Caller passes a local holding the side-table index (the runtime
    /// disc value, since enum-info entries are laid out per-case in
    /// disc order); we write disc 13 at offset 0 and the i32 index at
    /// the payload offset.
    pub(crate) fn emit_enum_case(&self, f: &mut Function, addr_local: u32, side_table_idx: u32) {
        self.emit_cell(
            f,
            addr_local,
            self.disc_of("enum-case"),
            &[PayloadPart {
                source: PayloadSource::Local(side_table_idx),
                kind: StoreKind::I32,
                offset: 0,
            }],
        );
    }

    /// `cell::variant-case(u32)` — index into `field-tree.variant-infos`.
    #[allow(dead_code)]
    pub(crate) fn emit_variant_case(&self, f: &mut Function, addr_local: u32, side_table_idx: u32) {
        let _ = (f, addr_local, side_table_idx);
        todo!("cell::variant-case — disc 14 + i32 variant-info side-table index");
    }

    /// `cell::resource-handle(u32)` — index into `field-tree.handle-infos`.
    #[allow(dead_code)]
    pub(crate) fn emit_resource_handle(
        &self,
        f: &mut Function,
        addr_local: u32,
        handle_info_idx: u32,
    ) {
        let _ = (f, addr_local, handle_info_idx);
        todo!("cell::resource-handle — disc 15 + i32 handle-info index");
    }

    /// `cell::stream-handle(u32)` — index into `field-tree.handle-infos`.
    #[allow(dead_code)]
    pub(crate) fn emit_stream_handle(
        &self,
        f: &mut Function,
        addr_local: u32,
        handle_info_idx: u32,
    ) {
        let _ = (f, addr_local, handle_info_idx);
        todo!("cell::stream-handle — disc 16 + i32 handle-info index");
    }

    /// `cell::future-handle(u32)` — index into `field-tree.handle-infos`.
    #[allow(dead_code)]
    pub(crate) fn emit_future_handle(
        &self,
        f: &mut Function,
        addr_local: u32,
        handle_info_idx: u32,
    ) {
        let _ = (f, addr_local, handle_info_idx);
        todo!("cell::future-handle — disc 17 + i32 handle-info index");
    }
}

/// Shared `(ptr, len)` payload layout used by `text` and `bytes`
/// (and, later, by any cell carrying a flat `list<T>` reference).
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

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_encoder::{
        CodeSection, EntityType, FunctionSection, ImportSection, MemoryType, Module, TypeSection,
        ValType,
    };

    /// Build a minimal wasm module containing one function whose body
    /// is whatever `emit_body` emits. Validates the produced bytes
    /// round-trip through wasmparser.
    ///
    /// `param_types` are the function's params (also become locals
    /// 0..n at the start of the function body — caller passes the
    /// matching local indices into the cell-emit helper).
    ///
    /// This is a structural smoke test — it confirms our emit doesn't
    /// produce ill-formed bytecode (alignments, store sizes, local
    /// indices in range). End-to-end "did the right value land in
    /// memory" coverage comes via the runtime fuzz harness.
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

    /// Synthetic `CellLayout` matching today's `cell` variant
    /// (size=16, align=8, payload_offset=8). The structural fuzz
    /// tests don't have a `Resolve` to derive from, so they pin the
    /// expected canonical-ABI numbers here. End-to-end "did the
    /// right value land in memory" coverage runs against the live
    /// schema-derived layout via `test_tier2_canned_primitives`.
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

    /// Structural fuzz over the primitive cell-emit helpers — for each
    /// random seed, pick a primitive at random, build a module that
    /// emits that cell, validate the bytecode. Catches regressions in
    /// alignment / store-size / local-index handling that single-shot
    /// unit tests might miss.
    ///
    /// Bounded iteration count keeps it fast under default `cargo
    /// test`. End-to-end "did the right value land in memory"
    /// coverage is the job of the e2e tier-2 fuzz harness (task #29),
    /// which runs the wasm under wasmtime.
    #[test]
    fn primitive_cells_structural_fuzz() {
        // Deterministic seed-derived bytes — re-seeded if a regression
        // bisects to a specific shape, run against a fresh seed.
        let seed: u64 = std::env::var("SPLICER_TIER2_FUZZ_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0xC0FF_EE00_DEAD_BEEF);

        // 5 primitive kinds × 100 iterations of random alignment of
        // helper choice. Cheap (each iter builds a tiny module).
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
        // Pin the discriminant numbering against the WIT cases listed
        // in `wit/common/world.wit`. Built by loading the live WIT
        // through `CellLayout::from_resolve`, so a reorder, rename, or
        // removal in the WIT fires here before lift codegen miscompiles
        // values into wrong cell cases.
        let common_wit = include_str!("../../../wit/common/world.wit");
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
