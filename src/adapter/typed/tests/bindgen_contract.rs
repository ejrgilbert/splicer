//! Contract pins for the wit-bindgen-rust output conventions the IR
//! walker depends on. These tests don't exercise our own code — if
//! wit-bindgen drifts, they fail with the actual emitted source
//! attached so the divergence surfaces before downstream codegen
//! silently breaks.

use heck::{ToSnakeCase, ToUpperCamelCase};
use proc_macro2::TokenTree;
use syn::{parse::Parser, Attribute, Fields, Ident, Item, ItemMacro, ItemStruct, Token};

use crate::adapter::typed::bindgen::run_wit_bindgen_rust;
use crate::adapter::typed::ir::is_rust_keyword;

/// Recurse into nested `Item::Mod`s, tracking the module path. For
/// every non-module item, invoke `visit(path, item)`. The walker
/// itself doesn't filter — leaf filtering is the visitor's job.
fn walk_with_path<'a, F>(items: &'a [Item], path: &mut Vec<String>, visit: &mut F)
where
    F: FnMut(&[String], &'a Item),
{
    for it in items {
        if let Item::Mod(m) = it {
            if let Some((_, inner)) = &m.content {
                path.push(m.ident.to_string());
                walk_with_path(inner, path, visit);
                path.pop();
                continue;
            }
        }
        visit(path, it);
    }
}

const FLAGS_WIT: &str = r#"
    package test:demo@0.1.0;
    interface ops {
        flags perms { read, write, exec }
        check: func(p: perms);
    }
    world demo { export ops; }
"#;

#[test]
fn flags_lower_to_bitflags_macro_with_pascal_struct_and_const_members() {
    let (_resolve, _world, src) = run_wit_bindgen_rust(FLAGS_WIT, Some("demo")).unwrap();
    let file = syn::parse_file(&src).expect("bindings parse as Rust");
    let mut found: Vec<(Vec<String>, &ItemMacro)> = Vec::new();
    walk_with_path(&file.items, &mut Vec::new(), &mut |path, item| {
        if let Item::Macro(m) = item {
            let ends_in_bitflags = m
                .mac
                .path
                .segments
                .last()
                .map(|s| s.ident == "bitflags")
                .unwrap_or(false);
            if ends_in_bitflags {
                found.push((path.to_vec(), m));
            }
        }
    });
    assert!(
        !found.is_empty(),
        "expected at least one bitflags! macro for `flags perms`; got bindings:\n{src}"
    );

    for (path, m) in &found {
        // Expected body: [attrs] [vis] struct <Ident>: <ReprTy>
        //                 { const <FlagIdent> = ...; ... }
        let tokens = m.mac.tokens.clone();
        let parser = |input: syn::parse::ParseStream| {
            input.call(Attribute::parse_outer)?;
            input.parse::<syn::Visibility>()?;
            input.parse::<Token![struct]>()?;
            let ident: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            let _: syn::Type = input.parse()?;
            let content;
            syn::braced!(content in input);
            let mut flags = vec![];
            while !content.is_empty() {
                content.parse::<Token![const]>()?;
                let fi: Ident = content.parse()?;
                flags.push(fi.to_string());
                while !content.peek(Token![;]) && !content.is_empty() {
                    content.parse::<TokenTree>()?;
                }
                content.parse::<Token![;]>()?;
            }
            Ok((ident, flags))
        };
        let (struct_ident, flag_idents) = parser.parse2(tokens).unwrap_or_else(|e| {
            panic!(
                "bitflags! macro body at {path:?} did not match expected shape: {e}\n\
                 full bindings:\n{src}"
            )
        });

        // Macro path ends in `bitflags!`, struct named after the
        // WIT type, member idents in declared order, SHOUTING_SNAKE.
        let segs: Vec<String> = m
            .mac
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        assert_eq!(
            segs.last().map(String::as_str),
            Some("bitflags"),
            "macro path must end in `bitflags`; got {segs:?}"
        );
        assert_eq!(struct_ident, "Perms", "expected struct ident `Perms`");
        assert_eq!(
            flag_idents,
            vec!["READ".to_string(), "WRITE".to_string(), "EXEC".to_string()],
            "expected flag members in declared order, SHOUTING_SNAKE_CASE"
        );
        // PascalCased type idents shouldn't need r#-rawness; if a
        // future wit-bindgen renames collisions to e.g. `Perms_`,
        // trip here rather than miscategorize silently downstream.
        assert!(
            !struct_ident.to_string().starts_with("r#"),
            "did not expect a raw ident; got `{struct_ident}`"
        );

        // Module path lowers kebab segments to snake; `flags` is
        // nested under the iface module.
        assert_eq!(
            path.last().map(String::as_str),
            Some("ops"),
            "expected bitflags inside interface module `ops`; got path {path:?}"
        );
    }
}

const EDGES_WIT: &str = r#"
    package my-pkg:some-ns@0.1.0;
    interface my-iface {
        record my-record {
            my-field: u32,
            other-field: string,
        }
        record %type {
            %loop: u32,
            %match: u32,
            %move: u32,
        }
        do-stuff: func(r: my-record);
        do-typish: func(t: %type);
    }
    world my-world { export my-iface; }
"#;

#[test]
fn idents_mirror_via_heck_with_trailing_underscore_for_keyword_fields() {
    let (_resolve, _world, src) = run_wit_bindgen_rust(EDGES_WIT, Some("my-world")).unwrap();
    let file = syn::parse_file(&src).expect("bindings parse as Rust");
    let mut structs: Vec<(Vec<String>, &ItemStruct)> = Vec::new();
    walk_with_path(&file.items, &mut Vec::new(), &mut |path, item| {
        if let Item::Struct(s) = item {
            if matches!(s.fields, Fields::Named(_)) {
                structs.push((path.to_vec(), s));
            }
        }
    });

    // Type-ident mirroring: kebab WIT name → UpperCamelCase Rust ident.
    for (wit_name, expected_rust) in [("my-record", "MyRecord"), ("type", "Type")] {
        let pascal = wit_name.to_upper_camel_case();
        assert_eq!(
            pascal, expected_rust,
            "heck::ToUpperCamelCase mismatch for {wit_name:?}"
        );
        assert!(
            structs.iter().any(|(_, s)| s.ident == expected_rust),
            "expected struct `{expected_rust}` for WIT type `{wit_name}`; \
             structs present: {:?}\nfull bindings:\n{src}",
            structs
                .iter()
                .map(|(p, s)| format!("{}::{}", p.join("::"), s.ident))
                .collect::<Vec<_>>()
        );
    }

    // Field-ident mirroring: kebab WIT field → snake_case Rust
    // ident. Keyword collisions get a trailing underscore (e.g.
    // `loop_`), NOT `r#` raw-ident prefixes. PascalCased type
    // idents almost never collide so the mangler only fires on
    // fields.
    struct Expect {
        struct_pascal: &'static str,
        wit_field: &'static str,
        rust_field: &'static str,
    }
    let expects = [
        Expect {
            struct_pascal: "MyRecord",
            wit_field: "my-field",
            rust_field: "my_field",
        },
        Expect {
            struct_pascal: "MyRecord",
            wit_field: "other-field",
            rust_field: "other_field",
        },
        Expect {
            struct_pascal: "Type",
            wit_field: "loop",
            rust_field: "loop_",
        },
        Expect {
            struct_pascal: "Type",
            wit_field: "match",
            rust_field: "match_",
        },
        Expect {
            struct_pascal: "Type",
            wit_field: "move",
            rust_field: "move_",
        },
    ];
    for Expect {
        struct_pascal,
        wit_field,
        rust_field,
    } in &expects
    {
        let snake = wit_field.to_snake_case();
        let expected_with_keyword_bump = if is_rust_keyword(&snake) {
            format!("{snake}_")
        } else {
            snake.clone()
        };
        assert_eq!(
            &expected_with_keyword_bump, rust_field,
            "snake_case + keyword-bump mismatch for {wit_field:?}"
        );
        let s = structs
            .iter()
            .find(|(_, s)| s.ident == *struct_pascal)
            .unwrap_or_else(|| panic!("expected struct {struct_pascal} in bindings:\n{src}"))
            .1;
        let found = s.fields.iter().any(|f: &syn::Field| {
            f.ident
                .as_ref()
                .map(|i: &syn::Ident| i.to_string())
                .as_deref()
                == Some(*rust_field)
        });
        assert!(
            found,
            "struct {struct_pascal} missing field `{rust_field}`; got fields {:?}",
            s.fields
                .iter()
                .filter_map(|f: &syn::Field| f.ident.as_ref().map(|i: &syn::Ident| i.to_string()))
                .collect::<Vec<_>>()
        );
    }

    // Kebab package / namespace / iface segments lower to
    // snake_cased Rust module names.
    let module_paths: Vec<String> = structs
        .iter()
        .map(|(p, _): &(Vec<String>, &ItemStruct)| p.join("::"))
        .collect();
    assert!(
        module_paths
            .iter()
            .any(|p| p == "exports::my_pkg::some_ns::my_iface"),
        "expected module path `exports::my_pkg::some_ns::my_iface`; \
         got module paths {module_paths:?}\nfull bindings:\n{src}"
    );
}
