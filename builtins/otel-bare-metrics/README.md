# otel-bare-metrics

Tier-1 builtin that emits `wasi:otel` count + duration metrics for
wrapped calls, with call-id attributes (interface name, function name)
only. No payload-derived data.

`on-call` records the start time; `on-return` folds the duration into
a per-(iface, fn) delta-temporality accumulator (count + histogram
buckets, sum, min, max). The window flushes via
`wasi:otel/metrics.export` when `buffer` samples have been seen or
when `flush_after_seconds` have elapsed since the window opened — the
downstream collector re-aggregates the deltas.

Staleness is checked on every `on-return`, so a window that goes
quiet sits unflushed until the next call. Tier-1 has no shutdown
hook, so the unflushed tail (≤ one flush window per `(iface, fn)`)
is dropped at process exit.

## Config keys

Read at first call via `splicer:builtin-config/get` and cached for
the rest of the wasm-instance lifetime. Parse failures fall back to
the default silently (tier-1 has no logging surface).

| Key                   | Type | Default | Description                              |
|-----------------------|------|---------|------------------------------------------|
| `buffer`              | u32  | 1       | Samples per window before flushing.      |
| `flush_after_seconds` | f64  | 10.0    | Wall-clock staleness flush trigger.      |

**`buffer`** accumulates that many samples per `(iface, fn)` before
flushing. `1` reproduces always-export-per-call. Clamped to a minimum
of 1.

**`flush_after_seconds`** triggers a flush when
`now - window_start >= flush_after_seconds`. Effectively moot when
`buffer == 1`.

Example splice config:

```yaml
inject:
  - builtin:
      name: otel-bare-metrics
      config:
        buffer: 100
        flush_after_seconds: 5.0
```
