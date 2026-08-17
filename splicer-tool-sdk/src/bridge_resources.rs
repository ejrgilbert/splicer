//! Resource-aware companion to [`crate::bridge`].
//!
//! [`WitTyped`](crate::WitTyped) routes through `wasm-wave`, which
//! has no representation for canonical-ABI resource handles.
//! [`WitTypedWithResources`] walks cells directly so each resource
//! leaf can be slotted in as a [`MockedResource`].
//!
//! The SDK provides impls for primitives, generic containers, and
//! tuples. Per-WIT-resource wrapper newtypes and value-typed user
//! types get their impls emitted via the macros below, so the same
//! Rust type can satisfy both bridges.

use crate::bridge::{cells_to_typed, BridgeError};
use crate::types::{Cell, FieldTree};

/// Type-erased backing for a synthesized resource handle.
///
/// Per-WIT-resource wrapper newtypes hold one of these; their
/// [`WitTypedWithResources`] impl reads a [`Cell::ResourceHandle`]
/// and pulls the matching `handle_infos` entry.
///
/// `handle` is `u64` to match [`crate::HandleInfo::id`] and the
/// wire encoder's 8-byte handle payload; narrower types alias
/// distinct stream handles. It is the id from the cell stream,
/// **not** the runtime export-table index wit-bindgen allocates
/// when the wrapper hands the value back downstream.
///
/// `name` is `Cow<'static, str>` so callers can pass either a
/// borrowed `&'static str` (compile-time-known) or an owned name
/// from `HandleInfo::type_name`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MockedResource {
    pub handle: u64,
    pub name: std::borrow::Cow<'static, str>,
}

impl MockedResource {
    /// Decode a single `Cell::ResourceHandle` at `root` into a
    /// `MockedResource`. Shared by SDK walkers and codegen-emitted
    /// wrapper impls so error variants stay consistent.
    pub fn from_handle_cell(
        tree: &FieldTree,
        root: u32,
        name: std::borrow::Cow<'static, str>,
    ) -> Result<Self, BridgeError> {
        let side_idx = match get_cell(tree, root)? {
            Cell::ResourceHandle(i) => *i,
            _ => {
                return Err(BridgeError::Unsupported(
                    "expected Cell::ResourceHandle for resource leaf",
                ))
            }
        };
        let info = tree.handle_infos.get(side_idx as usize).ok_or(
            BridgeError::SideTableOutOfBounds {
                table: "handle_infos",
                idx: side_idx,
                len: tree.handle_infos.len(),
            },
        )?;
        Ok(MockedResource {
            handle: info.id,
            name,
        })
    }
}

/// Decode a [`FieldTree`] into a typed Rust value that may contain
/// resource leaves.
///
/// Mirrors [`WitTyped`](crate::WitTyped) but walks cells directly;
/// `wasm-wave`'s `Value` has no resource representation, so this
/// bridge picks up where the wave bridge stops.
pub trait WitTypedWithResources: Sized {
    fn from_cells(tree: &FieldTree, root: u32) -> Result<Self, BridgeError>;

    /// Rebuild `Vec<Self>` from a [`Cell::Bytes`] (the `list<u8>` fast path).
    /// `None` for every type but `u8`, so `Cell::Bytes` under any other
    /// `Vec<T>` stays rejected.
    fn vec_from_bytes(_bytes: &[u8]) -> Option<Vec<Self>> {
        None
    }
}

/// Convenience over `T::from_cells`.
pub fn cells_to_typed_with_resources<T: WitTypedWithResources>(
    tree: &FieldTree,
    root: u32,
) -> Result<T, BridgeError> {
    T::from_cells(tree, root)
}

/// Construct a `Wrapper(MockedResource)` newtype with a fresh
/// handle id. The per-call-site `AtomicU64` keeps successive
/// constructor calls distinguishable.
#[macro_export]
macro_rules! mint_mock_resource {
    ($wrapper:ident, $wit_name:literal) => {{
        static __COUNTER: ::std::sync::atomic::AtomicU64 =
            ::std::sync::atomic::AtomicU64::new(1);
        $wrapper($crate::MockedResource {
            handle: __COUNTER.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed),
            name: ::std::borrow::Cow::Borrowed($wit_name),
        })
    }};
}

/// Implement [`WitTypedWithResources`] for a value-typed type
/// (already impls [`WitTyped`](crate::WitTyped)) by delegating to
/// the wave bridge.
#[macro_export]
macro_rules! impl_wit_typed_with_resources_via_wave {
    ($ty:ty) => {
        impl $crate::WitTypedWithResources for $ty {
            fn from_cells(
                tree: &$crate::FieldTree,
                root: u32,
            ) -> ::core::result::Result<Self, $crate::BridgeError> {
                $crate::cells_to_typed::<$ty>(tree, root)
            }
        }
    };
}

/// Implement [`WitTypedWithResources`] for a `Wrapper(MockedResource)`
/// newtype by delegating to [`MockedResource::from_handle_cell`].
#[macro_export]
macro_rules! impl_wit_typed_with_resources_for_wrapper {
    ($wrapper:ident, $wit_name:literal) => {
        impl $crate::WitTypedWithResources for $wrapper {
            fn from_cells(
                tree: &$crate::FieldTree,
                root: u32,
            ) -> ::core::result::Result<Self, $crate::BridgeError> {
                $crate::MockedResource::from_handle_cell(
                    tree,
                    root,
                    ::std::borrow::Cow::Borrowed($wit_name),
                )
                .map($wrapper)
            }
        }
    };
}

/// Construct the `Err` arm of `Result<T, E: Default>` via
/// `E::default()`. The Ok arm is never instantiated, so it may
/// hold resource-bearing or otherwise uninstantiable types.
pub trait HasDefaultErr {
    fn default_err() -> Self;
}

impl<T, E: Default> HasDefaultErr for Result<T, E> {
    fn default_err() -> Self {
        Err(E::default())
    }
}

/// Construct the `Err` arm of `Result<T, E: Arbitrary>` by
/// sampling from an [`arbitrary::Unstructured`] stream. The Ok
/// arm needs no bound, so resource-bearing returns work without
/// an `ArbitraryWithResources` trait.
pub trait HasArbitraryErr: Sized {
    fn arbitrary_err<'a>(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self>;
}

impl<T, E: for<'a> arbitrary::Arbitrary<'a>> HasArbitraryErr for Result<T, E> {
    fn arbitrary_err<'a>(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        E::arbitrary(u).map(Err)
    }
}

// ---- primitive impls --------------------------------------------------
//
// Primitives have no resource positions, so each impl delegates to
// `cells_to_typed`. The dual impl lets a compound like
// `Vec<T: WitTypedWithResources>` accept value-typed and
// resource-bearing element types alike.
macro_rules! impl_via_wave {
    ($($T:ty),* $(,)?) => {$(
        impl WitTypedWithResources for $T {
            fn from_cells(tree: &FieldTree, root: u32) -> Result<Self, BridgeError> {
                cells_to_typed::<$T>(tree, root)
            }
        }
    )*};
}

impl_via_wave!(bool, u16, u32, u64, i8, i16, i32, i64, f32, f64, char, String);

// `u8` also overrides `vec_from_bytes` so `Vec<u8>` decodes the `Cell::Bytes`
// fast path; otherwise identical to the `impl_via_wave!` shape.
impl WitTypedWithResources for u8 {
    fn from_cells(tree: &FieldTree, root: u32) -> Result<Self, BridgeError> {
        cells_to_typed::<u8>(tree, root)
    }
    fn vec_from_bytes(bytes: &[u8]) -> Option<Vec<Self>> {
        Some(bytes.to_vec())
    }
}

// ---- compound impls ---------------------------------------------------
//
// Compounds walk cells directly and recurse via T::from_cells, so
// resource leaves reach their wrapper impl without flowing through
// `wasm-wave`. Value-typed leaves bottom out at the wave-routed
// primitive impls above.

fn get_cell<'a>(tree: &'a FieldTree, idx: u32) -> Result<&'a Cell, BridgeError> {
    tree.cells
        .get(idx as usize)
        .ok_or(BridgeError::CellOutOfBounds {
            idx,
            len: tree.cells.len(),
        })
}

impl<T: WitTypedWithResources> WitTypedWithResources for Vec<T> {
    fn from_cells(tree: &FieldTree, root: u32) -> Result<Self, BridgeError> {
        match get_cell(tree, root)? {
            Cell::ListOf(children) => children
                .iter()
                .map(|c| T::from_cells(tree, *c))
                .collect(),
            // `list<u8>` records as a single `Cell::Bytes`; only `Vec<u8>`
            // can rebuild from it (every other element type returns `None`).
            Cell::Bytes(b) => T::vec_from_bytes(b).ok_or(BridgeError::Unsupported(
                "Cell::Bytes fastpath is only valid for Vec<u8>",
            )),
            _ => Err(BridgeError::Unsupported(
                "expected Cell::ListOf for Vec<T>",
            )),
        }
    }
}

impl<T: WitTypedWithResources> WitTypedWithResources for Option<T> {
    fn from_cells(tree: &FieldTree, root: u32) -> Result<Self, BridgeError> {
        match get_cell(tree, root)? {
            Cell::OptionSome(c) => Ok(Some(T::from_cells(tree, *c)?)),
            Cell::OptionNone => Ok(None),
            _ => Err(BridgeError::Unsupported(
                "expected Cell::OptionSome or Cell::OptionNone for Option<T>",
            )),
        }
    }
}

// Same four `Result<...>` arm shapes as [`crate::bridge`]: a generic
// `Result<T, E>` plus three unit-arm specializations covering the
// payload-less WIT `result` arms that wit-bindgen emits as `()`.

impl<T: WitTypedWithResources, E: WitTypedWithResources> WitTypedWithResources for Result<T, E> {
    fn from_cells(tree: &FieldTree, root: u32) -> Result<Self, BridgeError> {
        match get_cell(tree, root)? {
            Cell::ResultOk(Some(c)) => Ok(Ok(T::from_cells(tree, *c)?)),
            Cell::ResultErr(Some(c)) => Ok(Err(E::from_cells(tree, *c)?)),
            Cell::ResultOk(None) | Cell::ResultErr(None) => Err(BridgeError::Unsupported(
                "result arm payload missing for Result<T, E>",
            )),
            _ => Err(BridgeError::Unsupported(
                "expected Cell::ResultOk or Cell::ResultErr for Result<T, E>",
            )),
        }
    }
}

impl<T: WitTypedWithResources> WitTypedWithResources for Result<T, ()> {
    fn from_cells(tree: &FieldTree, root: u32) -> Result<Self, BridgeError> {
        match get_cell(tree, root)? {
            Cell::ResultOk(Some(c)) => Ok(Ok(T::from_cells(tree, *c)?)),
            Cell::ResultErr(None) => Ok(Err(())),
            _ => Err(BridgeError::Unsupported(
                "expected Cell::ResultOk(Some) or Cell::ResultErr(None) for Result<T, ()>",
            )),
        }
    }
}

impl<E: WitTypedWithResources> WitTypedWithResources for Result<(), E> {
    fn from_cells(tree: &FieldTree, root: u32) -> Result<Self, BridgeError> {
        match get_cell(tree, root)? {
            Cell::ResultOk(None) => Ok(Ok(())),
            Cell::ResultErr(Some(c)) => Ok(Err(E::from_cells(tree, *c)?)),
            _ => Err(BridgeError::Unsupported(
                "expected Cell::ResultOk(None) or Cell::ResultErr(Some) for Result<(), E>",
            )),
        }
    }
}

impl WitTypedWithResources for Result<(), ()> {
    fn from_cells(tree: &FieldTree, root: u32) -> Result<Self, BridgeError> {
        match get_cell(tree, root)? {
            Cell::ResultOk(None) => Ok(Ok(())),
            Cell::ResultErr(None) => Ok(Err(())),
            _ => Err(BridgeError::Unsupported(
                "expected Cell::ResultOk(None) or Cell::ResultErr(None) for Result<(), ()>",
            )),
        }
    }
}

// ---- tuple impls ------------------------------------------------------
//
// Same arity range (1..=12) as the wave-routed tuple impls in
// [`crate::bridge`]. `()` is intentionally not impl'd; no WIT type
// maps to it (the unit shape is a sentinel record at codegen).

macro_rules! impl_tuple {
    ($($T:ident => $idx:tt),+) => {
        impl<$($T: WitTypedWithResources),+> WitTypedWithResources for ($($T,)+) {
            fn from_cells(tree: &FieldTree, root: u32) -> Result<Self, BridgeError> {
                let children = match get_cell(tree, root)? {
                    Cell::TupleOf(c) => c,
                    _ => return Err(BridgeError::Unsupported(
                        "expected Cell::TupleOf for tuple type",
                    )),
                };
                let expected_arity = 0usize $(+ { let _ = $idx; 1 })+;
                if children.len() != expected_arity {
                    return Err(BridgeError::ExpectedTypeShape(
                        "tuple arity mismatch between cell and Rust tuple type",
                    ));
                }
                Ok(( $($T::from_cells(tree, children[$idx])?,)+ ))
            }
        }
    };
}

impl_tuple!(T1 => 0);
impl_tuple!(T1 => 0, T2 => 1);
impl_tuple!(T1 => 0, T2 => 1, T3 => 2);
impl_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3);
impl_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4);
impl_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5);
impl_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5, T7 => 6);
impl_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5, T7 => 6, T8 => 7);
impl_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5, T7 => 6, T8 => 7, T9 => 8);
impl_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5, T7 => 6, T8 => 7, T9 => 8, T10 => 9);
impl_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5, T7 => 6, T8 => 7, T9 => 8, T10 => 9, T11 => 10);
impl_tuple!(T1 => 0, T2 => 1, T3 => 2, T4 => 3, T5 => 4, T6 => 5, T7 => 6, T8 => 7, T9 => 8, T10 => 9, T11 => 10, T12 => 11);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::HandleInfo;

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

    /// Stand-in for a codegen-emitted `WrapperBucket(MockedResource)`.
    /// Exercises the bridge end-to-end without pulling in the wrapper
    /// codegen, via the same [`MockedResource::from_handle_cell`]
    /// helper that codegen-emitted impls use.
    #[derive(Debug, PartialEq, Eq)]
    struct StandinBucket(MockedResource);

    impl WitTypedWithResources for StandinBucket {
        fn from_cells(tree: &FieldTree, root: u32) -> Result<Self, BridgeError> {
            MockedResource::from_handle_cell(tree, root, std::borrow::Cow::Borrowed("bucket"))
                .map(StandinBucket)
        }
    }

    #[test]
    fn primitives_round_trip() {
        let t = empty_tree(vec![Cell::Integer(7)], 0);
        let v = u32::from_cells(&t, 0).unwrap();
        assert_eq!(v, 7);

        let t = empty_tree(vec![Cell::Text("hi".into())], 0);
        let v = String::from_cells(&t, 0).unwrap();
        assert_eq!(v, "hi");

        let t = empty_tree(vec![Cell::Bool(true)], 0);
        assert!(bool::from_cells(&t, 0).unwrap());
    }

    #[test]
    fn option_some_and_none_round_trip() {
        let t = empty_tree(vec![Cell::Integer(3), Cell::OptionSome(0)], 1);
        let v: Option<u32> = WitTypedWithResources::from_cells(&t, 1).unwrap();
        assert_eq!(v, Some(3));

        let t = empty_tree(vec![Cell::OptionNone], 0);
        let v: Option<u32> = WitTypedWithResources::from_cells(&t, 0).unwrap();
        assert_eq!(v, None);
    }

    #[test]
    fn vec_of_primitives_round_trip() {
        let t = empty_tree(
            vec![
                Cell::Integer(10),
                Cell::Integer(20),
                Cell::Integer(30),
                Cell::ListOf(vec![0, 1, 2]),
            ],
            3,
        );
        let v: Vec<u32> = WitTypedWithResources::from_cells(&t, 3).unwrap();
        assert_eq!(v, vec![10, 20, 30]);
    }

    #[test]
    fn result_payload_arms_round_trip() {
        let t = empty_tree(
            vec![
                Cell::Integer(7),
                Cell::Text("err".into()),
                Cell::ResultOk(Some(0)),
                Cell::ResultErr(Some(1)),
            ],
            2,
        );
        let v: Result<u32, String> = WitTypedWithResources::from_cells(&t, 2).unwrap();
        assert_eq!(v, Ok(7));
        let v: Result<u32, String> = WitTypedWithResources::from_cells(&t, 3).unwrap();
        assert_eq!(v, Err("err".into()));
    }

    #[test]
    fn result_unit_ok_arm_round_trips() {
        // `Result<(), E>` covers the WIT `result<_, E>` shape.
        let t = empty_tree(
            vec![Cell::Text("nope".into()), Cell::ResultErr(Some(0))],
            1,
        );
        let v: Result<(), String> = WitTypedWithResources::from_cells(&t, 1).unwrap();
        assert_eq!(v, Err("nope".into()));

        let t = empty_tree(vec![Cell::ResultOk(None)], 0);
        let v: Result<(), String> = WitTypedWithResources::from_cells(&t, 0).unwrap();
        assert_eq!(v, Ok(()));
    }

    #[test]
    fn tuple_of_primitives_round_trips() {
        let t = empty_tree(
            vec![
                Cell::Integer(1),
                Cell::Text("two".into()),
                Cell::TupleOf(vec![0, 1]),
            ],
            2,
        );
        let v: (u32, String) = WitTypedWithResources::from_cells(&t, 2).unwrap();
        assert_eq!(v, (1, "two".into()));
    }

    #[test]
    fn resource_leaf_decodes_through_stand_in_wrapper() {
        // A bare `bucket` return: one ResourceHandle cell pointing at
        // a handle_infos slot. The stand-in wrapper's impl is what a
        // codegen-emitted `WrapperBucket` impl will look like.
        let mut t = empty_tree(vec![Cell::ResourceHandle(0)], 0);
        t.handle_infos.push(HandleInfo {
            type_name: "bucket".into(),
            id: 17,
        });
        let b: StandinBucket = WitTypedWithResources::from_cells(&t, 0).unwrap();
        assert_eq!(
            b,
            StandinBucket(MockedResource {
                handle: 17,
                name: std::borrow::Cow::Borrowed("bucket"),
            })
        );
    }

    #[test]
    fn handle_decode_preserves_full_u64_range() {
        // Stream handles can exceed u32::MAX (HandleInfo::id is u64
        // and the wire encoder writes the full 8 bytes). Two handles
        // that differ only above 2^32 must decode to distinct
        // `MockedResource`s.
        let big_a = (u32::MAX as u64) + 1;
        let big_b = (u32::MAX as u64) + 2;

        let mut t = empty_tree(vec![Cell::ResourceHandle(0)], 0);
        t.handle_infos.push(HandleInfo {
            type_name: "bucket".into(),
            id: big_a,
        });
        let a: StandinBucket = WitTypedWithResources::from_cells(&t, 0).unwrap();

        let mut t = empty_tree(vec![Cell::ResourceHandle(0)], 0);
        t.handle_infos.push(HandleInfo {
            type_name: "bucket".into(),
            id: big_b,
        });
        let b: StandinBucket = WitTypedWithResources::from_cells(&t, 0).unwrap();

        assert_eq!(a.0.handle, big_a);
        assert_eq!(b.0.handle, big_b);
        assert_ne!(a, b);
    }

    #[test]
    fn result_ok_arm_carrying_resource_decodes() {
        // `result<bucket, store-err>`: Ok arm carries a resource,
        // Err arm is a value type. Using `String` for the Err keeps
        // the test SDK-only (a user enum would work the same way).
        let mut t = empty_tree(
            vec![Cell::ResourceHandle(0), Cell::ResultOk(Some(0))],
            1,
        );
        t.handle_infos.push(HandleInfo {
            type_name: "bucket".into(),
            id: 42,
        });
        let v: Result<StandinBucket, String> =
            WitTypedWithResources::from_cells(&t, 1).unwrap();
        assert_eq!(
            v,
            Ok(StandinBucket(MockedResource {
                handle: 42,
                name: std::borrow::Cow::Borrowed("bucket"),
            }))
        );
    }

    #[test]
    fn vec_of_resources_decodes() {
        // `list<bucket>`: each element decodes through the wrapper
        // impl. Stream handles differ per element.
        let mut t = empty_tree(
            vec![
                Cell::ResourceHandle(0),
                Cell::ResourceHandle(1),
                Cell::ListOf(vec![0, 1]),
            ],
            2,
        );
        t.handle_infos.push(HandleInfo {
            type_name: "bucket".into(),
            id: 11,
        });
        t.handle_infos.push(HandleInfo {
            type_name: "bucket".into(),
            id: 22,
        });
        let v: Vec<StandinBucket> = WitTypedWithResources::from_cells(&t, 2).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].0.handle, 11);
        assert_eq!(v[1].0.handle, 22);
    }

    #[test]
    fn option_of_resource_decodes_both_arms() {
        // `option<bucket>`: Some arm threads through the wrapper impl.
        let mut t = empty_tree(vec![Cell::ResourceHandle(0), Cell::OptionSome(0)], 1);
        t.handle_infos.push(HandleInfo {
            type_name: "bucket".into(),
            id: 7,
        });
        let v: Option<StandinBucket> = WitTypedWithResources::from_cells(&t, 1).unwrap();
        assert_eq!(v.as_ref().map(|b| b.0.handle), Some(7));

        let t = empty_tree(vec![Cell::OptionNone], 0);
        let v: Option<StandinBucket> = WitTypedWithResources::from_cells(&t, 0).unwrap();
        assert!(v.is_none());
    }

    #[test]
    fn cells_to_typed_with_resources_helper_dispatches() {
        let t = empty_tree(vec![Cell::Integer(99)], 0);
        let v: u32 = cells_to_typed_with_resources(&t, 0).unwrap();
        assert_eq!(v, 99);
    }

    #[test]
    fn has_default_err_constructs_default_err() {
        let r: Result<u32, String> = Result::default_err();
        assert_eq!(r, Err(String::new()));
        // Bound only requires `E: Default`; the Ok arm can hold a
        // resource type and the call still type-checks because Ok
        // is never instantiated.
        let r: Result<StandinBucket, String> = Result::default_err();
        assert_eq!(r, Err(String::new()));
    }

    #[test]
    fn has_arbitrary_err_samples_err_from_entropy() {
        let bytes = [0x42u8; 64];
        let mut u = arbitrary::Unstructured::new(&bytes);
        let r: Result<u32, u8> = <Result<u32, u8> as HasArbitraryErr>::arbitrary_err(&mut u)
            .expect("u8 arbitrary fits 64 bytes");
        // Always Err — the trait's contract — but the value is
        // entropy-derived. With a constant-byte input we get a
        // deterministic value; the meaningful invariant here is
        // "the Ok arm is never returned."
        assert!(r.is_err());
    }

    #[test]
    fn has_arbitrary_err_works_with_resource_bearing_ok_arm() {
        // The Ok arm doesn't need to impl Arbitrary, only E does,
        // so resource-bearing Ok arms compose without a full
        // `R: Arbitrary` bound.
        let bytes = [0xABu8; 64];
        let mut u = arbitrary::Unstructured::new(&bytes);
        let r: Result<StandinBucket, String> =
            <Result<StandinBucket, String> as HasArbitraryErr>::arbitrary_err(&mut u)
                .expect("String arbitrary fits 64 bytes");
        assert!(r.is_err());
    }

    #[test]
    fn has_arbitrary_err_varies_across_distinct_inputs() {
        // Different entropy → different Err payloads. Catches a
        // regression where the impl ignored its Unstructured.
        // u8 reads one byte deterministically, so two clearly
        // distinct seeds produce distinct results without depending
        // on `Arbitrary` encoding quirks for larger types.
        let mut ua = arbitrary::Unstructured::new(&[0x11u8; 32]);
        let mut ub = arbitrary::Unstructured::new(&[0xAAu8; 32]);
        let ra: Result<u32, u8> =
            <Result<u32, u8> as HasArbitraryErr>::arbitrary_err(&mut ua).unwrap();
        let rb: Result<u32, u8> =
            <Result<u32, u8> as HasArbitraryErr>::arbitrary_err(&mut ub).unwrap();
        assert_ne!(ra, rb);
    }

    /// Pins the macro's surface area through a stand-in wrapper, so
    /// the wrapper codegen can keep using a one-line invocation.
    pub struct MacroBucket(pub MockedResource);
    crate::impl_wit_typed_with_resources_for_wrapper!(MacroBucket, "bucket");

    #[test]
    fn impl_macro_round_trips_resource_handle() {
        let mut t = empty_tree(vec![Cell::ResourceHandle(0)], 0);
        t.handle_infos.push(HandleInfo {
            type_name: "bucket".into(),
            id: 99,
        });
        let v: MacroBucket = WitTypedWithResources::from_cells(&t, 0).unwrap();
        assert_eq!(v.0.handle, 99);
        assert_eq!(v.0.name.as_ref(), "bucket");
    }

    // Single-site macro expansion: one call site, one shared static,
    // successive constructor calls draw fresh ids from it.
    fn mint_bucket() -> MacroBucket {
        crate::mint_mock_resource!(MacroBucket, "bucket")
    }

    /// Stand-in for a codegen-emitted value-typed user type. WitTyped
    /// delegates to the inner primitive; WTWR comes from the macro.
    #[derive(Debug, PartialEq)]
    struct WrappedU32(u32);
    impl crate::WitTyped for WrappedU32 {
        fn wave_type() -> wasm_wave::value::Type {
            <u32 as crate::WitTyped>::wave_type()
        }
        fn to_value(&self) -> wasm_wave::value::Value {
            <u32 as crate::WitTyped>::to_value(&self.0)
        }
        fn from_value(v: &wasm_wave::value::Value) -> Result<Self, BridgeError> {
            <u32 as crate::WitTyped>::from_value(v).map(WrappedU32)
        }
    }
    crate::impl_wit_typed_with_resources_via_wave!(WrappedU32);

    #[test]
    fn via_wave_macro_round_trips_value_typed() {
        let t = empty_tree(vec![Cell::Integer(42)], 0);
        let v: WrappedU32 = WitTypedWithResources::from_cells(&t, 0).unwrap();
        assert_eq!(v, WrappedU32(42));
    }

    #[test]
    fn via_wave_macro_composes_inside_compound_wtwr_impl() {
        // `Vec<WrappedU32>::from_cells` resolves through the SDK's
        // generic `Vec<T: WTWR>` impl, which recurses via
        // `WrappedU32::from_cells` (the macro-installed one). Pins
        // that the dual-impl pattern composes with the existing
        // container impls.
        let t = empty_tree(
            vec![Cell::Integer(10), Cell::Integer(20), Cell::ListOf(vec![0, 1])],
            2,
        );
        let v: Vec<WrappedU32> = WitTypedWithResources::from_cells(&t, 2).unwrap();
        assert_eq!(v, vec![WrappedU32(10), WrappedU32(20)]);
    }

    #[test]
    fn mint_macro_yields_distinct_handles_across_calls() {
        let a = mint_bucket();
        let b = mint_bucket();
        assert_ne!(a.0.handle, b.0.handle);
        assert_eq!(a.0.name.as_ref(), "bucket");
        assert_eq!(b.0.name.as_ref(), "bucket");
    }

    #[test]
    fn mint_macro_starts_handles_above_zero() {
        // `0` could collide with handles a strategy treats as
        // "missing"; the counter starts at 1 to avoid that ambiguity.
        let v = mint_bucket();
        assert!(v.0.handle >= 1);
    }

    #[test]
    fn impl_macro_propagates_decode_error_variants() {
        // Out-of-bounds root index surfaces as `CellOutOfBounds`,
        // matching the SDK's own walkers rather than collapsing to
        // an `Unsupported` catch-all.
        let t = empty_tree(vec![], 0);
        let r: Result<MacroBucket, _> = WitTypedWithResources::from_cells(&t, 0);
        assert!(matches!(r, Err(BridgeError::CellOutOfBounds { .. })));

        // Wrong cell kind at root → `Unsupported` (consistent with
        // [`MockedResource::from_handle_cell`]).
        let t = empty_tree(vec![Cell::Bool(true)], 0);
        let r: Result<MacroBucket, _> = WitTypedWithResources::from_cells(&t, 0);
        assert!(matches!(r, Err(BridgeError::Unsupported(_))));
    }

    #[test]
    fn bytes_cell_decodes_into_vec_u8() {
        // `list<u8>` records as a single `Cell::Bytes`; `Vec<u8>` rebuilds
        // from it via the `vec_from_bytes` fast path.
        let t = empty_tree(vec![Cell::Bytes(vec![1, 2, 3])], 0);
        let v: Vec<u8> = WitTypedWithResources::from_cells(&t, 0).unwrap();
        assert_eq!(v, vec![1, 2, 3]);
    }

    #[test]
    fn bytes_cell_rejected_for_non_u8_vec() {
        // The fast path is `u8`-only; any other element type still rejects it.
        let t = empty_tree(vec![Cell::Bytes(vec![1, 2, 3])], 0);
        let r: Result<Vec<u32>, _> = WitTypedWithResources::from_cells(&t, 0);
        assert!(matches!(r, Err(BridgeError::Unsupported(_))));
    }
}
