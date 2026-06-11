# hello-tier3

Tier-3 sibling of [`hello-tier1`](../hello-tier1/). A pass-through
`TransformStrategy` that `println!`s a line before and after each
wrapped call, then forwards to the real target unchanged. The
smallest meaningful strategy body.

Output format:

```
[<greeting>] before <iface>#<fn>
[<greeting>] after  <iface>#<fn>
```

Configurable keys, defaults, and the in-YAML scalar form live in the
embedded `manifest.toml`. Run:

```sh
splicer builtin hello-tier3
```

to see them.
