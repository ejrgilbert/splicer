# hello-tier4

Tier-4 sibling of [`hello-tier1`](../hello-tier1/). A
`VirtualizeStrategy` that replaces each wrapped call with
`R::default()` -- it never invokes the real target. Smoke-tests
splicer's tier-4 codegen pipeline end-to-end. The `R: Default`
bound narrows which targets match; pair it with splicer type
predication to constrain to `concrete` results.

Output format:

```
[<greeting>] virtualizing <iface>#<fn>
```

Configurable keys, defaults, and the in-YAML scalar form live in the
embedded `manifest.toml`. Run:

```sh
splicer builtin hello-tier4
```

to see them.
