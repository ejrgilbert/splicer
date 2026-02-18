# Splice Configuration Format

This document defines the **YAML schema** for the `splicer` splice configuration file (`SPLICE_CFG_YAML`).

The splice configuration describes **where and how middleware should be inserted** into a WebAssembly component composition graph.

This file is passed to:

```
splicer <COMP_GRAPH_JSON> <SPLICE_CFG_YAML> [--output <FILE>]
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
splicer graph.json splice-config.yaml --output planned.json
```
