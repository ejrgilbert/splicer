//! Data-segment packing helpers for tier-2.
//!
//! Mirrors what `cells::CellLayout` does on the wasm-emit side: read
//! field offsets from a [`RecordLayout`] (already schema-derived) and
//! expose name-keyed writes, so no caller has to do
//! `base + layout.offset_of("foo") + SLICE_PTR_OFFSET as usize` math
//! inline. Also collapses the dozens of `(u32, u32)` "pointer/length"
//! tuples into a typed [`BlobSlice`].

use super::super::abi::emit::{RecordLayout, SLICE_LEN_OFFSET, SLICE_PTR_OFFSET};

/// Variant disc values for `option<T>` — canonical-ABI invariants.
const OPTION_NONE: u8 = 0;
const OPTION_SOME: u8 = 1;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct BlobSlice {
    pub off: u32,
    pub len: u32,
}
impl BlobSlice {
    pub(super) const EMPTY: BlobSlice = BlobSlice { off: 0, len: 0 };

    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Add an absolute base offset to the relative `off`. No-op when
    /// the slice is empty (so a zero-relative-offset for a present
    /// entry doesn't get translated, but that's fine since empty
    /// entries have `len == 0` regardless).
    pub(super) fn translate(&mut self, base: u32) {
        if !self.is_empty() {
            self.off += base;
        }
    }
}

/// Write a 32-bit little-endian integer into a byte buffer at `offset`.
pub(super) fn write_le_i32(buf: &mut [u8], offset: usize, value: i32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// Field-keyed writer over one record instance inside a `Vec<u8>`
/// data segment. Holds the [`RecordLayout`] + base offset, exposes
/// `write_*("field-name", ...)` against it. Drops the borrow on the
/// blob between calls so a caller can interleave nested-record
/// writers without lifetime gymnastics.
pub(super) struct RecordWriter<'a> {
    pub layout: &'a RecordLayout,
    pub base: usize,
}
impl<'a> RecordWriter<'a> {
    /// Anchor at an existing record start; record bytes must already
    /// be present (zeroed) in the blob.
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

    /// Anchor a nested-record writer over `field`'s sub-layout.
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

    /// Write a `list<T>` / `string` field as a `(ptr, len)` slice pair.
    pub(super) fn write_slice(&self, blob: &mut [u8], field: &str, slice: BlobSlice) {
        let off = self.field_offset(field);
        write_le_i32(blob, off + SLICE_PTR_OFFSET as usize, slice.off as i32);
        write_le_i32(blob, off + SLICE_LEN_OFFSET as usize, slice.len as i32);
    }

    /// Set the `option<T>` discriminant byte at `field` to `none`.
    /// Zero the payload by leaving the rest of the record at its
    /// initial zeroes (caller should have used `extend_zero`).
    pub(super) fn write_option_none(&self, blob: &mut [u8], field: &str) {
        self.write_u8(blob, field, OPTION_NONE);
    }

    /// Set the option discriminant to `some` at `field`. The payload
    /// itself lives at `field_offset(field) + payload_off`; the caller
    /// fills it in via a separate writer anchored there.
    pub(super) fn write_option_some(&self, blob: &mut [u8], field: &str) {
        self.write_u8(blob, field, OPTION_SOME);
    }
}
