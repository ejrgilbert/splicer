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

use super::ty::{align_to_val, val_type_byte_size};

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

/// Flat shape of the bool slot `should-block-call` writes. The
/// canonical ABI stores a bool as an i32; the blocking phase
/// (see `super::dispatch::emit_blocking_phase`) reads it via
/// `i32.load` at `block_result_ptr + 0` and branches on zero /
/// non-zero.
const BLOCK_RESULT_SHAPE: &[ValType] = &[ValType::I32];

/// Byte-offset bookkeeper for the dispatch module's linear memory.
/// See the module docs for the overall layout and call-ordering rules.
pub(super) struct MemoryLayoutBuilder {
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

    /// Reserve `size` bytes for an async-lowered handler's result
    /// buffer. The canon-lower-async machinery writes flat result
    /// values here with natural per-slot alignment (each slot's
    /// offset driven by the canonical-ABI layout, which
    /// [`super::bindgen::WasmEncoderBindgen`] honors when emitting
    /// the `task.return` loads), so the allocator itself doesn't
    /// re-align between async buffers.
    pub fn alloc_async_result(&mut self, size: u32) -> u32 {
        let off = self.post_name_cursor;
        self.post_name_cursor += size;
        off
    }

    /// Reserve `size` bytes for a sync-complex retptr buffer. Canon
    /// lower's retptr pattern requires the buffer to start on an i32
    /// boundary so the first `(i32.store / f32.store)` inside the
    /// buffer is naturally aligned; wider stores (i64 / f64) re-align
    /// internally per the canonical ABI.
    pub fn alloc_sync_result(&mut self, size: u32) -> u32 {
        self.alloc_aligned(size, val_type_byte_size(&ValType::I32))
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

    /// Shared helper: align the post-name cursor up to `align`, then
    /// reserve `size` bytes starting from that aligned offset.
    fn alloc_aligned(&mut self, size: u32, align: u32) -> u32 {
        let aligned = align_to_val(self.post_name_cursor, align);
        self.post_name_cursor = aligned + size;
        aligned
    }
}
