//! Enum-info side-table builder. Convenience facades over the
//! generic [`super::register_side_table_strings`] /
//! [`super::build_side_table_blob`] tied to `Cell::EnumCase` +
//! `enum_info` field on `SideTableInfo`.

use super::super::super::super::abi::emit::RecordLayout;
use super::super::super::blob::{NameInterner, SymbolId};
use super::super::super::FuncClassified;
use super::{
    build_side_table_blob, register_side_table_strings, SideTableBlob, SideTableSpec, StringTable,
};

const ENUM_INFO_CASE_NAME: &str = "case-name";

/// Register enum-info strings for every enum-typed lift across all
/// funcs. Thin wrapper over [`register_side_table_strings`] tied to
/// `Cell::EnumCase`.
pub(crate) fn register_enum_strings(
    per_func: &[FuncClassified],
    names: &mut NameInterner,
) -> StringTable {
    register_side_table_strings(
        per_func,
        names,
        |plan| plan.enum_infos().collect(),
        |st| st.enum_info.as_ref(),
    )
}

/// Build the enum-info side-table blob. Thin wrapper over
/// [`build_side_table_blob`] tied to the `enum-info` record + the
/// `enum_info` field on `SideTableInfo`.
///
/// Today's plans have at most one `EnumCase` per param (enum is a
/// primitive at the param level — only nested-in-record enums could
/// produce multiple, and that's not yet supported). When that lands,
/// the per-plan extractor will need to surface multiple infos per
/// plan, with the side-table builder appending contiguous ranges
/// per plan-cell.
pub(crate) fn build_enum_info_blob(
    per_func: &[FuncClassified],
    strings: &StringTable,
    entry_layout: &RecordLayout,
    segment_id: SymbolId,
) -> SideTableBlob {
    build_side_table_blob(
        per_func,
        strings,
        &SideTableSpec {
            entry_layout,
            item_name_field: ENUM_INFO_CASE_NAME,
        },
        segment_id,
        |plan| plan.enum_infos().next(),
        |st| st.enum_info.as_ref(),
    )
}
