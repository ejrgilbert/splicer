//! Human-readable pretty-printer for [`FieldTree`] values. Recurses
//! through child indices for compound cells and reads side-table
//! entries for nominal cells, producing a `(type_label, value)` pair
//! suitable for logging or composing into a higher-level renderer.
//!
//! Panics on out-of-bounds child / side-table indices as those signal
//! a splicer codegen contract violation, not a user-recoverable
//! condition.

use crate::types::{Cell, FieldTree};

/// Format the cell at `idx` as `(type_label, value)`.
pub fn cell_to_str(tree: &FieldTree, idx: u32) -> (String, String) {
    let cell = tree.cells.get(idx as usize).unwrap_or_else(|| {
        panic!(
            "cell index {idx} out of bounds (cells.len() = {})",
            tree.cells.len()
        )
    });
    match cell {
        Cell::Bool(b) => ("bool".to_string(), b.to_string()),
        Cell::Integer(i) => ("int".to_string(), i.to_string()),
        Cell::Floating(x) => ("float".to_string(), x.to_string()),
        Cell::Text(s) => ("text".to_string(), format!("{s:?}")),
        Cell::Bytes(b) => ("bytes".to_string(), format!("[{}B]", b.len())),

        Cell::ListOf(children) => {
            let parts = render_children(tree, children);
            ("list".to_string(), format!("[{}]", parts.join(", ")))
        }
        Cell::TupleOf(children) => {
            let parts = render_children(tree, children);
            ("tuple".to_string(), format!("({})", parts.join(", ")))
        }
        Cell::OptionSome(child) => {
            let (_, v) = cell_to_str(tree, *child);
            ("option".to_string(), format!("some({v})"))
        }
        Cell::OptionNone => ("option".to_string(), "none".to_string()),
        Cell::ResultOk(payload) => match payload {
            Some(c) => {
                let (_, v) = cell_to_str(tree, *c);
                ("result".to_string(), format!("ok({v})"))
            }
            None => ("result".to_string(), "ok".to_string()),
        },
        Cell::ResultErr(payload) => match payload {
            Some(c) => {
                let (_, v) = cell_to_str(tree, *c);
                ("result".to_string(), format!("err({v})"))
            }
            None => ("result".to_string(), "err".to_string()),
        },

        Cell::RecordOf(side_idx) => {
            let info = side_table_get(&tree.record_infos, *side_idx, "record_infos");
            let parts: Vec<String> = info
                .fields
                .iter()
                .map(|(name, child)| {
                    let (_, v) = cell_to_str(tree, *child);
                    format!("{name}: {v}")
                })
                .collect();
            (
                format!("record({})", info.type_name),
                format!("{{ {} }}", parts.join(", ")),
            )
        }
        Cell::FlagsSet(side_idx) => {
            let info = side_table_get(&tree.flags_infos, *side_idx, "flags_infos");
            (
                format!("flags({})", info.type_name),
                info.set_flags.join(" | "),
            )
        }
        Cell::EnumCase(side_idx) => {
            let info = side_table_get(&tree.enum_infos, *side_idx, "enum_infos");
            (format!("enum({})", info.type_name), info.case_name.clone())
        }
        Cell::VariantCase(side_idx) => {
            let info = side_table_get(&tree.variant_infos, *side_idx, "variant_infos");
            let val = match info.payload {
                Some(p) => {
                    let (_, v) = cell_to_str(tree, p);
                    format!("{}({v})", info.case_name)
                }
                None => info.case_name.clone(),
            };
            (format!("variant({})", info.type_name), val)
        }

        Cell::ResourceHandle(side_idx) => fmt_handle(tree, *side_idx, "resource"),
        Cell::StreamHandle(side_idx) => fmt_handle(tree, *side_idx, "stream"),
        Cell::FutureHandle(side_idx) => fmt_handle(tree, *side_idx, "future"),
        Cell::ErrorContextHandle(side_idx) => {
            // type-name is empty for error-context (the cell-disc already names
            // the kind), so skip the `kind(type)` parenthetical the others use.
            let info = side_table_get(&tree.handle_infos, *side_idx, "handle_infos");
            ("error-context".to_string(), format!("#{}", info.id))
        }
    }
}

/// Convenience: format the root cell as `"type: value"`.
pub fn format_field_tree(tree: &FieldTree) -> String {
    let (ty, val) = cell_to_str(tree, tree.root);
    format!("{ty}: {val}")
}

fn side_table_get<'a, T>(table: &'a [T], idx: u32, name: &'static str) -> &'a T {
    table.get(idx as usize).unwrap_or_else(|| {
        panic!(
            "{name} index {idx} out of bounds (len = {}); splicer codegen contract violation",
            table.len()
        )
    })
}

fn render_children(tree: &FieldTree, children: &[u32]) -> Vec<String> {
    children
        .iter()
        .map(|c| {
            let (_, v) = cell_to_str(tree, *c);
            v
        })
        .collect()
}

fn fmt_handle(tree: &FieldTree, side_idx: u32, kind: &str) -> (String, String) {
    let info = side_table_get(&tree.handle_infos, side_idx, "handle_infos");
    (
        format!("{kind}({})", info.type_name),
        format!("#{}", info.id),
    )
}
