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
    splicer_tool_sdk::wit_bindgen!({
        world: "hello-tier2-mdl",
        generate_all,
    });
}

// Codegenned from manifest.toml: manifest custom section + typed
// accessors in `mod config`. Defaults live only in `manifest.toml`.
include!(concat!(env!("OUT_DIR"), "/builtin_config_codegen.rs"));

use crate::bindings::exports::splicer::tier2::after::Guest as AfterGuest;
use crate::bindings::exports::splicer::tier2::before::Guest as BeforeGuest;
use splicer_tool_sdk::{cell_to_str, CallId, Field, FieldTree};

pub struct HelloTier2;

impl BeforeGuest for HelloTier2 {
    fn on_call(call: CallId, args: Vec<Field>) {
        let g = config::greeting();
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
    fn on_return(call: CallId, result: Option<FieldTree>) {
        let g = config::greeting();
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

bindings::export!(HelloTier2 with_types_in bindings);
