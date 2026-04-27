//! Spike: validate that splicer's planned WIT-level adapter-generator
//! rewrite is actually viable.
//!
//! Three things need to hold for the new architecture to work:
//!
//! 1. `wit_component::decode` faithfully extracts a `Resolve` + `WorldId`
//!    from a wit-bindgen-compiled component.
//! 2. `wit_parser::Resolve::push_path` can merge an external WIT file
//!    (`wit/tier1/world.wit`) into that same Resolve.
//! 3. `wit_component::ComponentEncoder::module(...).encode()` produces a
//!    valid component when given a core wasm module + the merged WIT.
//!    For this spike we hand it `wit_component::dummy_module(...)` which
//!    is a no-op core module that satisfies the world's import/export
//!    contract — that's enough to prove ComponentEncoder accepts our
//!    flow without committing to a real dispatch core module yet.
//!
//! Run with: `cargo run --example wit_component_spike`. Writes the
//! produced component to `/tmp/spike_adapter.wasm`.

use anyhow::{Context, Result};
use std::path::PathBuf;
use wasm_encoder::{
    CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection,
    ImportSection, MemorySection, MemoryType, Module, TypeSection, ValType,
};
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

/// Spike WIT — start with the simplest case (one primitive func,
/// hook imports merged in) so we can hand-roll a real dispatch core
/// module and verify the import/export naming contract end-to-end.
/// Resources and methods come back once the basic dispatch shape
/// validates.
const SPIKE_WIT: &str = r#"
package spike:demo;

interface api {
    foo: func(x: u32) -> u32;
}

world adapter {
    import api;
    export api;
    import splicer:tier1/before@0.1.0;
    import splicer:tier1/after@0.1.0;
}
"#;

const TIER1_WORLD_WIT: &str = "wit/tier1/world.wit";

fn main() -> Result<()> {
    // ── Step 1: build a Resolve from inline WIT + tier1's world.wit.
    //    This is the "we construct the WIT in memory" half of the
    //    rewrite plan: splicer would do this from the input component's
    //    discovered interfaces plus the tier1 file.
    let mut resolve = Resolve::default();
    // Push tier1 first so the spike's `import splicer:tier1/...` lines
    // resolve against an already-known package.
    let tier1_path = std::env::current_dir()?.join(TIER1_WORLD_WIT);
    let (tier1_pkg, _tier1_paths) = resolve
        .push_path(&tier1_path)
        .with_context(|| format!("merge tier1 WIT from {}", tier1_path.display()))?;
    let spike_pkg = resolve
        .push_str("spike.wit", SPIKE_WIT)
        .context("parse inline spike WIT")?;

    eprintln!(
        "[spike] resolve has {} packages: {:?}",
        resolve.packages.len(),
        resolve
            .packages
            .iter()
            .map(|(_, p)| p.name.to_string())
            .collect::<Vec<_>>()
    );
    let _ = (spike_pkg, tier1_pkg);

    // ── Step 2: pick the spike `adapter` world.
    let world_id = resolve
        .select_world(&[spike_pkg], Some("adapter"))
        .context("select adapter world")?;
    let world = &resolve.worlds[world_id];
    eprintln!(
        "[spike] world `{}`: {} imports, {} exports",
        world.name,
        world.imports.len(),
        world.exports.len(),
    );

    // ── Step 3: hand-build a real dispatch core module that wraps
    //    `foo` with `before-call` / `after-call` hooks. Core-module
    //    naming matches wit-component's contract (verified against
    //    `dummy_module`'s output earlier in this file's history):
    //      - import "spike:demo/api" "foo"
    //      - import "splicer:tier1/before@0.1.0" "before-call"
    //      - import "splicer:tier1/after@0.1.0" "after-call"
    //      - export "spike:demo/api#foo"
    //      - export "memory", "_initialize"
    //    The `before-call`/`after-call` imports take `(name_ptr, name_len)`
    //    after canonical lowering of the WIT `string` param. We bake
    //    the literal "foo" into linear memory at offset 0.
    let mut core_module = build_dispatch_core_module()?;
    embed_component_metadata(&mut core_module, &resolve, world_id, StringEncoding::UTF8)
        .context("embed_component_metadata")?;
    eprintln!("[spike] dispatch core module: {} bytes", core_module.len());
    std::fs::write("/tmp/spike_dispatch_core.wasm", &core_module)?;

    // ── Step 4: encode core module → component.
    let encoded = ComponentEncoder::default()
        .validate(true)
        .module(&core_module)
        .context("ComponentEncoder::module")?
        .encode()
        .context("ComponentEncoder::encode")?;

    let out_path = PathBuf::from("/tmp/spike_adapter.wasm");
    std::fs::write(&out_path, &encoded)?;
    eprintln!(
        "[spike] encoded component: {} ({} bytes)",
        out_path.display(),
        encoded.len(),
    );

    // ── Step 5: independent validation via wasmparser.
    let mut validator =
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    validator
        .validate_all(&encoded)
        .context("post-encode validation")?;

    println!("[spike] OK — push WIT → encode → validate all passed.");
    println!("[spike] Output: {}", out_path.display());
    Ok(())
}

/// Hand-build a minimal dispatch core module for the spike WIT
/// (`foo: func(x: u32) -> u32`, hooks: before-call + after-call).
///
/// Layout: imports come first (foo, before-call, after-call), then
/// memory, then defined funcs (the wrapper + `_initialize`), then the
/// data segment with the literal name "foo" at offset 0, then exports.
fn build_dispatch_core_module() -> Result<Vec<u8>> {
    let mut module = Module::new();

    // ── Type section ─────────────────────────────────────────────────
    // type 0: (i32) -> (i32)        — foo's lowered signature
    // type 1: (i32, i32) -> ()      — before-call / after-call lowered
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32], [ValType::I32]); // 0
    types.ty().function([ValType::I32, ValType::I32], []); // 1
    types.ty().function([], []); // 2 — _initialize signature
    module.section(&types);

    // ── Import section ───────────────────────────────────────────────
    // Funcs imported (indices 0, 1, 2 in the func index space):
    //   0: spike:demo/api / foo
    //   1: splicer:tier1/before@0.1.0 / before-call
    //   2: splicer:tier1/after@0.1.0  / after-call
    let mut imports = ImportSection::new();
    imports.import("spike:demo/api", "foo", EntityType::Function(0));
    imports.import(
        "splicer:tier1/before@0.1.0",
        "before-call",
        EntityType::Function(1),
    );
    imports.import(
        "splicer:tier1/after@0.1.0",
        "after-call",
        EntityType::Function(1),
    );
    module.section(&imports);

    // ── Function section ─────────────────────────────────────────────
    // Defined funcs (indices 3, 4 after the 3 imports):
    //   3: wrapper for foo  — type 0
    //   4: _initialize       — type 2
    let mut funcs = FunctionSection::new();
    funcs.function(0); // wrapper foo
    funcs.function(2); // _initialize
    module.section(&funcs);

    // ── Memory section ───────────────────────────────────────────────
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memory);

    // ── Export section ───────────────────────────────────────────────
    let mut exports = ExportSection::new();
    exports.export("spike:demo/api#foo", ExportKind::Func, 3);
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("_initialize", ExportKind::Func, 4);
    module.section(&exports);

    // ── Code section ─────────────────────────────────────────────────
    let mut code = CodeSection::new();

    // Wrapper for foo:
    //   call before-call(name_ptr=0, name_len=3)  ; "foo" lives at offset 0
    //   local.get 0                               ; the input x
    //   call $imported_foo                        ; → result
    //   local.set $result                         ; stash
    //   call after-call(name_ptr=0, name_len=3)
    //   local.get $result
    //   end
    let mut wrapper = Function::new(vec![(1, ValType::I32)]); // 1 local: $result
    wrapper.instructions().i32_const(0);
    wrapper.instructions().i32_const(3);
    wrapper.instructions().call(1); // before-call
    wrapper.instructions().local_get(0);
    wrapper.instructions().call(0); // imported foo
    wrapper.instructions().local_set(1);
    wrapper.instructions().i32_const(0);
    wrapper.instructions().i32_const(3);
    wrapper.instructions().call(2); // after-call
    wrapper.instructions().local_get(1);
    wrapper.instructions().end();
    code.function(&wrapper);

    // _initialize: empty body.
    let mut init = Function::new(vec![]);
    init.instructions().end();
    code.function(&init);

    module.section(&code);

    // ── Data section ─────────────────────────────────────────────────
    // Active segment: bytes "foo" at memory offset 0.
    let mut data = wasm_encoder::DataSection::new();
    data.active(
        0,
        &wasm_encoder::ConstExpr::i32_const(0),
        b"foo".iter().copied(),
    );
    module.section(&data);

    Ok(module.finish())
}
