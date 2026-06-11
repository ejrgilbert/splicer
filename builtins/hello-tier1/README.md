# hello-tier1

Tier-1 builtin that `println!`s a line per wrapped call. Lightweight
smoke for splice rules.

Output format:

```
[<greeting>] before <iface>#<fn>
[<greeting>] after  <iface>#<fn>
```

Configurable keys, defaults, and the in-YAML scalar form live in the
embedded `manifest.toml`. Run:

```sh
splicer builtin hello-tier1
```

to see them.
