//! Chaos err-injection virtualize strategy.
//!
//! Use this when you want every call to deterministically take the
//! err path (e.g. to verify caller-side error handling executes on
//! every test run), while still seeing variance in the err *value*
//! across calls (different enum cases, different string contents).

mod bindings {
    wit_bindgen::generate!({
        world: "consumer",
        generate_all,
    });
}

include!(concat!(env!("OUT_DIR"), "/builtin_config_codegen.rs"));

use splicer_tool_sdk::arbitrary::Unstructured;
use splicer_tool_sdk::{CallId, HasArbitraryErr, VirtualizeStrategy};

#[derive(Default)]
pub struct ChaosErr;

impl<Args, R: HasArbitraryErr> VirtualizeStrategy<Args, R> for ChaosErr {
    async fn handle(&self, call: CallId, _args: Args) -> R {
        let entropy_bytes = config::entropy_bytes();
        let mut buf = vec![0u8; entropy_bytes as usize];
        getrandom::fill(&mut buf)
            .expect("wasi:random/random is available to the wrapper component");
        let mut u = Unstructured::new(&buf);
        match R::arbitrary_err(&mut u) {
            Ok(r) => r,
            Err(e) => {
                panic!(
                    "[chaos-err] {}#{} : arbitrary_err exhausted entropy ({entropy_bytes} bytes); \
                     bump the `entropy-bytes` config key. underlying error: {e:?}",
                    call.interface_name,
                    call.function_name
                );
            }
        }
    }
}
