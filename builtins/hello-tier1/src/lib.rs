mod bindings {
    wit_bindgen::generate!({
        world: "hello-tier1-mdl",
        async: true,
        generate_all
    });
}

use crate::bindings::exports::splicer::tier1::after::Guest as AfterGuest;
use crate::bindings::exports::splicer::tier1::before::Guest as BeforeGuest;
use crate::bindings::splicer::common::types::CallId;

pub struct HelloTier1;

impl BeforeGuest for HelloTier1 {
    async fn on_call(call: CallId) {
        println!(
            "[hello-tier1] before {}#{}",
            call.interface_name, call.function_name
        );
    }
}

impl AfterGuest for HelloTier1 {
    async fn on_return(call: CallId) {
        println!(
            "[hello-tier1] after  {}#{}",
            call.interface_name, call.function_name
        );
    }
}

bindings::export!(HelloTier1 with_types_in bindings);
