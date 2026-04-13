//! splicer — plan and generate WebAssembly component compositions.
//!
//! Most users only need the two top-level entry points:
//!
//! - [`splice`] — splice middleware into an existing composition.
//! - [`compose`] — synthesise a composition from N components.
//!
//! Both take a typed request struct and return a typed output struct
//! whose `wac_deps` field is shaped to be handed straight to
//! [`wac_resolver::FileSystemPackageResolver`](https://docs.rs/wac-resolver)
//! (or formatted into a `wac compose ... --dep ...` shell command).
//!
//! Advanced users who want more granular control can reach for the
//! lower-level building blocks under [`lowlevel`], or import the
//! shared types from [`types`].

mod adapter;
mod api;
mod compose;
mod contract;
mod parse;
mod split;
mod wac;

#[cfg(test)]
mod tests;

// ── Top-level entry points ────────────────────────────────────────
pub use api::{
    compose, format_wac_compose_cmd, splice, ComponentInput, ComposeOutput, ComposeRequest,
    SpliceOutput, SpliceRequest,
};

// ── Shared types ──────────────────────────────────────────────────

/// Types that appear on the public API surface and may be useful to
/// import directly.
pub mod types {
    pub use crate::contract::{
        ContractResult, TIER1_AFTER, TIER1_BEFORE, TIER1_BLOCKING, TIER1_INTERFACES,
        TIER1_PACKAGE, TIER1_VERSION,
    };
    pub use crate::parse::config::{Injection, SpliceRule};
    pub use crate::wac::GeneratedAdapter;
}

// ── Lower-level building blocks ───────────────────────────────────

/// Direct access to splicer's internal pipeline stages, for callers
/// that need finer control than [`splice`] / [`compose`] offer.
///
/// The function signatures here are not stable in the same way the
/// top-level entry points are — they reflect splicer's internal
/// pipeline shape and may change between releases as the pipeline
/// evolves.
pub mod lowlevel {
    pub use crate::adapter::generate_tier1_adapter;
    pub use crate::compose::build_graph_from_components;
    pub use crate::contract::{
        validate_contract, versioned_interface, ContractResult, TIER1_INTERFACES,
    };
    pub use crate::parse::config::{parse_yaml, Injection, SpliceRule};
    pub use crate::split::{gen_split_path, split_out_composition, PATH_TO_SPLITS};
    pub use crate::wac::{generate_wac, GeneratedAdapter, WacOutput, INST_PREFIX};
}
