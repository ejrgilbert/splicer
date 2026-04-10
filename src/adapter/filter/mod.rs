//! # Closure-based filtering of a downstream split's import preamble
//!
//! Two phases live in this submodule:
//!
//! 1. [`section_filter`] computes the **dependency closure** of a target
//!    import in a downstream split. Output is a [`HandlerDeps`] map of
//!    `section_ordinal → set of in-section item indices` describing
//!    exactly which top-level type/import/alias items the target
//!    transitively depends on.
//!
//! 2. [`raw_sections_reencoder`] consumes a [`HandlerDeps`] and produces
//!    [`FilteredSections`] — re-encoded type/import/alias section bytes
//!    with all surviving items present, all dropped items removed, and
//!    every embedded type/instance index translated through an
//!    `old_idx → new_idx` map so the result is a self-consistent piece
//!    of a wasm component.
//!
//! Together they let `generate_tier1_adapter` build an adapter that
//! inherits *only* the import shape its target interface needs from a
//! fan-in split, instead of dragging along every unrelated import in
//! the source binary.

pub(crate) mod raw_sections_reencoder;
pub(crate) mod section_filter;
#[cfg(test)]
mod test_helpers;

pub(crate) use raw_sections_reencoder::{extract_filtered_sections, FilteredSections};
pub(crate) use section_filter::{find_handler_deps, HandlerDeps};
