//! Shared fixture helpers for tier-2 unit tests.

use wit_parser::{InterfaceId, Resolve};

/// Find an interface by its unversioned qname (e.g. `splicer:common/types`),
/// ignoring any `@x.y.z` suffix the WIT carries. Panics with a clear message
/// if no matching interface is loaded — every call site uses this for known
/// fixture WIT, so absence is a fixture bug, not a runtime condition.
pub(super) fn iface_by_unversioned_qname(resolve: &Resolve, qname: &str) -> InterfaceId {
    resolve
        .interfaces
        .iter()
        .find_map(|(id, _)| {
            let q = resolve.id_of(id)?;
            let unversioned = q.split('@').next().unwrap_or(&q);
            (unversioned == qname).then_some(id)
        })
        .unwrap_or_else(|| panic!("interface `{qname}` not found in resolve"))
}
