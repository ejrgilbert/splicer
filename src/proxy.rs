use std::path::PathBuf;

/// Generate a tier-1 proxy component that wraps `middleware_name` and adapts it to
/// export `target_interface`.
///
/// The generated proxy component:
/// - Exports `target_interface` (making it a drop-in replacement for the upstream caller)
/// - Imports the downstream component providing `target_interface`
/// - Imports the middleware via the tier-1 type-erased interface
/// - For each function in `target_interface`:
///   1. Calls `before-call(fn_name)` if the middleware exports it
///   2. Calls `should-block-call(fn_name)` if the middleware exports it; skips
///      the downstream invocation when it returns `true`
///   3. Forwards the call to the downstream (unless blocked)
///   4. Calls `after-call(fn_name)` if the middleware exports it
///
/// Returns the path to the generated proxy `.wasm` file.
pub fn generate_tier1_proxy(
    middleware_name: &str,
    _middleware_path: Option<&str>,
    target_interface: &str,
    _middleware_interfaces: &[String],
) -> anyhow::Result<PathBuf> {
    // Some notes on the implementation to do here:
    //
    // 1. Use `wasm_encoder`'s [`ComponentBuilder` API](https://docs.rs/wasm-encoder/0.246.1/wasm_encoder/struct.ComponentBuilder.html) directly during codegen.
    // 2. Start with supporting primitive/strings in the interface type signatures, other types should fail with `unimplemented!` macros. We can extend to support them in the next pass
    // 3. For the `should-block-call` -- constrain this to only work on interface signatures that return `void` and document that supporting return `result` with an error variant as future work in the error message
    //
    // Note that any necessary extensions to `wirm` (located locally at `~/git/research/compilers/wirm`)
    // or `cviz` (located locally at `~/git/cosmonic/cviz`) can be done since I own the repositories!

    todo!(
        "tier-1 proxy component generation not yet implemented \
         (middleware: '{middleware_name}', target interface: '{target_interface}')"
    )
}
