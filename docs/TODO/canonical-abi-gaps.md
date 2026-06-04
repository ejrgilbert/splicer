# Canonical-ABI gaps

- **Function-boundary pointer-form (params >16, results >1).**
  *Resolved.* Sync funcs whose params overflow `MAX_FLAT_PARAMS`
  (16) now flip the export sig to `(i32) -> …` (host writes the
  params record into our linear memory, hands the wrapper the
  pointer). Sync result overflow (`MAX_FLAT_RESULTS = 1` on wasm32)
  was already handled by the existing retptr machinery. See:

    - `require_indirect_params_supported_shape` in `abi/emit.rs` —
      now `is_async`-aware; the per-param flat-fits-in-16 check only
      gates the asymmetric `build_lower_params_to_memory` path
      (async-stackful 5..16 corner).
    - `build_lift_params_from_memory` in `abi/emit.rs` — sibling of
      `build_lower_params_to_memory`; tier-2's symmetric-indirect
      hook lift uses it to materialize each param's flat into synth
      locals from the host's params pointer.
    - Validation gates `tier1/emit.rs:require_supported_case` and
      `tier2/mod.rs:require_supported_case` — drop the async-only
      `if func.kind.is_async()` guard.
    - Reservation + `params_lower_seq` gates — both keyed on the
      asymmetric flip `import_sig.indirect_params &&
      !export_sig.indirect_params`; symmetric flips pass `local 0`
      through directly.

  Async params >16 (`GuestExportAsyncStackful` overflowing 16)
  inherits the symmetric-indirect plumbing for free but isn't
  fuzz-tested yet.

- **Nested positions still overflow.** The canonical ABI only
  requires a flat representation at the function boundary — types
  nested in memory (list elements, variant/result arms, compound-
  result fields) can be lifted field-by-field via `lift_from_memory`
  without any flat intermediate. Splicer2 currently materializes
  per-position flat locals anyway: `push_list_of` (element
  flat-slots staged once, written per iteration), `push_result` /
  `push_variant` (joined arm flat-types for slot widening), and the
  compound-result emit path (synth-locals across the whole result).
  When a nested type's flat exceeds 16, all four bail with
  `"flat representation exceeds MAX_FLAT_PARAMS"`. Lifting these
  limits is independent of the function-boundary work above: each
  site needs a memory-direct alternative (skip the flat-local
  staging, walk the cell tree against the source address) that the
  plan-builder can pick when overflow is detected.

- **Anonymous compound types as top-level results.** When a Record /
  Variant / Enum appears as a func result but isn't in
  `iface.type_exports` (unusual in WIT-compiled interfaces, but
  legal at the component-model level), the adapter's export-instance
  construction can't re-export the compound — the binary fails
  validation with "instance not valid to be used as export." Fix:
  synthesize names + auto-export in the export-emit pass. Low
  priority since real WIT always names its compounds.
