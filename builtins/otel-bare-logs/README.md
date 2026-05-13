# otel-bare-logs

Tier-1 builtin that emits a structured `wasi:otel/logs` record on
every wrapped call. Each record carries the call's `(interface,
function)` as attributes, a configurable severity (default `INFO`),
event-name `call.invoked`, an `observed-timestamp`, and
trace-correlation fields populated from the host's
`outer-span-context` when one is active. No payload-derived content.

**Audience:** shops with a structured-logging backend (Loki, ELK,
Splunk) but no tracing pipeline — they want call-event records
flowing through the format their existing tooling consumes,
independent of whether they also collect spans.

## Config keys

Read at first call via `splicer:builtin-config/get` and cached for
the rest of the wasm-instance lifetime. Unrecognized values fall back
to the default silently (tier-1 has no logging surface).

| Key        | Type   | Default | Description           |
|------------|--------|---------|-----------------------|
| `severity` | string | `INFO`  | OTel severity level.  |

**`severity`** accepts `TRACE` / `DEBUG` / `INFO` / `WARN` /
`ERROR` / `FATAL` (case-insensitive; `WARNING` aliases `WARN`).
Each name sets both the record's `severity-text` and the spec base
`severity-number` (1 / 5 / 9 / 13 / 17 / 21, respectively).

Example splice config — emit at `DEBUG` so the records are filtered
out by default in pipelines gated on level:

```yaml
inject:
  - builtin:
      name: otel-bare-logs
      config:
        severity: DEBUG
```
