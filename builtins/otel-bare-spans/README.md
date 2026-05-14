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

Configurable keys, defaults, and the in-YAML scalar form live in the
embedded `manifest.toml`. Run:

```sh
splicer builtin otel-bare-spans
```

to see them. Tip: set `span_kind: server` when wrapping
incoming-request handlers (e.g. `wasi:http/handler@0.3.0`) so trace
UIs render the spans as server-side request handling instead of an
internal hop.
