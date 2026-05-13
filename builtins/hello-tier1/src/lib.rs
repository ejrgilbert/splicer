mod bindings {
    // Per-export async filter (NOT `async: true`). Every import is
    // sync-WIT and MUST lower as plain `canon lower` (no async); see
    // `docs/TODO/sync-wit-suspend-limit.md` and hello-tier1 for the
    // rationale (sync-WIT-rooted task cannot block on canon-async wait).
    wit_bindgen::generate!({
        world: "hello-tier1-mdl",
        async: [
            "export:splicer:tier1/before@0.3.0#on-call",
            "export:splicer:tier1/after@0.3.0#on-return",
        ],
        generate_all
    });
}

use std::sync::OnceLock;

use crate::bindings::exports::splicer::tier1::after::Guest as AfterGuest;
use crate::bindings::exports::splicer::tier1::before::Guest as BeforeGuest;
use crate::bindings::splicer::builtin_config::get::get as get_config;
use crate::bindings::splicer::common::types::CallId;

/// Print prefix. Read from the `greeting` config key on first call
/// (defaults to `"hello-tier1"`) and cached for the rest of the
/// instance's lifetime. Sync because the substrate import is
/// per-import sync (see `bindings`).
fn greeting() -> &'static str {
    static G: OnceLock<String> = OnceLock::new();
    if let Some(g) = G.get() {
        return g.as_str();
    }
    let val = get_config("greeting")
        .unwrap_or_else(|| "hello-tier1".to_string());
    G.get_or_init(|| val).as_str()
}

pub struct HelloTier1;

impl BeforeGuest for HelloTier1 {
    async fn on_call(call: CallId) {
        println!(
            "[{}] before {}#{}",
            greeting(),
            call.interface_name,
            call.function_name
        );
    }
}

impl AfterGuest for HelloTier1 {
    async fn on_return(call: CallId) {
        println!(
            "[{}] after  {}#{}",
            greeting(),
            call.interface_name,
            call.function_name
        );
    }
}

bindings::export!(HelloTier1 with_types_in bindings);
