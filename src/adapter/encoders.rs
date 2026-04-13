//! Component-level and instance-level type encoders for the
//! tier-1 adapter generator.
//!
//! Three encoders live here:
//!
//! - [`InstTypeCtx`] — recursively encodes component value types into
//!   an [`InstanceType`] for use as the *handler-import* instance
//!   type. Tracks resource exports and supports two resource modes:
//!   in-instance `SubResource` exports (the default) or `alias outer`
//!   to a parent-scope resource (the via-types-instance strategy).
//! - [`build_types_instance_type`] / [`encode_types_inst_cv`] —
//!   build the *types-instance* import type. In an import instance
//!   type, compound types referenced by other types must be exported
//!   via `(type (eq N))` and later types must reference the export
//!   index, not the raw definition index. This encoder is what
//!   enforces that rule (see `memory/project_proxy_import_type.md`).
//! - [`encode_comp_cv`] — encodes a value type at the *component*
//!   scope (the outer Component's `ComponentTypeSection`). Used for
//!   the adapter's own export instance type and for the lift function
//!   types.
//!
//! All three share the `prim_cv` primitive shortcut from `super::ty`.

use std::collections::{BTreeMap, HashMap};

use cviz::model::{TypeArena, ValueType, ValueTypeId};
use wasm_encoder::{
    ComponentTypeRef, ComponentTypeSection, ComponentValType, InstanceType, PrimitiveValType,
    TypeBounds,
};

use super::ty::prim_cv;

// ─── Types-instance encoder (export-aware) ─────────────────────────────────
//
// In an import instance type, compound types (records, variants) that are
// referenced by other types MUST be exported via `(type (eq N))` and later
// types must use the EXPORT type index. This encoder handles that by
// exporting named types immediately after defining them.

/// Build the types instance type with export-aware indexing.
///
/// `type_exports` maps export names (e.g. "request", "error-code") to ValueTypeIds.
/// Resources are exported as SubResource; compound types are defined, exported (eq),
/// and subsequent references use the export index.
pub(super) fn build_types_instance_type(
    type_exports: &BTreeMap<String, ValueTypeId>,
    arena: &TypeArena,
) -> anyhow::Result<InstanceType> {
    let mut inst = InstanceType::new();
    let mut cache: HashMap<ValueTypeId, ComponentValType> = HashMap::new();
    let name_map: HashMap<ValueTypeId, &str> = type_exports
        .iter()
        .map(|(name, &vid)| (vid, name.as_str()))
        .collect();

    for (_name, &vid) in type_exports {
        encode_types_inst_cv(vid, &mut inst, arena, &mut cache, &name_map)?;
    }

    Ok(inst)
}

/// Recursively encode a value type into the types instance type.
///
/// Returns the `ComponentValType` to use when referencing this type.
/// Named types (those in `name_map`) are exported and the returned CVT
/// uses the export's type index; anonymous types use the raw definition index.
pub(super) fn encode_types_inst_cv(
    id: ValueTypeId,
    inst: &mut InstanceType,
    arena: &TypeArena,
    cache: &mut HashMap<ValueTypeId, ComponentValType>,
    name_map: &HashMap<ValueTypeId, &str>,
) -> anyhow::Result<ComponentValType> {
    if let Some(&cv) = cache.get(&id) {
        return Ok(cv);
    }

    let vt = arena.lookup_val(id).clone();

    if let Some(cv) = prim_cv(&vt) {
        return Ok(cv);
    }

    if matches!(vt, ValueType::Resource(_) | ValueType::AsyncHandle) {
        let name = name_map.get(&id).copied().unwrap_or("resource");
        let idx = inst.type_count();
        inst.export(name, ComponentTypeRef::Type(TypeBounds::SubResource));
        let cv = ComponentValType::Type(idx);
        cache.insert(id, cv);
        return Ok(cv);
    }

    let raw_idx = match vt {
        ValueType::Record(ref fields) => {
            let mut encoded: Vec<(String, ComponentValType)> = Vec::new();
            for (name, fid) in fields {
                encoded.push((name.clone(), encode_types_inst_cv(*fid, inst, arena, cache, name_map)?));
            }
            let idx = inst.type_count();
            inst.ty().defined_type().record(encoded.iter().map(|(n, cv)| (n.as_str(), *cv)));
            idx
        }
        ValueType::Variant(ref cases) => {
            let mut encoded: Vec<(String, Option<ComponentValType>)> = Vec::new();
            for (name, opt_id) in cases {
                let opt_cv = opt_id.map(|fid| encode_types_inst_cv(fid, inst, arena, cache, name_map)).transpose()?;
                encoded.push((name.clone(), opt_cv));
            }
            let idx = inst.type_count();
            inst.ty().defined_type().variant(encoded.iter().map(|(n, cv)| (n.as_str(), *cv)));
            idx
        }
        ValueType::Option(inner) => {
            let inner_cv = encode_types_inst_cv(inner, inst, arena, cache, name_map)?;
            let idx = inst.type_count();
            inst.ty().defined_type().option(inner_cv);
            idx
        }
        ValueType::Result { ok, err } => {
            let ok_cv = ok.map(|fid| encode_types_inst_cv(fid, inst, arena, cache, name_map)).transpose()?;
            let err_cv = err.map(|fid| encode_types_inst_cv(fid, inst, arena, cache, name_map)).transpose()?;
            let idx = inst.type_count();
            inst.ty().defined_type().result(ok_cv, err_cv);
            idx
        }
        ValueType::Tuple(ref ids) => {
            let mut encoded: Vec<ComponentValType> = Vec::new();
            for fid in ids {
                encoded.push(encode_types_inst_cv(*fid, inst, arena, cache, name_map)?);
            }
            let idx = inst.type_count();
            inst.ty().defined_type().tuple(encoded.into_iter());
            idx
        }
        ValueType::List(inner) => {
            let inner_cv = encode_types_inst_cv(inner, inst, arena, cache, name_map)?;
            let idx = inst.type_count();
            inst.ty().defined_type().list(inner_cv);
            idx
        }
        ValueType::Enum(ref tags) => {
            let idx = inst.type_count();
            inst.ty().defined_type().enum_type(tags.iter().map(|s| s.as_str()));
            idx
        }
        ValueType::Flags(ref names) => {
            let idx = inst.type_count();
            inst.ty().defined_type().flags(names.iter().map(|s| s.as_str()));
            idx
        }
        other => anyhow::bail!(
            "Unsupported type {:?} in tier-1 adapter types-instance encoding. \
             If you need support for this type, \
             please open an issue with a repro at https://github.com/ejrgilbert/splicer/issues",
            other
        ),
    };

    let needs_export = name_map.contains_key(&id)
        || matches!(vt, ValueType::Record(_) | ValueType::Variant(_));

    if needs_export {
        let name = name_map.get(&id).copied().unwrap_or_else(|| {
            Box::leak(format!("type-{}", raw_idx).into_boxed_str())
        });
        let export_idx = inst.type_count();
        inst.export(name, ComponentTypeRef::Type(TypeBounds::Eq(raw_idx)));
        let cv = ComponentValType::Type(export_idx);
        cache.insert(id, cv);
        Ok(cv)
    } else {
        let cv = ComponentValType::Type(raw_idx);
        cache.insert(id, cv);
        Ok(cv)
    }
}

// ─── InstTypeCtx: recursive InstanceType encoder ────────────────────────────

/// Encodes component types into an InstanceType recursively.
///
/// Tracks:
/// - `cache`: ValueTypeId → local type index within the InstanceType
/// - `resource_exports`: (vid, export_name, resource_local_idx, own_local_idx)
/// - `outer_resources`: When non-empty, resources are resolved via `alias outer`
///   instead of being exported as SubResource.  Maps ValueTypeId → component-scope type index.
pub(super) struct InstTypeCtx {
    pub cache: HashMap<ValueTypeId, u32>,
    pub resource_exports: Vec<(ValueTypeId, String, u32, u32)>,
    /// Maps resource ValueTypeId → component-scope type index.
    /// When populated, resources use `alias outer 1 <comp_idx>` + inline own<T>
    /// instead of SubResource exports.
    pub outer_resources: HashMap<ValueTypeId, u32>,
    /// Maps resource ValueTypeId → local alias index within the instance type.
    /// Populated by the caller after emitting `alias outer` declarations.
    pub alias_locals: HashMap<ValueTypeId, u32>,
}

impl InstTypeCtx {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            resource_exports: Vec::new(),
            outer_resources: HashMap::new(),
            alias_locals: HashMap::new(),
        }
    }

    pub fn with_outer_resources(outer: HashMap<ValueTypeId, u32>) -> Self {
        Self {
            cache: HashMap::new(),
            resource_exports: Vec::new(),
            outer_resources: outer,
            alias_locals: HashMap::new(),
        }
    }

    /// Encodes a value type into the InstanceType, returning its `ComponentValType`.
    ///
    /// - Primitives: returns `ComponentValType::Primitive(...)` (no allocation).
    /// - Resources: declares sub-resource export + `own<>`, returns `Type(own_local_idx)`.
    ///   Named resources (`ValueType::Resource("request")`) use their interface name as the
    ///   export name; unnamed resources use synthetic `"res-N"` names.  Each distinct
    ///   `ValueTypeId` is encoded only once (cached).
    /// - Compounds (result, variant, option, record, etc.): recursively encodes,
    ///   returns `Type(compound_local_idx)`.
    pub fn encode_cv(
        &mut self,
        id: ValueTypeId,
        inst: &mut InstanceType,
        arena: &TypeArena,
    ) -> anyhow::Result<ComponentValType> {
        // Primitives — no local type needed.
        if let Some(cv) = prim_cv(arena.lookup_val(id)) {
            return Ok(cv);
        }

        // Already encoded?
        if let Some(&local_idx) = self.cache.get(&id) {
            return Ok(ComponentValType::Type(local_idx));
        }

        // Clone to avoid borrow conflicts during recursion.
        let vt = arena.lookup_val(id).clone();

        let local_idx = match vt {
            ValueType::Resource(ref name) => {
                if self.outer_resources.contains_key(&id) {
                    let alias_local = self.alias_locals.get(&id).copied().ok_or_else(|| {
                        anyhow::anyhow!(
                            "outer_resources entry for {:?} but no alias_locals entry; \
                             alias outer should have been emitted before encode_cv",
                            id
                        )
                    })?;
                    let own_local = inst.type_count();
                    inst.ty().defined_type().own(alias_local);
                    let export_name = if name.is_empty() {
                        format!("res-{}", self.resource_exports.len())
                    } else {
                        name.clone()
                    };
                    self.resource_exports
                        .push((id, export_name, alias_local, own_local));
                    own_local
                } else {
                    let export_name = if name.is_empty() {
                        format!("res-{}", self.resource_exports.len())
                    } else {
                        name.clone()
                    };
                    let res_local = inst.type_count();
                    inst.export(&export_name, ComponentTypeRef::Type(TypeBounds::SubResource));
                    let own_local = inst.type_count();
                    inst.ty().defined_type().own(res_local);
                    self.resource_exports
                        .push((id, export_name, res_local, own_local));
                    own_local
                }
            }
            ValueType::AsyncHandle => {
                if self.outer_resources.contains_key(&id) {
                    let alias_local = self.alias_locals.get(&id).copied().ok_or_else(|| {
                        anyhow::anyhow!(
                            "outer_resources entry for AsyncHandle but no alias_locals entry"
                        )
                    })?;
                    let own_local = inst.type_count();
                    inst.ty().defined_type().own(alias_local);
                    let export_name = format!("res-{}", self.resource_exports.len());
                    self.resource_exports
                        .push((id, export_name, alias_local, own_local));
                    own_local
                } else {
                    let export_name = format!("res-{}", self.resource_exports.len());
                    let res_local = inst.type_count();
                    inst.export(&export_name, ComponentTypeRef::Type(TypeBounds::SubResource));
                    let own_local = inst.type_count();
                    inst.ty().defined_type().own(res_local);
                    self.resource_exports
                        .push((id, export_name, res_local, own_local));
                    own_local
                }
            }

            ValueType::Option(inner_id) => {
                let inner_cv = self.encode_cv(inner_id, inst, arena)?;
                let idx = inst.type_count();
                inst.ty().defined_type().option(inner_cv);
                idx
            }

            ValueType::Result { ok, err } => {
                let ok_cv = ok.map(|id| self.encode_cv(id, inst, arena)).transpose()?;
                let err_cv = err.map(|id| self.encode_cv(id, inst, arena)).transpose()?;
                let idx = inst.type_count();
                inst.ty().defined_type().result(ok_cv, err_cv);
                idx
            }

            ValueType::Variant(cases) => {
                let mut encoded: Vec<(String, Option<ComponentValType>)> = Vec::new();
                for (name, opt_id) in &cases {
                    let opt_cv = opt_id
                        .map(|id| self.encode_cv(id, inst, arena))
                        .transpose()?;
                    encoded.push((name.clone(), opt_cv));
                }
                let idx = inst.type_count();
                inst.ty()
                    .defined_type()
                    .variant(encoded.iter().map(|(n, cv)| (n.as_str(), *cv)));
                idx
            }

            ValueType::Record(fields) => {
                let mut encoded: Vec<(String, ComponentValType)> = Vec::new();
                for (name, id) in &fields {
                    encoded.push((name.clone(), self.encode_cv(*id, inst, arena)?));
                }
                let idx = inst.type_count();
                inst.ty()
                    .defined_type()
                    .record(encoded.iter().map(|(n, cv)| (n.as_str(), *cv)));
                idx
            }

            ValueType::Tuple(ids) => {
                let mut encoded: Vec<ComponentValType> = Vec::new();
                for id in &ids {
                    encoded.push(self.encode_cv(*id, inst, arena)?);
                }
                let idx = inst.type_count();
                inst.ty().defined_type().tuple(encoded.into_iter());
                idx
            }

            ValueType::List(inner_id) => {
                let inner_cv = self.encode_cv(inner_id, inst, arena)?;
                let idx = inst.type_count();
                inst.ty().defined_type().list(inner_cv);
                idx
            }

            ValueType::FixedSizeList(inner_id, _n) => {
                let inner_cv = self.encode_cv(inner_id, inst, arena)?;
                let idx = inst.type_count();
                inst.ty().defined_type().list(inner_cv);
                idx
            }

            ValueType::Enum(tags) => {
                let idx = inst.type_count();
                inst.ty()
                    .defined_type()
                    .enum_type(tags.iter().map(|s| s.as_str()));
                idx
            }

            ValueType::Flags(names) => {
                let idx = inst.type_count();
                inst.ty()
                    .defined_type()
                    .flags(names.iter().map(|s| s.as_str()));
                idx
            }

            other => anyhow::bail!(
                "Unsupported type {:?} in tier-1 adapter instance-type encoding. \
                 If you need support for this type, \
                 please open an issue with a repro at https://github.com/ejrgilbert/splicer/issues",
                other
            ),
        };

        self.cache.insert(id, local_idx);
        Ok(ComponentValType::Type(local_idx))
    }
}

// ─── Component-level type encoder ───────────────────────────────────────────

/// Encodes a value type at the component level, adding any needed compound type definitions
/// to `comp_types` and returning the `ComponentValType` that references them.
///
/// - Primitives → `ComponentValType::Primitive(...)` (no allocation)
/// - Resources / AsyncHandle → looks up the component-level `own<T>` index from `comp_own_by_vid`
/// - Compound types (Result, Variant, Option, Record, etc.) → recursively encodes inner types,
///   adds a new defined-type entry to `comp_types`, returns `ComponentValType::Type(idx)`
///
/// `comp_type_count` tracks the running component-level type index (incremented for each new entry).
/// `comp_cache` prevents redundant encoding of the same `ValueTypeId`.
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_comp_cv(
    id: ValueTypeId,
    arena: &TypeArena,
    comp_types: &mut ComponentTypeSection,
    comp_type_count: &mut u32,
    comp_own_by_vid: &HashMap<ValueTypeId, u32>,
    comp_cache: &mut HashMap<ValueTypeId, u32>,
) -> anyhow::Result<ComponentValType> {
    if let Some(cv) = prim_cv(arena.lookup_val(id)) {
        return Ok(cv);
    }
    if let Some(&idx) = comp_cache.get(&id) {
        return Ok(ComponentValType::Type(idx));
    }

    let vt = arena.lookup_val(id).clone();

    match vt {
        ValueType::Resource(_) | ValueType::AsyncHandle => {
            if let Some(&own_idx) = comp_own_by_vid.get(&id) {
                Ok(ComponentValType::Type(own_idx))
            } else {
                Ok(ComponentValType::Primitive(PrimitiveValType::U32))
            }
        }
        ValueType::Result { ok, err } => {
            let ok_cv = ok
                .map(|id| encode_comp_cv(id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache))
                .transpose()?;
            let err_cv = err
                .map(|id| encode_comp_cv(id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache))
                .transpose()?;
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().result(ok_cv, err_cv);
            comp_cache.insert(id, idx);
            Ok(ComponentValType::Type(idx))
        }
        ValueType::Option(inner_id) => {
            let inner_cv = encode_comp_cv(inner_id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache)?;
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().option(inner_cv);
            comp_cache.insert(id, idx);
            Ok(ComponentValType::Type(idx))
        }
        ValueType::Variant(cases) => {
            let mut encoded: Vec<(String, Option<ComponentValType>)> = Vec::new();
            for (name, opt_id) in &cases {
                let opt_cv = opt_id
                    .map(|id| encode_comp_cv(id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache))
                    .transpose()?;
                encoded.push((name.clone(), opt_cv));
            }
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().variant(encoded.iter().map(|(n, cv)| (n.as_str(), *cv)));
            comp_cache.insert(id, idx);
            Ok(ComponentValType::Type(idx))
        }
        ValueType::Record(fields) => {
            let mut encoded: Vec<(String, ComponentValType)> = Vec::new();
            for (name, fid) in &fields {
                encoded.push((name.clone(), encode_comp_cv(*fid, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache)?));
            }
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().record(encoded.iter().map(|(n, cv)| (n.as_str(), *cv)));
            comp_cache.insert(id, idx);
            Ok(ComponentValType::Type(idx))
        }
        ValueType::Tuple(ids) => {
            let mut encoded: Vec<ComponentValType> = Vec::new();
            for fid in &ids {
                encoded.push(encode_comp_cv(*fid, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache)?);
            }
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().tuple(encoded.into_iter());
            comp_cache.insert(id, idx);
            Ok(ComponentValType::Type(idx))
        }
        ValueType::List(inner_id) => {
            let inner_cv = encode_comp_cv(inner_id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache)?;
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().list(inner_cv);
            comp_cache.insert(id, idx);
            Ok(ComponentValType::Type(idx))
        }
        ValueType::FixedSizeList(inner_id, _n) => {
            let inner_cv = encode_comp_cv(inner_id, arena, comp_types, comp_type_count, comp_own_by_vid, comp_cache)?;
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().list(inner_cv);
            comp_cache.insert(id, idx);
            Ok(ComponentValType::Type(idx))
        }
        ValueType::Enum(tags) => {
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().enum_type(tags.iter().map(|s| s.as_str()));
            comp_cache.insert(id, idx);
            Ok(ComponentValType::Type(idx))
        }
        ValueType::Flags(names) => {
            let idx = *comp_type_count;
            *comp_type_count += 1;
            comp_types.defined_type().flags(names.iter().map(|s| s.as_str()));
            comp_cache.insert(id, idx);
            Ok(ComponentValType::Type(idx))
        }
        other => anyhow::bail!(
            "Unsupported type {:?} in tier-1 adapter component-type encoding. \
             If you need support for this type, \
             please open an issue with a repro at https://github.com/ejrgilbert/splicer/issues",
            other
        ),
    }
}
