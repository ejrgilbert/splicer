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

| Field                 | Type         | Required | Description                                                                     |
|-----------------------|--------------|----------|---------------------------------------------------------------------------------|
| `before` OR `between` | object       | ✅        | The matching strategy of the rule. See [Before](#before) / [Between](#between). |
| `inject`              | list<object> | ✅        | The middleware(s) to inject at the match site. See [Inject](#inject).           |

---

# Before

```yaml
version: 1

rules:
  - before:
      interface: wasi:http/handler@0.3.0-rc-2026-01-06
      provider:
        name: srv-b
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

| Field            | Type                 | Required | Description                                                                                                                  |
|------------------|----------------------|----------|------------------------------------------------------------------------------------------------------------------------------|
| `interface`      | pattern or list (OR) | ✅        | Interface(s) to match on (glob; see [Pattern matching](#pattern-matching-globs--lists)).                                     |
| `provider.name`  | pattern or list (OR) | ❌        | Constrains the match to the named provider node(s). Omitted ==> matches every provider.                                      |
| `provider.alias` | string               | ❌        | Rename the matched provider in the generated WAC.                                                                            |
| `all-funcs`      | object               | ❌        | Gate the match on the target interface's function shapes, see [Function-shape matching](#function-shape-matching-all-funcs). |

---

# Between

```yaml
version: 1

rules:
  - between:
      interface: wasi:http/handler@0.3.0-rc-2026-01-06
      inner:
        name: srv-c
      outer:
        name: srv-b
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

Both endpoints are **optional**. An omitted `inner`/`outer` matches any
node on that end, which is what lets a globbed `interface` fan out across
edges. Combined with node-name globs, this unlocks rules like:

```yaml
between: { interface: "wasi:*", outer: { name: auth } }              # every wasi edge INTO auth
between: { interface: "*", inner: { name: "wasi*" }, outer: { name: auth } }   # auth's calls into wasi shim providers
between: { interface: "*", inner: { name: "wasi*" }, outer: { name: "mysrv*" } } # mysrv* → wasi* edges
```

## Fields

| Field         | Type                 | Required | Description                                                                                                                   |
|---------------|----------------------|----------|-------------------------------------------------------------------------------------------------------------------------------|
| `interface`   | pattern or list (OR) | ✅        | Interface(s) to match on (glob; see [Pattern matching](#pattern-matching-globs--lists)).                                      |
| `inner.name`  | pattern or list (OR) | ❌        | The _downstream_ node(s) (exports the `interface` called by `outer`). Omitted ==> matches any.                                |
| `inner.alias` | string               | ❌        | Rename the matched inner node in the generated WAC.                                                                           |
| `outer.name`  | pattern or list (OR) | ❌        | The _upstream_ node(s) (calls the exported `interface` of `inner`). Omitted ==> matches any.                                  |
| `outer.alias` | string               | ❌        | Rename the matched outer node in the generated WAC.                                                                           |
| `all-funcs`   | object               | ❌        | Gate the match on the target interface's function shapes — see [Function-shape matching](#function-shape-matching-all-funcs). |

`inner` and `outer` are rejected only when both are present and are the
**same literal pattern**. A glob may legitimately fan out over both
ends. A fully-open `between` (`interface: "*"` with both names omitted)
splices every edge in the composition; that's allowed, just deliberate.

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

### User middleware

```yaml
inject:
  - name: tracing
    path: ./tracing.wasm
```

| Field  | Type   | Required | Description                                                                                 |
|--------|--------|----------|---------------------------------------------------------------------------------------------|
| `name` | string | ✅        | WAC variable name; must be globally unique across rules.                                    |
| `path` | string | ✅        | Path to a `.wasm` (tier-1/2) **or** a tier-3/4 strategy crate directory (see [tier-3][t3]). |

Splicer distinguishes the two `path` shapes at materialize time: a
`.wasm` file flows through the existing tier-1/2 pipeline; a directory
containing a `manifest.toml` is treated as a tier-3/4 strategy crate
root and runs the codegen + cargo build described in [tier-3][t3] and
[tier-4][t4].

[t3]: ./tiers/tier-3.md#referencing-your-strategy-from-a-splice-config
[t4]: ./tiers/tier-4.md

### Builtin middleware

Splicer ships configurable middleware for common interposition tasks —
recording invocations to a sink, OTel telemetry, fuzzing/defaulting
inputs, and so on — so each YAML doesn't have to bring its own. They
plug in by name, no separate build step, and accept a `config:` block
that splicer wires into them at splice time.

Reference one with `builtin:` instead of supplying a `name`/`path`:

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

List every available builtin (with its one-line description) by
running `splicer builtin`; pass a name (`splicer builtin <name>`) to
print its accepted `config` keys. `builtin.config:` values are
scalars only (strings, numbers, booleans) — lists and maps are
rejected at parse time. See [`docs/builtins.md`](builtins.md) for the
technical details.

---

# Pattern matching (globs + lists)

The `interface` field and every node-name field (`provider`, `inner`,
`outer`) are **glob patterns**, and each accepts either a single pattern
(scalar) or a **list** of patterns (`OR` semantic, matches if any one matches):

```yaml
interface: "wasi:*"                 # one glob
interface: ["wasi:*", "my:srv/*"]   # matches either
```

Glob syntax follows the [`globset` crate](https://docs.rs/globset/latest/globset/#syntax)
(splicer uses globset's default flat matching, so `*` and `?` cross `/`;
`**` behaves identically to `*` — there's no recursive form to reach for).

A plain string with no `*`/`?`/`[…]` matches exactly. A bad glob
(e.g. an unterminated `[`) fails quickly, when the config is parsed.

---

# Function-shape matching (`all-funcs`)

`before` and `between` accept an optional `all-funcs:` block that gates
the match on a **property of the target interface's functions**, not just
its name. The properties apply to **every** function of the matched
interface:

```yaml
before:
  interface: "*"                     # broad glob
  all-funcs:                         # gate glob match to compatible targets
    async: true                      # every function is `async func`
    results: [concrete, defaultable] # every result satisfies both
```

Omitting `all-funcs:` imposes no function gate, so every name-only config
keeps working unchanged. An empty `all-funcs: {}` is rejected — omit the
key to mean "no requirement".

## Keys

| Key       | Value                               | Holds when...                                                                                      |
|-----------|-------------------------------------|----------------------------------------------------------------------------------------------------|
| `async`   | bool                                | `true`: every function is `async func`; `false`: every function is sync.                           |
| `scope`   | keyword or list of keywords (OR)    | every function's WIT-tree surface is one of the listed scopes. Defaults to `interface` if omitted. |
| `args`    | keyword or list of keywords (AND)   | every argument of every function satisfies every listed property.                                  |
| `results` | keyword or list of keywords (AND)   | every result of every function satisfies every listed property.                                    |

`scope` gates the match on the kind of WIT-tree surface every function
inhabits: an interface matches only when **every** function is on one
of the listed surfaces. Splicer interposes on whole interfaces, so a
single out-of-scope function disqualifies the match. Available values:

| Keyword     | A function matches when                                                                   |
|-------------|-------------------------------------------------------------------------------------------|
| `interface` | it's a WIT-level free function. The common case; this is the default.                     |
| `resource`  | it's a component-model resource surface (`[constructor]r`, `[method]r.f`, `[static]r.f`). |

> ⚠️ **`scope: resource` is not yet implemented.** Splicer's adapter
> codegen can't currently wrap resource constructor/method/static
> surfaces, so any rule that opts into `scope: resource` will panic at
> selection time with a pointer to the implementation site.

Value-property keywords for `args` / `results`:

| Keyword       | A type matches when...                                                                                                                 |
|---------------|----------------------------------------------------------------------------------------------------------------------------------------|
| `concrete`    | is directly-representable, self-describing data. No resource/async handle or `error-context` anywhere within it (checked recursively). |
| `defaultable` | an unambiguous default can be synthesized (e.g. primitives-->zero, `string`-->`""`, `option`-->none).                                  |

`concrete` and `defaultable` are **independent**; neither implies the
other. `result<u32, string>` is concrete but not defaultable;
`option<resource>` is defaultable but not concrete.

---

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
      provider:
        name: auth
    inject:
      - encrypt
  - between:
      interface: wasi:http/handler@0.3.0-rc-2026-01-06
      inner:
        name: auth-backend
      outer:
        name: auth
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
