use anyhow::Context;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use wirm::ir::component::visitor::{walk_structural, ComponentVisitor, VisitCtx};
use wirm::{Component, Module};

pub const PATH_TO_SPLITS: &str = "./splits";

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
            shim_comps: HashMap::new(),
            err: None,
        }
    }
    fn handle_enter_component(&mut self, comp: &Component) {
        // we reserve 0 for the outermost component!
        // (if it's the outermost, the id is None)
        self.comp_num_stack.push(self.curr_comp_num);

        if let Err(e) = comp.emit_wasm(&gen_split_path(&self.output_path, self.curr_comp_num)) {
            self.err = Some(e);
        }
        self.curr_comp_num += 1;
        self.has_core_module.push(false);
    }
    fn handle_exit_component(&mut self, _: &Component) {
        self.apply_shim_identification_heuristic();
    }

    fn apply_shim_identification_heuristic(&mut self) {
        let has_core_module = self.has_core_module.pop().unwrap();
        if let Some(my_comp_num) = self.comp_num_stack.pop() {
            if !has_core_module {
                // protect against doing this for the outermost component
                if let Some(outer_comp_num) = self.comp_num_stack.last() {
                    // I'm making an assumption here, if the component I'm exiting does
                    // not have an inner core module, it's likely a shim!
                    // The outer component likely contains this inner shim with some necessary non-wac
                    // stitching around it! So, I would need to make sure I instantiate the OUTER component
                    // rather than the inner shim.
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

pub fn gen_split_path(splits_path: &str, comp_id: usize) -> String {
    format!("{splits_path}/split{comp_id}.wasm")
}
