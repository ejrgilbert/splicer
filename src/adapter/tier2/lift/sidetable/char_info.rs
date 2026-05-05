//! Char per-cell scratch sizing + addr map. No segment of its own —
//! `Cell::Char` lifts to a `cell::text` whose `(ptr, len)` references
//! a per-cell scratch buffer (4 bytes max for utf-8). The layout phase
//! reserves the scratch slabs and the per-cell map gives each cell
//! its absolute scratch base.

use super::super::super::super::abi::emit::MAX_UTF8_LEN;
use super::super::super::FuncClassified;
use super::super::plan::{Cell, LiftPlan};
use super::PerCellIndices;

/// Per-`Cell::Char` scratch byte count, in plan-walk order matching
/// [`build_char_scratch_map`]'s consumption.
pub(crate) fn char_scratch_sizes(per_func: &[FuncClassified]) -> Vec<u32> {
    let mut sizes = Vec::new();
    for fd in per_func {
        for p in &fd.params {
            collect(&p.plan, &mut sizes);
        }
        if let Some(c) = fd.result_lift.as_ref().and_then(|rl| rl.compound()) {
            collect(&c.plan, &mut sizes);
        }
    }
    sizes
}

fn collect(plan: &LiftPlan, sizes: &mut Vec<u32>) {
    for cell in &plan.cells {
        if matches!(cell, Cell::Char { .. }) {
            sizes.push(MAX_UTF8_LEN);
        }
    }
}

/// Per-(fn, param | result) per-plan-cell scratch base for
/// `Cell::Char`. `Some(addr)` on Char cells, `None` elsewhere. Caller
/// supplies pre-reserved scratch addresses in the same plan-walk
/// order as [`char_scratch_sizes`].
pub(crate) fn build_char_scratch_map(
    per_func: &[FuncClassified],
    scratch_addrs: &mut impl Iterator<Item = u32>,
) -> PerCellIndices<i32> {
    let mut per_param: Vec<Vec<Vec<Option<i32>>>> = Vec::with_capacity(per_func.len());
    let mut per_result: Vec<Vec<Option<i32>>> = Vec::with_capacity(per_func.len());
    for fd in per_func {
        let mut params = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            params.push(map_plan(&p.plan, scratch_addrs));
        }
        per_param.push(params);
        per_result.push(
            fd.result_lift
                .as_ref()
                .and_then(|rl| rl.compound())
                .map(|c| map_plan(&c.plan, scratch_addrs))
                .unwrap_or_default(),
        );
    }
    PerCellIndices {
        per_param,
        per_result,
    }
}

fn map_plan(plan: &LiftPlan, scratch_addrs: &mut impl Iterator<Item = u32>) -> Vec<Option<i32>> {
    plan.cells
        .iter()
        .map(|cell| match cell {
            Cell::Char { .. } => Some(
                scratch_addrs
                    .next()
                    .expect("layout phase must reserve one scratch slot per Cell::Char")
                    as i32,
            ),
            _ => None,
        })
        .collect()
}
