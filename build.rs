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
//!
//! Additionally, asserts that every typedef / record-field /
//! function-param name that the Rust adapter codegen string-matches
//! against actually exists in the corresponding WIT file. A WIT
//! rename without a matching Rust update fails the build here,
//! instead of producing a runtime panic at adapter generation.

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

    validate_schema_names(wit_dir);

    fs::write(&dest, &generated).unwrap();

    generate_builtin_manifest(&out_dir);
}

/// Scan `builtins/<name>/Cargo.toml` for every builtin crate and emit
/// a slice expression `[(name, version), ...]` that `src/builtins.rs`
/// `include!`s as the registry of builtin name → published version.
/// Single source of truth: each builtin's own Cargo.toml.
fn generate_builtin_manifest(out_dir: &str) {
    let dest = Path::new(out_dir).join("builtin_manifest.rs");
    let builtins_dir = Path::new("builtins");

    // Watch the directory itself so cargo reruns when crates are
    // added/removed; per-Cargo.toml lines below catch content changes.
    println!("cargo::rerun-if-changed=builtins");

    let mut rows = String::new();
    if builtins_dir.is_dir() {
        let mut crate_dirs: Vec<_> = fs::read_dir(builtins_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .collect();
        crate_dirs.sort_by_key(|e| e.file_name());

        for entry in crate_dirs {
            let cargo_toml = entry.path().join("Cargo.toml");
            if !cargo_toml.exists() {
                continue;
            }
            println!("cargo::rerun-if-changed={}", cargo_toml.display());
            let src = fs::read_to_string(&cargo_toml).unwrap_or_else(|e| {
                panic!("Failed to read {}: {e}", cargo_toml.display());
            });
            let version = parse_cargo_package_version(&src, &cargo_toml);
            // The directory name is what users put in yaml and what the
            // publish workflow uses as the OCI path component, so it's
            // the canonical "builtin name" — not Cargo.toml's `name`,
            // even though they match by convention today.
            let name = entry.file_name().to_string_lossy().into_owned();
            rows.push_str(&format!("    (\"{name}\", \"{version}\"),\n"));
        }
    }

    let content = format!(
        "// Auto-generated by build.rs from builtins/*/Cargo.toml. Do not edit.\n&[\n{rows}]\n"
    );
    fs::write(&dest, content).unwrap();
}

/// Extract `version = "..."` from inside the `[package]` section of a
/// Cargo.toml file. Line-based to keep build.rs free of a `toml` dep.
/// Tolerant of a trailing `# comment` after the value.
fn parse_cargo_package_version(src: &str, path: &Path) -> String {
    let mut in_package = false;
    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        // Look for `version = "..."` only — `version.workspace = true`
        // and other dotted variants are intentionally rejected by the
        // requirement that the next non-space token after `version` be `=`.
        let Some(rest) = trimmed.strip_prefix("version") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            panic!(
                "[package] version in {} is not a quoted string: {trimmed:?}",
                path.display()
            );
        };
        let Some(end) = rest.find('"') else {
            panic!(
                "[package] version in {} has unterminated quote: {trimmed:?}",
                path.display()
            );
        };
        let v = &rest[..end];
        if v.is_empty() {
            panic!("Empty version in [package] of {}", path.display());
        }
        return v.to_string();
    }
    panic!(
        "No `version = \"...\"` found in [package] of {}",
        path.display()
    );
}

/// Asserts that every WIT typedef / record-field / function-param
/// that `src/adapter/tier2/emit.rs` looks up by string actually
/// exists in the WIT files. A WIT rename without a matching Rust
/// update fails here at build time.
///
/// The expected names below MUST stay in sync with the `TYPEDEF_*` /
/// `FIELD_*` / `TREE_*` / `CALLID_*` / `ON_*` constants declared in
/// `src/adapter/tier2/emit.rs`. The mirroring isn't ideal — but it
/// turns a runtime panic ("no field named …") into a compile-time
/// failure at the file that owns the schema.
fn validate_schema_names(wit_dir: &Path) {
    let common_path = wit_dir.join("common").join("world.wit");
    if !common_path.exists() {
        // Nothing to validate. The runtime `include_str!` will fail
        // anyway if this file is genuinely missing.
        return;
    }
    let common_src = fs::read_to_string(&common_path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", common_path.display()));

    // (typedef-name, expected-fields). Mirrors `TYPEDEF_*` and the
    // `FIELD_*` / `TREE_*` / `CALLID_*` field constants in
    // `src/adapter/tier2/emit.rs`. Empty `fields` means typedef-only.
    let common_records: &[(&str, &[&str])] = &[
        ("field", &["name", "tree"]),
        (
            "field-tree",
            &[
                "cells",
                "record-infos",
                "flags-infos",
                "enum-infos",
                "variant-infos",
                "handle-infos",
                "root",
            ],
        ),
        ("call-id", &["interface-name", "function-name"]),
        ("enum-info", &["type-name", "case-name"]),
        ("record-info", &["type-name", "fields"]),
    ];
    for (name, fields) in common_records {
        require_record_with_fields(&common_src, &common_path, name, fields);
    }
    // `cell` is a variant; this item only requires its existence.
    // The 18 case discriminants are pinned by an ordering test in
    // `tier2/cells.rs` and are tracked as a separate audit item.
    require_typedef(&common_src, &common_path, "variant", "cell");

    let tier2_path = wit_dir.join("tier2").join("world.wit");
    if !tier2_path.exists() {
        return;
    }
    let tier2_src = fs::read_to_string(&tier2_path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", tier2_path.display()));
    // Mirrors `ON_CALL_*` / `ON_RET_*` in `src/adapter/tier2/emit.rs`.
    // `result` is a WIT keyword and shows up as `%result` in the WIT
    // source; the validator strips the `%` before comparison.
    require_func_params(&tier2_src, &tier2_path, "on-call", &["call", "args"]);
    require_func_params(&tier2_src, &tier2_path, "on-return", &["call", "result"]);
}

fn require_typedef(src: &str, path: &Path, kind: &str, name: &str) {
    if extract_typedef_body(src, kind, name).is_none() {
        panic!(
            "Schema mismatch: `{kind} {name}` not found in {}.\n\
             The Rust adapter codegen (src/adapter/tier2/emit.rs) references this typedef.\n\
             Either restore the WIT typedef, or update the constants in emit.rs and build.rs.",
            path.display()
        );
    }
}

fn require_record_with_fields(src: &str, path: &Path, name: &str, fields: &[&str]) {
    let body = extract_typedef_body(src, "record", name).unwrap_or_else(|| {
        panic!(
            "Schema mismatch: `record {name}` not found in {}.\n\
             The Rust adapter codegen (src/adapter/tier2/emit.rs) references this typedef.\n\
             Either restore the WIT typedef, or update the constants in emit.rs and build.rs.",
            path.display()
        )
    });
    for field in fields {
        if !record_body_has_field(&body, field) {
            panic!(
                "Schema mismatch: `record {name}` in {} is missing field `{field}`.\n\
                 The Rust adapter codegen (src/adapter/tier2/emit.rs) references this field.\n\
                 Either restore the WIT field, or update the constants in emit.rs and build.rs.",
                path.display()
            );
        }
    }
}

fn require_func_params(src: &str, path: &Path, fn_name: &str, params: &[&str]) {
    let decl = extract_func_decl(src, fn_name).unwrap_or_else(|| {
        panic!(
            "Schema mismatch: function `{fn_name}` not found in {}.\n\
             The Rust adapter codegen (src/adapter/tier2/emit.rs) references it.",
            path.display()
        )
    });
    let param_names = parse_func_param_names(&decl);
    for expected in params {
        let canonical = expected.strip_prefix('%').unwrap_or(expected);
        if !param_names.iter().any(|n| n == canonical) {
            panic!(
                "Schema mismatch: function `{fn_name}` in {} is missing param `{canonical}`. \
                 Found params: {:?}\n\
                 The Rust adapter codegen (src/adapter/tier2/emit.rs) references this param.\n\
                 Either restore the WIT param, or update the constants in emit.rs and build.rs.",
                path.display(),
                param_names,
            );
        }
    }
}

/// Return the body text of a typedef block (everything between the
/// matching `{` and `}`), or `None` if no `<kind> <name> { … }`
/// declaration exists. Brace-depth tracking handles nested types.
fn extract_typedef_body(src: &str, kind: &str, name: &str) -> Option<String> {
    let header_prefix = format!("{kind} {name}");
    let mut depth: i32 = 0;
    let mut body = String::new();
    let mut found = false;
    for line in src.lines() {
        let trimmed = line.trim();
        if !found {
            if let Some(rest) = trimmed.strip_prefix(&header_prefix) {
                // The next non-whitespace character must be `{` for
                // this to be a typedef declaration (vs. e.g. a use of
                // `record-info` as a type reference).
                let rest = rest.trim_start();
                if let Some(after_brace) = rest.strip_prefix('{') {
                    found = true;
                    depth = 1;
                    if !after_brace.trim().is_empty() {
                        body.push_str(after_brace);
                        body.push('\n');
                    }
                }
            }
            continue;
        }
        for ch in trimmed.chars() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        if depth <= 0 {
            // Strip the closing brace and stop. WIT typedef bodies
            // don't put trailing content after `}` on the same line,
            // but be defensive in case future WIT does.
            let close = trimmed.rfind('}').unwrap_or(trimmed.len());
            body.push_str(&trimmed[..close]);
            return Some(body);
        }
        body.push_str(trimmed);
        body.push('\n');
    }
    if found { Some(body) } else { None }
}

/// True iff the typedef body has a line whose first ident-token is
/// `field` (with the leading `%` keyword-escape stripped) followed by
/// a `:`.
fn record_body_has_field(body: &str, field: &str) -> bool {
    for line in body.lines() {
        let trimmed = line.trim();
        let trimmed = trimmed.strip_prefix('%').unwrap_or(trimmed);
        if let Some(rest) = trimmed.strip_prefix(field) {
            // `record_body_has_field("cells", body)` must not match
            // `cells-of:` etc. The next char must be whitespace or `:`.
            let next = rest.chars().next();
            if next == Some(':') || next.map(|c| c.is_whitespace()).unwrap_or(false) {
                let after = rest.trim_start();
                if after.starts_with(':') {
                    return true;
                }
            }
        }
    }
    false
}

/// Find a function declaration line of the form
/// `<fn_name>: [async] func( ... );` and return the trimmed line.
/// Tier-2 hooks fit on one line; multi-line decls aren't supported
/// (extend if a future hook ever spans lines).
fn extract_func_decl(src: &str, fn_name: &str) -> Option<String> {
    for line in src.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(fn_name) else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix(':') else {
            continue;
        };
        let rest = rest.trim_start();
        let rest = rest.strip_prefix("async ").unwrap_or(rest);
        if rest.starts_with("func(") || rest.starts_with("func (") {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn parse_func_param_names(decl: &str) -> Vec<String> {
    let Some(open) = decl.find('(') else {
        return Vec::new();
    };
    let Some(close) = decl[open + 1..].rfind(')') else {
        return Vec::new();
    };
    let inside = &decl[open + 1..open + 1 + close];

    // Split on commas at depth 0 — types like `list<u32>` or
    // `tuple<string, u32>` carry their own commas inside angle
    // brackets that we must not split on.
    let mut depth: i32 = 0;
    let mut params: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in inside.chars() {
        match ch {
            '<' | '(' | '{' => {
                depth += 1;
                current.push(ch);
            }
            '>' | ')' | '}' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                if let Some(n) = parse_one_param_name(&current) {
                    params.push(n);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        if let Some(n) = parse_one_param_name(&current) {
            params.push(n);
        }
    }
    params
}

fn parse_one_param_name(s: &str) -> Option<String> {
    let s = s.trim();
    let (name, _ty) = s.split_once(':')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some(name.strip_prefix('%').unwrap_or(name).to_string())
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
