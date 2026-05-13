# hello-tier2

Tier-2 sibling of [`hello-tier1`](../hello-tier1/). `println!`s a
line per wrapped call with the function's lifted args (on `on-call`)
and lifted result (on `on-return`), so you can eyeball the
`field-tree` shape splicer's tier-2 adapter generates for any
wrapped interface.

Output format:

```
[<greeting>] before <iface>#<fn> (arg-name: type = value, …)
[<greeting>] after  <iface>#<fn> --> type: value
```

The rendering walks the lifted `field-tree` representation defined
in [`wit/common/world.wit`](../../wit/common/world.wit) — primitives
print inline, structural compounds bracket their children, nominal
compounds (`record` / `variant` / `enum` / `flags`) carry their
type-name in the type label, and opaque handles
(`resource` / `stream` / `future` / `error-context`) print as
`kind(type)#id`.

## Config keys

Read at first call via `splicer:builtin-config/get` and cached for
the rest of the wasm-instance lifetime.

| Key        | Type   | Default       | Description                                  |
|------------|--------|---------------|----------------------------------------------|
| `greeting` | string | `hello-tier2` | Replaces the bracketed prefix in each line.  |

Example splice config:

```yaml
inject:
  - builtin:
      name: hello-tier2
      config:
        greeting: "typed-logger"
```
