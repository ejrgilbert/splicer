//! Run wit-bindgen-rust in-process to produce the bindings source
//! for a target WIT. The bindings get parsed downstream (with `syn`)
//! to discover types and function signatures for codegen.

use anyhow::{anyhow, Context, Result};
use wit_bindgen_core::{Files, WorldGenerator};
use wit_parser::Resolve;

/// Run wit-bindgen-rust against the given WIT text and produce the
/// bindings source. Returns the contents of the single `.rs` file
/// wit-bindgen-rust emits.
///
/// `world_name` selects which world to generate bindings for. Pass
/// `None` if the WIT contains exactly one world.
pub fn run_wit_bindgen_rust(wit_text: &str, world_name: Option<&str>) -> Result<String> {
    let mut resolve = Resolve::new();
    // The path is a label for wit-parser's diagnostic messages,
    // not a real file. `<in-memory>` makes that obvious if an error
    // bubbles up.
    let pkg_id = resolve
        .push_str("<in-memory>", wit_text)
        .context("failed to parse input WIT")?;
    let world_id = resolve
        .select_world(&[pkg_id], world_name)
        .with_context(|| match world_name {
            Some(n) => format!("could not select world '{n}'"),
            None => "could not select default world (WIT contains multiple worlds?)".to_string(),
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

    let (_, bytes) = files
        .iter()
        .find(|(name, _)| name.ends_with(".rs"))
        .ok_or_else(|| anyhow!("wit-bindgen-rust produced no .rs output"))?;
    std::str::from_utf8(bytes)
        .map(String::from)
        .context("wit-bindgen-rust output is not UTF-8")
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
        let src = run_wit_bindgen_rust(TINY_WIT, Some("demo")).unwrap();
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
