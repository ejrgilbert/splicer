//! Binary encoder for streamed `FieldTree` events. Wire format
//! documented in `wire-format.md` at the crate root.
//!
//! Paired with `wire-format.md`; keep encoding logic in sync.

use crate::types::{
    CallId, Cell, EnumInfo, Field, FieldTree, FlagsInfo, HandleInfo, RecordInfo, VariantInfo,
};

/// 4-byte ASCII magic at the start of every stream.
pub const MAGIC: &[u8; 4] = b"SPLR";

/// Wire-format version emitted by this encoder.
pub const VERSION: u32 = 1;

/// Phase discriminant for an `on-call` event.
pub const PHASE_CALL: u8 = 0;

/// Phase discriminant for an `on-return` event.
pub const PHASE_RETURN: u8 = 1;

/// Wire discriminator byte for the absent arm of an optional field
/// (`option-none`, `has_result == 0`, `has_payload == 0`).
pub(crate) const OPTIONAL_ABSENT: u8 = 0;

/// Wire discriminator byte for the present arm of an optional field
/// (`option-some`, `has_result == 1`, `has_payload == 1`).
pub(crate) const OPTIONAL_PRESENT: u8 = 1;

/// On-wire width of the `rec_len` event-frame length prefix.
const REC_LEN_FIELD_BYTES: usize = std::mem::size_of::<u32>();

/// Cell-variant discriminants on the wire. Variant names mirror
/// `splicer:common.cell` exactly so each match arm in `write_cell`
/// reads as a name-paired mapping (`Cell::Bool` to `Tag::Bool`).
/// Reordering arms cannot shift tags because each is pinned to its
/// numeric discriminant.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tag {
    Bool = 0,
    Integer = 1,
    Floating = 2,
    Text = 3,
    Bytes = 4,
    ListOf = 5,
    TupleOf = 6,
    OptionSome = 7,
    OptionNone = 8,
    ResultOk = 9,
    ResultErr = 10,
    RecordOf = 11,
    FlagsSet = 12,
    EnumCase = 13,
    VariantCase = 14,
    ResourceHandle = 15,
    StreamHandle = 16,
    FutureHandle = 17,
    ErrorContextHandle = 18,
}

impl TryFrom<u8> for Tag {
    type Error = u8;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Tag::Bool),
            1 => Ok(Tag::Integer),
            2 => Ok(Tag::Floating),
            3 => Ok(Tag::Text),
            4 => Ok(Tag::Bytes),
            5 => Ok(Tag::ListOf),
            6 => Ok(Tag::TupleOf),
            7 => Ok(Tag::OptionSome),
            8 => Ok(Tag::OptionNone),
            9 => Ok(Tag::ResultOk),
            10 => Ok(Tag::ResultErr),
            11 => Ok(Tag::RecordOf),
            12 => Ok(Tag::FlagsSet),
            13 => Ok(Tag::EnumCase),
            14 => Ok(Tag::VariantCase),
            15 => Ok(Tag::ResourceHandle),
            16 => Ok(Tag::StreamHandle),
            17 => Ok(Tag::FutureHandle),
            18 => Ok(Tag::ErrorContextHandle),
            bad => Err(bad),
        }
    }
}

/// Append the stream header (magic + version) to `out`. Call once per
/// stream, before any events.
pub fn write_stream_header(out: &mut Vec<u8>) {
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
}

/// Append a fully framed `on-call` event to `out`.
pub fn write_call_event(out: &mut Vec<u8>, ts_ns: u64, call: &CallId, args: &[Field]) {
    let rec_len_pos = reserve_rec_len(out);
    out.push(PHASE_CALL);
    write_event_prefix(out, ts_ns, call);
    write_len(out, args.len());
    for arg in args {
        write_str(out, &arg.name);
        write_field_tree(out, &arg.tree);
    }
    patch_rec_len(out, rec_len_pos);
}

/// Append a fully framed `on-return` event to `out`. `result` is
/// `None` for void functions; `Some` carries the lifted result tree.
pub fn write_return_event(out: &mut Vec<u8>, ts_ns: u64, call: &CallId, result: Option<&FieldTree>) {
    let rec_len_pos = reserve_rec_len(out);
    out.push(PHASE_RETURN);
    write_event_prefix(out, ts_ns, call);
    match result {
        Some(tree) => {
            out.push(OPTIONAL_PRESENT);
            write_field_tree(out, tree);
        }
        None => out.push(OPTIONAL_ABSENT),
    }
    patch_rec_len(out, rec_len_pos);
}

/// Append a `field_tree` (cells + side tables + root) to `out`. Useful
/// on its own for tooling that captures single trees without the
/// event framing.
pub fn write_field_tree(out: &mut Vec<u8>, tree: &FieldTree) {
    write_len(out, tree.cells.len());
    for cell in &tree.cells {
        write_cell(out, cell);
    }
    write_len(out, tree.record_infos.len());
    for info in &tree.record_infos {
        write_record_info(out, info);
    }
    write_len(out, tree.flags_infos.len());
    for info in &tree.flags_infos {
        write_flags_info(out, info);
    }
    write_len(out, tree.enum_infos.len());
    for info in &tree.enum_infos {
        write_enum_info(out, info);
    }
    write_len(out, tree.variant_infos.len());
    for info in &tree.variant_infos {
        write_variant_info(out, info);
    }
    write_len(out, tree.handle_infos.len());
    for info in &tree.handle_infos {
        write_handle_info(out, info);
    }
    write_u32(out, tree.root);
}

fn write_event_prefix(out: &mut Vec<u8>, ts_ns: u64, call: &CallId) {
    out.extend_from_slice(&ts_ns.to_le_bytes());
    out.extend_from_slice(&call.id.to_le_bytes());
    write_str(out, &call.interface_name);
    write_str(out, &call.function_name);
}

fn reserve_rec_len(out: &mut Vec<u8>) -> usize {
    let pos = out.len();
    out.extend_from_slice(&[0u8; REC_LEN_FIELD_BYTES]);
    pos
}

fn patch_rec_len(out: &mut Vec<u8>, rec_len_pos: usize) {
    let body_len: u32 = (out.len() - rec_len_pos - REC_LEN_FIELD_BYTES)
        .try_into()
        .expect("event body length exceeds u32::MAX");
    out[rec_len_pos..rec_len_pos + REC_LEN_FIELD_BYTES].copy_from_slice(&body_len.to_le_bytes());
}

fn write_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}

/// Write a `usize` length as a u32, panicking explicitly on overflow.
/// Centralized so every `usize → u32` narrowing on the wire goes
/// through the same checked path.
fn write_len(out: &mut Vec<u8>, len: usize) {
    let n: u32 = len.try_into().expect("length exceeds u32::MAX");
    write_u32(out, n);
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    write_len(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

fn write_u32_list(out: &mut Vec<u8>, xs: &[u32]) {
    write_len(out, xs.len());
    for x in xs {
        write_u32(out, *x);
    }
}

fn write_optional_u32(out: &mut Vec<u8>, x: &Option<u32>) {
    match x {
        Some(v) => {
            out.push(OPTIONAL_PRESENT);
            write_u32(out, *v);
        }
        None => out.push(OPTIONAL_ABSENT),
    }
}

fn write_cell(out: &mut Vec<u8>, cell: &Cell) {
    match cell {
        Cell::Bool(b) => {
            out.push(Tag::Bool as u8);
            out.push(*b as u8);
        }
        Cell::Integer(n) => {
            out.push(Tag::Integer as u8);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Cell::Floating(f) => {
            out.push(Tag::Floating as u8);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Cell::Text(s) => {
            out.push(Tag::Text as u8);
            write_str(out, s);
        }
        Cell::Bytes(b) => {
            out.push(Tag::Bytes as u8);
            write_len(out, b.len());
            out.extend_from_slice(b);
        }
        Cell::ListOf(children) => {
            out.push(Tag::ListOf as u8);
            write_u32_list(out, children);
        }
        Cell::TupleOf(children) => {
            out.push(Tag::TupleOf as u8);
            write_u32_list(out, children);
        }
        Cell::OptionSome(idx) => {
            out.push(Tag::OptionSome as u8);
            write_u32(out, *idx);
        }
        Cell::OptionNone => out.push(Tag::OptionNone as u8),
        Cell::ResultOk(payload) => {
            out.push(Tag::ResultOk as u8);
            write_optional_u32(out, payload);
        }
        Cell::ResultErr(payload) => {
            out.push(Tag::ResultErr as u8);
            write_optional_u32(out, payload);
        }
        Cell::RecordOf(idx) => {
            out.push(Tag::RecordOf as u8);
            write_u32(out, *idx);
        }
        Cell::FlagsSet(idx) => {
            out.push(Tag::FlagsSet as u8);
            write_u32(out, *idx);
        }
        Cell::EnumCase(idx) => {
            out.push(Tag::EnumCase as u8);
            write_u32(out, *idx);
        }
        Cell::VariantCase(idx) => {
            out.push(Tag::VariantCase as u8);
            write_u32(out, *idx);
        }
        Cell::ResourceHandle(idx) => {
            out.push(Tag::ResourceHandle as u8);
            write_u32(out, *idx);
        }
        Cell::StreamHandle(idx) => {
            out.push(Tag::StreamHandle as u8);
            write_u32(out, *idx);
        }
        Cell::FutureHandle(idx) => {
            out.push(Tag::FutureHandle as u8);
            write_u32(out, *idx);
        }
        Cell::ErrorContextHandle(idx) => {
            out.push(Tag::ErrorContextHandle as u8);
            write_u32(out, *idx);
        }
    }
}

fn write_record_info(out: &mut Vec<u8>, info: &RecordInfo) {
    write_str(out, &info.type_name);
    write_len(out, info.fields.len());
    for (name, idx) in &info.fields {
        write_str(out, name);
        write_u32(out, *idx);
    }
}

fn write_flags_info(out: &mut Vec<u8>, info: &FlagsInfo) {
    write_str(out, &info.type_name);
    write_len(out, info.set_flags.len());
    for f in &info.set_flags {
        write_str(out, f);
    }
}

fn write_enum_info(out: &mut Vec<u8>, info: &EnumInfo) {
    write_str(out, &info.type_name);
    write_str(out, &info.case_name);
}

fn write_variant_info(out: &mut Vec<u8>, info: &VariantInfo) {
    write_str(out, &info.type_name);
    write_str(out, &info.case_name);
    write_optional_u32(out, &info.payload);
}

fn write_handle_info(out: &mut Vec<u8>, info: &HandleInfo) {
    write_str(out, &info.type_name);
    out.extend_from_slice(&info.id.to_le_bytes());
}
