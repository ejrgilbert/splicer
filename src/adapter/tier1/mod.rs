//! Tier-1 adapter generator: wraps a middleware component with
//! before/after/blocking hooks and re-exports the wrapped handler's
//! target interface.
//!
//! Cross-tier infrastructure (canonical-ABI compat helpers, shared
//! index/memory bookkeeping) lives at the [`super`] level under
//! `adapter/{compat,indices,mem_layout,shared}`.
//!
//! Submodules:
//! - [`emit`] — entry point ([`emit::build_adapter`]) that synthesizes
//!   the adapter world's WIT, builds a dispatch core module, and
//!   hands everything to `wit_component::ComponentEncoder`.

mod emit;
#[cfg(test)]
mod tests;

pub(super) use emit::build_adapter;
