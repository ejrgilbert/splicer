//! Per-target wrapper-component source codegen for forward
//! (transform) and virtualize strategies. Sibling to the
//! bytecode-emitting [`super::tier1`] and [`super::tier2`]
//! adapters; emits Rust source that downstream stages compile to
//! wasm via `cargo build`.
//!
//! Pipeline (stages 3-4 of `docs/TODO/tier3-tier4-substrate.md`):
//! 1. [`read_behavior`] reads the strategy crate's `Cargo.toml`
//!    `[package.metadata.splicer] behavior = ...` declaration.
//! 2. The codegen template emits a Cargo project tailored to the
//!    declared behavior: forward strategies get a wrapper that
//!    imports the target; virtualize strategies get a wrapper that
//!    does not.
//! 3. The cargo build pipeline compiles the project to a wrapper
//!    `.wasm` and caches the result.

mod behavior_meta;
mod bindgen;
mod bindings_walk;
mod emit_method;
mod emit_wit_typed;

pub use behavior_meta::{read_behavior, read_behavior_from_str, Behavior, BehaviorReadError};
pub use bindgen::run_wit_bindgen_rust;
pub use bindings_walk::{
    walk_bindings, GuestMethod, GuestTrait, TypeDef, TypeDefKind, WrapperBindings,
};
pub use emit_method::{emit_guest, EmittedGuest};
pub use emit_wit_typed::emit_wit_typed_impls;
