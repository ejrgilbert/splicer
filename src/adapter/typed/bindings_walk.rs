//! Walk a wit-bindgen-generated bindings source with `syn` to
//! extract the shapes downstream emission consumes: every `Guest`
//! trait declaration (with its methods) and every named type
//! (struct or enum) the bindings define.

use anyhow::{Context, Result};
use syn::{Item, ItemEnum, ItemMod, ItemStruct, ItemTrait, TraitItem, TraitItemFn};

/// What [`walk_bindings`] extracts from a bindings source file.
#[derive(Default)]
pub struct WrapperBindings {
    /// Every `Guest` trait found, keyed by the module path it was
    /// declared in. wit-bindgen emits one `Guest` per exported
    /// interface, nested under `exports::<package>::<interface>`.
    pub guest_traits: Vec<GuestTrait>,
    /// Every named struct or enum the bindings define.
    pub types: Vec<TypeDef>,
}

/// A `Guest` trait declaration the wrapper must implement.
pub struct GuestTrait {
    /// Module path the trait was declared in, e.g.
    /// `["exports", "test", "demo", "ops"]`.
    pub module_path: Vec<String>,
    /// Each method declared in the trait — one per wrapped function.
    pub methods: Vec<GuestMethod>,
}

/// One method on a `Guest` trait — i.e., one wrapped function.
pub struct GuestMethod {
    pub ident: syn::Ident,
    pub sig: syn::Signature,
}

/// A type the bindings defined.
pub struct TypeDef {
    /// Module path the type was declared in.
    pub module_path: Vec<String>,
    pub kind: TypeDefKind,
}

pub enum TypeDefKind {
    Struct(ItemStruct),
    Enum(ItemEnum),
}

/// Parse the bindings source and extract the [`WrapperBindings`]
/// shape needed for downstream emission.
pub fn walk_bindings(src: &str) -> Result<WrapperBindings> {
    let file = syn::parse_file(src).context("failed to parse bindings as Rust source")?;
    let mut out = WrapperBindings::default();
    walk_items(&file.items, &mut Vec::new(), &mut out);
    Ok(out)
}

fn walk_items(items: &[Item], path: &mut Vec<String>, out: &mut WrapperBindings) {
    for item in items {
        match item {
            Item::Mod(ItemMod {
                ident,
                content: Some((_, inner_items)),
                ..
            }) => {
                path.push(ident.to_string());
                walk_items(inner_items, path, out);
                path.pop();
            }
            Item::Trait(t) if is_guest_trait(t) => {
                out.guest_traits.push(GuestTrait {
                    module_path: path.clone(),
                    methods: trait_methods(t),
                });
            }
            Item::Struct(s) => {
                out.types.push(TypeDef {
                    module_path: path.clone(),
                    kind: TypeDefKind::Struct(s.clone()),
                });
            }
            Item::Enum(e) => {
                out.types.push(TypeDef {
                    module_path: path.clone(),
                    kind: TypeDefKind::Enum(e.clone()),
                });
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
            get: func(name: string) -> outcome;
        }

        world rich {
            export store;
        }
    "#;

    #[test]
    fn extracts_guest_trait_with_one_method() {
        let src = run_wit_bindgen_rust(TINY_WIT, Some("demo")).unwrap();
        let bindings = walk_bindings(&src).unwrap();
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
    fn extracts_types_for_richer_wit() {
        let src = run_wit_bindgen_rust(RICHER_WIT, Some("rich")).unwrap();
        let bindings = walk_bindings(&src).unwrap();

        // Should find the `Entry` record (struct) and `Outcome` variant (enum).
        let type_names: Vec<String> = bindings
            .types
            .iter()
            .map(|t| match &t.kind {
                TypeDefKind::Struct(s) => s.ident.to_string(),
                TypeDefKind::Enum(e) => e.ident.to_string(),
            })
            .collect();
        assert!(
            type_names.iter().any(|n| n == "Entry"),
            "expected struct Entry; got types: {type_names:?}"
        );
        assert!(
            type_names.iter().any(|n| n == "Outcome"),
            "expected enum Outcome; got types: {type_names:?}"
        );

        // And exactly one Guest trait for the `store` interface.
        assert_eq!(bindings.guest_traits.len(), 1);
        assert_eq!(
            bindings.guest_traits[0]
                .module_path
                .last()
                .map(String::as_str),
            Some("store")
        );
    }

    #[test]
    fn no_guest_trait_when_source_has_none() {
        // walk_bindings makes no judgment about which structs are
        // wit-bindgen-emitted vs hand-written; it captures every
        // top-level/mod-nested struct or enum it sees. Documented
        // here as a contract: callers feed bindings-shaped input.
        let src = r#"
            pub fn unrelated() {}
            pub mod thing {
                pub struct NotAGuest;
            }
        "#;
        let bindings = walk_bindings(src).unwrap();
        assert!(bindings.guest_traits.is_empty());
        assert_eq!(bindings.types.len(), 1);
    }

    #[test]
    fn parse_error_surfaces() {
        match walk_bindings("this is { not valid rust") {
            Ok(_) => panic!("expected a parse error"),
            Err(e) => assert!(e.to_string().contains("parse")),
        }
    }
}
