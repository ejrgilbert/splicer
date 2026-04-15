use anyhow::Context;
use wasm_encoder::ComponentSectionId;

use super::filter::FilteredSections;

/// Extracted import structure from a consumer split component.
/// Raw section bytes are copied verbatim into the adapter.
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
}

impl From<FilteredSections> for SplitImports {
    fn from(f: FilteredSections) -> Self {
        Self {
            raw_sections: f.raw_sections,
            import_names: f.import_names,
            type_count: f.type_count,
            instance_count: f.instance_count,
        }
    }
}

/// Read a split component binary and extract its type/import/alias sections
/// as raw bytes, along with import names and index counts.
pub(crate) fn extract_split_imports(split_path: &str) -> anyhow::Result<SplitImports> {
    let bytes = std::fs::read(split_path)
        .with_context(|| format!("Failed to read consumer split at '{}'", split_path))?;

    let mut raw_sections = Vec::new();
    let mut import_names = Vec::new();
    let mut type_count = 0u32;
    let mut instance_count = 0u32;

    let parser = wasmparser::Parser::new(0);
    for payload in parser.parse_all(&bytes) {
        let payload = payload?;
        match &payload {
            wasmparser::Payload::ComponentTypeSection(reader) => {
                type_count += reader.count();
                let range = reader.range();
                raw_sections.push((
                    ComponentSectionId::Type,
                    bytes[range.start..range.end].to_vec(),
                ));
            }
            wasmparser::Payload::ComponentImportSection(reader) => {
                let range = reader.range();
                for import in reader.clone() {
                    let import = import?;
                    import_names.push(import.name.0.to_string());
                    if matches!(import.ty, wasmparser::ComponentTypeRef::Instance(_)) {
                        instance_count += 1;
                    }
                }
                raw_sections.push((
                    ComponentSectionId::Import,
                    bytes[range.start..range.end].to_vec(),
                ));
            }
            wasmparser::Payload::ComponentAliasSection(reader) => {
                let range = reader.range();
                for alias in reader.clone() {
                    let alias = alias?;
                    if matches!(
                        alias,
                        wasmparser::ComponentAlias::InstanceExport {
                            kind: wasmparser::ComponentExternalKind::Type,
                            ..
                        } | wasmparser::ComponentAlias::Outer {
                            kind: wasmparser::ComponentOuterAliasKind::Type,
                            ..
                        }
                    ) {
                        type_count += 1;
                    }
                }
                raw_sections.push((
                    ComponentSectionId::Alias,
                    bytes[range.start..range.end].to_vec(),
                ));
            }
            // Stop at the first non-type/import/alias section — everything
            // after that is the split's internal implementation.
            wasmparser::Payload::ComponentInstanceSection(_)
            | wasmparser::Payload::ModuleSection { .. }
            | wasmparser::Payload::ComponentSection { .. }
            | wasmparser::Payload::ComponentCanonicalSection(_)
            | wasmparser::Payload::ComponentExportSection(_) => break,
            // Skip the component header and other preamble
            _ => {}
        }
    }

    Ok(SplitImports {
        raw_sections,
        import_names,
        type_count,
        instance_count,
    })
}
