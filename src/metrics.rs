//! Code-size accounting for a splice/compose run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::wac::INST_PREFIX;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DepKind {
    /// A sub-component of the original app.
    App,
    /// A tier-1/2 injected middleware component (paired with a [`DepKind::Shim`]).
    Tool,
    /// A tier-3/4 injected component
    Wrapper,
    /// A splicer-generated tier-1/2 adapter component.
    Shim,
    /// A splicer-generated support component (edge shim, sync/async
    /// bridge, or builtin-config provider).
    Support,
}

impl DepKind {
    /// Buckets in display / JSON order.
    const ALL: [DepKind; 5] = [
        DepKind::App,
        DepKind::Shim,
        DepKind::Tool,
        DepKind::Wrapper,
        DepKind::Support,
    ];

    fn label(self) -> &'static str {
        match self {
            DepKind::App => "app",
            DepKind::Shim => "shims",
            DepKind::Tool => "tools",
            DepKind::Wrapper => "wrappers",
            DepKind::Support => "support",
        }
    }
}

/// One leaf component's contribution to the composed total. A tool
/// injected N times appears as N items (one per WAC instantiation),
/// mirroring the N physical copies embedded in the output.
#[derive(Clone, Debug, Serialize)]
pub struct DepItem {
    /// Fully-qualified WAC package key (e.g. `"my:mdl-a"`).
    pub pkg: String,
    pub path: String,
    pub bytes: u64,
    pub kind: DepKind,
}

/// Per-kind rollup: the summed bytes plus the individual line items.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Bucket {
    pub bytes: u64,
    pub items: Vec<DepItem>,
}
impl Bucket {
    fn push(&mut self, item: DepItem) {
        self.bytes += item.bytes;
        self.items.push(item);
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Composed {
    /// On-disk size of the fused component.
    pub total_bytes: u64,
    /// `total − sum(leaves)`
    pub glue_bytes: i64,
}

/// Code-size breakdown of a splice/compose run.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SizeReport {
    pub app: Bucket,
    pub shims: Bucket,
    pub tools: Bucket,
    pub wrappers: Bucket,
    pub support: Bucket,
    pub composed: Option<Composed>,
}
impl SizeReport {
    pub(crate) fn build(
        wac_deps: &BTreeMap<String, PathBuf>,
        kinds: &BTreeMap<String, DepKind>,
    ) -> Self {
        let mut report = SizeReport::default();
        let prefix = format!("{INST_PREFIX}:");
        for (pkg, path) in wac_deps {
            let raw = pkg.strip_prefix(&prefix).unwrap_or(pkg);
            let kind = kinds.get(raw).copied().unwrap_or(DepKind::App);
            let item = DepItem {
                pkg: pkg.clone(),
                path: path.display().to_string(),
                bytes: file_size(path),
                kind,
            };
            report.bucket_mut(kind).push(item);
        }
        report
    }

    fn bucket(&self, kind: DepKind) -> &Bucket {
        match kind {
            DepKind::App => &self.app,
            DepKind::Tool => &self.tools,
            DepKind::Wrapper => &self.wrappers,
            DepKind::Shim => &self.shims,
            DepKind::Support => &self.support,
        }
    }

    fn bucket_mut(&mut self, kind: DepKind) -> &mut Bucket {
        match kind {
            DepKind::App => &mut self.app,
            DepKind::Tool => &mut self.tools,
            DepKind::Wrapper => &mut self.wrappers,
            DepKind::Shim => &mut self.shims,
            DepKind::Support => &mut self.support,
        }
    }

    /// Sum of every leaf's bytes (excludes wac composition glue).
    pub fn leaves_bytes(&self) -> u64 {
        DepKind::ALL.iter().map(|k| self.bucket(*k).bytes).sum()
    }

    /// Record the composed total and derive the glue residual.
    pub(crate) fn set_composed(&mut self, total_bytes: u64) {
        let glue_bytes = total_bytes as i64 - self.leaves_bytes() as i64;
        self.composed = Some(Composed {
            total_bytes,
            glue_bytes,
        });
    }

    /// Serialize the report as pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Render a human-readable table.
    pub fn to_table(&self) -> String {
        let mut out = String::from("code-size breakdown\n");
        for kind in DepKind::ALL {
            let bucket = self.bucket(kind);
            if bucket.items.is_empty() {
                continue;
            }
            let n = bucket.items.len();
            out.push_str(&row(kind.label(), bucket.bytes as i64));
            out.push_str(&format!("  ({n} item{})\n", if n == 1 { "" } else { "s" }));
            for item in &bucket.items {
                // Right-align item bytes under the bucket total column.
                out.push_str(&format!(
                    "{:>28}  {}\n",
                    commas(item.bytes as i64),
                    item.pkg
                ));
            }
        }
        out.push('\n');
        out.push_str(&row("ALL leaves", self.leaves_bytes() as i64));
        out.push('\n');
        if let Some(c) = &self.composed {
            out.push_str(&row("wac glue", c.glue_bytes));
            if c.glue_bytes < 0 {
                out.push_str("  (negative: composer deduped leaves?)");
            }
            out.push('\n');
            out.push_str(&row("--> total", c.total_bytes as i64));
            out.push('\n');
        }
        out
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// A `<label> <right-aligned bytes>` table row (no trailing newline).
fn row(label: &str, bytes: i64) -> String {
    format!("  {:<10} {:>15}", label, commas(bytes))
}

/// Format an integer with thousands separators (`1234567` → `1,234,567`).
/// std has no grouping formatter, so group the digits by hand.
fn commas(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(name: &str, path: &str) -> (String, PathBuf) {
        (format!("{INST_PREFIX}:{name}"), PathBuf::from(path))
    }

    #[test]
    fn classifies_and_defaults_to_app() {
        let mut wac_deps = BTreeMap::new();
        // Two app splits (no kind), one tool, one shim.
        wac_deps.extend([
            dep("split0", "/nope/split0.wasm"),
            dep("split1", "/nope/split1.wasm"),
            dep("mdl-a", "/nope/mdl-a.wasm"),
            dep("mdl-a-adapter-x", "/nope/adapter.wasm"),
        ]);
        let mut kinds = BTreeMap::new();
        kinds.insert("mdl-a".to_string(), DepKind::Tool);
        kinds.insert("mdl-a-adapter-x".to_string(), DepKind::Shim);

        let report = SizeReport::build(&wac_deps, &kinds);
        // Missing files contribute 0 bytes but still land in the right bucket.
        assert_eq!(report.app.items.len(), 2);
        assert_eq!(report.tools.items.len(), 1);
        assert_eq!(report.shims.items.len(), 1);
        assert_eq!(report.wrappers.items.len(), 0);
    }

    #[test]
    fn multiplicity_counts_each_injection() {
        // Same tool injected 3× → 3 distinct pkgs → 3 items.
        let mut wac_deps = BTreeMap::new();
        let mut kinds = BTreeMap::new();
        for n in ["mdl-a", "mdl-b", "mdl-c"] {
            let (k, p) = dep(n, "/nope/printer_mdl.wasm");
            wac_deps.insert(k, p);
            kinds.insert(n.to_string(), DepKind::Tool);
        }
        let report = SizeReport::build(&wac_deps, &kinds);
        assert_eq!(report.tools.items.len(), 3);
    }

    #[test]
    fn glue_is_total_minus_leaves() {
        let report = SizeReport {
            app: Bucket {
                bytes: 100,
                items: vec![],
            },
            shims: Bucket::default(),
            tools: Bucket {
                bytes: 50,
                items: vec![],
            },
            wrappers: Bucket::default(),
            support: Bucket::default(),
            composed: None,
        };
        let mut report = report;
        report.set_composed(170);
        let c = report.composed.unwrap();
        assert_eq!(report.leaves_bytes(), 150);
        assert_eq!(c.glue_bytes, 20);
    }

    #[test]
    fn all_kinds_reconcile_to_total() {
        // One dep of every kind; assert bucket routing + the
        // app+shims+tools+wrappers+support+glue == total identity.
        let mut wac_deps = BTreeMap::new();
        let mut kinds = BTreeMap::new();
        for (name, kind) in [
            ("split0", None),
            ("mdl", Some(DepKind::Tool)),
            ("mdl-adapter-x", Some(DepKind::Shim)),
            ("wrap", Some(DepKind::Wrapper)),
            ("mdl-config", Some(DepKind::Support)),
        ] {
            let (k, p) = dep(name, "/nope/x.wasm");
            wac_deps.insert(k, p);
            if let Some(kind) = kind {
                kinds.insert(name.to_string(), kind);
            }
        }
        let mut report = SizeReport::build(&wac_deps, &kinds);
        assert_eq!(report.app.items.len(), 1);
        assert_eq!(report.tools.items.len(), 1);
        assert_eq!(report.shims.items.len(), 1);
        assert_eq!(report.wrappers.items.len(), 1);
        assert_eq!(report.support.items.len(), 1);

        report.set_composed(report.leaves_bytes() + 42);
        let c = report.composed.unwrap();
        assert_eq!(c.glue_bytes, 42);
        // The reconciliation identity the report guarantees.
        assert_eq!(
            report.leaves_bytes() as i64 + c.glue_bytes,
            c.total_bytes as i64
        );
    }

    #[test]
    fn commas_formats() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1_000), "1,000");
        assert_eq!(commas(3_720_955), "3,720,955");
        assert_eq!(commas(-1_234), "-1,234");
    }
}
