//! Syn walk over wit-bindgen output: for every WIT type, record which
//! Rust shape wit-bindgen emitted (struct / enum / `bitflags!`).
//! Also extracts the `Guest` trait declarations the wrapper must
//! implement.
//!
//! The IR builder probes the index by `(module_path, PascalIdent)`;
//! a miss surfaces a loud error rather than silently miscategorizing.

use std::collections::HashMap;

use anyhow::{Context, Result};
use heck::ToUpperCamelCase;
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

/// A `Guest`-flavored trait the wrapper must implement: either the
/// interface-level `Guest` (covers freestanding fns + a `type Bucket
/// = …;` line per resource) or a per-resource `GuestBucket` (covers
/// constructor + methods + statics of one resource).
pub struct GuestTrait {
    pub module_path: BindingsPath,
    /// Trait ident verbatim from wit-bindgen (`Guest` or
    /// `Guest<ResourcePascal>`)
    pub trait_ident: syn::Ident,
    pub kind: GuestTraitKind,
    pub methods: Vec<GuestMethod>,
}

pub enum GuestTraitKind {
    /// `pub trait Guest { ... }`.
    Interface,
    /// `pub trait Guest<R> { ... }`; the ident is just the resource
    /// PascalCase (`Bucket`), used to derive `WrapperBucket` and
    /// `type Bucket = WrapperBucket;`.
    Resource(syn::Ident),
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
            Item::Trait(t) => {
                if let Some(kind) = classify_guest_trait(t) {
                    guests.push(GuestTrait {
                        module_path: path.clone(),
                        trait_ident: t.ident.clone(),
                        kind,
                        methods: trait_methods(t),
                    });
                }
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

/// Returns `Some(kind)` for `Guest` and for `Guest<ResourcePascal>`
/// traits; `None` for everything else. The PascalCase suffix carries
/// the resource's wit-bindgen ident, matching the `type Bucket = ...;`
/// associated type in the interface-level `Guest` trait.
fn classify_guest_trait(t: &ItemTrait) -> Option<GuestTraitKind> {
    let ident = t.ident.to_string();
    if ident == "Guest" {
        return Some(GuestTraitKind::Interface);
    }
    let suffix = ident.strip_prefix("Guest").filter(|s| !s.is_empty())?;
    // wit-bindgen names per-resource traits as `Guest<PascalCase>`;
    // round-trip through heck to confirm the suffix matches the form
    // (rejects accidental matches like a user-defined `GuestX_y`).
    if suffix.to_upper_camel_case() != suffix {
        return None;
    }
    Some(GuestTraitKind::Resource(syn::Ident::new(
        suffix,
        Span::call_site(),
    )))
}

fn trait_methods(t: &ItemTrait) -> Vec<GuestMethod> {
    t.items
        .iter()
        .filter_map(|i| match i {
            // Skip provided (default-bodied) methods like wit-bindgen's
            // hidden `_resource_new` / `_resource_rep` on a
            // `GuestBucket` trait — only the required methods are
            // user-facing.
            TraitItem::Fn(TraitItemFn {
                sig, default: None, ..
            }) => Some(GuestMethod {
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
        assert!(matches!(g.kind, GuestTraitKind::Interface));
        let methods: Vec<String> = g.methods.iter().map(|m| m.ident.to_string()).collect();
        assert!(
            methods.iter().any(|s| s == "add"),
            "Guest methods: {methods:?}"
        );
        assert_eq!(g.module_path.last().map(String::as_str), Some("ops"));
    }

    const RESOURCE_WIT: &str = r#"
        package test:resz@0.1.0;
        interface store {
            resource bucket {
                constructor(name: string);
                get: func(key: string) -> option<list<u8>>;
                put: func(key: string, val: list<u8>);
            }
            open: func(name: string) -> bucket;
        }
        world w { export store; }
    "#;

    #[test]
    fn extracts_interface_and_per_resource_guest_traits() {
        let (_, _, src) = run_wit_bindgen_rust(RESOURCE_WIT, Some("w")).unwrap();
        let bindings = build_bindings_index(&src).unwrap();
        // Two Guest traits: the interface-level one (with `open`) and
        // the per-resource `GuestBucket`.
        let iface = bindings
            .guest_traits
            .iter()
            .find(|g| matches!(g.kind, GuestTraitKind::Interface))
            .expect("interface-level Guest");
        let iface_method_names: Vec<String> =
            iface.methods.iter().map(|m| m.ident.to_string()).collect();
        assert!(
            iface_method_names.iter().any(|s| s == "open"),
            "interface Guest methods: {iface_method_names:?}"
        );

        let bucket = bindings
            .guest_traits
            .iter()
            .find(|g| matches!(&g.kind, GuestTraitKind::Resource(id) if id == "Bucket"))
            .expect("GuestBucket trait");
        let bucket_method_names: Vec<String> =
            bucket.methods.iter().map(|m| m.ident.to_string()).collect();
        for required in ["new", "get", "put"] {
            assert!(
                bucket_method_names.iter().any(|s| s == required),
                "expected `{required}` on GuestBucket; got: {bucket_method_names:?}"
            );
        }
        // Hidden helpers (`_resource_new`, `_resource_rep`) ship
        // with default impls; the trait_methods filter should skip
        // them so the wrapper isn't asked to re-implement them.
        assert!(
            !bucket_method_names.iter().any(|s| s.starts_with('_')),
            "default-bodied `_resource_*` methods should be filtered: {bucket_method_names:?}"
        );
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
