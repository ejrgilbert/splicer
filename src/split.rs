use anyhow::Context;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use wirm::ir::component::visitor::{walk_structural, ComponentVisitor, VisitCtx};
use wirm::{Component, Module};

/// Default directory where split sub-components are written.
pub const PATH_TO_SPLITS: &str = "./splits";

/// Split a composed Wasm component into its sub-components, writing
/// one `.wasm` file per nested component into the splits directory.
/// Returns `(splits_dir, shim_map)` where `shim_map` records which
/// splits are shim components that should be replaced by their outer
/// component.
pub fn split_out_composition(
    wasm_path: &PathBuf,
    splits_path: &Option<String>,
) -> anyhow::Result<(String, HashMap<usize, usize>)> {
    let output = if let Some(splits_path) = splits_path {
        splits_path.clone()
    } else {
        PATH_TO_SPLITS.to_string()
    };
    fs::create_dir_all(&output)
        .with_context(|| format!("Failed to create splits directory: {output}"))?;
    let buff = fs::read(wasm_path)
        .with_context(|| format!("Failed to read composition wasm: {}", wasm_path.display()))?;
    let component = Component::parse(&buff, false, false).with_context(|| {
        format!(
            "Failed to parse composition wasm as a component: {}",
            wasm_path.display()
        )
    })?;

    let mut visitor = EmitVisitor::new(&output);
    walk_structural(&component, &mut visitor);

    if let Some(e) = &visitor.err {
        return Err(anyhow::anyhow!("{}", e));
    }

    Ok((output, visitor.shim_comps))
}

struct EmitVisitor {
    output_path: String,
    curr_comp_num: usize,
    comp_num_stack: Vec<usize>,

    // Used to find shims
    has_core_module: Vec<bool>,
    has_child_component: Vec<bool>,
    shim_comps: HashMap<usize, usize>, // shim_comp_num -> outer_comp_num

    err: Option<wirm::error::Error>,
}
impl EmitVisitor {
    fn new(output_path: &str) -> Self {
        Self {
            output_path: output_path.to_string(),
            curr_comp_num: 0,
            comp_num_stack: vec![],
            has_core_module: vec![],
            has_child_component: vec![],
            shim_comps: HashMap::new(),
            err: None,
        }
    }
    fn handle_enter_component(&mut self, comp: &Component) {
        // Record that our parent (if any) nests at least one child.
        if let Some(parent_has_child) = self.has_child_component.last_mut() {
            *parent_has_child = true;
        }
        // we reserve 0 for the outermost component!
        // (if it's the outermost, the id is None)
        self.comp_num_stack.push(self.curr_comp_num);

        if let Err(e) = comp.emit_wasm(&gen_split_path(&self.output_path, self.curr_comp_num)) {
            self.err = Some(e);
        }
        self.curr_comp_num += 1;
        self.has_core_module.push(false);
        self.has_child_component.push(false);
    }
    fn handle_exit_component(&mut self, _: &Component) {
        self.apply_shim_identification_heuristic();
    }

    fn apply_shim_identification_heuristic(&mut self) {
        let has_core_module = self.has_core_module.pop().unwrap();
        let has_child_component = self.has_child_component.pop().unwrap();
        if let Some(my_comp_num) = self.comp_num_stack.pop() {
            // A genuine shim is a *leaf* component with no inner core module:
            // a thin adapter the outer wraps with non-WAC stitching, so we
            // instantiate the OUTER rather than the inner shim.
            //
            // A module-less component that itself nests child components is
            // NOT a shim -- it's a real subcomposition that re-exports its
            // children's interfaces (common in deeply-nested pre-composed
            // inputs). Collapsing it to its parent would merge distinct
            // providers onto one instance (and onto a split that doesn't
            // export the interface), so leave it standalone.
            if !has_core_module && !has_child_component {
                // protect against doing this for the outermost component
                if let Some(outer_comp_num) = self.comp_num_stack.last() {
                    self.shim_comps.insert(my_comp_num, *outer_comp_num);
                }
            }
        }
    }
}
impl ComponentVisitor<'_> for EmitVisitor {
    fn enter_root_component(&mut self, _cx: &VisitCtx, component: &Component) {
        self.handle_enter_component(component);
    }
    fn exit_root_component(&mut self, _cx: &VisitCtx, component: &Component) {
        self.handle_exit_component(component);
    }
    fn enter_component(&mut self, _cx: &VisitCtx, _: u32, component: &Component) {
        self.handle_enter_component(component);
    }
    fn exit_component(&mut self, _cx: &VisitCtx, _id: u32, component: &Component) {
        self.handle_exit_component(component);
    }
    fn visit_module(&mut self, _cx: &VisitCtx<'_>, _id: u32, _module: &Module<'_>) {
        *self.has_core_module.last_mut().unwrap() = true;
    }
}

/// Build the filesystem path for a split sub-component by index.
pub fn gen_split_path(splits_path: &str, comp_id: usize) -> String {
    format!("{splits_path}/split{comp_id}.wasm")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Split a WAT component from a temp file and return its shim map.
    fn shim_map_of(wat: &str) -> HashMap<usize, usize> {
        let bytes = wat::parse_str(wat).expect("valid component wat");
        let dir = std::env::temp_dir().join(format!(
            "splicer-split-test-{}-{}",
            std::process::id(),
            // cheap unique-ish suffix so parallel tests don't collide
            wat.len()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir splits");
        let wasm_path = dir.join("input.wasm");
        std::fs::write(&wasm_path, &bytes).expect("write input wasm");
        let (_out, shim_comps) =
            split_out_composition(&wasm_path, &Some(dir.to_string_lossy().into_owned()))
                .expect("split succeeds");
        shim_comps
    }

    /// The shim heuristic must collapse a module-less *leaf* component (a
    /// thin adapter) to its parent, but leave a module-less *non-leaf*
    /// subcomposition standalone — collapsing the latter merged distinct
    /// providers onto the root in deeply-nested pre-composed inputs.
    #[test]
    fn module_less_subcomposition_is_not_a_shim_but_leaf_shim_is() {
        // comp 0 = root
        //   comp 1 = wrapper: module-less, but nests comp 2 -> NOT a shim
        //     comp 2 = inner: has a core module               -> NOT a shim
        //   comp 3 = leaf: module-less, no children           -> shim -> 0
        let wat = r#"
            (component
              (component
                (component
                  (core module)
                )
              )
              (component)
            )
        "#;
        let shim = shim_map_of(wat);
        assert!(
            !shim.contains_key(&1),
            "module-less subcomposition (comp 1) must NOT be a shim; got {shim:?}"
        );
        assert!(
            !shim.contains_key(&2),
            "module-bearing inner (comp 2) must NOT be a shim; got {shim:?}"
        );
        assert_eq!(
            shim.get(&3),
            Some(&0),
            "module-less leaf (comp 3) must collapse to its parent; got {shim:?}"
        );
    }
}
