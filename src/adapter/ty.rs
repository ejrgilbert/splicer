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
//! - **Inspection** ([`type_has_strings`], [`type_has_lists`])
//!   — predicates that recurse through compound types so the
//!   adapter knows whether it needs `memory` / `realloc` /
//!   `string-encoding` canonical options.
//! - **Layout** ([`canonical_size_and_align`], [`discriminant_align`],
//!   [`align_to_val`]) — size and alignment math for the canonical
//!   ABI. Destructure the `(size, align)` tuple at call sites when
//!   you need only one half.
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
        // Dynamic list / map: (ptr, len) pair — always two i32s on
        // the wire regardless of the element type.
        ValueType::List(_) | ValueType::Map(..) => vec![ValType::I32, ValType::I32],
        // Fixed-size list: N copies of the element's flat types
        // inlined. Distinct from dynamic list — canonical ABI
        // flattens `list<T, N>` to `N × flat(T)`, not `(ptr, len)`.
        ValueType::FixedSizeList(inner, n) => {
            let inner_flat = flat_types_for(*inner, arena);
            (0..*n).flat_map(|_| inner_flat.iter().copied()).collect()
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
///
/// Match is exhaustive — a `ValueType` added to cviz will force a
/// compile error here so the traversal can't silently miss a new
/// string-bearing shape.
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
        ValueType::Map(k, v) => type_has_strings(*k, arena) || type_has_strings(*v, arena),

        // Leaf non-string types.
        ValueType::Bool
        | ValueType::S8
        | ValueType::U8
        | ValueType::S16
        | ValueType::U16
        | ValueType::S32
        | ValueType::U32
        | ValueType::S64
        | ValueType::U64
        | ValueType::F32
        | ValueType::F64
        | ValueType::Char
        | ValueType::ErrorContext
        | ValueType::Resource(_)
        | ValueType::AsyncHandle
        | ValueType::Enum(_)
        | ValueType::Flags(_) => false,
    }
}

/// Returns true if the type (or any type it transitively contains)
/// is a list. Drives the `needs_realloc` decision: canon lower
/// allocates memory via realloc to marshal list contents (both
/// dynamic `list<T>` and fixed-size `list<T, N>`), so any function
/// with a list param or result needs the memory provider to export
/// `realloc`.
///
/// Match is exhaustive — see [`type_has_strings`] for the same
/// rationale.
pub(super) fn type_has_lists(id: ValueTypeId, arena: &TypeArena) -> bool {
    match arena.lookup_val(id) {
        ValueType::List(_) | ValueType::FixedSizeList(..) => true,

        ValueType::Record(fields) => fields.iter().any(|(_, id)| type_has_lists(*id, arena)),
        ValueType::Tuple(ids) => ids.iter().any(|id| type_has_lists(*id, arena)),
        ValueType::Variant(cases) => cases
            .iter()
            .any(|(_, opt_id)| opt_id.map(|id| type_has_lists(id, arena)).unwrap_or(false)),
        ValueType::Option(inner) => type_has_lists(*inner, arena),
        ValueType::Result { ok, err } => {
            ok.map(|id| type_has_lists(id, arena)).unwrap_or(false)
                || err.map(|id| type_has_lists(id, arena)).unwrap_or(false)
        }
        ValueType::Map(k, v) => type_has_lists(*k, arena) || type_has_lists(*v, arena),

        // Leaf non-list types.
        ValueType::Bool
        | ValueType::S8
        | ValueType::U8
        | ValueType::S16
        | ValueType::U16
        | ValueType::S32
        | ValueType::U32
        | ValueType::S64
        | ValueType::U64
        | ValueType::F32
        | ValueType::F64
        | ValueType::Char
        | ValueType::String
        | ValueType::ErrorContext
        | ValueType::Resource(_)
        | ValueType::AsyncHandle
        | ValueType::Enum(_)
        | ValueType::Flags(_) => false,
    }
}

/// Returns `(size, align)` in bytes for a value type's canonical-ABI
/// linear-memory layout. Single walker so compound types don't visit
/// their fields twice (once for align, once for size) — the
/// [`canonical_align`] and [`canonical_size`] wrappers are thin
/// accessors.
///
/// Compound sizes include inter-field natural alignment padding and
/// are themselves rounded up to the type's own alignment so adjacent
/// allocations of the same type stay naturally aligned.
///
/// See [`elem_size`] / [`size`] / [`alignment`] in the canonical ABI
/// spec.
///
/// [`elem_size`]: https://github.com/WebAssembly/component-model/blob/main/design/mvp/canonical-abi/definitions.py
pub(super) fn canonical_size_and_align(id: ValueTypeId, arena: &TypeArena) -> (u32, u32) {
    match arena.lookup_val(id) {
        ValueType::Bool | ValueType::U8 | ValueType::S8 => (1, 1),
        ValueType::U16 | ValueType::S16 => (2, 2),
        ValueType::U32 | ValueType::S32 | ValueType::F32 | ValueType::Char => (4, 4),
        ValueType::U64 | ValueType::S64 | ValueType::F64 => (8, 8),
        // string / list / map: (ptr, len) — two i32s, so 8 bytes with 4-byte align.
        ValueType::String | ValueType::List(_) | ValueType::Map(..) => (8, 4),
        ValueType::FixedSizeList(inner, n) => {
            let (inner_size, inner_align) = canonical_size_and_align(*inner, arena);
            (inner_size * n, inner_align)
        }
        // own<T> / borrow<T> / async-handle / error-context: 4-byte handle.
        ValueType::Resource(_) | ValueType::AsyncHandle | ValueType::ErrorContext => (4, 4),

        ValueType::Record(fields) => compound_layout(
            fields
                .iter()
                .map(|(_, fid)| canonical_size_and_align(*fid, arena)),
        ),
        ValueType::Tuple(ids) => {
            compound_layout(ids.iter().map(|tid| canonical_size_and_align(*tid, arena)))
        }
        ValueType::Variant(cases) => {
            let (payload_size, payload_align) =
                variant_payload_layout(cases.iter().filter_map(|(_, opt_id)| {
                    opt_id.map(|id| canonical_size_and_align(id, arena))
                }));
            discriminated_layout(discriminant_align(cases.len()), payload_size, payload_align)
        }
        ValueType::Option(inner) => {
            // Option = variant { none, some(T) } — 2 cases, so 1-byte disc.
            let (payload_size, payload_align) = canonical_size_and_align(*inner, arena);
            discriminated_layout(1, payload_size, payload_align)
        }
        ValueType::Result { ok, err } => {
            let (payload_size, payload_align) = variant_payload_layout(
                [*ok, *err]
                    .into_iter()
                    .flatten()
                    .map(|id| canonical_size_and_align(id, arena)),
            );
            discriminated_layout(1, payload_size, payload_align)
        }
        ValueType::Enum(tags) => {
            let s = discriminant_align(tags.len());
            (s, s)
        }
        ValueType::Flags(names) => {
            // Per canonical ABI: 0 bytes for empty flags, then 1/2/4
            // bytes for ≤8/≤16/≤32 labels (packed into u8/u16/u32),
            // then `ceil(n/32) × 4` for 33+ labels (multiple u32
            // words, always 4-byte aligned). No upper cap in the
            // spec — any label count is valid.
            let n = names.len();
            if n == 0 {
                (0, 1)
            } else if n <= 8 {
                (1, 1)
            } else if n <= 16 {
                (2, 2)
            } else {
                (4 * ((n as u32).div_ceil(32)), 4)
            }
        }
    }
}

/// Lay out a record- or tuple-shaped sequence of fields, returning
/// `(total_size, type_align)`. Each field is placed at an offset
/// aligned to its own align; total size is rounded up to the
/// compound's max-field align so adjacent allocations stay aligned.
fn compound_layout(fields: impl IntoIterator<Item = (u32, u32)>) -> (u32, u32) {
    let mut offset = 0u32;
    let mut max_align = 1u32;
    for (size, align) in fields {
        max_align = max_align.max(align);
        offset = align_to_val(offset, align);
        offset += size;
    }
    (align_to_val(offset, max_align), max_align)
}

/// Reduce a set of variant case payloads to the payload region's
/// `(size, align)` — max size and max alignment across the
/// non-empty arms, with `(0, 1)` when every arm is empty.
fn variant_payload_layout(arms: impl IntoIterator<Item = (u32, u32)>) -> (u32, u32) {
    let mut max_size = 0u32;
    let mut max_align = 1u32;
    for (s, a) in arms {
        max_size = max_size.max(s);
        max_align = max_align.max(a);
    }
    (max_size, max_align)
}

/// Combine a discriminator + payload into a single discriminated-type
/// layout: discriminant at offset 0, payload at
/// `align_to(disc_size, payload_align)`, total rounded up to the
/// type's own alignment (`max(disc_size, payload_align)`).
fn discriminated_layout(disc_size: u32, payload_size: u32, payload_align: u32) -> (u32, u32) {
    let align = disc_size.max(payload_align);
    let total = align_to_val(disc_size, payload_align) + payload_size;
    (align_to_val(total, align), align)
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

/// Memory shape of a single `FlatSlot` — encodes exactly the six
/// valid (load-instruction, stack-type) combinations the canonical
/// ABI needs in linear memory. One variant per load instruction
/// keeps the invariant type-level: an ill-formed `(val_type,
/// load_byte_size)` pair is unrepresentable.
///
/// Used by the subword-widening loads (`U8` / `U16` — zero-extend to
/// `i32` on the stack), the regular full-width loads (`I32` / `I64`
/// / `F32` / `F64`), and every discriminator / flags-word / string-
/// or list-half slot.
#[derive(Clone, Copy, Debug)]
pub(super) enum FlatSlotShape {
    /// `i32.load8_u` — 1 byte loaded, zero-extended to i32. Used
    /// for `bool` / `u8` / `s8` / 1-byte discriminant / 1-byte flags.
    U8,
    /// `i32.load16_u` — 2 bytes loaded, zero-extended to i32. Used
    /// for `u16` / `s16` / 2-byte discriminant / 2-byte flags.
    U16,
    /// `i32.load` — 4 bytes as `i32`. Used for `u32` / `s32` /
    /// `char` / string-ptr / list-ptr / resource handle / 4-byte
    /// discriminant / per-word flags chunk.
    I32,
    /// `i64.load` — 8 bytes as `i64`. Used for `u64` / `s64`.
    I64,
    /// `f32.load` — 4 bytes as `f32`.
    F32,
    /// `f64.load` — 8 bytes as `f64`.
    F64,
}

/// One slot in the canonical-ABI memory layout of a component value
/// type. The dispatch module's load emitter reads one of these per
/// slot to issue the right load instruction.
#[derive(Clone, Debug)]
pub(super) struct FlatSlot {
    /// Byte offset of this slot within the containing type's memory
    /// layout.
    pub byte_offset: u32,
    /// Load instruction + stack type for this slot. See
    /// [`FlatSlotShape`].
    pub shape: FlatSlotShape,
}

/// Pre-computed byte-level canonical-ABI memory layout for a
/// component value type.
///
/// Built by walking the `ValueType` structure directly (not the
/// flat-widened type list), so every slot's `byte_offset` matches
/// what `canon lower` writes and every slot's `shape` drives the
/// load instruction `canon lift` expects.
///
/// For the total canonical byte size of a type, call
/// [`canonical_size_and_align`] directly — no need to materialise a
/// `FlatLayout`.
#[derive(Clone, Debug)]
pub(super) struct FlatLayout {
    pub slots: Vec<FlatSlot>,
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
    /// Build the canonical-ABI memory layout for a component value
    /// type. Walks the type structure; every slot's offset and
    /// shape matches what `canon lower` writes.
    pub fn new(type_id: ValueTypeId, arena: &TypeArena) -> Self {
        let mut slots = Vec::new();
        walk_val_type(type_id, &mut slots, 0, arena);
        FlatLayout { slots }
    }
}

/// Walk a `ValueType`, appending one slot per canonical-ABI memory
/// position starting at `base_offset`. Returns nothing — the walker
/// places each slot at its canonical offset; the caller gets size
/// info from [`canonical_size_and_align`] separately.
///
/// Handling of discriminated types (`option` / `result` / `variant` /
/// `enum`) deserves a note: at codegen time we don't know which arm
/// will be active at runtime, but `task.return`'s core signature uses
/// the JOINED flat type at each payload position. For homogeneous
/// variants (all arms share the same flat shape) the walker produces
/// slots that match every arm's canonical layout exactly. For
/// heterogeneous variants (e.g. `variant { u8, u64 }` whose join is
/// `i64`), the walker follows the canonical-ABI payload-start offset
/// and the longest-flat arm's layout from there; reads for shorter
/// arms pick up the arm's stored bytes in the low end plus
/// canon-lower's undefined padding in the high end, which canon lift
/// on the receiving side truncates back to the arm's own type. The
/// bump allocator keeps the padding zero-initialised, so lift's
/// widened view is canonical in practice.
fn walk_val_type(
    type_id: ValueTypeId,
    slots: &mut Vec<FlatSlot>,
    base_offset: u32,
    arena: &TypeArena,
) {
    let (_, align) = canonical_size_and_align(type_id, arena);
    let start = align_to_val(base_offset, align);

    match arena.lookup_val(type_id) {
        // Primitives — one slot each, shape derived from the type
        // itself via [`primitive_shape`].
        ValueType::Bool
        | ValueType::U8
        | ValueType::S8
        | ValueType::U16
        | ValueType::S16
        | ValueType::U32
        | ValueType::S32
        | ValueType::Char
        | ValueType::U64
        | ValueType::S64
        | ValueType::F32
        | ValueType::F64 => {
            slots.push(FlatSlot {
                byte_offset: start,
                shape: primitive_shape(arena.lookup_val(type_id)),
            });
        }
        // (ptr, len) pairs — two aligned i32 words.
        ValueType::String | ValueType::List(_) | ValueType::Map(..) => {
            slots.push(FlatSlot {
                byte_offset: start,
                shape: FlatSlotShape::I32,
            });
            slots.push(FlatSlot {
                byte_offset: start + 4,
                shape: FlatSlotShape::I32,
            });
        }
        // Fixed-size list: N copies of the element, inline.
        ValueType::FixedSizeList(inner, n) => {
            let (inner_size, inner_align) = canonical_size_and_align(*inner, arena);
            for i in 0..*n {
                let elem_offset = align_to_val(start, inner_align) + i * inner_size;
                walk_val_type(*inner, slots, elem_offset, arena);
            }
        }
        // Handles — 4-byte i32 values in memory.
        ValueType::Resource(_) | ValueType::AsyncHandle | ValueType::ErrorContext => {
            slots.push(FlatSlot {
                byte_offset: start,
                shape: FlatSlotShape::I32,
            });
        }
        // Records / tuples: fields in declaration order, each at its
        // own canonical alignment from the running cursor.
        ValueType::Record(fields) => {
            let mut cursor = start;
            for (_, fid) in fields {
                let (fs, fa) = canonical_size_and_align(*fid, arena);
                cursor = align_to_val(cursor, fa);
                walk_val_type(*fid, slots, cursor, arena);
                cursor += fs;
            }
        }
        ValueType::Tuple(ids) => {
            let mut cursor = start;
            for tid in ids {
                let (ts, ta) = canonical_size_and_align(*tid, arena);
                cursor = align_to_val(cursor, ta);
                walk_val_type(*tid, slots, cursor, arena);
                cursor += ts;
            }
        }
        // `option<T>` — 1-byte disc, then T at `align_to(1, align(T))`.
        ValueType::Option(inner) => {
            slots.push(FlatSlot {
                byte_offset: start,
                shape: FlatSlotShape::U8,
            });
            let (_, payload_align) = canonical_size_and_align(*inner, arena);
            let payload_start = align_to_val(start + 1, payload_align);
            walk_val_type(*inner, slots, payload_start, arena);
        }
        // `result<T, E>` — 1-byte disc, then the payload region at
        // `align_to(1, max(align(T), align(E)))`. Payload walk is
        // [`walk_discriminated_payload`]: if both arms have the same
        // flat shape, the representative arm's canonical layout is
        // used (preserves subword load widths); otherwise we fall
        // back to the JOINED flat types at natural alignment, which
        // is what `task.return`'s core signature expects.
        ValueType::Result { ok, err } => {
            slots.push(FlatSlot {
                byte_offset: start,
                shape: FlatSlotShape::U8,
            });
            let ok_a = ok
                .map(|id| canonical_size_and_align(id, arena).1)
                .unwrap_or(1);
            let err_a = err
                .map(|id| canonical_size_and_align(id, arena).1)
                .unwrap_or(1);
            let payload_start = align_to_val(start + 1, ok_a.max(err_a));
            let arms: Vec<ValueTypeId> = [*ok, *err].into_iter().flatten().collect();
            walk_discriminated_payload(&arms, slots, payload_start, arena);
        }
        // `variant<cases>` — disc of size 1/2/4, then the payload
        // region via [`walk_discriminated_payload`] (same homogeneous-
        // vs-heterogeneous dispatch as `result`).
        ValueType::Variant(cases) => {
            let disc_size = discriminant_align(cases.len());
            slots.push(FlatSlot {
                byte_offset: start,
                shape: int_shape(disc_size),
            });
            let payload_align = cases
                .iter()
                .filter_map(|(_, opt)| opt.map(|id| canonical_size_and_align(id, arena).1))
                .max()
                .unwrap_or(1);
            let payload_start = align_to_val(start + disc_size, payload_align);
            let arms: Vec<ValueTypeId> = cases.iter().filter_map(|(_, opt)| *opt).collect();
            walk_discriminated_payload(&arms, slots, payload_start, arena);
        }
        // `enum` — just the discriminant, no payload.
        ValueType::Enum(tags) => {
            slots.push(FlatSlot {
                byte_offset: start,
                shape: int_shape(discriminant_align(tags.len())),
            });
        }
        // `flags<labels>` — 1/2/4 bytes for ≤32 labels (one `i32`-family
        // slot with width matching the label count), then
        // `ceil(n/32)` full i32 words for >32.
        ValueType::Flags(names) => {
            let n = names.len();
            if n == 0 {
                // Empty flags occupy zero bytes — no slot.
            } else if n <= 32 {
                // 1/2/4-byte single slot — width tracks label count.
                let bytes = if n <= 8 {
                    1
                } else if n <= 16 {
                    2
                } else {
                    4
                };
                slots.push(FlatSlot {
                    byte_offset: start,
                    shape: int_shape(bytes),
                });
            } else {
                let n_words = (n as u32).div_ceil(32);
                for i in 0..n_words {
                    slots.push(FlatSlot {
                        byte_offset: start + i * 4,
                        shape: FlatSlotShape::I32,
                    });
                }
            }
        }
    }
}

/// Map a primitive `ValueType` to its [`FlatSlotShape`]. Panics on
/// non-primitives — the walker only calls this when it's already
/// matched on a primitive arm.
fn primitive_shape(vt: &ValueType) -> FlatSlotShape {
    match vt {
        ValueType::Bool | ValueType::U8 | ValueType::S8 => FlatSlotShape::U8,
        ValueType::U16 | ValueType::S16 => FlatSlotShape::U16,
        ValueType::U32 | ValueType::S32 | ValueType::Char => FlatSlotShape::I32,
        ValueType::U64 | ValueType::S64 => FlatSlotShape::I64,
        ValueType::F32 => FlatSlotShape::F32,
        ValueType::F64 => FlatSlotShape::F64,
        other => panic!("primitive_shape: {other:?} is not a primitive ValueType"),
    }
}

/// Map a 1/2/4-byte subword-integer width to a [`FlatSlotShape`].
/// Used for discriminators (`option` / `result` / `variant` /
/// `enum`) and sub-i32 `flags` storage widths.
fn int_shape(byte_size: u32) -> FlatSlotShape {
    match byte_size {
        1 => FlatSlotShape::U8,
        2 => FlatSlotShape::U16,
        4 => FlatSlotShape::I32,
        other => panic!("int_shape: {other} is not a valid subword-integer byte size"),
    }
}

/// Emit slots for the payload region of a discriminated type
/// (`result` / `variant`) given the arms with payloads.
///
/// Two strategies depending on whether the arms are homogeneous in
/// their flat representation:
///
/// - **Homogeneous** (all arms produce the same `flat_types_for`):
///   walk one arm's canonical layout. Each slot's `byte_offset`
///   matches `canon lower`'s write offsets exactly, and subword
///   loads (`i32.load8_u` / `i32.load16_u`) stay narrow — the
///   correct behavior for `Option<T>` (single arm),
///   `Result<u8, u8>`, and similar cases where the active arm
///   doesn't matter.
///
/// - **Heterogeneous** (at least one arm's flat differs, typically
///   in an `i64` vs `i32` slot): fall back to laying out the JOINED
///   flat types sequentially at natural alignment from
///   `payload_start`. The shapes we emit match what
///   `task.return`'s core signature expects (the joined types,
///   including `i64` where arms diverge). Subword canonical offsets
///   for narrower arms don't line up in this mode, so runtime
///   values for those arms may read trailing padding instead of the
///   active arm's bytes — a known limitation pending the
///   runtime-dispatch follow-up documented in
///   `docs/adapter-comp-planning.md`.
fn walk_discriminated_payload(
    arms: &[ValueTypeId],
    slots: &mut Vec<FlatSlot>,
    payload_start: u32,
    arena: &TypeArena,
) {
    if arms.is_empty() {
        return;
    }

    let arm_flats: Vec<Vec<ValType>> = arms.iter().map(|id| flat_types_for(*id, arena)).collect();
    let homogeneous = arm_flats.windows(2).all(|pair| pair[0] == pair[1]);

    if homogeneous {
        // All arms agree on flat shape — walk one canonically so
        // subword offsets and load widths stay correct.
        walk_val_type(arms[0], slots, payload_start, arena);
        return;
    }

    // Heterogeneous: emit slots matching the JOINED flat types so
    // `task.return`'s core signature validates. Each slot is placed
    // at natural alignment from `payload_start`; size falls out of
    // `val_type_byte_size`.
    let joined = join_flat_lists(&arm_flats);
    let mut cursor = payload_start;
    for vt in joined {
        let size = val_type_byte_size(&vt);
        cursor = align_to_val(cursor, size);
        slots.push(FlatSlot {
            byte_offset: cursor,
            shape: joined_flat_shape(vt),
        });
        cursor += size;
    }
}

/// Translate a JOINED flat `ValType` (post-widening) to its
/// [`FlatSlotShape`]. Joined flats don't carry subword info — they're
/// always full-width i32/i64/f32/f64. Size falls out of
/// [`val_type_byte_size`] at the call site.
fn joined_flat_shape(vt: ValType) -> FlatSlotShape {
    match vt {
        ValType::I32 => FlatSlotShape::I32,
        ValType::I64 => FlatSlotShape::I64,
        ValType::F32 => FlatSlotShape::F32,
        ValType::F64 => FlatSlotShape::F64,
        other => panic!("joined_flat_shape: {other:?} not expected in joined flat"),
    }
}

/// Runtime trap gate for heterogeneous-variant types used as
/// top-level task.return results.
///
/// When a type is a `result` / `variant` whose arms disagree on flat
/// shape, our heterogeneous [`FlatLayout`] fallback places slots at
/// joined-flat natural-alignment offsets from `payload_start`. For
/// each arm, those offsets do or don't match that arm's canonical
/// write offsets — and for most heterogeneous variants only some
/// arms line up. Arms that DON'T line up would produce wrong
/// runtime values; we trap instead.
///
/// For now the guard is conservative: at most one arm is considered
/// safe — the arm whose canonical layout (starting at `payload_start`)
/// matches our heterogeneous fallback's slot offsets AND whose flat
/// at each position equals the joined flat at that position. When
/// exactly one such arm exists, its disc value is the `allowed_disc`;
/// the dispatch emitter traps any other disc. When none is safe, the
/// guard's `allowed_disc` is unreachable and every disc traps.
///
/// See `docs/TODO/test-with-real-compositions.md` — expanding this
/// to multiple safe arms (or full runtime dispatch) is follow-up
/// work.
#[derive(Clone, Copy, Debug)]
pub(super) struct HeterogeneityGuard {
    /// Byte offset of the top-level disc (always 0 for top-level
    /// variants/results, but kept explicit for clarity).
    pub disc_offset: u32,
    /// Load shape for the disc (`U8` / `U16` / `I32` depending on
    /// arm count).
    pub disc_shape: FlatSlotShape,
    /// The one disc value that proceeds without trapping. `None` if
    /// no arm is safe — every disc traps.
    pub allowed_disc: Option<u32>,
}

/// If `type_id` is a top-level `result` / `variant` with
/// heterogeneous arms, return a [`HeterogeneityGuard`] the dispatch
/// emitter uses to insert a runtime trap for unsafe arms. Returns
/// `None` for homogeneous arms (or non-variant-shaped types) —
/// nothing to guard.
pub(super) fn top_level_heterogeneity_guard(
    type_id: ValueTypeId,
    arena: &TypeArena,
) -> Option<HeterogeneityGuard> {
    // Collect arm ids for the top-level type. Option is always
    // single-arm → homogeneous by construction.
    let (arms, disc_cases): (Vec<ValueTypeId>, usize) = match arena.lookup_val(type_id) {
        ValueType::Result { ok, err } => ([*ok, *err].into_iter().flatten().collect(), 2),
        ValueType::Variant(cases) => {
            let arms: Vec<ValueTypeId> = cases.iter().filter_map(|(_, o)| *o).collect();
            (arms, cases.len())
        }
        _ => return None,
    };

    if arms.is_empty() {
        return None;
    }

    let arm_flats: Vec<Vec<ValType>> = arms.iter().map(|id| flat_types_for(*id, arena)).collect();
    let homogeneous = arm_flats.windows(2).all(|pair| pair[0] == pair[1]);
    if homogeneous {
        return None;
    }

    // Heterogeneous. Identify the single safe arm (if any): arm
    // whose flat equals the joined flat in length AND whose
    // canonical slot offsets match our heterogeneous fallback's
    // joined-natural-alignment offsets starting from `payload_start`.
    //
    // For now we take a narrow definition of "safe": arm flat has
    // length ≤ 1 (i.e., primitive or handle) — in that case the
    // single slot is at payload_start under both layouts, so it
    // always aligns. This covers the `ok` arm of typical
    // `result<simple_handle, complex_err>` cases (e.g.
    // `wasi:http/handler`'s return type). Broader safety detection
    // is follow-up.
    //
    // NOTE: The disc-value assignment maps arm INDEX to disc VALUE
    // per canonical ABI (case 0 = disc 0, case 1 = disc 1, …). For
    // `result`, arm order is `[ok, err]` → disc 0 for ok, 1 for err.
    // For `variant`, arm index = case index.
    let safe_disc = (0..arms.len()).find(|&i| {
        // Map arm-index (among arms-with-payload) back to case-index —
        // for result<T, E>, arms_with_payload equals cases, so index
        // matches disc. For variant, skipping None arms means we
        // can't simply use arm index as disc. Fall back to
        // "only find safe when length-1 arm is at position 0"
        // for now; Variant ordering with None arms is deferred.
        arm_flats[i].len() <= 1
    });

    let disc_shape = int_shape(discriminant_align(disc_cases));
    Some(HeterogeneityGuard {
        disc_offset: 0,
        disc_shape,
        allowed_disc: safe_disc.map(|i| i as u32),
    })
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
