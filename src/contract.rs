use std::collections::HashMap;
use std::fs;
use colored::Colorize;
use cviz::model::compatible_fingerprints;
use wirm::Component;
use wirm::ir::component::refs::GetItemRef;
use wirm::ir::component::visitor::{walk_structural, ComponentVisitor, ItemKind, ResolvedItem, VisitCtx};
use wirm::wasmparser::{ComponentAlias, ComponentExport};
use crate::parse::config::Injection;

pub struct ExportContract {
    name: String,
    fingerprint: String,
}

pub fn validate_contract(
    to_inject: &[Injection],
    interface_name: &str,
    contract_fingerprint: &Option<String>,
    // caches middleware export discovery
    checked_middlewares: &mut HashMap<String, Vec<ExportContract>>
) {
    for Injection {
        name, path
    } in to_inject.iter() {
        let exports = checked_middlewares
            .entry(name.to_string())
            .or_insert(discover_middleware_exports(path));

        let mut validated = false;
        for ExportContract {
            name, fingerprint
        } in exports.iter() {
            if name == interface_name {
                if !compatible_fingerprints(contract_fingerprint, &Some(fingerprint.clone())) {
                    // TODO: make this more elegant!
                    panic!("incompatible type signatures for function {}", interface_name);
                } else {
                    validated = true;
                }
            }
        }

        if !validated {
            eprintln!("{}: {}", "WARN".yellow().bold(), format!("\t Unable to validate contract for injection on interface '{interface_name}'").yellow());
        }
    }
}

fn discover_middleware_exports(wasm_path: &Option<String>) -> Vec<ExportContract> {
    if let Some(path) = wasm_path {
        let buff = fs::read(path).unwrap(); // todo: make this more elegant (handle the error)!
        let component = Component::parse(&buff, false, false).expect("Unable to parse");
        
        let mut discovery = DiscoverExports::default();
        walk_structural(&component, &mut discovery);
        
        discovery.contracts
    } else {
        vec![]
    }
}

#[derive(Default)]
struct DiscoverExports {
    contracts: Vec<ExportContract>,
}
impl ComponentVisitor<'_> for DiscoverExports {
    fn visit_comp_export(&mut self, cx: &VisitCtx<'_>, _kind: ItemKind, _id: u32, export: &ComponentExport<'_>) {
        let export_name = export.name.0.to_string();
        let item = cx.resolve(&export.get_item_ref().ref_);

        // Only track instance exports
        match item {
            ResolvedItem::CompInst(inst_id, _) => {
                add_export(&mut self.contracts, export_name, todo!());
            }
            ResolvedItem::Alias(_, alias) => {
                resolve_imp_alias(cx, alias, &export_name, &mut self.contracts);
            }
            _ => {}
        }
    }
}
pub fn add_export(contracts: &mut Vec<ExportContract>, name: String, fingerprint: String) {
    contracts.push(ExportContract {
        name, fingerprint
    })
}
fn resolve_imp_alias(
    cx: &VisitCtx,
    alias: &ComponentAlias,
    export_name: &str,
    contracts: &mut Vec<ExportContract>,
) {
    let inst_ref = alias.get_item_ref();

    match cx.resolve(&inst_ref.ref_) {
        ResolvedItem::CompInst(inst_id, _) => {
            add_export(contracts, export_name.to_string(), todo!())
        }
        ResolvedItem::Alias(_, nested_alias) => {
            resolve_imp_alias(cx, nested_alias, export_name, contracts)
        }
        _ => {}
    }
}
