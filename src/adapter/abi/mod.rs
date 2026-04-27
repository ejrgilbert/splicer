//! Bridges `wit_bindgen_core::abi::lift_from_memory` (and its
//! `Bindgen` interface) to `wasm-encoder`. The dispatch module's
//! task.return arg-prep loads the canonical-ABI laid-out result out
//! of memory via [`WasmEncoderBindgen`].
//!
//! Submodules:
//! - [`bindgen`] — the `Bindgen` impl that emits `wasm-encoder`
//!   instructions for `lift_from_memory`.
//! - [`compat`] — verbatim copies of two private helpers in
//!   `wit-bindgen-core` (`cast`, `flat_types`). Pending an upstream
//!   visibility flip; see the module header.

mod bindgen;
mod compat;

pub(super) use bindgen::WasmEncoderBindgen;
