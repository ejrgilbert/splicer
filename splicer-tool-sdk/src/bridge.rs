//! `WitTyped`: the trait that lets a Rust value flow to and from a
//! wasm-wave Value (and through that, splicer's cells wire format).
//!
//! `WitTyped` impls split by where the type comes from:
//!
//! - **WIT core types** (this file): primitives (`bool`, ints, floats,
//!   `char`, `String`) and generic containers (`Vec<T>`, `Option<T>`,
//!   `Result<T, E>`). These have a fixed Rust shape, so the impls are
//!   handwritten here and shared across every wrapper crate.
//! - **Generated wrapper types** (in `splicer::adapter::typed::emit_wit_typed`):
//!   record, enum, and variant types that a user declares in their
//!   target WIT. Their Rust shape changes per WIT, so splicer
//!   generates the impls per wrapper crate. The generated impls call
//!   into the core-type impls here for field and element conversion.
//! - **User-authored types** (via `#[derive(WitTyped)]`, behind the
//!   `derive` feature): structs and enums a user declares directly in
//!   a strategy crate, where there is no WIT to drive codegen. The
//!   derive emits the same impl shape as the wrapper codegen, mapping
//!   field/case identifiers to kebab-case WIT names.
//!
//! Also in this file: the lower-level [`cells_to_value`] function
//! that turns cells (the wire format used by tier-2 hooks and
//! recorded traces) into a wasm-wave Value. Strategies that consume
//! traces typically reach for [`cells_to_typed`] instead, which
//! chains cells → Value → typed Rust in one call.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

use wasm_wave::value::{Type as WaveType, Value as WaveValue};
use wasm_wave::wasm::{WasmType, WasmTypeKind, WasmValue, WasmValueError};

use crate::types::{Cell, Field, FieldTree};

/// Carries a Rust type's WIT type info plus both conversion
/// directions to/from [`WaveValue`].
pub trait WitTyped: Sized {
    /// The WIT type this Rust type corresponds to, as a wasm-wave
    /// `Type` instance.
    fn wave_type() -> WaveType;
    /// Convert a typed Rust value into a wasm-wave Value.
    fn to_value(&self) -> WaveValue;
    /// Decode a wasm-wave Value into a typed Rust value.
    fn from_value(value: &WaveValue) -> Result<Self, BridgeError>;
}

// ---- primitive impls --------------------------------------------------

// Every Copy primitive whose `WaveType` constant, `make_*` constructor,
// and `unwrap_*` accessor follow the same naming pattern flows through
// this macro. Only `String` diverges (needs `Cow` plumbing).
macro_rules! impl_wit_typed_primitive {
    ($($ty:ty => $wave_ty:ident, $make:ident, $unwrap:ident);* $(;)?) => {$(
        impl WitTyped for $ty {
            fn wave_type() -> WaveType { WaveType::$wave_ty }
            fn to_value(&self) -> WaveValue { WaveValue::$make(*self) }
            fn from_value(v: &WaveValue) -> Result<Self, BridgeError> { Ok(v.$unwrap()) }
        }
    )*};
}

impl_wit_typed_primitive! {
    bool => BOOL, make_bool, unwrap_bool;
    u8 => U8, make_u8, unwrap_u8;
    u16 => U16, make_u16, unwrap_u16;
    u32 => U32, make_u32, unwrap_u32;
    u64 => U64, make_u64, unwrap_u64;
    i8 => S8, make_s8, unwrap_s8;
    i16 => S16, make_s16, unwrap_s16;
    i32 => S32, make_s32, unwrap_s32;
    i64 => S64, make_s64, unwrap_s64;
    f32 => F32, make_f32, unwrap_f32;
    f64 => F64, make_f64, unwrap_f64;
    char => CHAR, make_char, unwrap_char;
}

impl WitTyped for String {
    fn wave_type() -> WaveType {
        WaveType::STRING
    }
    fn to_value(&self) -> WaveValue {
        WaveValue::make_string(Cow::Borrowed(self.as_str()))
    }
    fn from_value(v: &WaveValue) -> Result<Self, BridgeError> {
        Ok(v.unwrap_string().into_owned())
    }
}

// ---- compound impls ---------------------------------------------------

impl<T: WitTyped> WitTyped for Vec<T> {
    fn wave_type() -> WaveType {
        WaveType::list(T::wave_type())
    }
    fn to_value(&self) -> WaveValue {
        let ty = Self::wave_type();
        WaveValue::make_list(&ty, self.iter().map(T::to_value))
            .expect("element values match the list's declared element type")
    }
    fn from_value(v: &WaveValue) -> Result<Self, BridgeError> {
        v.unwrap_list().map(|c| T::from_value(&c)).collect()
    }
}

impl<T: WitTyped> WitTyped for Option<T> {
    fn wave_type() -> WaveType {
        WaveType::option(T::wave_type())
    }
    fn to_value(&self) -> WaveValue {
        let ty = Self::wave_type();
        let inner = self.as_ref().map(T::to_value);
        WaveValue::make_option(&ty, inner).expect("inner value matches declared option type")
    }
    fn from_value(v: &WaveValue) -> Result<Self, BridgeError> {
        match v.unwrap_option() {
            Some(inner) => Ok(Some(T::from_value(&inner)?)),
            None => Ok(None),
        }
    }
}

impl<T: WitTyped, E: WitTyped> WitTyped for Result<T, E> {
    fn wave_type() -> WaveType {
        WaveType::result(Some(T::wave_type()), Some(E::wave_type()))
    }
    fn to_value(&self) -> WaveValue {
        let ty = Self::wave_type();
        let inner = match self {
            Ok(t) => Ok(Some(t.to_value())),
            Err(e) => Err(Some(e.to_value())),
        };
        WaveValue::make_result(&ty, inner).expect("inner value matches declared result type")
    }
    fn from_value(v: &WaveValue) -> Result<Self, BridgeError> {
        match v.unwrap_result() {
            Ok(Some(inner)) => Ok(Ok(T::from_value(&inner)?)),
            Err(Some(inner)) => Ok(Err(E::from_value(&inner)?)),
            Ok(None) | Err(None) => Err(BridgeError::Unsupported(
                "result arm/type shape mismatch (both arms expected payloads)",
            )),
        }
    }
}

// wit-bindgen-rust emits `()` for `result<...>` arms that don't carry a
// payload — `result<u32>` → `Result<u32, ()>`, `result<_, E>` →
// `Result<(), E>`, `result<_>` → `Result<(), ()>`. `()` is intentionally
// NOT impl'd as `WitTyped` (no Wave type maps to it); each unit-arm
// `Result` shape gets its own impl below, which keeps the bounds on the
// generic `impl<T, E>` above from triggering coherence overlap.

impl<T: WitTyped> WitTyped for Result<T, ()> {
    fn wave_type() -> WaveType {
        WaveType::result(Some(T::wave_type()), None)
    }
    fn to_value(&self) -> WaveValue {
        let ty = Self::wave_type();
        let inner = match self {
            Ok(t) => Ok(Some(t.to_value())),
            Err(()) => Err(None),
        };
        WaveValue::make_result(&ty, inner).expect("inner value matches declared result type")
    }
    fn from_value(v: &WaveValue) -> Result<Self, BridgeError> {
        match v.unwrap_result() {
            Ok(Some(inner)) => Ok(Ok(T::from_value(&inner)?)),
            Err(None) => Ok(Err(())),
            Ok(None) | Err(Some(_)) => Err(BridgeError::Unsupported(
                "result arm payload mismatch for Result<T, ()>",
            )),
        }
    }
}

impl<E: WitTyped> WitTyped for Result<(), E> {
    fn wave_type() -> WaveType {
        WaveType::result(None, Some(E::wave_type()))
    }
    fn to_value(&self) -> WaveValue {
        let ty = Self::wave_type();
        let inner = match self {
            Ok(()) => Ok(None),
            Err(e) => Err(Some(e.to_value())),
        };
        WaveValue::make_result(&ty, inner).expect("inner value matches declared result type")
    }
    fn from_value(v: &WaveValue) -> Result<Self, BridgeError> {
        match v.unwrap_result() {
            Ok(None) => Ok(Ok(())),
            Err(Some(inner)) => Ok(Err(E::from_value(&inner)?)),
            Ok(Some(_)) | Err(None) => Err(BridgeError::Unsupported(
                "result arm payload mismatch for Result<(), E>",
            )),
        }
    }
}

impl WitTyped for Result<(), ()> {
    fn wave_type() -> WaveType {
        WaveType::result(None, None)
    }
    fn to_value(&self) -> WaveValue {
        let ty = Self::wave_type();
        let inner = match self {
            Ok(()) => Ok(None),
            Err(()) => Err(None),
        };
        WaveValue::make_result(&ty, inner).expect("inner value matches declared result type")
    }
    fn from_value(v: &WaveValue) -> Result<Self, BridgeError> {
        match v.unwrap_result() {
            Ok(None) => Ok(Ok(())),
            Err(None) => Ok(Err(())),
            Ok(Some(_)) | Err(Some(_)) => Err(BridgeError::Unsupported(
                "result arm payload mismatch for Result<(), ()>",
            )),
        }
    }
}

// ---- tuple impls ------------------------------------------------------
//
// `tuple<T1, ..., Tn>` lowers to `(T1, ..., Tn)`. Rust has no
// arity-agnostic tuple syntax for trait impls, so the SDK enumerates
// blanket impls 1..=12 — bump the list if a real WIT needs higher
// arity.
//
// `()` is intentionally NOT impl'd (no Wave type maps to it).

macro_rules! impl_wit_typed_tuple {
    ($($T:ident => $idx:tt),+) => {
        impl<$($T: WitTyped),+> WitTyped for ($($T,)+) {
            fn wave_type() -> WaveType {
                WaveType::tuple([$($T::wave_type()),+])
                    .expect("WIT tuple has at least one element type")
            }
            fn to_value(&self) -> WaveValue {
                let ty = Self::wave_type();
                WaveValue::make_tuple(&ty, [$(self.$idx.to_value()),+])
                    .expect("element values match the declared tuple type")
            }
            fn from_value(v: &WaveValue) -> Result<Self, BridgeError> {
                let mut it = v.unwrap_tuple();
                Ok(( $({
                    let next = it.next().ok_or(BridgeError::ExpectedTypeShape(
                        "tuple value has fewer elements than the declared type",
                    ))?;
                    $T::from_value(&next)?
                },)+ ))
            }
        }
    };
}

impl_wit_typed_tuple!(T1 => 0);
impl_wit_typed_tuple!(T1 => 0, T2 => 1);
impl_wit_typed_tuple!(T1 => 0, T2 => 1, T3 => 2);
impl_wit_typed_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3);
impl_wit_typed_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4);
impl_wit_typed_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5);
impl_wit_typed_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5, T7 => 6);
impl_wit_typed_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5, T7 => 6, T8 => 7);
impl_wit_typed_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5, T7 => 6, T8 => 7, T9 => 8);
impl_wit_typed_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5, T7 => 6, T8 => 7, T9 => 8, T10 => 9);
impl_wit_typed_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5, T7 => 6, T8 => 7, T9 => 8, T10 => 9, T11 => 10);
impl_wit_typed_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5, T7 => 6, T8 => 7, T9 => 8, T10 => 9, T11 => 10, T12 => 11);

/// Implement [`WitTyped`] for a zero-field args struct via the
/// single-field sentinel-record encoding that works around
/// `wasm-wave`'s rejection of zero-field records (and zero-element
/// tuples). Codegen emits one invocation per synthesized zero-arg
/// args record.
#[macro_export]
macro_rules! impl_wit_typed_for_zero_arg_args {
    ($args:ty) => {
        impl $crate::WitTyped for $args {
            fn wave_type() -> $crate::wasm_wave::value::Type {
                $crate::wasm_wave::value::Type::record([(
                    "unit",
                    $crate::wasm_wave::value::Type::BOOL,
                )])
                .expect("single-field record is permitted")
            }
            fn to_value(&self) -> $crate::wasm_wave::value::Value {
                $crate::wasm_wave::value::Value::make_record(
                    &<Self as $crate::WitTyped>::wave_type(),
                    [(
                        "unit",
                        $crate::wasm_wave::value::Value::make_bool(true),
                    )],
                )
                .expect("sentinel field matches the declared record type")
            }
            fn from_value(
                _value: &$crate::wasm_wave::value::Value,
            ) -> ::core::result::Result<Self, $crate::BridgeError> {
                ::core::result::Result::Ok(Self {})
            }
        }
    };
}

/// Decode the cell at `root` directly into a typed Rust value.
/// Convenience over [`cells_to_value`] + [`WitTyped::from_value`].
pub fn cells_to_typed<T: WitTyped>(tree: &FieldTree, root: u32) -> Result<T, BridgeError> {
    let value: WaveValue = cells_to_value(tree, root, &T::wave_type())?;
    T::from_value(&value)
}

/// Decode a function-call argument list into a tuple `Args`.
///
/// Each argument is its own self-contained [`FieldTree`]. This walks
/// `Args`'s tuple element types, decodes the i-th argument against the
/// i-th element, and assembles the tuple. `Args` must be a tuple type
/// whose arity matches `args.len()`; a zero-argument call has no tuple
/// form, so read those from the raw event instead.
pub fn args_to_typed<Args: WitTyped>(args: &[Field]) -> Result<Args, BridgeError> {
    let ty = Args::wave_type();
    let elem_types: Vec<WaveType> = ty.tuple_element_types().collect();
    if args.len() != elem_types.len() {
        return Err(BridgeError::ExpectedTypeShape(
            "argument count does not match the tuple arity of Args",
        ));
    }
    let elems = args
        .iter()
        .zip(&elem_types)
        .map(|(f, t)| cells_to_value::<WaveValue>(&f.tree, f.tree.root, t))
        .collect::<Result<Vec<_>, _>>()?;
    Args::from_value(&WaveValue::make_tuple(&ty, elems)?)
}

/// Decode the cell at `root` (an index into `tree.cells`) into a
/// `WasmValue` typed by `expected_type`. Walks cells and the
/// expected type in parallel.
pub fn cells_to_value<V: WasmValue>(
    tree: &FieldTree,
    root: u32,
    expected_type: &V::Type,
) -> Result<V, BridgeError> {
    let cell = get_cell(tree, root)?;
    decode_cell(tree, cell, expected_type)
}

/// Failure modes for the cells→value bridge.
#[derive(Debug)]
pub enum BridgeError {
    /// A `tree.cells` index was out of bounds.
    CellOutOfBounds { idx: u32, len: usize },
    /// A side-table index was out of bounds.
    SideTableOutOfBounds {
        table: &'static str,
        idx: u32,
        len: usize,
    },
    /// The cell shape didn't match the expected type's kind
    /// (e.g. expected_type says `record`, got `Cell::Bool`).
    KindMismatch {
        expected: WasmTypeKind,
        got_cell: &'static str,
    },
    /// An integer cell didn't fit the narrower target type.
    IntOutOfRange { value: i64, target: WasmTypeKind },
    /// A name (variant/enum case, record field, or flag) didn't
    /// appear in the expected type's declared members. Signals
    /// encoder/decoder schema drift — usually a bug, not noise.
    UnknownCase {
        type_kind: WasmTypeKind,
        case: String,
    },
    /// A record/variant declared a field not present in the cell.
    MissingField { name: String },
    /// `Cell::Bytes` (the `list<u8>` fast path) was used with a
    /// non-u8 element type.
    BytesWithNonU8Element { element_kind: WasmTypeKind },
    /// An expected type slot couldn't be queried (e.g.
    /// `list_element_type()` returned `None`).
    ExpectedTypeShape(&'static str),
    /// `Cell::Integer` didn't represent a valid Unicode codepoint.
    InvalidChar(u32),
    /// Cell shape isn't supported (resources, streams, futures,
    /// error-context).
    Unsupported(&'static str),
    /// wasm-wave rejected the constructed value (e.g. flag name
    /// unknown, payload type mismatch).
    WasmValue(WasmValueError),
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CellOutOfBounds { idx, len } => {
                write!(f, "cell index {idx} out of bounds (len = {len})")
            }
            Self::SideTableOutOfBounds { table, idx, len } => {
                write!(f, "{table}[{idx}] out of bounds (len = {len})")
            }
            Self::KindMismatch { expected, got_cell } => {
                write!(f, "expected {expected:?}, got {got_cell}")
            }
            Self::IntOutOfRange { value, target } => {
                write!(f, "Integer {value} out of range for {target:?}")
            }
            Self::UnknownCase { type_kind, case } => {
                let member_word = match type_kind {
                    WasmTypeKind::Record => "field",
                    WasmTypeKind::Flags => "flag",
                    _ => "case",
                };
                write!(f, "unknown {member_word} {case:?} for {type_kind:?}")
            }
            Self::MissingField { name } => write!(f, "missing field {name:?} in cell"),
            Self::BytesWithNonU8Element { element_kind } => write!(
                f,
                "Cell::Bytes is only valid for list<u8>; got list element kind {element_kind:?}"
            ),
            Self::ExpectedTypeShape(s) => write!(f, "expected-type shape issue: {s}"),
            Self::InvalidChar(c) => write!(f, "Integer {c} is not a valid Unicode codepoint"),
            Self::Unsupported(s) => write!(f, "{s} cells are not supported"),
            Self::WasmValue(e) => write!(f, "wasm-wave value construction failed: {e}"),
        }
    }
}

impl std::error::Error for BridgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::WasmValue(e) => Some(e),
            _ => None,
        }
    }
}

impl From<WasmValueError> for BridgeError {
    fn from(e: WasmValueError) -> Self {
        Self::WasmValue(e)
    }
}

fn get_cell(tree: &FieldTree, idx: u32) -> Result<&Cell, BridgeError> {
    tree.cells
        .get(idx as usize)
        .ok_or(BridgeError::CellOutOfBounds {
            idx,
            len: tree.cells.len(),
        })
}

fn kind_mismatch<T>(expected: WasmTypeKind, cell: &Cell) -> Result<T, BridgeError> {
    Err(BridgeError::KindMismatch {
        expected,
        got_cell: cell_variant_name(cell),
    })
}

fn cell_variant_name(cell: &Cell) -> &'static str {
    match cell {
        Cell::Bool(_) => "Bool",
        Cell::Integer(_) => "Integer",
        Cell::Floating(_) => "Floating",
        Cell::Text(_) => "Text",
        Cell::Bytes(_) => "Bytes",
        Cell::ListOf(_) => "ListOf",
        Cell::TupleOf(_) => "TupleOf",
        Cell::OptionSome(_) => "OptionSome",
        Cell::OptionNone => "OptionNone",
        Cell::ResultOk(_) => "ResultOk",
        Cell::ResultErr(_) => "ResultErr",
        Cell::RecordOf(_) => "RecordOf",
        Cell::FlagsSet(_) => "FlagsSet",
        Cell::EnumCase(_) => "EnumCase",
        Cell::VariantCase(_) => "VariantCase",
        Cell::ResourceHandle(_) => "ResourceHandle",
        Cell::StreamHandle(_) => "StreamHandle",
        Cell::FutureHandle(_) => "FutureHandle",
        Cell::ErrorContextHandle(_) => "ErrorContextHandle",
    }
}

fn decode_cell<V: WasmValue>(
    tree: &FieldTree,
    cell: &Cell,
    expected: &V::Type,
) -> Result<V, BridgeError> {
    let kind = expected.kind();
    match kind {
        WasmTypeKind::Bool => match cell {
            Cell::Bool(b) => Ok(V::make_bool(*b)),
            _ => kind_mismatch(kind, cell),
        },
        WasmTypeKind::S8 => decode_integer(cell, kind, |i| {
            i8::try_from(i).map(V::make_s8).map_err(|_| out_of_range(i, kind))
        }),
        WasmTypeKind::S16 => decode_integer(cell, kind, |i| {
            i16::try_from(i).map(V::make_s16).map_err(|_| out_of_range(i, kind))
        }),
        WasmTypeKind::S32 => decode_integer(cell, kind, |i| {
            i32::try_from(i).map(V::make_s32).map_err(|_| out_of_range(i, kind))
        }),
        WasmTypeKind::S64 => decode_integer(cell, kind, |i| Ok(V::make_s64(i))),
        WasmTypeKind::U8 => decode_integer(cell, kind, |i| {
            u8::try_from(i).map(V::make_u8).map_err(|_| out_of_range(i, kind))
        }),
        WasmTypeKind::U16 => decode_integer(cell, kind, |i| {
            u16::try_from(i).map(V::make_u16).map_err(|_| out_of_range(i, kind))
        }),
        WasmTypeKind::U32 => decode_integer(cell, kind, |i| {
            u32::try_from(i).map(V::make_u32).map_err(|_| out_of_range(i, kind))
        }),
        WasmTypeKind::U64 => decode_integer(cell, kind, |i| {
            u64::try_from(i).map(V::make_u64).map_err(|_| out_of_range(i, kind))
        }),
        WasmTypeKind::F32 => match cell {
            Cell::Floating(f) => Ok(V::make_f32(*f as f32)),
            _ => kind_mismatch(kind, cell),
        },
        WasmTypeKind::F64 => match cell {
            Cell::Floating(f) => Ok(V::make_f64(*f)),
            _ => kind_mismatch(kind, cell),
        },
        WasmTypeKind::Char => match cell {
            Cell::Integer(i) => {
                let codepoint = u32::try_from(*i).map_err(|_| BridgeError::InvalidChar(*i as u32))?;
                let c = char::from_u32(codepoint).ok_or(BridgeError::InvalidChar(codepoint))?;
                Ok(V::make_char(c))
            }
            _ => kind_mismatch(kind, cell),
        },
        WasmTypeKind::String => match cell {
            Cell::Text(s) => Ok(V::make_string(Cow::Owned(s.clone()))),
            _ => kind_mismatch(kind, cell),
        },
        WasmTypeKind::List => decode_list::<V>(tree, cell, expected),
        WasmTypeKind::Record => decode_record::<V>(tree, cell, expected),
        WasmTypeKind::Tuple => decode_tuple::<V>(tree, cell, expected),
        WasmTypeKind::Variant => decode_variant::<V>(tree, cell, expected),
        WasmTypeKind::Enum => decode_enum::<V>(tree, cell, expected),
        WasmTypeKind::Option => decode_option::<V>(tree, cell, expected),
        WasmTypeKind::Result => decode_result::<V>(tree, cell, expected),
        WasmTypeKind::Flags => decode_flags::<V>(tree, cell, expected),
        // Resources, streams, futures, and error-context aren't
        // supported.
        _ => Err(BridgeError::Unsupported("non-value-typed shape")),
    }
}

fn out_of_range(value: i64, target: WasmTypeKind) -> BridgeError {
    BridgeError::IntOutOfRange { value, target }
}

fn decode_integer<V: WasmValue>(
    cell: &Cell,
    kind: WasmTypeKind,
    build: impl FnOnce(i64) -> Result<V, BridgeError>,
) -> Result<V, BridgeError> {
    match cell {
        Cell::Integer(i) => build(*i),
        _ => kind_mismatch(kind, cell),
    }
}

fn decode_list<V: WasmValue>(
    tree: &FieldTree,
    cell: &Cell,
    expected: &V::Type,
) -> Result<V, BridgeError> {
    let elem_ty = expected
        .list_element_type()
        .ok_or(BridgeError::ExpectedTypeShape("list_element_type"))?;
    let elems: Vec<V> = match cell {
        Cell::ListOf(children) => children
            .iter()
            .map(|c| cells_to_value::<V>(tree, *c, &elem_ty))
            .collect::<Result<_, _>>()?,
        Cell::Bytes(bs) => {
            if elem_ty.kind() != WasmTypeKind::U8 {
                return Err(BridgeError::BytesWithNonU8Element {
                    element_kind: elem_ty.kind(),
                });
            }
            bs.iter().map(|&b| V::make_u8(b)).collect()
        }
        _ => return kind_mismatch(WasmTypeKind::List, cell),
    };
    Ok(V::make_list(expected, elems)?)
}

fn decode_option<V: WasmValue>(
    tree: &FieldTree,
    cell: &Cell,
    expected: &V::Type,
) -> Result<V, BridgeError> {
    let inner_ty = expected
        .option_some_type()
        .ok_or(BridgeError::ExpectedTypeShape("option_some_type"))?;
    match cell {
        Cell::OptionSome(c) => {
            let inner = cells_to_value::<V>(tree, *c, &inner_ty)?;
            Ok(V::make_option(expected, Some(inner))?)
        }
        Cell::OptionNone => Ok(V::make_option(expected, None)?),
        _ => kind_mismatch(WasmTypeKind::Option, cell),
    }
}

fn decode_result<V: WasmValue>(
    tree: &FieldTree,
    cell: &Cell,
    expected: &V::Type,
) -> Result<V, BridgeError> {
    let (ok_ty, err_ty) = expected
        .result_types()
        .ok_or(BridgeError::ExpectedTypeShape("result_types"))?;
    let make = |is_ok: bool, payload: Option<u32>, payload_ty: Option<V::Type>| -> Result<V, BridgeError> {
        let payload_v = match (payload, payload_ty) {
            (Some(c), Some(t)) => Some(cells_to_value::<V>(tree, c, &t)?),
            (None, None) => None,
            (Some(_), None) => return Err(BridgeError::ExpectedTypeShape("result payload type missing")),
            (None, Some(_)) => return Err(BridgeError::ExpectedTypeShape("result payload cell missing")),
        };
        let r = if is_ok { Ok(payload_v) } else { Err(payload_v) };
        Ok(V::make_result(expected, r)?)
    };
    match cell {
        Cell::ResultOk(c) => make(true, *c, ok_ty),
        Cell::ResultErr(c) => make(false, *c, err_ty),
        _ => kind_mismatch(WasmTypeKind::Result, cell),
    }
}

fn decode_tuple<V: WasmValue>(
    tree: &FieldTree,
    cell: &Cell,
    expected: &V::Type,
) -> Result<V, BridgeError> {
    let elem_types: Vec<V::Type> = expected.tuple_element_types().collect();
    match cell {
        Cell::TupleOf(children) => {
            if children.len() != elem_types.len() {
                return Err(BridgeError::ExpectedTypeShape(
                    "tuple arity mismatch between cell and expected type",
                ));
            }
            let elems: Vec<V> = children
                .iter()
                .zip(elem_types.iter())
                .map(|(c, t)| cells_to_value::<V>(tree, *c, t))
                .collect::<Result<_, _>>()?;
            Ok(V::make_tuple(expected, elems)?)
        }
        _ => kind_mismatch(WasmTypeKind::Tuple, cell),
    }
}

fn decode_record<V: WasmValue>(
    tree: &FieldTree,
    cell: &Cell,
    expected: &V::Type,
) -> Result<V, BridgeError> {
    let side_idx = match cell {
        Cell::RecordOf(i) => *i,
        _ => return kind_mismatch(WasmTypeKind::Record, cell),
    };
    let info = tree
        .record_infos
        .get(side_idx as usize)
        .ok_or(BridgeError::SideTableOutOfBounds {
            table: "record_infos",
            idx: side_idx,
            len: tree.record_infos.len(),
        })?;
    // Side-table fields by name → child cell.
    let by_name: HashMap<&str, u32> =
        info.fields.iter().map(|(n, c)| (n.as_str(), *c)).collect();
    // Walk expected type's fields in order; for each, look up cell + recurse.
    let expected_fields: Vec<(Cow<'_, str>, V::Type)> = expected.record_fields().collect();
    let mut owned_names: Vec<String> = Vec::with_capacity(expected_fields.len());
    let mut values: Vec<V> = Vec::with_capacity(expected_fields.len());
    for (name, field_ty) in &expected_fields {
        let cell_idx = by_name
            .get(name.as_ref())
            .ok_or_else(|| BridgeError::MissingField {
                name: name.to_string(),
            })?;
        let v = cells_to_value::<V>(tree, *cell_idx, field_ty)?;
        owned_names.push(name.to_string());
        values.push(v);
    }
    let fields_iter = owned_names
        .iter()
        .map(|s| s.as_str())
        .zip(values.into_iter());
    Ok(V::make_record(expected, fields_iter)?)
}

fn decode_variant<V: WasmValue>(
    tree: &FieldTree,
    cell: &Cell,
    expected: &V::Type,
) -> Result<V, BridgeError> {
    let side_idx = match cell {
        Cell::VariantCase(i) => *i,
        _ => return kind_mismatch(WasmTypeKind::Variant, cell),
    };
    let info = tree
        .variant_infos
        .get(side_idx as usize)
        .ok_or(BridgeError::SideTableOutOfBounds {
            table: "variant_infos",
            idx: side_idx,
            len: tree.variant_infos.len(),
        })?;
    let case_name = info.case_name.as_str();
    // Find the case in expected_type to get the payload type (if any).
    let case_payload_ty = expected
        .variant_cases()
        .find(|(n, _)| n.as_ref() == case_name)
        .ok_or_else(|| BridgeError::UnknownCase {
            type_kind: WasmTypeKind::Variant,
            case: case_name.to_string(),
        })?
        .1;
    let payload = match (info.payload, case_payload_ty) {
        (Some(c), Some(t)) => Some(cells_to_value::<V>(tree, c, &t)?),
        (None, None) => None,
        (Some(_), None) => {
            return Err(BridgeError::ExpectedTypeShape(
                "variant case has payload in cells but not in expected type",
            ));
        }
        (None, Some(_)) => {
            return Err(BridgeError::ExpectedTypeShape(
                "variant case has payload in expected type but not in cells",
            ));
        }
    };
    Ok(V::make_variant(expected, case_name, payload)?)
}

fn decode_enum<V: WasmValue>(
    tree: &FieldTree,
    cell: &Cell,
    expected: &V::Type,
) -> Result<V, BridgeError> {
    let side_idx = match cell {
        Cell::EnumCase(i) => *i,
        _ => return kind_mismatch(WasmTypeKind::Enum, cell),
    };
    let info = tree
        .enum_infos
        .get(side_idx as usize)
        .ok_or(BridgeError::SideTableOutOfBounds {
            table: "enum_infos",
            idx: side_idx,
            len: tree.enum_infos.len(),
        })?;
    Ok(V::make_enum(expected, &info.case_name)?)
}

fn decode_flags<V: WasmValue>(
    tree: &FieldTree,
    cell: &Cell,
    expected: &V::Type,
) -> Result<V, BridgeError> {
    let side_idx = match cell {
        Cell::FlagsSet(i) => *i,
        _ => return kind_mismatch(WasmTypeKind::Flags, cell),
    };
    let info = tree
        .flags_infos
        .get(side_idx as usize)
        .ok_or(BridgeError::SideTableOutOfBounds {
            table: "flags_infos",
            idx: side_idx,
            len: tree.flags_infos.len(),
        })?;
    let names = info.set_flags.iter().map(|s| s.as_str());
    Ok(V::make_flags(expected, names)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EnumInfo, FlagsInfo, RecordInfo, VariantInfo};
    use wasm_wave::value::{Type, Value};

    fn empty_tree(cells: Vec<Cell>, root: u32) -> FieldTree {
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
    fn decodes_primitives() {
        // bool
        let t = empty_tree(vec![Cell::Bool(true)], 0);
        let v: Value = cells_to_value(&t, 0, &Type::BOOL).unwrap();
        assert!(v.unwrap_bool());

        // u8 with range check
        let t = empty_tree(vec![Cell::Integer(42)], 0);
        let v: Value = cells_to_value(&t, 0, &Type::U8).unwrap();
        assert_eq!(v.unwrap_u8(), 42);
        let t = empty_tree(vec![Cell::Integer(300)], 0);
        assert!(matches!(
            cells_to_value::<Value>(&t, 0, &Type::U8),
            Err(BridgeError::IntOutOfRange { .. })
        ));

        // s32
        let t = empty_tree(vec![Cell::Integer(-7)], 0);
        let v: Value = cells_to_value(&t, 0, &Type::S32).unwrap();
        assert_eq!(v.unwrap_s32(), -7);

        // f64
        let t = empty_tree(vec![Cell::Floating(1.5)], 0);
        let v: Value = cells_to_value(&t, 0, &Type::F64).unwrap();
        assert_eq!(v.unwrap_f64(), 1.5);

        // String
        let t = empty_tree(vec![Cell::Text("hello".into())], 0);
        let v: Value = cells_to_value(&t, 0, &Type::STRING).unwrap();
        assert_eq!(v.unwrap_string(), "hello");

        // char
        let t = empty_tree(vec![Cell::Integer('A' as i64)], 0);
        let v: Value = cells_to_value(&t, 0, &Type::CHAR).unwrap();
        assert_eq!(v.unwrap_char(), 'A');
    }

    #[test]
    fn decodes_option() {
        let opt_ty = Type::option(Type::U32);
        let t = empty_tree(vec![Cell::Integer(7), Cell::OptionSome(0)], 1);
        let v: Value = cells_to_value(&t, 1, &opt_ty).unwrap();
        let inner = v.unwrap_option().unwrap();
        assert_eq!(inner.unwrap_u32(), 7);

        let t = empty_tree(vec![Cell::OptionNone], 0);
        let v: Value = cells_to_value(&t, 0, &opt_ty).unwrap();
        assert!(v.unwrap_option().is_none());
    }

    #[test]
    fn decodes_result_with_payloads() {
        let res_ty = Type::result(Some(Type::U32), Some(Type::STRING));
        let t = empty_tree(
            vec![
                Cell::Integer(99),
                Cell::Text("oops".into()),
                Cell::ResultOk(Some(0)),
                Cell::ResultErr(Some(1)),
            ],
            2,
        );
        let v: Value = cells_to_value(&t, 2, &res_ty).unwrap();
        let inner = v.unwrap_result().unwrap().unwrap();
        assert_eq!(inner.unwrap_u32(), 99);

        let v: Value = cells_to_value(&t, 3, &res_ty).unwrap();
        let inner = v.unwrap_result().unwrap_err().unwrap();
        assert_eq!(inner.unwrap_string(), "oops");
    }

    #[test]
    fn decodes_result_with_unit_arms() {
        let res_ty = Type::result(None, Some(Type::STRING));
        let t = empty_tree(vec![Cell::ResultOk(None)], 0);
        let v: Value = cells_to_value(&t, 0, &res_ty).unwrap();
        let inner = v.unwrap_result().unwrap();
        assert!(inner.is_none());
    }

    #[test]
    fn decodes_list_of_strings() {
        let list_ty = Type::list(Type::STRING);
        let t = empty_tree(
            vec![
                Cell::Text("a".into()),
                Cell::Text("b".into()),
                Cell::ListOf(vec![0, 1]),
            ],
            2,
        );
        let v: Value = cells_to_value(&t, 2, &list_ty).unwrap();
        let elems: Vec<_> = v.unwrap_list().map(|c| c.unwrap_string().into_owned()).collect();
        assert_eq!(elems, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn decodes_bytes_fastpath_for_list_u8() {
        let list_ty = Type::list(Type::U8);
        let t = empty_tree(vec![Cell::Bytes(vec![10, 20, 30])], 0);
        let v: Value = cells_to_value(&t, 0, &list_ty).unwrap();
        let elems: Vec<_> = v.unwrap_list().map(|c| c.unwrap_u8()).collect();
        assert_eq!(elems, vec![10, 20, 30]);
    }

    #[test]
    fn bytes_with_wrong_element_type_errors() {
        let list_ty = Type::list(Type::STRING);
        let t = empty_tree(vec![Cell::Bytes(vec![1, 2])], 0);
        assert!(matches!(
            cells_to_value::<Value>(&t, 0, &list_ty),
            Err(BridgeError::BytesWithNonU8Element { .. })
        ));
    }

    #[test]
    fn decodes_tuple() {
        let tup_ty = Type::tuple([Type::U32, Type::STRING]).unwrap();
        let t = empty_tree(
            vec![
                Cell::Integer(42),
                Cell::Text("answer".into()),
                Cell::TupleOf(vec![0, 1]),
            ],
            2,
        );
        let v: Value = cells_to_value(&t, 2, &tup_ty).unwrap();
        let elems: Vec<_> = v.unwrap_tuple().collect();
        assert_eq!(elems[0].unwrap_u32(), 42);
        assert_eq!(elems[1].unwrap_string(), "answer");
    }

    #[test]
    fn decodes_record() {
        let rec_ty = Type::record([("x", Type::U32), ("y", Type::U32)]).unwrap();
        let mut t = empty_tree(
            vec![Cell::Integer(3), Cell::Integer(4), Cell::RecordOf(0)],
            2,
        );
        t.record_infos.push(RecordInfo {
            type_name: "point".into(),
            fields: vec![("x".into(), 0), ("y".into(), 1)],
        });
        let v: Value = cells_to_value(&t, 2, &rec_ty).unwrap();
        let fields: Vec<_> = v
            .unwrap_record()
            .map(|(n, val)| (n.into_owned(), val.unwrap_u32()))
            .collect();
        assert!(fields.contains(&("x".into(), 3)));
        assert!(fields.contains(&("y".into(), 4)));
    }

    #[test]
    fn record_kebab_case_field_names() {
        let rec_ty = Type::record([("pet-name", Type::STRING), ("age-years", Type::U32)]).unwrap();
        let mut t = empty_tree(
            vec![
                Cell::Text("Whiskers".into()),
                Cell::Integer(7),
                Cell::RecordOf(0),
            ],
            2,
        );
        t.record_infos.push(RecordInfo {
            type_name: "pet".into(),
            fields: vec![("pet-name".into(), 0), ("age-years".into(), 1)],
        });
        let v: Value = cells_to_value(&t, 2, &rec_ty).unwrap();
        let fields: Vec<(String, Value)> = v
            .unwrap_record()
            .map(|(n, val)| (n.into_owned(), val.into_owned()))
            .collect();
        assert_eq!(fields.len(), 2);
        let name = fields.iter().find(|(n, _)| n == "pet-name").unwrap();
        assert_eq!(name.1.unwrap_string(), "Whiskers");
    }

    #[test]
    fn decodes_enum() {
        let enum_ty = Type::enum_ty(["red", "green", "blue"]).unwrap();
        let mut t = empty_tree(vec![Cell::EnumCase(0)], 0);
        t.enum_infos.push(EnumInfo {
            type_name: "color".into(),
            case_name: "green".into(),
        });
        let v: Value = cells_to_value(&t, 0, &enum_ty).unwrap();
        assert_eq!(v.unwrap_enum(), "green");
    }

    #[test]
    fn decodes_variant_unit_case() {
        let var_ty = Type::variant([
            ("click", Some(Type::U32)),
            ("hover", None),
        ])
        .unwrap();
        let mut t = empty_tree(vec![Cell::VariantCase(0)], 0);
        t.variant_infos.push(VariantInfo {
            type_name: "event".into(),
            case_name: "hover".into(),
            payload: None,
        });
        let v: Value = cells_to_value(&t, 0, &var_ty).unwrap();
        let (case, payload) = v.unwrap_variant();
        assert_eq!(case, "hover");
        assert!(payload.is_none());
    }

    #[test]
    fn decodes_variant_payload_case() {
        let var_ty = Type::variant([
            ("click", Some(Type::U32)),
            ("hover", None),
        ])
        .unwrap();
        let mut t = empty_tree(vec![Cell::Integer(99), Cell::VariantCase(0)], 1);
        t.variant_infos.push(VariantInfo {
            type_name: "event".into(),
            case_name: "click".into(),
            payload: Some(0),
        });
        let v: Value = cells_to_value(&t, 1, &var_ty).unwrap();
        let (case, payload) = v.unwrap_variant();
        assert_eq!(case, "click");
        assert_eq!(payload.unwrap().unwrap_u32(), 99);
    }

    #[test]
    fn variant_unknown_case_errors() {
        let var_ty = Type::variant([("foo", None), ("bar", None)]).unwrap();
        let mut t = empty_tree(vec![Cell::VariantCase(0)], 0);
        t.variant_infos.push(VariantInfo {
            type_name: "v".into(),
            case_name: "baz".into(),
            payload: None,
        });
        assert!(matches!(
            cells_to_value::<Value>(&t, 0, &var_ty),
            Err(BridgeError::UnknownCase { .. })
        ));
    }

    #[test]
    fn decodes_flags() {
        let flag_ty = Type::flags(["read", "write", "exec"]).unwrap();
        let mut t = empty_tree(vec![Cell::FlagsSet(0)], 0);
        t.flags_infos.push(FlagsInfo {
            type_name: "perms".into(),
            set_flags: vec!["read".into(), "exec".into()],
        });
        let v: Value = cells_to_value(&t, 0, &flag_ty).unwrap();
        let names: Vec<_> = v.unwrap_flags().map(|n| n.into_owned()).collect();
        assert!(names.contains(&"read".to_string()));
        assert!(names.contains(&"exec".to_string()));
    }

    #[test]
    fn kind_mismatch_errors() {
        let t = empty_tree(vec![Cell::Bool(true)], 0);
        assert!(matches!(
            cells_to_value::<Value>(&t, 0, &Type::STRING),
            Err(BridgeError::KindMismatch { .. })
        ));
    }

    /// Hand-written WitTyped impl for a primitive-only struct.
    /// Verifies the trait + cells_to_typed pipeline end-to-end.
    /// Stage-3 codegen will emit this exact shape automatically for
    /// every wit-bindgen-generated type.
    #[derive(Debug, PartialEq)]
    struct Point {
        x: u32,
        y: u32,
    }

    impl WitTyped for Point {
        fn wave_type() -> WaveType {
            WaveType::record([("x", WaveType::U32), ("y", WaveType::U32)]).unwrap()
        }
        fn to_value(&self) -> WaveValue {
            WaveValue::make_record(
                &Self::wave_type(),
                [("x", WaveValue::make_u32(self.x)), ("y", WaveValue::make_u32(self.y))],
            )
            .expect("record matches wave_type")
        }
        fn from_value(value: &WaveValue) -> Result<Self, BridgeError> {
            let mut x: Option<u32> = None;
            let mut y: Option<u32> = None;
            for (name, v) in value.unwrap_record() {
                match name.as_ref() {
                    "x" => x = Some(v.unwrap_u32()),
                    "y" => y = Some(v.unwrap_u32()),
                    _ => {}
                }
            }
            Ok(Self {
                x: x.ok_or_else(|| BridgeError::MissingField { name: "x".into() })?,
                y: y.ok_or_else(|| BridgeError::MissingField { name: "y".into() })?,
            })
        }
    }

    #[test]
    fn cells_to_typed_round_trips_record() {
        let mut t = empty_tree(
            vec![Cell::Integer(3), Cell::Integer(4), Cell::RecordOf(0)],
            2,
        );
        t.record_infos.push(RecordInfo {
            type_name: "point".into(),
            fields: vec![("x".into(), 0), ("y".into(), 1)],
        });
        let p: Point = cells_to_typed(&t, t.root).unwrap();
        assert_eq!(p, Point { x: 3, y: 4 });
    }

    #[test]
    fn wit_typed_to_value_round_trip_via_wave() {
        // typed → Value → typed (without involving cells; just
        // confirms the WitTyped pair is consistent on its own).
        let p = Point { x: 7, y: 11 };
        let v = p.to_value();
        let back = Point::from_value(&v).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn wit_typed_tuple_roundtrips_via_wave() {
        // 1-tuple (matches WIT `tuple<u32>`).
        let one: (u32,) = (42,);
        let v = one.to_value();
        let back: (u32,) = WitTyped::from_value(&v).unwrap();
        assert_eq!(back, one);

        // 2-tuple, mixed primitive + String (matches `tuple<u32, string>`).
        let two: (u32, String) = (7, "hi".to_string());
        let v = two.to_value();
        let back: (u32, String) = WitTyped::from_value(&v).unwrap();
        assert_eq!(back, two);

        // 3-tuple, nesting an Option (matches `tuple<u32, string, option<u32>>`).
        let three: (u32, String, Option<u32>) = (3, "x".into(), Some(9));
        let v = three.to_value();
        let back: (u32, String, Option<u32>) = WitTyped::from_value(&v).unwrap();
        assert_eq!(back, three);
    }

    #[test]
    fn sentinel_record_for_zero_arg_args_roundtrips() {
        // wasm-wave rejects both empty records and empty tuples, so
        // the unit shape gets encoded as a 1-field sentinel record.
        // Exercise that shape at runtime to catch a regression in
        // wasm-wave's Type/Value APIs.
        let ty = WaveType::record([("unit", WaveType::BOOL)])
            .expect("single-field record is permitted");
        let v = WaveValue::make_record(&ty, [("unit", WaveValue::make_bool(true))])
            .expect("sentinel field matches the declared record type");
        let mut fields = v.unwrap_record();
        let (name, _val) = fields.next().expect("one field present");
        assert_eq!(name.as_ref(), "unit");
    }

    /// Stand-in for a codegen-emitted zero-arg args struct. Exercises
    /// the macro end-to-end so the SDK pins the impl shape
    /// independent of the splicer-side codegen.
    #[derive(Debug, PartialEq)]
    struct NoopArgs {}
    crate::impl_wit_typed_for_zero_arg_args!(NoopArgs);

    #[test]
    fn zero_arg_args_macro_round_trips_via_wave() {
        let a = NoopArgs {};
        let v = a.to_value();
        let back = NoopArgs::from_value(&v).unwrap();
        assert_eq!(back, NoopArgs {});
    }

    #[test]
    fn zero_arg_args_macro_wave_type_matches_sentinel_shape() {
        let ty = <NoopArgs as WitTyped>::wave_type();
        let fields: Vec<_> = ty.record_fields().collect();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0.as_ref(), "unit");
    }

    #[test]
    fn wit_typed_tuple_roundtrips_at_high_arity() {
        // Lower arities are exercised above; this hits the upper end
        // of the macro expansion.
        type Eight = (u8, u16, u32, u64, i32, bool, char, String);
        let eight: Eight = (1, 2, 3, 4, -5, true, 'z', "tail".to_string());
        let v = eight.to_value();
        let back: Eight = WitTyped::from_value(&v).unwrap();
        assert_eq!(back, eight);
    }

    #[test]
    fn wit_typed_tuple_wave_type_matches_explicit_construction() {
        // The blanket impl's WaveType must equal what the SDK would
        // build by hand — the codegen relies on this equivalence.
        let blanket = <(u32, String) as WitTyped>::wave_type();
        let manual = WaveType::tuple([WaveType::U32, WaveType::STRING]).unwrap();
        // Wave Types compare by structural equality.
        assert_eq!(blanket, manual);
    }

    #[test]
    fn resource_cell_errors_as_unsupported() {
        let mut t = empty_tree(vec![Cell::ResourceHandle(0)], 0);
        t.handle_infos.push(crate::HandleInfo {
            type_name: "thing".into(),
            id: 42,
        });
        // Resource cells aren't a supported shape; with a
        // non-resource expected type the error surfaces as kind
        // mismatch rather than Unsupported, but either way the cell
        // is rejected.
        let res = cells_to_value::<Value>(&t, 0, &Type::BOOL);
        assert!(res.is_err());
    }
}
