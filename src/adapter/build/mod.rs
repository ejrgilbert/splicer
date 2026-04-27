//! Wasm binary emission. Builds the outer Component and its nested
//! core modules (memory provider + dispatch), turning a resolved
//! [`super::func::AdapterFunc`] list into adapter bytes. The ABI
//! semantics are inherited from [`super::abi`]; this layer is
//! concerned with section ordering, index management, and
//! `wasm-encoder` opcode emission.
//!
//! Submodules:
//! - [`component`] — outer Component orchestration across its numbered
//!   phases. Entry point [`build_adapter_bytes`].
//! - [`dispatch`] — the inner core-wasm dispatch module: per-func
//!   wrappers, hook phases, async wait loops, `task.return` wiring.
//! - [`encoders`] — component-level type-section encoders for the
//!   handler import's instance type.
//! - [`mem_layout`] — byte-offset allocator for the dispatch module's
//!   scratch memory (function names, result buffers, event record,
//!   bump-allocator start).
//! - [`ty`] — small wasm-encoder-adjacent helpers (`prim_cv`,
//!   `val_type_byte_size`, `align_to_val`) shared by the encoders and
//!   allocator.

mod component;
mod dispatch;
mod encoders;
mod mem_layout;
mod ty;
mod wit_component_emit;

pub(super) use component::build_adapter_bytes;
pub(super) use mem_layout::MemoryLayoutBuilder;
pub(super) use wit_component_emit::build_adapter_via_wit_component;
