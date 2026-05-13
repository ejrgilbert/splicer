# otel-bare-spans

Tier-1 builtin that wraps every call in a `wasi:otel` span — timing
plus call-id attributes (interface name, function name) only. No
payload-derived data, so it works across every WIT signature splicer
can attach to.

`on-call` mints a span-context (inheriting the host's outer trace-id
when one is active), pushes the start time, and notifies the host via
`wasi:otel/tracing.on-start`. `on-return` emits `on-end` with the
captured timestamps and `Ok` status. Pending spans are tracked per
`(interface, function)` so concurrent or recursive invocations of the
same name don't clobber each other.

## Config keys

Read at first call via `splicer:builtin-config/get` and cached for
the rest of the wasm-instance lifetime. Unrecognized values fall back
to the default silently (tier-1 has no logging surface).

| Key         | Type   | Default    | Description       |
|-------------|--------|------------|-------------------|
| `span_kind` | string | `internal` | OTel span kind.   |

**`span_kind`** accepts `internal` / `server` / `client` /
`producer` / `consumer` (case-insensitive). Set it to `server` when
wrapping incoming-request handlers (e.g. `wasi:http/handler@0.3.0`)
so trace UIs render the spans as server-side request handling
instead of an internal hop.

Example splice config:

```yaml
inject:
  - builtin:
      name: otel-bare-spans
      config:
        span_kind: server
```
