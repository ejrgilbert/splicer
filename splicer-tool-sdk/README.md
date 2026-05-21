# splicer-tool-sdk

Shared Rust surface for splicer's typed tiers. Two things:

1. **Canonical `splicer:common/types` mirror** for tier-2
   middleware and any tool that consumes `FieldTree` values
   (decoders, replay drivers, fixture sanitizers). Forces one shared
   Rust type identity across every crate in the splicer stack, so
   values pass between crates without per-boundary conversion.
2. **Tier-3/4 strategy traits**
   ([`TransformStrategy`](src/strategy.rs),
   [`VirtualizeStrategy`](src/strategy.rs)) — the implementation
   contract for tier-3/4 strategy crates.

Tier-1 middleware doesn't need this crate — its only payload is
`CallId`, which a tier-1 middleware can `wit_bindgen::generate!`
locally without cross-crate value flow.

## Canonical types for tier-2

A [`wit_bindgen!`](src/lib.rs) macro that wraps
`wit_bindgen::generate!` and injects canonical `with:` mappings for
every type in `splicer:common/types`. Drop-in replacement; use the
extra macro args (`with:`, `async:`, `generate_all`) exactly like the
upstream macro.

```rust
splicer_tool_sdk::wit_bindgen!({
    world: "my-middleware-mdl",
    async: ["export:splicer:tier2/before@0.1.0#on-call"],
    generate_all,
});
```

Without canonical mappings, two crates running
`wit_bindgen::generate!` against `splicer:common/types` each get a
distinct Rust copy of the types — nominally incompatible despite
matching at the WIT level. The macro forces one shared identity.

See [`tier-2.md`](../docs/tiers/tier-2.md) for the tier-2 authoring
guide; this crate is the type-identity plumbing it depends on.

## Tier-3/4 strategy traits

Two traits — pick one based on whether you forward or replace:

- [`TransformStrategy`](src/strategy.rs) — receives a `downstream`
  closure; you call it (optionally mutating args/result).
- [`VirtualizeStrategy`](src/strategy.rs) — no `downstream`
  parameter; you return `R` directly. The wrapper component doesn't
  import the target interface.

## Install

```toml
[dependencies]
splicer-tool-sdk = "0.1"
```

Tier-1/2 middleware also needs `wit-bindgen` as a dep; tier-3/4
strategies don't (splicer runs wit-bindgen for you at splice-time).

## API reference

See [splicer-tool-sdk docs](https://crates.io/crates/wirm) for the
full public API. The crate is organized into `types` (canonical
mirrors of `splicer:common/types`), `strategy` (tier-3/4 traits), and
helper modules built on top of them.
