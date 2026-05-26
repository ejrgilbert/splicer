# Tier 4: Virtualize

**Status:** supported as builtins and as user-supplied strategy crates.

The middleware **replaces** the downstream entirely. There is no inner
call; the wrapper synthesizes the return value itself from the lifted
parameters and any state it carries. The caller can't tell the
difference between a real provider and a tier-4 implementation that
synthesizes responses locally.

Tier-4 shares tier-3's authoring model: a Rust strategy crate
implementing a trait from
[`splicer-tool-sdk`](../../splicer-tool-sdk/src/strategy.rs), shipped
either embedded as a builtin or as a user-supplied directory referenced
from the splice YAML, codegen'd into a per-target wrapper at
splice-time. See [tier-3.md](./tier-3.md) for the strategy-crate
layout, codegen pipeline, async-targets-only constraint, and user-form
(`name:` + `path:` to a strategy crate root) — they apply identically
to tier-4 (declare `tier = 4` in the manifest).

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

`hello-tier4` won't wrap an interface whose return type contains a
resource handle (resources can't impl `Default`).

## Tier-4-specific limitation

**Value-typed returns only.** Today, tier-4 can wrap interfaces whose
return types are value-typed (primitives, records, variants, lists,
options, results — anything wit-bindgen lowers without resource
handles). Targets whose returns contain resource handles (e.g.
`wasi:http/handler` returning `Response`) aren't supported yet —
those need a resource walker + `MockedResource` pattern +
types-interface composition wiring; see
[`docs/TODO/tier3-tier4-substrate.md`](../TODO/tier3-tier4-substrate.md).

## Good for

WASI-Virt-style virtualization (intercepting `wasi:filesystem` or
`wasi:keyvalue` to redirect or mock), test mocks that synthesize fixed
responses, shadow replayers that serve a recorded trace back to the
caller, fuzzing harness backends that generate inputs from a model
rather than forwarding to a real implementation, chaos generators that
return configured errors.
