//! Render the WIT material needed to codegen a tier-3/4 wrapper for
//! a target interface, from the composition wasm's own component
//! types. The output is a single WIT text + world name + qualified
//! interface name, suitable for [`super::GenerateWrapperInput`].

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Context, Result};
use wit_component::{Output, WitPrinter};
use wit_parser::{InterfaceId, Resolve, TypeOwner};

use super::Behavior;
use crate::adapter::async_mirror::{synthesize_async_mirror, MIRROR_NAME_MISMATCH_PREFIX};
use crate::adapter::resolve::{decode_input_resolve, find_target_interface};

#[derive(Debug, Clone)]
pub struct TargetWit {
    pub wit_text: String,
    pub world_name: String,
    /// User-facing target name.
    pub qualified_name: String,
}

const WRAPPER_PACKAGE: &str = "splicer:wrapper@0.0.0";
const WRAPPER_WORLD: &str = "target";

/// Renders every package in the resolve.
///
/// Pass `mirror_export_name = Some(qname)` to trigger the sync-to-async bridge
/// path. This synthesizes an async mirror, exports it instead of the target,
/// cross-checks against the caller-supplied name. Bail on mismatch.
pub fn target_wit_for_codegen(
    component_bytes: &[u8],
    target_interface: &str,
    behavior: Behavior,
    mirror_export_name: Option<&str>,
) -> Result<TargetWit> {
    let mut resolve = decode_input_resolve(component_bytes)?;
    let target_iface_id = find_target_interface(&resolve, target_interface)?;
    // Snapshot before `synthesize_async_mirror` mutates the resolve.
    let qualified = resolve
        .id_of(target_iface_id)
        .ok_or_else(|| anyhow!("target interface `{target_interface}` has no qualified name"))?;

    // Sibling `-types` interfaces the target `use`s. For the
    // wasi-style factored-types pattern (resources declared in a
    // sibling, referenced via `use types.{R}`), the wrapper world
    // has to also claim these so wit-bindgen materializes the
    // resource type in the wrapper crate's bindings and so wac can
    // wire handle traffic between consumer and inner producer.
    let sibling_ifaces = sibling_types_ifaces_of(&resolve, target_iface_id);
    let sibling_qualified: Vec<String> = sibling_ifaces
        .iter()
        .map(|id| {
            resolve.id_of(*id).ok_or_else(|| {
                anyhow!("sibling interface used by `{target_interface}` has no qualified name")
            })
        })
        .collect::<Result<_>>()?;

    let mirror_qname = match mirror_export_name {
        Some(expected) => {
            let (_mirror_iface_id, synthesized) =
                synthesize_async_mirror(&mut resolve, target_iface_id)
                    .with_context(|| format!("synthesize async mirror for `{target_interface}`"))?;
            if synthesized != expected {
                bail!(
                    "{MIRROR_NAME_MISMATCH_PREFIX} for `{target_interface}`: \
                     expected `{expected}`, synthesized `{synthesized}`",
                );
            }
            Some(synthesized)
        }
        None => None,
    };

    let export_iface = mirror_qname.as_deref().unwrap_or(&qualified);

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
            out.push_str(&format!("    export {export_iface};\n"));
            // Tier-3 imports the real (sync) target even when
            // lifting the mirror.
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
            out.push_str(&format!("    export {export_iface};\n"));
        }
    }
    out.push_str("}\n\n");

    let mut printer = WitPrinter::default();
    for (i, (pkg_id, _)) in resolve.packages.iter().enumerate() {
        if i > 0 {
            printer.output.newline();
            printer.output.newline();
        }
        printer
            .print_package(&resolve, pkg_id, false)
            .with_context(|| format!("printing package #{i} for `{target_interface}`"))?;
    }
    out.push_str(&printer.output.to_string());

    Ok(TargetWit {
        wit_text: out,
        world_name: WRAPPER_WORLD.to_string(),
        qualified_name: qualified,
    })
}

/// Walk `target`'s types and return every sibling interface that
/// declares a type referenced via `use types.{R}` (or any other type
/// whose original owner is a different interface). The wasi-style
/// factored-types pattern lands resources in a sibling `-types`
/// interface; the wrapper world has to claim that interface too so
/// the resource type identity is part of the wac composition.
fn sibling_types_ifaces_of(resolve: &Resolve, target: InterfaceId) -> BTreeSet<InterfaceId> {
    let mut out = BTreeSet::new();
    let iface = &resolve.interfaces[target];
    for (_name, type_id) in &iface.types {
        // Follow `Type(_)` aliases (wit-parser models `use` as a
        // local alias whose `kind = Type(original_id)`) until we
        // reach the original declaration. The original's owner is
        // the interface that actually declared the type.
        let mut cur = *type_id;
        loop {
            let td = &resolve.types[cur];
            if let wit_parser::TypeDefKind::Type(wit_parser::Type::Id(next)) = td.kind {
                cur = next;
                continue;
            }
            if let TypeOwner::Interface(declaring) = td.owner {
                if declaring != target {
                    out.insert(declaring);
                }
            }
            break;
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
        let target =
            target_wit_for_codegen(&component, "test:demo/ops@0.1.0", Behavior::Transform, None)
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
        let target = target_wit_for_codegen(
            &component,
            "test:demo/ops@0.1.0",
            Behavior::Virtualize,
            None,
        )
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
        let target =
            target_wit_for_codegen(&component, "test:demo/ops@0.1.0", Behavior::Transform, None)
                .expect("extract");
        let (_resolve, _world, src) =
            run_wit_bindgen_rust(&target.wit_text, Some(&target.world_name))
                .expect("wit-bindgen accepts extracted WIT");
        assert!(src.contains("pub trait Guest"), "bindings shape:\n{src}");
    }

    #[test]
    fn unknown_target_interface_errors() {
        let component = component_from_wit(TINY_WIT, "demo").expect("synthesize fixture");
        let err =
            target_wit_for_codegen(&component, "no:such/iface@0.1.0", Behavior::Transform, None)
                .unwrap_err();
        assert!(err.to_string().contains("no:such/iface"));
    }

    // ── Mirror-lift path (sync-WIT bridged target) ───────────────────

    /// Sync-WIT target fixture (`func`, not `async func`).
    const TINY_SYNC_WIT: &str = r#"
        package test:demo@0.1.0;
        interface ops {
            add: func(a: u32, b: u32) -> u32;
        }
        world demo {
            export ops;
        }
    "#;

    /// The printed package dump below the wrapper world re-emits the
    /// original component's `root` world, which legitimately exports
    /// the sync target. Wrapper-world assertions must be scoped here.
    fn wrapper_world_block(wit: &str) -> &str {
        let start = wit
            .find(&format!("world {WRAPPER_WORLD} {{"))
            .expect("rendered wit must contain the wrapper world");
        let rest = &wit[start..];
        let end = rest
            .find("\n}\n")
            .expect("wrapper world block must close with `\\n}\\n`");
        &rest[..end]
    }

    /// Mirror qname the EmitPlan would compute — uses the same
    /// `short_hash_hex` so the adapter-side cross-check passes.
    fn expected_mirror_name(target: &str) -> String {
        use crate::adapter::async_mirror::short_hash_hex;
        use crate::parse::wit_name::WitName;
        let iface = WitName::parse(target)
            .expect("target qname must parse as ns:pkg/iface[@ver]")
            .iface;
        // Mirror package is always `@0.0.1` regardless of the target's version.
        format!(
            "splicer:async-mirror-{}/{iface}@0.0.1",
            short_hash_hex(target)
        )
    }

    #[test]
    fn transform_mirror_lift_exports_mirror_and_imports_target() {
        let component = component_from_wit(TINY_SYNC_WIT, "demo").expect("synthesize fixture");
        let target_iface = "test:demo/ops@0.1.0";
        let mirror = expected_mirror_name(target_iface);
        let target =
            target_wit_for_codegen(&component, target_iface, Behavior::Transform, Some(&mirror))
                .expect("extract bridged tier-3 target");
        // user-facing qualified_name stays as the original target so
        // runtime CallId.interface_name reflects what the YAML says.
        assert_eq!(target.qualified_name, target_iface);
        let world = wrapper_world_block(&target.wit_text);
        assert!(
            world.contains(&format!("export {mirror};")),
            "wrapper world should export mirror; world:\n{world}"
        );
        assert!(
            world.contains("import test:demo/ops@0.1.0;"),
            "Transform wrapper should still import the real sync target; world:\n{world}"
        );
        assert!(
            !world.contains("export test:demo/ops@0.1.0;"),
            "bridged wrapper world must NOT export the sync target; world:\n{world}"
        );
    }

    #[test]
    fn virtualize_mirror_lift_exports_mirror_with_no_downstream_import() {
        let component = component_from_wit(TINY_SYNC_WIT, "demo").expect("synthesize fixture");
        let target_iface = "test:demo/ops@0.1.0";
        let mirror = expected_mirror_name(target_iface);
        let target = target_wit_for_codegen(
            &component,
            target_iface,
            Behavior::Virtualize,
            Some(&mirror),
        )
        .expect("extract bridged tier-4 target");
        let world = wrapper_world_block(&target.wit_text);
        assert!(
            world.contains(&format!("export {mirror};")),
            "wrapper world should export mirror; world:\n{world}"
        );
        assert!(
            !world.contains("import test:demo/ops@0.1.0;"),
            "Virtualize wrapper must not import a downstream; world:\n{world}"
        );
    }

    #[test]
    fn no_mirror_lift_on_sync_target_preserves_today_behavior() {
        let component = component_from_wit(TINY_SYNC_WIT, "demo").expect("synthesize fixture");
        let target =
            target_wit_for_codegen(&component, "test:demo/ops@0.1.0", Behavior::Transform, None)
                .expect("extract");
        let world = wrapper_world_block(&target.wit_text);
        assert!(world.contains("export test:demo/ops@0.1.0;"), "{world}");
        assert!(world.contains("import test:demo/ops@0.1.0;"), "{world}");
        assert!(
            !target.wit_text.contains("splicer:async-mirror-"),
            "no mirror package should have been synthesized; wit:\n{}",
            target.wit_text
        );
    }

    #[test]
    fn mirror_lift_wit_round_trips_through_wit_bindgen() {
        use crate::adapter::typed::run_wit_bindgen_rust;
        let component = component_from_wit(TINY_SYNC_WIT, "demo").expect("synthesize fixture");
        let target_iface = "test:demo/ops@0.1.0";
        let mirror = expected_mirror_name(target_iface);
        let target =
            target_wit_for_codegen(&component, target_iface, Behavior::Transform, Some(&mirror))
                .expect("extract");
        let (_resolve, _world, src) =
            run_wit_bindgen_rust(&target.wit_text, Some(&target.world_name))
                .expect("wit-bindgen accepts bridged WIT");
        assert!(src.contains("pub trait Guest"), "bindings shape:\n{src}");
        assert!(
            src.contains("async fn"),
            "expected async Guest fns in bridged bindings; src:\n{src}"
        );
    }

    #[test]
    fn mirror_name_mismatch_bails() {
        let component = component_from_wit(TINY_SYNC_WIT, "demo").expect("synthesize fixture");
        let err = target_wit_for_codegen(
            &component,
            "test:demo/ops@0.1.0",
            Behavior::Transform,
            Some("splicer:async-mirror-deadbeef/ops@0.0.1"),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(MIRROR_NAME_MISMATCH_PREFIX),
            "expected mirror-name mismatch bail, got: {msg}"
        );
    }
}
