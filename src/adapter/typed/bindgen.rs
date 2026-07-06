//! Run wit-bindgen-rust in-process to produce the bindings source
//! for a target WIT. The bindings get parsed downstream (with `syn`)
//! to discover types and function signatures for codegen.

use anyhow::{anyhow, Context, Result};
use heck::ToUpperCamelCase;
use std::collections::HashSet;
use wit_bindgen_core::{Files, WorldGenerator};
use wit_parser::{Resolve, WorldId, WorldItem};

/// Run wit-bindgen-rust against the given WIT text.
///
/// Returns the parsed [`Resolve`], the [`WorldId`] the bindings were
/// generated for, and the Rust source. Keeping the [`Resolve`] lets
/// downstream walks read WIT semantics directly rather than inferring
/// them from the emitted Rust.
///
/// `world_name` selects which world. Pass `None` if the WIT contains
/// exactly one world.
pub fn run_wit_bindgen_rust(
    wit_text: &str,
    world_name: Option<&str>,
) -> Result<(Resolve, WorldId, String)> {
    let mut resolve = Resolve::new();
    // The label appears in wit-parser diagnostics if parsing fails;
    // there's no real file backing this call.
    let pkg_id = resolve
        .push_str("<in-memory>", wit_text)
        .context("failed to parse input WIT")?;
    let world_id =
        resolve
            .select_world(&[pkg_id], world_name)
            .with_context(|| match world_name {
                Some(n) => format!("could not select world '{n}'"),
                None => {
                    "could not select default world (WIT contains multiple worlds?)".to_string()
                }
            })?;

    let mut generator = wit_bindgen_rust::Opts {
        generate_all: true,
        ..Default::default()
    }
    .build();
    let mut files = Files::default();
    generator
        .generate(&mut resolve, world_id, &mut files)
        .context("wit-bindgen-rust generation failed")?;

    let rs_files: Vec<_> = files
        .iter()
        .filter(|(name, _)| name.ends_with(".rs"))
        .collect();
    let bytes = match rs_files.as_slice() {
        [(_, bytes)] => *bytes,
        [] => return Err(anyhow!("wit-bindgen-rust produced no .rs output")),
        many => {
            let names: Vec<&str> = many.iter().map(|(name, _)| *name).collect();
            return Err(anyhow!(
                "wit-bindgen-rust produced {} .rs files; expected exactly one: {names:?}",
                many.len()
            ));
        }
    };
    let source = std::str::from_utf8(bytes)
        .map(String::from)
        .context("wit-bindgen-rust output is not UTF-8")?;
    Ok((resolve, world_id, source))
}

/// When a WIT world both imports and exports the same interface (as tier-3
/// wrappers do), wit-bindgen generates two independent Rust type definitions —
/// one in the import module and one under `exports::`. Passing a value from
/// one side to the other fails to compile because Rust sees two distinct types.
///
/// This function post-processes the generated source to replace export-side
/// named-type definitions (`struct`, `enum`, bitflags macros) with `pub type`
/// aliases that point back to the import-side definition, making both sides
/// the same Rust type. All other items in the export module (the `Guest`
/// trait, the `_export_*_cabi` unsafe fns, the export macro) are left intact.
pub fn alias_shared_export_types(
    src: &str,
    resolve: &Resolve,
    world_id: WorldId,
) -> Result<String> {
    use super::ir::module_path_for_interface;

    let world = &resolve.worlds[world_id];

    // Match interfaces by qualified name, not by InterfaceId. The same
    // logical interface (e.g. `my:benchmark/downstream`) can be assigned
    // two different IDs in a single Resolve when a multi-package WIT text
    // is pushed (the import reference and the export reference may each
    // resolve against a different entry). Comparing IDs would miss the
    // match; comparing qualified names is stable.
    let import_names: HashSet<String> = world
        .imports
        .values()
        .filter_map(|item| match item {
            WorldItem::Interface { id, .. } => resolve.id_of(*id),
            _ => None,
        })
        .collect();

    struct SharedIface {
        export_path: Vec<String>,
        import_path: Vec<String>,
        type_names: HashSet<String>,
    }

    let mut shared: Vec<SharedIface> = Vec::new();
    for item in world.exports.values() {
        let WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let Some(export_qname) = resolve.id_of(*id) else {
            continue;
        };
        if !import_names.contains(&export_qname) {
            continue;
        }
        // Find the import-side InterfaceId that has this qualified name
        // so we can get its module path (they may differ from the export ID).
        let import_id = world
            .imports
            .values()
            .find_map(|item| match item {
                WorldItem::Interface { id: iid, .. }
                    if resolve.id_of(*iid).as_deref() == Some(&export_qname) =>
                {
                    Some(*iid)
                }
                _ => None,
            })
            .unwrap_or(*id);
        let type_names: HashSet<String> = resolve.interfaces[*id]
            .types
            .keys()
            .map(|n| n.to_upper_camel_case())
            .collect();
        if type_names.is_empty() {
            continue;
        }
        shared.push(SharedIface {
            export_path: module_path_for_interface(resolve, *id, true)?,
            import_path: module_path_for_interface(resolve, import_id, false)?,
            type_names,
        });
    }

    if shared.is_empty() {
        return Ok(src.to_string());
    }

    let mut file: syn::File =
        syn::parse_str(src).with_context(|| "failed to parse generated bindings as Rust")?;

    for info in &shared {
        replace_types_in_module(
            &mut file.items,
            &info.export_path,
            &info.type_names,
            info.export_path.len(),
            &info.import_path,
        );
    }

    Ok(prettyplease::unparse(&file))
}

/// Recursively walk the nested `mod` tree following `remaining_path`.
/// At the leaf module, replace named-type definitions with `pub type` aliases
/// and remove the now-redundant `impl Debug` blocks.
fn replace_types_in_module(
    items: &mut Vec<syn::Item>,
    remaining_path: &[String],
    type_names: &HashSet<String>,
    num_supers: usize,
    import_path: &[String],
) {
    if remaining_path.is_empty() {
        let mut i = 0;
        while i < items.len() {
            if let Some(alias) = try_make_alias(&items[i], type_names, num_supers, import_path) {
                items[i] = alias;
                i += 1;
            } else if is_debug_impl_for_named_type(&items[i], type_names) {
                // Remove the hand-rolled Debug impl; type aliases inherit
                // Debug from the import-side definition automatically.
                items.remove(i);
            } else {
                i += 1;
            }
        }
        return;
    }

    let target = remaining_path[0].as_str();
    for item in items.iter_mut() {
        if let syn::Item::Mod(m) = item {
            if m.ident == target {
                if let Some((_, ref mut content)) = m.content {
                    replace_types_in_module(
                        content,
                        &remaining_path[1..],
                        type_names,
                        num_supers,
                        import_path,
                    );
                }
            }
        }
    }
}

/// If `item` is a named-type definition (struct / enum / bitflags macro) whose
/// Rust ident is in `type_names`, return a `pub type Name = super::…::Name;`
/// alias item. Returns `None` for everything else.
fn try_make_alias(
    item: &syn::Item,
    type_names: &HashSet<String>,
    num_supers: usize,
    import_path: &[String],
) -> Option<syn::Item> {
    let name = named_type_ident(item, type_names)?;
    let path: Vec<&str> = std::iter::repeat_n("super", num_supers)
        .chain(import_path.iter().map(String::as_str))
        .collect();
    let src = format!("pub type {name} = {}::{name};", path.join("::"));
    Some(syn::parse_str::<syn::Item>(&src).expect("generated type alias must parse"))
}

/// Return the Rust ident of `item` if it is a named-type definition (struct,
/// enum, or `bitflags!` macro) whose ident is in `type_names`.
fn named_type_ident<'a>(item: &syn::Item, type_names: &'a HashSet<String>) -> Option<&'a str> {
    let name = match item {
        syn::Item::Struct(s) => s.ident.to_string(),
        syn::Item::Enum(e) => e.ident.to_string(),
        syn::Item::Macro(m) => bitflags_type_name(m)?,
        _ => return None,
    };
    type_names.get(&name).map(String::as_str)
}

/// True when `item` is `impl <…::>Debug for TypeName` and TypeName ∈ `type_names`.
fn is_debug_impl_for_named_type(item: &syn::Item, type_names: &HashSet<String>) -> bool {
    let syn::Item::Impl(imp) = item else {
        return false;
    };
    let Some((_, trait_path, _)) = &imp.trait_ else {
        return false;
    };
    let Some(last) = trait_path.segments.last() else {
        return false;
    };
    if last.ident != "Debug" {
        return false;
    }
    let syn::Type::Path(ty) = &*imp.self_ty else {
        return false;
    };
    ty.path
        .segments
        .last()
        .is_some_and(|s| type_names.contains(&s.ident.to_string()))
}

/// Extract the type name from a `bitflags!` macro invocation by scanning the
/// flat token stream for a `struct <Ident>` pair.
fn bitflags_type_name(mac: &syn::ItemMacro) -> Option<String> {
    if mac.mac.path.segments.last()?.ident != "bitflags" {
        return None;
    }
    let tokens: Vec<proc_macro2::TokenTree> = mac.mac.tokens.clone().into_iter().collect();
    tokens.windows(2).find_map(|w| {
        if let (proc_macro2::TokenTree::Ident(kw), proc_macro2::TokenTree::Ident(name)) =
            (&w[0], &w[1])
        {
            (kw == "struct").then(|| name.to_string())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_WIT: &str = r#"
        package test:demo@0.1.0;

        interface ops {
            add: func(a: u32, b: u32) -> u32;
        }

        world demo {
            export ops;
        }
    "#;

    #[test]
    fn generates_bindings_for_tiny_world() {
        let (_resolve, _world, src) = run_wit_bindgen_rust(TINY_WIT, Some("demo")).unwrap();
        // Sanity-check the output looks like wit-bindgen-rust bindings.
        assert!(
            src.contains("pub trait Guest"),
            "expected a Guest trait in bindings; got:\n{src}"
        );
        assert!(
            src.contains("fn add"),
            "expected the `add` function in bindings; got:\n{src}"
        );
    }

    #[test]
    fn errors_on_unknown_world() {
        let err = run_wit_bindgen_rust(TINY_WIT, Some("does-not-exist")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does-not-exist"),
            "error should mention the missing world; got: {msg}"
        );
    }

    #[test]
    fn errors_on_invalid_wit() {
        let err = run_wit_bindgen_rust("this is not valid wit", None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("wit"),
            "error should mention WIT parsing; got: {msg}"
        );
    }
}
