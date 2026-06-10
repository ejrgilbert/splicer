//! Pass-through transform strategy. Prints a line before and after
//! each wrapped call. Smoke-tests the tier-3 codegen pipeline
//! end-to-end with the smallest meaningful strategy body — no bounds
//! on `R` so it can interpose any tier-3-eligible target.

mod bindings {
    wit_bindgen::generate!({
        world: "consumer",
        generate_all,
    });
}

include!(concat!(env!("OUT_DIR"), "/builtin_config_codegen.rs"));

use splicer_tool_sdk::{CallId, TransformStrategy};

#[derive(Default)]
pub struct HelloTier3;

impl<Args, R> TransformStrategy<Args, R> for HelloTier3 {
    async fn handle(
        &self,
        call: CallId,
        args: Args,
        downstream: impl AsyncFn(Args) -> R,
    ) -> R {
        let greeting = config::greeting();
        println!(
            "[{greeting}] before {}#{}",
            call.interface_name, call.function_name
        );
        let r = downstream(args).await;
        println!(
            "[{greeting}] after  {}#{}",
            call.interface_name, call.function_name
        );
        r
    }
}
