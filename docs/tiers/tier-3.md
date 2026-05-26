# Tier 3: Transform

**Status:** supported as builtins and as user-supplied strategy crates.

The middleware can see AND modify both the arguments going to the
downstream and the results coming back. The downstream is still
invoked — the middleware sits in the data path, not in place of it.

## How tier-3 differs from tier-1/2

Tier-1/2 middlewares are wasm components: you write your middleware
against the tier's WIT hook ABI
([`wit/tier1/world.wit`](../../wit/tier1/world.wit),
[`wit/tier2/world.wit`](../../wit/tier2/world.wit)), splicer generates
a separate adapter component that translates the target interface's
calls into hook calls, and the two get wired together at compose time.

Tier-3 has no WIT hook ABI. Instead:

1. You write a **Rust strategy crate** implementing
   [`splicer_tool_sdk::TransformStrategy`](../../splicer-tool-sdk/src/strategy.rs).
2. At splice time, splicer reads the target interface's WIT from the
   composition, runs `wit-bindgen` against it to produce typed Rust
   bindings, and emits a wrapper crate that implements the target's
   `Guest` traits by dispatching each method to your strategy.
3. The wrapper crate gets `cargo build`-ed to a wasm component and
   linked into the composition. The wrapper is the adapter; there's no
   second artifact.

Net effect: tier-3 middleware sees **typed Rust values** for the args, `Args`,
and the result, `R`, (not the structural `field-tree` tier-2 uses), and can return
modified values directly. wit-bindgen handles canonical-ABI
lifting/lowering on both sides.

## The `TransformStrategy` trait

The trait shape lives in
[`splicer-tool-sdk/src/strategy.rs`](../../splicer-tool-sdk/src/strategy.rs)
— see its rustdoc for the canonical signature. Per wrapped call,
`handle` receives:

- `call`: the same `CallId` shape tier-1/2 use (`interface_name`,
  `function_name`, `id`), threaded through verbatim.
- `args`: a tuple of the function's arguments, typed per the target
  WIT. For `add: func(a: s32, b: s32) -> s32` you get `(i32, i32)`.
- `downstream`: a closure that calls the wrapped target with the args
  you pass it. Returns the typed result.

You return `R`. Three shapes are legal:

1. Forward unchanged: `downstream(args).await`
2. Transform either side: `let r = downstream(mutated_args).await; mutate(r)`
3. Synthesize without forwarding: build an `R` yourself and return it; never call `downstream`

Shape 3 is valid Rust and valid splicer behavior, but if you're doing
it consistently, [tier-4](./tier-4.md) is the right tier — its trait
signature has no `downstream` parameter at all, so the wrapper doesn't
import the target and the choice is enforced at the type level.

`Args` and `R` are generic at the trait level. Strategies that accept
any target shape leave them unconstrained; strategies that need
specific properties on the return type (a numeric, a `Result`, a
hashable value) declare them as where-clauses on the impl — see
[Per-strategy bounds](#per-strategy-bounds) below.

## Writing a tier-3 strategy

[`builtins/hello-tier3/src/lib.rs`](../../builtins/hello-tier3/src/lib.rs)
is the smallest working strategy. Read it as the canonical template.

A complete strategy crate needs:

- A `Cargo.toml` with `splicer-tool-sdk` as a path dep, declaring a
  `[lib]` (cdylib for the eventual wrapper, but cargo's regular lib
  output is fine — the wrapper crate is what becomes the cdylib).
- A `manifest.toml` next to `Cargo.toml` declaring
  `[builtin] tier = 3` plus the description shown by `splicer builtin
  <name>`.
- A `Default`-able strategy struct named in PascalCase form of the
  Cargo package name (`hello-tier3` → `HelloTier3`), exported from
  `lib.rs`.

The splice-time codegen pipeline assumes this layout — manifest tier,
struct name, and trait impl together tell splicer which wrapper shape
to emit.

### Referencing your strategy from a splice config

Two distribution paths, same crate layout:

- **Builtin** (shipped with splicer): reference by name.

  ```yaml
  inject:
    - builtin: hello-tier3
  ```

- **User-supplied** (BYO crate): `name:` + `path:`, where `path:` is
  the **strategy crate's directory** (not a `.wasm` file).

  ```yaml
  inject:
    - name: greeter             # WAC variable name
      path: ./my-strategy       # directory: Cargo.toml + manifest.toml + src/
  ```

  Splicer detects the directory at materialize time and runs the same
  codegen + cargo pipeline as the builtin path, against your sources.
  The Cargo package name and PascalCase Rust ident are read from the
  strategy's own `Cargo.toml` — the YAML `name:` is only the WAC
  variable identifier, so it doesn't have to match the crate name.

### SDK dependency (user-supplied only)

Until `splicer-tool-sdk` is published to crates.io, your strategy
crate must depend on it via a path dependency:

```toml
[dependencies]
splicer-tool-sdk = { path = "<path to splicer's splicer-tool-sdk>" }
```

Splicer canonicalizes that path and feeds it to the generated wrapper
crate so cargo dedupes the two references into a single crate.
Registry/git deps aren't supported yet and surface a clear error at
splice time.

## Per-strategy bounds

The `TransformStrategy` trait itself has no bounds on `Args` or `R`.
Strategies that need more declare it on the impl:

| Strategy intent              | Typical bound on the impl                         |
|------------------------------|---------------------------------------------------|
| Pass-through (`hello-tier3`) | unconstrained                                     |
| Retry-on-error               | `R: IntoResult` (so the strategy can inspect Err) |
| Memoize                      | `Args: Hash, R: Clone`                            |
| Add-one-to-numeric-returns   | `R: Add<R, Output = R> + From<i32> + Copy`        |

If the bound is too tight for the target the rule wires it to, the
wrapper crate's cargo build fails with a precise Rust trait-bound
error — splicer surfaces the cargo error with a hint pointing at the
strategy's manifest. The substrate trait stays minimal so strategies
that accept any shape stay generic; per-strategy bounds opt into
narrowing.

## Limitations

- **Async targets only.** wit-bindgen emits sync `fn` Guest methods
  for `func` WIT signatures but the strategy traits are `async fn`,
  producing an E0053 type mismatch in the generated wrapper crate.
  Today tier-3 only wraps interfaces whose functions are declared
  `async func`. Sync-target support is on the roadmap.

## Good for

Request enrichment (adding headers, injecting context), response
transformation, payload encryption/redaction, content filtering, A/B
testing (routing different request variants to the same downstream),
retry-with-backoff that mutates request state between attempts.
