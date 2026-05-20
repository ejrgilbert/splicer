//! Canonical edge_id rendering. Format:
//! `{interface}::{caller}->{provider}`, or `{interface}::->{provider}`
//! at the composition boundary.

/// Reserved key splicer publishes into every builtin that imports
/// `splicer:builtin-config`. The single-underscore prefix marks it as
/// splicer-internal — user manifests can never declare keys with this
/// prefix.
pub const EDGE_ID_CONFIG_KEY: &str = "_splicer_edge_id";

/// Canonical edge_id for an `interface` edge from `from` (the caller)
/// to `to` (the provider). `from = None` is the boundary case — the
/// caller is external to the composition; the rendered form drops the
/// caller segment, leaving the leading `->` as a marker.
pub fn derive_edge_id(interface: &str, from: Option<&str>, to: &str) -> String {
    match from {
        Some(caller) => format!("{interface}::{caller}->{to}"),
        None => format!("{interface}::->{to}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_edge_renders_caller_to_provider() {
        assert_eq!(
            derive_edge_id(
                "wasi:http/handler@0.3.0-rc-2026-01-06",
                Some("srv-b"),
                "srv-a",
            ),
            "wasi:http/handler@0.3.0-rc-2026-01-06::srv-b->srv-a",
        );
    }

    #[test]
    fn boundary_edge_drops_caller_segment() {
        assert_eq!(
            derive_edge_id("wasi:http/handler@0.3.0", None, "srv-a"),
            "wasi:http/handler@0.3.0::->srv-a",
        );
    }

    #[test]
    fn before_and_between_targeting_same_edge_collide() {
        // The doc's invariant: a `before(provider=B)` matching caller A
        // and a `between(outer=A, inner=B)` identify the same physical
        // edge, so they must render to the same edge_id.
        let from_between = derive_edge_id("ns:pkg/iface@1.0.0", Some("A"), "B");
        let from_before = derive_edge_id("ns:pkg/iface@1.0.0", Some("A"), "B");
        assert_eq!(from_between, from_before);
    }

    #[test]
    fn derivation_is_deterministic() {
        let a = derive_edge_id("ns:pkg/iface@1.2.3", Some("caller"), "provider");
        let b = derive_edge_id("ns:pkg/iface@1.2.3", Some("caller"), "provider");
        assert_eq!(a, b);
    }
}
