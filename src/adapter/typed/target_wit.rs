//! Render the WIT material needed to codegen a tier-3/4 wrapper for
//! a target interface, from the composition wasm's own component
//! types. The output is a single WIT text + world name + qualified
//! interface name, suitable for [`super::GenerateWrapperInput`].

use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, Context, Result};
use wit_component::{Output, WitPrinter};
use wit_parser::{
    Docs, Function, FunctionKind, Handle, IndexMap, Interface, InterfaceId, Package, PackageId,
    PackageName, Param, Resolve, Result_, Span, Stability, Tuple, Type, TypeDef, TypeDefKind,
    TypeId, TypeOwner, World, WorldId, WorldItem, WorldKey,
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
    let mut resolve = decode_input_resolve(component_bytes)?;
    let target_iface_id = find_target_interface(&resolve, target_interface)?;
    let qualified = resolve
        .id_of(target_iface_id)
        .ok_or_else(|| anyhow!("target interface `{target_interface}` has no qualified name"))?;

    // Sibling `-types` interfaces the target `use`s.
    let sibling_ifaces: Vec<InterfaceId> = sibling_types_ifaces_of(&resolve, target_iface_id)
        .into_iter()
        .collect();
    let sibling_qualified: Vec<String> = sibling_ifaces
        .iter()
        .map(|id| {
            resolve.id_of(*id).ok_or_else(|| {
                anyhow!("sibling interface used by `{target_interface}` has no qualified name")
            })
        })
        .collect::<Result<_>>()?;

    // For Transform + resources: emit T' + bridge instead of the identity re-export.
    let resources = resources_to_wrap(&resolve, target_iface_id);
    if !resources.is_empty() && matches!(behavior, Behavior::Transform) {
        return emit_t_prime_world(
            &mut resolve,
            target_iface_id,
            target_interface,
            qualified,
            &sibling_ifaces,
            &sibling_qualified,
            &resources,
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

/// Build the T' WIT package in the resolve programmatically, render it with
/// WitPrinter (which auto-generates `use` statements), and assemble the final
/// TargetWit for resource Transform targets.
fn emit_t_prime_world(
    resolve: &mut Resolve,
    target_iface_id: InterfaceId,
    target_interface: &str,
    qualified: String,
    sibling_ifaces: &[InterfaceId],
    sibling_qualified: &[String],
    resources: &[ResourceToWrap],
) -> Result<TargetWit> {
    let local_name = resolve.interfaces[target_iface_id]
        .name
        .as_deref()
        .ok_or_else(|| anyhow!("target interface `{target_interface}` has no local name"))?
        .to_string();

    // Capture original packages BEFORE mutating resolve with T' nodes.
    let original_pkg_text = print_all_packages(resolve, target_interface)?;

    let t_prime_pkg_id = build_t_prime_package(
        resolve,
        target_iface_id,
        &local_name,
        sibling_ifaces,
        sibling_qualified,
        resources,
    )?;

    let mut printer = WitPrinter::default();
    printer
        .print_package(resolve, t_prime_pkg_id, true)
        .context("printing T' wrapper package")?;
    let t_prime_text = printer.output.to_string();

    // WAC cross-name redirect pairs: main interface + siblings that have factored resources.
    let sibling_local_names: Vec<&str> = sibling_qualified.iter().map(|q| iface_of(q)).collect();
    let mut t_prime_redirects = vec![(
        qualified.clone(),
        format!("splicer:wrapper/{local_name}@0.0.0"),
    )];
    for (sibling_q, &sibling_local) in sibling_qualified.iter().zip(&sibling_local_names) {
        if resources.iter().any(|r| r.sibling_qualified.as_deref() == Some(sibling_q.as_str())) {
            t_prime_redirects.push((
                sibling_q.clone(),
                format!("splicer:wrapper/{sibling_local}@0.0.0"),
            ));
        }
    }

    Ok(TargetWit {
        wit_text: format!("{t_prime_text}\n\n{original_pkg_text}"),
        world_name: WRAPPER_WORLD.to_string(),
        qualified_name: qualified,
        t_prime_redirects,
    })
}

// ==================
// ==== T' utils ====
// ==================

/// A resource that needs a T' forwarding wrapper.
struct ResourceToWrap {
    wit_name: String,
    type_id: TypeId,
    /// Interface that declares the resource
    source_iface_id: InterfaceId,
    /// Qualified WIT name of the declaring interface for factored resources
    /// (`None` for inline)
    sibling_qualified: Option<String>,
}

/// Collect all resource types visible in `target`.
fn resources_to_wrap(resolve: &Resolve, target: InterfaceId) -> Vec<ResourceToWrap> {
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
        let (source_iface_id, sibling_qualified) = if declaring == target {
            (target, None)
        } else {
            match resolve.id_of(declaring) {
                Some(q) => (declaring, Some(q)),
                None => continue, // unnamed sibling, skip
            }
        };
        out.push(ResourceToWrap {
            wit_name: name.clone(),
            type_id,
            source_iface_id,
            sibling_qualified,
        });
    }
    out
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

// ==================
// ==== builders ====
// ==================

/// Allocate all T' package nodes in the resolve and return the PackageId.
fn build_t_prime_package(
    resolve: &mut Resolve,
    target_iface_id: InterfaceId,
    local_name: &str,
    sibling_ifaces: &[InterfaceId],
    sibling_qualified: &[String],
    resources: &[ResourceToWrap],
) -> Result<PackageId> {
    let pkg_name = PackageName {
        namespace: WRAPPER_PKG_NS.to_string(),
        name: WRAPPER_PKG_NAME.to_string(),
        version: Some(semver::Version::new(0, 0, 0)),
    };
    let pkg_id = resolve.packages.alloc(Package {
        name: pkg_name.clone(),
        docs: Docs::default(),
        interfaces: IndexMap::default(),
        worlds: IndexMap::default(),
    });
    resolve.package_names.insert(pkg_name, pkg_id);

    // Sibling T' interfaces FIRST: each sibling that has factored resources gets its own
    // T' interface that DECLARES the fresh resource (with its methods). The main T' interface
    // then uses those types via USE aliases, mirroring the original WIT structure.
    let mut subst: HashMap<TypeId, TypeId> = HashMap::new();
    let mut sibling_t_prime_iface_ids: Vec<InterfaceId> = Vec::new();
    for (_sibling_iface_id, sibling_q) in sibling_ifaces.iter().zip(sibling_qualified.iter()) {
        let sibling_local = iface_of(sibling_q);
        let res_for_sibling: Vec<&ResourceToWrap> = resources
            .iter()
            .filter(|r| r.sibling_qualified.as_deref() == Some(sibling_q.as_str()))
            .collect();
        if res_for_sibling.is_empty() {
            continue;
        }
        let sibling_t_prime_id = build_sibling_t_prime_iface(
            resolve,
            pkg_id,
            sibling_local,
            &res_for_sibling,
            &mut subst,
        )?;
        resolve.packages[pkg_id]
            .interfaces
            .insert(sibling_local.to_string(), sibling_t_prime_id);
        sibling_t_prime_iface_ids.push(sibling_t_prime_id);
    }

    let t_prime_iface_id = build_t_prime_iface(
        resolve,
        pkg_id,
        target_iface_id,
        local_name,
        resources,
        &mut subst,
    )?;
    resolve.packages[pkg_id]
        .interfaces
        .insert(local_name.to_string(), t_prime_iface_id);

    // Bridge interface: wrap/unwrap helpers for resources.
    let bridge_iface_id = build_bridge_iface(resolve, pkg_id, resources, &subst)?;
    resolve.packages[pkg_id]
        .interfaces
        .insert(BRIDGE_IFACE.to_string(), bridge_iface_id);

    // World: imports target + siblings, exports T' iface + bridge + sibling reexports.
    let world_id = build_wrapper_world(
        resolve,
        pkg_id,
        target_iface_id,
        sibling_ifaces,
        t_prime_iface_id,
        bridge_iface_id,
        &sibling_t_prime_iface_ids,
    );
    resolve.packages[pkg_id]
        .worlds
        .insert(WRAPPER_WORLD.to_string(), world_id);

    Ok(pkg_id)
}

fn alloc_interface(resolve: &mut Resolve, pkg_id: PackageId, name: &str) -> InterfaceId {
    resolve.interfaces.alloc(Interface {
        name: Some(name.to_string()),
        types: IndexMap::default(),
        functions: IndexMap::default(),
        docs: Docs::default(),
        stability: Stability::default(),
        package: Some(pkg_id),
        span: Span::default(),
        clone_of: None,
    })
}

/// Alloc a fresh Resource TypeDef owned by `dest_iface_id`, insert it into the
/// interface's types map, and record the (original → fresh) mapping in `subst`.
fn declare_fresh_resource(
    resolve: &mut Resolve,
    dest_iface_id: InterfaceId,
    wit_name: &str,
    original_id: TypeId,
    subst: &mut HashMap<TypeId, TypeId>,
) -> TypeId {
    let fresh_id = resolve.types.alloc(TypeDef {
        name: Some(wit_name.to_string()),
        kind: TypeDefKind::Resource,
        owner: TypeOwner::Interface(dest_iface_id),
        docs: Docs::default(),
        stability: Stability::default(),
        span: Span::default(),
    });
    resolve.interfaces[dest_iface_id]
        .types
        .insert(wit_name.to_string(), fresh_id);
    subst.insert(original_id, fresh_id);
    fresh_id
}

/// Copy all constructor/method/static functions for a resource from `source_iface_id`
/// into `dest_iface_id`, remapping the resource type from `original_id` to `fresh_id`.
fn copy_resource_methods(
    resolve: &mut Resolve,
    dest_iface_id: InterfaceId,
    source_iface_id: InterfaceId,
    original_id: TypeId,
    fresh_id: TypeId,
    subst: &HashMap<TypeId, TypeId>,
    use_cache: &mut HashMap<TypeId, TypeId>,
) {
    let res_fns: Vec<(String, Function)> = resolve.interfaces[source_iface_id]
        .functions
        .iter()
        .filter(|(_, func)| matches_resource(&func.kind, original_id))
        .map(|(name, func)| (name.clone(), func.clone()))
        .collect();
    for (fn_name, func) in res_fns {
        let new_kind = remap_func_kind(&func.kind, original_id, fresh_id);
        let new_params =
            substitute_params(resolve, &func.params, subst, dest_iface_id, use_cache);
        let new_result = func
            .result
            .map(|ty| substitute_ty(resolve, ty, subst, dest_iface_id, use_cache));
        resolve.interfaces[dest_iface_id].functions.insert(
            fn_name,
            Function {
                name: func.name,
                kind: new_kind,
                params: new_params,
                result: new_result,
                docs: Docs::default(),
                stability: Stability::default(),
                span: Span::default(),
            },
        );
    }
}

/// Emit `raw-R`/`wrapped-R` type aliases plus `wrap-R`/`unwrap-R` functions for one
/// resource into the bridge interface.
fn emit_bridge_pair(
    resolve: &mut Resolve,
    bridge_iface_id: InterfaceId,
    original_id: TypeId,
    fresh_id: TypeId,
    wit_name: &str,
) {
    let raw_id = resolve.types.alloc(TypeDef {
        name: Some(format!("raw-{wit_name}")),
        kind: TypeDefKind::Type(Type::Id(original_id)),
        owner: TypeOwner::Interface(bridge_iface_id),
        docs: Docs::default(),
        stability: Stability::default(),
        span: Span::default(),
    });
    resolve.interfaces[bridge_iface_id]
        .types
        .insert(format!("raw-{wit_name}"), raw_id);

    let wrapped_id = resolve.types.alloc(TypeDef {
        name: Some(format!("wrapped-{wit_name}")),
        kind: TypeDefKind::Type(Type::Id(fresh_id)),
        owner: TypeOwner::Interface(bridge_iface_id),
        docs: Docs::default(),
        stability: Stability::default(),
        span: Span::default(),
    });
    resolve.interfaces[bridge_iface_id]
        .types
        .insert(format!("wrapped-{wit_name}"), wrapped_id);

    let wrap_fn = format!("wrap-{wit_name}");
    resolve.interfaces[bridge_iface_id].functions.insert(
        wrap_fn.clone(),
        Function {
            name: wrap_fn,
            kind: FunctionKind::Freestanding,
            params: vec![Param {
                name: "inner".to_string(),
                ty: Type::Id(raw_id),
                span: Span::default(),
            }],
            result: Some(Type::Id(wrapped_id)),
            docs: Docs::default(),
            stability: Stability::default(),
            span: Span::default(),
        },
    );

    let unwrap_fn = format!("unwrap-{wit_name}");
    resolve.interfaces[bridge_iface_id].functions.insert(
        unwrap_fn.clone(),
        Function {
            name: unwrap_fn,
            kind: FunctionKind::Freestanding,
            params: vec![Param {
                name: "w".to_string(),
                ty: Type::Id(wrapped_id),
                span: Span::default(),
            }],
            result: Some(Type::Id(raw_id)),
            docs: Docs::default(),
            stability: Stability::default(),
            span: Span::default(),
        },
    );
}

fn build_t_prime_iface(
    resolve: &mut Resolve,
    pkg_id: PackageId,
    target_iface_id: InterfaceId,
    local_name: &str,
    resources: &[ResourceToWrap],
    subst: &mut HashMap<TypeId, TypeId>,
) -> Result<InterfaceId> {
    let t_prime_iface_id = alloc_interface(resolve, pkg_id, local_name);

    let mut use_cache: HashMap<TypeId, TypeId> = HashMap::new();

    // Declare fresh resource TypeDefs for inline resources FIRST so `subst` is
    // populated before any function signatures are substituted.
    for res in resources.iter().filter(|r| r.sibling_qualified.is_none()) {
        let original = resolve_type_alias(resolve, res.type_id);
        declare_fresh_resource(resolve, t_prime_iface_id, &res.wit_name, original, subst);
    }

    // Copy constructor/method/static functions for each inline resource.
    for res in resources.iter().filter(|r| r.sibling_qualified.is_none()) {
        let original = resolve_type_alias(resolve, res.type_id);
        let fresh_id = subst[&original];
        copy_resource_methods(
            resolve,
            t_prime_iface_id,
            target_iface_id,
            original,
            fresh_id,
            subst,
            &mut use_cache,
        );
    }

    // Copy freestanding functions from the target interface. Substitution adds USE aliases
    // in this interface for any factored resource types encountered in params/results.
    let freestanding: Vec<(String, Function)> = resolve.interfaces[target_iface_id]
        .functions
        .iter()
        .filter(|(_, func)| {
            matches!(
                func.kind,
                FunctionKind::Freestanding | FunctionKind::AsyncFreestanding
            )
        })
        .map(|(name, func)| (name.clone(), func.clone()))
        .collect();

    for (fn_name, func) in freestanding {
        let new_params = substitute_params(
            resolve,
            &func.params,
            subst,
            t_prime_iface_id,
            &mut use_cache,
        );
        let new_result = func.result.map(|ty| {
            substitute_ty(resolve, ty, subst, t_prime_iface_id, &mut use_cache)
        });
        resolve.interfaces[t_prime_iface_id].functions.insert(
            fn_name,
            Function {
                name: func.name,
                kind: func.kind,
                params: new_params,
                result: new_result,
                docs: Docs::default(),
                stability: Stability::default(),
                span: Span::default(),
            },
        );
    }

    // Also add USE aliases for factored resources that don't appear in any freestanding
    // function (e.g. resources exported purely so callers can construct them), so the
    // T' world export is still type-consistent even when freestanding functions happen
    // not to reference the resource type directly.
    for res in resources.iter().filter(|r| r.sibling_qualified.is_some()) {
        let original = resolve_type_alias(resolve, res.type_id);
        if let Some(&fresh_id) = subst.get(&original) {
            if resolve.types[fresh_id].owner != TypeOwner::Interface(t_prime_iface_id) {
                ensure_use_alias(resolve, fresh_id, t_prime_iface_id, &mut use_cache);
            }
        }
    }

    Ok(t_prime_iface_id)
}

/// Build the bridge interface: wrap/unwrap functions for each resource.
fn build_bridge_iface(
    resolve: &mut Resolve,
    pkg_id: PackageId,
    resources: &[ResourceToWrap],
    subst: &HashMap<TypeId, TypeId>,
) -> Result<InterfaceId> {
    let bridge_iface_id = alloc_interface(resolve, pkg_id, BRIDGE_IFACE);

    for res in resources {
        let original = resolve_type_alias(resolve, res.type_id);
        let fresh_id = subst[&original];
        emit_bridge_pair(resolve, bridge_iface_id, original, fresh_id, &res.wit_name);
    }

    Ok(bridge_iface_id)
}

/// Build a sibling T' interface that DECLARES the fresh resource (mirroring the original
/// sibling's structure) with all of its methods. This ensures the T' sibling exports the
/// constructor and methods that the consumer expects to call on the import side, and
/// populates `subst` with the (original → fresh) mapping for subsequent builders.
fn build_sibling_t_prime_iface(
    resolve: &mut Resolve,
    pkg_id: PackageId,
    sibling_local: &str,
    res_for_sibling: &[&ResourceToWrap],
    subst: &mut HashMap<TypeId, TypeId>,
) -> Result<InterfaceId> {
    let iface_id = alloc_interface(resolve, pkg_id, sibling_local);

    for res in res_for_sibling {
        let original = resolve_type_alias(resolve, res.type_id);
        declare_fresh_resource(resolve, iface_id, &res.wit_name, original, subst);
    }

    let mut use_cache: HashMap<TypeId, TypeId> = HashMap::new();

    for res in res_for_sibling {
        let original = resolve_type_alias(resolve, res.type_id);
        let fresh_id = subst[&original];
        copy_resource_methods(
            resolve,
            iface_id,
            res.source_iface_id,
            original,
            fresh_id,
            subst,
            &mut use_cache,
        );
    }

    Ok(iface_id)
}

/// Build the wrapper world that imports the target + siblings and exports
/// the T' interface, bridge, and sibling reexport interfaces.
fn build_wrapper_world(
    resolve: &mut Resolve,
    pkg_id: PackageId,
    target_iface_id: InterfaceId,
    sibling_ifaces: &[InterfaceId],
    t_prime_iface_id: InterfaceId,
    bridge_iface_id: InterfaceId,
    sibling_reexport_iface_ids: &[InterfaceId],
) -> WorldId {
    let mut imports: IndexMap<WorldKey, WorldItem> = IndexMap::default();
    let mut exports: IndexMap<WorldKey, WorldItem> = IndexMap::default();

    let wi = |id: InterfaceId| WorldItem::Interface {
        id,
        stability: Stability::default(),
        span: Span::default(),
    };

    // Imports: external target + sibling interfaces.
    // WitPrinter emits their qualified names (different package).
    imports.insert(WorldKey::Interface(target_iface_id), wi(target_iface_id));
    for &sibling_id in sibling_ifaces {
        imports.insert(WorldKey::Interface(sibling_id), wi(sibling_id));
    }

    // Exports: T' iface + bridge + sibling reexports.
    // WitPrinter emits their local names (same package).
    exports.insert(WorldKey::Interface(t_prime_iface_id), wi(t_prime_iface_id));
    exports.insert(WorldKey::Interface(bridge_iface_id), wi(bridge_iface_id));
    for &reexport_id in sibling_reexport_iface_ids {
        exports.insert(WorldKey::Interface(reexport_id), wi(reexport_id));
    }
    // Also export the original target interface so the WAC export line
    // `export t_prime_var["original_iface_name"]` resolves correctly.
    // The generated delegation impl delegates to the T' handler impl
    // (wrapping/unwrapping resources via the bridge).
    exports.insert(WorldKey::Interface(target_iface_id), wi(target_iface_id));

    resolve.worlds.alloc(World {
        name: WRAPPER_WORLD.to_string(),
        imports,
        exports,
        package: Some(pkg_id),
        docs: Docs::default(),
        stability: Stability::default(),
        includes: vec![],
        span: Span::default(),
    })
}

// ========================
// ==== type substitution ====
// ========================

fn matches_resource(kind: &FunctionKind, resource_id: TypeId) -> bool {
    match kind {
        FunctionKind::Constructor(rid)
        | FunctionKind::Method(rid)
        | FunctionKind::AsyncMethod(rid)
        | FunctionKind::Static(rid)
        | FunctionKind::AsyncStatic(rid) => *rid == resource_id,
        _ => false,
    }
}

fn remap_func_kind(kind: &FunctionKind, old_id: TypeId, new_id: TypeId) -> FunctionKind {
    match kind {
        FunctionKind::Constructor(rid) if *rid == old_id => FunctionKind::Constructor(new_id),
        FunctionKind::Method(rid) if *rid == old_id => FunctionKind::Method(new_id),
        FunctionKind::AsyncMethod(rid) if *rid == old_id => FunctionKind::AsyncMethod(new_id),
        FunctionKind::Static(rid) if *rid == old_id => FunctionKind::Static(new_id),
        FunctionKind::AsyncStatic(rid) if *rid == old_id => FunctionKind::AsyncStatic(new_id),
        other => other.clone(),
    }
}

fn substitute_params(
    resolve: &mut Resolve,
    params: &[Param],
    subst: &HashMap<TypeId, TypeId>,
    t_prime_iface_id: InterfaceId,
    use_cache: &mut HashMap<TypeId, TypeId>,
) -> Vec<Param> {
    params
        .iter()
        .map(|p| {
            let new_ty = substitute_ty(resolve, p.ty, subst, t_prime_iface_id, use_cache);
            Param {
                name: p.name.clone(),
                ty: new_ty,
                span: p.span,
            }
        })
        .collect()
}

fn substitute_ty(
    resolve: &mut Resolve,
    ty: Type,
    subst: &HashMap<TypeId, TypeId>,
    t_prime_iface_id: InterfaceId,
    use_cache: &mut HashMap<TypeId, TypeId>,
) -> Type {
    match ty {
        Type::Id(id) => Type::Id(substitute_id(resolve, id, subst, t_prime_iface_id, use_cache)),
        other => other,
    }
}

fn substitute_id(
    resolve: &mut Resolve,
    id: TypeId,
    subst: &HashMap<TypeId, TypeId>,
    t_prime_iface_id: InterfaceId,
    use_cache: &mut HashMap<TypeId, TypeId>,
) -> TypeId {
    let original = resolve_type_alias(resolve, id);

    // Factored resource → substitute with fresh T' resource.
    // When the fresh resource lives in a sibling T' interface (different from the current
    // interface being built), add a USE alias so WitPrinter emits the correct `use` line.
    if let Some(&fresh_id) = subst.get(&original) {
        if resolve.types[fresh_id].owner != TypeOwner::Interface(t_prime_iface_id) {
            return ensure_use_alias(resolve, fresh_id, t_prime_iface_id, use_cache);
        }
        return fresh_id;
    }

    // Named type from another interface → add USE alias so WitPrinter emits `use`.
    // find_named_external follows cross-interface USE aliases to the declaring
    // interface, but preserves intra-interface aliases (e.g. `type headers = fields`).
    if let Some(ext_id) = find_named_external(resolve, id, t_prime_iface_id) {
        return ensure_use_alias(resolve, ext_id, t_prime_iface_id, use_cache);
    }

    // Anonymous structural type → clone kind, substitute inner types, alloc new TypeDef.
    let kind = resolve.types[original].kind.clone();
    let new_kind = substitute_kind(resolve, kind, subst, t_prime_iface_id, use_cache);
    resolve.types.alloc(TypeDef {
        name: None,
        kind: new_kind,
        owner: TypeOwner::None,
        docs: Docs::default(),
        stability: Stability::default(),
        span: Span::default(),
    })
}

/// Walk the alias chain of `id` to find the first named type whose declaring interface
/// is not `exclude_iface`. Follows cross-interface USE aliases but stops at
/// intra-interface aliases (e.g., `type headers = fields` stays as `headers`).
fn find_named_external(
    resolve: &Resolve,
    id: TypeId,
    exclude_iface: InterfaceId,
) -> Option<TypeId> {
    let td = &resolve.types[id];
    if let (Some(_name), TypeOwner::Interface(owner)) = (&td.name, td.owner) {
        if owner != exclude_iface {
            // If this named type is itself a cross-interface USE alias, follow to the
            // declaring interface so WitPrinter emits the right `use` path.
            if let TypeDefKind::Type(Type::Id(inner)) = td.kind {
                let inner_owner = resolve.types[inner].owner;
                if inner_owner != TypeOwner::Interface(owner) {
                    return find_named_external(resolve, inner, exclude_iface);
                }
            }
            return Some(id);
        }
    }
    // Anonymous or owned by exclude_iface: follow Type aliases only.
    if let TypeDefKind::Type(Type::Id(inner)) = td.kind {
        return find_named_external(resolve, inner, exclude_iface);
    }
    None
}

fn substitute_kind(
    resolve: &mut Resolve,
    kind: TypeDefKind,
    subst: &HashMap<TypeId, TypeId>,
    t_prime_iface_id: InterfaceId,
    use_cache: &mut HashMap<TypeId, TypeId>,
) -> TypeDefKind {
    match kind {
        TypeDefKind::List(ty) => {
            TypeDefKind::List(substitute_ty(resolve, ty, subst, t_prime_iface_id, use_cache))
        }
        TypeDefKind::Option(ty) => {
            TypeDefKind::Option(substitute_ty(resolve, ty, subst, t_prime_iface_id, use_cache))
        }
        TypeDefKind::Future(Some(ty)) => TypeDefKind::Future(Some(substitute_ty(
            resolve,
            ty,
            subst,
            t_prime_iface_id,
            use_cache,
        ))),
        TypeDefKind::Stream(Some(ty)) => TypeDefKind::Stream(Some(substitute_ty(
            resolve,
            ty,
            subst,
            t_prime_iface_id,
            use_cache,
        ))),
        TypeDefKind::Result(r) => {
            let ok =
                r.ok.map(|ty| substitute_ty(resolve, ty, subst, t_prime_iface_id, use_cache));
            let err =
                r.err
                    .map(|ty| substitute_ty(resolve, ty, subst, t_prime_iface_id, use_cache));
            TypeDefKind::Result(Result_ { ok, err })
        }
        TypeDefKind::Tuple(t) => {
            let types = t
                .types
                .into_iter()
                .map(|ty| substitute_ty(resolve, ty, subst, t_prime_iface_id, use_cache))
                .collect();
            TypeDefKind::Tuple(Tuple { types })
        }
        TypeDefKind::Handle(Handle::Own(rid)) => TypeDefKind::Handle(Handle::Own(substitute_id(
            resolve,
            rid,
            subst,
            t_prime_iface_id,
            use_cache,
        ))),
        TypeDefKind::Handle(Handle::Borrow(rid)) => {
            TypeDefKind::Handle(Handle::Borrow(substitute_id(
                resolve,
                rid,
                subst,
                t_prime_iface_id,
                use_cache,
            )))
        }
        TypeDefKind::Type(inner) => {
            TypeDefKind::Type(substitute_ty(resolve, inner, subst, t_prime_iface_id, use_cache))
        }
        // Pass through: Resource, Record, Flags, Variant, Enum, Future(None), Stream(None), Unknown
        other => other,
    }
}

/// Ensure a USE alias TypeDef for `external_id` exists in the T' interface.
/// WitPrinter detects the cross-interface alias and emits `use <iface>.{name}` automatically.
fn ensure_use_alias(
    resolve: &mut Resolve,
    external_id: TypeId,
    t_prime_iface_id: InterfaceId,
    use_cache: &mut HashMap<TypeId, TypeId>,
) -> TypeId {
    if let Some(&cached) = use_cache.get(&external_id) {
        return cached;
    }
    let name = resolve.types[external_id]
        .name
        .clone()
        .expect("named type has no name");

    // Reuse existing aliases if already produced.
    if let Some(&existing) = resolve.interfaces[t_prime_iface_id].types.get(&name) {
        use_cache.insert(external_id, existing);
        return existing;
    }
    let alias_id = resolve.types.alloc(TypeDef {
        name: Some(name.clone()),
        kind: TypeDefKind::Type(Type::Id(external_id)),
        owner: TypeOwner::Interface(t_prime_iface_id),
        docs: Docs::default(),
        stability: Stability::default(),
        span: Span::default(),
    });
    resolve.interfaces[t_prime_iface_id]
        .types
        .insert(name, alias_id);
    use_cache.insert(external_id, alias_id);
    alias_id
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
        assert!(wit.contains("wrap-bucket:"), "missing wrap-bucket:\n{wit}");
        assert!(wit.contains("unwrap-bucket:"), "missing unwrap-bucket:\n{wit}");
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
        // Sibling types interface: wrapper exports a store-types interface that
        // DECLARES the fresh T' bucket (with methods) so the consumer's sibling import
        // is type-consistent and includes [constructor]bucket.
        assert!(
            wit.contains("interface store-types"),
            "missing sibling T' interface:\n{wit}"
        );
        assert!(
            wit.contains("resource bucket"),
            "sibling T' interface must declare fresh bucket resource:\n{wit}"
        );
        // Main T' interface must USE the bucket from the sibling T' interface.
        assert!(
            wit.contains("use store-types.{bucket}"),
            "main T' interface must use bucket from sibling:\n{wit}"
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
        assert!(target.t_prime_redirects.is_empty(), "non-T' should have no redirects");
    }

    // Inline resource: resource declared directly inside the target interface (no sibling
    // types interface). The T' main interface must own the fresh resource and its methods,
    // and the bridge must emit wrap/unwrap.
    const INLINE_RESOURCE_WIT: &str = r#"
        package test:kv@0.1.0;
        interface store {
            resource bucket {
                constructor(name: string);
                get: async func(k: string) -> option<string>;
                set: async func(k: string, v: string);
            }
            open: async func(name: string) -> bucket;
        }
        world provider {
            export store;
        }
    "#;

    #[test]
    fn transform_with_inline_resource_emits_t_prime_world() {
        let component =
            component_from_wit(INLINE_RESOURCE_WIT, "provider").expect("synthesize fixture");
        let target = target_wit_for_codegen(&component, "test:kv/store@0.1.0", Behavior::Transform)
            .expect("extract");
        assert!(
            !target.t_prime_redirects.is_empty(),
            "inline resource must trigger T' world"
        );
        let wit = &target.wit_text;
        assert!(wit.contains("interface bridge"), "missing bridge:\n{wit}");
        assert!(wit.contains("wrap-bucket:"), "missing wrap-bucket:\n{wit}");
        assert!(wit.contains("unwrap-bucket:"), "missing unwrap-bucket:\n{wit}");
        assert!(wit.contains("resource bucket"), "T' must declare fresh bucket:\n{wit}");
        assert!(wit.contains("export store;"), "missing export store:\n{wit}");
        assert!(wit.contains("export bridge;"), "missing export bridge:\n{wit}");
        // Inline resource: no sibling types interface, so only one redirect.
        assert_eq!(
            target.t_prime_redirects.len(),
            1,
            "inline-only: expected 1 redirect, got {:?}",
            target.t_prime_redirects
        );
        assert!(
            target.t_prime_redirects.contains(&(
                "test:kv/store@0.1.0".to_string(),
                "splicer:wrapper/store@0.0.0".to_string()
            )),
            "missing main redirect: {:?}",
            target.t_prime_redirects
        );
    }

    #[test]
    fn transform_with_inline_resource_declares_methods_in_main_iface() {
        let component =
            component_from_wit(INLINE_RESOURCE_WIT, "provider").expect("synthesize fixture");
        let target = target_wit_for_codegen(&component, "test:kv/store@0.1.0", Behavior::Transform)
            .expect("extract");
        let wit = &target.wit_text;
        // Constructor and methods must appear in the T' main interface.
        assert!(wit.contains("constructor(name: string)"), "missing constructor:\n{wit}");
        assert!(wit.contains("get:"), "missing get method:\n{wit}");
        assert!(wit.contains("set:"), "missing set method:\n{wit}");
        // No sibling types interface should appear.
        assert!(
            !wit.contains("interface store-types"),
            "inline resource must not generate a sibling types interface:\n{wit}"
        );
    }

    #[test]
    fn transform_with_inline_resource_round_trips_through_wit_bindgen() {
        use crate::adapter::typed::run_wit_bindgen_rust;
        let component =
            component_from_wit(INLINE_RESOURCE_WIT, "provider").expect("synthesize fixture");
        let target = target_wit_for_codegen(&component, "test:kv/store@0.1.0", Behavior::Transform)
            .expect("extract");
        assert!(!target.t_prime_redirects.is_empty());
        run_wit_bindgen_rust(&target.wit_text, Some(&target.world_name))
            .expect("wit-bindgen accepts inline-resource T' WIT");
    }
}
