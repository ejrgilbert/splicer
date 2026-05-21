//! Pass-through transform strategy. Prints a line before and after
//! each wrapped call. Smoke-tests the tier-3 codegen pipeline
//! end-to-end with the smallest meaningful strategy body.

use splicer_tool_sdk::{CallId, TransformStrategy};

#[derive(Default)]
pub struct HelloTier3;

impl<Args, R> TransformStrategy<Args, R> for HelloTier3 {
    async fn handle(
        &self,
        call: CallId,
        args: Args,
        downstream: impl AsyncFnOnce(Args) -> R,
    ) -> R {
        println!(
            "[hello-tier3] before {}#{}",
            call.interface_name, call.function_name
        );
        let r = downstream(args).await;
        println!(
            "[hello-tier3] after  {}#{}",
            call.interface_name, call.function_name
        );
        r
    }
}
