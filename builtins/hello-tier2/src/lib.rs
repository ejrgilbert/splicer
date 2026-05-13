//! Builtin: tier-2 sibling of `hello-tier1`. Prints the wrapped
//! call's lifted args on `on-call` and its lifted result on
//! `on-return`, prefixed with a configurable greeting. Useful for
//! eyeballing the field-tree shape splicer's tier-2 adapter
//! produces against any wrapped interface.
//!
//! Config keys are read once at first observation via
//! `splicer:builtin-config/get` and cached for the instance's
//! lifetime.

mod bindings {
    // Per-export async filter (NOT `async: true`). Every import is
    // sync-WIT and MUST lower as plain `canon lower` (no async); see
    // `docs/TODO/sync-wit-suspend-limit.md` and hello-tier1 for the
    // rationale (sync-WIT-rooted task cannot block on canon-async wait).
    wit_bindgen::generate!({
        world: "hello-tier2-mdl",
        async: [
            "export:splicer:tier2/before@0.1.0#on-call",
            "export:splicer:tier2/after@0.1.0#on-return",
        ],
        generate_all,
    });
}

use std::sync::OnceLock;
use crate::bindings::splicer::builtin_config::get::get as get_config;

use crate::bindings::exports::splicer::tier2::after::Guest as AfterGuest;
use crate::bindings::exports::splicer::tier2::before::Guest as BeforeGuest;
use crate::bindings::splicer::common::types::{CallId, Cell, Field, FieldTree};

/// Print prefix. Read from the `greeting` config key on first call
/// (defaults to `"hello-tier2"`) and cached for the rest of the
/// instance's lifetime. Sync — substrate import is per-import sync
/// (see `bindings`).
fn greeting() -> &'static str {
    static G: OnceLock<String> = OnceLock::new();
    if let Some(g) = G.get() {
        return g.as_str();
    }
    let val = get_config("greeting")
        .unwrap_or_else(|| "hello-tier2".to_string());
    G.get_or_init(|| val).as_str()
}

pub struct HelloTier2;

impl BeforeGuest for HelloTier2 {
    async fn on_call(call: CallId, args: Vec<Field>) {
        let g = greeting();
        let rendered: Vec<String> = args.iter().map(fmt_arg).collect();
        let payload = if rendered.is_empty() {
            "()".to_string()
        } else {
            format!("({})", rendered.join(", "))
        };
        println!(
            "[{}] before {}#{} {}",
            g, call.interface_name, call.function_name, payload
        );
    }
}

impl AfterGuest for HelloTier2 {
    async fn on_return(call: CallId, result: Option<FieldTree>) {
        let g = greeting();
        let payload = match &result {
            Some(tree) => fmt_res(tree),
            None => "()".to_string(),
        };
        println!(
            "[{}] after  {}#{} --> {}",
            g, call.interface_name, call.function_name, payload
        );
    }
}

fn fmt_arg(f: &Field) -> String {
    let (ty, val) = cell_to_str(&f.tree, f.tree.root);
    format!("{}: {ty} = {val}", f.name)
}

fn fmt_res(tree: &FieldTree) -> String {
    let (ty, val) = cell_to_str(tree, tree.root);
    format!("{ty}: {val}")
}

/// Format the cell at `idx` into `(type_label, value)`. Recurses
/// through child indices for compound cells; reads side-table
/// entries for nominal cells. Panics on out-of-bounds lookups —
/// those signal a splicer codegen contract violation, not a user-
/// recoverable condition.
fn cell_to_str(tree: &FieldTree, idx: u32) -> (String, String) {
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

fn side_table_get<'a, T>(table: &'a [T], idx: u32, name: &'static str) -> &'a T {
    table.get(idx as usize).unwrap_or_else(|| {
        panic!(
            "{name} index {idx} out of bounds (len = {}) — splicer codegen contract violation",
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

bindings::export!(HelloTier2 with_types_in bindings);
