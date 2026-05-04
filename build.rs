//! Extracts tier interface constants from the canonical WIT files in
//! `wit/tierN/world.wit` so the Rust code has a single source of truth
//! for the interface names it detects at composition time.
//!
//! For each `wit/tierN/world.wit` file, we parse:
//!   - the `package` declaration → e.g. `splicer:tier1@0.1.0`
//!   - every `interface <name>` declaration → e.g. `before`, `after`, `blocking`
//!   - every `<fn-name>: [async] func(...)` line inside each interface
//!
//! and generate a Rust source file at `$OUT_DIR/tier_interfaces.rs` with:
//!   - `TIER{N}_PACKAGE: &str` — the unversioned package key
//!   - `TIER{N}_VERSION: &str` — the semver version
//!   - `TIER{N}_INTERFACES: &[&str]` — fully-qualified interface names
//!   - `TIER{N}_{IFACE}_FNS: &[&str]` — function names inside each interface

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

    // Watch every world.wit under wit/ (tier dirs + common + any
    // future siblings) so cargo rebuilds when any WIT changes.
    for entry in fs::read_dir(wit_dir).unwrap().filter_map(|e| e.ok()) {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let world_path = entry.path().join("world.wit");
        if world_path.exists() {
            println!("cargo::rerun-if-changed={}", world_path.display());
        }
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

        // Parse every `interface <name> { ... }` block, capturing the
        // function names declared inside each.
        let ifaces = parse_interfaces(&wit_src);
        let iface_names: Vec<String> = ifaces.iter().map(|(n, _)| n.clone()).collect();

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
        for ((name, _fns), fq) in ifaces.iter().zip(fq_names.iter()) {
            let iface_upper = name.to_uppercase().replace('-', "_");
            let const_name = format!("TIER{upper}_{iface_upper}");
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
            let iface_upper = name.to_uppercase().replace('-', "_");
            let const_name = format!("TIER{upper}_{iface_upper}");
            generated.push_str(&format!("    {const_name},\n"));
        }
        generated.push_str("];\n\n");
    }

    fs::write(&dest, &generated).unwrap();

    generate_builtins_registry(&out_dir);
}

/// Scan `assets/builtins/*.wasm` and emit a slice expression that
/// `src/builtins.rs` `include!`s as the auto-generated `BUILTINS`
/// registry. Drop a built `.wasm` under that dir and the next splicer
/// build picks it up — no source edits in `src/builtins.rs` needed.
fn generate_builtins_registry(out_dir: &str) {
    let dest = Path::new(out_dir).join("builtins_registry.rs");
    let assets_dir = Path::new("assets").join("builtins");

    // Watch the directory itself so cargo reruns when files are
    // added or removed (changes to existing files are caught by the
    // per-file rerun-if-changed lines below).
    println!("cargo::rerun-if-changed=assets/builtins");

    let mut rows = String::new();
    if assets_dir.is_dir() {
        let mut wasms: Vec<_> = fs::read_dir(&assets_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("wasm"))
            .collect();
        wasms.sort_by_key(|e| e.file_name());
        for entry in wasms {
            let path = entry.path();
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            let abs = path.canonicalize().unwrap_or_else(|_| path.clone());
            // Forward-slashes work on every platform `include_bytes!`
            // resolves paths on, and avoid Windows backslash escaping.
            let abs_str = abs.to_string_lossy().replace('\\', "/");
            println!("cargo::rerun-if-changed={abs_str}");
            rows.push_str(&format!(
                "    (\"{name}\", include_bytes!(\"{abs_str}\") as &[u8]),\n"
            ));
        }
    }

    let content = format!(
        "// Auto-generated by build.rs from assets/builtins/*.wasm. \
         Do not edit.\n&[\n{rows}]\n"
    );
    fs::write(&dest, content).unwrap();
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

/// Extract every `interface <name> { ... }` block from a WIT source
/// string, returning `(iface_name, fn_names)` pairs in declaration
/// order. Function names are parsed with a line-based matcher —
/// anything of the form `<name>: [async] func(...)` inside the
/// interface body. This is deliberately narrow: full WIT type
/// parsing (param/result types, compound signatures) is a separate
/// project that would want `wit-parser`; here we only extract names
/// the adapter needs to string-match against component exports at
/// runtime.
fn parse_interfaces(src: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;

    for line in src.lines() {
        let line = line.trim();

        if let Some(rest) = line.strip_prefix("interface ") {
            // Starting a new interface block. If one was open (shouldn't
            // happen in well-formed WIT), flush it first.
            if let Some(prev) = current.take() {
                out.push(prev);
            }
            let name = rest
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches('{')
                .to_string();
            if !name.is_empty() {
                current = Some((name, Vec::new()));
            }
            continue;
        }

        if line == "}" {
            if let Some(iface) = current.take() {
                out.push(iface);
            }
            continue;
        }

        if let Some((_, ref mut fns)) = current.as_mut() {
            if let Some(fn_name) = parse_fn_decl_name(line) {
                fns.push(fn_name);
            }
        }
    }

    // A file missing a closing `}` still flushes a partial block, for
    // sanity — malformed WIT should fail elsewhere.
    if let Some(iface) = current {
        out.push(iface);
    }

    out
}

/// Extract the function name from a line like
/// `before-call: async func(name: string);` or
/// `get-info: func() -> string;`. Returns `None` for anything that
/// doesn't look like a function declaration.
fn parse_fn_decl_name(line: &str) -> Option<String> {
    // Must contain `func(` somewhere after a `:`.
    let (lhs, rhs) = line.split_once(':')?;
    let rhs = rhs.trim();
    let rhs = rhs.strip_prefix("async ").unwrap_or(rhs);
    if !rhs.starts_with("func(") && !rhs.starts_with("func ") {
        return None;
    }
    let name = lhs.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some(name.to_string())
}
