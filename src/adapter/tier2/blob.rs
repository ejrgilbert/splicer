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

use std::collections::HashMap;

use super::super::abi::emit::{BlobSlice, RecordLayout, SLICE_LEN_OFFSET, SLICE_PTR_OFFSET};

// Variant disc values for `option<T>`. Fixed by the canonical-ABI
// spec, not by any WIT we control: wit-parser models `option<T>` as
// its own `TypeDefKind::Option(T)` (not a `Variant`), so there's no
// case data on a `Resolve` to derive these from — they're just the
// spec values.
const OPTION_NONE: u8 = 0;
const OPTION_SOME: u8 = 1;

/// Append-only string interner whose handle type is [`BlobSlice`].
/// Wraps the `Vec<u8>` that backs the tier-2 name-blob data segment;
/// the order in which `intern` is called determines the byte offsets
/// reported back as [`BlobSlice::off`] — that ordering used to be a
/// comment ("appending order determines offset") and is now a type
/// contract: callers can only produce a [`BlobSlice`] by going through
/// `intern`, and the only way to surface the bytes is `into_bytes`.
///
/// Repeat calls with the same string return the same [`BlobSlice`]
/// (offset + length) so `point` mentioned in two different functions
/// only contributes one copy of `"point"` / `"x"` / `"y"` to the blob.
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

/// Identifier handed out by [`SymbolBases::alloc`]; names a future
/// data-segment base address that is not yet known at build time.
pub(crate) type SymbolId = u32;

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
/// `off`. Held in builder outputs until [`resolve`] looks up
/// `target`'s placed base; calling resolve consumes the symbolic form
/// so a "translate twice" mistake becomes a type error. Absence is
/// modeled by the surrounding [`Option`] — `None` resolves to
/// [`BlobSlice::EMPTY`].
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
    /// Index into `data_segments` of the entry holding the slot.
    /// Captured at queue time so resolve skips the segment scan.
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

    /// Record that `seg` landed inside `data_segments[seg_idx]` at
    /// absolute `seg_base`. Caller must have already registered the
    /// segment's symbol via [`SymbolBases::set`].
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

    /// Write `bases[target] + addend` as little-endian i32 at every
    /// queued site. O(n) in relocs — each carries its `seg_idx`.
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
    /// onto `relocs` for the layout phase to resolve. `None` leaves
    /// the slot zeroed (no reloc, len = 0).
    pub(super) fn write_slice_reloc(
        &self,
        blob: &mut [u8],
        relocs: &mut Vec<Reloc>,
        field: &str,
        sym: Option<SymRef>,
    ) {
        let off = self.field_offset(field);
        let len = match sym {
            Some(s) => {
                relocs.push(Reloc {
                    site: (off + SLICE_PTR_OFFSET as usize) as u32,
                    target: s.target,
                    addend: s.off as i32,
                });
                s.len
            }
            None => 0,
        };
        write_le_i32(blob, off + SLICE_LEN_OFFSET as usize, len as i32);
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

#[cfg(test)]
mod reloc_tests {
    use super::*;

    /// Many segments × many relocs — guards the seg_idx threading.
    #[test]
    fn resolve_writes_each_site_in_owning_segment() {
        const N: u32 = 64;
        const SLOTS_PER_SEG: u32 = 8;
        const SEG_BYTES: u32 = SLOTS_PER_SEG * 4;
        // Leave a 4-byte gap between segments so they don't coalesce
        // and seg_idx == placement order.
        const STRIDE: u32 = SEG_BYTES + 4;

        let mut symbols = SymbolBases::new();
        let mut plan = RelocPlan::new();
        let mut data_segments: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut targets: Vec<SymbolId> = Vec::new();

        for i in 0..N {
            let id = symbols.alloc();
            let base = i * STRIDE;
            symbols.set(id, base);
            data_segments.push((base, vec![0u8; SEG_BYTES as usize]));
            targets.push(id);
        }

        // Each segment patches all 8 slots, each pointing at a
        // different segment's base + a unique addend.
        for (seg_idx, (base, _)) in data_segments.iter().enumerate() {
            let relocs: Vec<Reloc> = (0..SLOTS_PER_SEG)
                .map(|s| Reloc {
                    site: s * 4,
                    target: targets[(seg_idx + s as usize) % N as usize],
                    addend: (seg_idx as i32) * 100 + s as i32,
                })
                .collect();
            plan.record_segment(seg_idx, *base, relocs);
        }

        plan.resolve(&symbols, &mut data_segments);

        for (seg_idx, (_, bytes)) in data_segments.iter().enumerate() {
            for s in 0..SLOTS_PER_SEG as usize {
                let off = s * 4;
                let written = i32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
                let target_base = ((seg_idx + s) % N as usize) as i32 * STRIDE as i32;
                let addend = (seg_idx as i32) * 100 + s as i32;
                assert_eq!(written, target_base + addend);
            }
        }
    }

    /// Coalesced placements share an entry; each reloc still hits
    /// the right local offset.
    #[test]
    fn resolve_handles_coalesced_segments() {
        let mut symbols = SymbolBases::new();
        let mut plan = RelocPlan::new();

        let id_a = symbols.alloc();
        let id_b = symbols.alloc();
        symbols.set(id_a, 0);
        symbols.set(id_b, 8);

        // Single coalesced entry holds both placements.
        let mut data_segments = vec![(0u32, vec![0u8; 16])];

        // Placement A: bytes 0..8, site at offset 4 → target B (=8).
        plan.record_segment(
            0,
            0,
            vec![Reloc {
                site: 4,
                target: id_b,
                addend: 0,
            }],
        );
        // Placement B: bytes 8..16, site at offset 0 → target A (=0) + 3.
        plan.record_segment(
            0,
            8,
            vec![Reloc {
                site: 0,
                target: id_a,
                addend: 3,
            }],
        );

        plan.resolve(&symbols, &mut data_segments);

        let bytes = &data_segments[0].1;
        assert_eq!(i32::from_le_bytes(bytes[4..8].try_into().unwrap()), 8);
        assert_eq!(i32::from_le_bytes(bytes[8..12].try_into().unwrap()), 3);
    }
}
