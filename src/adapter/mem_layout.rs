//! Byte-offset allocator for the tier-1 adapter's linear memory.
//!
//! Every slot the adapter reserves in the dispatch module's memory —
//! function-name bytes, async and sync-complex result buffers, the
//! `waitable-set.wait` event record, the `should-block-call` bool
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
//! [4-aligned .. …)               per-func result buffers
//!                                  - async results pack contiguously
//!                                  - sync-complex results re-align to 4
//! [… .. +8)                      event slot (if has_async_machinery)
//! [… .. +4)                      block slot (if has_blocking)
//! 8-aligned upward               bump_start (consumed on finish)
//! ```
//!
//! Call ordering matters: the builder is single-cursor across the
//! post-name region, so callers must allocate in the order sections
//! appear above. [`super::func::extract_adapter_funcs`] handles the
//! name + per-func result passes; [`super::component`] finishes the
//! layout with the event / block / bump calls.

use super::ty::align_to_val;

// ─── Slot sizes + alignments ───────────────────────────────────────────────
//
// Every hard-coded number the memory layout depends on lives here, named
// for the canonical-ABI feature that pins it. Each constant is used by
// exactly one allocation method below; if you're tempted to sprinkle a
// literal `4` or `8` into adapter code, the right answer is almost
// always a named constant added to this table.

/// Size of the event record `waitable-set.wait` writes into memory:
/// a pair of i32s — `(event_code, waitable_handle)` — for 8 bytes
/// total. The wait loop in the dispatch module reads both halves,
/// but the allocator only needs the total to reserve the slot.
const EVENT_RECORD_BYTES: u32 = 8;

/// Size of the bool slot `should-block-call` writes. The canonical
/// ABI stores a bool as an i32 (4 bytes). The wait loop reads this
/// via `i32.load` and branches on zero / non-zero.
const BLOCK_RESULT_BYTES: u32 = 4;

/// Minimum alignment for everything in the post-name region:
/// per-func result buffers, the event record, the block-result
/// slot. Pinned by i32 / f32 loads and stores, which require 4-byte
/// alignment; wider slots (i64 / f64) are handled inside the
/// per-func buffers by [`super::ty::FlatLayout`].
const RESULT_REGION_ALIGN: u32 = 4;

/// Alignment of the bump-allocator start. The canonical-ABI
/// `realloc` can be called with up to 8-byte alignment requests
/// (for i64 / f64-bearing strings and records), and starting the
/// bump region on an 8-aligned boundary lets the allocator satisfy
/// those on the first call without burning an alignment jump.
const BUMP_START_ALIGN: u32 = 8;

/// Byte-offset bookkeeper for the dispatch module's linear memory.
/// See the module docs for the overall layout and call-ordering rules.
pub(super) struct MemoryLayoutBuilder {
    /// Running cursor inside `[0 .. total_name_bytes)`. Bumped by
    /// [`alloc_name`](Self::alloc_name).
    name_cursor: u32,
    /// Running cursor for everything AFTER the name region.
    /// Initialized to `align_to_val(total_name_bytes, RESULT_REGION_ALIGN)`
    /// so the first post-name allocation lands on boundary; subsequent
    /// allocations each re-align as their own contracts require.
    post_name_cursor: u32,
}

impl MemoryLayoutBuilder {
    pub fn new(total_name_bytes: u32) -> Self {
        Self {
            name_cursor: 0,
            post_name_cursor: align_to_val(total_name_bytes, RESULT_REGION_ALIGN),
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

    /// Reserve `size` bytes for an async-lowered handler's result
    /// buffer. The canon-lower-async machinery writes flat result
    /// values here with natural per-slot alignment (handled inside
    /// the buffer by [`super::ty::FlatLayout`]), so the allocator
    /// itself doesn't re-align between async buffers.
    pub fn alloc_async_result(&mut self, size: u32) -> u32 {
        let off = self.post_name_cursor;
        self.post_name_cursor += size;
        off
    }

    /// Reserve `size` bytes for a sync-complex retptr buffer. The
    /// canon-lower retptr pattern requires `RESULT_REGION_ALIGN`-byte
    /// alignment at the start of the buffer.
    pub fn alloc_sync_result(&mut self, size: u32) -> u32 {
        self.alloc_aligned(size, RESULT_REGION_ALIGN)
    }

    /// Reserve the fixed `EVENT_RECORD_BYTES`-byte slot for the
    /// `waitable-set.wait` event record. Called once per adapter
    /// when any async or hook machinery is present.
    pub fn alloc_event_slot(&mut self) -> u32 {
        self.alloc_aligned(EVENT_RECORD_BYTES, RESULT_REGION_ALIGN)
    }

    /// Reserve the fixed `BLOCK_RESULT_BYTES`-byte slot for the
    /// bool result of `should_block_call`. Called only when the
    /// middleware exports `splicer:tier1/blocking`.
    pub fn alloc_block_result(&mut self) -> u32 {
        self.alloc_aligned(BLOCK_RESULT_BYTES, RESULT_REGION_ALIGN)
    }

    /// Finalize the layout and return the first free byte for the
    /// bump allocator, aligned up to `BUMP_START_ALIGN` so `realloc`
    /// calls with up-to-8-byte-aligned requests land on boundary on
    /// their first invocation.
    pub fn finish_as_bump_start(self) -> u32 {
        align_to_val(self.post_name_cursor, BUMP_START_ALIGN)
    }

    /// Shared helper: align the post-name cursor up to `align`, then
    /// reserve `size` bytes starting from that aligned offset.
    fn alloc_aligned(&mut self, size: u32, align: u32) -> u32 {
        let aligned = align_to_val(self.post_name_cursor, align);
        self.post_name_cursor = aligned + size;
        aligned
    }
}
