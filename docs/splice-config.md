# Splice Configuration Format

This document defines the **YAML schema** for the `splicer` splice configuration file (`SPLICE_CFG_YAML`).

The splice configuration describes **where and how middleware should be inserted** into a WebAssembly component composition graph.

This file is passed to:

```
splicer splice <SPLICE_CFG_YAML> <COMP_WASM> [-o composed.wasm]
```

---

# Overview

A splice configuration file contains a list of **splice rules**.

Each rule describes:

* A **middleware component**
* A **splice strategy**
* A **target location** in the composition graph

At runtime, `splicer` reads the JSON graph, applies each rule, and produces a modified graph.
**_Rule application follows the order of the configuration YAML file._**

---

# Top-Level Structure

```yaml
version: 1

rules:
  ...
```

---

# Schema

## Root Object

| Field     | Type       | Required | Description                                                                                |
|-----------|------------| -------- |--------------------------------------------------------------------------------------------|
| `version` | integer    | ✅       | Configuration format version. Currently must be `1`.                                       |
| `rules`   | list<Rule> | ✅       | Ordered list of splice rules. The order of application will follow the order of this list! |

---

# Rule

```yaml
version: 1

rules:
- before | between:
    ...
  inject:
    ...
```

## Fields

| Field                 | Type         | Required  | Description                                             |
|-----------------------|--------------|-----------|---------------------------------------------------------|
| `before` OR `between` | object       | ✅        | The matching strategy of the rule.                      |
| `inject`              | list<string> | ✅        | Names of the middleware(s) to inject at the match site. |
| `strategy`            | enum         | ✅        | How the middleware should be inserted.                  |
| `target`              | object       | ✅        | Describes where the splice occurs.                      |

---

# Before

```yaml
version: 1

rules:
  - before:
      interface: wasi:http/handler@0.3.0-rc-2026-01-06
      provider_name: srv-b
    inject:
      ...
```

The `before` field instructs the middleware(s) to be inserted _before_ the target interface.

Example effect:

```
B
```

Becomes:

```
M → B
```

## Fields

| Field           | Type     | Required | Description                                                                    |
|-----------------|----------|----------|--------------------------------------------------------------------------------|
| `interface`     | string   | ✅       | The name of the exported function to match on.                                 |
| `provider_name` | string   | ❌       | (if included) Constrains the match to the interface of the specified provider. |

---

# Between

```yaml
version: 1

rules:
  - between:
      interface: wasi:http/handler@0.3.0-rc-2026-01-06
      inner: srv-c
      outer: srv-b
    inject:
      ...
```

The `between` field instructs the middleware(s) to be inserted _between_ the two services communicating over the target interface.

Example effect:

```
A → B
```

Becomes:

```
A → M → B
```

Unlike `before`, `between` requires both endpoints to be explicitly specified.

## Fields

| Field       | Type     | Required | Description                                                                                         |
|-------------|----------|----------|-----------------------------------------------------------------------------------------------------|
| `interface` | string   | ✅       | The name of the exported function to match on.                                                      |
| `inner`     | string   | ✅       | The name of the _downstream_ service to match on (exports the `interface` to be called by `outer`). |
| `outer`     | string   | ✅       | The name of the _upstream_ service to match on (calls the exported `interface` of `inner`).         |

---

# Inject

```yaml
version: 1

rules:
  - before | between:
    ...
    inject:
      - middleware-a
      - middleware-b
```

The middleware(s) to inject at the specified match location (`before` or `between` some interface function invocation).
The order of the middleware in this list will follow the order of invocation on the chain.

For example, the above `yaml` will produce the following chain if matching between A and B (middleware-a gets invoked first):
```
A → middleware-a → middleware-b → B
```

## Inject entry shapes

Each entry under `inject:` is one of two forms — they are mutually
exclusive:

### User middleware (existing form)

```yaml
inject:
  - name: tracing
    path: ./tracing.wasm    # always pass this — see below
```

| Field  | Type   | Required             | Description                                                                                  |
|--------|--------|----------------------|----------------------------------------------------------------------------------------------|
| `name` | string | ✅                   | WAC variable name; must be globally unique across rules.                                     |
| `path` | string | strongly recommended | Path to a `.wasm` (tier-1/2) **or** a tier-3/4 strategy crate directory (see [tier-3][t3]).  |

**Always pass `path`.** Splicer loads the bytes to verify the
middleware's type signature is compatible with the target interface
before composing. If you omit `path`, the type check is downgraded to
a warning (no bytes to fingerprint) and the WAC carries a
`/path/to/comp.wasm` placeholder you'd have to substitute by hand
before any external `wac compose` run could resolve it.

Splicer distinguishes the two `path` shapes at materialize time: a
`.wasm` file flows through the existing tier-1/2 pipeline; a directory
containing a `manifest.toml` is treated as a tier-3/4 strategy crate
root and runs the codegen + cargo build described in [tier-3][t3] and
[tier-4][t4].

[t3]: ./tiers/tier-3.md#user-form-byo-strategy-crate
[t4]: ./tiers/tier-4.md

### Builtin middleware

Builtins ship as OCI artifacts under
`ghcr.io/ejrgilbert/splicer/builtins/<name>:<version>`. At splice
time, splicer resolves each referenced builtin in this order:
`$SPLICER_BUILTINS_DIR/<name>.wasm` (local override, intended for
iterating on a builtin without re-publishing — `make build-builtins`
populates `assets/builtins/`, the natural value to point this at) →
on-disk cache at `<user-cache>/splicer/builtins/<name>@<version>.wasm`
→ OCI pull (populating the cache for next time). Reference one with
`builtin:` instead of supplying a `name`/`path`:

```yaml
inject:
  - builtin: hello-tier1                # short form: scalar = builtin name
  - builtin:                            # long form: extras live inside
      name: hello-tier1
      alias: greeter                    # optional WAC-variable override
      config:                           # optional, see "Builtin config" below
        greeting: "wired-up-greeting"
```

| Field            | Type                | Required      | Description                                                                                     |
|------------------|---------------------|---------------|-------------------------------------------------------------------------------------------------|
| `builtin`        | scalar **or** map   | ✅             | Identifies a splicer-shipped builtin. Scalar form names the builtin directly.                   |
| `builtin.name`   | string              | ✅ (long form) | Builtin's registry name (e.g. `hello-tier1`).                                                   |
| `builtin.alias`  | string              | ❌             | WAC variable name override. Defaults to the builtin's name when omitted.                        |
| `builtin.config` | map<string, scalar> | ❌             | Key-value config sealed into the builtin at splice time (see below). Only present in long form. |

The two forms cannot be mixed: you cannot put `path:` next to
`builtin:`, and you cannot put a top-level `name:` next to `builtin:`
(use `builtin.alias` instead).

Available builtins are discovered at compile time from
[`src/builtins.rs`](../src/builtins.rs) — see that file for the current
list and source crates under [`builtins/`](../builtins/).

#### Builtin config

If a builtin imports the `splicer:builtin-config` substrate (see its
WIT world to check), values you set under `builtin.config:` are sealed
into a tiny per-inject-site provider component that splicer wires next
to the builtin at WAC-composition time. The builtin reads each key at
runtime via `splicer:builtin-config/get`; any key the YAML didn't set
returns `none`, and the builtin falls back to its own hardcoded
default.

Values are scalars (strings, numbers, booleans) — splicer stringifies
them verbatim and the builtin parses them at init. Lists and maps are
rejected at parse time; if a builtin needs structured config it
encodes the structure inside a single string value (JSON,
newline-separated, etc.). Two co-injected builtins get independent
providers — no key namespace collisions — but a key renamed inside a
builtin between versions is a breaking change splicer can't migrate
for you.

If the builtin doesn't import `splicer:builtin-config`, splicer rejects
the splice with a clear error rather than silently dropping the
values — the most common cause is a typo in the builtin name or
picking a builtin that simply doesn't consume the substrate.

**Supported keys + defaults live in each builtin's README**
(`builtins/<name>/README.md`). Splicer doesn't type-check values
against a schema today, so an unknown key passes parse time and just
gets ignored at runtime; consult the README before reaching for the
source.

# Ordering Semantics

Splice rules are applied **in the order they appear** in the file.

Later rules operate on the graph as modified by earlier rules.

This allows stacking middleware intentionally:

```yaml
rules:
  - ...
    inject:
      - logging

  - ...
    inject:
      - metrics
```

Results in:

```
A → logging → metrics → B
```

(if both target the same location)

---

# Validation Rules

The configuration will fail validation if:

* `version` is missing or unsupported
* Any required fields are missing from a rule

Note: If no matches are found in the graph using your configuration, no error will occur!
Rather, the `wac` generated will produce an identity component (should roundtrip to an equivalent component).

---

# Complete Example

```yaml
version: 1

rules:
  - before:
      interface: wasi:http/handler@0.3.0-rc-2026-01-06
    inject:
      - tracing
  - before:
      interface: wasi:http/handler@0.3.0-rc-2026-01-06
      provider_name: auth
    inject:
      - encrypt
  - between:
      interface: wasi:http/handler@0.3.0-rc-2026-01-06
      inner: auth-backend
      outer: auth
    inject:
      - tracing-backend
```

When applying the above rules on the following chained composition:
```
srv-b → auth → auth-backend
```

You get the following chain:
```
tracing → srv-b → tracing → encrypt → auth → tracing → tracing-backend → auth-backend
```

---

# Versioning Policy

The `version` field allows future evolution of the configuration format.

Currently supported:

```
version: 1
```

Future incompatible changes will increment the version number.

---

# Best Practices

* Use descriptive splice rule names
* Avoid overlapping rules unless intentional
* Prefer `between` when targeting a specific edge
* Prefer `before` when targeting a node regardless of incoming/outgoing edges

---

# CLI Usage Reminder

```bash
splicer splice splice-config.yaml composition.wasm -o composed.wasm
```

See the [README](../README.md#usage) for the full flag list, including
`--plan` (emit WAC + a `wac compose ...` shell command instead of
composing in-process) and `--emit-wac` (persist the intermediate WAC
for auditing).
