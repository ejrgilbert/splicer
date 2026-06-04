# Canonical-ABI gaps

- **Nested positions overflow.** The canonical ABI only requires a
  flat representation at the function boundary — types nested in
  memory (list elements, variant/result arms, compound-result
  fields) can be lifted field-by-field via `lift_from_memory`
  without any flat intermediate. Splicer2 currently materializes
  per-position flat locals anyway: `push_list_of` (element
  flat-slots staged once, written per iteration), `push_result` /
  `push_variant` (joined arm flat-types for slot widening), and the
  compound-result emit path (synth-locals across the whole result).
  When a nested type's flat exceeds 16, all four bail with
  `"flat representation exceeds MAX_FLAT_PARAMS"`. Each site needs
  a memory-direct alternative (skip the flat-local staging, walk
  the cell tree against the source address) that the plan-builder
  can pick when overflow is detected. Structurally similar to
  `build_lift_params_from_memory`, but applied per-position.

- **Async-asymmetric single-param flat >16.** Independent of the
  nested-overflow item above, but shares its error string. Fires in
  `require_indirect_params_supported_shape` (`abi/emit.rs:403`) when
  an async func's total flat exceeds `MAX_FLAT_ASYNC_PARAMS` (4) AND
  some single param's own flat exceeds `MAX_FLAT_PARAMS` (16). The
  constraint comes from `build_lower_params_to_memory`: it allocates
  per-param flat wasm locals from a single bindgen whose
  `param_flat_locals` slice can't exceed 16 entries per param.
  Sync indirect-params doesn't have this restriction (the wrapper
  passes the host's pointer through directly; no lower-to-memory
  pass). Fix: either swap the bindgen for a memory-direct lower on
  this corner, or widen the `param_flat_locals` slice. Niche shape
  (5..16 total flat plus a single fat param), but worth noting
  because the bail string overlaps with the nested-overflow item.

- **Anonymous compound types as top-level results.** When a Record /
  Variant / Enum appears as a func result but isn't in
  `iface.type_exports` (unusual in WIT-compiled interfaces, but
  legal at the component-model level), the adapter's export-instance
  construction can't re-export the compound — the binary fails
  validation with "instance not valid to be used as export." Fix:
  synthesize names + auto-export in the export-emit pass. Low
  priority since real WIT always names its compounds.
