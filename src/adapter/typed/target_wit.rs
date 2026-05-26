//! Render the WIT material needed to codegen a tier-3/4 wrapper for
//! a target interface, from the composition wasm's own component
//! types. The output is a single WIT text + world name + qualified
//! interface name, suitable for [`super::GenerateWrapperInput`].

use anyhow::{anyhow, Context, Result};
use wit_component::{Output, WitPrinter};

use super::Behavior;
use crate::adapter::resolve::{decode_input_resolve, find_target_interface};

#[derive(Debug, Clone)]
pub struct TargetWit {
    pub wit_text: String,
    pub world_name: String,
    /// Fully-qualified, e.g. `"wasi:http/handler@0.3.0-rc-..."`.
    pub qualified_name: String,
}

/// Wrapping the synthetic world in its own package keeps the world
/// name from colliding with any world the composition's packages
/// already define.
const WRAPPER_PACKAGE: &str = "splicer:wrapper@0.0.0";
const WRAPPER_WORLD: &str = "target";

/// Renders every package in the resolve, not just the target's
/// transitive deps — wit-bindgen tolerates unused packages, and a
/// precise closure walk would reach into wit-parser internals.
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

    let mut out = String::new();
    out.push_str(&format!("package {WRAPPER_PACKAGE};\n\n"));
    out.push_str(&format!("world {WRAPPER_WORLD} {{\n"));
    out.push_str(&format!("    export {qualified};\n"));
    // Tier-4 synthesizes the result in-strategy, so no downstream import.
    if matches!(behavior, Behavior::Transform) {
        out.push_str(&format!("    import {qualified};\n"));
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
}
