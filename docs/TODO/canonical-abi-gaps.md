# Canonical-ABI gaps

Two known limitations that still surface as `anyhow::bail!` errors:

- **Flat params / results > 16 — pointer-form lowering.** The
  canonical ABI collapses to `(i32)` pointer form when a function's
  flat representation exceeds the `MAX_FLAT_PARAMS` cap (16, defined
  in `abi/compat.rs`). Tier-1 and tier-2 both bail at this boundary
  today instead of silently declaring wrong core types.
  Implementing pointer-form needs: `params_are_ptr` /
  `results_are_ptr` flags carried through the per-fn dispatch
  records, pointer-form type declarations in every dispatch emitter,
  and a memory-layout buffer reservation for the spilled args.

  *Nested positions also overflow.* The canonical ABI only requires
  a flat representation at the function boundary — types nested in
  memory (list elements, variant/result arms, compound-result fields)
  can be lifted field-by-field via `lift_from_memory` without any
  flat intermediate. Splicer2 currently materializes per-position
  flat locals anyway: `push_list_of` (element flat-slots staged once,
  written per iteration), `push_result` / `push_variant` (joined arm
  flat-types for slot widening), and the compound-result emit path
  (synth-locals across the whole result). When a nested type's flat
  exceeds 16, all four bail with `"flat representation exceeds
  MAX_FLAT_PARAMS"`. Lifting these limits is independent of the
  function-boundary pointer-form work: each site needs a
  memory-direct alternative (skip the flat-local staging, walk the
  cell tree against the source address) that the plan-builder can
  pick when overflow is detected.

- **Anonymous compound types as top-level results.** When a Record /
  Variant / Enum appears as a func result but isn't in
  `iface.type_exports` (unusual in WIT-compiled interfaces, but
  legal at the component-model level), the adapter's export-instance
  construction can't re-export the compound — the binary fails
  validation with "instance not valid to be used as export." Fix:
  synthesize names + auto-export in the export-emit pass. Low
  priority since real WIT always names its compounds.
