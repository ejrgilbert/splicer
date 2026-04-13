//! Extracts tier interface constants from the canonical WIT files in
//! `wit/tierN/world.wit` so the Rust code has a single source of truth
//! for the interface names it detects at composition time.
//!
//! For each `wit/tierN/world.wit` file, we parse:
//!   - the `package` declaration → e.g. `splicer:tier1@0.1.0`
//!   - every `interface <name>` declaration → e.g. `before`, `after`, `blocking`
//!
//! and generate a Rust source file at `$OUT_DIR/tier_interfaces.rs` with:
//!   - `TIER{N}_PACKAGE: &str` — the unversioned package key
//!   - `TIER{N}_VERSION: &str` — the semver version
//!   - `TIER{N}_INTERFACES: &[&str]` — fully-qualified interface names

use std::fs;
use std::path::Path;

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest = Path::new(&out_dir).join("tier_interfaces.rs");

    let mut generated = String::new();

    // Discover all wit/tierN/ directories in sorted order.
    let wit_dir = Path::new("wit");
    if !wit_dir.is_dir() {
        // No wit/ directory — write an empty file so the include! doesn't fail.
        fs::write(&dest, "// No wit/ directory found during build.\n").unwrap();
        return;
    }

    let mut tier_dirs: Vec<_> = fs::read_dir(wit_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("tier"))
                .unwrap_or(false)
        })
        .collect();
    tier_dirs.sort_by_key(|e| e.file_name());

    for dir_entry in &tier_dirs {
        let dir_name = dir_entry.file_name();
        let dir_name = dir_name.to_str().unwrap();

        // Extract the tier number from the directory name (e.g. "tier1" → "1").
        let tier_num = dir_name
            .strip_prefix("tier")
            .expect("directory name must start with 'tier'");

        let world_path = dir_entry.path().join("world.wit");
        if !world_path.exists() {
            panic!(
                "Expected {}/world.wit to exist for tier {}",
                dir_entry.path().display(),
                tier_num
            );
        }

        // Tell cargo to re-run build.rs if the WIT file changes.
        println!("cargo::rerun-if-changed={}", world_path.display());

        let wit_src = fs::read_to_string(&world_path).unwrap_or_else(|e| {
            panic!("Failed to read {}: {e}", world_path.display());
        });

        // Parse `package splicer:tier1@0.1.0;`
        let (pkg_unversioned, pkg_version) = parse_package_decl(&wit_src, &world_path);

        // Parse every `interface <name> {` declaration.
        let iface_names = parse_interface_names(&wit_src);

        // Build the fully-qualified interface names: "splicer:tier1/before" etc.
        let fq_names: Vec<String> = iface_names
            .iter()
            .map(|name| format!("{pkg_unversioned}/{name}"))
            .collect();

        let upper = tier_num.to_uppercase();
        generated.push_str(&format!(
            "/// Package key for tier-{tier_num} interfaces (no version suffix).\n\
             #[allow(dead_code)]\n\
             pub const TIER{upper}_PACKAGE: &str = \"{pkg_unversioned}\";\n\n\
             /// Semver version of the tier-{tier_num} WIT package.\n\
             #[allow(dead_code)]\n\
             pub const TIER{upper}_VERSION: &str = \"{pkg_version}\";\n\n"
        ));

        // Per-interface named constants (e.g. TIER1_BEFORE, TIER1_AFTER).
        for (name, fq) in iface_names.iter().zip(fq_names.iter()) {
            let const_name = format!(
                "TIER{upper}_{iface_upper}",
                iface_upper = name.to_uppercase()
            );
            generated.push_str(&format!(
                "/// Fully-qualified name of the `{name}` interface in the tier-{tier_num} WIT package.\n\
                 /// Derived from `wit/{dir_name}/world.wit` at build time.\n\
                 pub const {const_name}: &str = \"{fq}\";\n\n"
            ));
        }

        // The aggregate array referencing the per-interface constants.
        generated.push_str(&format!(
            "/// All tier-{tier_num} interface names, for middleware detection.\n\
             /// Derived from `wit/{dir_name}/world.wit` at build time.\n\
             pub const TIER{upper}_INTERFACES: &[&str] = &[\n"
        ));
        for name in &iface_names {
            let const_name = format!(
                "TIER{upper}_{iface_upper}",
                iface_upper = name.to_uppercase()
            );
            generated.push_str(&format!("    {const_name},\n"));
        }
        generated.push_str("];\n\n");
    }

    fs::write(&dest, &generated).unwrap();
}

/// Extract the package declaration from a WIT source string.
/// Returns `(unversioned_package, version)` — e.g. `("splicer:tier1", "0.1.0")`.
fn parse_package_decl(src: &str, path: &Path) -> (String, String) {
    for line in src.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("package ") {
            // "splicer:tier1@0.1.0;" → strip trailing ';' and split on '@'.
            let rest = rest.trim().trim_end_matches(';').trim();
            if let Some((pkg, ver)) = rest.split_once('@') {
                return (pkg.to_string(), ver.to_string());
            }
            panic!(
                "Package declaration in {} missing version: '{}'",
                path.display(),
                line
            );
        }
    }
    panic!("No `package` declaration found in {}", path.display());
}

/// Extract all `interface <name>` declarations from a WIT source string.
fn parse_interface_names(src: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in src.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("interface ") {
            // "interface before {" → extract "before"
            let name = rest
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches('{');
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
    }
    names
}
