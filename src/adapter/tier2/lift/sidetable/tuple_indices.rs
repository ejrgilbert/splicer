//! Tuple-indices side-table builder.
//!
//! Each `Cell::TupleOf` cell carries a `[u32; N]` of child plan-cell
//! positions in its `(ptr, len)` payload. Indices are adapter-build-
//! time constants, so the arrays go in one shared static segment.
//!
//! No internal relocs — the cell payload is materialized at emit
//! time from a resolved [`super::super::super::super::abi::emit::BlobSlice`],
//! not patched in the segment bytes.

use super::super::super::blob::{Segment, SymRef, SymbolId};
use super::super::super::FuncClassified;
use super::super::plan::{Cell, LiftPlan};
use super::PerCellIndices;

const U32_SIZE: u32 = 4;

/// Output of [`build_tuple_indices_blob`]: the packed `[u32]`
/// segment plus a [`PerCellIndices<SymRef>`] keyed by (fn, param |
/// result) × plan-cell. The layout phase calls
/// [`PerCellIndices::resolve_param`] / [`PerCellIndices::resolve_result`]
/// to materialize absolute [`super::super::super::super::abi::emit::BlobSlice`]s.
pub(crate) struct TupleIndicesBlob {
    pub segment: Segment,
    pub per_cell_idx: PerCellIndices<SymRef>,
}

pub(crate) fn build_tuple_indices_blob(
    per_func: &[FuncClassified],
    segment_id: SymbolId,
) -> TupleIndicesBlob {
    let mut bytes: Vec<u8> = Vec::new();
    let mut per_param: Vec<Vec<Vec<Option<SymRef>>>> = Vec::with_capacity(per_func.len());
    let mut per_result: Vec<Vec<Option<SymRef>>> = Vec::with_capacity(per_func.len());

    for fd in per_func {
        let params_syms: Vec<Vec<Option<SymRef>>> = fd
            .params
            .iter()
            .map(|p| append_plan(&mut bytes, segment_id, &p.plan))
            .collect();
        per_param.push(params_syms);

        let result_syms = match fd.result_lift.as_ref().and_then(|rl| rl.compound()) {
            Some(c) => append_plan(&mut bytes, segment_id, &c.plan),
            None => Vec::new(),
        };
        per_result.push(result_syms);
    }

    TupleIndicesBlob {
        segment: Segment {
            id: segment_id,
            align: U32_SIZE,
            bytes,
            relocs: Vec::new(),
        },
        per_cell_idx: PerCellIndices {
            per_param,
            per_result,
        },
    }
}

fn append_plan(bytes: &mut Vec<u8>, segment_id: SymbolId, plan: &LiftPlan) -> Vec<Option<SymRef>> {
    let mut cell_idx_map: Vec<Option<SymRef>> = vec![None; plan.cells.len()];
    for (cell_pos, op) in plan.cells.iter().enumerate() {
        let Cell::TupleOf { children } = op else {
            continue;
        };
        let off = bytes.len() as u32;
        let len = children.len() as u32;
        for &child_idx in children {
            bytes.extend_from_slice(&child_idx.to_le_bytes());
        }
        cell_idx_map[cell_pos] = Some(SymRef {
            target: segment_id,
            off,
            len,
        });
    }
    cell_idx_map
}
