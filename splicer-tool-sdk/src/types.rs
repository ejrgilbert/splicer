//! Canonical Rust mirror of `splicer:common/types` (see
//! `wit/common/world.wit` in the splicer repo). Field/variant names
//! match what `wit_bindgen` would emit for the same WIT, so consumers
//! can point their `wit_bindgen::generate!` macro's `with:` parameter
//! at these types and share one Rust type identity across every crate
//! that participates in the splicer middleware stack.
//!
//! Paired with `wit/common/world.wit`; keep shapes in sync.

/// Identity of a wrapped call: target interface, canonical-ABI
/// function name, and a monotonic per-instance id correlating the
/// `on-call` and `on-return` of the same invocation.
#[derive(Clone, Debug, PartialEq)]
pub struct CallId {
    pub interface_name: String,
    pub function_name: String,
    pub id: u64,
}

/// `record { ... }` payload: type name plus each declared field's
/// `(name, cell-index)` pair, in WIT order.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordInfo {
    pub type_name: String,
    pub fields: Vec<(String, u32)>,
}

/// `flags { ... }` payload: type name plus the names of set bits.
#[derive(Clone, Debug, PartialEq)]
pub struct FlagsInfo {
    pub type_name: String,
    pub set_flags: Vec<String>,
}

/// `enum { ... }` payload: type name plus the case name in this value.
#[derive(Clone, Debug, PartialEq)]
pub struct EnumInfo {
    pub type_name: String,
    pub case_name: String,
}

/// `variant { ... }` payload: type name, case name, and the optional
/// payload cell index (`None` for payload-less cases).
#[derive(Clone, Debug, PartialEq)]
pub struct VariantInfo {
    pub type_name: String,
    pub case_name: String,
    pub payload: Option<u32>,
}

/// `own<R>` / `borrow<R>` / `stream<T>` / `future<T>` / `error-context`
/// payload: element type name plus an opaque correlation id. The id
/// is not a usable handle; the adapter still owns canonical-ABI
/// ownership semantics. `type_name` is empty for `error-context-handle`.
#[derive(Clone, Debug, PartialEq)]
pub struct HandleInfo {
    pub type_name: String,
    pub id: u64,
}

/// One node in a lifted value tree. Compound cases reference children
/// by `u32` index into the same `FieldTree::cells` array; nominal
/// cases reference entries in the matching side table.
///
/// Variant order matches the WIT discriminant order (downstream
/// binary encoders may rely on it).
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    // Primitives: payload fits in 8 bytes.
    Bool(bool),
    Integer(i64),
    Floating(f64),
    Text(String),
    Bytes(Vec<u8>),

    // Structural / anonymous types: index into `cells`.
    ListOf(Vec<u32>),
    TupleOf(Vec<u32>),
    OptionSome(u32),
    OptionNone,
    ResultOk(Option<u32>),
    ResultErr(Option<u32>),

    // Nominal types: index into the corresponding side table.
    RecordOf(u32),
    FlagsSet(u32),
    EnumCase(u32),
    VariantCase(u32),

    // Opaque correlation handles: index into `handle_infos`.
    ResourceHandle(u32),
    StreamHandle(u32),
    FutureHandle(u32),
    ErrorContextHandle(u32),
}

impl Cell {
    /// Short, stable label for this cell's kind (e.g. `"text"`, `"list"`,
    /// `"resource"`). One exhaustive match beside the type: adding a
    /// `Cell` variant stops this compiling until a label is supplied, so
    /// there is no parallel table to drift. Useful for diagnostics and
    /// for tagging metrics/traces by value kind.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Cell::Bool(_) => "bool",
            Cell::Integer(_) => "integer",
            Cell::Floating(_) => "floating",
            Cell::Text(_) => "text",
            Cell::Bytes(_) => "bytes",
            Cell::ListOf(_) => "list",
            Cell::TupleOf(_) => "tuple",
            Cell::OptionSome(_) => "option-some",
            Cell::OptionNone => "option-none",
            Cell::ResultOk(_) => "result-ok",
            Cell::ResultErr(_) => "result-err",
            Cell::RecordOf(_) => "record",
            Cell::FlagsSet(_) => "flags",
            Cell::EnumCase(_) => "enum",
            Cell::VariantCase(_) => "variant",
            Cell::ResourceHandle(_) => "resource",
            Cell::StreamHandle(_) => "stream",
            Cell::FutureHandle(_) => "future",
            Cell::ErrorContextHandle(_) => "error-context",
        }
    }
}

/// A lifted value tree: a flat array of cells, side tables for
/// nominal-typed information, plus the root cell index. Walk by
/// reading `cells[root]` and following child indices.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldTree {
    pub cells: Vec<Cell>,
    pub record_infos: Vec<RecordInfo>,
    pub flags_infos: Vec<FlagsInfo>,
    pub enum_infos: Vec<EnumInfo>,
    pub variant_infos: Vec<VariantInfo>,
    pub handle_infos: Vec<HandleInfo>,
    pub root: u32,
}

// Assumed sizes of different WIT types
const LIST_HEADER: u64 = 8;
const DISCRIMINANT: u64 = 1;
const HANDLE: u64 = 4;
// Flags capped at 32 labels in component model.
// Assume max size here (lifted cells lose exactness)
const FLAGS: u64 = 4;

impl FieldTree {
    /// Child cell indices in WIT order
    pub fn child_indices(&self, cell: &Cell) -> Vec<u32> {
        match cell {
            Cell::ListOf(ix) | Cell::TupleOf(ix) => ix.clone(),
            Cell::OptionSome(i) => vec![*i],
            Cell::ResultOk(o) | Cell::ResultErr(o) => o.iter().copied().collect(),
            Cell::RecordOf(i) => self
                .record_infos
                .get(*i as usize)
                .map(|r| r.fields.iter().map(|(_, c)| *c).collect())
                .unwrap_or_default(),
            Cell::VariantCase(i) => self
                .variant_infos
                .get(*i as usize)
                .and_then(|v| v.payload)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    /// *Estimated* canonical-ABI byte size of the value this tree represents.
    pub fn size_est(&self) -> u64 {
        let mut total = 0;
        let mut visited = vec![false; self.cells.len()];
        let mut stack = vec![self.root];
        while let Some(idx) = stack.pop() {
            let ui = idx as usize;
            if ui >= self.cells.len() || visited[ui] {
                continue;
            }
            visited[ui] = true;
            let cell = &self.cells[ui];
            total += cell_own_size(cell);
            stack.extend(self.child_indices(cell));
        }
        total
    }

}

/// *Estimated* canonical-ABI size of `cell`'s own representation,
/// excluding children (summed separately by [`FieldTree::size_est`]).
fn cell_own_size(cell: &Cell) -> u64 {
    use core::mem::size_of;
    match cell {
        Cell::Bool(_) => size_of::<bool>() as u64,
        Cell::Integer(_) => size_of::<i64>() as u64,
        Cell::Floating(_) => size_of::<f64>() as u64,
        // Descriptor + out-of-line content bytes.
        Cell::Text(s) => LIST_HEADER + s.len() as u64,
        Cell::Bytes(b) => LIST_HEADER + b.len() as u64,
        // Descriptor only; elements are summed as child cells.
        Cell::ListOf(_) => LIST_HEADER,
        // Laid out inline; fields/elements are the summed child cells.
        Cell::TupleOf(_) | Cell::RecordOf(_) => 0,
        // Discriminant; the payload (if any) is a summed child cell.
        Cell::OptionSome(_)
        | Cell::OptionNone
        | Cell::ResultOk(_)
        | Cell::ResultErr(_)
        | Cell::VariantCase(_)
        | Cell::EnumCase(_) => DISCRIMINANT,
        Cell::FlagsSet(_) => FLAGS,
        Cell::ResourceHandle(_)
        | Cell::StreamHandle(_)
        | Cell::FutureHandle(_)
        | Cell::ErrorContextHandle(_) => HANDLE,
    }
}

/// Named slot in a function's argument list. Results are unnamed and
/// surfaced as a bare `FieldTree`; only arguments carry a `Field`
/// wrapper.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub tree: FieldTree,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(cells: Vec<Cell>, root: u32) -> FieldTree {
        FieldTree {
            cells,
            record_infos: vec![],
            flags_infos: vec![],
            enum_infos: vec![],
            variant_infos: vec![],
            handle_infos: vec![],
            root,
        }
    }

    #[test]
    fn kind_name_is_stable() {
        assert_eq!(Cell::Text(String::new()).kind_name(), "text");
        assert_eq!(Cell::ListOf(vec![]).kind_name(), "list");
        assert_eq!(Cell::ResourceHandle(0).kind_name(), "resource");
    }

    #[test]
    fn estimated_size_of_scalars() {
        assert_eq!(tree(vec![Cell::Bool(true)], 0).size_est(), 1);
        assert_eq!(tree(vec![Cell::Integer(1)], 0).size_est(), 8);
        // string = 8-byte descriptor + utf-8 content.
        assert_eq!(tree(vec![Cell::Text("hello".into())], 0).size_est(), 13);
        assert_eq!(tree(vec![], 0).size_est(), 0);
    }

    #[test]
    fn estimated_size_follows_children() {
        // record { name: "abc", tags: list["x","y"], status: result-err }
        let mut t = tree(
            vec![
                Cell::RecordOf(0),
                Cell::Text("abc".into()),   // 8 + 3
                Cell::ListOf(vec![3, 4]),   // 8 header
                Cell::Text("x".into()),     // 8 + 1
                Cell::Text("y".into()),     // 8 + 1
                Cell::ResultErr(None),      // 1 discriminant
            ],
            0,
        );
        t.record_infos = vec![RecordInfo {
            type_name: "rec".into(),
            fields: vec![("name".into(), 1), ("tags".into(), 2), ("status".into(), 5)],
        }];
        // 0 + 11 + 8 + 9 + 9 + 1 = 38.
        assert_eq!(t.size_est(), 38);
    }

    #[test]
    fn flags_sized_at_u32_max() {
        // The label count isn't recoverable from the lifted tree, so a
        // flags value is sized at the 4-byte max (CM caps flags at 32).
        let mut t = tree(vec![Cell::FlagsSet(0)], 0);
        t.flags_infos = vec![FlagsInfo {
            type_name: "perms".into(),
            set_flags: vec!["read".into(), "write".into()],
        }];
        assert_eq!(t.size_est(), 4);
    }

    #[test]
    fn cyclic_indices_terminate() {
        assert_eq!(tree(vec![Cell::ListOf(vec![0])], 0).size_est(), 8);
    }
}
