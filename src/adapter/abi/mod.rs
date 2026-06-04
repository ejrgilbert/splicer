//! Canonical-ABI infrastructure shared across tiers. Bridges
//! `wit_bindgen_core::abi::lift_from_memory` (and its `Bindgen`
//! interface) to `wasm-encoder` instructions, plus a couple of
//! verbatim wit-bindgen-core helpers awaiting an upstream visibility
//! flip, plus the small wasm-encoder helpers that every adapter
//! emit module reaches for (memory + bump-allocator setup, type
//! conversions, the standard `cabi_realloc` body).
//!
//! Submodules:
//! - [`bindgen`] — the [`Bindgen`] impl that emits `wasm-encoder`
//!   instructions for `lift_from_memory`.
//! - [`compat`] — verbatim copies of two private helpers in
//!   `wit-bindgen-core` (`cast`, `flat_types`). Pending an upstream
//!   visibility flip; see the module header.
//! - [`emit`] — wasm-encoder emit helpers (memory section, bump
//!   pointer global, `cabi_realloc`, `_initialize`, type
//!   conversions). Used by both tier-1 and tier-2 emit modules.

mod bindgen;
pub(super) mod canon_async;
mod compat;
pub(super) mod emit;

pub(super) use bindgen::WasmEncoderBindgen;
pub(super) use compat::{cast, flat_types};
