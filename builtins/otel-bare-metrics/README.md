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

Configurable keys, defaults, and the in-YAML scalar form live in the
embedded `manifest.toml`. Run:

```sh
splicer builtin otel-bare-metrics
```

to see them.
