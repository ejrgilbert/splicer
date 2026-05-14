# otel-bare-logs

Tier-1 builtin that emits a structured `wasi:otel/logs` record on
every wrapped call. Each record carries the call's `(interface,
function)` as attributes, a configurable severity, event-name
`call.invoked`, an `observed-timestamp`, and trace-correlation
fields populated from the host's `outer-span-context` when one is
active. No payload-derived content.

**Audience:** shops with a structured-logging backend (Loki, ELK,
Splunk) but no tracing pipeline — they want call-event records
flowing through the format their existing tooling consumes,
independent of whether they also collect spans.

Configurable keys, defaults, and the in-YAML scalar form live in the
embedded `manifest.toml`. Run:

```sh
splicer builtin otel-bare-logs
```

to see them. Each severity name sets both the record's
`severity-text` and the spec base `severity-number` (1 / 5 / 9 / 13 /
17 / 21 for trace/debug/info/warn/error/fatal).
