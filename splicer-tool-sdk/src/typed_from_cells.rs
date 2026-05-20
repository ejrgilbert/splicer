//! Decode a [`FieldTree`] into a typed Rust value.
//!
//! Use this when you have a `FieldTree` (e.g. read back from a
//! recorder trace) and need it as a strongly-typed Rust value
//! matching the wrapped target's WIT shape. The [`TypedFromCells`]
//! trait is the consumer of the derive macro re-exported as
//! [`crate::TypedFromCells`]; splicer's per-target codegen template
//! wires the derive in via wit-bindgen's `additional_derives`, so
//! every generated record / variant / enum picks up an impl
//! automatically.
//!
//! # Example: decode a hand-built tree of an `option<string>`
//!
//! ```
//! use splicer_tool_sdk::{Cell, FieldTree, TypedFromCells};
//!
//! let tree = FieldTree {
//!     cells: vec![Cell::Text("hello".into()), Cell::OptionSome(0)],
//!     record_infos: vec![],
//!     flags_infos: vec![],
//!     enum_infos: vec![],
//!     variant_infos: vec![],
//!     handle_infos: vec![],
//!     root: 1,
//! };
//! let v: Option<String> = TypedFromCells::from_cells(&tree, tree.root).unwrap();
//! assert_eq!(v.as_deref(), Some("hello"));
//! ```

use crate::types::{Cell, FieldTree};
use std::fmt;

/// Result alias for [`TypedFromCells::from_cells`] callers.
pub type FromCellsResult<T> = Result<T, FromCellsError>;

/// Decode failure: cell shape didn't match the expected type, an
/// index was out of bounds, a nominal type name didn't match a known
/// case, etc. Carries a free-form message describing the mismatch;
/// stable as a `Display` string, not as a structured discriminant.
#[derive(Debug)]
pub struct FromCellsError(String);

impl FromCellsError {
    pub fn new(msg: impl Into<String>) -> Self {
        FromCellsError(msg.into())
    }
}

impl fmt::Display for FromCellsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for FromCellsError {}

/// Types that can be decoded from a [`FieldTree`] node.
///
/// The codegen template emits a `#[derive(TypedFromCells)]` for
/// every record / variant / enum wit-bindgen generates from a
/// target's WIT; primitives, `Option`, `Result`, `Vec`, and tuples
/// pick up blanket impls below. Strategies (e.g. replay) place
/// `R: TypedFromCells` on their `impl WrapperStrategy<Args, R>`
/// where-clause to require this capability.
pub trait TypedFromCells: Sized {
    /// Decode the cell at `root` (an index into `tree.cells`) into
    /// `Self`. Recurses through child indices for compound cells
    /// and reads side-table entries for nominal cells.
    fn from_cells(tree: &FieldTree, root: u32) -> FromCellsResult<Self>;

    /// Fast-path hook for `Vec<u8>`: when tier-2 emits a `list<u8>`
    /// as the compact [`Cell::Bytes`] form, the blanket
    /// `Vec<T>` impl decodes each byte through this method instead
    /// of synthesizing a per-element [`Cell::Integer`]. Override on
    /// `u8` only; the default errors out for every other type.
    fn from_byte(_b: u8) -> FromCellsResult<Self> {
        Err(FromCellsError::new(format!(
            "cannot decode a byte into {}",
            std::any::type_name::<Self>()
        )))
    }
}

/// Look up `tree.cells[idx]` with a contract-violation error on
/// out-of-bounds. Public to the derive output; not part of the
/// stable API.
#[doc(hidden)]
pub fn __get_cell(tree: &FieldTree, idx: u32) -> FromCellsResult<&Cell> {
    tree.cells.get(idx as usize).ok_or_else(|| {
        FromCellsError::new(format!(
            "cell index {idx} out of bounds (cells.len() = {})",
            tree.cells.len()
        ))
    })
}

// ----- Primitive impls --------------------------------------------------

impl TypedFromCells for bool {
    fn from_cells(tree: &FieldTree, root: u32) -> FromCellsResult<Self> {
        match __get_cell(tree, root)? {
            Cell::Bool(b) => Ok(*b),
            other => Err(FromCellsError::new(format!(
                "expected Bool at cell {root}, got {other:?}"
            ))),
        }
    }
}

// `u8` is written by hand so it can override `from_byte` for the
// Vec<u8>/Cell::Bytes fast path; the macro below handles the rest.
impl TypedFromCells for u8 {
    fn from_cells(tree: &FieldTree, root: u32) -> FromCellsResult<Self> {
        match __get_cell(tree, root)? {
            Cell::Integer(i) => u8::try_from(*i).map_err(|e| {
                FromCellsError::new(format!("Integer {i} out of range for u8: {e}"))
            }),
            other => Err(FromCellsError::new(format!(
                "expected Integer at cell {root}, got {other:?}"
            ))),
        }
    }

    fn from_byte(b: u8) -> FromCellsResult<Self> {
        Ok(b)
    }
}

macro_rules! impl_integer {
    ($($t:ty),* $(,)?) => {$(
        impl TypedFromCells for $t {
            fn from_cells(tree: &FieldTree, root: u32) -> FromCellsResult<Self> {
                match __get_cell(tree, root)? {
                    Cell::Integer(i) => <$t>::try_from(*i).map_err(|e| {
                        FromCellsError::new(format!(
                            "Integer {i} out of range for {}: {e}",
                            stringify!($t)
                        ))
                    }),
                    other => Err(FromCellsError::new(format!(
                        "expected Integer at cell {root}, got {other:?}"
                    ))),
                }
            }
        }
    )*};
}

impl_integer!(u16, u32, u64, i8, i16, i32, i64);

impl TypedFromCells for f32 {
    fn from_cells(tree: &FieldTree, root: u32) -> FromCellsResult<Self> {
        match __get_cell(tree, root)? {
            Cell::Floating(f) => Ok(*f as f32),
            other => Err(FromCellsError::new(format!(
                "expected Floating at cell {root}, got {other:?}"
            ))),
        }
    }
}

impl TypedFromCells for f64 {
    fn from_cells(tree: &FieldTree, root: u32) -> FromCellsResult<Self> {
        match __get_cell(tree, root)? {
            Cell::Floating(f) => Ok(*f),
            other => Err(FromCellsError::new(format!(
                "expected Floating at cell {root}, got {other:?}"
            ))),
        }
    }
}

impl TypedFromCells for char {
    fn from_cells(tree: &FieldTree, root: u32) -> FromCellsResult<Self> {
        match __get_cell(tree, root)? {
            Cell::Integer(i) => {
                let u = u32::try_from(*i)
                    .map_err(|_| FromCellsError::new(format!("char codepoint {i} negative")))?;
                char::from_u32(u)
                    .ok_or_else(|| FromCellsError::new(format!("invalid char codepoint {u}")))
            }
            other => Err(FromCellsError::new(format!(
                "expected Integer (for char) at cell {root}, got {other:?}"
            ))),
        }
    }
}

impl TypedFromCells for String {
    fn from_cells(tree: &FieldTree, root: u32) -> FromCellsResult<Self> {
        match __get_cell(tree, root)? {
            Cell::Text(s) => Ok(s.clone()),
            other => Err(FromCellsError::new(format!(
                "expected Text at cell {root}, got {other:?}"
            ))),
        }
    }
}

// ----- Compound impls ---------------------------------------------------

impl<T: TypedFromCells> TypedFromCells for Option<T> {
    fn from_cells(tree: &FieldTree, root: u32) -> FromCellsResult<Self> {
        match __get_cell(tree, root)? {
            Cell::OptionSome(c) => T::from_cells(tree, *c).map(Some),
            Cell::OptionNone => Ok(None),
            other => Err(FromCellsError::new(format!(
                "expected Option at cell {root}, got {other:?}"
            ))),
        }
    }
}

impl<T: TypedFromCells, E: TypedFromCells> TypedFromCells for Result<T, E> {
    fn from_cells(tree: &FieldTree, root: u32) -> FromCellsResult<Self> {
        // v1 limitation: result<_, E> / result<T, _> (unit payloads)
        // are not yet supported. Add when needed.
        match __get_cell(tree, root)? {
            Cell::ResultOk(Some(c)) => T::from_cells(tree, *c).map(Ok),
            Cell::ResultErr(Some(c)) => E::from_cells(tree, *c).map(Err),
            Cell::ResultOk(None) | Cell::ResultErr(None) => Err(FromCellsError::new(
                "v1 TypedFromCells does not yet support unit-payload result arms",
            )),
            other => Err(FromCellsError::new(format!(
                "expected Result at cell {root}, got {other:?}"
            ))),
        }
    }
}

impl<T: TypedFromCells> TypedFromCells for Vec<T> {
    fn from_cells(tree: &FieldTree, root: u32) -> FromCellsResult<Self> {
        match __get_cell(tree, root)? {
            Cell::ListOf(children) => children
                .iter()
                .map(|c| T::from_cells(tree, *c))
                .collect(),
            Cell::Bytes(bs) => bs.iter().map(|&b| T::from_byte(b)).collect(),
            other => Err(FromCellsError::new(format!(
                "expected ListOf or Bytes at cell {root}, got {other:?}"
            ))),
        }
    }
}

// TODO(stage-3): revisit whether `Args` should be a tuple or a
// per-function generated struct. Today we ship tuple blanket impls
// (arity 1-8) so the codegen template can package args as `(a, b)`,
// matching proxy-component's structural shape. Once the stage-3
// template is sketched, weigh "8 hardcoded tuple impls in the SDK"
// against "one struct + impls generated per wrapped function" and
// pick. If we switch to per-function structs, this whole macro
// block and the 8 invocations below disappear.
//
// In the slice pattern below, each `$T` ident binds twice in the
// macro body: once as a type (`$T::from_cells`) and once as a value
// (`*$T`). Type and value namespaces are separate, so the reuse is
// legal; `#[allow(non_snake_case)]` quiets the lint about
// `A`/`B`/... not being snake_case value names.
macro_rules! impl_tuple {
    ($($T:ident),+) => {
        impl<$($T: TypedFromCells),+> TypedFromCells for ($($T,)+) {
            #[allow(non_snake_case)]
            fn from_cells(tree: &FieldTree, root: u32) -> FromCellsResult<Self> {
                match __get_cell(tree, root)? {
                    Cell::TupleOf(children) => match children.as_slice() {
                        [$($T),+] => Ok(($($T::from_cells(tree, *$T)?,)+)),
                        _ => Err(FromCellsError::new(format!(
                            "tuple arity mismatch at cell {root}: expected {}, got {}",
                            [$(stringify!($T)),+].len(),
                            children.len(),
                        ))),
                    },
                    other => Err(FromCellsError::new(format!(
                        "expected TupleOf at cell {root}, got {other:?}"
                    ))),
                }
            }
        }
    };
}

impl_tuple!(A);
impl_tuple!(A, B);
impl_tuple!(A, B, C);
impl_tuple!(A, B, C, D);
impl_tuple!(A, B, C, D, E);
impl_tuple!(A, B, C, D, E, F);
impl_tuple!(A, B, C, D, E, F, G);
impl_tuple!(A, B, C, D, E, F, G, H);

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf_tree(cells: Vec<Cell>, root: u32) -> FieldTree {
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
    fn decodes_bool() {
        let t = leaf_tree(vec![Cell::Bool(true)], 0);
        assert!(bool::from_cells(&t, 0).unwrap());
    }

    #[test]
    fn decodes_integer_with_range_check() {
        let t = leaf_tree(vec![Cell::Integer(300)], 0);
        assert!(u8::from_cells(&t, 0).is_err());
        assert_eq!(u16::from_cells(&t, 0).unwrap(), 300);
    }

    #[test]
    fn decodes_text_and_floats() {
        let t = leaf_tree(vec![Cell::Text("hi".into())], 0);
        assert_eq!(String::from_cells(&t, 0).unwrap(), "hi");
        let t = leaf_tree(vec![Cell::Floating(1.5)], 0);
        assert_eq!(f64::from_cells(&t, 0).unwrap(), 1.5);
        assert_eq!(f32::from_cells(&t, 0).unwrap(), 1.5_f32);
    }

    #[test]
    fn decodes_option_some_and_none() {
        let t = leaf_tree(vec![Cell::Integer(42), Cell::OptionSome(0)], 1);
        let v: Option<u32> = TypedFromCells::from_cells(&t, t.root).unwrap();
        assert_eq!(v, Some(42));

        let t = leaf_tree(vec![Cell::OptionNone], 0);
        let v: Option<u32> = TypedFromCells::from_cells(&t, t.root).unwrap();
        assert_eq!(v, None);
    }

    #[test]
    fn decodes_result_ok_and_err() {
        let t = leaf_tree(
            vec![
                Cell::Integer(7),
                Cell::Text("oops".into()),
                Cell::ResultOk(Some(0)),
                Cell::ResultErr(Some(1)),
            ],
            2,
        );
        let v: Result<u32, String> = TypedFromCells::from_cells(&t, 2).unwrap();
        assert_eq!(v, Ok(7));
        let v: Result<u32, String> = TypedFromCells::from_cells(&t, 3).unwrap();
        assert_eq!(v, Err("oops".into()));
    }

    #[test]
    fn decodes_list_of_strings_via_listof() {
        let t = leaf_tree(
            vec![
                Cell::Text("a".into()),
                Cell::Text("b".into()),
                Cell::ListOf(vec![0, 1]),
            ],
            2,
        );
        let v: Vec<String> = TypedFromCells::from_cells(&t, 2).unwrap();
        assert_eq!(v, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn decodes_list_of_bytes_via_bytes_fastpath() {
        let t = leaf_tree(vec![Cell::Bytes(vec![1, 2, 3])], 0);
        let v: Vec<u8> = TypedFromCells::from_cells(&t, 0).unwrap();
        assert_eq!(v, vec![1, 2, 3]);
    }

    #[test]
    fn vec_of_nonbyte_from_bytes_errors() {
        let t = leaf_tree(vec![Cell::Bytes(vec![1, 2, 3])], 0);
        let v: FromCellsResult<Vec<String>> = TypedFromCells::from_cells(&t, 0);
        assert!(v.is_err());
    }

    #[test]
    fn decodes_tuple() {
        let t = leaf_tree(
            vec![
                Cell::Integer(1),
                Cell::Text("two".into()),
                Cell::TupleOf(vec![0, 1]),
            ],
            2,
        );
        let v: (u32, String) = TypedFromCells::from_cells(&t, 2).unwrap();
        assert_eq!(v, (1, "two".into()));
    }

    #[test]
    fn type_mismatch_errors() {
        let t = leaf_tree(vec![Cell::Bool(true)], 0);
        let v: FromCellsResult<String> = TypedFromCells::from_cells(&t, 0);
        assert!(v.is_err());
    }
}
