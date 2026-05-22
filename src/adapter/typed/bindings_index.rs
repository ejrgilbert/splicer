//! Syn walk over wit-bindgen output: for every WIT type, record which
//! Rust shape wit-bindgen emitted (struct / enum / `bitflags!`).
//! Also extracts the `Guest` trait declarations the wrapper must
//! implement.
//!
//! The IR builder probes the index by `(module_path, PascalIdent)`;
//! a miss surfaces a loud error rather than silently miscategorizing.

use std::collections::HashMap;

use anyhow::{Context, Result};
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{Item, ItemMacro, ItemMod, ItemTrait, TraitItem, TraitItemFn};

/// Module-path segments inside the generated `bindings` module —
/// the namespace, *not* the item. E.g. `["exports", "test", "demo",
/// "ops"]` is the path to a module containing items like `Point`,
/// `Color`, etc.
pub type BindingsPath = Vec<String>;

/// Render `bindings::<seg1>::<seg2>::…[::<ident>]` from a path slice
/// and an optional terminal ident. E.g.:
/// - `(["exports","pkg","ops"], Some(Point))` → `bindings::exports::pkg::ops::Point`
/// - `(["pkg","ops"], None)` → `bindings::pkg::ops`
/// - `([], Some(Foo))` → `bindings::Foo`
pub fn bindings_path_tokens(path: &[String], ident: Option<&syn::Ident>) -> TokenStream {
    let segs: Vec<syn::Ident> = path
        .iter()
        .map(|s| syn::Ident::new(s, Span::call_site()))
        .collect();
    match (segs.as_slice(), ident) {
        ([], Some(id)) => quote!(bindings::#id),
        ([], None) => quote!(bindings),
        (segs, Some(id)) => quote!(bindings::#(#segs)::*::#id),
        (segs, None) => quote!(bindings::#(#segs)::*),
    }
}

pub struct WrapperBindings {
    pub index: BindingsIndex,
    pub guest_traits: Vec<GuestTrait>,
}

/// A `Guest` trait the wrapper must implement.
pub struct GuestTrait {
    pub module_path: BindingsPath,
    pub methods: Vec<GuestMethod>,
}

pub struct GuestMethod {
    pub ident: syn::Ident,
    pub sig: syn::Signature,
}

/// What Rust shape wit-bindgen emitted for a given WIT type.
#[derive(Copy, Clone)]
pub enum BindingsItem {
    Struct,
    Enum,
    BitflagsMacro,
}

/// `(module path, item name) → shape wit-bindgen emitted`.
#[derive(Default)]
pub struct BindingsIndex {
    by_name: HashMap<(BindingsPath, String), BindingsItem>,
}

impl BindingsIndex {
    /// Look up an item by its containing module path and Rust ident.
    /// e.g.`bindings::exports::pkg::ops::Point` --> (`bindings::exports::pkg::ops`, "Point")
    pub fn get(&self, path: &[String], ident: &str) -> Option<&BindingsItem> {
        self.by_name.get(&(path.to_vec(), ident.to_string()))
    }
}

/// Parse the bindings source and produce a [`WrapperBindings`].
pub fn build_bindings_index(src: &str) -> Result<WrapperBindings> {
    let file = syn::parse_file(src).context("failed to parse bindings as Rust source")?;
    let mut index = BindingsIndex::default();
    let mut guest_traits = Vec::new();
    walk_items(&file.items, &mut Vec::new(), &mut index, &mut guest_traits);
    Ok(WrapperBindings {
        index,
        guest_traits,
    })
}

fn walk_items(
    items: &[Item],
    path: &mut Vec<String>,
    index: &mut BindingsIndex,
    guests: &mut Vec<GuestTrait>,
) {
    for item in items {
        match item {
            Item::Mod(ItemMod {
                ident,
                content: Some((_, inner_items)),
                ..
            }) => {
                path.push(ident.to_string());
                walk_items(inner_items, path, index, guests);
                path.pop();
            }
            Item::Trait(t) if is_guest_trait(t) => {
                guests.push(GuestTrait {
                    module_path: path.clone(),
                    methods: trait_methods(t),
                });
            }
            Item::Struct(s) => {
                if matches!(s.fields, syn::Fields::Named(_)) {
                    let key = (path.clone(), s.ident.to_string());
                    index.by_name.insert(key, BindingsItem::Struct);
                }
            }
            Item::Enum(e) => {
                let key = (path.clone(), e.ident.to_string());
                index.by_name.insert(key, BindingsItem::Enum);
            }
            Item::Macro(m) => {
                let ends_in_bitflags = m
                    .mac
                    .path
                    .segments
                    .last()
                    .map(|s| s.ident == "bitflags")
                    .unwrap_or(false);
                if ends_in_bitflags {
                    if let Some(ident) = bitflags_struct_ident(m) {
                        let key = (path.clone(), ident);
                        index.by_name.insert(key, BindingsItem::BitflagsMacro);
                    }
                }
            }
            _ => {}
        }
    }
}

fn is_guest_trait(t: &ItemTrait) -> bool {
    t.ident == "Guest"
}

fn trait_methods(t: &ItemTrait) -> Vec<GuestMethod> {
    t.items
        .iter()
        .filter_map(|i| match i {
            TraitItem::Fn(TraitItemFn { sig, .. }) => Some(GuestMethod {
                ident: sig.ident.clone(),
                sig: sig.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// Recover the struct ident from a `bitflags! { ... }` macro body.
///
/// Matches the shape `[attrs] [vis] struct <Ident>: <ReprTy> { const … }`.
fn bitflags_struct_ident(m: &ItemMacro) -> Option<String> {
    use syn::parse::Parser;
    use syn::{Attribute, Ident, Token, Type};

    let tokens = m.mac.tokens.clone();
    let parser = |input: syn::parse::ParseStream| {
        input.call(Attribute::parse_outer)?;
        input.parse::<syn::Visibility>()?;
        input.parse::<Token![struct]>()?;
        let ident: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let _: Type = input.parse()?;
        // Drain the body so the parser reaches EOF — `Parser::parse2`
        // errors on leftover tokens, and we only need the struct ident.
        let body;
        syn::braced!(body in input);
        while !body.is_empty() {
            body.parse::<proc_macro2::TokenTree>()?;
        }
        Ok(ident)
    };
    parser.parse2(tokens).ok().map(|i| i.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::typed::bindgen::run_wit_bindgen_rust;

    const TINY_WIT: &str = r#"
        package test:demo@0.1.0;

        interface ops {
            add: func(a: u32, b: u32) -> u32;
        }

        world demo {
            export ops;
        }
    "#;

    const RICHER_WIT: &str = r#"
        package test:rich@0.1.0;

        interface store {
            record entry {
                key: string,
                value: list<u8>,
            }
            variant outcome {
                hit(entry),
                miss,
            }
            flags perms { read, write }
            get: func(name: string) -> outcome;
        }

        world rich {
            export store;
        }
    "#;

    #[test]
    fn extracts_guest_trait_with_one_method() {
        let (_, _, src) = run_wit_bindgen_rust(TINY_WIT, Some("demo")).unwrap();
        let bindings = build_bindings_index(&src).unwrap();
        assert_eq!(bindings.guest_traits.len(), 1);
        let g = &bindings.guest_traits[0];
        let methods: Vec<String> = g.methods.iter().map(|m| m.ident.to_string()).collect();
        assert!(
            methods.iter().any(|s| s == "add"),
            "Guest methods: {methods:?}"
        );
        assert_eq!(g.module_path.last().map(String::as_str), Some("ops"));
    }

    #[test]
    fn indexes_record_variant_and_flags() {
        let (_, _, src) = run_wit_bindgen_rust(RICHER_WIT, Some("rich")).unwrap();
        let bindings = build_bindings_index(&src).unwrap();

        let store_path = bindings
            .guest_traits
            .first()
            .expect("Guest trait present")
            .module_path
            .clone();

        assert!(matches!(
            bindings.index.get(&store_path, "Entry"),
            Some(BindingsItem::Struct)
        ));
        assert!(matches!(
            bindings.index.get(&store_path, "Outcome"),
            Some(BindingsItem::Enum)
        ));
        assert!(matches!(
            bindings.index.get(&store_path, "Perms"),
            Some(BindingsItem::BitflagsMacro)
        ));
    }

    #[test]
    fn unknown_keys_miss() {
        let (_, _, src) = run_wit_bindgen_rust(TINY_WIT, Some("demo")).unwrap();
        let bindings = build_bindings_index(&src).unwrap();
        assert!(bindings.index.get(&[], "NoSuchThing").is_none());
        assert!(bindings
            .index
            .get(&["nonexistent".to_string()], "Whatever")
            .is_none());
    }

    #[test]
    fn no_guest_trait_when_source_has_none() {
        // The walker accepts any Rust source and indexes whatever
        // structs/enums/`bitflags!` it finds; it doesn't validate
        // that the input came from wit-bindgen.
        let bindings = build_bindings_index(
            r#"
            pub fn unrelated() {}
            pub mod thing {
                pub struct NotAGuest { x: u32 }
            }
        "#,
        )
        .unwrap();
        assert!(bindings.guest_traits.is_empty());
        assert!(matches!(
            bindings.index.get(&["thing".to_string()], "NotAGuest"),
            Some(BindingsItem::Struct)
        ));
    }

    #[test]
    fn parse_error_surfaces() {
        match build_bindings_index("this is { not valid rust") {
            Ok(_) => panic!("expected a parse error"),
            Err(e) => assert!(e.to_string().contains("parse")),
        }
    }
}
