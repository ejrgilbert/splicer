//! # Scoped section filtering for adapter component generation
//!
//! ## Why this module exists
//!
//! When `generate_tier1_adapter` builds an adapter for a middleware that fronts a
//! handler-providing component, the adapter has to inherit the same import structure
//! that the *consumer* of that handler exposes — same type instance, same alias
//! chain, same resource handles — so the wac composition step accepts it as a
//! drop-in replacement in front of that consumer.
//!
//! ## Chain vs fan-in (the shape that matters)
//!
//! splicer wraps middleware on top of one or more handler-providing services. A
//! **chain** config has each service consuming exactly one upstream interface,
//! so the consumer split only imports the interfaces needed by that single
//! handler:
//!
//! ```text
//!     caller ──▶ middleware-a ──▶ service-b
//!                    │                │
//!                    │   (split)      │
//!                    └─── imports ────┘
//!                            │
//!                            ▼
//!                  wasi:http/handler        ◀── target the adapter rebuilds
//!                  wasi:http/types          ◀── support type instance
//! ```
//!
//! For chain configs the simple "copy every type/import/alias section
//! verbatim" strategy in [`crate::adapter::split_imports`] works fine — every
//! import in the split is something the handler actually needs.
//!
//! A **fan-in** config has one middleware fronting a service that imports
//! several unrelated interfaces:
//!
//! ```text
//!     caller-1 ──▶ ┐
//!     caller-2 ──▶ ├──▶ middleware-a ──▶ service-b
//!     caller-3 ──▶ ┘                          │
//!                       (split)               │
//!                          │                  │
//!                          ▼                  │
//!              wasi:http/handler               │   ◀── target
//!              wasi:http/types                 │   ◀── handler dependency
//!              my:service/messenger-async  ────┘   ◀── unrelated! must NOT
//!                                                       leak into the adapter
//! ```
//!
//! Copying everything in the fan-in case brings `messenger-async` along for the
//! ride and wac rejects the resulting composition with `type mismatch for
//! import …`. The fix is to compute, ahead of the byte copy, which sections —
//! and which items inside each section — the target import actually depends
//! on, and copy only those.
//!
//! ## Two-pass approach
//!
//! 1. **wirm semantic walk** ([`find_handler_deps`], this module). We use wirm's
//!    structural visitor because wirm's [`VisitCtx::resolve`] understands
//!    `alias outer` chain resolution natively — exactly the gnarly part of the
//!    problem. The walk records where every top-level type / import / alias
//!    item lives (section ordinal + in-section item index), builds a dependency
//!    graph keyed by that location, and then BFS-closes the graph from the
//!    target import. The result is a [`HandlerDeps`] map: for each section
//!    that contributes anything to the closure, the set of in-section item
//!    indices that survive.
//!
//! 2. **wasmparser byte slicer** (Step 3, separate function). The wirm walk
//!    works on the parsed IR, but the adapter writer needs raw bytes. A
//!    second pass over the binary uses
//!    `wasmparser::SectionLimited::into_iter_with_offsets()` to recover
//!    per-item byte ranges. Sections classified as **clean** (every item in
//!    them is needed) get copied verbatim via `RawSection`; **dirty**
//!    sections (only some items needed) are reassembled by concatenating
//!    only the byte ranges of needed items and prepending an updated
//!    LEB128-encoded item count.
//!
//! ## Why locations and not type-space indices
//!
//! Step 3 cuts the binary by section + item position, not by type-space index.
//! Tracking dependencies as `(section_idx, in_section_item_idx)` directly
//! avoids the second translation step and naturally handles the case where
//! types contributed by imports / aliases / type sections all share the same
//! type space.

use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use wirm::ir::component::idx_spaces::Space;
use wirm::ir::component::refs::ReferencedIndices;
use wirm::ir::component::visitor::{walk_structural, ComponentVisitor, ItemKind, VisitCtx};
use wirm::wasmparser::{
    ComponentAlias, ComponentImport, ComponentType, ComponentTypeDeclaration,
    InstanceTypeDeclaration,
};

/// Result of a wirm-based handler dependency walk.
///
/// The `needed` map is keyed by **section ordinal** — the position of a
/// section in the split's `component.sections` list, zero-indexed in source
/// order, the same value the wirm visitor exposes via
/// `VisitCtx::curr_section_idx()`.
///
/// Each value is the set of **in-section item indices** that must be
/// preserved when emitting the adapter's pass-through imports — i.e. the
/// 0-based position of an item *within its enclosing section's item list*,
/// NOT the item's position in some component-level type/instance space.
/// Step 3's byte slicer feeds these directly into
/// `wasmparser::SectionLimited::into_iter_with_offsets()` to recover per-item
/// byte ranges.
///
/// Section ordinals not present as keys in `needed` are entirely droppable —
/// nothing in them contributes to the handler closure.
///
/// Example: `needed = { 2: {0, 3}, 4: {0} }` means
/// "from section 2, keep items 0 and 3; from section 4, keep item 0; drop
/// every other section."
#[derive(Debug, Default)]
pub(crate) struct HandlerDeps {
    pub needed: BTreeMap<usize, BTreeSet<usize>>,
}

impl HandlerDeps {
    /// `true` when the wirm walk did not find an import matching the requested
    /// target interface name. Caller may treat this as "fall back to the
    /// existing copy-everything path".
    pub fn is_empty(&self) -> bool {
        self.needed.is_empty()
    }
}

/// Walk a consumer split with wirm and compute the set of section items
/// that the target import (transitively) depends on.
///
/// `target_interface` is the **import name** to look for, e.g.
/// `wasi:http/handler@0.3.0-rc-2026-01-06`. If no import with that name is
/// found in the split, an empty [`HandlerDeps`] is returned.
pub(crate) fn find_handler_deps(
    split_path: &str,
    target_interface: &str,
) -> anyhow::Result<HandlerDeps> {
    let bytes = std::fs::read(split_path)
        .with_context(|| format!("Failed to read consumer split at '{split_path}'"))?;
    find_handler_deps_in_bytes(&bytes, target_interface)
}

/// Same as [`find_handler_deps`] but works on already-loaded bytes. Useful for
/// tests that build component fixtures inline via `wat::parse_str`.
pub(crate) fn find_handler_deps_in_bytes(
    bytes: &[u8],
    target_interface: &str,
) -> anyhow::Result<HandlerDeps> {
    let component = wirm::Component::parse(bytes, false, false)
        .context("Failed to parse consumer split bytes")?;
    let mut collector = DepCollector::new(target_interface);
    walk_structural(&component, &mut collector);
    Ok(collector.into_handler_deps())
}

// ─── DepCollector ────────────────────────────────────────────────────────────

/// Identifies a single top-level item in the split.
///
/// The first `usize` is the **section ordinal** — position in
/// `component.sections`, matching `VisitCtx::curr_section_idx()`.
///
/// The second `usize` is the **in-section item index** — 0-based position of
/// the item within its enclosing section's item list (not a component-level
/// space index). The byte slicer in Step 3 uses this directly to look up the
/// item's byte range via `into_iter_with_offsets()`.
type ItemLoc = (usize, usize);

/// Visitor state for the wirm structural walk. Records where each top-level
/// type / import / alias lives, what it depends on, and which one matches the
/// target import.
struct DepCollector {
    target: String,

    /// Per-section running counter, keyed by section ordinal.
    ///
    /// `in_section_count[section_idx]` holds "how many top-level items in
    /// this section we have visited so far". Every top-level visitor
    /// callback (`visit_comp_type`, `enter_component_type_inst`,
    /// `visit_comp_import`, `visit_alias`, …) calls
    /// `alloc_loc(section_idx)`, which returns the current value as the
    /// item's in-section index and then bumps the counter. This is what
    /// turns "Nth callback firing inside this section" into the
    /// in-section item index that Step 3 needs.
    ///
    /// Sections we don't track items from (Module, Canon, CoreInstance, etc.)
    /// never get an entry here, which is fine because they're skipped
    /// wholesale in Step 3.
    in_section_count: HashMap<usize, usize>,

    /// component-level type-space idx → location of the item that defined it.
    /// Populated by component-type sections, type-producing imports, and
    /// type-producing aliases.
    type_to_loc: HashMap<u32, ItemLoc>,
    /// Same idea for the component instance space.
    instance_to_loc: HashMap<u32, ItemLoc>,

    /// Forward dependency graph keyed by location. After the walk we BFS this
    /// to compute the closure starting from the target import.
    deps: HashMap<ItemLoc, BTreeSet<ItemLoc>>,

    /// Location of the import whose name matches `target`, if found.
    target_loc: Option<ItemLoc>,

    /// Stack of currently-entering top-level component-type locations. Inner
    /// `visit_inst_type_decl` / `visit_comp_type_decl` callbacks attribute the
    /// references they discover (alias-outer-type refs in particular) back to
    /// the top of this stack.
    type_body_stack: Vec<ItemLoc>,

    /// How many enclosing instance/component type bodies we are currently
    /// inside. Only the outermost (depth 0) item counts as a top-level
    /// section item; nested types are private to their parent and skipped.
    nesting_depth: usize,
}

/// What kind of name space (if any) a wirm `ItemKind` contributes to that we
/// care about for this filter.
#[derive(Clone, Copy)]
enum ItemSpace {
    Type,
    Instance,
    Other,
}

fn item_kind_to_space(kind: ItemKind) -> ItemSpace {
    match kind {
        ItemKind::CompType => ItemSpace::Type,
        ItemKind::CompInst => ItemSpace::Instance,
        _ => ItemSpace::Other,
    }
}

impl DepCollector {
    fn new(target: &str) -> Self {
        Self {
            target: target.to_string(),
            in_section_count: HashMap::new(),
            type_to_loc: HashMap::new(),
            instance_to_loc: HashMap::new(),
            deps: HashMap::new(),
            target_loc: None,
            type_body_stack: Vec::new(),
            nesting_depth: 0,
        }
    }

    /// Consume the next in-section index for `section_idx`.
    fn alloc_loc(&mut self, section_idx: usize) -> ItemLoc {
        let entry = self.in_section_count.entry(section_idx).or_insert(0);
        let in_idx = *entry;
        *entry += 1;
        (section_idx, in_idx)
    }

    /// Record a freshly-encountered top-level item: allocate its location,
    /// stash the type/instance-space lookup so future ref resolutions can find
    /// it, and return the location for the caller to use as the dep-graph key.
    /// Returns `None` if we're not inside a section (root component event).
    fn record_top_level(&mut self, cx: &VisitCtx, space: ItemSpace, id: u32) -> Option<ItemLoc> {
        let section_idx = cx.curr_section_idx()?;
        let loc = self.alloc_loc(section_idx);
        match space {
            ItemSpace::Type => {
                self.type_to_loc.insert(id, loc);
            }
            ItemSpace::Instance => {
                self.instance_to_loc.insert(id, loc);
            }
            ItemSpace::Other => {}
        }
        Some(loc)
    }

    /// Walk an item's `referenced_indices()` and record any resolvable deps
    /// against `owner` in the graph.
    fn add_refs<T: ReferencedIndices>(&mut self, cx: &VisitCtx, owner: ItemLoc, item: &T) {
        let resolved_locs: BTreeSet<ItemLoc> = item
            .referenced_indices()
            .iter()
            .filter_map(|r| {
                let resolved = cx.resolve(&r.ref_);
                self.lookup_loc(resolved.space(), resolved.idx())
            })
            .collect();
        if !resolved_locs.is_empty() {
            self.deps.entry(owner).or_default().extend(resolved_locs);
        }
    }

    /// Look up a recorded item location by `(space, idx)`. Returns `None`
    /// for spaces we don't track (functions, modules, etc.) — those never
    /// participate in the type/instance closure that the handler import
    /// drags along.
    fn lookup_loc(&self, space: Space, idx: u32) -> Option<ItemLoc> {
        match space {
            Space::CompType => self.type_to_loc.get(&idx).copied(),
            Space::CompInst => self.instance_to_loc.get(&idx).copied(),
            _ => None,
        }
    }

    /// BFS from the target import's location and bucket the resulting set of
    /// needed locations by section.
    fn into_handler_deps(self) -> HandlerDeps {
        let Some(start) = self.target_loc else {
            // Target import not found in this split — caller decides how to
            // handle the empty result.
            return HandlerDeps::default();
        };

        let mut needed: BTreeSet<ItemLoc> = BTreeSet::new();
        let mut queue: VecDeque<ItemLoc> = VecDeque::new();
        needed.insert(start);
        queue.push_back(start);

        while let Some(item) = queue.pop_front() {
            if let Some(adj) = self.deps.get(&item) {
                for dep in adj {
                    if needed.insert(*dep) {
                        queue.push_back(*dep);
                    }
                }
            }
        }

        let mut by_section: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
        for (sec, item) in needed {
            by_section.entry(sec).or_default().insert(item);
        }
        HandlerDeps { needed: by_section }
    }
}

impl<'a> ComponentVisitor<'a> for DepCollector {
    // ─── leaf component types (Defined / Func / Resource) ────────────────

    fn visit_comp_type(&mut self, cx: &VisitCtx<'a>, id: u32, item: &ComponentType<'a>) {
        // Only record this item if it's a top-level type in a section. Leaf
        // types nested inside an instance/component type body are private to
        // their parent and don't get their own slot in our graph.
        if self.nesting_depth > 0 {
            return;
        }
        let Some(loc) = self.record_top_level(cx, ItemSpace::Type, id) else {
            return;
        };
        // For Defined types, `referenced_indices()` returns the field/case
        // type refs at component scope — those are the deps we want.
        self.add_refs(cx, loc, item);
    }

    // ─── instance / component type bodies ────────────────────────────────

    fn enter_component_type_inst(&mut self, cx: &VisitCtx<'a>, id: u32, _ty: &ComponentType<'a>) {
        // Top-level instance types only contribute deps via cross-scope refs
        // inside their body — those arrive through `visit_inst_type_decl`
        // (alias outer with depth ≥ 1). Calling `referenced_indices` on the
        // parent here would only surface body-local Export refs at depth 0,
        // which collide with component-scope IDs in `type_to_loc` and
        // produce false positives. So we record the loc and rely on the
        // decl callbacks to pick up real cross-scope deps.
        if self.nesting_depth == 0 {
            if let Some(loc) = self.record_top_level(cx, ItemSpace::Type, id) {
                self.type_body_stack.push(loc);
            }
        }
        self.nesting_depth += 1;
    }

    fn exit_component_type_inst(&mut self, _: &VisitCtx<'a>, _: u32, _: &ComponentType<'a>) {
        self.nesting_depth -= 1;
        if self.nesting_depth == 0 {
            self.type_body_stack.pop();
        }
    }

    fn enter_component_type_comp(&mut self, cx: &VisitCtx<'a>, id: u32, _ty: &ComponentType<'a>) {
        if self.nesting_depth == 0 {
            if let Some(loc) = self.record_top_level(cx, ItemSpace::Type, id) {
                self.type_body_stack.push(loc);
            }
        }
        self.nesting_depth += 1;
    }

    fn exit_component_type_comp(&mut self, _: &VisitCtx<'a>, _: u32, _: &ComponentType<'a>) {
        self.nesting_depth -= 1;
        if self.nesting_depth == 0 {
            self.type_body_stack.pop();
        }
    }

    fn visit_inst_type_decl(
        &mut self,
        cx: &VisitCtx<'a>,
        _decl_idx: usize,
        _id: u32,
        _parent: &ComponentType<'a>,
        decl: &InstanceTypeDeclaration<'a>,
    ) {
        // We're inside an instance type body. The only deps we care about
        // here are cross-scope ones — alias-outer-type refs that point at
        // items in the enclosing component scope. Body-local refs (depth=0)
        // are private to the parent type and would collide with our
        // component-scope `type_to_loc` keys.
        let Some(&top_loc) = self.type_body_stack.last() else {
            return;
        };
        let new_deps: BTreeSet<ItemLoc> = decl
            .referenced_indices()
            .iter()
            .filter(|r| !r.ref_.depth.is_curr())
            .filter_map(|r| {
                let resolved = cx.resolve(&r.ref_);
                self.lookup_loc(resolved.space(), resolved.idx())
            })
            .collect();
        if !new_deps.is_empty() {
            self.deps.entry(top_loc).or_default().extend(new_deps);
        }
    }

    fn visit_comp_type_decl(
        &mut self,
        cx: &VisitCtx<'a>,
        _decl_idx: usize,
        _id: u32,
        _parent: &ComponentType<'a>,
        decl: &ComponentTypeDeclaration<'a>,
    ) {
        let Some(&top_loc) = self.type_body_stack.last() else {
            return;
        };
        let new_deps: BTreeSet<ItemLoc> = decl
            .referenced_indices()
            .iter()
            .filter(|r| !r.ref_.depth.is_curr())
            .filter_map(|r| {
                let resolved = cx.resolve(&r.ref_);
                self.lookup_loc(resolved.space(), resolved.idx())
            })
            .collect();
        if !new_deps.is_empty() {
            self.deps.entry(top_loc).or_default().extend(new_deps);
        }
    }

    // ─── top-level imports / aliases ─────────────────────────────────────

    fn visit_comp_import(
        &mut self,
        cx: &VisitCtx<'a>,
        kind: ItemKind,
        id: u32,
        import: &ComponentImport<'a>,
    ) {
        let Some(loc) = self.record_top_level(cx, item_kind_to_space(kind), id) else {
            return;
        };
        if import.name.0 == self.target {
            self.target_loc = Some(loc);
        }
        self.add_refs(cx, loc, import);
    }

    fn visit_alias(
        &mut self,
        cx: &VisitCtx<'a>,
        kind: ItemKind,
        id: u32,
        alias: &ComponentAlias<'a>,
    ) {
        let Some(loc) = self.record_top_level(cx, item_kind_to_space(kind), id) else {
            return;
        };
        self.add_refs(cx, loc, alias);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convert WAT source to component bytes, panicking on failure (test only).
    fn wat(src: &str) -> Vec<u8> {
        wat::parse_str(src).expect("invalid WAT in test fixture")
    }

    /// Minimal fan-in fixture: a single component imports three unrelated
    /// instance interfaces, each backed by its own anonymous func type. The
    /// section layout (alternating type / import sections, one item each) is
    /// exactly what `wac compose` emits for these compositions.
    ///
    /// Section ordinals as encoded by `wat`:
    ///   0: type   (adder instance type)
    ///   1: import (my:service/adder)
    ///   2: type   (messenger instance type)
    ///   3: import (my:service/messenger)
    ///   4: type   (printer instance type)
    ///   5: import (my:service/printer)
    fn simple_fanin() -> Vec<u8> {
        wat(r#"
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
        "#)
    }

    /// Helper: closure for a single target import in `simple_fanin`.
    /// The closure for any target in this fixture is exactly the
    /// pair `(its type, its import)` — we ask the layout for both
    /// rather than hardcoding section ordinals.
    fn simple_fanin_closure_for(layout: &BinaryLayout, target: &str) -> Vec<(usize, usize)> {
        let import_loc = layout
            .import_loc(target)
            .unwrap_or_else(|| panic!("import {target} in layout"));
        // Each interface's type section sits immediately before its
        // import section in source order. The closure includes the
        // type at the same position-within-types as the target
        // import is within imports.
        let type_locs = layout.type_locs();
        let imports_in_order: Vec<&str> = layout
            .sections
            .iter()
            .filter(|s| s.kind == BinarySectionKind::Import)
            .flat_map(|s| s.items.iter())
            .map(|i| i.name.as_deref().unwrap_or(""))
            .collect();
        let target_pos = imports_in_order
            .iter()
            .position(|&n| n == target)
            .expect("target import in import-order list");
        vec![type_locs[target_pos], import_loc]
    }

    /// Targeting `my:service/adder` against the simple fan-in fixture
    /// should keep exactly the adder type section and the adder
    /// import section, and drop everything else.
    #[test]
    fn simple_fanin_keeps_only_target_interface() {
        let bytes = simple_fanin();
        let layout = BinaryLayout::from_bytes(&bytes);
        let expected = simple_fanin_closure_for(&layout, "my:service/adder");
        let deps = find_handler_deps_in_bytes(&bytes, "my:service/adder").expect("dep walk");
        assert_deps_match(&deps, &expected);
    }

    /// Targeting the middle import in the fan-in. Confirms we don't
    /// accidentally pick up earlier or later sections via stale state.
    #[test]
    fn simple_fanin_middle_target() {
        let bytes = simple_fanin();
        let layout = BinaryLayout::from_bytes(&bytes);
        let expected = simple_fanin_closure_for(&layout, "my:service/messenger");
        let deps = find_handler_deps_in_bytes(&bytes, "my:service/messenger").expect("dep walk");
        assert_deps_match(&deps, &expected);
    }

    /// Targeting an interface that doesn't exist in the split should yield an
    /// empty [`HandlerDeps`] so the caller can fall back to the legacy path.
    #[test]
    fn missing_target_yields_empty_deps() {
        let bytes = simple_fanin();
        let deps = find_handler_deps_in_bytes(&bytes, "no:such/iface").expect("dep walk");
        assert!(deps.is_empty());
    }

    // ─── multi-item alias section coverage ───────────────────────────────
    //
    // The fixtures below all share the same shape: a single types-instance
    // exporting `num_aliases` resources, a single alias section that brings
    // every resource into component scope, and a handler whose instance type
    // body uses `alias outer` to reference exactly the subset given to the
    // helper. With wirm's `add_to_sections` collapsing consecutive
    // same-kind items, the alias section ends up as one logical section
    // with `num_aliases` items — exactly the dirty/clean classification
    // surface Step 3's byte slicer needs.

    use super::super::test_helpers::{
        alias_section_expected_locs, alias_section_fixture, assert_deps_match, assert_layout_kinds,
        BinaryLayout, BinarySectionKind,
    };

    /// Run the dep walker against an [`alias_section_fixture`] and
    /// assert the closure matches the layout-derived expected shape
    /// for the given `handler_uses` subset.
    fn run_alias_section_test(num_aliases: usize, handler_uses: &[usize]) {
        let bytes = alias_section_fixture(num_aliases, handler_uses);
        let layout = BinaryLayout::from_bytes(&bytes);
        let expected = alias_section_expected_locs(&layout, handler_uses);
        let deps = find_handler_deps_in_bytes(&bytes, "wasi:http/handler").expect("dep walk");
        assert_deps_match(&deps, &expected);
    }

    #[test]
    fn alias_section_all_clean() {
        run_alias_section_test(4, &[0, 1, 2, 3]);
    }

    #[test]
    fn alias_section_dirty_scrub_first() {
        run_alias_section_test(4, &[1, 2, 3]);
    }

    #[test]
    fn alias_section_dirty_scrub_middle() {
        run_alias_section_test(4, &[0, 3]);
    }

    #[test]
    fn alias_section_dirty_scrub_last() {
        run_alias_section_test(4, &[0, 1, 2]);
    }

    #[test]
    fn alias_section_dirty_single_needed_among_many() {
        run_alias_section_test(5, &[2]);
    }

    #[test]
    fn alias_section_dirty_all_but_one() {
        run_alias_section_test(5, &[0, 1, 2, 3]);
    }

    /// Two alias groups separated by a record-type section: the
    /// first alias group is fully clean (both items needed), the
    /// record section is unrelated and gets skipped entirely, and
    /// the second alias group is dirty (only its first item is
    /// needed). The expected closure is derived from the binary
    /// layout — we ask wasmparser where each alias and import
    /// actually sits, then build the expected loc set from those
    /// queries. Hardcoded section ordinals would mask any
    /// future drift.
    #[test]
    fn mixed_clean_dirty_and_skip_sections() {
        let bytes = wat(r#"
            (component
              (type (instance
                (export "r0" (type (sub resource)))
                (export "r1" (type (sub resource)))
                (export "r2" (type (sub resource)))
                (export "r3" (type (sub resource)))))
              (import "wasi:http/types" (instance (type 0)))

              ;; alias group A
              (alias export 0 "r0" (type))
              (alias export 0 "r1" (type))

              ;; record type (separator — not in the closure)
              (type (record (field "x" u32)))

              ;; alias group B
              (alias export 0 "r2" (type))
              (alias export 0 "r3" (type))

              ;; handler instance type uses r0, r1, r2 — drops r3
              (type (instance
                (alias outer 1 1 (type))
                (alias outer 1 2 (type))
                (alias outer 1 4 (type))
                (export "e0" (type (eq 0)))
                (export "e1" (type (eq 1)))
                (export "e2" (type (eq 2)))))
              (import "wasi:http/handler" (instance (type 6))))
        "#);

        let layout = BinaryLayout::from_bytes(&bytes);
        let deps = find_handler_deps_in_bytes(&bytes, "wasi:http/handler").expect("dep walk");

        // Build the expected closure by name, asking the layout
        // for each item's actual loc:
        //   - both imports (types instance + handler)
        //   - r0, r1, r2 aliases (r3 is dropped)
        //   - both top-level instance types (types-instance and
        //     handler-instance — the record-type section is the
        //     ONLY type item NOT in the closure)
        let r0 = layout.alias_loc("r0").expect("r0 in layout");
        let r1 = layout.alias_loc("r1").expect("r1 in layout");
        let r2 = layout.alias_loc("r2").expect("r2 in layout");
        let r3 = layout.alias_loc("r3").expect("r3 in layout");
        let types_import = layout
            .import_loc("wasi:http/types")
            .expect("types import in layout");
        let handler_import = layout
            .import_loc("wasi:http/handler")
            .expect("handler import in layout");

        // For the types: we need every type EXCEPT the standalone
        // record (which has no name we can query). Use type_locs()
        // and check that the closure matches all type locs minus
        // the unrelated record.
        let all_type_locs = layout.type_locs();

        // The unrelated record sits between the two alias groups
        // structurally, but we don't want to hardcode its position.
        // Instead, we EXPECT the closure to contain all type locs
        // EXCEPT exactly one — and we verify the exact "minus one"
        // by checking the dep walker's output directly.
        let mut expected: Vec<(usize, usize)> = vec![r0, r1, r2, types_import, handler_import];
        // Add the two top-level instance types from the layout.
        // The record type is the third type by source order, so
        // it's at index 1 in `type_locs` (between the types-inst
        // type at 0 and the handler-inst type at 2). We assert
        // that explicitly via the closure size check below.
        expected.push(all_type_locs[0]); // types-instance type
        expected.push(all_type_locs[2]); // handler-instance type
        assert_deps_match(&deps, &expected);

        // And r3 must NOT be in the closure (the dropped item).
        assert!(
            !deps.needed.get(&r3.0).is_some_and(|s| s.contains(&r3.1)),
            "r3 should be dropped, but it's in {:?}",
            deps.needed
        );
        // The standalone record type loc must NOT be in the closure.
        let record_loc = all_type_locs[1];
        assert!(
            !deps
                .needed
                .get(&record_loc.0)
                .is_some_and(|s| s.contains(&record_loc.1)),
            "standalone record type at {:?} should be dropped, got {:?}",
            record_loc,
            deps.needed
        );
    }

    /// Fan-in fixture that exercises the alias-outer chain — the case the
    /// fan-in service splits actually hit. A `wasi:http/types`-style instance
    /// is imported up front, then resource handles + a value type are aliased
    /// out of it, and the handler interface uses those aliased types via
    /// `alias outer 1 N (type)` references inside its instance type body.
    ///
    /// The test confirms that targeting `wasi:http/handler` follows the
    /// alias-outer chain back through the alias section and into the
    /// types-instance import, while leaving the unrelated `messenger` import
    /// behind.
    fn fanin_with_alias_outer() -> Vec<u8> {
        wat(r#"
            (component
              ;; section 0: types instance type, exporting two resources + a value type
              (type (instance
                (export "request"  (type (sub resource)))
                (export "response" (type (sub resource)))
                (type (record (field "code" u16)))
                (export "error-info" (type (eq 2)))))

              ;; section 1: import the types instance
              (import "wasi:http/types" (instance (type 0)))

              ;; section 2: alias resource type exports out into component scope
              (alias export 0 "request"    (type))
              (alias export 0 "response"   (type))
              (alias export 0 "error-info" (type))

              ;; section 3: handler instance type that references the aliased
              ;; types via alias outer (depth=1 = component scope). Body-local
              ;; type space, in declaration order:
              ;;   0: request resource (alias outer to component type 1)
              ;;   1: response resource (alias outer to component type 2)
              ;;   2: error-info record (alias outer to component type 3)
              ;;   3: own<request>
              ;;   4: own<response>
              ;;   5: result<own<response>, error-info>
              ;;   6: handle func type
              (type (instance
                (alias outer 1 1 (type))
                (alias outer 1 2 (type))
                (alias outer 1 3 (type))
                (type (own 0))
                (type (own 1))
                (type (result 4 (error 2)))
                (type (func async (param "req" 3) (result 5)))
                (export "handle" (func (type 6)))))

              ;; section 4: handler import — the target. References component
              ;; type 4 (the handler instance type defined just above).
              (import "wasi:http/handler" (instance (type 4)))

              ;; section 5: an unrelated fan-in interface that should be dropped.
              ;; This is component type 5.
              (type (instance
                (type (func (result string)))
                (export "get-msg" (func (type 0)))))
              (import "my:service/messenger" (instance (type 5)))
            )
        "#)
    }

    #[test]
    fn alias_outer_fanin_follows_chain_to_types_instance() {
        let bytes = fanin_with_alias_outer();
        let layout = BinaryLayout::from_bytes(&bytes);
        let deps = find_handler_deps_in_bytes(&bytes, "wasi:http/handler").expect("dep walk");

        // The expected closure pulls in everything reachable from
        // the handler import via the alias-outer chain back to the
        // wasi:http/types instance, but leaves the unrelated
        // messenger interface alone.
        //
        // We ask the layout for every named item by name, and grab
        // both the types-instance and handler-instance types
        // positionally — those are the FIRST and SECOND of the
        // three top-level instance types in the fixture. The
        // messenger instance type is the third top-level type and
        // must be excluded; we assert that separately.
        let request = layout.alias_loc("request").expect("request alias");
        let response = layout.alias_loc("response").expect("response alias");
        let error_info = layout.alias_loc("error-info").expect("error-info alias");
        let types_import = layout.import_loc("wasi:http/types").expect("types import");
        let handler_import = layout
            .import_loc("wasi:http/handler")
            .expect("handler import");

        let all_type_locs = layout.type_locs();
        let types_instance_type = all_type_locs[0];
        let handler_instance_type = all_type_locs[1];
        let messenger_instance_type = all_type_locs[2];

        let expected = vec![
            types_instance_type,
            types_import,
            request,
            response,
            error_info,
            handler_instance_type,
            handler_import,
        ];
        assert_deps_match(&deps, &expected);

        // The unrelated messenger pieces must be excluded — verify
        // by name and position.
        assert!(
            layout.import_loc("my:service/messenger").is_some(),
            "fixture should contain the messenger import"
        );
        let messenger_import = layout
            .import_loc("my:service/messenger")
            .expect("messenger import");
        assert!(
            !deps
                .needed
                .get(&messenger_import.0)
                .is_some_and(|s| s.contains(&messenger_import.1)),
            "messenger import should be dropped"
        );
        assert!(
            !deps
                .needed
                .get(&messenger_instance_type.0)
                .is_some_and(|s| s.contains(&messenger_instance_type.1)),
            "messenger instance type should be dropped"
        );
    }

    /// Top-level **defined types** with field refs to other top-level
    /// types exercise the `visit_comp_type` leaf handler. The handler
    /// calls `referenced_indices()`, which walks the compound type
    /// structure (record, variant, list, option, …) and returns refs
    /// to embedded type indices. The dep walker should follow those
    /// refs transitively into the closure.
    ///
    /// Verified by closure-shape assertions only — the reassembled
    /// component model bytes wouldn't necessarily validate (the
    /// component model has strict rules about which compound shapes
    /// are allowed at the top level of imports), but the dep walker
    /// is what we're testing, not the import-validity rules.
    #[test]
    fn defined_type_field_refs_follow_into_closure() {
        // Component-scope types:
        //   0: record { x: string }   ← leaf defined type
        //   1: record { y: 0 }        ← compound; field "y" references type 0
        //   2: instance type { alias outer 1 1 (type); export "thing" (eq 0) }
        // Handler import references type 2.
        //
        // Closure walk: handler import → type 2 → type 1 (via alias
        // outer in body) → type 0 (via field "y" — this is the link
        // that visit_comp_type's add_refs path is responsible for).
        // If add_refs doesn't follow the field ref, type 0 is
        // dropped from the closure.
        let bytes = wat(r#"
            (component
              (type (record (field "x" string)))
              (type (record (field "y" 0)))
              (type (instance
                (alias outer 1 1 (type))
                (export "thing" (type (eq 0)))))
              (import "my:foo/handler" (instance (type 2))))
        "#);

        // Independently parse with wasmparser. The fixture defines
        // 3 top-level types and 1 import — but we don't hardcode
        // section ordinals; we ask the layout where they actually
        // live.
        let layout = BinaryLayout::from_bytes(&bytes);
        let all_type_locs = layout.type_locs();
        assert_eq!(
            all_type_locs.len(),
            3,
            "fixture should define exactly three top-level types"
        );
        let target_loc = layout
            .import_loc("my:foo/handler")
            .expect("handler import in layout");

        let deps = find_handler_deps_in_bytes(&bytes, "my:foo/handler").expect("dep walk");

        // Expected closure: every top-level type plus the handler
        // import. The critical link is the leaf record (the first
        // type) — if the dep walker's leaf-type handler doesn't
        // follow the field ref from the second record, the leaf
        // type's loc would be missing from the closure and
        // `assert_deps_match` would surface the diff.
        let mut expected: Vec<(usize, usize)> = all_type_locs;
        expected.push(target_loc);
        assert_deps_match(&deps, &expected);
    }

    /// When two imports reference the same type, dropping the
    /// non-target one must NOT drop the shared type — the target
    /// still depends on it. Verifies that the closure walker scopes
    /// "is this type needed?" by reachability from the target, not
    /// by counting how many imports reference it.
    ///
    /// Test expectations are derived from the actual binary layout
    /// via [`BinaryLayout`] rather than from positional assumptions
    /// about the fixture, so a layout shift surfaces as a clear
    /// shape error before any closure-shape assertions run.
    #[test]
    fn shared_type_ref_preserved_when_one_consumer_dropped() {
        let bytes = wat(r#"
            (component
              (type (instance (export "fn" (func))))
              (import "my:other/thing" (instance (type 0)))
              (import "my:foo/handler" (instance (type 0))))
        "#);

        // Independently parse the fixture with wasmparser to know
        // exactly which section each item lives in.
        let layout = BinaryLayout::from_bytes(&bytes);
        assert_layout_kinds(
            &layout,
            &[BinarySectionKind::Type, BinarySectionKind::Import],
        );
        let type_locs = layout.type_locs();
        assert_eq!(type_locs.len(), 1, "fixture should define exactly one type");
        let shared_type_loc = type_locs[0];
        let target_loc = layout
            .import_loc("my:foo/handler")
            .expect("handler import in layout");

        let deps = find_handler_deps_in_bytes(&bytes, "my:foo/handler").expect("dep walk");

        // Closure: shared type (because the handler depends on it)
        // + handler import. The non-target "my:other/thing" import
        // is reachable from nothing in the closure and must be
        // dropped — `assert_deps_match` enforces this because
        // `expected_locs` doesn't include it.
        assert_deps_match(&deps, &[shared_type_loc, target_loc]);
    }
}
