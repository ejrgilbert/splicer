//! Default-stub virtualize strategy. Synthesizes the default value
//! of each wrapped call's return type — never invokes the target.
//! Smoke-tests the tier-4 codegen pipeline end-to-end.

use splicer_tool_sdk::{CallId, VirtualizeStrategy};

#[derive(Default)]
pub struct HelloTier4;

// `R: Default` narrows which targets match.
// Use splicer type predication to constrain to `concrete` results.
impl<Args, R: Default> VirtualizeStrategy<Args, R> for HelloTier4 {
    async fn handle(&self, call: CallId, _args: Args) -> R {
        println!(
            "[hello-tier4] virtualizing {}#{}",
            call.interface_name, call.function_name
        );
        R::default()
    }
}
