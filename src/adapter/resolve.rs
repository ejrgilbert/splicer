//! WIT-resolve plumbing shared between tier-1 and tier-2 adapter
//! generators: decoding the input split's WIT into a [`Resolve`] and
//! locating a target interface within it. Both tiers' `build_*_adapter`
//! entry points open with the same two calls.

use anyhow::{anyhow, bail, Context, Result};
use wit_component::{decode, DecodedWasm};
use wit_parser::{InterfaceId, LiftLowerAbi, ManglingAndAbi, Resolve};

// ─── Name-mangling helpers ────────────────────────────────────────
//
// `ManglingAndAbi::Legacy` is the only mangling currently supported
// for the async ABI in wit-parser; `Standard32` exists but is
// sync-only as of v0.247. Until upstream tools migrate (wit-component,
// wit-bindgen, wac all emit Legacy-mangled names today), we have to
// match. These helpers centralize that choice — when the migration
// happens, every callsite flips at once from one place.

/// Mangling for a target dispatch function: `AsyncStackful` mangling
/// when the WIT function is `async`, else `Sync`.
pub(crate) fn dispatch_mangling(is_async: bool) -> ManglingAndAbi {
    ManglingAndAbi::Legacy(if is_async {
        LiftLowerAbi::AsyncStackful
    } else {
        LiftLowerAbi::Sync
    })
}

/// Mangling for canon-async hook imports (tier-1 before/after/blocking,
/// tier-2 on-call/on-return/on-trap). Always `AsyncCallback`.
pub(crate) fn hook_callback_mangling() -> ManglingAndAbi {
    ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback)
}

/// Mangling for plain sync imports — used by tier-1's resource-drop
/// import lookup.
pub(crate) fn sync_mangling() -> ManglingAndAbi {
    ManglingAndAbi::Legacy(LiftLowerAbi::Sync)
}

/// Decode the input split's WIT into a [`Resolve`]; bail if the bytes
/// decode to a WIT package rather than a component. `wit_component::decode`
/// panics on splits that import + re-export a resource-bearing instance
/// (https://github.com/bytecodealliance/wasm-tools/issues/2506); catch
/// it and surface a structured error so the process doesn't die.
pub(super) fn decode_input_resolve(split_bytes: &[u8]) -> Result<Resolve> {
    let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode(split_bytes)))
        .map_err(|_| {
            anyhow!(
                "wit-parser panic during component decode — likely the import + re-export \
                 of a resource-bearing instance (upstream issue \
                 https://github.com/bytecodealliance/wasm-tools/issues/2506). The new emit \
                 path can't proceed until that's fixed upstream."
            )
        })?
        .context("wit_component::decode split")?;
    match decoded {
        DecodedWasm::Component(resolve, _world) => Ok(resolve),
        DecodedWasm::WitPackage(_, _) => bail!(
            "split bytes decoded to a WIT package; \
             expected a component"
        ),
    }
}

/// Find the target interface by its fully-qualified name.
pub(super) fn find_target_interface(
    resolve: &Resolve,
    target_interface: &str,
) -> Result<InterfaceId> {
    resolve
        .interfaces
        .iter()
        .find(|(id, _)| resolve.id_of(*id).as_deref() == Some(target_interface))
        .map(|(id, _)| id)
        .ok_or_else(|| {
            anyhow!(
                "interface `{target_interface}` not found in \
                 the decoded WIT; available: {:?}",
                resolve
                    .interfaces
                    .iter()
                    .filter_map(|(id, _)| resolve.id_of(id))
                    .collect::<Vec<_>>()
            )
        })
}
