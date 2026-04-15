//! Type analysis helpers: the cviz `ValueType` model on the input
//! side, and the canonical ABI / wasm-encoder `ValType` model on
//! the output side.
//!
//! This file collects every per-type helper the adapter generator
//! needs:
//!
//! - **Flattening** ([`flat_types_for`], [`join_flat_lists`],
//!   [`join_val_type`]) — turn a component-level value type into
//!   the flat sequence of core-Wasm `ValType`s the canonical ABI
//!   uses for parameters and return slots.
//! - **Inspection** ([`type_has_strings`], [`type_has_resources`])
//!   — predicates that recurse through compound types so the
//!   adapter knows whether it needs `memory` / `realloc` /
//!   `string-encoding` canonical options.
//! - **Layout** ([`canonical_align`], [`discriminant_align`],
//!   [`align_to_val`]) — alignment math for the canonical ABI.
//! - **Conversion** ([`prim_cv`]) — turn a primitive `ValueType`
//!   into a `wasm_encoder::ComponentValType`.

use cviz::model::{TypeArena, ValueType, ValueTypeId};
use wasm_encoder::{ComponentValType, PrimitiveValType, ValType};

/// Flatten a component-level value type into the canonical ABI
/// sequence of core-Wasm `ValType`s.
pub(super) fn flat_types_for(id: ValueTypeId, arena: &TypeArena) -> Vec<ValType> {
    match arena.lookup_val(id) {
        ValueType::Bool
        | ValueType::U8
        | ValueType::S8
        | ValueType::U16
        | ValueType::S16
        | ValueType::U32
        | ValueType::S32
        | ValueType::Char
        | ValueType::ErrorContext => vec![ValType::I32],
        ValueType::S64 | ValueType::U64 => vec![ValType::I64],
        ValueType::F32 => vec![ValType::F32],
        ValueType::F64 => vec![ValType::F64],
        ValueType::String => vec![ValType::I32, ValType::I32],
        ValueType::Resource(_) | ValueType::AsyncHandle => vec![ValType::I32],
        ValueType::Enum(_) => vec![ValType::I32],
        ValueType::Flags(names) => {
            let n_words = names.len().div_ceil(32);
            vec![ValType::I32; n_words]
        }
        ValueType::Record(fields) => fields
            .iter()
            .flat_map(|(_, id)| flat_types_for(*id, arena))
            .collect(),
        ValueType::Tuple(ids) => ids
            .iter()
            .flat_map(|id| flat_types_for(*id, arena))
            .collect(),
        ValueType::List(_) | ValueType::FixedSizeList(..) | ValueType::Map(..) => {
            vec![ValType::I32, ValType::I32]
        }
        ValueType::Option(inner) => {
            let inner_flat = flat_types_for(*inner, arena);
            let mut result = vec![ValType::I32]; // discriminant
            result.extend(join_flat_lists(&[inner_flat]));
            result
        }
        ValueType::Result { ok, err } => {
            let ok_flat = ok.map(|id| flat_types_for(id, arena)).unwrap_or_default();
            let err_flat = err.map(|id| flat_types_for(id, arena)).unwrap_or_default();
            let mut result = vec![ValType::I32]; // discriminant
            result.extend(join_flat_lists(&[ok_flat, err_flat]));
            result
        }
        ValueType::Variant(cases) => {
            let case_flats: Vec<Vec<ValType>> = cases
                .iter()
                .filter_map(|(_, opt_id)| opt_id.map(|id| flat_types_for(id, arena)))
                .collect();
            let mut result = vec![ValType::I32]; // discriminant
            result.extend(join_flat_lists(&case_flats));
            result
        }
    }
}

/// Element-wise join of multiple flat type vectors (canonical ABI
/// widening rule).
pub(super) fn join_flat_lists(lists: &[Vec<ValType>]) -> Vec<ValType> {
    let max_len = lists.iter().map(|l| l.len()).max().unwrap_or(0);
    let mut result = Vec::with_capacity(max_len);
    for i in 0..max_len {
        let mut joined = ValType::I32;
        for list in lists {
            let vt = list.get(i).copied().unwrap_or(ValType::I32);
            joined = join_val_type(joined, vt);
        }
        result.push(joined);
    }
    result
}

pub(super) fn join_val_type(a: ValType, b: ValType) -> ValType {
    match (a, b) {
        (ValType::I32, ValType::I32) => ValType::I32,
        (ValType::F32, ValType::F32) => ValType::F32,
        (ValType::F64, ValType::F64) => ValType::F64,
        (ValType::I64, _) | (_, ValType::I64) => ValType::I64,
        (ValType::F64, _) | (_, ValType::F64) => ValType::F64,
        _ => ValType::I32,
    }
}

/// Returns true if the type (or any type it transitively contains)
/// is a string.
pub(super) fn type_has_strings(id: ValueTypeId, arena: &TypeArena) -> bool {
    match arena.lookup_val(id) {
        ValueType::String => true,
        ValueType::Record(fields) => fields.iter().any(|(_, id)| type_has_strings(*id, arena)),
        ValueType::Tuple(ids) => ids.iter().any(|id| type_has_strings(*id, arena)),
        ValueType::Variant(cases) => cases.iter().any(|(_, opt_id)| {
            opt_id
                .map(|id| type_has_strings(id, arena))
                .unwrap_or(false)
        }),
        ValueType::Option(inner) => type_has_strings(*inner, arena),
        ValueType::Result { ok, err } => {
            ok.map(|id| type_has_strings(id, arena)).unwrap_or(false)
                || err.map(|id| type_has_strings(id, arena)).unwrap_or(false)
        }
        ValueType::List(inner) | ValueType::FixedSizeList(inner, _) => {
            type_has_strings(*inner, arena)
        }
        _ => false,
    }
}

/// Returns true if the type (or any type it transitively contains)
/// is a resource.
pub(super) fn type_has_resources(id: ValueTypeId, arena: &TypeArena) -> bool {
    match arena.lookup_val(id) {
        ValueType::Resource(_) | ValueType::AsyncHandle => true,
        ValueType::Record(fields) => fields.iter().any(|(_, id)| type_has_resources(*id, arena)),
        ValueType::Tuple(ids) => ids.iter().any(|id| type_has_resources(*id, arena)),
        ValueType::Variant(cases) => cases.iter().any(|(_, opt_id)| {
            opt_id
                .map(|id| type_has_resources(id, arena))
                .unwrap_or(false)
        }),
        ValueType::Option(inner) => type_has_resources(*inner, arena),
        ValueType::Result { ok, err } => {
            ok.map(|id| type_has_resources(id, arena)).unwrap_or(false)
                || err.map(|id| type_has_resources(id, arena)).unwrap_or(false)
        }
        ValueType::List(inner) | ValueType::FixedSizeList(inner, _) => {
            type_has_resources(*inner, arena)
        }
        _ => false,
    }
}

/// Returns the canonical ABI alignment (in bytes) for a value type.
pub(super) fn canonical_align(id: ValueTypeId, arena: &TypeArena) -> u32 {
    match arena.lookup_val(id) {
        ValueType::Bool | ValueType::U8 | ValueType::S8 => 1,
        ValueType::U16 | ValueType::S16 => 2,
        ValueType::U32
        | ValueType::S32
        | ValueType::F32
        | ValueType::Char
        | ValueType::String
        | ValueType::Resource(_)
        | ValueType::AsyncHandle => 4,
        ValueType::U64 | ValueType::S64 | ValueType::F64 => 8,
        ValueType::List(_) | ValueType::FixedSizeList(..) | ValueType::Map(..) => 4,
        ValueType::Record(fields) => fields
            .iter()
            .map(|(_, id)| canonical_align(*id, arena))
            .max()
            .unwrap_or(1),
        ValueType::Tuple(ids) => ids
            .iter()
            .map(|id| canonical_align(*id, arena))
            .max()
            .unwrap_or(1),
        ValueType::Variant(cases) => {
            let payload_align = cases
                .iter()
                .filter_map(|(_, opt_id)| opt_id.map(|id| canonical_align(id, arena)))
                .max()
                .unwrap_or(1);
            std::cmp::max(discriminant_align(cases.len()), payload_align)
        }
        ValueType::Option(inner) => std::cmp::max(1, canonical_align(*inner, arena)),
        ValueType::Result { ok, err } => {
            let ok_a = ok.map(|id| canonical_align(id, arena)).unwrap_or(1);
            let err_a = err.map(|id| canonical_align(id, arena)).unwrap_or(1);
            std::cmp::max(1, std::cmp::max(ok_a, err_a))
        }
        ValueType::Enum(tags) => discriminant_align(tags.len()),
        ValueType::Flags(names) => {
            if names.len() > 16 {
                4
            } else if names.len() > 8 {
                2
            } else {
                1
            }
        }
        ValueType::ErrorContext => 4,
    }
}

/// Discriminant alignment (in bytes) for a variant/enum with `n` cases.
///
/// The canonical ABI encodes a variant's discriminant as the smallest
/// unsigned integer type that can represent all case indices:
///
/// - `n <= 256` (fits in u8) → 1-byte discriminant, 1-byte alignment
/// - `n <= 65536` (fits in u16) → 2-byte discriminant, 2-byte alignment
/// - `n <= 2^32` (fits in u32) → 4-byte discriminant, 4-byte alignment
///
/// The component model caps variant cases at `u32::MAX`, so u32 is the
/// largest discriminant type. Inputs exceeding that are rejected.
///
/// See [`discriminant_type`] in the canonical ABI spec.
///
/// [`discriminant_type`]: https://github.com/WebAssembly/component-model/blob/main/design/mvp/canonical-abi/definitions.py
pub(super) fn discriminant_align(n: usize) -> u32 {
    if n <= 256 {
        1
    } else if n <= 65_536 {
        2
    } else if n <= u32::MAX as usize {
        4
    } else {
        panic!(
            "Variant/enum has {n} cases, which exceeds the component model's \
             u32::MAX limit. This should be unreachable for valid components."
        )
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
///
pub(super) fn align_to_val(offset: u32, align: u32) -> u32 {
    offset.div_ceil(align) * align
}

// ─── FlatLayout: canonical ABI memory layout for flat values ─────────────

/// One slot in the flat memory layout of a component value type.
#[derive(Clone, Debug)]
pub(super) struct FlatSlot {
    /// Byte offset of this value within the layout.
    pub byte_offset: u32,
    /// Core Wasm value type of this slot.
    pub val_type: ValType,
    /// `true` when the discriminant is stored as a u8 and must be
    /// loaded via `i32.load8_u` instead of a regular `i32.load`.
    /// Only set for the discriminant slot of `result<T, E>`.
    pub is_discriminant: bool,
}

/// Pre-computed byte-level layout of a component value type's flat
/// representation in linear memory.
///
/// Modeled after wasmtime-environ's `TypeInformation` / `FlatTypes`
/// and wasmparser's `LoweredTypes`, but simplified to our use case:
/// byte offsets + val types so we can emit load instructions for
/// `task.return` and sync-complex retptr patterns.
///
/// # How `result<T, E>` is flattened
///
/// The canonical ABI flattens `result<ok_type, err_type>` into:
///
/// ```text
/// flat_types = [discriminant, ...joined_payload_slots...]
/// ```
///
/// - **discriminant** (i32): 0 = Ok arm, 1 = Err arm.
/// - **joined_payload_slots**: the element-wise type-join of
///   `flatten(ok_type)` and `flatten(err_type)`, padded to the max
///   length of the two. For example, `result<u32, string>` flattens
///   to `[i32, i32, i32]` — discriminant + join(\[i32\], \[i32, i32\]).
///
/// In memory, the layout is:
/// - offset 0: discriminant (stored as u8, loaded as i32)
/// - offset `align_to(1, max(ok_align, err_align))`: payload values
///   at sequential naturally-aligned offsets from there
///
/// # Other types
///
/// For non-result types (string, tuple, record, etc.), the flat values
/// are laid out sequentially at naturally-aligned offsets from byte 0.
///
/// # Construction
///
/// Use [`FlatLayout::new`], passing the component-level type id and
/// the pre-flattened `ValType` slice. The constructor inspects the
/// type to choose the right layout strategy.
#[derive(Clone, Debug)]
pub(super) struct FlatLayout {
    pub slots: Vec<FlatSlot>,
    /// Total byte size of the layout (available for buffer allocation).
    pub total_bytes: u32,
}

/// Byte size of a core Wasm value type in linear memory.
/// Exhaustive so the compiler catches new `ValType` variants.
pub(super) fn val_type_byte_size(vt: &ValType) -> u32 {
    match vt {
        ValType::I32 | ValType::F32 => 4,
        ValType::I64 | ValType::F64 => 8,
        ValType::V128 => 16,
        ValType::Ref(_) => 4,
    }
}

impl FlatLayout {
    /// Build a layout for any component value type.
    ///
    /// - For `result<T, E>`: the first slot is the discriminant (1
    ///   byte, loaded as i32 via `i32.load8_u`). The remaining slots
    ///   are the joined payload, starting at an aligned offset after
    ///   the discriminant. All payload slots are loaded from memory.
    /// - For everything else (string, tuple, record, etc.): slots are
    ///   laid out sequentially at naturally-aligned offsets from byte 0.
    pub fn new(
        type_id: ValueTypeId,
        flat_types: &[ValType],
        arena: &TypeArena,
    ) -> Self {
        let vt = arena.lookup_val(type_id);
        match vt {
            ValueType::Result { ok, err } => {
                Self::build_result_layout(flat_types, *ok, *err, arena)
            }
            _ => Self::build_sequential_layout(flat_types),
        }
    }

    /// Append sequential naturally-aligned slots starting at `offset`.
    /// Returns the byte offset past the last slot.
    fn append_sequential(
        slots: &mut Vec<FlatSlot>,
        flat_types: &[ValType],
        mut offset: u32,
    ) -> u32 {
        for vt in flat_types {
            let size = val_type_byte_size(vt);
            offset = align_to_val(offset, size);
            slots.push(FlatSlot {
                byte_offset: offset,
                val_type: *vt,
                is_discriminant: false,
            });
            offset += size;
        }
        offset
    }

    /// Sequential layout: each flat value at naturally-aligned offsets
    /// starting from byte 0.
    fn build_sequential_layout(flat_types: &[ValType]) -> Self {
        let mut slots = Vec::with_capacity(flat_types.len());
        let total_bytes = Self::append_sequential(&mut slots, flat_types, 0);
        FlatLayout { slots, total_bytes }
    }

    /// Result layout: discriminant byte + aligned payload.
    ///
    /// The canonical ABI stores `result<T, E>` as:
    /// - offset 0: discriminant (u8)
    /// - offset `align_to(1, max(ok_align, err_align))`: payload
    ///   values laid out sequentially from there
    fn build_result_layout(
        flat_types: &[ValType],
        ok_id: Option<ValueTypeId>,
        err_id: Option<ValueTypeId>,
        arena: &TypeArena,
    ) -> Self {
        if flat_types.is_empty() {
            return FlatLayout {
                slots: vec![],
                total_bytes: 0,
            };
        }

        let mut slots = Vec::with_capacity(flat_types.len());

        // Slot 0: discriminant (u8 at offset 0, loaded as i32).
        slots.push(FlatSlot {
            byte_offset: 0,
            val_type: ValType::I32,
            is_discriminant: true,
        });

        // Payload starts after the 1-byte discriminant, aligned to
        // the strictest alignment of the Ok/Err arms.
        let ok_a = ok_id.map(|id| canonical_align(id, arena)).unwrap_or(1);
        let err_a = err_id.map(|id| canonical_align(id, arena)).unwrap_or(1);
        let payload_start = align_to_val(1, std::cmp::max(ok_a, err_a));

        let total_bytes =
            Self::append_sequential(&mut slots, &flat_types[1..], payload_start);
        FlatLayout { slots, total_bytes }
    }
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
