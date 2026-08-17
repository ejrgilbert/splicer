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

/// Named slot in a function's argument list. Results are unnamed and
/// surfaced as a bare `FieldTree`; only arguments carry a `Field`
/// wrapper.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub tree: FieldTree,
}
