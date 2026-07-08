//! Render the WIT material needed to codegen a tier-3/4 wrapper for
//! a target interface, from the composition wasm's own component
//! types. The output is a single WIT text + world name + qualified
//! interface name, suitable for [`super::GenerateWrapperInput`].

use std::collections::BTreeSet;

use anyhow::{anyhow, Context, Result};
use wit_component::{Output, WitPrinter};
use wit_parser::{
    FunctionKind, Handle, InterfaceId, Resolve, Type, TypeDefKind, TypeId, TypeOwner,
};

use super::Behavior;
use crate::adapter::resolve::{decode_input_resolve, find_target_interface, resolve_type_alias};
use crate::parse::wit_name::iface_of;

#[derive(Debug, Clone)]
pub struct TargetWit {
    pub wit_text: String,
    pub world_name: String,
    /// User-facing target name.
    pub qualified_name: String,
    /// T' mode: (consumer_import_key, t_prime_export_key) cross-name wires for WAC routing.
    /// Covers the main interface and all sibling types interfaces. Empty otherwise.
    pub t_prime_redirects: Vec<(String, String)>,
}

/// Package identifier for the splicer-emitted T' wrapper package.
pub(crate) const WRAPPER_PACKAGE: &str = "splicer:wrapper@0.0.0";
/// Namespace component of the wrapper package name (used in WIT and Resolve checks).
pub(crate) const WRAPPER_PKG_NS: &str = "splicer";
/// Name component of the wrapper package name.
pub(crate) const WRAPPER_PKG_NAME: &str = "wrapper";
/// World name inside the T' wrapper package.
pub(crate) const WRAPPER_WORLD: &str = "target";
/// Interface name for the T' bridge interface.
pub(crate) const BRIDGE_IFACE: &str = "bridge";

/// Renders every package in the resolve.
pub fn target_wit_for_codegen(
    component_bytes: &[u8],
    target_interface: &str,
    behavior: Behavior,
) -> Result<TargetWit> {
    let resolve = decode_input_resolve(component_bytes)?;
    let target_iface_id = find_target_interface(&resolve, target_interface)?;
    let qualified = resolve
        .id_of(target_iface_id)
        .ok_or_else(|| anyhow!("target interface `{target_interface}` has no qualified name"))?;

    // Sibling `-types` interfaces the target `use`s.
    let sibling_ifaces = sibling_types_ifaces_of(&resolve, target_iface_id);
    let sibling_qualified: Vec<String> = sibling_ifaces
        .iter()
        .map(|id| {
            resolve.id_of(*id).ok_or_else(|| {
                anyhow!("sibling interface used by `{target_interface}` has no qualified name")
            })
        })
        .collect::<Result<_>>()?;

    // For Transform + factored resources: emit T' + bridge instead of
    // the identity re-export.
    let factored = factored_resources_of(&resolve, target_iface_id);
    if !factored.is_empty() && matches!(behavior, Behavior::Transform) {
        return emit_t_prime_world(
            &resolve,
            target_iface_id,
            target_interface,
            qualified,
            &sibling_qualified,
            &factored,
        );
    }

    let mut out = String::new();
    out.push_str(&format!("package {WRAPPER_PACKAGE};\n\n"));
    out.push_str(&format!("world {WRAPPER_WORLD} {{\n"));
    match behavior {
        Behavior::Transform => {
            // Tier-3 wraps with an inner producer; sibling types
            // iface is pulled in transitively by the target's `use`
            // statement. wit-component emits the right import in
            // the encoded wrapper regardless.
            for q in &sibling_qualified {
                out.push_str(&format!("    import {q};\n"));
            }
            out.push_str(&format!("    export {qualified};\n"));
            out.push_str(&format!("    import {qualified};\n"));
        }
        Behavior::Virtualize => {
            // Tier-4 has no inner producer; the wrapper IS the type
            // owner. Export the sibling types iface and synthesize
            // resources via the strategy. No downstream import
            // (result synthesized in-strategy).
            for q in &sibling_qualified {
                out.push_str(&format!("    export {q};\n"));
            }
            out.push_str(&format!("    export {qualified};\n"));
        }
    }
    out.push_str("}\n\n");
    out.push_str(&print_all_packages(&resolve, target_interface)?);

    Ok(TargetWit {
        wit_text: out,
        world_name: WRAPPER_WORLD.to_string(),
        qualified_name: qualified,
        t_prime_redirects: vec![],
    })
}

/// Print every package in the resolve as WIT text, separated by blank lines.
fn print_all_packages(resolve: &Resolve, target_interface: &str) -> Result<String> {
    let mut printer = WitPrinter::default();
    for (i, (pkg_id, _)) in resolve.packages.iter().enumerate() {
        if i > 0 {
            printer.output.newline();
            printer.output.newline();
        }
        printer
            .print_package(resolve, pkg_id, false)
            .with_context(|| format!("printing package #{i} for `{target_interface}`"))?;
    }
    Ok(printer.output.to_string())
}

/// Assemble the T'-forwarding WIT package (bridge interface + fresh
/// resource declarations) for a factored-resource Transform target.
fn emit_t_prime_world(
    resolve: &Resolve,
    target_iface_id: InterfaceId,
    target_interface: &str,
    qualified: String,
    sibling_qualified: &[String],
    factored: &[FactoredResource],
) -> Result<TargetWit> {
    let local_name = resolve.interfaces[target_iface_id]
        .name
        .as_deref()
        .ok_or_else(|| anyhow!("target interface `{target_interface}` has no local name"))?;
    let t_prime_iface = emit_t_prime_interface_wit(resolve, target_iface_id, local_name, factored)?;
    let bridge_iface = emit_bridge_interface_wit(factored, local_name);

    // For each sibling types interface, emit a re-export interface that re-exports
    // the T' resource type. This makes the consumer's sibling import consistent
    // with the T' type it gets from the main interface (both sourced from same wrapper instance).
    let sibling_local_names: Vec<&str> = sibling_qualified.iter().map(|q| iface_of(q)).collect();
    let mut sibling_reexport_blocks = String::new();
    for (sibling_q, &sibling_local) in sibling_qualified.iter().zip(&sibling_local_names) {
        let res_names: Vec<&str> = factored
            .iter()
            .filter(|r| r.declaring_qualified == *sibling_q)
            .map(|r| r.wit_name.as_str())
            .collect();
        if res_names.is_empty() {
            continue;
        }
        sibling_reexport_blocks.push_str(&format!("interface {sibling_local} {{\n"));
        for res in &res_names {
            sibling_reexport_blocks.push_str(&format!("  use {local_name}.{{{res}}};\n"));
        }
        sibling_reexport_blocks.push_str("}\n");
    }

    // Collect WAC cross-name redirect pairs: (consumer_import_key → t_prime_export_key).
    let mut t_prime_redirects = vec![(
        qualified.clone(),
        format!("splicer:wrapper/{local_name}@0.0.0"),
    )];
    for (sibling_q, &sibling_local) in sibling_qualified.iter().zip(&sibling_local_names) {
        t_prime_redirects.push((
            sibling_q.clone(),
            format!("splicer:wrapper/{sibling_local}@0.0.0"),
        ));
    }

    let mut out = String::new();
    out.push_str(&format!("package {WRAPPER_PACKAGE};\n\n"));
    out.push_str(&t_prime_iface);
    out.push('\n');
    out.push_str(&bridge_iface);
    if !sibling_reexport_blocks.is_empty() {
        out.push('\n');
        out.push_str(&sibling_reexport_blocks);
    }
    out.push_str(&format!("\nworld {WRAPPER_WORLD} {{\n"));
    out.push_str(&format!("    import {qualified};\n"));
    for q in sibling_qualified {
        out.push_str(&format!("    import {q};\n"));
    }
    out.push_str(&format!("    export {local_name};\n"));
    out.push_str(&format!("    export {BRIDGE_IFACE};\n"));
    for sibling_local in &sibling_local_names {
        out.push_str(&format!("    export {sibling_local};\n"));
    }
    out.push_str("}\n\n");
    out.push_str(&print_all_packages(resolve, target_interface)?);
    Ok(TargetWit {
        wit_text: out,
        world_name: WRAPPER_WORLD.to_string(),
        qualified_name: qualified,
        t_prime_redirects,
    })
}

// ==================
// ==== T' utils ====
// ==================

/// A resource declared in a sibling interface and referenced from the
/// target via a `use` import.
struct FactoredResource {
    wit_name: String,
    type_id: TypeId,
    declaring_iface_id: InterfaceId,
    /// Qualified WIT name of the declaring interface, e.g.
    /// `"my:kv/store-types@0.1.0"`.
    declaring_qualified: String,
}

/// Collect resource types in `target` whose canonical owner is a
/// sibling interface (i.e. factored, not inline).
fn factored_resources_of(resolve: &Resolve, target: InterfaceId) -> Vec<FactoredResource> {
    let iface = &resolve.interfaces[target];
    let mut out = Vec::new();
    for (name, &type_id) in &iface.types {
        let original = resolve_type_alias(resolve, type_id);
        let td = &resolve.types[original];
        if !matches!(td.kind, TypeDefKind::Resource) {
            continue;
        }
        let TypeOwner::Interface(declaring) = td.owner else {
            continue;
        };
        if declaring == target {
            continue; // inline, not factored
        }
        let declaring_qualified = match resolve.id_of(declaring) {
            Some(q) => q,
            None => continue, // unnamed sibling, skip
        };
        out.push(FactoredResource {
            wit_name: name.clone(),
            type_id,
            declaring_iface_id: declaring,
            declaring_qualified,
        });
    }
    out
}

/// Render a WIT type as its source-level text (e.g. `option<string>`,
/// `borrow<bucket>`). Resource own-handles are emitted as bare names
/// (WIT shorthand for `own<R>`).
fn emit_wit_type_text(resolve: &Resolve, ty: &Type) -> String {
    match ty {
        Type::Bool => "bool".to_string(),
        Type::U8 => "u8".to_string(),
        Type::U16 => "u16".to_string(),
        Type::U32 => "u32".to_string(),
        Type::U64 => "u64".to_string(),
        Type::S8 => "s8".to_string(),
        Type::S16 => "s16".to_string(),
        Type::S32 => "s32".to_string(),
        Type::S64 => "s64".to_string(),
        Type::F32 => "f32".to_string(),
        Type::F64 => "f64".to_string(),
        Type::Char => "char".to_string(),
        Type::String => "string".to_string(),
        Type::ErrorContext => "error-context".to_string(),
        Type::Id(id) => emit_type_id_text(resolve, *id),
    }
}

fn emit_type_id_text(resolve: &Resolve, id: TypeId) -> String {
    let td = &resolve.types[id];
    match &td.kind {
        TypeDefKind::List(ty) => format!("list<{}>", emit_wit_type_text(resolve, ty)),
        TypeDefKind::Option(ty) => format!("option<{}>", emit_wit_type_text(resolve, ty)),
        TypeDefKind::Result(r) => match (&r.ok, &r.err) {
            (None, None) => "result".to_string(),
            (Some(ok), None) => format!("result<{}>", emit_wit_type_text(resolve, ok)),
            (None, Some(err)) => format!("result<_, {}>", emit_wit_type_text(resolve, err)),
            (Some(ok), Some(err)) => format!(
                "result<{}, {}>",
                emit_wit_type_text(resolve, ok),
                emit_wit_type_text(resolve, err)
            ),
        },
        TypeDefKind::Tuple(t) => {
            let elems: Vec<_> = t
                .types
                .iter()
                .map(|ty| emit_wit_type_text(resolve, ty))
                .collect();
            format!("tuple<{}>", elems.join(", "))
        }
        // own<R> in WIT is written as just the resource name.
        TypeDefKind::Handle(Handle::Own(rid)) => resolve.types[*rid]
            .name
            .as_deref()
            .unwrap_or("?")
            .to_string(),
        TypeDefKind::Handle(Handle::Borrow(rid)) => {
            let name = resolve.types[*rid].name.as_deref().unwrap_or("?");
            format!("borrow<{name}>")
        }
        // Follow aliases.
        TypeDefKind::Type(inner) => emit_wit_type_text(resolve, inner),
        // Named types: record, variant, enum, flags, resource — bare name.
        _ => td.name.as_deref().unwrap_or("?").to_string(),
    }
}

/// Emit the WIT method/constructor/static declarations for one resource
/// by walking the functions stored in its declaring interface.
fn emit_resource_methods_wit(
    resolve: &Resolve,
    resource_type_id: TypeId,
    declaring_iface_id: InterfaceId,
) -> String {
    let original = resolve_type_alias(resolve, resource_type_id);
    let iface = &resolve.interfaces[declaring_iface_id];
    let mut out = String::new();
    // Functions are sorted by key for determinism.
    let mut fns: Vec<_> = iface.functions.iter().collect();
    fns.sort_by_key(|(name, _)| *name);
    for (_, func) in fns {
        match &func.kind {
            FunctionKind::Constructor(rid) if *rid == original => {
                let params = emit_params(resolve, &func.params, false);
                out.push_str(&format!("    constructor({params});\n"));
            }
            FunctionKind::Method(rid) | FunctionKind::AsyncMethod(rid) if *rid == original => {
                let async_kw = if matches!(func.kind, FunctionKind::AsyncMethod(_)) {
                    "async "
                } else {
                    ""
                };
                let method_name = method_short_name(&func.name);
                let params = emit_params(resolve, &func.params, true); // skip self
                let ret = emit_result(resolve, func.result.as_ref());
                out.push_str(&format!(
                    "    {method_name}: {async_kw}func({params}){ret};\n"
                ));
            }
            FunctionKind::Static(rid) | FunctionKind::AsyncStatic(rid) if *rid == original => {
                let async_kw = if matches!(func.kind, FunctionKind::AsyncStatic(_)) {
                    "async "
                } else {
                    ""
                };
                let static_name = static_short_name(&func.name);
                let params = emit_params(resolve, &func.params, false);
                let ret = emit_result(resolve, func.result.as_ref());
                out.push_str(&format!(
                    "    {static_name}: static {async_kw}func({params}){ret};\n"
                ));
            }
            // function isn't tied to the resource we're targeting
            _ => {}
        }
    }
    out
}

/// Emit the T' interface block: fresh resource declarations + forwarded
/// freestanding functions from the target interface.
fn emit_t_prime_interface_wit(
    resolve: &Resolve,
    target_iface_id: InterfaceId,
    local_name: &str,
    resources: &[FactoredResource],
) -> Result<String> {
    let mut out = format!("interface {local_name} {{\n");
    for res in resources {
        let methods = emit_resource_methods_wit(resolve, res.type_id, res.declaring_iface_id);
        out.push_str(&format!("  resource {} {{\n", res.wit_name));
        out.push_str(&methods);
        out.push_str("  }\n");
    }
    // Freestanding functions from the target interface (factories, etc.).
    let iface = &resolve.interfaces[target_iface_id];
    let mut fns: Vec<_> = iface.functions.iter().collect();
    fns.sort_by_key(|(name, _)| *name);
    for (fn_name, func) in fns {
        match func.kind {
            FunctionKind::Freestanding | FunctionKind::AsyncFreestanding => {
                let async_kw = if matches!(
                    func.kind,
                    FunctionKind::AsyncFreestanding
                        | FunctionKind::AsyncMethod(_)
                        | FunctionKind::AsyncStatic(_)
                ) {
                    "async "
                } else {
                    ""
                };
                let params = emit_params(resolve, &func.params, false);
                let ret = emit_result(resolve, func.result.as_ref());
                out.push_str(&format!("  {fn_name}: {async_kw}func({params}){ret};\n"));
            }
            _ => {} // resource surfaces are emitted above, not here
        }
    }
    out.push_str("}\n");
    Ok(out)
}

/// Emit the bridge interface that exposes wrap/unwrap for T'.
fn emit_bridge_interface_wit(resources: &[FactoredResource], t_prime_local_name: &str) -> String {
    let mut out = format!("interface {BRIDGE_IFACE} {{\n");
    for res in resources {
        let wn = &res.wit_name;
        out.push_str(&format!(
            "  use {}.{{{} as raw-{}}};\n",
            res.declaring_qualified, wn, wn
        ));
        out.push_str(&format!(
            "  use {t_prime_local_name}.{{{wn} as wrapped-{wn}}};\n"
        ));
        out.push_str(&format!("  wrap: func(inner: raw-{wn}) -> wrapped-{wn};\n"));
        out.push_str(&format!("  unwrap: func(w: wrapped-{wn}) -> raw-{wn};\n"));
    }
    out.push_str("}\n");
    out
}

// ── emit_params / emit_result helpers ───────────────────────────────

fn emit_params(resolve: &Resolve, params: &[wit_parser::Param], skip_first: bool) -> String {
    let iter = params.iter().skip(usize::from(skip_first));
    iter.map(|p| format!("{}: {}", p.name, emit_wit_type_text(resolve, &p.ty)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn emit_result(resolve: &Resolve, result: Option<&Type>) -> String {
    match result {
        Some(ty) => format!(" -> {}", emit_wit_type_text(resolve, ty)),
        None => String::new(),
    }
}

/// Strip `[method]resource.` or `[async-method]resource.` prefix.
fn method_short_name(fn_name: &str) -> &str {
    fn_name
        .find(']')
        .and_then(|i| {
            fn_name[i + 1..]
                .find('.')
                .map(|j| &fn_name[i + 1 + j + 1..])
        })
        .unwrap_or(fn_name)
}

/// Strip `[static]resource.` prefix.
fn static_short_name(fn_name: &str) -> &str {
    method_short_name(fn_name) // same pattern
}

/// Walk `target`'s types and return every sibling interface that
/// declares a type referenced via `use types.{R}` (or any other type
/// whose original owner is a different interface).
fn sibling_types_ifaces_of(resolve: &Resolve, target: InterfaceId) -> BTreeSet<InterfaceId> {
    let mut out = BTreeSet::new();
    let iface = &resolve.interfaces[target];
    for (_name, type_id) in &iface.types {
        let original_id = resolve_type_alias(resolve, *type_id);
        if let TypeOwner::Interface(declaring) = resolve.types[original_id].owner {
            if declaring != target {
                out.insert(declaring);
            }
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod test_fixture {
    use anyhow::{Context, Result};
    use wit_component::{embed_component_metadata, ComponentEncoder, StringEncoding};
    use wit_parser::{LiftLowerAbi, ManglingAndAbi, Resolve};

    /// Synthesize a wasm component from inline WIT for unit tests
    /// that need a real (decode-able) composition fixture. Uses
    /// `wit_component::dummy_module` so we don't have to ship a
    /// prebuilt fixture.
    pub fn component_from_wit(wit_text: &str, world_name: &str) -> Result<Vec<u8>> {
        let mut resolve = Resolve::default();
        let pkg_id = resolve
            .push_str("<fixture>", wit_text)
            .context("parse fixture WIT")?;
        let world_id = resolve
            .select_world(&[pkg_id], Some(world_name))
            .context("select fixture world")?;
        let mut core = wit_component::dummy_module(
            &resolve,
            world_id,
            ManglingAndAbi::Legacy(LiftLowerAbi::AsyncStackful),
        );
        embed_component_metadata(&mut core, &resolve, world_id, StringEncoding::UTF8)
            .context("embed_component_metadata")?;
        ComponentEncoder::default()
            .validate(false)
            .module(&core)
            .context("ComponentEncoder::module")?
            .encode()
            .context("ComponentEncoder::encode")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_fixture::component_from_wit;

    const TINY_WIT: &str = r#"
        package test:demo@0.1.0;
        interface ops {
            add: async func(a: u32, b: u32) -> u32;
        }
        world demo {
            export ops;
        }
    "#;

    #[test]
    fn transform_wraps_target_with_export_and_import() {
        let component = component_from_wit(TINY_WIT, "demo").expect("synthesize fixture");
        let target = target_wit_for_codegen(&component, "test:demo/ops@0.1.0", Behavior::Transform)
            .expect("extract");
        assert_eq!(target.world_name, WRAPPER_WORLD);
        assert_eq!(target.qualified_name, "test:demo/ops@0.1.0");
        let wit = &target.wit_text;
        assert!(
            wit.contains(&format!("package {WRAPPER_PACKAGE};")),
            "{wit}"
        );
        assert!(wit.contains(&format!("world {WRAPPER_WORLD}")), "{wit}");
        assert!(wit.contains("export test:demo/ops@0.1.0;"), "{wit}");
        assert!(wit.contains("import test:demo/ops@0.1.0;"), "{wit}");
    }

    #[test]
    fn virtualize_omits_downstream_import() {
        let component = component_from_wit(TINY_WIT, "demo").expect("synthesize fixture");
        let target =
            target_wit_for_codegen(&component, "test:demo/ops@0.1.0", Behavior::Virtualize)
                .expect("extract");
        let wit = &target.wit_text;
        assert!(wit.contains("export test:demo/ops@0.1.0;"), "{wit}");
        assert!(!wit.contains("import test:demo/ops@0.1.0;"), "{wit}");
    }

    #[test]
    fn extracted_wit_round_trips_through_wit_bindgen() {
        // Sanity-check that the rendered text actually parses + a
        // bindgen run can pick the synthetic world. Catches printer
        // round-trip regressions and synthetic-world syntax bugs.
        use crate::adapter::typed::run_wit_bindgen_rust;
        let component = component_from_wit(TINY_WIT, "demo").expect("synthesize fixture");
        let target = target_wit_for_codegen(&component, "test:demo/ops@0.1.0", Behavior::Transform)
            .expect("extract");
        let (_resolve, _world, src) =
            run_wit_bindgen_rust(&target.wit_text, Some(&target.world_name))
                .expect("wit-bindgen accepts extracted WIT");
        assert!(src.contains("pub trait Guest"), "bindings shape:\n{src}");
    }

    #[test]
    fn unknown_target_interface_errors() {
        let component = component_from_wit(TINY_WIT, "demo").expect("synthesize fixture");
        let err = target_wit_for_codegen(&component, "no:such/iface@0.1.0", Behavior::Transform)
            .unwrap_err();
        assert!(err.to_string().contains("no:such/iface"));
    }

    const FACTORED_RESOURCE_WIT: &str = r#"
        package test:kv@0.1.0;
        interface store-types {
            resource bucket {
                constructor(name: string);
                get: async func(k: string) -> option<string>;
                set: async func(k: string, v: string);
            }
        }
        interface store {
            use store-types.{bucket};
            open: async func(name: string) -> bucket;
        }
        world provider {
            export store;
            export store-types;
        }
    "#;

    #[test]
    fn transform_with_factored_resource_emits_t_prime_world() {
        let component =
            component_from_wit(FACTORED_RESOURCE_WIT, "provider").expect("synthesize fixture");
        let target = target_wit_for_codegen(&component, "test:kv/store@0.1.0", Behavior::Transform)
            .expect("extract");
        assert!(
            !target.t_prime_redirects.is_empty(),
            "should have emitted T' world"
        );
        let wit = &target.wit_text;
        assert!(wit.contains("interface bridge"), "missing bridge:\n{wit}");
        assert!(wit.contains("wrap:"), "missing wrap:\n{wit}");
        assert!(wit.contains("unwrap:"), "missing unwrap:\n{wit}");
        assert!(
            wit.contains("export store;"),
            "missing export store:\n{wit}"
        );
        assert!(
            wit.contains("export bridge;"),
            "missing export bridge:\n{wit}"
        );
        assert!(
            wit.contains("import test:kv/store@0.1.0;"),
            "missing import:\n{wit}"
        );
        assert!(
            wit.contains("resource bucket"),
            "T' interface must declare fresh bucket resource:\n{wit}"
        );
        assert!(
            wit.contains("raw-bucket"),
            "bridge must use raw-bucket alias:\n{wit}"
        );
    }

    #[test]
    fn transform_with_factored_resource_round_trips_through_wit_bindgen() {
        use crate::adapter::typed::run_wit_bindgen_rust;
        let component =
            component_from_wit(FACTORED_RESOURCE_WIT, "provider").expect("synthesize fixture");
        let target = target_wit_for_codegen(&component, "test:kv/store@0.1.0", Behavior::Transform)
            .expect("extract");
        assert!(!target.t_prime_redirects.is_empty());
        run_wit_bindgen_rust(&target.wit_text, Some(&target.world_name))
            .expect("wit-bindgen accepts T' WIT");
    }

    #[test]
    fn t_prime_world_emits_sibling_reexport_and_redirects() {
        let component =
            component_from_wit(FACTORED_RESOURCE_WIT, "provider").expect("synthesize fixture");
        let target = target_wit_for_codegen(&component, "test:kv/store@0.1.0", Behavior::Transform)
            .expect("extract");
        assert!(!target.t_prime_redirects.is_empty());
        let wit = &target.wit_text;
        // Sibling types re-export: wrapper exports a store-types interface that
        // re-exports the T' bucket so consumer's sibling import is consistent.
        assert!(
            wit.contains("interface store-types"),
            "missing sibling re-export interface:\n{wit}"
        );
        assert!(
            wit.contains("use store.{bucket}"),
            "sibling re-export must use T' bucket:\n{wit}"
        );
        assert!(
            wit.contains("export store-types;"),
            "world must export store-types:\n{wit}"
        );
        // Redirects: one for the main interface, one for the sibling.
        assert_eq!(target.t_prime_redirects.len(), 2, "expected 2 redirects");
        assert!(
            target.t_prime_redirects.contains(&(
                "test:kv/store@0.1.0".to_string(),
                "splicer:wrapper/store@0.0.0".to_string()
            )),
            "missing main redirect: {:?}",
            target.t_prime_redirects
        );
        assert!(
            target.t_prime_redirects.contains(&(
                "test:kv/store-types@0.1.0".to_string(),
                "splicer:wrapper/store-types@0.0.0".to_string()
            )),
            "missing sibling redirect: {:?}",
            target.t_prime_redirects
        );
    }

    #[test]
    fn non_t_prime_target_has_empty_redirects() {
        let component = component_from_wit(TINY_WIT, "demo").expect("synthesize fixture");
        let target = target_wit_for_codegen(&component, "test:demo/ops@0.1.0", Behavior::Transform)
            .expect("extract");
        assert!(target.t_prime_redirects.is_empty());
        assert!(
            target.t_prime_redirects.is_empty(),
            "non-T' should have no redirects"
        );
    }
}
