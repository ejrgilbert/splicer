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

Configurable keys, defaults, and the in-YAML scalar form live in the
embedded `manifest.toml`. Run:

```sh
splicer builtin hello-tier2
```

to see them.
