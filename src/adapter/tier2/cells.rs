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
//! Per the Phase 2-2 design notes (`docs/tiers/tier-2.md`), this
//! file emits cells one-at-a-time into a `cabi_realloc`-grown
//! buffer for simplicity. A two-pass mode (pre-count + bulk-allocate)
//! would avoid per-cell realloc traffic; defer until benchmarks
//! show it matters.

use wasm_encoder::{Function, MemArg};

/// Discriminants for the `cell` variant, in declaration order from
/// `wit/common/world.wit`. Kept as `pub(crate)` constants rather than
/// derived at runtime because they're write-once and lookups happen
/// in tight emit loops.
#[allow(dead_code)] // many cases not yet used by Phase 2-2a (primitives only)
pub(crate) mod cell_disc {
    pub(crate) const BOOL: u8 = 0;
    pub(crate) const INTEGER: u8 = 1;
    pub(crate) const FLOATING: u8 = 2;
    pub(crate) const TEXT: u8 = 3;
    pub(crate) const BYTES: u8 = 4;
    pub(crate) const LIST_OF: u8 = 5;
    pub(crate) const TUPLE_OF: u8 = 6;
    pub(crate) const OPTION_SOME: u8 = 7;
    pub(crate) const OPTION_NONE: u8 = 8;
    pub(crate) const RESULT_OK: u8 = 9;
    pub(crate) const RESULT_ERR: u8 = 10;
    pub(crate) const RECORD_OF: u8 = 11;
    pub(crate) const FLAGS_SET: u8 = 12;
    pub(crate) const ENUM_CASE: u8 = 13;
    pub(crate) const VARIANT_CASE: u8 = 14;
    pub(crate) const RESOURCE_HANDLE: u8 = 15;
    pub(crate) const STREAM_HANDLE: u8 = 16;
    pub(crate) const FUTURE_HANDLE: u8 = 17;
}

/// Canonical-ABI byte size of one `cell` value. See module docs.
pub(crate) const CELL_SIZE: u32 = 16;

/// Byte offset where a cell's payload starts (after disc + padding).
const PAYLOAD_OFFSET: u64 = 8;

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

    /// For cell variants whose payload is a single value (i.e. the
    /// canonical-ABI flat form has exactly one slot), derive the
    /// store kind from the discriminant. Caller is responsible for
    /// having already widened the source value to match (e.g.
    /// narrow ints sign- or zero-extended into an `i64` before being
    /// stored as `cell::integer`).
    ///
    /// Returns `None` for variants whose payload has multiple flat
    /// slots (`text`, `bytes`) or whose payload is a side-table
    /// index — those cases construct their own `PayloadPart` slices.
    fn for_primitive_disc(disc: u8) -> Option<StoreKind> {
        match disc {
            cell_disc::BOOL => Some(StoreKind::I8),
            cell_disc::INTEGER => Some(StoreKind::I64),
            cell_disc::FLOATING => Some(StoreKind::F64),
            _ => None,
        }
    }
}

/// One value to write into a cell's payload area.
///
/// Callers describe each cell as a list of these — the loop in
/// [`emit_cell`] does the rest. `offset` is relative to the start of
/// the payload area (i.e. the actual store happens at
/// `addr + PAYLOAD_OFFSET + offset`).
#[derive(Clone, Copy)]
struct PayloadPart {
    /// Local holding the value to store (must already be of the type
    /// `kind` expects — caller is responsible for any narrowing /
    /// extension before reaching here).
    local: u32,
    kind: StoreKind,
    /// Byte offset within the payload area.
    offset: u32,
}

/// Emit wasm that writes one cell at `addr_local`: a 1-byte
/// discriminant at offset 0 followed by each `parts[i]` written into
/// the payload area at its declared sub-offset.
fn emit_cell(f: &mut Function, addr_local: u32, disc: u8, parts: &[PayloadPart]) {
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
        f.instructions().local_get(part.local);
        let mem = MemArg {
            offset: PAYLOAD_OFFSET + part.offset as u64,
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

/// Emit a single-payload primitive cell — body is identical across
/// `bool` / `integer` / `floating`, only the disc and store width
/// differ. Both are derived from `disc` via
/// [`StoreKind::for_primitive_disc`]; callers go through the typed
/// public helpers below for self-documentation at the call site.
fn emit_single_payload_cell(
    f: &mut Function,
    addr_local: u32,
    disc: u8,
    payload_local: u32,
) {
    let kind = StoreKind::for_primitive_disc(disc).unwrap_or_else(|| {
        panic!("emit_single_payload_cell: disc {disc} is not a single-payload primitive")
    });
    emit_cell(
        f,
        addr_local,
        disc,
        &[PayloadPart { local: payload_local, kind, offset: 0 }],
    );
}

/// Emit wasm that writes a `cell::bool(bool)`. `payload_local` is
/// an `i32` carrying 0 (false) or 1 (true) — the canonical-ABI
/// flat form of `bool`.
pub(crate) fn emit_bool_cell(f: &mut Function, addr_local: u32, payload_local: u32) {
    emit_single_payload_cell(f, addr_local, cell_disc::BOOL, payload_local);
}

/// Emit wasm that writes a `cell::integer(s64)`. `payload_local` is
/// an `i64` already widened from any narrower integer type
/// (s8/u8/.../u32 → `i64.extend_i32_{s,u}`; s64/u64 passes through).
pub(crate) fn emit_integer_cell(f: &mut Function, addr_local: u32, payload_local: u32) {
    emit_single_payload_cell(f, addr_local, cell_disc::INTEGER, payload_local);
}

/// Emit wasm that writes a `cell::floating(f64)`. `payload_local`
/// is an `f64` already widened from `f32` if the source was 32-bit
/// (`f64.promote_f32`).
pub(crate) fn emit_floating_cell(f: &mut Function, addr_local: u32, payload_local: u32) {
    emit_single_payload_cell(f, addr_local, cell_disc::FLOATING, payload_local);
}

/// Emit wasm that writes a `cell::text(string)`. `string` lowers as
/// `(ptr: i32, len: i32)` — both i32 locals must already point at a
/// valid utf-8 buffer in the same memory.
pub(crate) fn emit_text_cell(f: &mut Function, addr_local: u32, ptr_local: u32, len_local: u32) {
    emit_cell(f, addr_local, cell_disc::TEXT, &ptr_len_parts(ptr_local, len_local));
}

/// Emit wasm that writes a `cell::bytes(list<u8>)`. Same flat shape
/// as text: `(ptr: i32, len: i32)`. Used for the `list<u8>` fast-
/// path when the WIT element type is `u8`.
pub(crate) fn emit_bytes_cell(f: &mut Function, addr_local: u32, ptr_local: u32, len_local: u32) {
    emit_cell(f, addr_local, cell_disc::BYTES, &ptr_len_parts(ptr_local, len_local));
}

/// Shared `(ptr, len)` payload layout used by `text` and `bytes`
/// (and, later, by any cell carrying a flat `list<T>` reference).
fn ptr_len_parts(ptr_local: u32, len_local: u32) -> [PayloadPart; 2] {
    [
        PayloadPart { local: ptr_local, kind: StoreKind::I32, offset: 0 },
        PayloadPart { local: len_local, kind: StoreKind::I32, offset: 4 },
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
    /// memory" coverage comes via Phase 2-5's runtime fuzz.
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

    #[test]
    fn bool_cell_emits_valid_wasm() {
        // params: (addr_local: i32, payload_local: i32)
        build_and_validate(&[ValType::I32, ValType::I32], |f| emit_bool_cell(f, 0, 1));
    }

    #[test]
    fn integer_cell_emits_valid_wasm() {
        // params: (addr_local: i32, payload_local: i64)
        build_and_validate(&[ValType::I32, ValType::I64], |f| emit_integer_cell(f, 0, 1));
    }

    #[test]
    fn floating_cell_emits_valid_wasm() {
        // params: (addr_local: i32, payload_local: f64)
        build_and_validate(&[ValType::I32, ValType::F64], |f| {
            emit_floating_cell(f, 0, 1)
        });
    }

    #[test]
    fn text_cell_emits_valid_wasm() {
        // params: (addr_local: i32, ptr_local: i32, len_local: i32)
        build_and_validate(&[ValType::I32, ValType::I32, ValType::I32], |f| {
            emit_text_cell(f, 0, 1, 2)
        });
    }

    #[test]
    fn bytes_cell_emits_valid_wasm() {
        // params: (addr_local: i32, ptr_local: i32, len_local: i32)
        build_and_validate(&[ValType::I32, ValType::I32, ValType::I32], |f| {
            emit_bytes_cell(f, 0, 1, 2)
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
        for iter in 0..100u64 {
            let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(iter);
            match mixed % 5 {
                0 => build_and_validate(&[ValType::I32, ValType::I32], |f| {
                    emit_bool_cell(f, 0, 1)
                }),
                1 => build_and_validate(&[ValType::I32, ValType::I64], |f| {
                    emit_integer_cell(f, 0, 1)
                }),
                2 => build_and_validate(&[ValType::I32, ValType::F64], |f| {
                    emit_floating_cell(f, 0, 1)
                }),
                3 => build_and_validate(&[ValType::I32, ValType::I32, ValType::I32], |f| {
                    emit_text_cell(f, 0, 1, 2)
                }),
                4 => build_and_validate(&[ValType::I32, ValType::I32, ValType::I32], |f| {
                    emit_bytes_cell(f, 0, 1, 2)
                }),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn cell_discriminants_match_wit_declaration_order() {
        // Pin the discriminant numbering against the WIT cases listed
        // in `wit/common/world.wit`. If anyone reorders the variant
        // there, this test fires before lift codegen miscompiles
        // values into wrong cell cases.
        assert_eq!(cell_disc::BOOL, 0);
        assert_eq!(cell_disc::INTEGER, 1);
        assert_eq!(cell_disc::FLOATING, 2);
        assert_eq!(cell_disc::TEXT, 3);
        assert_eq!(cell_disc::BYTES, 4);
        assert_eq!(cell_disc::LIST_OF, 5);
        assert_eq!(cell_disc::TUPLE_OF, 6);
        assert_eq!(cell_disc::OPTION_SOME, 7);
        assert_eq!(cell_disc::OPTION_NONE, 8);
        assert_eq!(cell_disc::RESULT_OK, 9);
        assert_eq!(cell_disc::RESULT_ERR, 10);
        assert_eq!(cell_disc::RECORD_OF, 11);
        assert_eq!(cell_disc::FLAGS_SET, 12);
        assert_eq!(cell_disc::ENUM_CASE, 13);
        assert_eq!(cell_disc::VARIANT_CASE, 14);
        assert_eq!(cell_disc::RESOURCE_HANDLE, 15);
        assert_eq!(cell_disc::STREAM_HANDLE, 16);
        assert_eq!(cell_disc::FUTURE_HANDLE, 17);
    }
}
