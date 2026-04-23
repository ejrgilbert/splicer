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
//! # Quick start: shell out to `wac compose`
//!
//! ```no_run
//! # fn main() -> anyhow::Result<()> {
//! let rules_yaml = std::fs::read_to_string("splice.yaml")?;
//! let out = splicer::splice(splicer::SpliceRequest {
//!     composition_wasm: "composition.wasm".into(),
//!     rules_yaml,
//!     package_name: "example:composition".into(),
//!     splits_dir: "./splits".into(),
//!     skip_type_check: false,
//! })?;
//!
//! std::fs::write("output.wac", &out.wac)?;
//! println!("{}", out.wac_compose_cmd("output.wac"));
//!
//! for adapter in &out.generated_adapters {
//!     println!(
//!         "generated adapter for middleware '{}' at {}",
//!         adapter.middleware_name, adapter.adapter_path,
//!     );
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Programmatic compose with the `wac` crates
//!
//! [`SpliceOutput::wac_deps`] is shaped to plug straight into
//! [`wac_resolver::FileSystemPackageResolver`](https://docs.rs/wac-resolver/0.9/wac_resolver/struct.FileSystemPackageResolver.html)
//! — the keys are fully-qualified WAC package keys (e.g. `"my:srv-a"`),
//! the values are `PathBuf`s. The full programmatic pipeline that
//! mirrors `wac compose` looks like this:
//!
//! ```ignore
//! use std::collections::HashMap;
//! use wac_parser::Document;
//! use wac_resolver::{packages, FileSystemPackageResolver};
//! use wac_graph::EncodeOptions;
//!
//! # fn run(out: splicer::SpliceOutput) -> anyhow::Result<Vec<u8>> {
//! // 1. parse the WAC source splicer emitted
//! let doc = Document::parse(&out.wac)?;
//!
//! // 2. discover which package keys the document references
//! let keys = packages(&doc)?;
//!
//! // 3. resolve those keys against splicer's wac_deps map
//! let overrides: HashMap<String, std::path::PathBuf> =
//!     out.wac_deps.into_iter().collect();
//! let resolver = FileSystemPackageResolver::new(".", overrides, true);
//! let pkgs = resolver.resolve(&keys)?;
//!
//! // 4. resolve and encode
//! let resolution = doc.resolve(pkgs)?;
//! let composed: Vec<u8> = resolution.encode(EncodeOptions::default())?;
//! # Ok(composed)
//! # }
//! ```
//!
//! See `examples/wac_compose.rs` for a fully runnable version of this
//! pipeline.
//!
//! # Side effects on disk
//!
//! Both [`splice`] and [`compose`] write files as part of their work:
//!
//! - [`splice`] writes one `.wasm` file per sub-component into
//!   `splits_dir` (the splitter pass), and may write
//!   `splicer_adapter_*.wasm` files alongside them (the adapter
//!   generator). Adapter paths are surfaced in
//!   [`SpliceOutput::generated_adapters`] and [`SpliceOutput::wac_deps`].
//! - Neither function writes the generated WAC source — that's returned
//!   in [`SpliceOutput::wac`] / [`ComposeOutput::wac`] for the caller
//!   to write wherever they want.

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

/// Re-export so consumers pick up the exact cviz version splicer
/// links against, avoiding version-skew on shared types.
pub use ::cviz;

// ── Shared types ──────────────────────────────────────────────────

/// Types that appear on the public API surface and may be useful to
/// import directly.
pub mod types {
    pub use crate::contract::{
        ContractResult, TIER1_AFTER, TIER1_BEFORE, TIER1_BLOCKING, TIER1_INTERFACES, TIER1_PACKAGE,
        TIER1_VERSION,
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
