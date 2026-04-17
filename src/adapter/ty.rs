//! Small wasm-encoder-adjacent helpers used by the adapter's binary
//! emission. Each helper is a leaf utility — no canonical-ABI logic
//! lives here anymore (see [`super::bindgen`] and [`super::wit_bridge`]
//! for that).
//!
//! - [`prim_cv`] — primitive cviz `ValueType` → wasm-encoder
//!   `ComponentValType`. Used by [`super::encoders`] as a shortcut
//!   that skips the full recursive encoder for scalar types.
//! - [`val_type_byte_size`] — byte size of a core-wasm `ValType` in
//!   linear memory. Used by [`super::mem_layout`] to size fixed
//!   canonical-ABI slots (event records, bool result buffers).
//! - [`align_to_val`] — round a byte offset up to the next multiple
//!   of `align`. Used by [`super::mem_layout`] to align cursors
//!   inside the adapter's scratch memory.

use cviz::model::ValueType;
use wasm_encoder::{ComponentValType, PrimitiveValType, ValType};

/// Byte size of a core Wasm value type in linear memory. Exhaustive
/// so the compiler catches new `ValType` variants when `wasm-encoder`
/// grows one.
pub(super) fn val_type_byte_size(vt: &ValType) -> u32 {
    match vt {
        ValType::I32 | ValType::F32 => 4,
        ValType::I64 | ValType::F64 => 8,
        ValType::V128 => 16,
        ValType::Ref(_) => 4,
    }
}

/// Round `offset` up to the nearest multiple of `align`.
///
/// Wasm linear memory loads and stores require naturally aligned
/// addresses — an `i32.load` needs a 4-byte-aligned address, an
/// `i64.load` needs 8-byte-aligned, etc. Misaligned access traps.
///
/// This function is used whenever a region of raw bytes (e.g. the
/// function-name blob) is followed by typed values that need
/// alignment. For example, if the names total 7 bytes:
///
/// ```text
/// align_to_val(7, 4) → 8    // next multiple of 4 after 7
/// align_to_val(8, 4) → 8    // already aligned, no change
/// align_to_val(9, 4) → 12
/// ```
pub(super) fn align_to_val(offset: u32, align: u32) -> u32 {
    offset.div_ceil(align) * align
}

/// Convert a primitive `ValueType` into a wasm-encoder
/// `ComponentValType`. Returns `None` for non-primitive variants.
pub(super) fn prim_cv(vt: &ValueType) -> Option<ComponentValType> {
    let pv = match vt {
        ValueType::Bool => PrimitiveValType::Bool,
        ValueType::U8 => PrimitiveValType::U8,
        ValueType::S8 => PrimitiveValType::S8,
        ValueType::U16 => PrimitiveValType::U16,
        ValueType::S16 => PrimitiveValType::S16,
        ValueType::U32 => PrimitiveValType::U32,
        ValueType::S32 => PrimitiveValType::S32,
        ValueType::U64 => PrimitiveValType::U64,
        ValueType::S64 => PrimitiveValType::S64,
        ValueType::F32 => PrimitiveValType::F32,
        ValueType::F64 => PrimitiveValType::F64,
        ValueType::Char => PrimitiveValType::Char,
        ValueType::String => PrimitiveValType::String,
        _ => return None,
    };
    Some(ComponentValType::Primitive(pv))
}
