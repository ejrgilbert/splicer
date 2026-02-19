use std::fs;
use std::path::PathBuf;
use wirm::Component;
use wirm::ir::component::visitor::{traverse_component, ComponentVisitor, VisitCtx};

pub const PATH_TO_SPLITS: &str = "./splits";

pub fn split_out_composition(wasm_path: &PathBuf, splits_path: &Option<String>) -> anyhow::Result<(String, Vec<usize>)> {
    let output = if let Some(splits_path) = splits_path {
        splits_path.clone()
    } else {
        PATH_TO_SPLITS.to_string()
    };
    fs::create_dir_all(output.clone())?;
    let buff = fs::read(wasm_path)?;
    let component = Component::parse(&buff, false, false).expect("Unable to parse");

    let mut visitor = EmitVisitor::new(&output);
    traverse_component(&component, &mut visitor);

    if let Some(e) = &visitor.err {
        return Err(anyhow::anyhow!("{}", e));
    }

    Ok((output, visitor.shim_comps))
}

struct EmitVisitor {
    output_path: String,
    curr_comp_num: usize,
    nested_comps: Vec<usize>,
    shim_comps: Vec<usize>,
    err: Option<std::io::Error>,
}
impl EmitVisitor {
    fn new(output_path: &str) -> Self {
        Self {
            output_path: output_path.to_string(),
            curr_comp_num: 0,
            nested_comps: vec![],
            shim_comps: vec![],
            err: None,
        }
    }
}
impl ComponentVisitor for EmitVisitor {
    fn enter_component(&mut self, _cx: &VisitCtx, _: Option<u32>, component: &Component) {
        // we reserve 0 for the outermost component!
        // (if it's the outermost, the id is None)
        self.nested_comps.push(self.curr_comp_num);

        if let Err(e) = component.emit_wasm(&gen_split_path(&self.output_path, self.curr_comp_num)) {
            self.err = Some(e);
        }
        self.curr_comp_num += 1;
    }
    fn exit_component(&mut self, _cx: &VisitCtx, _id: Option<u32>, _component: &Component) {
        if let Some(prev_count) = self.nested_comps.pop() {
            let count = self.curr_comp_num - prev_count;
            if count == 1 {
                // I'm making an assumption here! If there are 2 splits, that means I have:
                // 1 outer component
                // 1 nested component
                // This is likely a component that contains an inner shim with some necessary non-wac
                // stitching around it! So, I would need to make sure I instantiate the OUTER component
                // rather than the inner shim.
                self.shim_comps.push(self.curr_comp_num - 1);
            }
        }
    }
}

pub fn gen_split_path(splits_path: &str, comp_id: usize) -> String {
    format!("{splits_path}/split{comp_id}.wasm")
}
