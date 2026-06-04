mod bindings {
    // Per-export async filter (NOT `async: true`). Every import is
    // sync-WIT and MUST lower as plain `canon lower` (no async); see
    // `docs/TODO/sync-wit-suspend-limit.md` and hello-tier1 for the
    // rationale (sync-WIT-rooted task cannot block on canon-async wait).
    wit_bindgen::generate!({
        world: "hello-tier1-mdl",
        async: [
            "export:splicer:tier1/before@0.4.0#on-call",
            "export:splicer:tier1/after@0.4.0#on-return",
        ],
        generate_all
    });
}

// Codegenned from manifest.toml: `SPLICER_BUILTIN_MANIFEST` static
// (embedded blob for `splicer builtin <name>` introspection) +
// `mod config` (typed accessors with manifest-rooted defaults).
include!(concat!(env!("OUT_DIR"), "/builtin_config_codegen.rs"));

use crate::bindings::exports::splicer::tier1::after::Guest as AfterGuest;
use crate::bindings::exports::splicer::tier1::before::Guest as BeforeGuest;
use crate::bindings::splicer::common::types::CallId;

pub struct HelloTier1;

impl BeforeGuest for HelloTier1 {
    async fn on_call(call: CallId) {
        println!(
            "[{}] before {}#{}",
            config::greeting(),
            call.interface_name,
            call.function_name
        );
    }
}

impl AfterGuest for HelloTier1 {
    async fn on_return(call: CallId) {
        println!(
            "[{}] after  {}#{}",
            config::greeting(),
            call.interface_name,
            call.function_name
        );
    }
}

bindings::export!(HelloTier1 with_types_in bindings);
