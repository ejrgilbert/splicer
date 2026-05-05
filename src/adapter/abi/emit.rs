//! Wasm-encoder emit helpers shared between tier-1 and tier-2's
//! dispatch core modules. Everything here is canonical-ABI plumbing
//! that wit-component's `ComponentEncoder` requires the core module
//! to provide regardless of which tier of adapter it backs.

use anyhow::{anyhow, bail, Result};
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, Function,
    GlobalSection, GlobalType, MemArg, MemorySection, MemoryType, Module, ValType,
};
use wit_parser::abi::{AbiVariant, WasmSignature, WasmType};
use wit_parser::{
    Int, Resolve, SizeAlign, Type, TypeDefKind, TypeId, WasmImport, WorldId, WorldItem,
};

use super::super::indices::FunctionIndices;
use super::super::resolve::hook_callback_mangling;

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
    let mut locals = FunctionIndices::new(PARAM_COUNT);
    let aligned = locals.alloc_local(ValType::I32);
    let new_bump = locals.alloc_local(ValType::I32);
    let mut f = Function::new_with_locals_types(locals.into_locals());

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
        // memory.grow returns -1 on failure; trap so the host sees a
        // recognizable wasm trap instead of a corrupted bump pointer.
        f.instructions().i32_const(-1);
        f.instructions().i32_eq();
        f.instructions().if_(BlockType::Empty);
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
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BlobSlice {
    pub off: u32,
    pub len: u32,
}

impl BlobSlice {
    pub(crate) const EMPTY: BlobSlice = BlobSlice { off: 0, len: 0 };
}

/// Store `slice.off` then `slice.len` as the canonical-ABI `(ptr, len)`
/// pair at `base_ptr + field_off`.
pub(crate) fn emit_store_slice(f: &mut Function, base_ptr: i32, field_off: u32, slice: BlobSlice) {
    let store = |f: &mut Function, sub_off: u32, value: i32| {
        f.instructions().i32_const(base_ptr);
        f.instructions().i32_const(value);
        f.instructions().i32_store(MemArg {
            offset: (field_off + sub_off) as u64,
            align: 2,
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
        align: 3,
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
