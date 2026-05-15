# splicer-tool-sdk

Canonical Rust types + helpers for splicer middleware and downstream
tooling. Use this crate to share one Rust type identity for
`splicer:common/types` values across every crate in your splicer stack.

## Why this exists

Splicer's tier-N middleware contract is defined in WIT. If two crates
both run `wit_bindgen::generate!` against a world that imports
`splicer:common/types`, each one emits its own fresh Rust mirror of
those types in its own module namespace, and Rust's nominal typing
treats those as distinct types, even though at the WIT (and wire) level
they're the same. Passing a `FieldTree` from one crate's bindings to
another requires conversion code that has no semantic content.

This SDK defines a single canonical Rust mirror of every type in
`splicer:common/types`. By pointing `wit_bindgen::generate!`'s `with:`
parameter at these types, your middleware re-uses the same Rust types
every other middleware and tool re-uses, and values flow between them
without conversion.

## Usage

```toml
[dependencies]
splicer-tool-sdk = "0.1"
wit-bindgen = "0.51"
```

The SDK ships a `wit_bindgen!` macro that wraps `wit_bindgen::generate!`
and injects the canonical `with:` mappings for every type in
`splicer:common/types`. Use it exactly like `wit_bindgen::generate!`:

```rust
mod bindings {
    splicer_tool_sdk::wit_bindgen!({
        world: "my-middleware-mdl",
        async: [
            "export:splicer:tier2/before@0.1.0#on-call",
            "export:splicer:tier2/after@0.1.0#on-return",
        ],
        generate_all,
    });
}
```

Your `on-call` / `on-return` hook signatures will then take
`splicer_tool_sdk::Field` / `FieldTree` directly, and helpers in this
crate work on them without any per-crate conversion.

If your world needs additional `with:` remappings (for example, a
`wasi:io/streams` handle type), include a `with: { ... }` block in
the macro args. The SDK merges your entries with its own:

```rust
splicer_tool_sdk::wit_bindgen!({
    world: "my-middleware-mdl",
    async: [...],
    with: {
        "wasi:io/streams@0.2.0/output-stream": my_crate::OutputStream,
    },
    generate_all,
});
```

## API reference

See [splicer-tool-sdk docs](https://crates.io/crates/wirm) for the
full public API. The crate is organized into `types` (canonical mirrors
of `splicer:common/types`) and helper modules built on top of them.
