//! Code generation for edge shim components. Each edge shim sits between a consumer
//! (holding T' handles) and a raw collateral interface, unwrapping T' → raw before
//! forwarding the call.

use anyhow::{Context, Result};
use heck::{ToSnakeCase, ToUpperCamelCase};
use proc_macro2::TokenStream;
use quote::quote;
use toml::{map::Map, Value};

use super::target_wit::{EdgeShimWit, BRIDGE_IFACE};
use super::WrapperCrate;
use crate::parse::wit_name::WitName;

/// Generate the complete source of an edge shim crate for `shim_wit`.
pub fn generate_edge_shim_crate(shim_wit: &EdgeShimWit) -> Result<WrapperCrate> {
    let (_, _, bindings_src) = super::run_wit_bindgen_rust(
        &shim_wit.wit_text,
        Some(&shim_wit.world_name),
    )
    .with_context(|| {
        format!(
            "wit-bindgen failed for edge shim of `{}`",
            shim_wit.collateral_iface
        )
    })?;

    let guest_impl = emit_edge_shim_guest_impl(shim_wit);
    let lib_rs = assemble_edge_shim_lib_rs(&bindings_src, &guest_impl)
        .context("assembling edge shim lib.rs")?;
    let crate_name = edge_shim_crate_name(shim_wit);
    let cargo_toml = edge_shim_cargo_toml(&crate_name);

    Ok(WrapperCrate { crate_name, lib_rs, cargo_toml })
}

/// Build a stable crate name for the edge shim (kebab-case, no special chars).
pub fn edge_shim_crate_name(shim_wit: &EdgeShimWit) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    shim_wit.collateral_iface.hash(&mut h);
    shim_wit.wit_text.hash(&mut h);
    let suffix = format!("{:08x}", h.finish() as u32);

    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
            .collect()
    };
    format!("splicer_edge_shim_{}_{}", sanitize(&shim_wit.collateral_iface), suffix)
}

/// Emit the `impl Guest for EdgeShim` block for the edge shim crate.
pub fn emit_edge_shim_guest_impl(shim_wit: &EdgeShimWit) -> TokenStream {
    // Derive the export module path: ["exports", "splicer", "edge_shim", "{collateral_snake}"]
    let collateral_local = crate::parse::wit_name::iface_of(&shim_wit.collateral_iface);
    let collateral_snake = collateral_local.replace('-', "_");

    let export_mod_segs: Vec<syn::Ident> = ["exports", "splicer", "edge_shim", &collateral_snake]
        .iter()
        .map(|s| syn::Ident::new(s, proc_macro2::Span::call_site()))
        .collect();

    // Raw collateral import path: e.g. bindings::my::service::shapes_viewer
    let raw_path = wit_name_to_rust_idents(&shim_wit.collateral_iface)
        .unwrap_or_default();

    // Bridge path: bindings::splicer::wrapper::bridge
    let bridge_segs: Vec<syn::Ident> = ["splicer", "wrapper", BRIDGE_IFACE]
        .iter()
        .map(|s| syn::Ident::new(&s.replace('-', "_"), proc_macro2::Span::call_site()))
        .collect();

    let resource_snake = shim_wit.resource_wit_name.to_snake_case();
    let resource_pascal: syn::Ident = syn::Ident::new(
        &shim_wit.resource_wit_name.to_upper_camel_case(),
        proc_macro2::Span::call_site(),
    );
    let unwrap_fn: syn::Ident = syn::Ident::new(
        &format!("unwrap_{resource_snake}"),
        proc_macro2::Span::call_site(),
    );

    let methods: Vec<TokenStream> = shim_wit
        .functions
        .iter()
        .map(|f| {
            let fn_ident: syn::Ident = syn::Ident::new(
                &f.fn_name.replace('-', "_"),
                proc_macro2::Span::call_site(),
            );
            let maybe_async = if f.is_async { quote!(async) } else { quote!() };
            let maybe_await = if f.is_async { quote!(.await) } else { quote!() };

            let param_decls: Vec<TokenStream> = f
                .params
                .iter()
                .map(|p| {
                    let pname: syn::Ident = syn::Ident::new(&p.name, proc_macro2::Span::call_site());
                    // Parse the Rust type string. Fall back to a placeholder on parse failure.
                    let ty: syn::Type = syn::parse_str(&p.rust_ty).unwrap_or_else(|_| {
                        syn::parse_str("()").unwrap()
                    });
                    quote!(#pname: #ty)
                })
                .collect();

            let return_ty: syn::Type = syn::parse_str(&f.return_rust_ty).unwrap_or_else(|_| {
                syn::parse_str("()").unwrap()
            });
            let has_explicit_return = f.return_rust_ty != "()";

            let (let_bindings, call_args): (Vec<TokenStream>, Vec<TokenStream>) = f
                .params
                .iter()
                .map(|p| {
                    let pname: syn::Ident = syn::Ident::new(&p.name, proc_macro2::Span::call_site());
                    if p.is_resource_own {
                        let raw_pname: syn::Ident = syn::Ident::new(
                            &format!("raw_{}", p.name),
                            proc_macro2::Span::call_site(),
                        );
                        let let_b = quote! {
                            let #raw_pname = bindings::#(#bridge_segs)::*::#unwrap_fn(#pname);
                        };
                        (let_b, quote!(#raw_pname))
                    } else {
                        (quote!(), quote!(#pname))
                    }
                })
                .unzip();

            let call_expr = quote! {
                bindings::#(#raw_path)::*::#fn_ident(#(#call_args),*)#maybe_await
            };

            let body = quote! {
                #(#let_bindings)*
                #call_expr
            };

            if has_explicit_return {
                quote! {
                    #maybe_async fn #fn_ident(#(#param_decls),*) -> #return_ty {
                        #body
                    }
                }
            } else {
                quote! {
                    #maybe_async fn #fn_ident(#(#param_decls),*) {
                        #body
                    }
                }
            }
        })
        .collect();

    quote! {
        impl bindings::#(#export_mod_segs)::*::Guest for EdgeShim {
            type #resource_pascal = bindings::#(#bridge_segs)::*::#resource_pascal;
            #(#methods)*
        }
    }
}

fn assemble_edge_shim_lib_rs(bindings_src: &str, guest_impl: &TokenStream) -> Result<String> {
    let bindings_file =
        syn::parse_file(bindings_src).context("could not parse edge shim bindings as Rust")?;
    let bindings_items = &bindings_file.items;

    let assembled = quote! {
        mod bindings {
            #(#bindings_items)*
        }
        struct EdgeShim;
        #guest_impl
        bindings::export!(EdgeShim with_types_in bindings);
    };

    let parsed =
        syn::parse2::<syn::File>(assembled).context("assembled edge shim lib.rs is not valid Rust")?;
    Ok(prettyplease::unparse(&parsed))
}

fn edge_shim_cargo_toml(crate_name: &str) -> String {
    let mut package = Map::new();
    package.insert("name".into(), Value::String(crate_name.into()));
    package.insert("version".into(), Value::String("0.1.0".into()));
    package.insert("edition".into(), Value::String("2021".into()));
    package.insert("publish".into(), Value::Boolean(false));

    let mut lib = Map::new();
    lib.insert(
        "crate-type".into(),
        Value::Array(vec![Value::String("cdylib".into())]),
    );

    let mut dependencies = Map::new();
    dependencies.insert("wit-bindgen".into(), Value::String("0.57".into()));

    let mut root = Map::new();
    root.insert("package".into(), Value::Table(package));
    root.insert("lib".into(), Value::Table(lib));
    root.insert("dependencies".into(), Value::Table(dependencies));

    toml::to_string(&Value::Table(root))
        .expect("toml serialization is infallible")
}

/// Convert a qualified WIT interface name to module ident segments for Rust.
/// E.g. `"my:service/shapes-viewer"` → `["my", "service", "shapes_viewer"]`
fn wit_name_to_rust_idents(qualified: &str) -> Option<Vec<syn::Ident>> {
    let n = WitName::parse(qualified)?;
    let segs = [n.ns, n.pkg, n.iface];
    Some(
        segs.iter()
            .map(|s| {
                syn::Ident::new(
                    &s.replace('-', "_"),
                    proc_macro2::Span::call_site(),
                )
            })
            .collect(),
    )
}
