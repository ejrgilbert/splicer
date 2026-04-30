# Tier 4: Virtualize

**Status:** planned.

The middleware **replaces** the downstream entirely. There is no inner
call; the wrapper synthesizes the return value itself from the lifted
parameters and any state it carries. Downstream resource handles, when
present in the return type, can be fabricated by the wrapper or
threaded through to a backend it controls.

This is the tier where the wrapper *is* the downstream from the
caller's perspective. The caller can't tell the difference between a
real `wasi:http/handler` and a tier-4 implementation that synthesizes
responses locally.

For the cross-tier framework (one-tier-per-middleware rule, async
convention, hook-trap propagation, chain composition — including
tier-4-as-chain-terminator), see
[`adapter-components.md`](../adapter-components.md). For the value
representation tier 4 inherits from tier 2, see
[`tier-2.md`](./tier-2.md).

**WIT definition:** `wit/tier4/world.wit` (not yet published)

**Good for:** WASI-Virt-style virtualization (intercepting
`wasi:filesystem` or `wasi:keyvalue` to redirect or mock), test mocks
that synthesize fixed responses, shadow replayers that serve a recorded
trace back to the caller, fuzzing harness backends that generate inputs
from a model rather than forwarding to a real implementation.
