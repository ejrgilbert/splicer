//! splicer — plan and generate WebAssembly component compositions.
//!
//! Most users only need the two top-level entry points:
//!
//! - [`splice`] — splice middleware into an existing composition.
//! - [`compose`] — synthesize a composition from N components.
//!
//! Both take a typed request struct and return a typed output struct
//! whose `wac_deps` field is shaped to be handed straight to
//! [`wac_resolver::FileSystemPackageResolver`](https://docs.rs/wac-resolver)
//! (or formatted into a `wac compose ... --dep ...` shell command).
//!
//! Advanced users who want more granular control can reach for the
//! lower-level building blocks under [`lowlevel`], or import the
//! shared types from [`types`].
//!
//! # Quick start
//!
//! ```no_run
//! # fn main() -> anyhow::Result<()> {
//! let rules_yaml = std::fs::read_to_string("splice.yaml")?;
//! let bundle = splicer::splice(splicer::SpliceRequest {
//!     composition_wasm: "composition.wasm".into(),
//!     rules_yaml,
//!     package_name: "example:composition".into(),
//!     splits_dir: "./splits".into(),
//!     skip_type_check: false,
//! })?;
//!
//! // Compose to a single Wasm component, in-process — no shelling out.
//! let composed: Vec<u8> = bundle.to_wasm()?;
//! std::fs::write("composed.wasm", &composed)?;
//!
//! for adapter in &bundle.generated_adapters {
//!     println!(
//!         "generated adapter for middleware '{}' at {}",
//!         adapter.middleware_name, adapter.adapter_path,
//!     );
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Want to drive `wac compose` yourself?
//!
//! [`Bundle::wac`] and [`Bundle::wac_deps`] expose the raw inputs.
//! Write the WAC to disk and call [`Bundle::wac_compose_cmd`] to get
//! the equivalent `wac compose ... --dep ...` shell command, or feed
//! `wac_deps` directly into
//! [`wac_resolver::FileSystemPackageResolver`](https://docs.rs/wac-resolver)
//! — the keys are fully-qualified WAC package keys, the values are
//! `PathBuf`s, no translation step required.
//!
//! For finer control over the in-process path (e.g. a custom
//! filesystem search base for unresolved package references), reach
//! for [`compose_wac`].
//!
//! # Side effects on disk
//!
//! Both [`splice`] and [`compose`] write files as part of their work:
//!
//! - [`splice`] writes one `.wasm` file per sub-component into
//!   `splits_dir` (the splitter pass), and may write
//!   `splicer_adapter_*.wasm` files alongside them (the adapter
//!   generator). Adapter paths are surfaced in
//!   [`Bundle::generated_adapters`] and [`Bundle::wac_deps`].
//! - Neither function writes the generated WAC source — that's
//!   returned in [`Bundle::wac`] for the caller to use however they
//!   like (typically by passing the bundle to [`Bundle::to_wasm`]).

mod adapter;
mod api;
mod builtins;
mod compose;
mod contract;
mod parse;
mod split;
mod wac;

#[cfg(test)]
mod tests;

// ── Top-level entry points ────────────────────────────────────────
pub use api::{
    compose, compose_wac, format_wac_compose_cmd, splice, Bundle, ComponentInput, ComposeRequest,
    SpliceRequest,
};

/// Re-export so consumers pick up the exact cviz version splicer
/// links against, avoiding version-skew on shared types.
pub use ::cviz;

// ── Shared types ──────────────────────────────────────────────────

/// Types that appear on the public API surface and may be useful to
/// import directly.
pub mod types {
    pub use crate::contract::{
        ContractResult, TIER1_AFTER, TIER1_BEFORE, TIER1_BLOCKING, TIER1_INTERFACES, TIER1_PACKAGE,
        TIER1_VERSION, TIER2_AFTER, TIER2_BEFORE, TIER2_INTERFACES, TIER2_PACKAGE, TIER2_TRAP,
        TIER2_VERSION,
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
