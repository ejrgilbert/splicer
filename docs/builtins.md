# Builtin middleware

Technical reference for splicer's builtin middleware: how the config
substrate works, how splicer wires it at splice time, and where
per-builtin keys are documented. For the YAML field surface
(`inject: - builtin: ...`), see
[`splice-config.md`](splice-config.md#builtin-middleware).

## Listing builtins

To see every builtin shipped with splicer (with a one-line
description per row):

```
splicer builtin
```

Per-builtin config keys are covered in
[Per-builtin keys and defaults](#per-builtin-keys-and-defaults) below.

## How splicer resolves builtin bytes

Tier-1/2 and tier-3/4 builtins ship differently, so their resolution
paths differ:

| Tier  | What ships                                                                   | Resolution order at splice time                                                                                                                                                                                                            |
|-------|------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 1 & 2 | Pre-built `.wasm` component bytes, published to an OCI registry.             | 1. Local cache override: `$SPLICER_BUILTINS_DIR/<name>.wasm`. <br>2. On-disk cache: `<user-cache>/splicer/builtins/<name>@<version>.wasm`. <br>3. OCI pull from `ghcr.io/ejrgilbert/splicer/builtins/<name>:<version>` (pulls into cache). |
| 3 & 4 | Strategy crate **source**, embedded inside the splicer binary at build time. | 1. Look up `<name>` in the embedded set (no disk or network). <br>2. Extract the source tree to a cache dir. <br>3. cargo codegen + build → a wrapper wasm specialized to the target interface.                                            |

Why they differ: tier-1/2 builtins are reusable components — one wasm
fits every target interface, so an OCI registry is natural. Tier-3/4
builtins produce a *target-specialized* wrapper via codegen, so the
source has to be available at splice time. Embedding it in the splicer
binary keeps installs offline-capable and skips a network round-trip
in the common case.

User-supplied middleware (`path:` in YAML, not `builtin:`) skips all
of the above — splicer loads bytes directly from the path you give
it. Tier-3/4 user strategies (`path:` pointing at a crate dir, see
[`tiers/tier-3.md`](tiers/tier-3.md)) take a parallel codegen path to
the embedded-builtin tier-3/4 flow.

## The `splicer:builtin-config` substrate

A builtin opts into runtime config by importing the
`splicer:builtin-config` WIT interface in its world. When a YAML rule
sets `builtin.config:` for that builtin, splicer **seals** the values
into a tiny per-inject-site provider component and wires it next to
the builtin at WAC-composition time.

At runtime the builtin reads each key via
`splicer:builtin-config/get`. Any key the YAML didn't set returns
`none`, and the builtin falls back to its own hardcoded default.

### Value shape

Each builtin declares its accepted keys and their **WIT types** in
its embedded manifest. At splice time, splicer validates the YAML
values against those declared types and encodes each one as canonical
WAVE — primitives, strings, chars, enums, options, lists, tuples,
records are all supported, so structured config is natural:

```yaml
inject:
  - builtin:
      name: recorder
      config:
        sinks: ["file:traces/", "stdout"]   # list<string>
        buffer: 1024                        # u32
```

A wrong-shaped value (e.g. a string where the manifest declared a
`u32`, or an enum case not in the declared set) fails at config-parse
time with an actionable error naming the key and the expected type.
Run `splicer builtin <name>` to see the declared types for any
builtin.

## Per-builtin keys and defaults

The authoritative list of accepted keys (with WIT types and defaults)
for a given builtin lives in its embedded manifest. Print it with:

```
splicer builtin <name>
```

Some builtins also keep a hand-written `builtins/<name>/README.md`
with extra prose context, but the manifest (via `splicer builtin
<name>`) is the source of truth for what splicer actually accepts.
