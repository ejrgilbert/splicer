//! Chaos err-injection virtualize strategy.
//!
//! Each wrapped call returns a fresh `Err(_)` whose value is sampled
//! from a per-call entropy stream via the SDK's [`HasArbitraryErr`]
//! trait. The Ok arm is never instantiated, so its type doesn't need
//! to impl `Arbitrary` — resource-bearing Ok arms compose fine.
//!
//! Use this when you want every call to deterministically take the
//! err path (e.g. to verify caller-side error handling executes on
//! every test run), while still seeing variance in the err *value*
//! across calls (different enum cases, different string contents).

use splicer_tool_sdk::arbitrary::Unstructured;
use splicer_tool_sdk::{CallId, HasArbitraryErr, VirtualizeStrategy};

#[derive(Default)]
pub struct ChaosErr;

/// Per-call entropy buffer. 256 bytes covers typical err shapes
/// (enums, short strings, small records); `Unstructured` reads only
/// what each `Arbitrary` impl needs and returns sensible defaults if
/// it runs out, so the buffer caps tail latency without compromising
/// variance for realistic err types.
const ENTROPY_BYTES: usize = 256;

impl<Args, R: HasArbitraryErr> VirtualizeStrategy<Args, R> for ChaosErr {
    async fn handle(&self, call: CallId, _args: Args) -> R {
        let mut buf = [0u8; ENTROPY_BYTES];
        getrandom::getrandom(&mut buf)
            .expect("wasi:random/random is available to the wrapper component");
        let mut u = Unstructured::new(&buf);
        match R::arbitrary_err(&mut u) {
            Ok(r) => r,
            Err(e) => {
                // `Arbitrary` should never run out on 256 bytes for
                // realistic err types; if a specific E claims more,
                // bump ENTROPY_BYTES rather than fall through silently.
                panic!(
                    "[chaos-err] {}#{} : arbitrary_err exhausted entropy: {e:?}",
                    call.interface_name, call.function_name,
                );
            }
        }
    }
}
