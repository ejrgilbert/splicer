# `splicer` 🔍✂️

**Plan and generate middleware splice operations for WebAssembly component composition graphs.**

`splicer` reads:

* A **composition graph** (JSON)
* A **splice configuration** (YAML)

It produces a modified plan that injects middleware components according to declarative rules.

This tool is designed to work with component-based systems such as WASI HTTP services, but is interface-agnostic and can splice across any interface edge in a component graph.

---

# Why splicer?

When building component-based systems, middleware insertion often requires:

* Rewriting instantiation chains
* Re-threading handler references
* Maintaining correct edge ordering
* Traversing nested provider chains

`splicer` automates that planning step.

Instead of manually restructuring component wiring, you define:

* What interface to target
* Where to inject middleware
* What middleware components to insert

And `splicer` generates the modified composition plan.

---

# Installation

From source:

```bash
cargo build --release
```

Binary will be located at:

```
target/release/splicer
```

---

# Usage

```bash
splicer <JSON_GRAPH> <SPLICE_CFG> [--output <FILE>]
```

### Arguments

| Argument     | Description                                  |
| ------------ | -------------------------------------------- |
| `JSON_GRAPH` | Path to the composition graph in JSON format |
| `SPLICE_CFG` | Path to the splice configuration YAML file   |
| `--output`   | Optional output file (defaults to stdout)    |

---

# Configuration Format

Splicing behavior is defined in a YAML configuration file.

See full specification:

```
docs/splice-config.md
```

---

# Example Configuration

```yaml
version: 1

rules:
  - before:
      interface: wasi:http/handler
      provider:
        name: auth
    inject:
        - middleware-a
        - middleware-b

  - between:
      interface: wasi:http/handler
      inner:
        name: auth
      outer:
        name: handler
    inject:
        - tracing
```

---

# Splice Semantics

`splicer` operates on interface edges in the graph.

If no matches are found, the generated `wac` will produce an identity component (roundtrips to same component).

Two matching modes are supported:

## 1. Single-Target Injection

Inject middleware for a given interface, optionally scoped to a specific provider.

```yaml
before:
  interface: wasi:http/handler
  provider:
    name: auth
```

If `provider.name` is omitted, all providers of that interface are matched.

---

## 2. Between Injection

Inject middleware between two specific components connected via an interface edge.

```yaml
between:
   interface: wasi:http/handler
   inner:
     name: auth
   outer:
     name: handler
```

This replaces:

```
handler → auth
```

With:

```
handler → middleware → auth
```

Middleware chains are traversed in reverse order during injection to preserve declared ordering.

---

# Rule Ordering

Rules are applied in file order.

Later rules operate on the graph after earlier modifications.

This allows intentional stacking:

```
auth → logging → metrics → handler
```

---

# Validation

The configuration will fail if:

* `version` is missing or unsupported
* Required fields are absent
* Middleware list is empty

---

# Project Structure

```
splicer/
├── src/
├── docs/
│   └── splice-config.md
├── README.md
```

---

# Design Principles

* **Declarative configuration**
* **Deterministic ordering**
* **Interface-driven matching**
* **Graph-aware edge replacement**
* **Middleware-agnostic**

`splicer` does not assume HTTP semantics — it operates on generic interface edges.

---

# Future Evolution

The configuration format is versioned:

```yaml
version: 1
```

Breaking changes will increment the version number.
