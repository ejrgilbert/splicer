//! Data-segment packing helpers for tier-2.
//!
//! Mirrors what `cells::CellLayout` does on the wasm-emit side: read
//! field offsets from a [`RecordLayout`] (already schema-derived) and
//! expose name-keyed writes, so no caller has to do
//! `base + layout.offset_of("foo") + SLICE_PTR_OFFSET as usize` math
//! inline. Also collapses the dozens of `(u32, u32)` "pointer/length"
//! tuples into a typed [`BlobSlice`].
//!
//! Cross-segment pointers go through the [`Segment`] / [`SymRef`] /
//! [`Reloc`] relocation model: a builder writes a placeholder + a
//! [`Reloc`] (or hands back a [`SymRef`]), and the layout phase
//! resolves both in one pass after every segment has a base address.
//! This makes segment placement order commutative — no
//! "patch-then-translate" sequence to get wrong.

use super::super::abi::emit::{RecordLayout, SLICE_LEN_OFFSET, SLICE_PTR_OFFSET};

/// Variant disc values for `option<T>` — canonical-ABI invariants.
const OPTION_NONE: u8 = 0;
const OPTION_SOME: u8 = 1;

/// Identifier handed out by [`SymbolBases::alloc`]; names a future
/// data-segment base address that is not yet known at build time.
pub(crate) type SymbolId = u32;

/// Sentinel for a [`SymRef`] that points nowhere; paired with
/// `len == 0`, marks a missing slice. Picked as `u32::MAX` so that
/// hitting `SymbolBases::base_of` with it would panic loudly if
/// `len == 0` ever stops gating the resolve.
pub(super) const NO_SYMBOL: SymbolId = u32::MAX;

/// One pending pointer write into a [`Segment`]'s bytes. After every
/// segment has a base in the [`SymbolBases`], the layout pass writes
/// `bases[target] + addend` as a little-endian i32 at
/// `segment_base + site`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Reloc {
    pub(crate) site: u32,
    pub(crate) target: SymbolId,
    pub(crate) addend: i32,
}

/// One bytes-and-relocs unit handed off to the layout phase. The
/// builder fills `bytes` and records cross-segment pointer slots in
/// `relocs`; the layout resolves and emits the bytes.
pub(crate) struct Segment {
    pub(crate) id: SymbolId,
    pub(crate) align: u32,
    pub(crate) bytes: Vec<u8>,
    pub(crate) relocs: Vec<Reloc>,
}

/// A `(ptr, len)` pair that points into segment `target` at relative
/// `off`. Held in builder outputs until [`Self::resolve`] looks up
/// `target`'s placed base; calling resolve consumes the symbolic form
/// so a "translate twice" mistake becomes a type error.
#[derive(Clone, Copy, Debug)]
pub(super) struct SymRef {
    pub(super) target: SymbolId,
    pub(super) off: u32,
    pub(super) len: u32,
}

impl SymRef {
    pub(super) const EMPTY: SymRef = SymRef {
        target: NO_SYMBOL,
        off: 0,
        len: 0,
    };

    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(super) fn resolve(self, symbols: &SymbolBases) -> BlobSlice {
        if self.is_empty() {
            BlobSlice::EMPTY
        } else {
            BlobSlice {
                off: symbols.base_of(self.target) + self.off,
                len: self.len,
            }
        }
    }
}

/// One assigned base address per [`SymbolId`] — a thin
/// `Vec<Option<u32>>` keyed by id. Not a compiler-style symbol table
/// (no names, types, or scopes); only the linker-side question
/// "where did symbol N land?". The layout phase calls `set` once per
/// segment after assigning its base; reads happen during reloc
/// application + [`SymRef::resolve`].
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

/// Defers reloc resolution until every target symbol has a base.
/// `record_segment` is called as each segment lands in the layout's
/// data area (with that segment's assigned absolute base);
/// [`Self::resolve`] then applies every queued write in one pass.
///
/// Decoupling segment placement from reloc application is the whole
/// point of this layer — it's why placing two segments in either
/// order produces the same final bytes.
pub(super) struct RelocPlan {
    pending: Vec<PendingReloc>,
}

struct PendingReloc {
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

    /// Record that `seg` was placed with its bytes starting at
    /// `seg_base`. Each of its relocs becomes a pending absolute-site
    /// write; the segment's symbol must already have been registered
    /// by the caller via [`SymbolBases::set`].
    pub(super) fn record_segment(&mut self, seg_base: u32, relocs: Vec<Reloc>) {
        for r in relocs {
            self.pending.push(PendingReloc {
                site: seg_base + r.site,
                target: r.target,
                addend: r.addend,
            });
        }
    }

    /// Apply every queued reloc against the placed `data_segments`,
    /// writing `bases[target] + addend` as a little-endian i32 at
    /// each site.
    pub(super) fn resolve(self, symbols: &SymbolBases, data_segments: &mut [(u32, Vec<u8>)]) {
        for r in self.pending {
            let value = (symbols.base_of(r.target) as i32).wrapping_add(r.addend);
            patch_le_i32_in_segments(data_segments, r.site, value);
        }
    }
}

/// Find the placed segment containing absolute byte `site` and
/// overwrite its 4-byte slot with `value` (little-endian). Splits in
/// the data segment list don't matter — this scans them all.
fn patch_le_i32_in_segments(segs: &mut [(u32, Vec<u8>)], site: u32, value: i32) {
    for (base, bytes) in segs.iter_mut() {
        let len = bytes.len() as u32;
        if site >= *base && site + 4 <= *base + len {
            let off = (site - *base) as usize;
            bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
            return;
        }
    }
    panic!("reloc site {site} falls outside any placed segment");
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct BlobSlice {
    pub off: u32,
    pub len: u32,
}
impl BlobSlice {
    pub(super) const EMPTY: BlobSlice = BlobSlice { off: 0, len: 0 };
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

    /// Like [`Self::write_slice`] but for a slice that points into
    /// another segment that hasn't been placed yet. The `len` lands
    /// directly; the `ptr` slot stays zero and a [`Reloc`] is pushed
    /// onto `relocs` for the layout phase to resolve.
    pub(super) fn write_slice_reloc(
        &self,
        blob: &mut [u8],
        relocs: &mut Vec<Reloc>,
        field: &str,
        sym: SymRef,
    ) {
        let off = self.field_offset(field);
        if !sym.is_empty() {
            relocs.push(Reloc {
                site: (off + SLICE_PTR_OFFSET as usize) as u32,
                target: sym.target,
                addend: sym.off as i32,
            });
        }
        write_le_i32(blob, off + SLICE_LEN_OFFSET as usize, sym.len as i32);
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
