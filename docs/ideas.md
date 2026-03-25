# Generate Proxy Component #

Proxy component adapters for ease of middleware injection.

A middleware that only imports from the host can be wrapped with an adapter to satisfy the
type requirements of the chain. Basically this would be an identity proxy component that
calls the middleware, calls the downstream component, and returns the downstream component's
output. This would only be necessary for middleware that does not import the downstream chain
function. So something like an OTel component.

This can be started once I have type information in the composition graph (waiting on the
following PRs):
- [ ] https://github.com/composablesys/wirm/pull/307
- [ ] https://github.com/composablesys/wirm/pull/309
- [ ] https://github.com/cosmonic-labs/cviz/pull/9

Some resources that could be helpful here:
- [ ] https://github.com/chenyan2002/proxy-component/tree/main/src
- [ ] https://github.com/bytecodealliance/wasm-tools/tree/main/crates/wit-dylib
  - [Example that generates the lift](https://github.com/bytecodealliance/wasm-tools/blob/main/crates/wit-dylib/src/bindgen.rs#L768)

Make sure to extend the type checking error to include information on whether a proxy component could be generated.
If it can, print how that could be configured.
