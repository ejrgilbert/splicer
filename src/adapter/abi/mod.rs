//! Canonical-ABI abstraction layer. Everything that encodes knowledge
//! of the Component Model's canonical ABI — type flattening, memory
//! layout, lift/lower codegen — lives here. Higher layers ([`super::build`]
//! and the top-level `generate_tier1_adapter`) consume this module's
//! public surface and don't touch the ABI directly.
//!
//! Submodules:
//! - [`bindgen`] — the [`wit_bindgen_core::abi::Bindgen`] impl that
//!   emits `wasm-encoder` instructions for `lift_from_memory`.
//! - [`bridge`] — translation from splicer's input type model (cviz's
//!   `ValueType` / `TypeArena`) into [`wit_parser`]'s `Resolve` +
//!   `SizeAlign`, so the rest of the ABI layer can consume upstream.
//! - [`compat`] — verbatim copies of two private helpers in
//!   `wit-bindgen-core` (`cast`, `flat_types`). Pending an upstream
//!   PR; see the module header.

mod bindgen;
mod bridge;
mod compat;

pub(super) use bindgen::WasmEncoderBindgen;
pub(super) use bridge::WitBridge;
