//! Verbatim copies of two private helpers in `wit-bindgen-core`:
//! [`cast`] and [`flat_types`]. Both are spec-frozen lookup logic
//! with no decision-making; the adapter's lift-from-memory codegen
//! needs them to widen heterogeneous variant arms to the joined flat
//! representation before wasm block merge.
//!
//! **Pending upstream visibility flip:**
//! <https://github.com/bytecodealliance/wit-bindgen/pull/1597>. Once
//! that PR lands, delete this module and import the symbols directly
//! from `wit_bindgen_core::abi`.
//!
//! Source: `wit-bindgen-core-0.52.0/src/abi.rs` lines 2562-2627.

use wit_bindgen_core::abi::{Bitcast, FlatTypes, WasmType};
use wit_parser::{Resolve, Type};

/// Canonical-ABI parameter flat-width cap — 16 core-wasm values before
/// the ABI falls back to indirect-through-pointer lowering.
pub(crate) const MAX_FLAT_PARAMS: usize = 16;

/// Canonical-ABI bitcast selector: the wasm conversion needed to move
/// a value of flat type `from` into flat type `to`.
///
/// Verbatim from `wit-bindgen-core/src/abi.rs:2562`.
pub(crate) fn cast(from: WasmType, to: WasmType) -> Bitcast {
    use WasmType::*;

    match (from, to) {
        (I32, I32)
        | (I64, I64)
        | (F32, F32)
        | (F64, F64)
        | (Pointer, Pointer)
        | (PointerOrI64, PointerOrI64)
        | (Length, Length) => Bitcast::None,

        (I32, I64) => Bitcast::I32ToI64,
        (F32, I32) => Bitcast::F32ToI32,
        (F64, I64) => Bitcast::F64ToI64,

        (I64, I32) => Bitcast::I64ToI32,
        (I32, F32) => Bitcast::I32ToF32,
        (I64, F64) => Bitcast::I64ToF64,

        (F32, I64) => Bitcast::F32ToI64,
        (I64, F32) => Bitcast::I64ToF32,

        (I64, PointerOrI64) => Bitcast::I64ToP64,
        (Pointer, PointerOrI64) => Bitcast::PToP64,
        (_, PointerOrI64) => {
            Bitcast::Sequence(Box::new([cast(from, I64), cast(I64, PointerOrI64)]))
        }

        (PointerOrI64, I64) => Bitcast::P64ToI64,
        (PointerOrI64, Pointer) => Bitcast::P64ToP,
        (PointerOrI64, _) => Bitcast::Sequence(Box::new([cast(PointerOrI64, I64), cast(I64, to)])),

        (I32, Pointer) => Bitcast::I32ToP,
        (Pointer, I32) => Bitcast::PToI32,
        (I32, Length) => Bitcast::I32ToL,
        (Length, I32) => Bitcast::LToI32,
        (I64, Length) => Bitcast::I64ToL,
        (Length, I64) => Bitcast::LToI64,
        (Pointer, Length) => Bitcast::PToL,
        (Length, Pointer) => Bitcast::LToP,

        (F32, Pointer | Length) => Bitcast::Sequence(Box::new([cast(F32, I32), cast(I32, to)])),
        (Pointer | Length, F32) => Bitcast::Sequence(Box::new([cast(from, I32), cast(I32, F32)])),

        (F32, F64)
        | (F64, F32)
        | (F64, I32)
        | (I32, F64)
        | (Pointer | Length, I64 | F64)
        | (I64 | F64, Pointer | Length) => {
            unreachable!("Don't know how to bitcast from {:?} to {:?}", from, to);
        }
    }
}

/// Canonical-ABI flat-types computation: returns the flat wasm types
/// for `ty`, or `None` if flattening exceeds `max_params`.
///
/// Verbatim from `wit-bindgen-core/src/abi.rs:2622`, with the private
/// `MAX_FLAT_PARAMS` constant replaced by the local copy above.
pub(crate) fn flat_types(
    resolve: &Resolve,
    ty: &Type,
    max_params: Option<usize>,
) -> Option<Vec<WasmType>> {
    let max_params = max_params.unwrap_or(MAX_FLAT_PARAMS);
    let mut storage = std::iter::repeat_n(WasmType::I32, max_params).collect::<Vec<_>>();
    let mut flat = FlatTypes::new(storage.as_mut_slice());
    resolve.push_flat(ty, &mut flat).then_some(flat.to_vec())
}
