//! Binary decoder for streamed `FieldTree` events. Counterpart to
//! [`super::encode`]; reads what the encoder writes.
//!
//! Paired with `wire-format.md`; keep decoding logic in sync.

use std::fmt;

use super::encode::{
    Tag, MAGIC, OPTIONAL_ABSENT, OPTIONAL_PRESENT, PHASE_CALL, PHASE_RETURN, VERSION,
};
use crate::types::{
    CallId, Cell, EnumInfo, Field, FieldTree, FlagsInfo, HandleInfo, RecordInfo, VariantInfo,
};

/// Cap on `Vec::with_capacity` for length-prefixed reads, so a
/// malformed length prefix can't force a huge upfront allocation.
const MAX_PREALLOC: usize = 64;

/// One decoded event from a recorder stream.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Call {
        ts_ns: u64,
        call: CallId,
        args: Vec<Field>,
    },
    Return {
        ts_ns: u64,
        call: CallId,
        result: Option<FieldTree>,
    },
}

/// Failure modes when parsing a recorder stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Stream header didn't start with the expected `b"SPLR"` magic.
    BadMagic([u8; 4]),
    /// Header version isn't one this decoder understands.
    UnsupportedVersion(u32),
    /// Event `phase` byte wasn't 0 (call) or 1 (return).
    InvalidPhase(u8),
    /// Cell tag byte wasn't in `0..=18`.
    InvalidTag(u8),
    /// A `(u32 payload_idx)?` or `(field_tree result_tree)?` discriminator
    /// wasn't 0 or 1.
    InvalidOptionalFlag(u8),
    /// String bytes weren't valid UTF-8.
    InvalidUtf8,
    /// Read needed more bytes than the stream contained.
    Truncated,
    /// `rec_len` claimed more bytes than the stream had remaining,
    /// or the framed event ran past its own `rec_len`.
    FramingMismatch,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic(m) => write!(f, "bad stream magic: {m:?}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported wire version: {v}"),
            Self::InvalidPhase(p) => write!(f, "invalid phase byte: {p}"),
            Self::InvalidTag(t) => write!(f, "invalid cell tag: {t}"),
            Self::InvalidOptionalFlag(b) => write!(f, "invalid optional-flag byte: {b}"),
            Self::InvalidUtf8 => write!(f, "string bytes are not valid utf-8"),
            Self::Truncated => write!(f, "stream truncated mid-record"),
            Self::FramingMismatch => write!(f, "event body overran or underran its rec_len"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Streaming reader over recorder bytes. Validates the header on
/// construction; `.next()` yields one event per call until end of input.
#[derive(Debug)]
pub struct Reader<'a> {
    cur: Cursor<'a>,
    done: bool,
}

impl<'a> Reader<'a> {
    /// Validate the stream header (magic + version) and return a reader
    /// positioned at the first event.
    pub fn new(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        let mut cur = Cursor::new(bytes);
        let magic = cur.read_array::<4>()?;
        if &magic != MAGIC {
            return Err(DecodeError::BadMagic(magic));
        }
        let version = cur.read_u32()?;
        if version != VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        Ok(Self { cur, done: false })
    }
}

impl<'a> Iterator for Reader<'a> {
    type Item = Result<Event, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        // Clean EOF: nothing left to read.
        if self.cur.remaining() == 0 {
            return None;
        }
        // Partial trailing bytes (less than a complete rec_len) count as
        // EOF per the wire-format spec, not as an error.
        if self.cur.remaining() < 4 {
            self.done = true;
            return None;
        }
        let rec_len = match self.cur.read_u32() {
            Ok(n) => n as usize,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        if self.cur.remaining() < rec_len {
            // Truncated mid-event: per spec, drop the partial bytes.
            self.done = true;
            return None;
        }
        // Carve out the framed body so a body-level parse error can't
        // leak past the event boundary.
        let body_start = self.cur.pos;
        let body_end = body_start + rec_len;
        let mut body = Cursor::new(&self.cur.bytes[body_start..body_end]);
        let event = decode_event_body(&mut body);
        match event {
            Ok(ev) if body.remaining() == 0 => {
                self.cur.pos = body_end;
                Some(Ok(ev))
            }
            Ok(_) => {
                // Body parsed but didn't consume all its rec_len bytes.
                self.done = true;
                Some(Err(DecodeError::FramingMismatch))
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

fn decode_event_body(cur: &mut Cursor<'_>) -> Result<Event, DecodeError> {
    let phase = cur.read_u8()?;
    let ts_ns = cur.read_u64()?;
    let call_id = cur.read_u64()?;
    let interface_name = cur.read_str()?;
    let function_name = cur.read_str()?;
    let call = CallId {
        interface_name,
        function_name,
        id: call_id,
    };
    match phase {
        PHASE_CALL => {
            let n_args = cur.read_u32()? as usize;
            let mut args = Vec::with_capacity(n_args.min(MAX_PREALLOC));
            for _ in 0..n_args {
                let name = cur.read_str()?;
                let tree = read_field_tree(cur)?;
                args.push(Field { name, tree });
            }
            Ok(Event::Call { ts_ns, call, args })
        }
        PHASE_RETURN => {
            let result = match cur.read_u8()? {
                OPTIONAL_ABSENT => None,
                OPTIONAL_PRESENT => Some(read_field_tree(cur)?),
                bad => return Err(DecodeError::InvalidOptionalFlag(bad)),
            };
            Ok(Event::Return { ts_ns, call, result })
        }
        bad => Err(DecodeError::InvalidPhase(bad)),
    }
}

fn read_field_tree(cur: &mut Cursor<'_>) -> Result<FieldTree, DecodeError> {
    let n_cells = cur.read_u32()? as usize;
    let mut cells = Vec::with_capacity(n_cells.min(MAX_PREALLOC));
    for _ in 0..n_cells {
        cells.push(read_cell(cur)?);
    }
    let n_records = cur.read_u32()? as usize;
    let mut record_infos = Vec::with_capacity(n_records.min(MAX_PREALLOC));
    for _ in 0..n_records {
        record_infos.push(read_record_info(cur)?);
    }
    let n_flags = cur.read_u32()? as usize;
    let mut flags_infos = Vec::with_capacity(n_flags.min(MAX_PREALLOC));
    for _ in 0..n_flags {
        flags_infos.push(read_flags_info(cur)?);
    }
    let n_enums = cur.read_u32()? as usize;
    let mut enum_infos = Vec::with_capacity(n_enums.min(MAX_PREALLOC));
    for _ in 0..n_enums {
        enum_infos.push(read_enum_info(cur)?);
    }
    let n_variants = cur.read_u32()? as usize;
    let mut variant_infos = Vec::with_capacity(n_variants.min(MAX_PREALLOC));
    for _ in 0..n_variants {
        variant_infos.push(read_variant_info(cur)?);
    }
    let n_handles = cur.read_u32()? as usize;
    let mut handle_infos = Vec::with_capacity(n_handles.min(MAX_PREALLOC));
    for _ in 0..n_handles {
        handle_infos.push(read_handle_info(cur)?);
    }
    let root = cur.read_u32()?;
    Ok(FieldTree {
        cells,
        record_infos,
        flags_infos,
        enum_infos,
        variant_infos,
        handle_infos,
        root,
    })
}

fn read_cell(cur: &mut Cursor<'_>) -> Result<Cell, DecodeError> {
    let tag_byte = cur.read_u8()?;
    let tag = Tag::try_from(tag_byte).map_err(DecodeError::InvalidTag)?;
    Ok(match tag {
        Tag::Bool => Cell::Bool(cur.read_u8()? != 0),
        Tag::Integer => Cell::Integer(cur.read_i64()?),
        Tag::Floating => Cell::Floating(cur.read_f64()?),
        Tag::Text => Cell::Text(cur.read_str()?),
        Tag::Bytes => {
            let n = cur.read_u32()? as usize;
            Cell::Bytes(cur.read_bytes(n)?.to_vec())
        }
        Tag::ListOf => Cell::ListOf(cur.read_u32_list()?),
        Tag::TupleOf => Cell::TupleOf(cur.read_u32_list()?),
        Tag::OptionSome => Cell::OptionSome(cur.read_u32()?),
        Tag::OptionNone => Cell::OptionNone,
        Tag::ResultOk => Cell::ResultOk(read_optional_u32(cur)?),
        Tag::ResultErr => Cell::ResultErr(read_optional_u32(cur)?),
        Tag::RecordOf => Cell::RecordOf(cur.read_u32()?),
        Tag::FlagsSet => Cell::FlagsSet(cur.read_u32()?),
        Tag::EnumCase => Cell::EnumCase(cur.read_u32()?),
        Tag::VariantCase => Cell::VariantCase(cur.read_u32()?),
        Tag::ResourceHandle => Cell::ResourceHandle(cur.read_u32()?),
        Tag::StreamHandle => Cell::StreamHandle(cur.read_u32()?),
        Tag::FutureHandle => Cell::FutureHandle(cur.read_u32()?),
        Tag::ErrorContextHandle => Cell::ErrorContextHandle(cur.read_u32()?),
    })
}

fn read_optional_u32(cur: &mut Cursor<'_>) -> Result<Option<u32>, DecodeError> {
    match cur.read_u8()? {
        OPTIONAL_ABSENT => Ok(None),
        OPTIONAL_PRESENT => Ok(Some(cur.read_u32()?)),
        bad => Err(DecodeError::InvalidOptionalFlag(bad)),
    }
}

fn read_record_info(cur: &mut Cursor<'_>) -> Result<RecordInfo, DecodeError> {
    let type_name = cur.read_str()?;
    let n = cur.read_u32()? as usize;
    let mut fields = Vec::with_capacity(n.min(MAX_PREALLOC));
    for _ in 0..n {
        let name = cur.read_str()?;
        let idx = cur.read_u32()?;
        fields.push((name, idx));
    }
    Ok(RecordInfo { type_name, fields })
}

fn read_flags_info(cur: &mut Cursor<'_>) -> Result<FlagsInfo, DecodeError> {
    let type_name = cur.read_str()?;
    let n = cur.read_u32()? as usize;
    let mut set_flags = Vec::with_capacity(n.min(MAX_PREALLOC));
    for _ in 0..n {
        set_flags.push(cur.read_str()?);
    }
    Ok(FlagsInfo { type_name, set_flags })
}

fn read_enum_info(cur: &mut Cursor<'_>) -> Result<EnumInfo, DecodeError> {
    Ok(EnumInfo {
        type_name: cur.read_str()?,
        case_name: cur.read_str()?,
    })
}

fn read_variant_info(cur: &mut Cursor<'_>) -> Result<VariantInfo, DecodeError> {
    Ok(VariantInfo {
        type_name: cur.read_str()?,
        case_name: cur.read_str()?,
        payload: read_optional_u32(cur)?,
    })
}

fn read_handle_info(cur: &mut Cursor<'_>) -> Result<HandleInfo, DecodeError> {
    Ok(HandleInfo {
        type_name: cur.read_str()?,
        id: cur.read_u64()?,
    })
}

/// Byte cursor with typed primitive reads. Underflow returns
/// [`DecodeError::Truncated`].
#[derive(Debug)]
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::Truncated);
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let slice = self.read_bytes(N)?;
        // SAFETY: slice has length N (guaranteed by read_bytes).
        Ok(slice.try_into().unwrap())
    }

    fn read_u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.read_array::<1>()?[0])
    }

    fn read_u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.read_array()?))
    }

    fn read_u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.read_array()?))
    }

    fn read_i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.read_array()?))
    }

    fn read_f64(&mut self) -> Result<f64, DecodeError> {
        Ok(f64::from_le_bytes(self.read_array()?))
    }

    fn read_str(&mut self) -> Result<String, DecodeError> {
        let n = self.read_u32()? as usize;
        let bytes = self.read_bytes(n)?;
        std::str::from_utf8(bytes)
            .map(|s| s.to_string())
            .map_err(|_| DecodeError::InvalidUtf8)
    }

    fn read_u32_list(&mut self) -> Result<Vec<u32>, DecodeError> {
        let n = self.read_u32()? as usize;
        let mut out = Vec::with_capacity(n.min(MAX_PREALLOC));
        for _ in 0..n {
            out.push(self.read_u32()?);
        }
        Ok(out)
    }
}
