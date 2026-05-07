//! Byte-offset allocator for the tier-1 adapter's linear memory.
//!
//! Every slot the adapter reserves in the dispatch module's memory —
//! function-name bytes, async and sync-complex result buffers, the
//! `waitable-set.wait` event record, the `should-block` bool
//! slot, and the start of the bump allocator — goes through
//! [`MemoryLayoutBuilder`]. Centralizing the math in one place keeps
//! the extraction phase (which assigns per-func offsets) and the
//! build phase (which assigns the fixed post-func slots and the
//! bump-allocator start) from re-deriving cursor positions by
//! iterating the func table.
//!
//! The layout emitted by a normal run is:
//!
//! ```text
//! [0 .. total_name_bytes)        concatenated UTF-8 function names
//! [i32-aligned .. …)             per-func result buffers
//!                                  - async results pack contiguously
//!                                  - sync-complex results re-align to i32
//! [… .. +sum(EVENT_RECORD_SHAPE)) event slot (if has_async_machinery)
//! [… .. +sum(BLOCK_RESULT_SHAPE)) block slot (if has_blocking)
//! i64-aligned upward             bump_start (consumed on finish)
//! ```
//!
//! Call ordering matters: the builder is single-cursor across the
//! post-name region, so callers must allocate in the order sections
//! appear above. [`super::func::extract_adapter_funcs`] handles the
//! name + per-func result passes; [`super::component`] finishes the
//! layout with the event / block / bump calls.

use wasm_encoder::ValType;

/// Byte size of a core Wasm value type in linear memory.
fn val_type_byte_size(vt: &ValType) -> u32 {
    match vt {
        ValType::I32 | ValType::F32 => 4,
        ValType::I64 | ValType::F64 => 8,
        ValType::V128 => 16,
        ValType::Ref(_) => 4,
    }
}

/// Round `offset` up to the nearest multiple of `align`.
fn align_to_val(offset: u32, align: u32) -> u32 {
    offset.div_ceil(align) * align
}

// ─── Slot shapes ───────────────────────────────────────────────────────────
//
// Every fixed slot the builder reserves in the post-name region
// corresponds to a canonical-ABI record with a known flat core-Wasm
// shape. We declare the SHAPE here; [`MemoryLayoutBuilder::alloc_record`]
// derives size and natural alignment from the shape so the numbers
// the allocator uses can't drift out of sync with the dispatch
// module's loads/stores that read and write those bytes.
//
// Per-region alignment choices live inline in the allocator methods
// below, each expressed as `val_type_byte_size(&ValType::<the
// relevant slot type>)` — so "4" and "8" never appear as raw literals;
// the numbers fall out of the ValTypes they belong to.

/// Flat shape of the event record `waitable-set.wait` writes:
/// `(event_code: i32, waitable_handle: i32)`. The wait-loop in the
/// dispatch module reads both halves via `i32.load` at
/// `event_ptr + 0` and `event_ptr + 4`.
const EVENT_RECORD_SHAPE: &[ValType] = &[ValType::I32, ValType::I32];

/// Flat shape of the bool slot `should-block` writes. The
/// canonical ABI stores a bool as an i32; the blocking phase
/// (see `super::dispatch::emit_blocking_phase`) reads it via
/// `i32.load` at `block_result_ptr + 0` and branches on zero /
/// non-zero.
const BLOCK_RESULT_SHAPE: &[ValType] = &[ValType::I32];

/// Byte-offset bookkeeper for the dispatch module's linear memory.
/// See the module docs for the overall layout and call-ordering rules.
pub(crate) struct MemoryLayoutBuilder {
    /// Running cursor inside `[0 .. total_name_bytes)`. Bumped by
    /// [`alloc_name`](Self::alloc_name).
    name_cursor: u32,
    /// Running cursor for everything AFTER the name region.
    /// Initialized to i32-aligned past the names so the first
    /// post-name allocation lands on an i32 boundary; subsequent
    /// allocations each re-align as their own contracts require.
    post_name_cursor: u32,
}

impl MemoryLayoutBuilder {
    pub fn new(total_name_bytes: u32) -> Self {
        // i32 is the narrowest slot we load from in the post-name
        // region (block-result bool, event-record fields, canon-lift
        // result pointers), so everything must start on that
        // boundary at minimum.
        let post_name_align = val_type_byte_size(&ValType::I32);
        Self {
            name_cursor: 0,
            post_name_cursor: align_to_val(total_name_bytes, post_name_align),
        }
    }

    /// Reserve `name_len` bytes at the current name cursor and return
    /// the base offset. Callers concatenate the raw UTF-8 bytes at
    /// these offsets when emitting the data segment.
    pub fn alloc_name(&mut self, name_len: u32) -> u32 {
        let off = self.name_cursor;
        self.name_cursor += name_len;
        off
    }

    /// Reserve the event-record slot written by `waitable-set.wait`.
    /// Size and alignment fall out of [`EVENT_RECORD_SHAPE`].
    pub fn alloc_event_slot(&mut self) -> u32 {
        self.alloc_record(EVENT_RECORD_SHAPE)
    }

    /// Reserve the bool-result slot written by `should_block_call`.
    /// Size and alignment fall out of [`BLOCK_RESULT_SHAPE`].
    pub fn alloc_block_result(&mut self) -> u32 {
        self.alloc_record(BLOCK_RESULT_SHAPE)
    }

    /// Finalize the layout and return the first free byte for the
    /// bump allocator. Aligned up to the widest canonical-ABI scalar
    /// (i64 / f64 — both 8 bytes) so the allocator can satisfy
    /// 8-byte-aligned `realloc` requests on its first call without
    /// burning an alignment jump.
    pub fn finish_as_bump_start(self) -> u32 {
        align_to_val(self.post_name_cursor, val_type_byte_size(&ValType::I64))
    }

    /// Reserve one canonical-ABI record with a fixed flat shape.
    /// Size is the sum of the flat slot sizes; natural alignment is
    /// the widest slot.
    fn alloc_record(&mut self, flat: &[ValType]) -> u32 {
        let size: u32 = flat.iter().map(val_type_byte_size).sum();
        let align: u32 = flat.iter().map(val_type_byte_size).max().unwrap_or(1);
        self.alloc_aligned(size, align)
    }

    /// Reserve `size` bytes aligned to `align`. Both numbers must come
    /// from `wit_parser::SizeAlign` (retptr scratch, call-id buffer,
    /// ...). Misaligned buffers trap on i64/f64 stores inside the
    /// canonical-ABI lowering.
    pub fn alloc_aligned(&mut self, size: u32, align: u32) -> u32 {
        let aligned = align_to_val(self.post_name_cursor, align);
        self.post_name_cursor = aligned + size;
        aligned
    }
}

// ─── StaticLayout: alignment-safe data + scratch placer ────────────
//
// Tier-2's adapter has a more varied layout than tier-1: data
// segments (name strings, prebuilt `field` records) interleave with
// scratch slots (event record, on-call indirect-params buffer, per-
// fn cells slabs). Placing them by hand is fragile — a single
// missed `align_up` causes an unaligned-pointer trap deep inside
// canon-lower-async.
//
// `StaticLayout` centralizes that math: every section asks for an
// explicit alignment, the cursor is bumped automatically, and the
// data segments are returned ready for `wasm-encoder` to emit.

/// A linear-memory layout builder that places data-bearing and
/// scratch sections sequentially, automatically padding for the
/// alignment each caller requests.
///
/// Each `place_data` / `reserve_scratch` call advances a single
/// cursor and returns the byte offset of the section it placed.
/// `place_data` records the bytes for emission; `reserve_scratch`
/// only bumps the cursor (the wasm runtime zero-initializes
/// uninitialized memory, so scratch slots don't need explicit
/// zero-fill).
///
/// Adjacent `place_data` calls whose alignment requirement is
/// already met by the cursor coalesce into a single wasm data
/// segment; gaps (caused by larger alignment, or a `reserve_scratch`
/// in between) split segments at the appropriate boundaries.
pub(crate) struct StaticLayout {
    cursor: u32,
    segments: Vec<(u32, Vec<u8>)>,
}

impl StaticLayout {
    pub fn new() -> Self {
        Self {
            cursor: 0,
            segments: Vec::new(),
        }
    }

    /// Place a data-bearing section aligned to `align`. Returns the
    /// byte offset and the index of the `(base, bytes)` entry in
    /// [`Self::into_segments`] containing it. Index is meaningless
    /// for empty `bytes`; callers must not queue relocs against one.
    pub fn place_data(&mut self, align: u32, bytes: &[u8]) -> (u32, usize) {
        let offset = align_to_val(self.cursor, align);
        if !bytes.is_empty() {
            // Coalesce with the previous segment if there was no gap.
            let coalesced = match self.segments.last_mut() {
                Some(last) if last.0 + last.1.len() as u32 == offset => {
                    last.1.extend_from_slice(bytes);
                    true
                }
                _ => false,
            };
            if !coalesced {
                self.segments.push((offset, bytes.to_vec()));
            }
        }
        self.cursor = offset + bytes.len() as u32;
        (offset, self.segments.len().saturating_sub(1))
    }

    /// Reserve a scratch (uninitialized) section aligned to `align`.
    /// Returns the byte offset where the section starts.
    pub fn reserve_scratch(&mut self, align: u32, size: u32) -> u32 {
        let offset = align_to_val(self.cursor, align);
        self.cursor = offset + size;
        offset
    }

    /// First byte past the last allocated section. Use as the
    /// `bump_start` for `cabi_realloc` (caller may still align
    /// further before passing it to `emit_memory_and_globals`).
    pub fn end(&self) -> u32 {
        self.cursor
    }

    /// Consume the builder and return the data segments to feed to
    /// `wasm-encoder`. Each entry is `(byte_offset, bytes)`.
    pub fn into_segments(self) -> Vec<(u32, Vec<u8>)> {
        self.segments
    }
}

#[cfg(test)]
mod static_layout_tests {
    use super::*;

    #[test]
    fn coalesces_adjacent_data() {
        let mut l = StaticLayout::new();
        let (a, ai) = l.place_data(1, b"abc");
        let (b, bi) = l.place_data(1, b"de");
        assert_eq!((a, b), (0, 3));
        // Both calls land in the single coalesced entry.
        assert_eq!((ai, bi), (0, 0));
        assert_eq!(l.end(), 5);
        let segs = l.into_segments();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].0, 0);
        assert_eq!(&segs[0].1[..], b"abcde");
    }

    #[test]
    fn alignment_pads_cursor() {
        let mut l = StaticLayout::new();
        assert_eq!(l.place_data(1, b"x").0, 0); // cursor → 1
        assert_eq!(l.place_data(4, b"yyyy").0, 4); // padded to 4
        assert_eq!(l.end(), 8);
    }

    #[test]
    fn scratch_breaks_coalescing() {
        let mut l = StaticLayout::new();
        let (a, ai) = l.place_data(1, b"AAAA"); // 0..4
        let scratch = l.reserve_scratch(1, 8); // 4..12
        let (b, bi) = l.place_data(1, b"BB"); // 12..14
        assert_eq!((a, scratch, b), (0, 4, 12));
        // Scratch in between forces a new entry for B.
        assert_eq!((ai, bi), (0, 1));
        let segs = l.into_segments();
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].0, &segs[0].1[..]), (0, &b"AAAA"[..]));
        assert_eq!((segs[1].0, &segs[1].1[..]), (12, &b"BB"[..]));
    }

    #[test]
    fn aligned_data_after_scratch_does_not_coalesce() {
        let mut l = StaticLayout::new();
        l.place_data(1, b"AA"); // 0..2
        l.reserve_scratch(8, 0); // pure align-up: cursor → 8
        let (b, bi) = l.place_data(1, b"B"); // 8..9
        assert_eq!(b, 8);
        assert_eq!(bi, 1);
        let segs = l.into_segments();
        assert_eq!(segs.len(), 2);
    }
}
