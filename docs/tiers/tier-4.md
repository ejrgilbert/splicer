# Tier 4: Virtualize

**Status:** supported as builtins and as user-supplied strategy crates.

The middleware **replaces** the downstream entirely. There is no inner
call; the wrapper synthesizes the return value itself from the lifted
parameters and any state it carries. The caller can't tell the
difference between a real provider and a tier-4 implementation that
synthesizes responses locally.

Tier-4 shares tier-3's authoring model: a Rust strategy crate
implementing a trait from
[`splicer-tool-sdk`](../../splicer-tool-sdk/src/strategy.rs),
codegen'd into a per-target wrapper at splice-time. See
[tier-3.md](./tier-3.md) for the strategy-crate layout, the
builtin / user-supplied distribution choice, codegen pipeline, and
sync-target bridge -- they apply identically to tier-4 (declare
`tier = 4` in the manifest, implement `VirtualizeStrategy` instead
of `TransformStrategy`).

## How tier-4 differs from tier-3

Two differences, both enforced by the trait:

1. **You can't call the original.** The trait gives you `(call,
   args)` — no `downstream` closure. Whatever you return IS the
   result the caller sees.
2. **There is no import of the target in the composition.** The
   tier-4 wrapper component doesn't declare an import for the wrapped
   interface at all — splicer emits it as exports-only. The composition
   graph has no edge from your strategy to the original provider; the
   path to the original simply doesn't exist in the output.

If you wrote a tier-3 strategy that never called `downstream`, you'd
get the same runtime behavior — but the wrapper would still declare
the import and splicer would still wire it into the composition.
Switching to tier-4 removes the import entirely and shrinks the
composition graph by one edge.

The smallest working tier-4 strategy:
[`builtins/hello-tier4/src/lib.rs`](../../builtins/hello-tier4/src/lib.rs).

## Per-strategy bounds

Same pattern as [tier-3](./tier-3.md#per-strategy-bounds): the trait
has no bounds on `Args` or `R`, strategies declare bounds on the impl,
mismatches with the target surface as cargo trait-bound errors at
splice-time.

| Strategy intent              | Typical bound on the impl                                                                           |
|------------------------------|-----------------------------------------------------------------------------------------------------|
| Default-stub (`hello-tier4`) | `R: Default`                                                                                        |
| Trace replay                 | `R: WitTyped` (use `splicer_tool_sdk::cells_to_typed` to decode a tier-2 cells stream into `R`)     |
| Fixture mock                 | `R: WitTyped` (decode a configured cells/WAVE-text fixture)                                         |
| Random fuzz response         | `R: for<'a> Arbitrary<'a>` (planned)                                                                |
| Chaos: return configured Err | `R: IntoResult, R::Err: Clone` (planned)                                                            |

`WitTyped` is impl'd for `R` automatically: the wrapper codegen emits
it for the types wit-bindgen generates from the target WIT, and the SDK
hand-writes it for the WIT core types. For types you define yourself, or for a
component you author with your own `wit_bindgen::generate!` call, derive
it with `#[derive(splicer_tool_sdk::WitTyped)]` (enable the SDK's
`derive` feature; the macro's rustdoc covers both usages and the shape
mapping).

## Tier-4-specific limitations

- **Resources** work via the
  [`MockedResource`](../../splicer-tool-sdk/src/bridge_resources.rs)
  newtype + `WitTypedWithResources` bridge. The tier-4 wrapper owns
  the resource type (exports the `-types` interface and synthesizes
  via the strategy), so unlike tier-3 there's no method-dispatch
  gap. Inline resource declarations are still rejected; static
  methods on resources emit `core::compile_error!` (see the static-methods
  section of
  [`resource-method-interception.md`](../TODO/resource-method-interception.md)).
  `builtin-hello-tier4.yaml` exercises this on
  `my:service/bucket-as-arg`; [`chaos-err`](../../builtins/chaos-err/)
  is the shipping demo strategy.
- **`future`, `stream`, `error-context`** aren't supported.
  Future/stream synthesis would need host primitives splicer doesn't
  have; error-context awaits a wasmtime cross-component lift fix.

## Good for

WASI-Virt-style virtualization (intercepting `wasi:filesystem` or
`wasi:keyvalue` to redirect or mock), test mocks that synthesize fixed
responses, shadow replayers that serve a recorded trace back to the
caller, fuzzing harness backends that generate inputs from a model
rather than forwarding to a real implementation, chaos generators that
return configured errors.
