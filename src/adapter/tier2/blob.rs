//! Data-segment packing helpers for tier-2. Name-keyed record writes
//! over a schema-derived [`RecordLayout`], typed [`BlobSlice`]
//! pointer/length pairs, and a [`Segment`] / [`SymRef`] / [`Reloc`]
//! relocation model so segment placement order is commutative.

use std::collections::HashMap;

use super::super::abi::emit::{
    BlobSlice, RecordLayout, OPTION_NONE, OPTION_SOME, SLICE_LEN_OFFSET, SLICE_PTR_OFFSET,
};

/// Append-only string interner; the only way to obtain a `BlobSlice`
/// is `intern`, the only way to surface bytes is `into_bytes`. Repeat
/// `intern` of the same string returns the same slice (dedups).
pub(crate) struct NameInterner {
    bytes: Vec<u8>,
    seen: HashMap<String, BlobSlice>,
}

impl NameInterner {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            seen: HashMap::new(),
        }
    }

    /// Append `s` to the blob if not already present, returning the
    /// `(offset, len)` slice for it.
    pub(crate) fn intern(&mut self, s: &str) -> BlobSlice {
        if let Some(&slice) = self.seen.get(s) {
            return slice;
        }
        let slice = BlobSlice {
            off: self.bytes.len() as u32,
            len: s.len() as u32,
        };
        self.bytes.extend_from_slice(s.as_bytes());
        self.seen.insert(s.to_string(), slice);
        slice
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Names a future data-segment base address.
pub(crate) type SymbolId = u32;

/// One pending pointer write. After segments have bases, layout writes
/// `bases[target] + addend` as LE i32 at `segment_base + site`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Reloc {
    pub(crate) site: u32,
    pub(crate) target: SymbolId,
    pub(crate) addend: i32,
}

/// One bytes-and-relocs unit handed to the layout phase.
pub(crate) struct Segment {
    pub(crate) id: SymbolId,
    pub(crate) align: u32,
    pub(crate) bytes: Vec<u8>,
    pub(crate) relocs: Vec<Reloc>,
}

/// A `(ptr, len)` pair into segment `target` at relative `off`.
/// `resolve` consumes the symbolic form (typed "translate twice" check).
/// `None` resolves to `BlobSlice::EMPTY`.
#[derive(Clone, Copy, Debug)]
pub(super) struct SymRef {
    pub(super) target: SymbolId,
    pub(super) off: u32,
    pub(super) len: u32,
}

/// Resolve an optional [`SymRef`] to an absolute [`BlobSlice`]. `None`
/// maps to [`BlobSlice::EMPTY`]; `Some` looks the target up in
/// `symbols` and adds `off`.
pub(super) fn resolve(sym: Option<SymRef>, symbols: &SymbolBases) -> BlobSlice {
    match sym {
        None => BlobSlice::EMPTY,
        Some(s) => BlobSlice {
            off: symbols.base_of(s.target) + s.off,
            len: s.len,
        },
    }
}

/// One assigned base address per [`SymbolId`]. Linker-side "where did
/// symbol N land?" only — no names, types, or scopes.
pub(super) struct SymbolBases {
    bases: Vec<Option<u32>>,
}

impl SymbolBases {
    pub(super) fn new() -> Self {
        Self { bases: Vec::new() }
    }

    pub(super) fn alloc(&mut self) -> SymbolId {
        let id = self.bases.len() as SymbolId;
        self.bases.push(None);
        id
    }

    pub(super) fn set(&mut self, id: SymbolId, base: u32) {
        let prev = self.bases[id as usize].replace(base);
        debug_assert!(prev.is_none(), "symbol {id} placed twice");
    }

    pub(super) fn base_of(&self, id: SymbolId) -> u32 {
        self.bases[id as usize].expect("symbol queried before placement")
    }
}

/// Defers reloc resolution until every target symbol has a base. The
/// whole point of this layer — placing segments in any order produces
/// the same final bytes.
pub(super) struct RelocPlan {
    pending: Vec<PendingReloc>,
}

struct PendingReloc {
    /// Index into `data_segments`; captured so resolve skips the scan.
    seg_idx: usize,
    /// Absolute byte offset of the 4-byte slot to overwrite.
    site: u32,
    target: SymbolId,
    addend: i32,
}

impl RelocPlan {
    pub(super) fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Caller must have already registered the segment's symbol via
    /// `SymbolBases::set`.
    pub(super) fn record_segment(&mut self, seg_idx: usize, seg_base: u32, relocs: Vec<Reloc>) {
        for r in relocs {
            self.pending.push(PendingReloc {
                seg_idx,
                site: seg_base + r.site,
                target: r.target,
                addend: r.addend,
            });
        }
    }

    pub(super) fn resolve(self, symbols: &SymbolBases, data_segments: &mut [(u32, Vec<u8>)]) {
        for r in self.pending {
            let value = (symbols.base_of(r.target) as i32).wrapping_add(r.addend);
            let (entry_base, bytes) = &mut data_segments[r.seg_idx];
            let off = (r.site - *entry_base) as usize;
            bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
        }
    }
}

/// Write a 32-bit little-endian integer into a byte buffer at `offset`.
pub(super) fn write_le_i32(buf: &mut [u8], offset: usize, value: i32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// Field-keyed writer over one record instance. Drops the blob borrow
/// between calls so nested-record writers interleave freely.
pub(super) struct RecordWriter<'a> {
    pub layout: &'a RecordLayout,
    pub base: usize,
}
impl<'a> RecordWriter<'a> {
    /// Anchor at an existing record; record bytes must already be in the blob.
    pub(super) fn at(layout: &'a RecordLayout, base: usize) -> Self {
        Self { layout, base }
    }

    /// Append a fresh zeroed record and anchor at it.
    pub(super) fn extend_zero(blob: &mut Vec<u8>, layout: &'a RecordLayout) -> Self {
        let base = blob.len();
        blob.extend(std::iter::repeat_n(0u8, layout.size as usize));
        Self { layout, base }
    }

    /// Absolute byte offset of `field` within the blob.
    pub(super) fn field_offset(&self, field: &str) -> usize {
        self.base + self.layout.offset_of(field) as usize
    }

    pub(super) fn nested<'b>(
        &self,
        field: &str,
        nested_layout: &'b RecordLayout,
    ) -> RecordWriter<'b> {
        RecordWriter::at(nested_layout, self.field_offset(field))
    }

    pub(super) fn write_i32(&self, blob: &mut [u8], field: &str, value: i32) {
        write_le_i32(blob, self.field_offset(field), value);
    }

    pub(super) fn write_u8(&self, blob: &mut [u8], field: &str, value: u8) {
        blob[self.field_offset(field)] = value;
    }

    /// Write a `(ptr, len)` slice pair for a `list<T>` / `string` field.
    pub(super) fn write_slice(&self, blob: &mut [u8], field: &str, slice: BlobSlice) {
        let off = self.field_offset(field);
        write_le_i32(blob, off + SLICE_PTR_OFFSET as usize, slice.off as i32);
        write_le_i32(blob, off + SLICE_LEN_OFFSET as usize, slice.len as i32);
    }

    /// Set the option disc byte to `none`. Caller must `extend_zero`
    /// to zero the payload.
    pub(super) fn write_option_none(&self, blob: &mut [u8], field: &str) {
        self.write_u8(blob, field, OPTION_NONE);
    }

    /// Set the option disc to `some`. Caller fills the payload via a
    /// separate writer at `field_offset(field) + payload_off`.
    pub(super) fn write_option_some(&self, blob: &mut [u8], field: &str) {
        self.write_u8(blob, field, OPTION_SOME);
    }
}
