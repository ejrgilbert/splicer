//! Default-stub virtualize strategy. Synthesizes the default value
//! of each wrapped call's return type — never invokes the target.
//! Smoke-tests the tier-4 codegen pipeline end-to-end.

use splicer_tool_sdk::{CallId, VirtualizeStrategy};

#[derive(Default)]
pub struct HelloTier4;

// `R: Default` narrows which targets match. Today this surfaces as
// a wrapper compile error; planned splice-time type predicate will
// skip non-matching sites instead.
// See: `docs/TODO/tier3-tier4-substrate.md`, section "Type-predicated rule matching".
impl<Args, R: Default> VirtualizeStrategy<Args, R> for HelloTier4 {
    async fn handle(&self, call: CallId, _args: Args) -> R {
        println!(
            "[hello-tier4] virtualizing {}#{}",
            call.interface_name, call.function_name
        );
        R::default()
    }
}
