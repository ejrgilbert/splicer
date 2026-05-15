# Tier-2 middleware builtins

Read-only middleware builtins to ship as splicer primitives.
Each is a fixed component that consumes splicer's
interposition + lift primitives plus a config; none requires
splicer-specific codegen.

## Ranked checklist

Ranked best-for-paper to least.

- [ ] **Typed call/result recording** — lift args + result at the
  boundary, serialize the typed shape to a sink (file, socket,
  in-memory ring). Building block for replay, fuzzing, NMR, and
  end-user-supplied sanitization for fixture generation. Sharpest
  contribution: captures the *typed* value, not opaque bytes.

- [ ] **Typed metrics extraction** — lift args, extract field at
  configured path, increment a labeled counter. WIT field labels
  become metric dimensions for free, no body re-parsing.

- [ ] **Throttler / rate limit** — token bucket per call site or
  per-tenant key extracted from args. Table stakes; no novelty.

- [ ] **Otel spans / call sampler** — emit spans around forwarded
  calls, sample at configured rate. Table stakes observability.

- [ ] **Circuit breaker / timeout (gate-form)** — open/half/closed
  state machine driven by observed result outcomes; reject when
  open. The interesting half (retry-forward) needs lower and is
  out of scope here.

## Future work (not splicer builtins)

These need their own DSL + per-spec wasm codegen. They would be
downstream projects integrating with splicer, not components
shipped inside it. Mention in the paper as evidence that splicer's
primitives generalize.

- **Liquid / refinement type verification** — predicate language,
  parser, per-(WIT + spec) wasm emitter producing a specialized
  middleware component with checks baked in.
