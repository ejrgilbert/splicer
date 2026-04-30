//! Canonical-ABI infrastructure shared across tiers. Bridges
//! `wit_bindgen_core::abi::lift_from_memory` (and its `Bindgen`
//! interface) to `wasm-encoder` instructions, plus a couple of
//! verbatim wit-bindgen-core helpers awaiting an upstream visibility
//! flip.
//!
//! Today only [`super::tier1`] consumes this module; tier-2's lift
//! codegen will plug into the same machinery.
//!
//! Submodules:
//! - [`bindgen`] — the [`Bindgen`] impl that emits `wasm-encoder`
//!   instructions for `lift_from_memory`.
//! - [`compat`] — verbatim copies of two private helpers in
//!   `wit-bindgen-core` (`cast`, `flat_types`). Pending an upstream
//!   visibility flip; see the module header.

mod bindgen;
mod compat;

pub(super) use bindgen::WasmEncoderBindgen;
