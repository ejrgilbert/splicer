# resource-wrap-poc

End-to-end proof that resource **method** calls can be intercepted by a
splice wrapper, the open problem tracked in
`docs/TODO/resource-method-interception.md`.

The previously-blocked approach tried to make the wrapper re-export the
*same* resource type identity as the producer (unsupported in WIT,
wasm-tools#2506). This POC sidesteps it: the wrapper owns a **fresh**
resource type `T'`, holds the real `T` inside, and **forwards** method
calls in Rust. The consumer is rewired (cross-name) to `T'`. No resource
identity is bridged across the wrapper, so the WIT limitation never bites.

Verified against wac 0.10.0 / wasm-tools 1.247.0 / wasmtime 34 /
wit-bindgen 0.51 / cargo-component 0.21.

## Layout

- `realprov/`  -- provider, exports `host:kv/store` (bucket declared **inline**).
- `wrapper/`   -- the third component. Imports `host:kv/store`, exports
  `splice:wrap/store` (T' = forwarding bucket that holds the real one) and
  `splice:wrap/bridge` (`wrap(raw-bucket) -> wrapped-bucket` /
  `unwrap(wrapped-bucket) -> raw-bucket`). The bridge names both buckets
  distinctly via `use host:kv/store.{bucket as raw-bucket}; use store.{bucket as wrapped-bucket}`.
- `edge/`      -- driver exercising the bridge: write through a raw handle,
  `wrap` it, read through T' (forwards), `unwrap`, read through the
  recovered raw handle.
- `compose.wac` + `run.sh` -- build, compose, run.

## Run

```
./run.sh
```

Expected: `"via_t=Some(\"fromraw\") via_raw=Some(\"fromraw\")"`.
`via_t` proves T' forwards to the held real handle; `via_raw` proves
`unwrap` recovers the *exact same* underlying handle (round-trip identity,
no copy).

## wac-checks/ -- the two load-bearing wac facts, isolated

- `width-subtyping/`: consumer imports `bucket {ctor,get,set}`; `provider`
  exports the same id with extra `wrap`+`tag`. `wac plug` composes
  (width subtyping: a superset export satisfies a subset import).
  `provider-missing` (no `set`) is the negative control -- rejected with
  "no matching imports", proving wac really checks the method set.

  ```
  cd wac-checks/width-subtyping
  (cd consumer && cargo component build --release); (cd provider && cargo component build --release)
  wac plug consumer/target/wasm32-wasip1/release/consumer.wasm \
    --plug provider/target/wasm32-wasip1/release/provider.wasm -o /tmp/ok.wasm   # composes
  ```

- `cross-name/`: `consumer-orig` imports `test:orig/store`;
  `provider-wrapped` exports a structurally identical `test:wrapped/store`
  (different interface id). Auto `wac plug` by name does NOT match, but
  explicit `wac compose` wiring does. This is the consumer-rewire the real
  splice needs (consumer built against the original interface, wired to the
  wrapper's fresh interface).

  ```
  cd wac-checks/cross-name
  # build both, then:
  wac compose --dep test:orig=.../consumer.wasm --dep test:wrapped=.../provider.wasm compose.wac -o /tmp/y.wasm
  ```

## cargo-component gotcha

Local WIT deps must be declared in
`[package.metadata.component.target.dependencies]` with a path; dropping
files in `wit/deps/` alone is not enough for `cargo component build`.
