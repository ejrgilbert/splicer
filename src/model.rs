use std::collections::BTreeMap;

// TODO: I should be able to depend on the JSON and ComponentGraph definitions in `cviz`!

/// Represents a component instance in the composition
#[derive(Debug, Clone)]
pub struct ComponentNode {
    /// Instance name (e.g., "$srv", "$mdl-a")
    pub name: String,
    /// Which component is being instantiated
    pub component_index: u32,
    /// List of interface connections (what it receives)
    pub imports: Vec<InterfaceConnection>,
}

impl ComponentNode {
    pub fn new(name: String, component_index: u32) -> Self {
        Self {
            name,
            component_index,
            imports: Vec::new(),
        }
    }

    pub fn add_import(&mut self, connection: InterfaceConnection) {
        self.imports.push(connection);
    }

    /// Get a display label for the node
    pub fn display_label(&self) -> &str {
        self.name.trim_start_matches('$')
    }
}

/// Represents wiring between instances
#[derive(Debug, Clone)]
pub struct InterfaceConnection {
    /// e.g., "wasi:http/handler@0.3.0-rc-2026-01-06"
    pub interface_name: String,
    /// Which instance provides this
    pub source_instance: u32,
    /// Whether this comes from the host
    pub is_host_import: bool,
}

/// The complete parsed composition structure
#[derive(Debug, Default)]
pub struct CompositionGraph {
    /// All component instances, keyed by instance index
    pub nodes: BTreeMap<u32, ComponentNode>,
    /// What the composed component exports (interface name -> source instance)
    pub component_exports: BTreeMap<String, u32>,
}
