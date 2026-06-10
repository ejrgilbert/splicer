//! Tier-4 replayer strategy: per-call returns come from a recorded
//! tier-2 trace.
//!
//! Bound on `R: WitTypedWithResources` so resource leaves in the
//! return tree decode into wrapper newtypes wrapping a
//! `MockedResource`. Args are ignored.

mod bindings {
    wit_bindgen::generate!({
        world: "consumer",
        generate_all,
    });
}

include!(concat!(env!("OUT_DIR"), "/builtin_config_codegen.rs"));

use std::sync::Mutex;

use splicer_tool_sdk::{
    sanitize_for_filename, strip_leading_slashes, CallId, TraceReader, VirtualizeStrategy,
    WitTypedWithResources,
};

pub struct Replayer {
    reader: Mutex<TraceReader>,
}

impl Default for Replayer {
    fn default() -> Self {
        let path = trace_path();
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("[replayer] failed to read trace at {path:?}: {e}"));
        let reader = TraceReader::from_bytes(&bytes)
            .unwrap_or_else(|e| panic!("[replayer] failed to decode trace at {path:?}: {e}"));
        Replayer {
            reader: Mutex::new(reader),
        }
    }
}

impl<Args, R: WitTypedWithResources> VirtualizeStrategy<Args, R> for Replayer {
    async fn handle(&self, call: CallId, _args: Args) -> R {
        let mut reader = self
            .reader
            .lock()
            .expect("[replayer] trace reader poisoned");
        reader.next_return_typed_with_resources::<R>().unwrap_or_else(|e| {
            panic!(
                "[replayer] {}#{} : trace decode failed: {e}",
                call.interface_name, call.function_name,
            )
        })
    }
}

/// Path mirrors recorder's layout for edge recording file lookup.
fn trace_path() -> String {
    let dir = strip_leading_slashes(config::dir());
    let edge = sanitize_for_filename(&edge_id());
    if dir.is_empty() {
        format!("{edge}.bin")
    } else {
        format!("{dir}/{edge}.bin")
    }
}

fn edge_id() -> String {
    crate::bindings::splicer::builtin_config::get::get("_splicer_edge_id")
        .unwrap_or_else(|| "unknown-edge".to_string())
}
