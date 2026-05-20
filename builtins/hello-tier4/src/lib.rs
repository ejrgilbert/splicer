//! Default-stub virtualize strategy. Synthesizes the default value
//! of each wrapped call's return type — never invokes the target.
//! Smoke-tests the tier-4 codegen pipeline end-to-end.

use splicer_tool_sdk::{CallId, VirtualizeStrategy};

#[derive(Default)]
pub struct HelloTier4;

// `R: Default` narrows which target WIT shapes this strategy
// accepts. Targets whose return types don't satisfy `Default` will
// fail to compile the generated wrapper, with a precise error
// pointing at this bound.
impl<Args, R: Default> VirtualizeStrategy<Args, R> for HelloTier4 {
    async fn handle(&self, call: CallId, _args: Args) -> R {
        println!(
            "[hello-tier4] virtualizing {}#{}",
            call.interface_name, call.function_name
        );
        R::default()
    }
}
