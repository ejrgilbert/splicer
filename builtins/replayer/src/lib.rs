//! Tier-4 replayer strategy: per-call returns come from a recorded
//! tier-2 trace.
//!
//! Bound on `R: WitTypedWithResources` so resource leaves in the
//! return tree decode into wrapper newtypes wrapping a
//! `MockedResource`. Args are ignored.

use std::sync::Mutex;

use splicer_tool_sdk::{
    CallId, TraceReader, VirtualizeStrategy, WitTypedWithResources,
};

/// TODO: Long-term this should come through the tier-3/4
///   builtin-config substrate; until that lands, the wrapper component
///   reads the path from this env var at first-call time.
const TRACE_PATH_ENV: &str = "SPLICER_REPLAY_TRACE";

pub struct Replayer {
    reader: Mutex<TraceReader>,
}

impl Default for Replayer {
    fn default() -> Self {
        let path = std::env::var(TRACE_PATH_ENV).unwrap_or_else(|_| {
            panic!(
                "[replayer] {TRACE_PATH_ENV} env var must be set to the path of a recorded trace"
            )
        });
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
