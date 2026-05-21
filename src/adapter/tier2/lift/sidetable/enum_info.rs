//! Enum-info side-table builder. The per-(fn, param | result) slice
//! concatenates every enum's cases in plan-walk order; each
//! [`Cell::EnumCase`] carries the start offset of its enum within
//! that slice as `entry_offset`. Cell payload at runtime is
//! `disc + entry_offset`.

use super::super::super::super::abi::emit::RecordLayout;
use super::super::super::blob::{RecordWriter, Segment, SymRef, SymbolId};
use super::super::super::FuncClassified;
use super::super::classify::ResultSource;
use super::super::plan::{Cell, LiftPlan};
use super::{SideTableBlob, INFO_TYPE_NAME};

const ENUM_INFO_CASE_NAME: &str = "case-name";

/// Build the enum-info segment + stamp each [`Cell::EnumCase`]'s
/// `entry_offset` in one plan-walk per (fn, param | result). Plan-walk
/// order = segment layout order, so each cell's stamp matches its slice.
pub(crate) fn build_enum_info_blob(
    per_func: &mut [FuncClassified],
    entry_layout: &RecordLayout,
    segment_id: SymbolId,
) -> SideTableBlob {
    let mut bytes: Vec<u8> = Vec::new();
    let mut per_param: Vec<Vec<Option<SymRef>>> = Vec::with_capacity(per_func.len());
    let mut per_result: Vec<Option<SymRef>> = Vec::with_capacity(per_func.len());
    for fd in per_func {
        let mut params = Vec::with_capacity(fd.params.len());
        for p in &mut fd.params {
            params.push(append_range(&mut bytes, entry_layout, segment_id, |visit| {
                visit_enum_cases_mut(&mut p.plan, visit)
            }));
        }
        per_param.push(params);
        let sym = match fd.result_lift.as_mut() {
            Some(rl) => match rl.compound_mut() {
                Some(c) => append_range(&mut bytes, entry_layout, segment_id, |visit| {
                    visit_enum_cases_mut(&mut c.plan, visit)
                }),
                // Direct-result enum is one standalone cell — visit it
                // once. Offset stays 0 (it's alone in its range).
                None => match &mut rl.source {
                    ResultSource::Direct(cell @ Cell::EnumCase { .. }) => {
                        append_range(&mut bytes, entry_layout, segment_id, |visit| visit(cell))
                    }
                    _ => None,
                },
            },
            None => None,
        };
        per_result.push(sym);
    }
    SideTableBlob {
        segment: Segment {
            id: segment_id,
            align: entry_layout.align,
            bytes,
            relocs: Vec::new(),
        },
        per_param,
        per_result,
    }
}

/// Append every visited cell's case-names + stamp its `entry_offset`.
/// `walk` drives which cells are visited (full-plan or one-shot).
fn append_range(
    blob: &mut Vec<u8>,
    entry_layout: &RecordLayout,
    segment_id: SymbolId,
    walk: impl FnOnce(&mut dyn FnMut(&mut Cell)),
) -> Option<SymRef> {
    let range_start = blob.len() as u32;
    let mut entry_count: u32 = 0;
    walk(&mut |cell| {
        let Cell::EnumCase {
            type_name,
            case_names,
            entry_offset,
            ..
        } = cell
        else {
            unreachable!("append_range visitor handed non-EnumCase cell: {cell:?}");
        };
        *entry_offset = entry_count;
        for case_name in case_names.iter() {
            let entry = RecordWriter::extend_zero(blob, entry_layout);
            entry.write_slice(blob, INFO_TYPE_NAME, *type_name);
            entry.write_slice(blob, ENUM_INFO_CASE_NAME, *case_name);
        }
        entry_count += case_names.len() as u32;
    });
    (entry_count > 0).then(|| SymRef {
        target: segment_id,
        off: range_start,
        len: entry_count,
    })
}

/// Pre-order visit every [`Cell::EnumCase`] in `plan`. `ListOf` is the
/// only `Cell` carrying a nested `LiftPlan`; other inter-cell links
/// are plan-relative indices already covered by the outer loop.
fn visit_enum_cases_mut(plan: &mut LiftPlan, visit: &mut dyn FnMut(&mut Cell)) {
    for cell in plan.cells.iter_mut() {
        match cell {
            Cell::EnumCase { .. } => visit(cell),
            Cell::ListOf { element_plan, .. } => visit_enum_cases_mut(element_plan, visit),
            _ => {}
        }
    }
}
