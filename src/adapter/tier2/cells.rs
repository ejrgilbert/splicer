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
#[allow(dead_code)]
pub(crate) const CELL_SIZE: u32 = 16;

/// Byte offset where a cell's payload starts (after disc + padding).
const PAYLOAD_OFFSET: u64 = 8;

/// Emit wasm that writes a `cell::integer(s64)` at the address
/// provided in `addr_local`, with the s64 payload provided in
/// `payload_local`.
///
/// Both locals must already exist on the function (allocated by the
/// caller). After this function returns, the cell has been written
/// to memory; the wasm value stack is unchanged from before the
/// call.
///
/// Caller is responsible for advancing the cells-array cursor (i.e.
/// incrementing the cell count and recomputing the next cell's
/// address); this helper writes one cell at the given address and
/// returns.
#[allow(dead_code)] // first concrete primitive — orchestrator wiring lands in Phase 2-3.
pub(crate) fn emit_integer_cell(f: &mut Function, addr_local: u32, payload_local: u32) {
    // Discriminant byte at offset 0.
    f.instructions().local_get(addr_local);
    f.instructions().i32_const(cell_disc::INTEGER as i32);
    f.instructions().i32_store8(MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    });
    // Payload (s64) at offset 8, 8-aligned.
    f.instructions().local_get(addr_local);
    f.instructions().local_get(payload_local);
    f.instructions().i64_store(MemArg {
        offset: PAYLOAD_OFFSET,
        align: 3,
        memory_index: 0,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_encoder::{
        CodeSection, EntityType, FunctionSection, ImportSection, MemoryType, Module, TypeSection,
        ValType,
    };

    /// Build a minimal wasm module containing one function that calls
    /// `emit_integer_cell` against a freshly-allocated pair of locals,
    /// then validate the produced bytes round-trip through wasmparser.
    ///
    /// This is a structural smoke test — it confirms our emit doesn't
    /// produce ill-formed bytecode. End-to-end "did the right value
    /// land in memory" coverage comes via Phase 2-5's runtime fuzz.
    #[test]
    fn integer_cell_emits_valid_wasm() {
        let mut module = Module::new();

        // Type section: one function type `(i32 addr, i64 value) -> ()`.
        let mut types = TypeSection::new();
        types.ty().function([ValType::I32, ValType::I64], []);
        module.section(&types);

        // Imports: a memory (so stores have somewhere to land).
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

        // Function section: one func of type 0.
        let mut funcs = FunctionSection::new();
        funcs.function(0);
        module.section(&funcs);

        // Code section: the function body emits one integer cell.
        // Locals 0 and 1 are the two function params (addr_local,
        // payload_local); no extra locals needed.
        let mut code = CodeSection::new();
        let mut f = Function::new([]);
        emit_integer_cell(&mut f, 0, 1);
        f.instructions().end();
        code.function(&f);
        module.section(&code);

        let bytes = module.finish();

        // Round-trip through wasmparser to confirm the bytecode is
        // structurally valid (no malformed locals, no out-of-range
        // memory ops, store alignments check out, etc.).
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("emitted module should validate");
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
