//! Translate [`cviz`]'s value-type arena into a [`wit_parser`]
//! [`Resolve`] with a pre-filled [`SizeAlign`], and expose the
//! canonical-ABI queries (size, flat types, string/list predicates)
//! the adapter generator needs.
//!
//! [`WitBridge::from_cviz`] walks the cviz arena once, allocates a
//! [`TypeDef`] per compound type, and records a
//! `HashMap<ValueTypeId, Type>` so every call site can look up the
//! wit-parser handle for a cviz id in O(1). Types are inserted
//! children-first so `Resolve::types` stays topologically ordered —
//! [`SizeAlign::fill`] relies on that invariant.
//!
//! ## Mapping notes
//!
//! - Primitives (bool / int / float / char / string / error-context)
//!   are direct `Type::*` variants; no `TypeDef` allocated.
//! - `Resource(name)` becomes a `Handle(Own(resource_id))` where
//!   `resource_id` points to a `TypeDef { kind: Resource }` deduped
//!   by name. Splicer doesn't distinguish own vs borrow — both
//!   flatten to one `i32`, so defaulting to `Own` is layout-correct.
//! - `AsyncHandle` maps to `Future(None)` — wit-parser has no bare
//!   async-handle variant, and `Future(None)` has the right 4-byte
//!   i32 layout.
//! - `Map(k, v)` uses `TypeDefKind::Map`, which lowers to the same
//!   `(ptr, len)` shape as `List`.

use std::collections::HashMap;

use cviz::model::{TypeArena, ValueType, ValueTypeId};
use wasm_encoder::ValType;
use wit_parser::{
    abi::{FlatTypes, WasmType},
    Case, Docs, Enum, EnumCase, Field, Flag, Flags, Handle, Record, Resolve, Result_, SizeAlign,
    Stability, Tuple, Type, TypeDef, TypeDefKind, TypeId, TypeOwner, Variant,
};

/// Owns a [`Resolve`] built from a cviz arena plus a pre-filled
/// [`SizeAlign`]. Every adapter-gen pass builds one and threads it
/// through the emitters.
pub(super) struct WitBridge {
    pub resolve: Resolve,
    pub sizes: SizeAlign,
    type_map: HashMap<ValueTypeId, Type>,
    /// Cache of `Resource(name) → TypeId of the Resource TypeDef`.
    /// Distinct `Resource("request")` and `Resource("response")`
    /// cviz ids with the same name share one upstream resource.
    resource_by_name: HashMap<String, TypeId>,
}

impl WitBridge {
    /// Translate the full cviz arena into a wit-parser `Resolve`.
    /// Every `ValueTypeId` in the arena gets a `Type` in
    /// [`Self::get`].
    pub fn from_cviz(arena: &TypeArena) -> Self {
        let mut bridge = Self {
            resolve: Resolve::default(),
            sizes: SizeAlign::default(),
            type_map: HashMap::new(),
            resource_by_name: HashMap::new(),
        };

        for id in arena.iter_val_ids() {
            bridge.translate(id, arena);
        }

        bridge.sizes.fill(&bridge.resolve);
        bridge
    }

    /// Look up the wit-parser `Type` for a cviz id. Panics if the id
    /// wasn't in the arena at construction time — that would indicate
    /// a new cviz type appearing after bridge construction, which
    /// splicer's single-pass generator shouldn't do.
    pub fn get(&self, id: ValueTypeId) -> Type {
        self.type_map[&id]
    }

    /// Canonical-ABI byte size for a cviz type (wasm32 memory model).
    pub fn size_bytes(&self, id: ValueTypeId) -> u32 {
        self.sizes.size(&self.get(id)).size_wasm32() as u32
    }

    /// Canonical-ABI flattened core-Wasm types for a cviz type.
    /// Uses [`Resolve::push_flat`] with a fixed-size scratch buffer
    /// (MAX_FLAT_PARAMS = 16 is the canonical cap; 32 accommodates a
    /// non-overflow margin).
    pub fn flat_types(&self, id: ValueTypeId) -> Vec<ValType> {
        let ty = self.get(id);
        let mut buf = [WasmType::I32; 32];
        let mut flat = FlatTypes::new(&mut buf);
        self.resolve.push_flat(&ty, &mut flat);
        flat.to_vec().into_iter().map(wasm_to_val).collect()
    }

    /// True if the type (or any type it transitively contains) is a
    /// string. Drives the `needs_utf8` / `needs_memory` decisions.
    pub fn has_strings(&self, id: ValueTypeId) -> bool {
        self.any_type(self.get(id), &|ty, _| matches!(ty, Type::String))
    }

    /// True if the type (or any type it transitively contains) is a
    /// dynamic or fixed-size list, or a map (upstream lowers Map the
    /// same way). Drives the `needs_realloc` decision.
    pub fn has_lists(&self, id: ValueTypeId) -> bool {
        self.any_type(self.get(id), &|_, kind| {
            matches!(
                kind,
                Some(TypeDefKind::List(_) | TypeDefKind::FixedSizeList(..) | TypeDefKind::Map(..))
            )
        })
    }

    /// Recursive predicate walker over a `Type`. Visits the node
    /// itself (with its `TypeDefKind` if compound) and every
    /// transitively-contained child `Type` until `pred` returns
    /// true. Stops on the first hit.
    fn any_type(&self, ty: Type, pred: &impl Fn(&Type, Option<&TypeDefKind>) -> bool) -> bool {
        let kind = match ty {
            Type::Id(id) => Some(&self.resolve.types[id].kind),
            _ => None,
        };
        if pred(&ty, kind) {
            return true;
        }
        let Some(kind) = kind else {
            return false;
        };
        match kind {
            TypeDefKind::Record(r) => r.fields.iter().any(|f| self.any_type(f.ty, pred)),
            TypeDefKind::Tuple(t) => t.types.iter().any(|t| self.any_type(*t, pred)),
            TypeDefKind::Variant(v) => v
                .cases
                .iter()
                .any(|c| c.ty.is_some_and(|t| self.any_type(t, pred))),
            TypeDefKind::Option(t) => self.any_type(*t, pred),
            TypeDefKind::Result(r) => {
                r.ok.is_some_and(|t| self.any_type(t, pred))
                    || r.err.is_some_and(|t| self.any_type(t, pred))
            }
            TypeDefKind::List(t) | TypeDefKind::FixedSizeList(t, _) => self.any_type(*t, pred),
            TypeDefKind::Map(k, v) => self.any_type(*k, pred) || self.any_type(*v, pred),
            TypeDefKind::Type(t) => self.any_type(*t, pred),
            TypeDefKind::Enum(_)
            | TypeDefKind::Flags(_)
            | TypeDefKind::Handle(_)
            | TypeDefKind::Resource
            | TypeDefKind::Future(_)
            | TypeDefKind::Stream(_)
            | TypeDefKind::Unknown => false,
        }
    }

    /// Translate one cviz id, inserting dependencies before self so
    /// `Resolve::types` stays topologically ordered.
    fn translate(&mut self, id: ValueTypeId, arena: &TypeArena) -> Type {
        if let Some(ty) = self.type_map.get(&id) {
            return *ty;
        }

        let translated = match arena.lookup_val(id) {
            ValueType::Bool => Type::Bool,
            ValueType::S8 => Type::S8,
            ValueType::U8 => Type::U8,
            ValueType::S16 => Type::S16,
            ValueType::U16 => Type::U16,
            ValueType::S32 => Type::S32,
            ValueType::U32 => Type::U32,
            ValueType::S64 => Type::S64,
            ValueType::U64 => Type::U64,
            ValueType::F32 => Type::F32,
            ValueType::F64 => Type::F64,
            ValueType::Char => Type::Char,
            ValueType::String => Type::String,
            ValueType::ErrorContext => Type::ErrorContext,

            ValueType::Resource(name) => {
                let resource_id = self.resource_for_name(name);
                self.alloc(TypeDefKind::Handle(Handle::Own(resource_id)))
            }
            // Upstream has no bare async-handle variant; Future(None)
            // flattens / sizes to the same 4-byte i32.
            ValueType::AsyncHandle => self.alloc(TypeDefKind::Future(None)),

            ValueType::List(inner) => {
                let inner = self.translate(*inner, arena);
                self.alloc(TypeDefKind::List(inner))
            }
            ValueType::FixedSizeList(inner, n) => {
                let inner = self.translate(*inner, arena);
                self.alloc(TypeDefKind::FixedSizeList(inner, *n))
            }
            ValueType::Map(k, v) => {
                let k = self.translate(*k, arena);
                let v = self.translate(*v, arena);
                self.alloc(TypeDefKind::Map(k, v))
            }
            ValueType::Tuple(ids) => {
                let types: Vec<Type> = ids
                    .clone()
                    .iter()
                    .map(|i| self.translate(*i, arena))
                    .collect();
                self.alloc(TypeDefKind::Tuple(Tuple { types }))
            }
            ValueType::Record(fields) => {
                let fields: Vec<Field> = fields
                    .clone()
                    .iter()
                    .map(|(name, fid)| Field {
                        name: name.clone(),
                        ty: self.translate(*fid, arena),
                        docs: Docs::default(),
                    })
                    .collect();
                self.alloc(TypeDefKind::Record(Record { fields }))
            }
            ValueType::Variant(cases) => {
                let cases: Vec<Case> = cases
                    .clone()
                    .iter()
                    .map(|(name, payload)| Case {
                        name: name.clone(),
                        ty: payload.map(|p| self.translate(p, arena)),
                        docs: Docs::default(),
                    })
                    .collect();
                self.alloc(TypeDefKind::Variant(Variant { cases }))
            }
            ValueType::Enum(names) => {
                let cases: Vec<EnumCase> = names
                    .iter()
                    .map(|name| EnumCase {
                        name: name.clone(),
                        docs: Docs::default(),
                    })
                    .collect();
                self.alloc(TypeDefKind::Enum(Enum { cases }))
            }
            ValueType::Option(inner) => {
                let inner = self.translate(*inner, arena);
                self.alloc(TypeDefKind::Option(inner))
            }
            ValueType::Result { ok, err } => {
                let ok = ok.map(|i| self.translate(i, arena));
                let err = err.map(|i| self.translate(i, arena));
                self.alloc(TypeDefKind::Result(Result_ { ok, err }))
            }
            ValueType::Flags(names) => {
                let flags: Vec<Flag> = names
                    .iter()
                    .map(|name| Flag {
                        name: name.clone(),
                        docs: Docs::default(),
                    })
                    .collect();
                self.alloc(TypeDefKind::Flags(Flags { flags }))
            }
        };

        self.type_map.insert(id, translated);
        translated
    }

    /// Allocate a `TypeDef` in the Resolve with defaulted
    /// metadata, returning a `Type::Id` handle to it.
    fn alloc(&mut self, kind: TypeDefKind) -> Type {
        let id = self.resolve.types.alloc(TypeDef {
            name: None,
            kind,
            owner: TypeOwner::None,
            docs: Docs::default(),
            stability: Stability::default(),
        });
        Type::Id(id)
    }

    /// Look up or allocate the Resource TypeDef for a given name.
    /// Empty name collapses to a single anonymous resource so
    /// unnamed cviz resources share layout too.
    fn resource_for_name(&mut self, name: &str) -> TypeId {
        if let Some(id) = self.resource_by_name.get(name) {
            return *id;
        }
        let id = self.resolve.types.alloc(TypeDef {
            name: if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            },
            kind: TypeDefKind::Resource,
            owner: TypeOwner::None,
            docs: Docs::default(),
            stability: Stability::default(),
        });
        self.resource_by_name.insert(name.to_string(), id);
        id
    }
}

/// Map wit-parser's flat-type alphabet to wasm-encoder's `ValType`.
/// Pointer and Length collapse to `I32` (wasm32), `PointerOrI64` to
/// `I64`. Splicer doesn't need pointer-provenance distinctions — the
/// emitted wasm just uses i32 / i64 loads.
fn wasm_to_val(wt: WasmType) -> ValType {
    match wt {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => ValType::I32,
        WasmType::I64 | WasmType::PointerOrI64 => ValType::I64,
        WasmType::F32 => ValType::F32,
        WasmType::F64 => ValType::F64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cviz::model::TypeArena;
    use wit_parser::Alignment;

    fn bytes(a: Alignment) -> usize {
        a.align_wasm32()
    }

    #[test]
    fn primitives_size_and_align_match() {
        let mut arena = TypeArena::default();
        let cases = [
            (arena.intern_val(ValueType::Bool), 1, 1),
            (arena.intern_val(ValueType::U8), 1, 1),
            (arena.intern_val(ValueType::S16), 2, 2),
            (arena.intern_val(ValueType::U32), 4, 4),
            (arena.intern_val(ValueType::S64), 8, 8),
            (arena.intern_val(ValueType::F32), 4, 4),
            (arena.intern_val(ValueType::F64), 8, 8),
            (arena.intern_val(ValueType::Char), 4, 4),
            (arena.intern_val(ValueType::String), 8, 4),
        ];
        let bridge = WitBridge::from_cviz(&arena);
        for (id, expected_size, expected_align) in cases {
            let ty = bridge.get(id);
            assert_eq!(bridge.sizes.size(&ty).size_wasm32(), expected_size);
            assert_eq!(bytes(bridge.sizes.align(&ty)), expected_align);
        }
    }

    #[test]
    fn record_with_string_and_u32_has_canonical_layout() {
        let mut arena = TypeArena::default();
        let string_id = arena.intern_val(ValueType::String);
        let u32_id = arena.intern_val(ValueType::U32);
        let record_id = arena.intern_val(ValueType::Record(vec![
            ("name".to_string(), string_id),
            ("age".to_string(), u32_id),
        ]));

        let bridge = WitBridge::from_cviz(&arena);
        let ty = bridge.get(record_id);
        // string (ptr+len = 8 bytes, align 4) + u32 (4 bytes, align 4) = 12 bytes, align 4.
        assert_eq!(bridge.sizes.size(&ty).size_wasm32(), 12);
        assert_eq!(bytes(bridge.sizes.align(&ty)), 4);
    }

    #[test]
    fn variant_heterogeneous_arms_use_max_payload_layout() {
        let mut arena = TypeArena::default();
        let u8_id = arena.intern_val(ValueType::U8);
        let u64_id = arena.intern_val(ValueType::U64);
        // variant { small(u8), big(u64) } — disc 1 byte, payload
        // aligned to 8 (max), total 16.
        let variant_id = arena.intern_val(ValueType::Variant(vec![
            ("small".to_string(), Some(u8_id)),
            ("big".to_string(), Some(u64_id)),
        ]));

        let bridge = WitBridge::from_cviz(&arena);
        let ty = bridge.get(variant_id);
        assert_eq!(bridge.sizes.size(&ty).size_wasm32(), 16);
        assert_eq!(bytes(bridge.sizes.align(&ty)), 8);
    }

    #[test]
    fn fixed_size_list_is_n_copies() {
        let mut arena = TypeArena::default();
        let u32_id = arena.intern_val(ValueType::U32);
        let list_id = arena.intern_val(ValueType::FixedSizeList(u32_id, 4));

        let bridge = WitBridge::from_cviz(&arena);
        let ty = bridge.get(list_id);
        assert_eq!(bridge.sizes.size(&ty).size_wasm32(), 16);
        assert_eq!(bytes(bridge.sizes.align(&ty)), 4);
    }

    #[test]
    fn resource_is_four_bytes() {
        let mut arena = TypeArena::default();
        let request_id = arena.intern_val(ValueType::Resource("request".to_string()));
        let response_id = arena.intern_val(ValueType::Resource("response".to_string()));

        let bridge = WitBridge::from_cviz(&arena);
        assert_eq!(bridge.sizes.size(&bridge.get(request_id)).size_wasm32(), 4);
        assert_eq!(bridge.sizes.size(&bridge.get(response_id)).size_wasm32(), 4);
    }

    #[test]
    fn async_handle_is_four_bytes() {
        let mut arena = TypeArena::default();
        let id = arena.intern_val(ValueType::AsyncHandle);
        let bridge = WitBridge::from_cviz(&arena);
        assert_eq!(bridge.sizes.size(&bridge.get(id)).size_wasm32(), 4);
    }

    #[test]
    fn flags_packing_follows_canonical_rules() {
        let mut arena = TypeArena::default();
        let five = arena.intern_val(ValueType::Flags((0..5).map(|i| format!("f{i}")).collect()));
        let twenty = arena.intern_val(ValueType::Flags((0..20).map(|i| format!("f{i}")).collect()));
        let forty = arena.intern_val(ValueType::Flags((0..40).map(|i| format!("f{i}")).collect()));

        let bridge = WitBridge::from_cviz(&arena);
        assert_eq!(bridge.sizes.size(&bridge.get(five)).size_wasm32(), 1);
        assert_eq!(bridge.sizes.size(&bridge.get(twenty)).size_wasm32(), 4);
        assert_eq!(bridge.sizes.size(&bridge.get(forty)).size_wasm32(), 8);
    }

    #[test]
    fn flat_types_for_string_is_two_i32s() {
        let mut arena = TypeArena::default();
        let id = arena.intern_val(ValueType::String);
        let bridge = WitBridge::from_cviz(&arena);
        assert_eq!(bridge.flat_types(id), vec![ValType::I32, ValType::I32]);
    }

    #[test]
    fn flat_types_for_heterogeneous_variant_joins_arms() {
        let mut arena = TypeArena::default();
        let u8_id = arena.intern_val(ValueType::U8);
        let u64_id = arena.intern_val(ValueType::U64);
        let variant_id = arena.intern_val(ValueType::Variant(vec![
            ("small".to_string(), Some(u8_id)),
            ("big".to_string(), Some(u64_id)),
        ]));
        let bridge = WitBridge::from_cviz(&arena);
        // disc (i32) + joined payload (u8 + u64 -> i64)
        assert_eq!(
            bridge.flat_types(variant_id),
            vec![ValType::I32, ValType::I64]
        );
    }

    #[test]
    fn has_strings_finds_nested_string() {
        let mut arena = TypeArena::default();
        let string_id = arena.intern_val(ValueType::String);
        let u32_id = arena.intern_val(ValueType::U32);
        let nested = arena.intern_val(ValueType::Record(vec![
            ("s".to_string(), string_id),
            ("n".to_string(), u32_id),
        ]));
        let deep = arena.intern_val(ValueType::Option(nested));
        let bridge = WitBridge::from_cviz(&arena);
        assert!(bridge.has_strings(deep));
        assert!(!bridge.has_strings(u32_id));
    }

    #[test]
    fn has_lists_finds_list_fixed_list_and_map() {
        let mut arena = TypeArena::default();
        let u8_id = arena.intern_val(ValueType::U8);
        let string_id = arena.intern_val(ValueType::String);
        let list_id = arena.intern_val(ValueType::List(u8_id));
        let fixed_id = arena.intern_val(ValueType::FixedSizeList(u8_id, 4));
        let map_id = arena.intern_val(ValueType::Map(string_id, u8_id));
        let plain_record = arena.intern_val(ValueType::Record(vec![("n".to_string(), u8_id)]));

        let bridge = WitBridge::from_cviz(&arena);
        assert!(bridge.has_lists(list_id));
        assert!(bridge.has_lists(fixed_id));
        assert!(bridge.has_lists(map_id));
        assert!(!bridge.has_lists(plain_record));
        assert!(!bridge.has_lists(string_id));
    }
}
