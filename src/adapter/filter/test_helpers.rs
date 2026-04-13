//! Shared fixtures and binary-layout helpers for the `filter`
//! submodule's tests.
//!
//! Lives in its own file (rather than inline in one test module) so
//! both `section_filter::tests` and `raw_sections_reencoder::tests`
//! can use the same fixture builders, and so they can derive
//! expected closure shapes from the **actual binary layout** of
//! their fixtures rather than from positional assumptions about
//! what `wat::parse_str` produced.

use std::collections::{BTreeMap, BTreeSet};

use super::HandlerDeps;

/// Build a fixture where the handler imports references the resource
/// aliases at the given positions in `[0, num_aliases)`. Section
/// layout (with the wirm `add_to_sections` collapse fix in place):
///
/// ```text
/// 0: ComponentType   (types instance type, num_aliases resource exports)
/// 1: ComponentImport (wasi:http/types)
/// 2: Alias           (num_aliases items)
/// 3: ComponentType   (handler instance type)
/// 4: ComponentImport (wasi:http/handler)
/// ```
///
/// Used by both `section_filter::tests` (to verify dep walker output)
/// and `raw_sections_reencoder::tests` (to verify the reencoded
/// output validates and contains the expected items).
pub(crate) fn alias_section_fixture(num_aliases: usize, handler_uses: &[usize]) -> Vec<u8> {
    assert!(num_aliases > 0, "fixture needs at least one alias");
    assert!(
        !handler_uses.is_empty(),
        "handler must reference at least one alias for the WAT to be valid"
    );
    for &i in handler_uses {
        assert!(i < num_aliases, "handler_uses index {i} out of range");
    }

    let resource_exports: String = (0..num_aliases)
        .map(|i| format!(r#"(export "r{i}" (type (sub resource)))"#))
        .collect::<Vec<_>>()
        .join(" ");

    let aliases: String = (0..num_aliases)
        .map(|i| format!(r#"(alias export 0 "r{i}" (type))"#))
        .collect::<Vec<_>>()
        .join(" ");

    // For each alias the handler uses, emit `(alias outer 1 (1+i) (type))`
    // — type 0 is the types instance type, so component type `1+i` is
    // the i-th alias-produced resource. Then export the body-local
    // copy so the import is non-empty (some toolchains reject empty
    // instance type bodies).
    let handler_body: String = handler_uses
        .iter()
        .enumerate()
        .map(|(body_local, &outer_alias_idx)| {
            let outer_comp_type = outer_alias_idx + 1;
            format!(
                r#"(alias outer 1 {outer_comp_type} (type)) (export "e{body_local}" (type (eq {body_local})))"#
            )
        })
        .collect::<Vec<_>>()
        .join(" ");

    let handler_type_idx = num_aliases + 1;

    let wat = format!(
        r#"
        (component
          (type (instance {resource_exports}))
          (import "wasi:http/types" (instance (type 0)))
          {aliases}
          (type (instance {handler_body}))
          (import "wasi:http/handler" (instance (type {handler_type_idx}))))
        "#
    );

    wat::parse_str(&wat).unwrap_or_else(|e| panic!("invalid WAT:\n{wat}\nerror: {e}"))
}

// ─── Binary layout (wasmparser-derived) ──────────────────────────────────────
//
// Tests in this submodule should derive their expected
// `HandlerDeps` shape from the **actual binary section layout**
// of the fixture, not from positional assumptions about what
// `wat::parse_str` produced. The dep walker's `section_idx` is
// supposed to match wasmparser's binary section ordinal — that's
// the invariant the wirm `section_count_invariant_*` tests defend.
// If a splicer test hardcodes a section number and the layout
// changes (because of a wat upgrade, or a wirm fix, or anything
// else), the test masks the change instead of catching it.
//
// `BinaryLayout::from_bytes` walks the fixture independently with
// wasmparser and returns a structured view that tests can query
// by name (for imports / aliases) or by position-within-kind (for
// types). The expected `HandlerDeps` is then computed from the
// layout, not assumed.

/// Snapshot of a wasm component's section layout, parsed via
/// wasmparser.
#[derive(Debug)]
pub(crate) struct BinaryLayout {
    pub sections: Vec<BinarySection>,
}

#[derive(Debug)]
pub(crate) struct BinarySection {
    /// Section ordinal — should match `cx.curr_section_idx()` in
    /// the dep walker.
    pub idx: usize,
    pub kind: BinarySectionKind,
    /// Per-item info, in source order. The position within this
    /// `Vec` is the in-section item index that the dep walker uses
    /// in `HandlerDeps::needed`.
    pub items: Vec<BinaryItem>,
}

#[derive(Debug)]
pub(crate) struct BinaryItem {
    /// Optional name. Set for `ComponentImport` (the import name)
    /// and for `ComponentAlias::InstanceExport` (the export name).
    /// `None` for items without a natural name (top-level types,
    /// alias outers, etc.).
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinarySectionKind {
    Type,
    Import,
    Alias,
    /// Sections we don't filter through (canon, instance, export,
    /// module, etc.) but which still bump section_idx.
    Other,
}

impl BinaryLayout {
    /// Walk `bytes` with wasmparser and produce the section layout.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        use wasmparser::{ComponentAlias, Parser, Payload};
        let mut sections = Vec::new();
        let mut idx = 0usize;
        for payload in Parser::new(0).parse_all(bytes) {
            let payload = payload.expect("parse fixture for binary layout");
            let (kind, items) = match payload {
                Payload::ComponentTypeSection(reader) => {
                    let count = reader.count() as usize;
                    let items = (0..count).map(|_| BinaryItem { name: None }).collect();
                    (BinarySectionKind::Type, items)
                }
                Payload::ComponentImportSection(reader) => {
                    let items = reader
                        .into_iter()
                        .map(|im| BinaryItem {
                            name: Some(im.expect("import").name.0.to_string()),
                        })
                        .collect();
                    (BinarySectionKind::Import, items)
                }
                Payload::ComponentAliasSection(reader) => {
                    let items = reader
                        .into_iter()
                        .map(|a| {
                            let alias = a.expect("alias");
                            let name = match alias {
                                ComponentAlias::InstanceExport { name, .. } => {
                                    Some(name.to_string())
                                }
                                ComponentAlias::CoreInstanceExport { name, .. } => {
                                    Some(name.to_string())
                                }
                                ComponentAlias::Outer { .. } => None,
                            };
                            BinaryItem { name }
                        })
                        .collect();
                    (BinarySectionKind::Alias, items)
                }
                Payload::ComponentInstanceSection(_)
                | Payload::ComponentCanonicalSection(_)
                | Payload::ComponentExportSection(_)
                | Payload::CoreTypeSection(_)
                | Payload::InstanceSection(_)
                | Payload::ModuleSection { .. }
                | Payload::ComponentSection { .. }
                | Payload::ComponentStartSection { .. }
                | Payload::CustomSection(_) => (BinarySectionKind::Other, Vec::new()),
                _ => continue, // not a section payload (Version / End / etc.)
            };
            sections.push(BinarySection { idx, kind, items });
            idx += 1;
        }
        Self { sections }
    }

    /// Find the `(section_idx, in_section_idx)` of an import by name.
    pub fn import_loc(&self, name: &str) -> Option<(usize, usize)> {
        self.find_named(BinarySectionKind::Import, name)
    }

    /// Find the `(section_idx, in_section_idx)` of an alias by name
    /// (only `InstanceExport` aliases have names).
    pub fn alias_loc(&self, name: &str) -> Option<(usize, usize)> {
        self.find_named(BinarySectionKind::Alias, name)
    }

    fn find_named(&self, kind: BinarySectionKind, name: &str) -> Option<(usize, usize)> {
        for section in &self.sections {
            if section.kind != kind {
                continue;
            }
            for (i, item) in section.items.iter().enumerate() {
                if item.name.as_deref() == Some(name) {
                    return Some((section.idx, i));
                }
            }
        }
        None
    }

    /// Return all top-level type items as `(section_idx, in_section_idx)`
    /// pairs in source order. Useful for tests that need to identify
    /// types positionally (since they don't have names).
    pub fn type_locs(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for section in &self.sections {
            if section.kind != BinarySectionKind::Type {
                continue;
            }
            for i in 0..section.items.len() {
                out.push((section.idx, i));
            }
        }
        out
    }

    /// Return all alias items as `(section_idx, in_section_idx)`
    /// pairs in source order. The `i`-th alias in source order is
    /// at the `i`-th position in the returned vec.
    pub fn alias_locs(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for section in &self.sections {
            if section.kind != BinarySectionKind::Alias {
                continue;
            }
            for i in 0..section.items.len() {
                out.push((section.idx, i));
            }
        }
        out
    }
}

/// Bucket a flat set of `(section_idx, in_section_idx)` pairs into
/// the `HandlerDeps`-shaped `BTreeMap<usize, BTreeSet<usize>>`.
/// Tests use this to convert "the items I expect to keep" into the
/// shape the dep walker actually returns.
pub(crate) fn expected_deps(locs: &[(usize, usize)]) -> BTreeMap<usize, BTreeSet<usize>> {
    let mut by_section: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for &(section, item) in locs {
        by_section.entry(section).or_default().insert(item);
    }
    by_section
}

/// Sanity check that a fixture's binary layout has the kinds we
/// expect — used as a guard at the top of tests so a layout shift
/// produces a clear error before any closure-shape assertions.
pub(crate) fn assert_layout_kinds(layout: &BinaryLayout, expected: &[BinarySectionKind]) {
    let actual: Vec<BinarySectionKind> = layout.sections.iter().map(|s| s.kind).collect();
    assert_eq!(
        actual, expected,
        "binary layout shape changed — fixture or wat encoder produced \
         a different section sequence than the test expected. Got {:?}",
        actual
    );
}

/// Assert that the dep walker's [`HandlerDeps`] matches the closure
/// shape implied by `expected_locs`, with both sides expressed as
/// `(section_idx, in_section_idx)` pairs derived from
/// [`BinaryLayout`] queries.
pub(crate) fn assert_deps_match(actual: &HandlerDeps, expected_locs: &[(usize, usize)]) {
    let expected = expected_deps(expected_locs);
    assert_eq!(
        actual.needed, expected,
        "dep walker output did not match the closure shape derived from the \
         binary layout. Expected {:?}, got {:?}",
        expected, actual.needed
    );
}

/// Compute the expected closure locations for an
/// [`alias_section_fixture`]-shaped fixture, derived from its
/// [`BinaryLayout`].
///
/// The fixture always lays out as:
///
///   - all top-level types (types-instance type + handler instance type)
///   - the wasi:http/types instance import
///   - the alias section (with `num_aliases` resource aliases)
///   - the wasi:http/handler instance import
///
/// The closure for a given `handler_uses` subset always keeps
/// every top-level type, both imports, and only the alias items
/// at the positions named in `handler_uses`. This helper does the
/// `BinaryLayout` traversal so callers don't have to repeat it
/// in every test.
pub(crate) fn alias_section_expected_locs(
    layout: &BinaryLayout,
    handler_uses: &[usize],
) -> Vec<(usize, usize)> {
    let mut locs = layout.type_locs();
    let alias_locs = layout.alias_locs();
    for &i in handler_uses {
        locs.push(alias_locs[i]);
    }
    locs.push(
        layout
            .import_loc("wasi:http/types")
            .expect("wasi:http/types in layout"),
    );
    locs.push(
        layout
            .import_loc("wasi:http/handler")
            .expect("wasi:http/handler in layout"),
    );
    locs
}
