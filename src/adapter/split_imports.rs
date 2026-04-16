use std::collections::HashMap;
use wasm_encoder::ComponentSectionId;

use super::filter::FilteredSections;

/// Extracted import structure from a split component.
///
/// Populated by [`super::filter::extract_filtered_sections`] via the
/// [`From`] impl below. Raw section bytes are injected verbatim into
/// the adapter component; the metadata fields describe the shape of
/// those bytes so the adapter builder can reference them by the right
/// indices.
pub(crate) struct SplitImports {
    /// Raw section bytes `(section_kind, data)` for all type + import + alias
    /// sections, in order. These define the split's full import structure.
    pub raw_sections: Vec<(ComponentSectionId, Vec<u8>)>,
    /// Names of imported instances, in order of their instance index.
    pub import_names: Vec<String>,
    /// Total number of component-level types declared across all sections.
    pub type_count: u32,
    /// Total number of instances imported.
    pub instance_count: u32,
    /// Maps type-export names (e.g. "request", "response", "error-code")
    /// to their component-scope type indices, as produced by
    /// `alias instance-export` declarations in the filtered sections.
    /// Used by the adapter builder to reference resource types that are
    /// already present in the copied sections via `alias outer` instead
    /// of declaring fresh `SubResource` exports.
    pub aliased_type_exports: HashMap<String, u32>,
}

impl From<FilteredSections> for SplitImports {
    fn from(f: FilteredSections) -> Self {
        Self {
            raw_sections: f.raw_sections,
            import_names: f.import_names,
            type_count: f.type_count,
            instance_count: f.instance_count,
            aliased_type_exports: f.aliased_type_exports,
        }
    }
}
