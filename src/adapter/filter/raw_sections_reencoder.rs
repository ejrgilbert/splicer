//! # Closure re-encoder
//!
//! Step 3 of the closure-based filter. Given a [`HandlerDeps`] from
//! [`super::section_filter::find_handler_deps`], produce
//! [`FilteredSections`] — re-encoded type/import/alias section bytes
//! that contain only the items in the closure, with every embedded
//! type/instance index translated to its new position in the (smaller)
//! filtered space.
//!
//! ## Why we re-encode instead of byte-slicing
//!
//! The component model type space is flat and positional: when an item
//! is dropped, every item that came after it shifts down by one, and
//! any reference that pointed at the original index is now off by one
//! (or more). Byte slicing — copying surviving items' bytes verbatim —
//! only works when the dropped items are a contiguous suffix of every
//! space. For fan-in splits where the target import sits *after* a
//! pile of unrelated imports (e.g. `wasi:http/handler` after a bunch
//! of `my:service/*` imports), dropping the prefix would shift the
//! kept items and break their references. So we re-encode: walk the
//! surviving items in source order, translate every embedded
//! type/instance index through an `old_idx → new_idx` map, and emit
//! via wasm_encoder.
//!
//! ## Why source order is sufficient (no topological sort needed)
//!
//! The original split is a valid wasm component, which means every
//! reference in it points at an item earlier in source order (the wasm
//! validator requires this). Since we *only drop items, never insert
//! or reorder*, the surviving items maintain their relative order,
//! and every reference in a surviving item still points at an earlier
//! surviving item. Allocating new indices in source order produces a
//! monotonic `old_idx → new_idx` mapping.
//!
//! Wirm uses [`wirm::ir::component::visitor::walk_topological`] for
//! its own encoder because it sits at the end of an instrumentation
//! pipeline that may have inserted or reordered items — for which
//! source order is no longer guaranteed to be topological. Pure
//! filters like this one don't need that machinery.
//!
//! ## Why `wasm_encoder::reencode::ReencodeComponent`
//!
//! The trait already implements the structural walk for every
//! component item kind: defined types, instance type bodies, alias
//! outers, every nested compound type. Each embedded ref calls back
//! into a small set of per-namespace hooks (`component_type_index`,
//! `component_instance_index`, `outer_type_index`, etc.) whose
//! default impls are identity. We override the hooks to apply our
//! `old_idx → new_idx` map; the trait's section parsers
//! (`parse_component_type_section`, …) are overridden to filter
//! items before passing them to the per-item parser.
//!
//! The translation maps are built **during** the same walk:
//! `parse_component_type_section` allocates a new index for each kept
//! item *before* calling `parse_component_type` on it, so any ref in
//! the item's body that points back at an earlier kept item already
//! has its new index in the map by the time the body walk hits it.
//!
//! ## Section ordering
//!
//! The component model spec does **not** require type/import/alias
//! sections to come before any body section (canon, instance, export,
//! …). Toolchains can interleave them freely. The driver loop in
//! [`extract_filtered_sections`] therefore walks the *entire* binary,
//! not just a leading "preamble" — and bumps `section_idx` for every
//! section payload (including ones we don't emit) so the ordinal
//! matches what the dep walker (which uses
//! [`wirm::ir::component::visitor::VisitCtx::curr_section_idx`]) saw.
//!
//! Body sections like `ComponentInstanceSection` and
//! `ComponentExportSection` add items to namespaces we track (CompInst
//! and a kind-dependent space, respectively). The reencoder walks
//! their items just to bump the `orig_*` counters, then drops the
//! sections from the output. That keeps the original-side bookkeeping
//! consistent with what wirm saw, even when type/import/alias sections
//! appear *after* body sections that contributed to the same space.

use std::collections::HashMap;

use anyhow::Context;
use wasm_encoder::reencode::ReencodeComponent;
use wasm_encoder::{
    ComponentAliasSection, ComponentImportSection, ComponentSectionId, ComponentTypeSection,
};
use wasmparser::{
    ComponentAlias, ComponentExportSectionReader, ComponentExternalKind,
    ComponentInstanceSectionReader, ComponentOuterAliasKind, ComponentTypeRef, Parser, Payload,
};

use super::section_filter::HandlerDeps;

/// Filtered and reindexed type/import/alias section bytes ready to be
/// injected into an adapter component.
///
/// Mirrors the shape of [`crate::adapter::split_imports::SplitImports`]
/// so the adapter consumer can swap between the verbatim-copy path and
/// this filtered path with minimal code changes.
#[derive(Debug)]
pub(crate) struct FilteredSections {
    /// `(section_id, content_bytes)` pairs in source order.
    /// `content_bytes` is the section *content* (item count + items),
    /// suitable to feed to `wasm_encoder::RawSection { id, data: &content_bytes }`.
    pub raw_sections: Vec<(u8, Vec<u8>)>,
    /// Names of surviving instance imports, in source order.
    pub import_names: Vec<String>,
    /// Total component-level types contributed by the surviving items.
    pub type_count: u32,
    /// Total component instances contributed by the surviving items.
    pub instance_count: u32,
}

/// Drive the closure re-encoder over a downstream split.
///
/// Walks the binary once with wasmparser, dispatches each
/// type/import/alias section to a [`ClosureReencoder`] that filters items
/// and translates indices, and bumps the `orig_*` counters for body
/// sections that contribute items to spaces we track. Returns the
/// filtered section bytes plus the metadata the adapter needs to wire up
/// its own additions.
pub(crate) fn extract_filtered_sections(
    bytes: &[u8],
    deps: &HandlerDeps,
) -> anyhow::Result<FilteredSections> {
    let mut reencoder = ClosureReencoder::new(deps);
    let mut out = wasm_encoder::Component::new();

    let mut section_idx = 0usize;
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.context("parsing split for re-encode")?;
        reencoder.current_section_idx = section_idx;
        match payload {
            // ─── sections we filter + emit ───────────────────────────
            Payload::ComponentTypeSection(section) => {
                let mut types = ComponentTypeSection::new();
                reencoder
                    .parse_component_type_section(&mut types, section)
                    .map_err(|e| anyhow::anyhow!("type section reencode: {e:?}"))?;
                if !types.is_empty() {
                    out.section(&types);
                }
                section_idx += 1;
            }
            Payload::ComponentImportSection(section) => {
                let mut imports = ComponentImportSection::new();
                reencoder
                    .parse_component_import_section(&mut imports, section)
                    .map_err(|e| anyhow::anyhow!("import section reencode: {e:?}"))?;
                if !imports.is_empty() {
                    out.section(&imports);
                }
                section_idx += 1;
            }
            Payload::ComponentAliasSection(section) => {
                let mut aliases = ComponentAliasSection::new();
                reencoder
                    .parse_component_alias_section(&mut aliases, section)
                    .map_err(|e| anyhow::anyhow!("alias section reencode: {e:?}"))?;
                if !aliases.is_empty() {
                    out.section(&aliases);
                }
                section_idx += 1;
            }

            // ─── sections we don't emit but need to count ────────────
            // ComponentInstanceSection contributes one item per entry to
            // CompInst — bump orig_inst so any later type/import/alias
            // section that shows up after this body section gets its
            // CompInst keys right.
            Payload::ComponentInstanceSection(section) => {
                reencoder.absorb_component_instance_section(section)?;
                section_idx += 1;
            }
            // ComponentExportSection adds to whatever space the export
            // kind names. We dispatch per item.
            Payload::ComponentExportSection(section) => {
                reencoder.absorb_component_export_section(section)?;
                section_idx += 1;
            }

            // ─── other sections: bump section_idx, ignore items ──────
            // None of these contribute items to spaces our closure
            // walker tracks (CompType / CompInst), so we just need to
            // keep section_idx aligned with wirm's count.
            Payload::ComponentCanonicalSection(_)
            | Payload::CoreTypeSection(_)
            | Payload::InstanceSection(_)
            | Payload::ModuleSection { .. }
            | Payload::ComponentSection { .. }
            | Payload::ComponentStartSection { .. }
            | Payload::CustomSection(_) => {
                section_idx += 1;
            }

            // Version header / End marker / non-component payloads —
            // not section payloads in wirm's count.
            _ => {}
        }
    }

    let type_count = reencoder.type_map.len() as u32;
    let instance_count = reencoder.instance_map.len() as u32;
    let import_names = reencoder.import_names;

    // Re-parse the freshly-built component to peel each section's
    // content range out of the byte stream. We can't access
    // wasm_encoder's internal section buffers directly, so the cheapest
    // path is to walk the encoded bytes once more with wasmparser and
    // copy out each section's range.
    let out_bytes = out.finish();
    let raw_sections = peel_section_contents(&out_bytes)?;

    Ok(FilteredSections {
        raw_sections,
        import_names,
        type_count,
        instance_count,
    })
}

/// Walk a wasm component byte buffer and return `(section_id, content)`
/// for every type/import/alias section, where `content` is the section's
/// item count + items (i.e. exactly what `wasm_encoder::RawSection.data`
/// expects).
fn peel_section_contents(bytes: &[u8]) -> anyhow::Result<Vec<(u8, Vec<u8>)>> {
    let mut out = Vec::new();
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.context("re-parsing filtered output")?;
        match &payload {
            Payload::ComponentTypeSection(reader) => {
                let range = reader.range();
                out.push((
                    ComponentSectionId::Type as u8,
                    bytes[range.start..range.end].to_vec(),
                ));
            }
            Payload::ComponentImportSection(reader) => {
                let range = reader.range();
                out.push((
                    ComponentSectionId::Import as u8,
                    bytes[range.start..range.end].to_vec(),
                ));
            }
            Payload::ComponentAliasSection(reader) => {
                let range = reader.range();
                out.push((
                    ComponentSectionId::Alias as u8,
                    bytes[range.start..range.end].to_vec(),
                ));
            }
            _ => {}
        }
    }
    Ok(out)
}

// ─── ClosureReencoder ────────────────────────────────────────────────────────

/// `ReencodeComponent` impl that filters items by a [`HandlerDeps`]
/// closure and translates indices through `old_idx → new_idx` maps
/// built incrementally during the same walk.
///
/// State invariants:
///
/// - `orig_*` counters are the **original** index that the next item
///   we walk would have had in the unfiltered split. They are bumped
///   for *every* item we walk past in the appropriate space —
///   including items in body sections we don't emit — so the next
///   kept item gets its correct old index recorded as the map key.
/// - `*_map.len()` doubles as the **next available new index** — we
///   allocate sequentially as we encounter kept items.
/// - `body_depth` is bumped by `push_depth` and popped by `pop_depth`.
///   When `body_depth > 0` we're inside an instance/component type
///   body and the index hooks leave refs untranslated (body-local
///   refs are private to their parent and we never filter inside
///   bodies, so they need no translation).
struct ClosureReencoder<'a> {
    deps: &'a HandlerDeps,

    type_map: HashMap<u32, u32>,
    instance_map: HashMap<u32, u32>,

    orig_type: u32,
    orig_inst: u32,

    /// Set by the driver loop in [`extract_filtered_sections`] before
    /// each section parse, so the section-filter overrides know which
    /// entry of `deps.needed` to look up.
    current_section_idx: usize,

    /// Depth bookkeeping for nested type bodies.
    body_depth: usize,

    /// Names of surviving instance imports, in source order.
    import_names: Vec<String>,
}

impl<'a> ClosureReencoder<'a> {
    fn new(deps: &'a HandlerDeps) -> Self {
        Self {
            deps,
            type_map: HashMap::new(),
            instance_map: HashMap::new(),
            orig_type: 0,
            orig_inst: 0,
            current_section_idx: 0,
            body_depth: 0,
            import_names: Vec::new(),
        }
    }

    /// `true` if item index `i` is in the closure for the current section.
    fn item_kept(&self, i: usize) -> bool {
        self.deps
            .needed
            .get(&self.current_section_idx)
            .is_some_and(|s| s.contains(&i))
    }

    /// Walk a body's `ComponentInstanceSection` just for the side effect
    /// of bumping `orig_inst`. We don't emit anything from this section
    /// — instance sections live in the body and the adapter doesn't
    /// inherit them.
    fn absorb_component_instance_section(
        &mut self,
        section: ComponentInstanceSectionReader<'_>,
    ) -> anyhow::Result<()> {
        for item in section {
            let _ = item.context("walking component instance section")?;
            self.orig_inst += 1;
        }
        Ok(())
    }

    /// Walk a body's `ComponentExportSection` to bump the appropriate
    /// `orig_*` counters. Each export adds to the space named by its
    /// kind (Type → CompType, Instance → CompInst, others ignored).
    fn absorb_component_export_section(
        &mut self,
        section: ComponentExportSectionReader<'_>,
    ) -> anyhow::Result<()> {
        for export in section {
            let export = export.context("walking component export section")?;
            match export.kind {
                ComponentExternalKind::Type => self.orig_type += 1,
                ComponentExternalKind::Instance => self.orig_inst += 1,
                // Func / Module / Component / Value exports contribute
                // to spaces we don't track for the closure walker, so
                // we don't bump anything. If a future closure walker
                // starts tracking those spaces, this is the place to
                // bump them.
                _ => {}
            }
        }
        Ok(())
    }
}

impl<'a> wasm_encoder::reencode::Reencode for ClosureReencoder<'a> {
    type Error = anyhow::Error;
}

impl<'a> ReencodeComponent for ClosureReencoder<'a> {
    // ─── per-namespace translation hooks ────────────────────────────────

    fn component_type_index(&mut self, ty: u32) -> u32 {
        if self.body_depth == 0 {
            // Top-level ref → translate. If the lookup misses, leave
            // the index alone — that's a bug signal and the resulting
            // wasm will fail validation downstream, which is what we
            // want for a bad closure.
            self.type_map.get(&ty).copied().unwrap_or(ty)
        } else {
            // Body-local ref → don't translate. Body-local types are
            // unaffected because we keep type bodies atomically.
            ty
        }
    }

    fn component_instance_index(&mut self, ty: u32) -> u32 {
        if self.body_depth == 0 {
            self.instance_map.get(&ty).copied().unwrap_or(ty)
        } else {
            ty
        }
    }

    fn outer_type_index(
        &mut self,
        count: u32,
        ty: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        // An `alias outer count N (type)` decl inside a body climbs
        // `count` scopes outward. If `count >= body_depth`, we land
        // in (or above) the component scope and need to translate
        // against the top-level type map. If `count < body_depth`,
        // the ref lands in an enclosing body's local namespace, which
        // we don't translate.
        if (count as usize) >= self.body_depth {
            Ok(self.type_map.get(&ty).copied().unwrap_or(ty))
        } else {
            Ok(ty)
        }
    }

    fn outer_component_type_index(&mut self, count: u32, ty: u32) -> u32 {
        if (count as usize) >= self.body_depth {
            self.type_map.get(&ty).copied().unwrap_or(ty)
        } else {
            ty
        }
    }

    // ─── depth bookkeeping for nested type bodies ───────────────────────

    fn push_depth(&mut self) {
        self.body_depth += 1;
    }

    fn pop_depth(&mut self) {
        self.body_depth -= 1;
    }

    // ─── section-level filtering ────────────────────────────────────────

    fn parse_component_type_section(
        &mut self,
        dst: &mut wasm_encoder::ComponentTypeSection,
        section: wasmparser::ComponentTypeSectionReader<'_>,
    ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
        for (i, ty) in section.into_iter().enumerate() {
            let ty = ty.map_err(|e| {
                wasm_encoder::reencode::Error::UserError(anyhow::anyhow!(
                    "component type item {i}: {e}"
                ))
            })?;
            if self.item_kept(i) {
                // Allocate the new type-space index BEFORE walking the
                // body so any ref inside the body that points BACK to
                // this same item finds it in the map. (Component-level
                // types aren't self-recursive in practice, but the
                // ordering is the right invariant either way.)
                let new_idx = self.type_map.len() as u32;
                self.type_map.insert(self.orig_type, new_idx);

                // Delegate to the trait's per-item parser, which walks
                // the structure and calls our index hooks for every
                // embedded ref.
                self.parse_component_type(dst.ty(), ty)?;
            }
            self.orig_type += 1;
        }
        Ok(())
    }

    fn parse_component_import_section(
        &mut self,
        dst: &mut wasm_encoder::ComponentImportSection,
        section: wasmparser::ComponentImportSectionReader<'_>,
    ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
        eprintln!(
            "[reencoder] parse_import_section current_section_idx={} needed={:?}",
            self.current_section_idx,
            self.deps.needed.get(&self.current_section_idx)
        );
        for (i, import) in section.into_iter().enumerate() {
            let import = import.map_err(|e| {
                wasm_encoder::reencode::Error::UserError(anyhow::anyhow!(
                    "component import item {i}: {e}"
                ))
            })?;
            let kept = self.item_kept(i);

            // Imports contribute to whichever space their type ref
            // names. Track originals across all kinds — only one of
            // these matches per import, but we still need the original
            // counter bumped so the next import gets the right map key.
            match import.ty {
                ComponentTypeRef::Type(_) => {
                    if kept {
                        let new_idx = self.type_map.len() as u32;
                        self.type_map.insert(self.orig_type, new_idx);
                    }
                    self.orig_type += 1;
                }
                ComponentTypeRef::Instance(_) => {
                    if kept {
                        let new_idx = self.instance_map.len() as u32;
                        self.instance_map.insert(self.orig_inst, new_idx);
                        self.import_names.push(import.name.0.to_string());
                    }
                    self.orig_inst += 1;
                }
                ComponentTypeRef::Func(_)
                | ComponentTypeRef::Module(_)
                | ComponentTypeRef::Component(_)
                | ComponentTypeRef::Value(_) => {
                    // Splits we filter today don't put non-instance
                    // imports in the closure. If a future closure
                    // walker starts tracking these spaces, this is
                    // the place to add their orig_* bookkeeping.
                }
            }

            if kept {
                // The trait's component_type_ref hook translates the
                // embedded type ref via our index hooks.
                let translated_ty = self.component_type_ref(import.ty).map_err(|e| {
                    wasm_encoder::reencode::Error::UserError(anyhow::anyhow!(
                        "component import {} type ref: {e:?}",
                        import.name.0
                    ))
                })?;
                dst.import(import.name.0, translated_ty);
            }
        }
        Ok(())
    }

    fn parse_component_alias_section(
        &mut self,
        dst: &mut wasm_encoder::ComponentAliasSection,
        section: wasmparser::ComponentAliasSectionReader<'_>,
    ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
        for (i, alias) in section.into_iter().enumerate() {
            let alias = alias.map_err(|e| {
                wasm_encoder::reencode::Error::UserError(anyhow::anyhow!(
                    "component alias item {i}: {e}"
                ))
            })?;
            let kept = self.item_kept(i);

            // Each alias contributes to one space based on its kind.
            // We bump the original counter for the appropriate space
            // whether or not the item is kept.
            match alias_namespace(&alias) {
                AliasSpaceKind::Type => {
                    if kept {
                        let new_idx = self.type_map.len() as u32;
                        self.type_map.insert(self.orig_type, new_idx);
                    }
                    self.orig_type += 1;
                }
                AliasSpaceKind::Instance => {
                    if kept {
                        let new_idx = self.instance_map.len() as u32;
                        self.instance_map.insert(self.orig_inst, new_idx);
                    }
                    self.orig_inst += 1;
                }
                AliasSpaceKind::Other => {}
            }

            if kept {
                // The trait's component_alias hook walks the alias
                // and translates its embedded indices via our index
                // hooks. We just emit the result.
                let translated = self.component_alias(alias).map_err(|e| {
                    wasm_encoder::reencode::Error::UserError(anyhow::anyhow!(
                        "component alias item {i}: {e:?}"
                    ))
                })?;
                dst.alias(translated);
            }
        }
        Ok(())
    }
}

/// What namespace an alias item contributes to. Mirrors `IndexSpaceOf`
/// for `ComponentAlias` but only the variants we care about for
/// filtering.
enum AliasSpaceKind {
    Type,
    Instance,
    Other,
}

fn alias_namespace(alias: &ComponentAlias<'_>) -> AliasSpaceKind {
    match alias {
        ComponentAlias::InstanceExport { kind, .. } => match kind {
            ComponentExternalKind::Type => AliasSpaceKind::Type,
            ComponentExternalKind::Instance => AliasSpaceKind::Instance,
            _ => AliasSpaceKind::Other,
        },
        ComponentAlias::Outer { kind, .. } => match kind {
            ComponentOuterAliasKind::Type => AliasSpaceKind::Type,
            _ => AliasSpaceKind::Other,
        },
        ComponentAlias::CoreInstanceExport { .. } => AliasSpaceKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::super::section_filter::find_handler_deps_in_bytes;
    use super::*;

    /// Convert WAT source to component bytes (test helper).
    fn wat(src: &str) -> Vec<u8> {
        wat::parse_str(src).expect("invalid WAT in test fixture")
    }

    /// Wrap a list of `(section_id, content)` pairs in a complete
    /// component header so we can re-parse the result with wasmparser
    /// and assert against its structure.
    fn wrap_as_component(raw_sections: &[(u8, Vec<u8>)]) -> Vec<u8> {
        // Component preamble: magic + layer + version
        let mut bytes = vec![0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00];
        for (id, content) in raw_sections {
            bytes.push(*id);
            // LEB128-encode the content length
            let mut n = content.len() as u32;
            loop {
                let byte = (n & 0x7f) as u8;
                n >>= 7;
                if n == 0 {
                    bytes.push(byte);
                    break;
                } else {
                    bytes.push(byte | 0x80);
                }
            }
            bytes.extend_from_slice(content);
        }
        bytes
    }

    /// End-to-end helper: WAT → bytes → dep walk → reencode → assembled
    /// component bytes. Returns the [`FilteredSections`] and the
    /// reassembled component for downstream assertions.
    fn run_filter(wat_src: &str, target: &str) -> (FilteredSections, Vec<u8>) {
        let bytes = wat(wat_src);
        let deps = find_handler_deps_in_bytes(&bytes, target).expect("dep walk");
        let filtered = extract_filtered_sections(&bytes, &deps).expect("reencode");
        let reassembled = wrap_as_component(&filtered.raw_sections);
        (filtered, reassembled)
    }

    /// Count items per section kind in a wasm component byte buffer
    /// using wasmparser. Used to assert that the reencoder kept the
    /// expected items.
    fn count_top_level_items(bytes: &[u8]) -> (usize, usize, usize) {
        let mut types = 0usize;
        let mut imports = 0usize;
        let mut aliases = 0usize;
        for payload in wasmparser::Parser::new(0).parse_all(bytes) {
            match payload.expect("parse output") {
                Payload::ComponentTypeSection(reader) => types += reader.count() as usize,
                Payload::ComponentImportSection(reader) => imports += reader.count() as usize,
                Payload::ComponentAliasSection(reader) => aliases += reader.count() as usize,
                _ => {}
            }
        }
        (types, imports, aliases)
    }

    /// Validate that `bytes` parses as a wasm component, with the
    /// component-model async proposal enabled (the fan-in fixtures use
    /// async function types).
    fn validate_component(bytes: &[u8]) {
        let features = wasmparser::WasmFeatures::default()
            | wasmparser::WasmFeatures::CM_ASYNC
            | wasmparser::WasmFeatures::CM_ASYNC_BUILTINS
            | wasmparser::WasmFeatures::CM_ASYNC_STACKFUL;
        wasmparser::Validator::new_with_features(features)
            .validate_all(bytes)
            .expect("filtered output should validate");
    }

    /// Collect surviving import names from a wasm component byte buffer.
    fn collect_import_names(bytes: &[u8]) -> Vec<String> {
        let mut names = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(bytes) {
            if let Payload::ComponentImportSection(reader) = payload.expect("parse output") {
                for import in reader {
                    let import = import.expect("import");
                    names.push(import.name.0.to_string());
                }
            }
        }
        names
    }

    // ─── simple fan-in (alternating Type/Import sections) ────────────────

    /// Three-way fan-in with one type section + one import section per
    /// interface, alternating. Targeting the first interface should
    /// drop everything after — the result is the smallest possible
    /// useful filter.
    fn simple_fanin() -> &'static str {
        r#"
        (component
          (type (instance
            (type (func (param "a" s32) (param "b" s32) (result s32)))
            (export "add" (func (type 0)))))
          (import "my:service/adder" (instance (type 0)))

          (type (instance
            (type (func (result string)))
            (export "get-msg" (func (type 0)))))
          (import "my:service/messenger" (instance (type 1)))

          (type (instance
            (type (func (param "msg" string)))
            (export "print" (func (type 0)))))
          (import "my:service/printer" (instance (type 2)))
        )
        "#
    }

    #[test]
    fn simple_fanin_first_target_keeps_only_one_pair() {
        let (filtered, reassembled) = run_filter(simple_fanin(), "my:service/adder");
        assert_eq!(filtered.type_count, 1);
        assert_eq!(filtered.instance_count, 1);
        assert_eq!(filtered.import_names, vec!["my:service/adder".to_string()]);
        let (types, imports, aliases) = count_top_level_items(&reassembled);
        assert_eq!((types, imports, aliases), (1, 1, 0));
        assert_eq!(collect_import_names(&reassembled), vec!["my:service/adder"]);
    }

    #[test]
    fn simple_fanin_middle_target_keeps_only_middle_pair() {
        // Targeting the middle interface drops both the prefix (adder)
        // and the suffix (printer). The kept import's type ref must be
        // renumbered: messenger's type was originally at component
        // type idx 1, but in the filtered output it's the only type so
        // it has to be at idx 0. The reencoder's translation is what
        // makes this work.
        let (filtered, reassembled) = run_filter(simple_fanin(), "my:service/messenger");
        assert_eq!(filtered.type_count, 1);
        assert_eq!(filtered.instance_count, 1);
        assert_eq!(
            filtered.import_names,
            vec!["my:service/messenger".to_string()]
        );
        let (types, imports, aliases) = count_top_level_items(&reassembled);
        assert_eq!((types, imports, aliases), (1, 1, 0));
        assert_eq!(
            collect_import_names(&reassembled),
            vec!["my:service/messenger"]
        );

        // The reassembled component should validate as a wasm
        // component. If our index translation got it wrong, the
        // import's type ref would point at a non-existent type and
        // wasmparser would fail.
        validate_component(&reassembled);
    }

    #[test]
    fn simple_fanin_last_target_keeps_only_last_pair() {
        // Drop-prefix: targeting the last interface drops everything
        // before it. The kept import had type idx 2 in the original
        // (the third type defined); in the filtered output it must be
        // type idx 0. This is the case byte slicing alone could not
        // handle.
        let (filtered, reassembled) = run_filter(simple_fanin(), "my:service/printer");
        assert_eq!(filtered.type_count, 1);
        assert_eq!(filtered.instance_count, 1);
        assert_eq!(
            filtered.import_names,
            vec!["my:service/printer".to_string()]
        );
        validate_component(&reassembled);
    }

    // ─── alias-outer fan-in (the wasi:http/types-style chain) ────────────

    /// Fan-in fixture that exercises the alias-outer chain — the case
    /// the real fan-in service splits actually hit. A
    /// `wasi:http/types`-style instance is imported up front, then
    /// resource handles are aliased out of it, and the handler
    /// interface uses those aliased types via `alias outer` references
    /// inside its instance type body.
    fn alias_outer_fanin() -> &'static str {
        r#"
        (component
          (type (instance
            (export "request"  (type (sub resource)))
            (export "response" (type (sub resource)))
            (type (record (field "code" u16)))
            (export "error-info" (type (eq 2)))))
          (import "wasi:http/types" (instance (type 0)))

          (alias export 0 "request"    (type))
          (alias export 0 "response"   (type))
          (alias export 0 "error-info" (type))

          (type (instance
            (alias outer 1 1 (type))
            (alias outer 1 2 (type))
            (alias outer 1 3 (type))
            (type (own 0))
            (type (own 1))
            (type (result 4 (error 2)))
            (type (func async (param "req" 3) (result 5)))
            (export "handle" (func (type 6)))))
          (import "wasi:http/handler" (instance (type 4)))

          (type (instance
            (type (func (result string)))
            (export "get-msg" (func (type 0)))))
          (import "my:service/messenger" (instance (type 5)))
        )
        "#
    }

    #[test]
    fn alias_outer_fanin_renumbers_through_alias_chain() {
        // Closure for wasi:http/handler should pull in the types
        // instance, the types-instance import, all three resource
        // aliases, the handler instance type, and the handler import.
        // The messenger type + import must be dropped.
        let (filtered, reassembled) = run_filter(alias_outer_fanin(), "wasi:http/handler");

        // 2 component types kept (types instance + handler instance)
        // 3 alias-produced types kept (request, response, error-info)
        // → 5 types total
        assert_eq!(
            filtered.type_count, 5,
            "expected 2 type-section types + 3 alias types, got {}",
            filtered.type_count
        );
        // 2 instances kept (wasi:http/types import + handler import)
        assert_eq!(filtered.instance_count, 2);
        assert_eq!(
            filtered.import_names,
            vec!["wasi:http/types".to_string(), "wasi:http/handler".to_string()]
        );

        let (types, imports, aliases) = count_top_level_items(&reassembled);
        assert_eq!(types, 2, "type sections: types instance + handler instance");
        assert_eq!(imports, 2, "imports: wasi:http/types + wasi:http/handler");
        assert_eq!(aliases, 3, "aliases: request + response + error-info");

        assert_eq!(
            collect_import_names(&reassembled),
            vec!["wasi:http/types", "wasi:http/handler"]
        );

        // The renumbering invariant: the handler import's type ref
        // must point at the second type in the filtered output (the
        // handler instance type, now at idx 1), AND the alias outer
        // refs inside the handler body must climb out of the body
        // and land on the renumbered alias-produced types. If any
        // of those translations is wrong, validation fails.
        validate_component(&reassembled);
    }

    // ─── mixed clean/dirty/skip ─────────────────────────────────────────

    #[test]
    fn mixed_clean_dirty_and_skip_sections_validates() {
        // Two alias sections separated by a record-type section: the
        // first alias section is clean (both items needed), the
        // record section is unrelated and skipped, and the second
        // alias section is dirty (only its first item is needed).
        // This is the multi-classification matrix in a single
        // fixture.
        let src = r#"
            (component
              (type (instance
                (export "r0" (type (sub resource)))
                (export "r1" (type (sub resource)))
                (export "r2" (type (sub resource)))
                (export "r3" (type (sub resource)))))
              (import "wasi:http/types" (instance (type 0)))

              (alias export 0 "r0" (type))
              (alias export 0 "r1" (type))

              (type (record (field "x" u32)))

              (alias export 0 "r2" (type))
              (alias export 0 "r3" (type))

              (type (instance
                (alias outer 1 1 (type))
                (alias outer 1 2 (type))
                (alias outer 1 4 (type))
                (export "e0" (type (eq 0)))
                (export "e1" (type (eq 1)))
                (export "e2" (type (eq 2)))))
              (import "wasi:http/handler" (instance (type 6)))
            )
        "#;
        let (filtered, reassembled) = run_filter(src, "wasi:http/handler");

        // Kept: types instance type, 2 aliases r0/r1, 1 alias r2,
        // handler instance type → 5 types
        assert_eq!(filtered.type_count, 5);
        // Kept: wasi:http/types import + handler import
        assert_eq!(filtered.instance_count, 2);
        assert_eq!(
            filtered.import_names,
            vec!["wasi:http/types".to_string(), "wasi:http/handler".to_string()]
        );

        let (types, imports, aliases) = count_top_level_items(&reassembled);
        // 2 type sections kept (types instance + handler instance);
        // the record-type section was dropped because it's not in the
        // closure.
        assert_eq!(types, 2);
        assert_eq!(imports, 2);
        // 3 alias items kept across 2 alias sections (r0, r1, r2)
        assert_eq!(aliases, 3);

        validate_component(&reassembled);
    }

    /// Targeting an interface that doesn't exist in the split should
    /// produce an empty `FilteredSections`.
    #[test]
    fn missing_target_yields_empty_filtered_sections() {
        let (filtered, _) = run_filter(simple_fanin(), "no:such/iface");
        assert_eq!(filtered.type_count, 0);
        assert_eq!(filtered.instance_count, 0);
        assert!(filtered.raw_sections.is_empty());
        assert!(filtered.import_names.is_empty());
    }
}
