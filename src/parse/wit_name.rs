//! Parsing helpers for WIT qualified-interface names of the shape `ns:pkg/iface[@ver]`.

/// Strip the `@ver` suffix from a WIT qname or pattern, if present.
///
/// `wasi:http/handler@0.3.0` -> `wasi:http/handler`
/// `wasi:*` -> `wasi:*` (no `@`, returned unchanged)
pub(crate) fn unversioned(qname: &str) -> &str {
    qname.split_once('@').map(|(base, _)| base).unwrap_or(qname)
}

/// Structured view of `ns:pkg/iface[@ver]`.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WitName<'a> {
    pub(crate) ns: &'a str,
    pub(crate) pkg: &'a str,
    pub(crate) iface: &'a str,
    pub(crate) version: Option<&'a str>,
}

#[cfg(test)]
impl<'a> WitName<'a> {
    /// Parse `ns:pkg/iface[@ver]`. Returns `None` if the input lacks
    /// either the `ns:pkg` colon or the `/iface` slash.
    pub(crate) fn parse(qname: &'a str) -> Option<Self> {
        let (base, version) = match qname.split_once('@') {
            Some((b, v)) => (b, Some(v)),
            None => (qname, None),
        };
        let (ns_pkg, iface) = base.split_once('/')?;
        let (ns, pkg) = ns_pkg.split_once(':')?;
        if ns.is_empty() || pkg.is_empty() || iface.is_empty() {
            return None;
        }
        Some(Self {
            ns,
            pkg,
            iface,
            version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unversioned_strips_suffix() {
        assert_eq!(unversioned("wasi:http/handler@0.3.0"), "wasi:http/handler");
    }

    #[test]
    fn unversioned_passes_through_when_no_at() {
        assert_eq!(unversioned("wasi:http/handler"), "wasi:http/handler");
        assert_eq!(unversioned("wasi:*"), "wasi:*");
        assert_eq!(unversioned(""), "");
    }

    #[test]
    fn parse_with_version() {
        let n = WitName::parse("wasi:http/handler@0.3.0").unwrap();
        assert_eq!(n.ns, "wasi");
        assert_eq!(n.pkg, "http");
        assert_eq!(n.iface, "handler");
        assert_eq!(n.version, Some("0.3.0"));
    }

    #[test]
    fn parse_without_version() {
        let n = WitName::parse("my:srv/api").unwrap();
        assert_eq!(n.ns, "my");
        assert_eq!(n.pkg, "srv");
        assert_eq!(n.iface, "api");
        assert_eq!(n.version, None);
    }

    #[test]
    fn parse_rejects_missing_separators() {
        assert!(WitName::parse("wasi:http").is_none());
        assert!(WitName::parse("wasi/http/handler").is_none());
        assert!(WitName::parse("").is_none());
        assert!(WitName::parse(":pkg/iface").is_none());
        assert!(WitName::parse("ns:/iface").is_none());
        assert!(WitName::parse("ns:pkg/").is_none());
    }
}
