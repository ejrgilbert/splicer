//! Wasm-encoder emit helpers shared between tier-1 and tier-2's
//! dispatch core modules. Everything here is canonical-ABI plumbing
//! that wit-component's `ComponentEncoder` requires the core module
//! to provide regardless of which tier of adapter it backs.

use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;

use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection,
    Function, GlobalSection, GlobalType, ImportSection, MemArg, MemorySection, MemoryType, Module,
    ValType,
};
use wit_parser::abi::{AbiVariant, FlatTypes, WasmSignature, WasmType};
use wit_parser::{
    Function as WitFunction, Handle, Int, InterfaceId, Resolve, ResourceIntrinsic, SizeAlign, Type,
    TypeDefKind, TypeId, TypeOwner, WasmImport, WorldId, WorldItem, WorldKey,
};

use super::super::indices::LocalsBuilder;
use super::super::resolve::{hook_callback_mangling, sync_mangling};

// ─── Standard wasm-component-model exports ────────────────────────
//
// wit-component's `ComponentEncoder` requires the core module to
// export memory, cabi_realloc, and `_initialize` under these exact
// names — they're part of the canonical-ABI contract, not anything
// we get to pick.

pub(crate) const EXPORT_MEMORY: &str = "memory";
pub(crate) const EXPORT_CABI_REALLOC: &str = "cabi_realloc";
pub(crate) const EXPORT_INITIALIZE: &str = "_initialize";

/// Wasm linear-memory page size (64 KiB).
pub(crate) const WASM_PAGE_SIZE: u32 = 64 * 1024;

/// Indices of globals emitted by [`emit_memory_and_globals`].
/// Adding a new global = one field here + one line in the emitter.
pub(crate) struct GlobalIndices {
    /// i32 bump pointer consumed by `cabi_realloc`.
    pub bump: u32,
    /// i64 monotonic per-instance counter; bumped once per call to
    /// publish `call-id.id`. u64 won't realistically wrap.
    pub call_id_counter: u32,
}

/// Memory section + globals (bump pointer + call-id counter).
/// `bump_start` is where the bump allocator begins serving; initial
/// memory covers everything below it (one-page floor) so static data
/// segments don't trap, and `cabi_realloc` grows it from there.
pub(crate) fn emit_memory_and_globals(module: &mut Module, bump_start: u32) -> GlobalIndices {
    let pages_for_static = bump_start.div_ceil(WASM_PAGE_SIZE).max(1);
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: pages_for_static as u64,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memory);

    let mut globals = GlobalSection::new();
    let mut next_global: u32 = 0;
    let mut alloc_global = |val_type: ValType, init: ConstExpr| {
        globals.global(
            GlobalType {
                val_type,
                mutable: true,
                shared: false,
            },
            &init,
        );
        let idx = next_global;
        next_global += 1;
        idx
    };
    let bump = alloc_global(ValType::I32, ConstExpr::i32_const(bump_start as i32));
    let call_id_counter = alloc_global(ValType::I64, ConstExpr::i64_const(0));
    module.section(&globals);

    GlobalIndices {
        bump,
        call_id_counter,
    }
}

/// Standard `cabi_realloc(old_ptr, old_size, align, new_size) -> new_ptr`
/// implementation: bump-allocator that ignores `old_*`, aligns the
/// current bump pointer up to `align`, returns the aligned address,
/// and advances the bump global by `new_size`. If the new bump
/// position would exceed the current linear-memory size, the
/// allocator calls `memory.grow` for the shortfall and traps on
/// failure — turning host-side OOM into a wasm trap rather than a
/// silent bump-pointer wrap that would corrupt static data.
///
/// `align` is assumed to be a power of two (canonical-ABI guarantee).
/// Pushes the function into `code`; caller decides where it lands in
/// the function index space.
pub(crate) fn emit_cabi_realloc(code: &mut CodeSection, bump_global: u32) {
    const PARAM_COUNT: u32 = 4;
    const ALIGN_LOCAL: u32 = 2;
    const NEW_SIZE_LOCAL: u32 = 3;
    let mut locals = LocalsBuilder::new(PARAM_COUNT);
    let aligned = locals.alloc_local(ValType::I32);
    let new_bump = locals.alloc_local(ValType::I32);
    let mut f = Function::new_with_locals_types(locals.freeze().locals);

    // aligned = (global.bump + (align - 1)) & ~(align - 1)
    f.instructions().global_get(bump_global);
    f.instructions().local_get(ALIGN_LOCAL);
    f.instructions().i32_const(1);
    f.instructions().i32_sub();
    f.instructions().i32_add();
    f.instructions().local_get(ALIGN_LOCAL);
    f.instructions().i32_const(1);
    f.instructions().i32_sub();
    f.instructions().i32_const(-1);
    f.instructions().i32_xor();
    f.instructions().i32_and();
    f.instructions().local_set(aligned);

    // new_bump = aligned + new_size
    f.instructions().local_get(aligned);
    f.instructions().local_get(NEW_SIZE_LOCAL);
    f.instructions().i32_add();
    f.instructions().local_tee(new_bump);

    // if new_bump > memory.size * page_size: grow memory
    f.instructions().memory_size(0);
    f.instructions()
        .i32_const(WASM_PAGE_SIZE.trailing_zeros() as i32);
    f.instructions().i32_shl();
    f.instructions().i32_gt_u();
    f.instructions().if_(BlockType::Empty);
    {
        // delta_pages = ceil((new_bump - memory.size * page_size) / page_size).
        // The subtraction is positive in this branch, so the
        // `(x - 1) >> log2_page + 1` ceiling is well-defined.
        f.instructions().local_get(new_bump);
        f.instructions().memory_size(0);
        f.instructions()
            .i32_const(WASM_PAGE_SIZE.trailing_zeros() as i32);
        f.instructions().i32_shl();
        f.instructions().i32_sub();
        f.instructions().i32_const(1);
        f.instructions().i32_sub();
        f.instructions()
            .i32_const(WASM_PAGE_SIZE.trailing_zeros() as i32);
        f.instructions().i32_shr_u();
        f.instructions().i32_const(1);
        f.instructions().i32_add();
        f.instructions().memory_grow(0);
        f.instructions().i32_const(-1);
        f.instructions().i32_eq();
        f.instructions().if_(BlockType::Empty);
        // Trap: memory.grow returned -1 (host OOM); without this the
        // bump pointer would advance past valid memory.
        f.instructions().unreachable();
        f.instructions().end();
    }
    f.instructions().end();

    // global.bump = new_bump
    f.instructions().local_get(new_bump);
    f.instructions().global_set(bump_global);

    // return aligned
    f.instructions().local_get(aligned);
    f.instructions().end();
    code.function(&f);
}

/// Bump-pointer save/restore plumbing for one wrapper. Carries the
/// per-build global index and the per-fn local that holds the
/// snapshot — paired so emit helpers can take one ref instead of
/// two `u32`s.
#[derive(Clone, Copy)]
pub(crate) struct BumpReset {
    pub global: u32,
    pub saved_local: u32,
}

/// Snapshot the bump global into [`BumpReset::saved_local`]. Pair
/// with [`emit_bump_restore`] at every exit path so wrapper-body
/// `cabi_realloc` allocations (cells, side-buffers, list-of indices,
/// etc.) free atomically when control returns to the host.
/// Host-driven arg lowering (canon-lower writing string/list params
/// into the wrapper's memory before the wrapper body runs) is *not*
/// covered — those allocations precede the snapshot.
pub(crate) fn emit_bump_save(f: &mut Function, br: BumpReset) {
    f.instructions().global_get(br.global);
    f.instructions().local_set(br.saved_local);
}

/// Restore bump from the snapshot taken by [`emit_bump_save`]. Run
/// at every exit path (natural end, async pre-`task.return`, blocking
/// early `return_()`, etc.) once the last wrapper-body allocation has
/// been consumed.
pub(crate) fn emit_bump_restore(f: &mut Function, br: BumpReset) {
    f.instructions().local_get(br.saved_local);
    f.instructions().global_set(br.global);
}

/// One wrapper-shaped export, used by [`emit_export_section`].
pub(crate) struct WrapperExport<'a> {
    pub export_name: &'a str,
    /// `Some(idx)` iff the wrapper needs a paired `cabi_post_*` shim.
    pub cabi_post_idx: Option<u32>,
}

/// Standard export section: each wrapper at `wrapper_base + i`,
/// optionally with a `cabi_post_*` companion, plus memory,
/// `cabi_realloc`, and `_initialize`.
pub(crate) fn emit_export_section(
    module: &mut Module,
    wrappers: &[WrapperExport<'_>],
    wrapper_base: u32,
    init_idx: u32,
    cabi_realloc_idx: u32,
) {
    let mut exports = ExportSection::new();
    for (i, w) in wrappers.iter().enumerate() {
        exports.export(w.export_name, ExportKind::Func, wrapper_base + i as u32);
        if let Some(post_idx) = w.cabi_post_idx {
            exports.export(
                &format!("cabi_post_{}", w.export_name),
                ExportKind::Func,
                post_idx,
            );
        }
    }
    exports.export(EXPORT_MEMORY, ExportKind::Memory, 0);
    exports.export(EXPORT_CABI_REALLOC, ExportKind::Func, cabi_realloc_idx);
    exports.export(EXPORT_INITIALIZE, ExportKind::Func, init_idx);
    module.section(&exports);
}

/// Active data segments at memory 0. No-op when `segments` is empty.
pub(crate) fn emit_data_section(module: &mut Module, segments: &[(u32, Vec<u8>)]) {
    if segments.is_empty() {
        return;
    }
    let mut data = DataSection::new();
    for (offset, bytes) in segments {
        data.active(
            0,
            &ConstExpr::i32_const(*offset as i32),
            bytes.iter().copied(),
        );
    }
    module.section(&data);
}

/// `() -> ()` — empty body. Used for `_initialize` and per-func
/// `cabi_post_<name>` shims.
pub(crate) fn empty_function() -> Function {
    let mut f = Function::new_with_locals_types([]);
    f.instructions().end();
    f
}

/// wit-parser [`WasmType`]s → wasm-encoder [`ValType`]s. The
/// canonical-ABI flat-type representation of `Pointer` / `Length` is
/// always 32-bit on wasm32; `PointerOrI64` widens to i64.
pub(crate) fn val_types(types: &[WasmType]) -> Vec<ValType> {
    types.iter().copied().map(wasm_type_to_val).collect()
}

pub(crate) fn wasm_type_to_val(wt: WasmType) -> ValType {
    match wt {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => ValType::I32,
        WasmType::I64 | WasmType::PointerOrI64 => ValType::I64,
        WasmType::F32 => ValType::F32,
        WasmType::F64 => ValType::F64,
    }
}

// ─── Wrapper-body passthrough helpers ─────────────────────────────
// Bridge between callee-returns export sigs (`[] -> [I32]`) and
// caller-allocates import sigs (`[I32] -> []`) for sync compound
// returns. Shared across tiers.

/// `ValType` of a single-value direct (non-retptr) return, or `None`
/// for void / retptr-bound results.
pub(crate) fn direct_return_type(export_sig: &WasmSignature) -> Option<ValType> {
    if !export_sig.retptr && export_sig.results.len() == 1 {
        Some(wasm_type_to_val(export_sig.results[0]))
    } else {
        None
    }
}

/// Push the wrapper's `nparams` flat params (and an extra static
/// `retptr_offset` if the import wants caller-allocates), then call.
pub(crate) fn emit_handler_call(
    f: &mut Function,
    nparams: u32,
    import_retptr: bool,
    retptr_offset: Option<i32>,
    handler_idx: u32,
) {
    for p in 0..nparams {
        f.instructions().local_get(p);
    }
    if import_retptr {
        let off = retptr_offset.expect("import_retptr → retptr_offset must be Some");
        f.instructions().i32_const(off);
    }
    f.instructions().call(handler_idx);
}

/// Bail when an indirect-params async fn carries a param shape the
/// lower-mode bindgen doesn't handle. Today the bindgen covers the
/// full canonical-ABI value-type space; this gate exists so a future
/// spec addition (or a `TypeDefKind::Resource` slipping through as a
/// bare param, which canon-ABI forbids) fails loud rather than
/// emitting broken wasm.
pub(crate) fn require_indirect_params_supported_shape(
    resolve: &Resolve,
    fn_name: &str,
    func: &WitFunction,
) -> Result<()> {
    for param in &func.params {
        if !is_supported_indirect_params_ty(resolve, &param.ty) {
            bail!(
                "async function `{fn_name}` has params that overflow \
                 MAX_FLAT_ASYNC_PARAMS ({}) AND param `{}` carries an \
                 unsupported type.",
                Resolve::MAX_FLAT_ASYNC_PARAMS,
                param.name,
            );
        }
    }
    Ok(())
}

/// Recursively true iff `ty` is a shape the lower-mode bindgen
/// covers. Aggregates check each contained type.
fn is_supported_indirect_params_ty(resolve: &Resolve, ty: &Type) -> bool {
    match ty {
        Type::Bool
        | Type::S8
        | Type::U8
        | Type::S16
        | Type::U16
        | Type::S32
        | Type::U32
        | Type::S64
        | Type::U64
        | Type::F32
        | Type::F64
        | Type::Char
        | Type::String
        | Type::ErrorContext => true,
        Type::Id(id) => match &resolve.types[*id].kind {
            TypeDefKind::Type(inner) => is_supported_indirect_params_ty(resolve, inner),
            TypeDefKind::Record(r) => r
                .fields
                .iter()
                .all(|f| is_supported_indirect_params_ty(resolve, &f.ty)),
            TypeDefKind::Tuple(t) => t
                .types
                .iter()
                .all(|t| is_supported_indirect_params_ty(resolve, t)),
            TypeDefKind::Variant(v) => v.cases.iter().all(|c| match &c.ty {
                Some(t) => is_supported_indirect_params_ty(resolve, t),
                None => true,
            }),
            TypeDefKind::Option(t) => is_supported_indirect_params_ty(resolve, t),
            TypeDefKind::Result(r) => {
                r.ok.as_ref()
                    .is_none_or(|t| is_supported_indirect_params_ty(resolve, t))
                    && r.err
                        .as_ref()
                        .is_none_or(|t| is_supported_indirect_params_ty(resolve, t))
            }
            TypeDefKind::List(t) | TypeDefKind::FixedLengthList(t, _) => {
                is_supported_indirect_params_ty(resolve, t)
            }
            TypeDefKind::Map(k, v) => {
                is_supported_indirect_params_ty(resolve, k)
                    && is_supported_indirect_params_ty(resolve, v)
            }
            TypeDefKind::Enum(_)
            | TypeDefKind::Flags(_)
            | TypeDefKind::Handle(_)
            | TypeDefKind::Future(_)
            | TypeDefKind::Stream(_) => true,
            // Resources only show up as Handle, never as bare params.
            TypeDefKind::Resource | TypeDefKind::Unknown => false,
        },
    }
}

/// Build the wasm sequence that lowers the wrapper's flat function
/// params into a memory-resident canonical-ABI params record at
/// `params_record_base`, ready to be passed by pointer to an
/// `indirect_params = true` import. Used by async target functions
/// whose flat params overflow `Resolve::MAX_FLAT_ASYNC_PARAMS`.
///
/// Pre-built (returns a `Vec<Instruction>` rather than emitting into
/// a `Function`) so callers can allocate all of their locals — incl.
/// the bindgen's address + tmp scratch — before freezing the
/// [`LocalsBuilder`]. Caller plays the sequence back into the wrapper
/// body at the appropriate point.
///
/// The wrapper's wasm function takes the flat params as locals
/// `0 .. nparams_flat`. For each WIT param, the sequence:
///
/// 1. Stages `params_record_base + field_off` into a per-call addr
///    local via `i32.const` + `local.set` — the bindgen reads this
///    on every store emit.
/// 2. Drives `wit_bindgen_core::abi::lower_to_memory` through a
///    [`WasmEncoderBindgen`] in lower-mode whose `param_flat_locals`
///    is sliced to this param's flat wasm-locals.
///
/// One bindgen across all params: its `param_flat_locals` is the
/// concatenation of every param's flat slots in canonical order, and
/// its `flat_cursor` advances naturally. The shared
/// `store_tmp_by_valtype` cache reuses scratch locals across params
/// (≤4 total: one each of i32/i64/f32/f64) — instead of allocating a
/// fresh set per param.
///
/// **Realloc handling:** wit-bindgen-core 0.57.1's `lower_to_memory`
/// hardcodes `Realloc::Export("cabi_realloc")`. Our `StringLower` /
/// `ListCanonLower` emit arms intentionally ignore that flag and
/// emit pass-through `local.get`s — the host's canon-lower already
/// deposited the payload into our linear memory, and the receiving
/// import does its own canon-lift on its end of the wire.
pub(crate) fn build_lower_params_to_memory(
    resolve: &Resolve,
    sizes: &SizeAlign,
    indices: &mut super::super::indices::LocalsBuilder,
    func: &WitFunction,
    params_record_base: i32,
) -> Vec<wasm_encoder::Instruction<'static>> {
    use super::compat::flat_types;
    use wit_bindgen_core::abi::lower_to_memory;

    let param_types: Vec<Type> = func.params.iter().map(|p| p.ty).collect();
    let field_offsets = sizes.field_offsets(&param_types);

    // Concatenate every param's flat wasm-locals in canonical order.
    // `flat_types(...)` panic only fires if `is_primitive_param_ty` is
    // widened without re-bounding flat width; primitives flatten to
    // one slot each, far below `MAX_FLAT_PARAMS`.
    let total_flat_count: u32 = param_types
        .iter()
        .map(|ty| {
            flat_types(resolve, ty, None)
                .unwrap_or_else(|| panic!("param flat width exceeds MAX_FLAT_PARAMS"))
                .len() as u32
        })
        .sum();
    let all_flat_locals: Vec<u32> = (0..total_flat_count).collect();

    let addr_local = indices.alloc_local(wasm_encoder::ValType::I32);
    let mut bg = super::WasmEncoderBindgen::new(sizes, addr_local, indices)
        .with_param_flat_locals(all_flat_locals);

    for ((field_off, _field_size), ty) in field_offsets.iter().zip(&param_types) {
        // Both addends are bounded by each tier's LAYOUT_SIZE_BUDGET
        // (i32::MAX) — params_record_base sits inside the layout and
        // field_off is ≤ record_size ≤ layout end, so the sum stays
        // ≤ i32::MAX.
        let effective_addr = params_record_base + field_off.size_wasm32() as i32;
        bg.emit_set_addr_const(effective_addr);
        lower_to_memory(resolve, &mut bg, (), (), ty);
    }
    bg.into_instructions()
}

/// Push either the direct-return local, or the static retptr (when
/// the export sig is callee-returns). No-op for void.
pub(crate) fn emit_wrapper_return(
    f: &mut Function,
    result_local: Option<u32>,
    export_retptr: bool,
    retptr_offset: Option<i32>,
) {
    if let Some(local) = result_local {
        f.instructions().local_get(local);
    } else if export_retptr {
        let off = retptr_offset.expect("export_retptr → retptr_offset must be Some");
        f.instructions().i32_const(off);
    }
}

// ─── Schema-driven layout helpers ─────────────────────────────────
// Tier-2 codegen builds static data segments whose shapes mirror
// canonical-ABI lowerings of WIT records (e.g. `field`, the on-call
// indirect-params record). Rather than hand-roll the byte offsets,
// these helpers ask `wit-parser::SizeAlign` to compute them — so a
// later change to `wit/common/world.wit` flows through without code
// edits.

/// Size + alignment + per-field byte offset (keyed by WIT field
/// name) of a record-shaped canonical-ABI value, on wasm32.
pub(crate) struct RecordLayout {
    pub size: u32,
    pub align: u32,
    pub field_offsets: std::collections::HashMap<String, u32>,
}

impl RecordLayout {
    /// Layout for an arbitrary list of named typed fields. Used for
    /// function-param records (e.g. on-call's
    /// `record { call: call-id, args: list<field> }`).
    pub(crate) fn for_named_fields(sizes: &SizeAlign, fields: &[(String, Type)]) -> Self {
        let types: Vec<Type> = fields.iter().map(|(_, t)| *t).collect();
        let info = sizes.record(&types);
        let offs = sizes.field_offsets(&types);
        Self {
            size: info.size.size_wasm32() as u32,
            align: info.align.align_wasm32() as u32,
            field_offsets: fields
                .iter()
                .zip(offs)
                .map(|((name, _), (off, _))| (name.clone(), off.size_wasm32() as u32))
                .collect(),
        }
    }

    /// Layout for a `record { … }` typedef (e.g. `field`,
    /// `field-tree`). Panics if `id` doesn't refer to a record.
    pub(crate) fn for_record_typedef(sizes: &SizeAlign, resolve: &Resolve, id: TypeId) -> Self {
        let typedef = &resolve.types[id];
        let TypeDefKind::Record(r) = &typedef.kind else {
            panic!(
                "RecordLayout::for_record_typedef called with non-record typedef `{:?}`",
                typedef.name
            );
        };
        let fields: Vec<(String, Type)> = r.fields.iter().map(|f| (f.name.clone(), f.ty)).collect();
        Self::for_named_fields(sizes, &fields)
    }

    /// Byte offset of the named field. Panics with a descriptive
    /// message if the field doesn't exist — i.e. the WIT was renamed
    /// without updating codegen.
    pub(crate) fn offset_of(&self, name: &str) -> u32 {
        *self.field_offsets.get(name).unwrap_or_else(|| {
            let mut keys: Vec<&str> = self.field_offsets.keys().map(|s| s.as_str()).collect();
            keys.sort();
            panic!("RecordLayout: no field named `{name}` (record has: {keys:?})")
        })
    }
}

/// One imported hook function — module + name + signature, plus
/// the WIT params list (used by tiers that derive a hook-params
/// `RecordLayout` for indirect-params lowering).
pub(crate) struct HookImport {
    pub module: String,
    pub name: String,
    pub sig: WasmSignature,
    pub params: Vec<(String, Type)>,
}

/// Look up `target_iface` in `world_id`'s imports and return its
/// (single) function as a [`HookImport`]. `None` if the world doesn't
/// import that interface.
pub(crate) fn find_imported_hook(
    resolve: &Resolve,
    world_id: WorldId,
    target_iface: &str,
) -> Option<HookImport> {
    let world = &resolve.worlds[world_id];
    world.imports.iter().find_map(|(key, item)| {
        let WorldItem::Interface { id, .. } = item else {
            return None;
        };
        if resolve.id_of(*id).as_deref() != Some(target_iface) {
            return None;
        }
        let func = resolve.interfaces[*id].functions.values().next()?;
        let (module, name) = resolve.wasm_import_name(
            hook_callback_mangling(),
            WasmImport::Func {
                interface: Some(key),
                func,
            },
        );
        Some(HookImport {
            module,
            name,
            sig: resolve.wasm_signature(AbiVariant::GuestImportAsync, func),
            params: func.params.iter().map(|p| (p.name.clone(), p.ty)).collect(),
        })
    })
}

/// Synthesize the WIT for a tier's adapter world: import + export the
/// target interface by name, plus one import per active hook
/// interface (already-versioned, e.g. `"splicer:tier1/before@0.2.0"`).
pub(crate) fn synthesize_adapter_world_wit(
    package_name: &str,
    world_name: &str,
    target_interface: &str,
    hook_iface_imports: &[String],
) -> String {
    let mut wit = format!("package {package_name};\n\nworld {world_name} {{\n");
    wit.push_str(&format!("    import {target_interface};\n"));
    wit.push_str(&format!("    export {target_interface};\n"));
    for iface in hook_iface_imports {
        wit.push_str(&format!("    import {iface};\n"));
    }
    wit.push_str("}\n");
    wit
}

/// Look up a typedef in `splicer:common/types` (e.g. `"call-id"`,
/// `"field"`). The tier WITs travel with the common WIT in this repo,
/// so any loaded version is canonical — version is ignored.
pub(crate) fn find_common_typeid(resolve: &Resolve, type_name: &str) -> Result<TypeId> {
    for (id, _) in resolve.interfaces.iter() {
        let Some(qname) = resolve.id_of(id) else {
            continue;
        };
        let unversioned = qname.split('@').next().unwrap_or(&qname);
        if unversioned == "splicer:common/types" {
            return resolve.interfaces[id]
                .types
                .get(type_name)
                .copied()
                .ok_or_else(|| anyhow!("`splicer:common/types` is missing typedef `{type_name}`"));
        }
    }
    bail!("resolve has no `splicer:common/types` interface — was the common WIT loaded?")
}

/// [`CallIdLayout`] for `splicer:common/types.call-id` — used by both
/// tiers to lay out the canonical-ABI lowering of the hook params'
/// call-id portion.
pub(crate) fn call_id_layout(resolve: &Resolve, sizes: &SizeAlign) -> Result<CallIdLayout> {
    let id = find_common_typeid(resolve, "call-id")?;
    Ok(CallIdLayout(RecordLayout::for_record_typedef(
        sizes, resolve, id,
    )))
}

/// Byte offset of an `option<T>`'s payload area (i.e. the byte right
/// after the 1-byte disc, padded up to `align(T)`).
pub(crate) fn option_payload_offset(sizes: &SizeAlign, payload_ty: &Type) -> u32 {
    sizes
        .payload_offset(Int::U8, [Some(payload_ty)])
        .size_wasm32() as u32
}

/// Canonical-ABI byte offsets of `string`/`list<T>`'s flat (ptr, len)
/// pair. Both are i32-aligned — true regardless of which `T` the
/// list carries — so these are kept as named constants instead of
/// being looked up via `SizeAlign`.
pub(crate) const SLICE_PTR_OFFSET: u32 = 0;
pub(crate) const SLICE_LEN_OFFSET: u32 = 4;
/// Total size of the flat `(ptr, len)` pair — the canonical-ABI
/// `string` lowering, also the per-element stride of any `list<string>`.
pub(crate) const STRING_FLAT_BYTES: u32 = 8;

/// Maximum bytes a single Unicode scalar takes in UTF-8 — derived
/// from `char::MAX.len_utf8()` so the source of truth is the standard
/// library's char definition (which matches the Unicode standard the
/// canonical-ABI's `char` references).
pub(crate) const MAX_UTF8_LEN: u32 = char::MAX.len_utf8() as u32;

/// Canonical-ABI discriminant values for `option<T>`. Fixed by the
/// spec — wit-parser models `option<T>` as its own `TypeDefKind::Option(T)`
/// (not a `Variant`), so there's no per-case data on a `Resolve` to
/// derive these from.
pub(crate) const OPTION_NONE: u8 = 0;
pub(crate) const OPTION_SOME: u8 = 1;

/// Log2 alignment values for wasm `i32.store` / `i32.store8` /
/// `i64.store`. `MemArg::align` is in log2 form; these are wasm-
/// format constants, not Resolve-derivable.
pub(crate) const I32_STORE_LOG2_ALIGN: u32 = 2;
pub(crate) const I8_STORE_LOG2_ALIGN: u32 = 0;
pub(crate) const I64_STORE_LOG2_ALIGN: u32 = 3;

// `splicer:common/types.call-id` field names — encapsulated by
// [`CallIdLayout`]'s typed accessors so call sites can't fat-finger
// the keys. Must match `wit/common/world.wit`.
const CALLID_IFACE: &str = "interface-name";
const CALLID_FN: &str = "function-name";
const CALLID_ID: &str = "id";

/// Typed accessor over a `splicer:common/types.call-id` record's
/// canonical-ABI layout. Wraps a [`RecordLayout`]; exposes one method
/// per WIT field so a typo at the call site is a compile error instead
/// of a runtime `RecordLayout::offset_of` panic.
pub(crate) struct CallIdLayout(RecordLayout);

impl CallIdLayout {
    pub(crate) fn size(&self) -> u32 {
        self.0.size
    }
    pub(crate) fn align(&self) -> u32 {
        self.0.align
    }
    pub(crate) fn iface_off(&self) -> u32 {
        self.0.offset_of(CALLID_IFACE)
    }
    pub(crate) fn fn_off(&self) -> u32 {
        self.0.offset_of(CALLID_FN)
    }
    pub(crate) fn id_off(&self) -> u32 {
        self.0.offset_of(CALLID_ID)
    }

    /// Build-time twin of [`emit_populate_call_id`] for the name
    /// fields: store `iface_name` and `fn_name` into a call-id
    /// sub-record anchored at `base` in `blob`. The id field is left
    /// untouched — it gets written at runtime by the wasm sequence
    /// emitted by [`emit_populate_call_id`].
    pub(crate) fn store_names_in_blob(
        &self,
        blob: &mut [u8],
        base: usize,
        iface_name: BlobSlice,
        fn_name: BlobSlice,
    ) {
        store_slice_in_blob(blob, base + self.iface_off() as usize, iface_name);
        store_slice_in_blob(blob, base + self.fn_off() as usize, fn_name);
    }
}

/// Build-time twin of [`emit_store_slice`]: store a `(ptr, len)`
/// canonical-ABI slice pair into a byte buffer at `off`.
pub(crate) fn store_slice_in_blob(blob: &mut [u8], off: usize, slice: BlobSlice) {
    blob[off + SLICE_PTR_OFFSET as usize..][..4].copy_from_slice(&(slice.off as i32).to_le_bytes());
    blob[off + SLICE_LEN_OFFSET as usize..][..4].copy_from_slice(&(slice.len as i32).to_le_bytes());
}

/// Typed `(off, len)` pair into a tier's static blob. Avoids
/// accidental ptr/len swaps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BlobSlice {
    pub off: u32,
    pub len: u32,
}

impl BlobSlice {
    pub(crate) const EMPTY: BlobSlice = BlobSlice { off: 0, len: 0 };
}

/// Call `cabi_realloc(0, 0, align, size)` and store the returned
/// pointer in `dest_local`. Convenience wrapper for the per-call
/// fresh-allocation pattern (cells slab, list-of indices buffer, …).
pub(crate) fn emit_cabi_realloc_call(
    f: &mut Function,
    cabi_realloc_idx: u32,
    align: u32,
    size: u32,
    dest_local: u32,
) {
    debug_assert!(
        size <= i32::MAX as u32,
        "cabi_realloc size {size} doesn't fit in signed i32",
    );
    f.instructions().i32_const(0);
    f.instructions().i32_const(0);
    f.instructions().i32_const(align as i32);
    f.instructions().i32_const(size as i32);
    f.instructions().call(cabi_realloc_idx);
    f.instructions().local_set(dest_local);
}

/// Trap if `next_cell_idx_local + len_local * elem_count` would
/// exceed `i32::MAX / cell_size`. Guards the caller's subsequent i32
/// mul + add against silent mod-2^32 wrap that would slip past
/// [`emit_cabi_realloc_call_runtime`]'s own size check and
/// under-allocate the cells slab.
pub(crate) fn emit_trap_if_list_overflows_cell_slab(
    f: &mut Function,
    len_local: u32,
    elem_count: u32,
    next_cell_idx_local: u32,
    cell_size: u32,
) {
    assert!(elem_count > 0, "element plan must contribute ≥1 cell");
    assert!(cell_size > 0, "cell_size must be positive");
    let cell_limit = (i32::MAX as u32) / cell_size;
    f.instructions().local_get(len_local);
    f.instructions().i32_const(cell_limit as i32);
    f.instructions().local_get(next_cell_idx_local);
    f.instructions().i32_sub();
    if elem_count != 1 {
        f.instructions().i32_const(elem_count as i32);
        f.instructions().i32_div_u();
    }
    f.instructions().i32_gt_u();
    f.instructions().if_(BlockType::Empty);
    // Trap: appending this list's cells would exceed the i32 byte
    // budget for the cells slab; letting it through would silently
    // wrap the subsequent i32 mul + add and under-allocate.
    f.instructions().unreachable();
    f.instructions().end();
}

/// Runtime-sized `cabi_realloc(0, 0, align, count_local * elem_bytes)`
/// → `dest_local`. Pass `elem_bytes = 1` for byte-counted calls. Traps
/// on overflow — cabi_realloc takes size as i32, so a wrapped value
/// would silently under-allocate. The trap-vs-clip trade-off is
/// documented in `docs/tiers/tier-2.md` ("Oversized lists trap").
pub(crate) fn emit_cabi_realloc_call_runtime(
    f: &mut Function,
    cabi_realloc_idx: u32,
    align: u32,
    count_local: u32,
    elem_bytes: u32,
    dest_local: u32,
) {
    assert!(elem_bytes > 0, "elem_bytes must be positive");
    let max_count = (i32::MAX as u32) / elem_bytes;
    f.instructions().local_get(count_local);
    f.instructions().i32_const(max_count as i32);
    f.instructions().i32_gt_u();
    f.instructions().if_(BlockType::Empty);
    // Trap: count * elem_bytes would overflow i32, which cabi_realloc
    // takes as a signed size — a wrapped value silently under-allocates.
    f.instructions().unreachable();
    f.instructions().end();

    f.instructions().i32_const(0);
    f.instructions().i32_const(0);
    f.instructions().i32_const(align as i32);
    f.instructions().local_get(count_local);
    if elem_bytes != 1 {
        f.instructions().i32_const(elem_bytes as i32);
        f.instructions().i32_mul();
    }
    f.instructions().call(cabi_realloc_idx);
    f.instructions().local_set(dest_local);
}

/// Patch a slice's `ptr` field from a runtime wasm local. The slice's
/// `len` is left untouched — caller statically wrote it (at build
/// time, via [`emit_store_slice`] or similar) or patches it
/// separately when runtime-determined.
pub(crate) fn emit_store_slice_ptr_runtime(
    f: &mut Function,
    base_ptr: i32,
    field_off: u32,
    ptr_local: u32,
) {
    f.instructions().i32_const(base_ptr);
    f.instructions().local_get(ptr_local);
    f.instructions().i32_store(MemArg {
        offset: (field_off + SLICE_PTR_OFFSET) as u64,
        align: I32_STORE_LOG2_ALIGN,
        memory_index: 0,
    });
}

/// Patch a slice's `len` field from a runtime wasm local; pair
/// with [`emit_store_slice_ptr_runtime`] when both fields are
/// runtime-computed.
pub(crate) fn emit_store_slice_len_runtime(
    f: &mut Function,
    base_ptr: i32,
    field_off: u32,
    len_local: u32,
) {
    f.instructions().i32_const(base_ptr);
    f.instructions().local_get(len_local);
    f.instructions().i32_store(MemArg {
        offset: (field_off + SLICE_LEN_OFFSET) as u64,
        align: I32_STORE_LOG2_ALIGN,
        memory_index: 0,
    });
}

/// Store `slice.off` then `slice.len` as the canonical-ABI `(ptr, len)`
/// pair at `base_ptr + field_off`.
pub(crate) fn emit_store_slice(f: &mut Function, base_ptr: i32, field_off: u32, slice: BlobSlice) {
    let store = |f: &mut Function, sub_off: u32, value: i32| {
        f.instructions().i32_const(base_ptr);
        f.instructions().i32_const(value);
        f.instructions().i32_store(MemArg {
            offset: (field_off + sub_off) as u64,
            align: I32_STORE_LOG2_ALIGN,
            memory_index: 0,
        });
    };
    store(f, SLICE_PTR_OFFSET, slice.off as i32);
    store(f, SLICE_LEN_OFFSET, slice.len as i32);
}

/// Store the i64 in `local` at `base_ptr + field_off` (8-byte align).
pub(crate) fn emit_store_i64_local(f: &mut Function, base_ptr: i32, field_off: u32, local: u32) {
    f.instructions().i32_const(base_ptr);
    f.instructions().local_get(local);
    f.instructions().i64_store(MemArg {
        offset: field_off as u64,
        align: I64_STORE_LOG2_ALIGN,
        memory_index: 0,
    });
}

/// Bump the counter global and tee the new value into `id_local`.
/// First id is `1`.
pub(crate) fn emit_alloc_call_id(f: &mut Function, counter_global: u32, id_local: u32) {
    f.instructions().global_get(counter_global);
    f.instructions().i64_const(1);
    f.instructions().i64_add();
    f.instructions().local_tee(id_local);
    f.instructions().global_set(counter_global);
}

/// Bail if `target_iface` declares a resource inline (the resource's
/// `owner` is the same interface). Splicer's wrapper pattern routes
/// the target interface through an adapter component;
/// `wit_component::ComponentEncoder` synthesizes a fresh resource
/// type for the export side, diverging from the import side's
/// identity. The runtime then rejects handles crossing the boundary.
/// Both tier-1 and tier-2 wrappers hit this, so the check is shared.
pub(crate) fn require_no_inline_resources(
    resolve: &Resolve,
    target_iface: InterfaceId,
) -> Result<()> {
    let iface = &resolve.interfaces[target_iface];
    for (ty_name, &tid) in &iface.types {
        let td = &resolve.types[tid];
        if matches!(td.kind, TypeDefKind::Resource)
            && matches!(td.owner, TypeOwner::Interface(owner) if owner == target_iface)
        {
            let iface_name = resolve
                .id_of(target_iface)
                .unwrap_or_else(|| iface.name.clone().unwrap_or_default());
            bail!(
                "interface `{iface_name}` declares resource `{ty_name}` inline. \
                 Splicer's wrapper-component pattern can't preserve resource \
                 type identity for inline resources — runtime handle traffic \
                 between the import side and export side will be rejected. \
                 Move `{ty_name}` into a sibling `types` interface and \
                 reference it via `use types.{{{ty_name}}}` (the wasi-style \
                 factored-types pattern)."
            );
        }
    }
    Ok(())
}

/// Top-level `borrow<R>` params of `func`, returned as
/// `(flat_idx, resource_id)`. The wrapper must call
/// `[resource-drop]<R>` for each before returning — the canonical-
/// ABI runtime checks every borrow lifted on entry is dropped on
/// exit. Top-level only; borrows nested inside compound params
/// aren't dropped (out of scope until the fuzzer surfaces them).
pub(crate) fn collect_borrow_drops(resolve: &Resolve, func: &WitFunction) -> Vec<(u32, TypeId)> {
    let mut out = Vec::new();
    let mut flat_idx: u32 = 0;
    for param in &func.params {
        if let Type::Id(tid) = param.ty {
            if let TypeDefKind::Handle(Handle::Borrow(rid)) = &resolve.types[tid].kind {
                out.push((flat_idx, resolve_type_alias(resolve, *rid)));
                flat_idx += 1;
                continue;
            }
        }
        let mut storage = vec![WasmType::I32; 32];
        let mut flat = FlatTypes::new(storage.as_mut_slice());
        if !resolve.push_flat(&param.ty, &mut flat) {
            return Vec::new();
        }
        flat_idx += flat.to_vec().len() as u32;
    }
    out
}

/// Follow `TypeDefKind::Type` aliases to the underlying definition
/// (e.g. an `api`-side `use types.{cat}` alias → the `types`-side
/// `resource cat` definition).
pub(crate) fn resolve_type_alias(resolve: &Resolve, mut tid: TypeId) -> TypeId {
    while let TypeDefKind::Type(Type::Id(next)) = &resolve.types[tid].kind {
        tid = *next;
    }
    tid
}

/// Emit one `[resource-drop]<R>` import per unique borrow resource
/// across `per_func`. `drops_of` projects each fn's borrow_drops
/// slice (per-tier `FuncDispatch` is not a shared type, so the
/// projection is the only kind-specific bit). Each resource is
/// imported from its owning interface (factored-types pattern).
/// `alloc_func` allocates each new func-index. `drop_ty` is `Some`
/// iff the caller pre-allocated a `(func (param i32))` type; `None`
/// short-circuits.
///
/// Panics if a resource's owner isn't an interface — WIT resources
/// always live in interfaces, so this would mean the dispatch
/// pipeline produced a borrow whose drop can't be looked up later
/// in [`emit_borrow_drops`]. Fail loudly here rather than at the
/// drop-emit's unconditional HashMap lookup.
pub(crate) fn emit_resource_drop_imports<Fd>(
    imports: &mut ImportSection,
    resolve: &Resolve,
    per_func: &[Fd],
    drops_of: impl Fn(&Fd) -> &[(u32, TypeId)],
    drop_ty: Option<u32>,
    mut alloc_func: impl FnMut() -> u32,
) -> HashMap<TypeId, u32> {
    let Some(drop_ty) = drop_ty else {
        return HashMap::new();
    };
    let mut unique: Vec<TypeId> = per_func
        .iter()
        .flat_map(|fd| drops_of(fd).iter().map(|(_, rid)| *rid))
        .collect();
    unique.sort();
    unique.dedup();
    let mut out: HashMap<TypeId, u32> = HashMap::new();
    for rid in unique {
        let owner_iface = match resolve.types[rid].owner {
            TypeOwner::Interface(iid) => iid,
            other => panic!(
                "borrow resource {rid:?} has owner {other:?}, expected TypeOwner::Interface — \
                 emit_borrow_drops's HashMap lookup would panic with no entry",
            ),
        };
        let owner_key = WorldKey::Interface(owner_iface);
        let imp = WasmImport::ResourceIntrinsic {
            interface: Some(&owner_key),
            resource: rid,
            intrinsic: ResourceIntrinsic::ImportedDrop,
        };
        let (module_name, field_name) = resolve.wasm_import_name(sync_mangling(), imp);
        imports.import(&module_name, &field_name, EntityType::Function(drop_ty));
        out.insert(rid, alloc_func());
    }
    out
}

/// Emit `[resource-drop]<R>` calls for every borrow this fn lifted
/// on entry. The canon-ABI runtime requires these dropped before the
/// wrapper returns, otherwise `borrow handles still remain at the
/// end of the call`. Call before any return-flavored emit.
pub(crate) fn emit_borrow_drops(
    f: &mut Function,
    borrow_drops: &[(u32, TypeId)],
    resource_drop: &HashMap<TypeId, u32>,
) {
    for (flat_idx, rid) in borrow_drops {
        let drop_fn = resource_drop[rid];
        f.instructions().local_get(*flat_idx);
        f.instructions().call(drop_fn);
    }
}

/// Lower a `call-id` record into memory at `base_ptr + call_off`.
/// Names are static blob slices; id comes from `id_local`.
pub(crate) fn emit_populate_call_id(
    f: &mut Function,
    base_ptr: i32,
    call_off: u32,
    callid_layout: &CallIdLayout,
    iface_name: BlobSlice,
    fn_name: BlobSlice,
    id_local: u32,
) {
    let iface_off = call_off + callid_layout.iface_off();
    let fn_off = call_off + callid_layout.fn_off();
    let id_off = call_off + callid_layout.id_off();
    emit_store_slice(f, base_ptr, iface_off, iface_name);
    emit_store_slice(f, base_ptr, fn_off, fn_name);
    emit_store_i64_local(f, base_ptr, id_off, id_local);
}

#[cfg(test)]
mod tests {
    use super::super::super::indices::LocalsBuilder;
    use super::*;
    use wasm_encoder::Instruction;
    use wasm_encoder::{CodeSection, EntityType, FunctionSection, ImportSection, TypeSection};

    /// Build a one-fn module wrapping `emit_cabi_realloc_call_runtime`,
    /// validate, and count `unreachable` ops in the body.
    fn unreachable_count_for(elem_bytes: u32) -> usize {
        let mut module = Module::new();
        let mut types = TypeSection::new();
        types.ty().function(
            [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            [ValType::I32],
        );
        types.ty().function([ValType::I32], []);
        module.section(&types);

        let mut imports = ImportSection::new();
        imports.import("env", "cabi_realloc", EntityType::Function(0));
        module.section(&imports);

        let mut funcs = FunctionSection::new();
        funcs.function(1);
        module.section(&funcs);

        let mut code = CodeSection::new();
        let mut f = Function::new([(1, ValType::I32)]);
        emit_cabi_realloc_call_runtime(&mut f, 0, 4, 0, elem_bytes, 1);
        f.instructions().end();
        code.function(&f);
        module.section(&code);

        let bytes = module.finish();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("emit_cabi_realloc_call_runtime output must validate");

        let parser = wasmparser::Parser::new(0);
        for payload in parser.parse_all(&bytes) {
            if let Ok(wasmparser::Payload::CodeSectionEntry(body)) = payload {
                return body
                    .get_operators_reader()
                    .expect("ops reader")
                    .into_iter()
                    .filter(|op| matches!(op, Ok(wasmparser::Operator::Unreachable)))
                    .count();
            }
        }
        panic!("no CodeSectionEntry in module");
    }

    #[test]
    fn cabi_realloc_runtime_emits_overflow_trap() {
        for elem_bytes in [1u32, 4, 16] {
            let count = unreachable_count_for(elem_bytes);
            assert_eq!(
                count, 1,
                "elem_bytes={elem_bytes}: expected 1 `unreachable`, found {count}",
            );
        }
    }

    /// Build a one-fn module wrapping
    /// `emit_trap_if_list_overflows_cell_slab`, validate, and count
    /// `unreachable` ops in the body.
    fn list_overflow_unreachable_count(elem_count: u32, cell_size: u32) -> usize {
        let mut module = Module::new();
        let mut types = TypeSection::new();
        // (len: i32, next_cell_idx: i32) -> ()
        types.ty().function([ValType::I32, ValType::I32], []);
        module.section(&types);

        let mut funcs = FunctionSection::new();
        funcs.function(0);
        module.section(&funcs);

        let mut code = CodeSection::new();
        let mut f = Function::new([]);
        emit_trap_if_list_overflows_cell_slab(&mut f, 0, elem_count, 1, cell_size);
        f.instructions().end();
        code.function(&f);
        module.section(&code);

        let bytes = module.finish();
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("emit_trap_if_list_overflows_cell_slab output must validate");

        let parser = wasmparser::Parser::new(0);
        for payload in parser.parse_all(&bytes) {
            if let Ok(wasmparser::Payload::CodeSectionEntry(body)) = payload {
                return body
                    .get_operators_reader()
                    .expect("ops reader")
                    .into_iter()
                    .filter(|op| matches!(op, Ok(wasmparser::Operator::Unreachable)))
                    .count();
            }
        }
        panic!("no CodeSectionEntry in module");
    }

    #[test]
    fn list_overflow_trap_emits_unreachable_for_every_shape() {
        // Each (elem_count, cell_size) combination must validate as
        // wasm and emit exactly one `unreachable` for the trap branch.
        // elem_count=1 skips the inner `i32.div_u`; elem_count>1 keeps
        // it. cell_size 8/16 covers the realistic range for `cell`.
        for &elem_count in &[1u32, 2, 16] {
            for &cell_size in &[8u32, 16] {
                let count = list_overflow_unreachable_count(elem_count, cell_size);
                assert_eq!(
                    count, 1,
                    "elem_count={elem_count} cell_size={cell_size}: expected 1 `unreachable`, found {count}",
                );
            }
        }
    }

    /// One slot of `build_lower_params_to_memory` output: the canonical
    /// store width the test expects to see for a single param.
    /// Stripping `MemArg` keeps the matcher offset-agnostic; the
    /// per-block assertions below pin the offset separately.
    #[derive(Debug, Eq, PartialEq)]
    enum StoreSig {
        I32,
        I64,
        F32,
        F64,
        I32_8,
    }

    impl StoreSig {
        /// Match an `Instruction` against this kind, returning its
        /// `MemArg.offset` on hit. `None` on width mismatch — caller
        /// turns that into a panic with the surrounding context.
        fn match_offset(&self, inst: &Instruction<'_>) -> Option<u64> {
            match (self, inst) {
                (StoreSig::I32, Instruction::I32Store(ma)) => Some(ma.offset),
                (StoreSig::I64, Instruction::I64Store(ma)) => Some(ma.offset),
                (StoreSig::F32, Instruction::F32Store(ma)) => Some(ma.offset),
                (StoreSig::F64, Instruction::F64Store(ma)) => Some(ma.offset),
                (StoreSig::I32_8, Instruction::I32Store8(ma)) => Some(ma.offset),
                _ => None,
            }
        }
    }

    /// Parse `wit`, find the single function in `pkg/iface`, and run
    /// it through `build_lower_params_to_memory` with the given
    /// `params_record_base`. Returns the resolved size_align for
    /// downstream offset assertions plus the emitted instructions.
    fn run_lower_params(
        wit: &str,
        pkg_iface: &str,
        params_record_base: i32,
        nparams_flat: u32,
    ) -> (SizeAlign, Vec<Instruction<'static>>) {
        let mut resolve = Resolve::default();
        resolve.push_str("test.wit", wit).expect("parse test WIT");
        let (_, iface) = resolve
            .interfaces
            .iter()
            .find(|(id, _)| resolve.id_of(*id).as_deref() == Some(pkg_iface))
            .expect("target interface present");
        let func = iface
            .functions
            .values()
            .next()
            .expect("interface must have one fn")
            .clone();

        let mut sizes = SizeAlign::default();
        sizes.fill(&resolve);
        let mut indices = LocalsBuilder::new(nparams_flat);
        let insts =
            build_lower_params_to_memory(&resolve, &sizes, &mut indices, &func, params_record_base);
        (sizes, insts)
    }

    /// Snapshot test pinning the byte-level layout produced by
    /// `build_lower_params_to_memory` for a 6-primitive-mixed async
    /// fn. Catches the bugs validation can't: wrong store width
    /// (`i32.store8` vs `i32.store16`), swapped field offsets, off-by-
    /// one in `field_offsets`. The expected offsets fall out of the
    /// canonical ABI's record-of-primitives layout — pinning them
    /// here means a future `SizeAlign` regression fails this test
    /// instead of silently producing miswired wasm that still validates.
    #[test]
    fn build_lower_params_to_memory_pins_mixed_primitive_layout() {
        const BASE: i32 = 1024;
        // Six distinct flat widths + one repeated (u32 / char both i32)
        // — covers every store width the lower-mode bindgen emits today.
        let wit = r#"
            package my:shape@1.0.0;
            interface api {
                mixed: async func(
                    a: u32, b: u64, c: f32, d: f64, e: bool, f: char
                ) -> u32;
            }
        "#;
        let (sizes, insts) = run_lower_params(wit, "my:shape/api@1.0.0", BASE, 6);

        // Canonical-ABI record layout for `(u32, u64, f32, f64, bool, char)`:
        // u32@0, u64@8, f32@16, f64@24, bool@32, char@36 (size 40, align 8).
        // Pull live values from `SizeAlign::field_offsets` so a wit-parser
        // upgrade that changes the spec doesn't silently desync the test.
        let param_types = [
            Type::U32,
            Type::U64,
            Type::F32,
            Type::F64,
            Type::Bool,
            Type::Char,
        ];
        let field_offs = sizes.field_offsets(&param_types);
        let expected: Vec<(i64, StoreSig)> = field_offs
            .iter()
            .zip([
                StoreSig::I32,   // u32
                StoreSig::I64,   // u64
                StoreSig::F32,   // f32
                StoreSig::F64,   // f64
                StoreSig::I32_8, // bool — 1 byte store
                StoreSig::I32,   // char — full i32 store
            ])
            .map(|((off, _size), sig)| (BASE as i64 + off.size_wasm32() as i64, sig))
            .collect();

        // Each primitive lowers to a fixed 7-instruction block:
        //   i32.const <effective_addr>; local.set $addr_local;
        //   local.get $param_flat;
        //   local.set $tmp_<vt>; local.get $addr_local; local.get $tmp_<vt>;
        //   <store> offset=0
        // The addr_local was allocated first inside
        // `build_lower_params_to_memory`, so its index is `nparams_flat`.
        const ADDR_LOCAL: u32 = 6;
        const BLOCK_LEN: usize = 7;
        assert_eq!(
            insts.len(),
            BLOCK_LEN * expected.len(),
            "expected {} instructions per param, got {} total",
            BLOCK_LEN,
            insts.len(),
        );

        for (param_i, (eff_addr, sig)) in expected.iter().enumerate() {
            let block = &insts[param_i * BLOCK_LEN..(param_i + 1) * BLOCK_LEN];

            // [0] i32.const <effective_addr>
            match &block[0] {
                Instruction::I32Const(v) => assert_eq!(
                    *v as i64, *eff_addr,
                    "param {param_i}: i32.const should stage effective addr",
                ),
                other => panic!("param {param_i}: block[0] expected I32Const, got {other:?}"),
            }
            // [1] local.set $addr_local
            assert!(
                matches!(&block[1], Instruction::LocalSet(idx) if *idx == ADDR_LOCAL),
                "param {param_i}: block[1] expected LocalSet({ADDR_LOCAL}), got {:?}",
                &block[1],
            );
            // [2] local.get $param_flat (cursor advances 1 per primitive)
            assert!(
                matches!(&block[2], Instruction::LocalGet(idx) if *idx == param_i as u32),
                "param {param_i}: block[2] expected LocalGet({param_i}), got {:?}",
                &block[2],
            );
            // [3] local.set $tmp — captured for [5]'s reload check
            let tmp = match &block[3] {
                Instruction::LocalSet(idx) => *idx,
                other => panic!("param {param_i}: block[3] expected LocalSet, got {other:?}"),
            };
            // [4] local.get $addr_local
            assert!(
                matches!(&block[4], Instruction::LocalGet(idx) if *idx == ADDR_LOCAL),
                "param {param_i}: block[4] expected LocalGet({ADDR_LOCAL}), got {:?}",
                &block[4],
            );
            // [5] local.get $tmp — must be the same local stored at [3]
            assert!(
                matches!(&block[5], Instruction::LocalGet(idx) if *idx == tmp),
                "param {param_i}: block[5] should reload the same tmp ({tmp}) stored at [3], got {:?}",
                &block[5],
            );
            // [6] <store> offset=0 — the field offset is in addr_local,
            // not in MemArg.offset. A non-zero offset would mean the
            // bindgen mistakenly fused the addr stage and the store.
            let actual_offset = sig.match_offset(&block[6]).unwrap_or_else(|| {
                panic!(
                    "param {param_i}: block[6] expected {sig:?} store, got {:?}",
                    &block[6],
                )
            });
            assert_eq!(
                actual_offset, 0,
                "param {param_i}: store offset should be 0 (field offset goes in addr_local)",
            );
        }
    }

    /// Single record param `{a: u32, b: u64, c: f32, d: f64, e: bool,
    /// f: char}` — same widths as the mixed-primitives test but nested
    /// inside a record. The bindgen must stage the addr local *once*
    /// (one top-level param), advance cursor through all 6 fields, and
    /// emit each store with the field's canonical in-record offset
    /// baked into `MemArg.offset` (not into `addr_local`). A record-
    /// layout regression in `field_offsets` would silently produce
    /// miswired wasm that still validates.
    #[test]
    fn build_lower_params_to_memory_pins_record_field_offsets() {
        const BASE: i32 = 2048;
        // Record with one field per primitive width — same layout as
        // the standalone mixed-primitives test, just inside a record.
        let wit = r#"
            package my:shape@1.0.0;
            interface api {
                record mixed-rec {
                    a: u32, b: u64, c: f32, d: f64, e: bool, f: char,
                }
                only-rec: async func(r: mixed-rec) -> u32;
            }
        "#;
        let (sizes, insts) = run_lower_params(wit, "my:shape/api@1.0.0", BASE, 6);

        // The record's in-memory layout matches the standalone
        // mixed-primitives record (sourced live from `field_offsets`
        // so a wit-parser layout regression desyncs both at once).
        let field_tys = [
            Type::U32,
            Type::U64,
            Type::F32,
            Type::F64,
            Type::Bool,
            Type::Char,
        ];
        let field_offs = sizes.field_offsets(&field_tys);
        let expected: Vec<(u64, StoreSig)> = field_offs
            .iter()
            .zip([
                StoreSig::I32,
                StoreSig::I64,
                StoreSig::F32,
                StoreSig::F64,
                StoreSig::I32_8,
                StoreSig::I32,
            ])
            .map(|((off, _size), sig)| (off.size_wasm32() as u64, sig))
            .collect();

        // One top-level param → one `i32.const BASE; local.set $addr`
        // setup at the head, then 6 × 5-instruction field blocks
        // (local.get $param; local.set $tmp; local.get $addr;
        //  local.get $tmp; <store> offset=<field>).
        const HEAD_LEN: usize = 2;
        const FIELD_LEN: usize = 5;
        assert_eq!(
            insts.len(),
            HEAD_LEN + FIELD_LEN * expected.len(),
            "expected {HEAD_LEN} setup + {FIELD_LEN} per field, got {} total",
            insts.len(),
        );

        // Head: i32.const BASE; local.set $addr_local. addr_local
        // index = nparams_flat (allocated first inside
        // build_lower_params_to_memory).
        const ADDR_LOCAL: u32 = 6;
        match &insts[0] {
            Instruction::I32Const(v) => {
                assert_eq!(*v, BASE, "head: i32.const should stage params record base")
            }
            other => panic!("head[0] expected I32Const, got {other:?}"),
        }
        assert!(
            matches!(&insts[1], Instruction::LocalSet(idx) if *idx == ADDR_LOCAL),
            "head[1] expected LocalSet({ADDR_LOCAL}), got {:?}",
            &insts[1],
        );

        // Per-field block at indices [HEAD_LEN + i*FIELD_LEN ..]:
        for (field_i, (expected_off, sig)) in expected.iter().enumerate() {
            let block = &insts[HEAD_LEN + field_i * FIELD_LEN..][..FIELD_LEN];
            // [0] local.get $param_flat (cursor advances 1 per primitive field)
            assert!(
                matches!(&block[0], Instruction::LocalGet(idx) if *idx == field_i as u32),
                "field {field_i}: block[0] expected LocalGet({field_i}), got {:?}",
                &block[0],
            );
            // [1] local.set $tmp
            let tmp = match &block[1] {
                Instruction::LocalSet(idx) => *idx,
                other => panic!("field {field_i}: block[1] expected LocalSet, got {other:?}"),
            };
            // [2] local.get $addr_local
            assert!(
                matches!(&block[2], Instruction::LocalGet(idx) if *idx == ADDR_LOCAL),
                "field {field_i}: block[2] expected LocalGet({ADDR_LOCAL}), got {:?}",
                &block[2],
            );
            // [3] local.get $tmp (same as [1])
            assert!(
                matches!(&block[3], Instruction::LocalGet(idx) if *idx == tmp),
                "field {field_i}: block[3] should reload tmp ({tmp}), got {:?}",
                &block[3],
            );
            // [4] <store> with the field's in-record offset baked into MemArg.
            let actual_offset = sig.match_offset(&block[4]).unwrap_or_else(|| {
                panic!(
                    "field {field_i}: block[4] expected {sig:?} store, got {:?}",
                    &block[4],
                )
            });
            assert_eq!(
                actual_offset, *expected_off,
                "field {field_i}: store offset should match canonical record layout",
            );
        }
    }

    /// Sister snapshot for the all-`u32` case — verifies cursor
    /// advances per param and that all stores share the *same* tmp
    /// (`store_tmp_by_valtype` reuses across same-ValType params).
    #[test]
    fn build_lower_params_to_memory_reuses_tmp_across_same_valtype() {
        const BASE: i32 = 64;
        let wit = r#"
            package my:shape@1.0.0;
            interface api {
                many: async func(
                    a: u32, b: u32, c: u32, d: u32, e: u32
                ) -> u32;
            }
        "#;
        let (_sizes, insts) = run_lower_params(wit, "my:shape/api@1.0.0", BASE, 5);

        const BLOCK_LEN: usize = 7;
        assert_eq!(insts.len(), BLOCK_LEN * 5);

        // All 5 stores are i32 — the bindgen should allocate exactly
        // one i32 tmp and reuse it. Pull the tmp from block 0 and
        // assert every other block uses the same one.
        let first_tmp = match &insts[3] {
            Instruction::LocalSet(idx) => *idx,
            other => panic!("block[0]'s [3] expected LocalSet, got {other:?}"),
        };
        for i in 1..5 {
            let block = &insts[i * BLOCK_LEN..(i + 1) * BLOCK_LEN];
            assert!(
                matches!(&block[3], Instruction::LocalSet(idx) if *idx == first_tmp),
                "param {i}: tmp local should match first param's tmp ({first_tmp})",
            );
        }
    }
}
