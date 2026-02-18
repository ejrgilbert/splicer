use std::fs::File;
use serde::de::Error;
use serde::Deserialize;
use crate::model::{ComponentNode, CompositionGraph, InterfaceConnection};

pub fn parse_json(json_reader: &File) -> anyhow::Result<CompositionGraph> {
    let graph = CompositionGraph::from_json_reader(json_reader)?;
    if let Err(e) = graph.validate() {
        serde_json::Error::custom(e.to_string());
    }
    Ok(graph)
}

pub fn parse_json_str(json: &str) -> anyhow::Result<CompositionGraph> {
    let graph = CompositionGraph::from_json_str(json)?;
    if let Err(e) = graph.validate() {
        serde_json::Error::custom(e.to_string());
    }
    Ok(graph)
}

impl CompositionGraph {
    pub fn from_json_str(input: &str) -> Result<Self, serde_json::Error> {
        let model: JsonCompositionGraph = serde_json::from_str(input)?;
        Ok(Self::from_json_model(model))
    }
    pub fn from_json_reader<R: std::io::Read>(
        reader: R,
    ) -> Result<Self, serde_json::Error> {
        let model: JsonCompositionGraph = serde_json::from_reader(reader)?;
        Ok(Self::from_json_model(model))
    }
}

impl CompositionGraph {
    pub fn from_json_model(model: JsonCompositionGraph) -> Self {
        use std::collections::BTreeMap;

        let mut nodes = BTreeMap::new();

        for json_node in model.nodes {
            let mut node = ComponentNode::new(
                format!("${}", json_node.name), // restore `$` convention
                json_node.component_index,
            );

            for conn in json_node.imports {
                let connection = InterfaceConnection {
                    interface_name: conn.interface,
                    source_instance: conn.source_instance,
                    is_host_import: conn.is_host_import,
                };

                node.add_import(connection);
            }

            nodes.insert(json_node.id, node);
        }

        let mut component_exports = BTreeMap::new();
        for export in model.exports {
            component_exports.insert(export.interface, export.source_instance);
        }

        CompositionGraph {
            nodes,
            component_exports,
        }
    }
}

impl CompositionGraph {
    pub fn validate(&self) -> Result<(), String> {
        for (iface, src) in &self.component_exports {
            if !self.nodes.contains_key(src) {
                return Err(format!(
                    "Export '{}' references unknown instance {}",
                    iface, src
                ));
            }
        }

        for (id, node) in &self.nodes {
            for conn in &node.imports {
                let src = conn.source_instance;
                if !self.nodes.contains_key(&src) {
                    return Err(format!(
                        "Instance {} imports from unknown instance {}",
                        id, src
                    ));
                }
            }
        }

        Ok(())
    }
}



#[derive(Debug, Deserialize)]
pub struct JsonCompositionGraph {
    pub nodes: Vec<JsonNode>,
    pub exports: Vec<JsonExport>,
}

#[derive(Debug, Deserialize)]
pub struct JsonNode {
    pub id: u32,
    pub name: String,
    pub component_index: u32,
    pub imports: Vec<JsonInterfaceConnection>,
}

#[derive(Debug, Deserialize)]
pub struct JsonInterfaceConnection {
    pub interface: String,
    pub short: String,
    pub source_instance: u32,
    pub is_host_import: bool,
}

#[derive(Debug, Deserialize)]
pub struct JsonExport {
    pub interface: String,
    pub source_instance: u32,
}
