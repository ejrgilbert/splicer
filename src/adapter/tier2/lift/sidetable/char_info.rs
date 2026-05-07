//! Char per-cell scratch sizing + addr map. No segment of its own —
//! `Cell::Char` lifts to a `cell::text` whose `(ptr, len)` references
//! a per-cell scratch buffer (4 bytes max for utf-8). The layout phase
//! reserves the scratch slabs; the per-cell map gives each
//! plan-embedded char its base, and the per-result-single map gives
//! each Direct (sync flat) char result its base.
//!
//! Walk order is locked: params (per plan), then compound result plan
//! (when present) OR Direct char-result entry (when present).
//! [`char_scratch_sizes`] and [`build_char_scratch_map`] must walk in
//! lockstep; a divergence crashes the builder's `scratch_addrs.next()`.
//!
//! Outer plan only — list-element chars use per-call scratch from
//! [`super::super::emit::emit_list_of_arm`]; see
//! [`super::fold_cell_side_data`] for the lockstep rule.

use super::super::super::super::abi::emit::MAX_UTF8_LEN;
use super::super::super::FuncClassified;
use super::super::classify::ResultSource;
use super::super::plan::{Cell, LiftPlan};
use super::PerCellIndices;

/// Whether `fd`'s result is a Direct (sync flat) `Cell::Char`.
/// Retptr-routed char results (async or otherwise) ride Compound
/// and register via the plan walk. Reads from the classified `Cell`
/// variant so type aliases (`type my-char = char`) are handled.
fn result_is_direct_char(fd: &FuncClassified) -> bool {
    let Some(rl) = &fd.result_lift else {
        return false;
    };
    matches!(rl.source, ResultSource::Direct(Cell::Char { .. }))
}

/// Output of [`build_char_scratch_map`].
pub(crate) struct CharScratchMaps {
    /// Per-(fn, param | compound-result) per-plan-cell scratch addr.
    pub per_cell: PerCellIndices<i32>,
    /// Per-fn scratch addr for a Direct (sync flat) char-result.
    /// `Some` when the func's result classifies as Direct(Cell::Char).
    pub per_result_single: Vec<Option<i32>>,
}

/// Per-`Cell::Char` scratch byte count, in plan-walk order matching
/// [`build_char_scratch_map`]'s consumption.
pub(crate) fn char_scratch_sizes(per_func: &[FuncClassified]) -> Vec<u32> {
    let mut sizes = Vec::new();
    for fd in per_func {
        for p in &fd.params {
            collect(&p.plan, &mut sizes);
        }
        if let Some(rl) = &fd.result_lift {
            if let Some(c) = rl.compound() {
                collect(&c.plan, &mut sizes);
            } else if result_is_direct_char(fd) {
                sizes.push(MAX_UTF8_LEN);
            }
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
/// `Cell::Char` cells, plus per-fn single-cell char-result addr.
/// Caller supplies pre-reserved scratch addresses in the same
/// plan-walk order as [`char_scratch_sizes`].
pub(crate) fn build_char_scratch_map(
    per_func: &[FuncClassified],
    scratch_addrs: &mut impl Iterator<Item = u32>,
) -> CharScratchMaps {
    let mut per_param: Vec<Vec<Vec<Option<i32>>>> = Vec::with_capacity(per_func.len());
    let mut per_result: Vec<Vec<Option<i32>>> = Vec::with_capacity(per_func.len());
    let mut per_result_single: Vec<Option<i32>> = Vec::with_capacity(per_func.len());
    for fd in per_func {
        let mut params = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            params.push(map_plan(&p.plan, scratch_addrs));
        }
        per_param.push(params);
        let (compound_map, single) = match fd.result_lift.as_ref() {
            Some(rl) if rl.compound().is_some() => {
                let c = rl.compound().expect("matched Some above");
                (map_plan(&c.plan, scratch_addrs), None)
            }
            _ if result_is_direct_char(fd) => (
                Vec::new(),
                Some(
                    scratch_addrs
                        .next()
                        .expect("layout phase must reserve one scratch slot per char result")
                        as i32,
                ),
            ),
            _ => (Vec::new(), None),
        };
        per_result.push(compound_map);
        per_result_single.push(single);
    }
    CharScratchMaps {
        per_cell: PerCellIndices {
            per_param,
            per_result,
        },
        per_result_single,
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
