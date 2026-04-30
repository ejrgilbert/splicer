# Tier 3: Transform

**Status:** planned.

The middleware can see AND modify both the arguments going to the
downstream and the results coming back. The downstream is still
invoked — the middleware sits in the data path, not in place of it.

For the cross-tier framework (one-tier-per-middleware rule, async
convention, hook-trap propagation, chain composition), see
[`adapter-components.md`](../adapter-components.md). For the value
representation tier 3 inherits from tier 2, see
[`tier-2.md`](./tier-2.md).

Modifications round-trip through the same structural `list<field>`
representation used by tier 2. The adapter deserializes the modified
attribute list back into canonical-ABI values before forwarding to the
downstream (or returning to the caller).

**WIT definition:** `wit/tier3/world.wit` (not yet published)

**Good for:** request enrichment (adding headers, injecting context),
response transformation, payload encryption/redaction, content
filtering, A/B testing (routing different request variants to the same
downstream), retry-with-backoff that mutates request state between
attempts.
