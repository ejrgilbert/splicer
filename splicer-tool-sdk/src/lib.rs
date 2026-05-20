//! Splicer tool SDK: canonical Rust types for splicer middleware and
//! tooling, plus helpers built on top of them.
//!
//! Splicer's tier-N middleware contract is defined in WIT; without
//! coordination, each crate that runs `wit_bindgen::generate!` would
//! emit its own fresh Rust mirror of `splicer:common/types`, and Rust's
//! nominal typing would prevent passing values between them. This crate
//! defines a single canonical set of those types; consumers point
//! their `wit_bindgen::generate!` macro's `with:` parameter at these
//! types to share one type identity across every crate in the stack.
//!
//! See the crate-root `README.md` for the full `with:` boilerplate.

pub mod format;
pub mod strategy;
pub mod types;
pub mod wave_bridge;

pub use format::{cell_to_str, format_field_tree};
pub use strategy::{ForwardStrategy, VirtualizeStrategy};
pub use types::{
    CallId, Cell, EnumInfo, Field, FieldTree, FlagsInfo, HandleInfo, RecordInfo, VariantInfo,
};
pub use wave_bridge::{cells_to_typed, cells_to_value, BridgeError, WitTyped};

/// Re-export wasm-wave so consumers depend on one crate and pick up
/// the same `WasmType` / `WasmValue` traits the SDK's bridge speaks.
pub use wasm_wave;

/// Convenience wrapper around [`wit_bindgen::generate!`] that injects
/// the SDK's canonical `with:` mappings for every type in
/// `splicer:common/types@0.2.0`. Use it exactly like
/// `wit_bindgen::generate!` minus the boilerplate.
///
/// If your world needs additional `with:` remappings (e.g. for a
/// `wasi:io/streams` handle type), include a `with: { ... }` block in
/// the args; this macro merges your entries with the SDK's canonical
/// ones, so both end up in the final `wit_bindgen::generate!` call.
///
/// # Example
///
/// ```ignore
/// mod bindings {
///     splicer_tool_sdk::wit_bindgen!({
///         world: "my-middleware-mdl",
///         async: [
///             "export:splicer:tier2/before@0.1.0#on-call",
///             "export:splicer:tier2/after@0.1.0#on-return",
///         ],
///         generate_all,
///     });
/// }
/// ```
#[macro_export]
macro_rules! wit_bindgen {
    // Arm 1: user supplied their own `with:` block; merge it with the
    // SDK's canonical entries. The user's entries land after the SDK's,
    // so a key collision (e.g. user overriding our `Cell` type) lets
    // the user's mapping win under wit-bindgen's HashMap::insert
    // semantics.
    ({ $($before:tt)* with: { $($user_with:tt)* } $($after:tt)* }) => {
        ::wit_bindgen::generate!({
            $($before)*
            with: {
                "splicer:common/types@0.2.0/cell":          ::splicer_tool_sdk::Cell,
                "splicer:common/types@0.2.0/field-tree":    ::splicer_tool_sdk::FieldTree,
                "splicer:common/types@0.2.0/field":         ::splicer_tool_sdk::Field,
                "splicer:common/types@0.2.0/call-id":       ::splicer_tool_sdk::CallId,
                "splicer:common/types@0.2.0/record-info":   ::splicer_tool_sdk::RecordInfo,
                "splicer:common/types@0.2.0/flags-info":    ::splicer_tool_sdk::FlagsInfo,
                "splicer:common/types@0.2.0/enum-info":     ::splicer_tool_sdk::EnumInfo,
                "splicer:common/types@0.2.0/variant-info":  ::splicer_tool_sdk::VariantInfo,
                "splicer:common/types@0.2.0/handle-info":   ::splicer_tool_sdk::HandleInfo,
                $($user_with)*
            },
            $($after)*
        });
    };

    // Arm 2: no user `with:`. Emit just the SDK's canonical entries.
    ({ $($opts:tt)* }) => {
        ::wit_bindgen::generate!({
            $($opts)*
            with: {
                "splicer:common/types@0.2.0/cell":          ::splicer_tool_sdk::Cell,
                "splicer:common/types@0.2.0/field-tree":    ::splicer_tool_sdk::FieldTree,
                "splicer:common/types@0.2.0/field":         ::splicer_tool_sdk::Field,
                "splicer:common/types@0.2.0/call-id":       ::splicer_tool_sdk::CallId,
                "splicer:common/types@0.2.0/record-info":   ::splicer_tool_sdk::RecordInfo,
                "splicer:common/types@0.2.0/flags-info":    ::splicer_tool_sdk::FlagsInfo,
                "splicer:common/types@0.2.0/enum-info":     ::splicer_tool_sdk::EnumInfo,
                "splicer:common/types@0.2.0/variant-info":  ::splicer_tool_sdk::VariantInfo,
                "splicer:common/types@0.2.0/handle-info":   ::splicer_tool_sdk::HandleInfo,
            },
        });
    };
}
