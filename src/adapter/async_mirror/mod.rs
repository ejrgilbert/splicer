//! Async-WIT mirror synthesis and the sync→async bridge component
//! that uses it.

pub(crate) mod bridge;

/// Prefix of the bail emitted when an adapter's `synthesize_async_mirror`
/// produces a different qualified name than the caller-supplied
/// `mirror_export_name`. Both must derive the same hash; a mismatch
/// is a splicer bug, not a user error. Tests assert against this
/// prefix instead of the full free-form sentence.
pub(crate) const MIRROR_NAME_MISMATCH_PREFIX: &str = "async mirror name mismatch";

use anyhow::{anyhow, bail, Context, Result};
use wit_component::WitPrinter;
use wit_parser::{FunctionKind, InterfaceId, Resolve, Type};

/// Push a new package into `resolve` containing one interface that
/// async-mirrors `iface_id`: same named types (re-shared via `use`),
/// every `func` rewritten as `async func`. Returns the async mirror
/// interface's id plus its fully-qualified name (e.g.
/// `"splicer:async-mirror-<hash>/<iface>@0.0.1"`).
///
/// Bails on:
/// - interfaces with no qualified name (splicer never
///   targets these),
/// - interfaces with resource-bound functions (methods, statics,
///   constructors): WIT syntax requires those inside `resource { … }`
///   blocks, which the async mirror would have to redeclare with
///   re-shared resource identity — out of scope until the fuzzer
///   surfaces a real target shape that hits this.
#[allow(dead_code)]
pub(crate) fn synthesize_async_mirror(
    resolve: &mut Resolve,
    iface_id: InterfaceId,
) -> Result<(InterfaceId, String)> {
    let qualified = resolve
        .id_of(iface_id)
        .ok_or_else(|| anyhow!("target interface has no qualified name"))?;
    let iface_name = resolve.interfaces[iface_id]
        .name
        .clone()
        .ok_or_else(|| anyhow!("target interface `{qualified}` has no name"))?;

    for (fn_name, func) in &resolve.interfaces[iface_id].functions {
        if !matches!(
            func.kind,
            FunctionKind::Freestanding | FunctionKind::AsyncFreestanding
        ) {
            bail!(
                "interface `{qualified}` declares resource-bound function `{fn_name}` \
                 ({:?}); async-WIT mirror synthesis only supports freestanding functions. \
                 Targeting resource-bearing sync interfaces requires extending the async \
                 mirror synth to redeclare resources via `use` and re-emit methods inside \
                 the resource block.",
                func.kind,
            );
        }
    }

    // Render param/result types via the printer so the WIT we push is
    // syntactically faithful to the original — primitives, aggregates,
    // and `Id` references to named types all round-trip.
    let render_ty = |ty: &Type| -> Result<String> {
        let mut printer = WitPrinter::default();
        printer
            .print_type_name(resolve, ty)
            .context("print_type_name")?;
        Ok(printer.output.to_string())
    };

    let type_names: Vec<String> = resolve.interfaces[iface_id].types.keys().cloned().collect();

    let mirror_pkg_name = format!("splicer:async-mirror-{}@0.0.1", short_hash_hex(&qualified));
    let mut wit = format!("package {mirror_pkg_name};\n\n");
    wit.push_str(&format!("interface {iface_name} {{\n"));
    if !type_names.is_empty() {
        wit.push_str(&format!(
            "    use {qualified}.{{{}}};\n\n",
            type_names.join(", "),
        ));
    }
    for (fn_name, func) in &resolve.interfaces[iface_id].functions {
        let params_str = func
            .params
            .iter()
            .map(|p| Ok(format!("{}: {}", p.name, render_ty(&p.ty)?)))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        let result_str = match &func.result {
            Some(r) => format!(" -> {}", render_ty(r)?),
            None => String::new(),
        };
        wit.push_str(&format!(
            "    {fn_name}: async func({params_str}){result_str};\n"
        ));
    }
    wit.push_str("}\n");

    let pkg_id = resolve
        .push_str("splicer-async-mirror.wit", &wit)
        .with_context(|| format!("parse async mirror WIT for `{qualified}`:\n{wit}"))?;

    let mirror_iface_id = *resolve.packages[pkg_id]
        .interfaces
        .get(&iface_name)
        .ok_or_else(|| {
            anyhow!(
                "async mirror interface `{iface_name}` not found in pushed package — \
                 push_str didn't register it"
            )
        })?;
    let mirror_qualified = resolve.id_of(mirror_iface_id).ok_or_else(|| {
        anyhow!("async mirror interface `{iface_name}` has no qualified name after push")
    })?;
    Ok((mirror_iface_id, mirror_qualified))
}

/// 16-hex-char prefix of `sha2::Sha256(s)`. Stable across Rust
/// versions and processes — bridge codegen and adapter codegen
/// independently derive the same mirror package name, so the hash
/// has to agree across an arbitrary toolchain split. 64 bits keeps
/// collision probability negligible.
pub(crate) fn short_hash_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write;
        write!(out, "{byte:02x}").expect("write to String never fails");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::resolve::decode_input_resolve;
    use crate::adapter::typed::target_wit::test_fixture::component_from_wit;

    /// Sync interface with primitive params and a record result —
    /// exercises type rendering for both bare types and an `Id`
    /// reference to a named record.
    const SYNC_PRIMS_WIT: &str = r#"
        package test:demo@0.1.0;
        interface ops {
            record point { x: u32, y: u32 }
            add: func(a: u32, b: u32) -> u32;
            origin: func() -> point;
            greet: func(name: string) -> string;
        }
        world demo {
            export ops;
        }
    "#;

    fn load_resolve(wit: &str, world: &str) -> (Resolve, InterfaceId) {
        let component = component_from_wit(wit, world).expect("synthesize fixture");
        let resolve = decode_input_resolve(&component).expect("decode");
        let iface_id = resolve
            .interfaces
            .iter()
            .find(|(_, i)| i.name.as_deref() == Some("ops"))
            .map(|(id, _)| id)
            .expect("ops iface present");
        (resolve, iface_id)
    }

    #[test]
    fn mirror_redeclares_each_func_as_async() {
        let (mut resolve, iface_id) = load_resolve(SYNC_PRIMS_WIT, "demo");
        let (mirror_id, mirror_qname) =
            synthesize_async_mirror(&mut resolve, iface_id).expect("synth");
        assert!(
            mirror_qname.starts_with("splicer:async-mirror-"),
            "got: {mirror_qname}"
        );
        let mirror = &resolve.interfaces[mirror_id];
        assert_eq!(mirror.name.as_deref(), Some("ops"));
        assert_eq!(mirror.functions.len(), 3);
        for (name, f) in &mirror.functions {
            assert!(
                matches!(f.kind, FunctionKind::AsyncFreestanding),
                "fn `{name}` kind = {:?}, expected AsyncFreestanding",
                f.kind,
            );
        }
    }

    #[test]
    fn mirror_shares_named_type_identity_via_use() {
        let (mut resolve, iface_id) = load_resolve(SYNC_PRIMS_WIT, "demo");
        let original_point_id = resolve.interfaces[iface_id].types["point"];

        let (mirror_id, _) = synthesize_async_mirror(&mut resolve, iface_id).expect("synth");
        let mirror_point_id = resolve.interfaces[mirror_id].types["point"];

        // `use` should produce an alias whose `Type` ultimately follows
        // back to the original — that's how identity-sharing across
        // interfaces works. The async mirror's TypeDef.kind is
        // `Type(Id(orig))` for a plain `use`.
        use wit_parser::TypeDefKind;
        match &resolve.types[mirror_point_id].kind {
            TypeDefKind::Type(Type::Id(aliased)) => {
                assert_eq!(
                    *aliased, original_point_id,
                    "async mirror's `point` should alias the original's `point`"
                );
            }
            other => panic!("async mirror `point` kind = {other:?}, expected Type(Id(...))"),
        }
    }

    #[test]
    fn mirror_for_interface_with_no_types() {
        // No named types → the `use` line should be omitted, not emit
        // an empty `use foo.{};` which the parser rejects.
        const WIT: &str = r#"
            package test:demo@0.1.0;
            interface ops {
                ping: func();
                echo: func(n: u32) -> u32;
            }
            world demo { export ops; }
        "#;
        let (mut resolve, iface_id) = load_resolve(WIT, "demo");
        let (mirror_id, _) = synthesize_async_mirror(&mut resolve, iface_id).expect("synth");
        let mirror = &resolve.interfaces[mirror_id];
        assert_eq!(mirror.functions.len(), 2);
        assert!(
            mirror.types.is_empty(),
            "async mirror should have no named types"
        );
    }

    #[test]
    fn distinct_targets_get_distinct_mirror_packages() {
        // Two synth calls in the same `Resolve` on two different
        // qualified names should produce two distinct async mirror
        // packages — no collision risk in practice for short qualified
        // names, but the test pins the property.
        const WIT_A: &str = r#"
            package a:demo@0.1.0;
            interface ops { ping: func(); }
            world demo { export ops; }
        "#;
        const WIT_B: &str = r#"
            package b:demo@0.1.0;
            interface ops { ping: func(); }
            world demo { export ops; }
        "#;
        let (mut ra, ia) = load_resolve(WIT_A, "demo");
        let (_, qa) = synthesize_async_mirror(&mut ra, ia).expect("synth a");
        let (mut rb, ib) = load_resolve(WIT_B, "demo");
        let (_, qb) = synthesize_async_mirror(&mut rb, ib).expect("synth b");
        assert_ne!(
            qa.split('/').next(),
            qb.split('/').next(),
            "different targets should hash to different async mirror packages: {qa} vs {qb}"
        );
    }

    #[test]
    fn resource_bound_functions_bail() {
        const WIT: &str = r#"
            package test:demo@0.1.0;
            interface ops {
                resource client {
                    constructor(name: string);
                    ping: func();
                }
            }
            world demo { export ops; }
        "#;
        let (mut resolve, iface_id) = load_resolve(WIT, "demo");
        let err = synthesize_async_mirror(&mut resolve, iface_id).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("resource-bound function") && msg.contains("`[constructor]client`"),
            "unexpected error: {msg}"
        );
    }
}
