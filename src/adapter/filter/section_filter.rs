//! # Scoped section filtering for adapter component generation
//!
//! ## Why this module exists
//!
//! When `generate_tier1_adapter` builds an adapter for a middleware that fronts a
//! downstream component, the adapter has to inherit the same import structure that
//! the downstream split exposes — same type instance, same alias chain, same
//! resource handles — so the wac composition step accepts it as a drop-in
//! replacement.
//!
//! ## Chain vs fan-in (the shape that matters)
//!
//! splicer wraps middleware on top of one or more downstream services. A
//! **chain** config has each service consuming exactly one upstream interface,
//! so the split that the middleware fronts only imports the interfaces needed
//! by that single handler:
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
use wirm::ir::component::visitor::{
    walk_structural, ComponentVisitor, ItemKind, VisitCtx,
};
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

/// Walk a downstream split with wirm and compute the set of section items
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
        .with_context(|| format!("Failed to read downstream split at '{split_path}'"))?;
    find_handler_deps_in_bytes(&bytes, target_interface)
}

/// Same as [`find_handler_deps`] but works on already-loaded bytes. Useful for
/// tests that build component fixtures inline via `wat::parse_str`.
pub(crate) fn find_handler_deps_in_bytes(
    bytes: &[u8],
    target_interface: &str,
) -> anyhow::Result<HandlerDeps> {
    let component = wirm::Component::parse(bytes, false, false)
        .context("Failed to parse downstream split bytes")?;
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
    fn record_top_level(
        &mut self,
        cx: &VisitCtx,
        space: ItemSpace,
        id: u32,
    ) -> Option<ItemLoc> {
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

    fn visit_comp_type(
        &mut self,
        cx: &VisitCtx<'a>,
        id: u32,
        item: &ComponentType<'a>,
    ) {
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

    fn enter_component_type_inst(
        &mut self,
        cx: &VisitCtx<'a>,
        id: u32,
        _ty: &ComponentType<'a>,
    ) {
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

    fn exit_component_type_inst(
        &mut self,
        _: &VisitCtx<'a>,
        _: u32,
        _: &ComponentType<'a>,
    ) {
        self.nesting_depth -= 1;
        if self.nesting_depth == 0 {
            self.type_body_stack.pop();
        }
    }

    fn enter_component_type_comp(
        &mut self,
        cx: &VisitCtx<'a>,
        id: u32,
        _ty: &ComponentType<'a>,
    ) {
        if self.nesting_depth == 0 {
            if let Some(loc) = self.record_top_level(cx, ItemSpace::Type, id) {
                self.type_body_stack.push(loc);
            }
        }
        self.nesting_depth += 1;
    }

    fn exit_component_type_comp(
        &mut self,
        _: &VisitCtx<'a>,
        _: u32,
        _: &ComponentType<'a>,
    ) {
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
        eprintln!(
            "[depcoll] import name={:?} kind={:?} id={} loc={:?} target={:?}",
            import.name.0, kind, id, loc, self.target
        );
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
        eprintln!(
            "[depcoll] alias kind={:?} id={} loc={:?} alias={:?}",
            kind, id, loc, alias
        );
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

    /// Targeting `my:service/adder` against the simple fan-in fixture should
    /// keep exactly the adder type section and the adder import section, and
    /// drop everything else.
    #[test]
    fn simple_fanin_keeps_only_target_interface() {
        let bytes = simple_fanin();
        let deps =
            find_handler_deps_in_bytes(&bytes, "my:service/adder").expect("dep walk");

        assert!(!deps.is_empty(), "should have found the adder import");
        assert_eq!(deps.needed.get(&0), Some(&BTreeSet::from([0])));
        assert_eq!(deps.needed.get(&1), Some(&BTreeSet::from([0])));
        assert_eq!(
            deps.needed.len(),
            2,
            "exactly 2 sections expected; got {:?}",
            deps.needed
        );
    }

    /// Targeting the middle import in the fan-in: should pick out section 2
    /// (its type) and section 3 (its import), nothing else. Confirms we don't
    /// accidentally pick up earlier or later sections via stale state.
    #[test]
    fn simple_fanin_middle_target() {
        let bytes = simple_fanin();
        let deps =
            find_handler_deps_in_bytes(&bytes, "my:service/messenger").expect("dep walk");

        assert_eq!(deps.needed.get(&2), Some(&BTreeSet::from([0])));
        assert_eq!(deps.needed.get(&3), Some(&BTreeSet::from([0])));
        assert_eq!(deps.needed.len(), 2, "got {:?}", deps.needed);
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

    /// Build a fixture where the handler imports references the resource
    /// aliases at the given positions in `[0, num_aliases)`. Section layout
    /// (with the wirm `add_to_sections` collapse fix in place):
    ///
    /// ```text
    /// 0: ComponentType   (types instance type, num_aliases resource exports)
    /// 1: ComponentImport (wasi:http/types)
    /// 2: Alias           (num_aliases items)
    /// 3: ComponentType   (handler instance type)
    /// 4: ComponentImport (wasi:http/handler)
    /// ```
    fn alias_section_fixture(num_aliases: usize, handler_uses: &[usize]) -> Vec<u8> {
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

    /// Pull just the dep set for the alias section (section ordinal 2 in the
    /// `alias_section_fixture` shape).
    fn alias_section_needed(deps: &HandlerDeps) -> &BTreeSet<usize> {
        deps.needed
            .get(&2)
            .expect("alias section ordinal 2 missing from HandlerDeps")
    }

    #[test]
    fn alias_section_all_clean() {
        // 4 aliases, handler uses every one — section is fully clean.
        let bytes = alias_section_fixture(4, &[0, 1, 2, 3]);
        let deps =
            find_handler_deps_in_bytes(&bytes, "wasi:http/handler").expect("dep walk");
        assert_eq!(alias_section_needed(&deps), &BTreeSet::from([0, 1, 2, 3]));
    }

    #[test]
    fn alias_section_dirty_scrub_first() {
        // Drop item 0, keep items 1, 2, 3.
        let bytes = alias_section_fixture(4, &[1, 2, 3]);
        let deps =
            find_handler_deps_in_bytes(&bytes, "wasi:http/handler").expect("dep walk");
        assert_eq!(alias_section_needed(&deps), &BTreeSet::from([1, 2, 3]));
    }

    #[test]
    fn alias_section_dirty_scrub_middle() {
        // Keep first and last, drop the two middle items.
        let bytes = alias_section_fixture(4, &[0, 3]);
        let deps =
            find_handler_deps_in_bytes(&bytes, "wasi:http/handler").expect("dep walk");
        assert_eq!(alias_section_needed(&deps), &BTreeSet::from([0, 3]));
    }

    #[test]
    fn alias_section_dirty_scrub_last() {
        // Keep items 0, 1, 2; drop the trailing item.
        let bytes = alias_section_fixture(4, &[0, 1, 2]);
        let deps =
            find_handler_deps_in_bytes(&bytes, "wasi:http/handler").expect("dep walk");
        assert_eq!(alias_section_needed(&deps), &BTreeSet::from([0, 1, 2]));
    }

    #[test]
    fn alias_section_dirty_single_needed_among_many() {
        // 5 items, only the middle one is referenced.
        let bytes = alias_section_fixture(5, &[2]);
        let deps =
            find_handler_deps_in_bytes(&bytes, "wasi:http/handler").expect("dep walk");
        assert_eq!(alias_section_needed(&deps), &BTreeSet::from([2]));
    }

    #[test]
    fn alias_section_dirty_all_but_one() {
        // 5 items, only the last is dropped.
        let bytes = alias_section_fixture(5, &[0, 1, 2, 3]);
        let deps =
            find_handler_deps_in_bytes(&bytes, "wasi:http/handler").expect("dep walk");
        assert_eq!(alias_section_needed(&deps), &BTreeSet::from([0, 1, 2, 3]));
    }

    /// Two alias sections separated by a record-type section: the first
    /// alias section is fully clean, the record section is unrelated and
    /// gets skipped entirely, and the second alias section is dirty
    /// (only its first item is needed). This is the multi-section
    /// classification matrix in a single fixture.
    #[test]
    fn mixed_clean_dirty_and_skip_sections() {
        let bytes = wat(r#"
            (component
              ;; section 0: types instance with 4 resources (component type 0)
              (type (instance
                (export "r0" (type (sub resource)))
                (export "r1" (type (sub resource)))
                (export "r2" (type (sub resource)))
                (export "r3" (type (sub resource)))))

              ;; section 1: types-instance import
              (import "wasi:http/types" (instance (type 0)))

              ;; section 2: alias section A — both items needed (CLEAN)
              (alias export 0 "r0" (type))    ;; component type 1
              (alias export 0 "r1" (type))    ;; component type 2

              ;; section 3: defined record type (separator, unrelated to handler)
              (type (record (field "x" u32)))  ;; component type 3

              ;; section 4: alias section B — only the first item needed (DIRTY)
              (alias export 0 "r2" (type))    ;; component type 4
              (alias export 0 "r3" (type))    ;; component type 5

              ;; section 5: handler instance type — uses r0, r1, r2 (drops r3)
              (type (instance
                (alias outer 1 1 (type))      ;; r0  → body type 0
                (alias outer 1 2 (type))      ;; r1  → body type 1
                (alias outer 1 4 (type))      ;; r2  → body type 2 (skip record)
                (export "e0" (type (eq 0)))
                (export "e1" (type (eq 1)))
                (export "e2" (type (eq 2)))))

              ;; section 6: handler import (component type 6 above)
              (import "wasi:http/handler" (instance (type 6))))
        "#);

        let deps =
            find_handler_deps_in_bytes(&bytes, "wasi:http/handler").expect("dep walk");

        // section 2 — clean, both alias items needed
        assert_eq!(
            deps.needed.get(&2),
            Some(&BTreeSet::from([0, 1])),
            "section 2 should be clean (both alias items needed); got {:?}",
            deps.needed
        );

        // section 3 — record type, NOT referenced anywhere → skipped entirely
        assert!(
            !deps.needed.contains_key(&3),
            "record-type section 3 should be skipped; got {:?}",
            deps.needed
        );

        // section 4 — dirty, only the first alias item (r2) is needed
        assert_eq!(
            deps.needed.get(&4),
            Some(&BTreeSet::from([0])),
            "section 4 should be dirty (only item 0 needed); got {:?}",
            deps.needed
        );

        // sections 0 / 1 / 5 / 6 — handler closure tail, all needed
        assert!(deps.needed.contains_key(&0), "types-instance type missing");
        assert!(deps.needed.contains_key(&1), "types-instance import missing");
        assert!(deps.needed.contains_key(&5), "handler type missing");
        assert!(deps.needed.contains_key(&6), "handler import missing");
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
        let deps =
            find_handler_deps_in_bytes(&bytes, "wasi:http/handler").expect("dep walk");

        assert!(!deps.is_empty(), "handler import should have been found");

        // Section 0 (types instance type) — needed (transitively, via the
        // alias chain back to the types import).
        assert!(deps.needed.contains_key(&0), "missing types-instance type section: {:?}", deps.needed);
        // Section 1 (types instance import) — needed (the alias section
        // resolves through it).
        assert!(deps.needed.contains_key(&1), "missing types-instance import section: {:?}", deps.needed);
        // Section 2 (the three aliases) — needed.
        assert!(deps.needed.contains_key(&2), "missing alias section: {:?}", deps.needed);
        // Section 3 (handler instance type) — needed.
        assert!(deps.needed.contains_key(&3), "missing handler type section: {:?}", deps.needed);
        // Section 4 (handler import) — needed.
        assert!(deps.needed.contains_key(&4), "missing handler import section: {:?}", deps.needed);

        // Section 5 / 6 (messenger type + import) — must be dropped.
        assert!(!deps.needed.contains_key(&5), "should have dropped messenger type section");
        assert!(!deps.needed.contains_key(&6), "should have dropped messenger import section");

        // The alias section should have all three of its items (request,
        // response, error-info), since the handler type body references all
        // three via alias outer.
        assert_eq!(
            deps.needed.get(&2),
            Some(&BTreeSet::from([0, 1, 2])),
            "alias section closure mismatch"
        );
    }
}
