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
//! - **Layout** ([`canonical_align`], [`disc_align`],
//!   [`align_to_val`]) — alignment math for the canonical ABI.
//! - **Resources** ([`collect_resource_ids`],
//!   [`collect_resources_rec`]) — walk a function's signatures
//!   and collect every resource type it touches, with deterministic
//!   names.
//! - **Conversion** ([`prim_cv`]) — turn a primitive `ValueType`
//!   into a `wasm_encoder::ComponentValType`.

use std::collections::HashMap;

use cviz::model::{TypeArena, ValueType, ValueTypeId};
use wasm_encoder::{ComponentValType, PrimitiveValType, ValType};

use super::func::AdapterFunc;

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
            let n_words = (names.len() + 31) / 32;
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
        ValueType::Record(fields) => fields
            .iter()
            .any(|(_, id)| type_has_strings(*id, arena)),
        ValueType::Tuple(ids) => ids.iter().any(|id| type_has_strings(*id, arena)),
        ValueType::Variant(cases) => cases
            .iter()
            .any(|(_, opt_id)| opt_id.map(|id| type_has_strings(id, arena)).unwrap_or(false)),
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
        ValueType::Record(fields) => fields
            .iter()
            .any(|(_, id)| type_has_resources(*id, arena)),
        ValueType::Tuple(ids) => ids.iter().any(|id| type_has_resources(*id, arena)),
        ValueType::Variant(cases) => cases
            .iter()
            .any(|(_, opt_id)| opt_id.map(|id| type_has_resources(id, arena)).unwrap_or(false)),
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
            std::cmp::max(disc_align(cases.len()), payload_align)
        }
        ValueType::Option(inner) => std::cmp::max(1, canonical_align(*inner, arena)),
        ValueType::Result { ok, err } => {
            let ok_a = ok.map(|id| canonical_align(id, arena)).unwrap_or(1);
            let err_a = err.map(|id| canonical_align(id, arena)).unwrap_or(1);
            std::cmp::max(1, std::cmp::max(ok_a, err_a))
        }
        ValueType::Enum(tags) => disc_align(tags.len()),
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

/// Discriminant alignment for a variant/enum with `n` cases.
pub(super) fn disc_align(n: usize) -> u32 {
    if n <= 256 {
        1
    } else if n <= 65536 {
        2
    } else {
        4
    }
}

/// Round `offset` up to the nearest multiple of `align`.
pub(super) fn align_to_val(offset: u32, align: u32) -> u32 {
    (offset + align - 1) & !(align - 1)
}

/// Collect all unique resource `ValueTypeId`s used (transitively) in
/// function signatures, with deterministic export names.
pub(super) fn collect_resource_ids(
    funcs: &[AdapterFunc],
    arena: &TypeArena,
) -> Vec<(ValueTypeId, String)> {
    let mut seen = HashMap::new();
    for func in funcs {
        for &id in &func.param_type_ids {
            collect_resources_rec(id, arena, &mut seen);
        }
        if let Some(id) = func.result_type_id {
            collect_resources_rec(id, arena, &mut seen);
        }
    }
    let mut out: Vec<(ValueTypeId, String)> = seen.into_iter().collect();
    out.sort_by(|a, b| a.1.cmp(&b.1)); // deterministic order
    out
}

pub(super) fn collect_resources_rec(
    id: ValueTypeId,
    arena: &TypeArena,
    seen: &mut HashMap<ValueTypeId, String>,
) {
    match arena.lookup_val(id) {
        ValueType::Resource(name) => {
            if !seen.contains_key(&id) {
                let export_name = if name.is_empty() {
                    format!("res-{}", seen.len())
                } else {
                    name.clone()
                };
                seen.insert(id, export_name);
            }
        }
        ValueType::AsyncHandle => {
            if !seen.contains_key(&id) {
                let export_name = format!("res-{}", seen.len());
                seen.insert(id, export_name);
            }
        }
        ValueType::Record(fields) => {
            for (_, fid) in fields {
                collect_resources_rec(*fid, arena, seen);
            }
        }
        ValueType::Tuple(ids) => {
            for fid in ids {
                collect_resources_rec(*fid, arena, seen);
            }
        }
        ValueType::Variant(cases) => {
            for (_, opt_id) in cases {
                if let Some(fid) = opt_id {
                    collect_resources_rec(*fid, arena, seen);
                }
            }
        }
        ValueType::Option(inner) => collect_resources_rec(*inner, arena, seen),
        ValueType::Result { ok, err } => {
            if let Some(oid) = ok {
                collect_resources_rec(*oid, arena, seen);
            }
            if let Some(eid) = err {
                collect_resources_rec(*eid, arena, seen);
            }
        }
        ValueType::List(inner) | ValueType::FixedSizeList(inner, _) => {
            collect_resources_rec(*inner, arena, seen);
        }
        _ => {}
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
