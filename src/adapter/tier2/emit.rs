//! Tier-2 adapter generator: builds an adapter component that lifts
//! a target function's canonical-ABI parameters into the cell-array
//! representation and forwards them to the middleware's tier-2
//! `on-call` hook before dispatching to the handler.
//!
//! Status (Phase 2-3, slice 2): on-call invocation is wired in. For
//! each target function the wrapper builds the `call-id` strings,
//! calls `on-call(call_id, empty-list)` via canon-lower-async,
//! awaits via the canon-async runtime intrinsics, then forwards the
//! original args to the handler.
//!
//! `args` is currently always an empty `list<field>` — the
//! cell-construction path that fills it lives in
//! [`super::cells`] but isn't wired into the orchestrator yet (next
//! slice). End-to-end, this means a tier-2 middleware's `on-call`
//! fires with the right call identity but observes no payload data.
//!
//! Async target functions, `on-return`, and `on-trap` remain
//! out-of-scope for this slice.
//!
//! Design conventions intentionally mirror the tier-1 emit path
//! (`super::super::tier1::emit`).

use anyhow::{anyhow, bail, Context, Result};
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, MemArg, Module, TypeSection, ValType,
};
use wit_component::{embed_component_metadata, ComponentEncoder, StringEncoding};
use wit_parser::abi::{AbiVariant, WasmSignature};
use wit_parser::{
    Function as WitFunction, InterfaceId, LiftLowerAbi, ManglingAndAbi, Resolve, SizeAlign, Type,
    TypeId, WasmExport, WasmExportKind, WasmImport, WorldId, WorldItem, WorldKey,
};

use super::super::abi::canon_async;
use super::super::abi::emit::{
    empty_function, emit_cabi_realloc, emit_memory_and_globals, val_types, EXPORT_CABI_REALLOC,
    EXPORT_INITIALIZE, EXPORT_MEMORY,
};
use super::super::mem_layout::StaticLayout;
use super::super::resolve::{decode_input_resolve, find_target_interface};
use super::cells::{
    emit_bool_cell, emit_bytes_cell, emit_floating_cell, emit_integer_cell, emit_text_cell,
    CELL_SIZE,
};

const TIER2_ADAPTER_WORLD_PACKAGE: &str = "splicer:adapter-tier2";
const TIER2_ADAPTER_WORLD_NAME: &str = "adapter";

/// Generate a tier-2 adapter component.
pub(crate) fn build_tier2_adapter(
    target_interface: &str,
    has_before: bool,
    split_bytes: &[u8],
    common_wit: &str,
    tier2_wit: &str,
) -> Result<Vec<u8>> {
    if !has_before {
        bail!(
            "tier-2 adapter generation currently requires the middleware to export \
             `splicer:tier2/before` — `after`-only and `trap`-only middleware are \
             planned for a follow-up slice."
        );
    }

    let mut resolve = decode_input_resolve(split_bytes)?;
    let target_iface = find_target_interface(&resolve, target_interface)?;
    require_supported_case(&resolve, target_iface)?;

    resolve
        .push_str("splicer-common.wit", common_wit)
        .context("parse common WIT")?;
    resolve
        .push_str("splicer-tier2.wit", tier2_wit)
        .context("parse tier2 WIT")?;
    let world_pkg = resolve
        .push_str(
            "splicer-adapter-tier2.wit",
            &synthesize_adapter_world_wit(target_interface),
        )
        .context("parse synthesized tier-2 adapter world WIT")?;
    let world_id = resolve
        .select_world(&[world_pkg], Some(TIER2_ADAPTER_WORLD_NAME))
        .context("select tier-2 adapter world")?;

    let mut core_module =
        build_dispatch_module(&resolve, world_id, target_iface, target_interface)?;
    embed_component_metadata(&mut core_module, &resolve, world_id, StringEncoding::UTF8)
        .context("embed_component_metadata")?;

    ComponentEncoder::default()
        .validate(true)
        .module(&core_module)
        .context("ComponentEncoder::module")?
        .encode()
        .context("ComponentEncoder::encode")
}

/// Bail on cases that fail before the lift codegen even runs.
fn require_supported_case(resolve: &Resolve, target_iface: InterfaceId) -> Result<()> {
    let iface = &resolve.interfaces[target_iface];
    if iface.functions.is_empty() {
        bail!("interface has no functions");
    }
    for (name, func) in &iface.functions {
        if func.kind.is_async() {
            bail!(
                "tier-2 first slice doesn't yet support async target functions; \
                 `{name}` is async"
            );
        }
    }
    Ok(())
}

/// Synthesize the tier-2 adapter world.
fn synthesize_adapter_world_wit(target_interface: &str) -> String {
    use crate::contract::{versioned_interface, TIER2_BEFORE, TIER2_VERSION};
    let mut wit = format!(
        "package {TIER2_ADAPTER_WORLD_PACKAGE};\n\nworld {TIER2_ADAPTER_WORLD_NAME} {{\n"
    );
    wit.push_str(&format!("    import {target_interface};\n"));
    wit.push_str(&format!("    export {target_interface};\n"));
    wit.push_str(&format!(
        "    import {};\n",
        versioned_interface(TIER2_BEFORE, TIER2_VERSION)
    ));
    wit.push_str("}\n");
    wit
}

// ─── Dispatch core module ──────────────────────────────────────────

/// Per-function dispatch shape. The on-call hook is called with the
/// `(iface, fn)` strings shared from the same name blob — the
/// interface name is allocated once and reused; each function's name
/// gets its own slot.
struct FuncDispatch {
    import_module: String,
    import_field: String,
    export_name: String,
    sig: WasmSignature,
    needs_cabi_post: bool,
    /// Byte offset of the function name within the data segment.
    fn_name_offset: i32,
    fn_name_len: i32,
    /// Per-param lift recipe + wasm flat-slot indexing. Empty for
    /// zero-arg functions.
    params: Vec<ParamLift>,
    /// Byte offset of this function's cells scratch slab. The slab
    /// holds `params.len()` consecutive cells, each [`CELL_SIZE`]
    /// bytes. Cleared (zeroed by the runtime) at module instantiation;
    /// rewritten per-call in the wrapper body.
    cells_buf_offset: u32,
    /// Byte offset of this function's pre-built `field` records in
    /// the data segment. Holds `params.len()` consecutive `field`
    /// records, each [`FIELD_SIZE_BYTES`] bytes. Pointed at by the
    /// `args.list.ptr` field passed to `on-call`.
    fields_buf_offset: u32,
}

/// Per-parameter lift recipe. `first_local` is the wasm local index
/// of the first flat slot for this param (subsequent slots for
/// multi-slot params live at +1, +2, ...). `name_offset` /
/// `name_len` reference the param name in the shared name blob.
struct ParamLift {
    name_offset: i32,
    name_len: i32,
    kind: LiftKind,
    first_local: u32,
}

/// How to widen a param's flat-form value(s) into the matching
/// `cell` variant payload. One slot per kind unless noted.
#[derive(Clone, Copy)]
enum LiftKind {
    /// `bool` — 1 i32 slot (0/1) → `cell::bool`.
    Bool,
    /// `s8`/`s16`/`s32` — 1 i32 slot, sign-extend → `cell::integer`.
    IntegerSignExt,
    /// `u8`/`u16`/`u32` — 1 i32 slot, zero-extend → `cell::integer`.
    IntegerZeroExt,
    /// `s64`/`u64` — 1 i64 slot, no widen → `cell::integer`.
    Integer64,
    /// `f32` — 1 f32 slot, `f64.promote_f32` → `cell::floating`.
    FloatingF32,
    /// `f64` — 1 f64 slot, no widen → `cell::floating`.
    FloatingF64,
    /// `string` — 2 i32 slots (ptr, len) → `cell::text`.
    Text,
    /// `list<u8>` — 2 i32 slots (ptr, len) → `cell::bytes`.
    Bytes,
}

impl LiftKind {
    /// Number of flat wasm slots this param consumes.
    fn slot_count(self) -> u32 {
        match self {
            LiftKind::Text | LiftKind::Bytes => 2,
            _ => 1,
        }
    }

    /// Classify a WIT param type. Returns `None` for types not yet
    /// supported by Phase 2-2a primitives (compound types, char,
    /// resources, etc.) — caller bails with a clear error.
    fn classify(ty: &Type, resolve: &Resolve) -> Option<LiftKind> {
        match ty {
            Type::Bool => Some(LiftKind::Bool),
            Type::S8 | Type::S16 | Type::S32 => Some(LiftKind::IntegerSignExt),
            Type::U8 | Type::U16 | Type::U32 => Some(LiftKind::IntegerZeroExt),
            Type::S64 | Type::U64 => Some(LiftKind::Integer64),
            Type::F32 => Some(LiftKind::FloatingF32),
            Type::F64 => Some(LiftKind::FloatingF64),
            Type::String => Some(LiftKind::Text),
            // `list<u8>` fast-path: peek through TypeDefKind::List for u8.
            Type::Id(id) => match &resolve.types[*id].kind {
                wit_parser::TypeDefKind::List(elem) if matches!(elem, Type::U8) => {
                    Some(LiftKind::Bytes)
                }
                _ => None,
            },
            // Char (utf-8 encoding required at lift time) and
            // ErrorContext are deferred to follow-up slices.
            Type::Char | Type::ErrorContext => None,
        }
    }
}

/// Canonical-ABI byte size of one `field` record (`name: string,
/// tree: field-tree`) on wasm32. Verified at build time against
/// `wit-parser::SizeAlign`. Hard-coded here because `emit_data_section`
/// pre-builds the records' bytes; if the WIT layout drifts, the
/// assertion in [`build_dispatch_module`] fires.
const FIELD_SIZE_BYTES: u32 = 60;

/// Sub-offsets within one `field` record. Match the canonical-ABI
/// lower of `record { name: string, tree: field-tree }` — strings
/// and lists each lower to `(ptr: i32, len: i32)`, all i32-aligned.
mod field_offset {
    pub(super) const NAME_PTR: u32 = 0;
    pub(super) const NAME_LEN: u32 = 4;
    pub(super) const CELLS_PTR: u32 = 8;
    pub(super) const CELLS_LEN: u32 = 12;
    // Bytes 16..56 hold the five empty side-table list pairs and
    // are zero-initialized; they never get patched at runtime.
    pub(super) const ROOT: u32 = 56;
}

/// Alignment of one `field` record in linear memory. All fields are
/// i32-pointer-aligned (string + lists + u32 root → 4 bytes).
const FIELD_ALIGN_BYTES: u32 = 4;

/// Alignment for one cell. `cell::integer` / `cell::floating` use
/// 8-byte stores at the payload offset (cell_addr + 8), so the
/// slab base must be 8-aligned and CELL_SIZE must be a multiple of 8.
const CELL_SLAB_ALIGN: u32 = 8;

/// Size + alignment of the `waitable-set.wait` event record slot.
const EVENT_SLOT_SIZE: u32 = 8;
const EVENT_SLOT_ALIGN: u32 = 4;

/// Alignment for the on-call indirect-params buffer. All fields are
/// i32, so 4-byte alignment suffices.
const HOOK_PARAMS_ALIGN: u32 = 4;

/// Byte size of the on-call hook's indirect-params buffer. Matches
/// the canonical-ABI lower of `record { call-id, list<field> }`:
/// two strings (8 B each) + a list pointer/len pair (8 B) = 24 B,
/// 4-aligned. Reserved as a static slot in the adapter's memory
/// layout; populated per-call by [`emit_populate_hook_params`].
///
/// `on-call`'s 6 flat i32s exceed canon-lower-async's
/// MAX_FLAT_ASYNC_PARAMS = 4, so the canonical ABI lowers them into
/// a memory record and hands canon-lower-async a single pointer.
const HOOK_PARAMS_SIZE: u32 = 24;

/// Emit wasm that writes the call-id (interface + function name
/// pointers/lengths) and the per-call `list<field>` args pointer/
/// length into the indirect-params buffer at `base_ptr`. Field
/// offsets match the canonical-ABI lower of the on-call params
/// record:
///
/// ```text
/// offset  0: interface-name.ptr
/// offset  4: interface-name.len
/// offset  8: function-name.ptr
/// offset 12: function-name.len
/// offset 16: args.list.ptr
/// offset 20: args.list.len
/// ```
fn emit_populate_hook_params(
    f: &mut Function,
    base_ptr: i32,
    iface_name_offset: i32,
    iface_name_len: i32,
    fn_name_offset: i32,
    fn_name_len: i32,
    args_ptr: i32,
    args_len: i32,
) {
    let mut store_i32 = |field_offset: u64, value: i32| {
        f.instructions().i32_const(base_ptr);
        f.instructions().i32_const(value);
        f.instructions().i32_store(MemArg {
            offset: field_offset,
            align: 2,
            memory_index: 0,
        });
    };
    store_i32(0, iface_name_offset);
    store_i32(4, iface_name_len);
    store_i32(8, fn_name_offset);
    store_i32(12, fn_name_len);
    store_i32(16, args_ptr);
    store_i32(20, args_len);
}

fn build_dispatch_module(
    resolve: &Resolve,
    world_id: WorldId,
    target_iface: InterfaceId,
    target_interface_name: &str,
) -> Result<Vec<u8>> {
    let funcs: Vec<&WitFunction> = resolve.interfaces[target_iface]
        .functions
        .values()
        .collect();

    // Sanity-check the FIELD_SIZE_BYTES constant against the live
    // WIT — fires if the `field` / `field-tree` record gains a
    // member without bumping the constant.
    let mut size_align = SizeAlign::default();
    size_align.fill(resolve);
    let field_ty_id = find_field_typeid(resolve)?;
    let computed_field_size = size_align.size(&Type::Id(field_ty_id)).size_wasm32() as u32;
    if computed_field_size != FIELD_SIZE_BYTES {
        bail!(
            "FIELD_SIZE_BYTES ({FIELD_SIZE_BYTES}) is out of date; \
             wit-parser computes {computed_field_size} bytes for `splicer:common/types`.field — \
             update FIELD_SIZE_BYTES + field_offset::* to match"
        );
    }

    // ── Name blob layout ─────────────────────────────────────────
    // [interface_name][fn_name_0][param0_name][param1_name]...
    //                 [fn_name_1][...]
    // The interface name is shared across all funcs; each function
    // and each of its params get their own slots. Param names feed
    // the pre-built `field` records' `name: string` fields.

    let iface_name_offset: i32 = 0;
    let iface_name_len = target_interface_name.len() as i32;

    let target_world_key = WorldKey::Interface(target_iface);
    let sync_mangling = ManglingAndAbi::Legacy(LiftLowerAbi::Sync);

    let mut name_blob: Vec<u8> = target_interface_name.as_bytes().to_vec();
    let mut per_func: Vec<FuncDispatch> = Vec::with_capacity(funcs.len());

    for func in &funcs {
        let fn_name_offset = name_blob.len() as i32;
        let fn_name_len = func.name.len() as i32;
        name_blob.extend_from_slice(func.name.as_bytes());

        // Walk params: classify each, append name to the blob, track
        // the first wasm flat-slot index. `slot_count()` advances the
        // cursor for multi-slot params (string, list<u8>).
        let mut params_lift: Vec<ParamLift> = Vec::with_capacity(func.params.len());
        let mut slot_cursor: u32 = 0;
        for param in &func.params {
            let pname = &param.name;
            let ptype = &param.ty;
            let kind = LiftKind::classify(ptype, resolve).ok_or_else(|| {
                anyhow!(
                    "tier-2 lift codegen does not yet support param `{pname}` of `{}` \
                     (type {ptype:?}) — only primitives + string + list<u8> are wired today",
                    func.name
                )
            })?;
            let name_offset = name_blob.len() as i32;
            let name_len = pname.len() as i32;
            name_blob.extend_from_slice(pname.as_bytes());
            params_lift.push(ParamLift {
                name_offset,
                name_len,
                kind,
                first_local: slot_cursor,
            });
            slot_cursor += kind.slot_count();
        }

        let (import_module, import_field) = resolve.wasm_import_name(
            sync_mangling,
            WasmImport::Func {
                interface: Some(&target_world_key),
                func,
            },
        );
        let export_name = resolve.wasm_export_name(
            sync_mangling,
            WasmExport::Func {
                interface: Some(&target_world_key),
                func,
                kind: WasmExportKind::Normal,
            },
        );
        let sig = resolve.wasm_signature(AbiVariant::GuestExport, func);
        let needs_cabi_post = sig.retptr;

        per_func.push(FuncDispatch {
            import_module,
            import_field,
            export_name,
            sig,
            needs_cabi_post,
            fn_name_offset,
            fn_name_len,
            params: params_lift,
            // Buffer offsets get filled in once the layout is
            // resolved; placeholder zeros below.
            cells_buf_offset: 0,
            fields_buf_offset: 0,
        });
    }

    // ── Memory layout (built via `StaticLayout` for alignment safety) ──
    //
    // Sections are placed in this order; the builder pads between
    // them according to each section's declared alignment so a
    // future allocation can't accidentally land on a misaligned
    // boundary. Per-fn cells slabs require 8-byte alignment because
    // `cell::integer` and `cell::floating` use 8-byte stores at
    // `cell_addr + 8`; misaligned cells trap inside canon-lower-async.
    //
    //   name_blob (data, align 1)
    //   per-fn cells slabs (scratch, align 8) — placed BEFORE fields
    //                                            so fields can embed
    //                                            their addresses
    //   fields_blob (data, align 4)
    //   event_slot (scratch, align 4, size 8)
    //   hook_params (scratch, align 4, size HOOK_PARAMS_SIZE)
    //   bump_start = layout.end() padded to 8
    let mut layout = StaticLayout::new();

    layout.place_data(1, &name_blob);

    // Reserve cells slabs first — fields records embed pointers to
    // these slabs, so the layout must know their offsets before
    // building the fields blob. Slabs are scratch (zero-init by the
    // wasm runtime); the wrapper body overwrites them per-call.
    for fd in per_func.iter_mut() {
        let slab_size = fd.params.len() as u32 * CELL_SIZE;
        fd.cells_buf_offset = layout.reserve_scratch(CELL_SLAB_ALIGN, slab_size);
    }

    // Build the fields blob with all pointers already correct.
    let mut fields_blob: Vec<u8> = Vec::new();
    for fd in per_func.iter_mut() {
        fd.fields_buf_offset = u32::MAX; // sentinel; first param sets real value.
        for (i, p) in fd.params.iter().enumerate() {
            let field_start = fields_blob.len();
            fields_blob.extend(std::iter::repeat(0u8).take(FIELD_SIZE_BYTES as usize));
            let cell_addr = (fd.cells_buf_offset + i as u32 * CELL_SIZE) as i32;
            write_le_i32(&mut fields_blob, field_start + field_offset::NAME_PTR as usize, p.name_offset);
            write_le_i32(&mut fields_blob, field_start + field_offset::NAME_LEN as usize, p.name_len);
            write_le_i32(&mut fields_blob, field_start + field_offset::CELLS_PTR as usize, cell_addr);
            write_le_i32(&mut fields_blob, field_start + field_offset::CELLS_LEN as usize, 1);
            // Side-table lists (offsets 16..56) stay zero — empty
            // ptr/len pairs. `root` (offset 56) stays 0 — the single
            // primitive cell.
            write_le_i32(&mut fields_blob, field_start + field_offset::ROOT as usize, 0);
        }
    }
    let fields_base = layout.place_data(FIELD_ALIGN_BYTES, &fields_blob);
    // Now back-fill each fn's fields_buf_offset.
    let mut fields_cursor = fields_base;
    for fd in per_func.iter_mut() {
        fd.fields_buf_offset = fields_cursor;
        fields_cursor += fd.params.len() as u32 * FIELD_SIZE_BYTES;
    }

    let event_ptr = layout.reserve_scratch(EVENT_SLOT_ALIGN, EVENT_SLOT_SIZE) as i32;
    let hook_params_ptr = layout.reserve_scratch(HOOK_PARAMS_ALIGN, HOOK_PARAMS_SIZE);

    let bump_start = align_up(layout.end(), 8);
    let data_segments = layout.into_segments();

    // Find the on-call hook function (its WasmImport name + sig come
    // from `Resolve::wasm_import_name` / `wasm_signature` with the
    // async-callback mangling, same as tier-1's hooks).
    let hook = find_on_call_hook(resolve, world_id)?;

    let mut module = Module::new();
    let type_idx = emit_type_section(&mut module, &per_func, &hook.sig);
    let func_idx = emit_imports_and_funcs(&mut module, &per_func, &type_idx, &hook, event_ptr);
    emit_memory_and_globals(&mut module, bump_start);
    emit_export_section(
        &mut module,
        &per_func,
        func_idx.wrapper_base,
        func_idx.init_idx,
        func_idx.cabi_realloc_idx,
    );
    emit_code_section(
        &mut module,
        &per_func,
        &func_idx,
        iface_name_offset,
        iface_name_len,
        hook_params_ptr as i32,
    );
    emit_data_section(&mut module, &data_segments);
    Ok(module.finish())
}

/// Locate the `splicer:common/types`.field typedef in the resolved
/// `Resolve`. Returns the typedef id ready for `SizeAlign::size`.
fn find_field_typeid(resolve: &Resolve) -> Result<TypeId> {
    // Iterate interfaces and match by `id_of` against
    // `"splicer:common/types[@<version>]"`. The tier WITs and the
    // common WIT travel together in this repo, so whichever version
    // got loaded is the canonical one.
    for (id, _) in resolve.interfaces.iter() {
        let qname = match resolve.id_of(id) {
            Some(s) => s,
            None => continue,
        };
        // Match `splicer:common/types[@version]` regardless of version
        // — the resolve just loaded common WIT, so there's only one.
        let unversioned = qname.split('@').next().unwrap_or(&qname);
        if unversioned == "splicer:common/types" {
            return resolve.interfaces[id]
                .types
                .get("field")
                .copied()
                .ok_or_else(|| anyhow!("`splicer:common/types` is missing typedef `field`"));
        }
    }
    bail!("resolve has no `splicer:common/types` interface — was the common WIT loaded?")
}

fn write_le_i32(buf: &mut [u8], offset: usize, value: i32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn align_up(x: u32, align: u32) -> u32 {
    (x + align - 1) & !(align - 1)
}

/// Resolved on-call hook info — module + name + signature, all
/// sourced from `Resolve` so wit-component agrees on the canonical
/// names.
struct HookImport {
    module: String,
    name: String,
    sig: WasmSignature,
}

fn find_on_call_hook(resolve: &Resolve, world_id: WorldId) -> Result<HookImport> {
    use crate::contract::{TIER2_BEFORE, TIER2_VERSION};
    let target_iface = format!("{TIER2_BEFORE}@{TIER2_VERSION}");
    let world = &resolve.worlds[world_id];
    for (key, item) in &world.imports {
        if let WorldItem::Interface { id, .. } = item {
            if resolve.id_of(*id).as_deref() != Some(&target_iface) {
                continue;
            }
            let func = resolve.interfaces[*id]
                .functions
                .values()
                .next()
                .ok_or_else(|| anyhow!("splicer:tier2/before has no functions"))?;
            let (module, name) = resolve.wasm_import_name(
                ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback),
                WasmImport::Func {
                    interface: Some(key),
                    func,
                },
            );
            let sig = resolve.wasm_signature(AbiVariant::GuestImportAsync, func);
            return Ok(HookImport { module, name, sig });
        }
    }
    bail!("synthesized adapter world is missing import of {target_iface}");
}

// ─── Section emission ─────────────────────────────────────────────

struct TypeIndices {
    handler_ty: Vec<u32>,
    wrapper_ty: Vec<u32>,
    hook_ty: u32,
    init_ty: u32,
    cabi_post_ty: u32,
    cabi_realloc_ty: u32,
    async_types: canon_async::AsyncTypes,
}

fn emit_type_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
    hook_sig: &WasmSignature,
) -> TypeIndices {
    let mut types = TypeSection::new();
    let mut next_ty: u32 = 0;
    let mut alloc_one = |ty_section: &mut TypeSection,
                         params: Vec<ValType>,
                         results: Vec<ValType>|
     -> u32 {
        ty_section.ty().function(params, results);
        let idx = next_ty;
        next_ty += 1;
        idx
    };

    let handler_ty: Vec<u32> = per_func
        .iter()
        .map(|fd| {
            alloc_one(
                &mut types,
                val_types(&fd.sig.params),
                val_types(&fd.sig.results),
            )
        })
        .collect();
    let wrapper_ty: Vec<u32> = per_func
        .iter()
        .map(|fd| {
            alloc_one(
                &mut types,
                val_types(&fd.sig.params),
                val_types(&fd.sig.results),
            )
        })
        .collect();

    let hook_ty = alloc_one(
        &mut types,
        val_types(&hook_sig.params),
        val_types(&hook_sig.results),
    );
    let init_ty = alloc_one(&mut types, vec![], vec![]);
    let cabi_post_ty = alloc_one(&mut types, vec![ValType::I32], vec![]);
    let cabi_realloc_ty = alloc_one(
        &mut types,
        vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        vec![ValType::I32],
    );

    let async_types = canon_async::emit_types(&mut types, || {
        let i = next_ty;
        next_ty += 1;
        i
    });

    module.section(&types);
    TypeIndices {
        handler_ty,
        wrapper_ty,
        hook_ty,
        init_ty,
        cabi_post_ty,
        cabi_realloc_ty,
        async_types,
    }
}

struct FuncIndices {
    handler_imp_base: u32,
    hook_idx: u32,
    async_funcs: canon_async::AsyncFuncs,
    wrapper_base: u32,
    init_idx: u32,
    cabi_realloc_idx: u32,
}

fn emit_imports_and_funcs(
    module: &mut Module,
    per_func: &[FuncDispatch],
    ty: &TypeIndices,
    hook: &HookImport,
    event_ptr: i32,
) -> FuncIndices {
    let mut imports = ImportSection::new();
    let mut next_imp: u32 = 0;

    let handler_imp_base = next_imp;
    for (fd, &fty) in per_func.iter().zip(&ty.handler_ty) {
        imports.import(&fd.import_module, &fd.import_field, EntityType::Function(fty));
        next_imp += 1;
    }

    imports.import(&hook.module, &hook.name, EntityType::Function(ty.hook_ty));
    let hook_idx = next_imp;
    next_imp += 1;

    let async_funcs = canon_async::import_intrinsics(&mut imports, &ty.async_types, event_ptr, || {
        let i = next_imp;
        next_imp += 1;
        i
    });

    module.section(&imports);

    let wrapper_base = next_imp;

    let mut fsec = FunctionSection::new();
    for &fty in &ty.wrapper_ty {
        fsec.function(fty);
    }
    fsec.function(ty.init_ty);
    let init_idx = wrapper_base + per_func.len() as u32;
    let mut cabi_post_first_idx = init_idx + 1;
    for fd in per_func {
        if fd.needs_cabi_post {
            fsec.function(ty.cabi_post_ty);
            cabi_post_first_idx += 1;
        }
    }
    fsec.function(ty.cabi_realloc_ty);
    let cabi_realloc_idx = cabi_post_first_idx;
    module.section(&fsec);

    FuncIndices {
        handler_imp_base,
        hook_idx,
        async_funcs,
        wrapper_base,
        init_idx,
        cabi_realloc_idx,
    }
}

fn emit_export_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
    wrapper_base: u32,
    init_idx: u32,
    cabi_realloc_idx: u32,
) {
    let mut exports = ExportSection::new();
    let mut next_post_idx = init_idx + 1;
    for (i, fd) in per_func.iter().enumerate() {
        exports.export(&fd.export_name, ExportKind::Func, wrapper_base + i as u32);
        if fd.needs_cabi_post {
            let post_name = format!("cabi_post_{}", fd.export_name);
            exports.export(&post_name, ExportKind::Func, next_post_idx);
            next_post_idx += 1;
        }
    }
    exports.export(EXPORT_MEMORY, ExportKind::Memory, 0);
    exports.export(EXPORT_CABI_REALLOC, ExportKind::Func, cabi_realloc_idx);
    exports.export(EXPORT_INITIALIZE, ExportKind::Func, init_idx);
    module.section(&exports);
}

/// Wrapper body shape:
///
/// ```text
/// ;; build call-id flat: (iface_ptr, iface_len, fn_ptr, fn_len)
/// i32.const iface_offset
/// i32.const iface_len
/// i32.const fn_offset
/// i32.const fn_len
/// ;; empty list<field> args (ptr=0, len=0)
/// i32.const 0
/// i32.const 0
/// call $on_call               ;; canon-lower-async — returns packed (handle<<4)|status
/// local.set $st
/// ;; wait loop (only if subtask didn't return synchronously)
/// local.get $st
/// i32.const 4
/// i32.shr_u
/// local.set $st               ;; raw subtask handle now
/// local.get $st
/// if
///     call $waitable_set_new
///     local.set $ws
///     local.get $st
///     local.get $ws
///     call $waitable_join
///     local.get $ws
///     i32.const event_ptr
///     call $waitable_set_wait
///     drop                     ;; event code (we don't inspect)
///     local.get $st
///     call $subtask_drop
///     local.get $ws
///     call $waitable_set_drop
/// end
/// ;; pass-through to handler
/// local.get $param_0 ; ... ; local.get $param_N
/// call $handler
/// ```
fn emit_code_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
    func_idx: &FuncIndices,
    iface_name_offset: i32,
    iface_name_len: i32,
    hook_params_ptr: i32,
) {
    let async_funcs = &func_idx.async_funcs;
    let mut code = CodeSection::new();
    for (i, fd) in per_func.iter().enumerate() {
        // Locals beyond the wasm sig params:
        //   $addr     i32 — scratch for the cell write address
        //   $st       i32 — packed status from on-call
        //   $ws       i32 — waitable-set handle for the wait loop
        //   $ext64    i64 — widened source for IntegerSignExt/ZeroExt
        //   $ext_f64  f64 — promoted source for FloatingF32
        let nparams = fd.sig.params.len() as u32;
        let addr = nparams;
        let st = nparams + 1;
        let ws = nparams + 2;
        let ext64 = nparams + 3;
        let ext_f64 = nparams + 4;
        let mut f = Function::new([
            (3, ValType::I32),
            (1, ValType::I64),
            (1, ValType::F64),
        ]);

        // Lift each param into its cells_buf slot. After this loop
        // the static fields_blob's cells.ptr already points at the
        // freshly-written cells; the host reads them when it lowers
        // the on-call args list.
        for (p_idx, p) in fd.params.iter().enumerate() {
            let cell_addr = fd.cells_buf_offset as i32 + p_idx as i32 * CELL_SIZE as i32;
            f.instructions().i32_const(cell_addr);
            f.instructions().local_set(addr);
            emit_lift_param(&mut f, p, addr, ext64, ext_f64);
        }

        // Populate the indirect-params buffer + call on-call (the
        // canon-lower-async signature is `(params_ptr) -> i32 packed
        // status` because `on-call`'s 6 flat params overflow
        // MAX_FLAT_ASYNC_PARAMS = 4).
        let nargs = fd.params.len() as i32;
        let args_ptr = if nargs == 0 {
            0
        } else {
            fd.fields_buf_offset as i32
        };
        emit_populate_hook_params(
            &mut f,
            hook_params_ptr,
            iface_name_offset,
            iface_name_len,
            fd.fn_name_offset,
            fd.fn_name_len,
            args_ptr,
            nargs,
        );
        f.instructions().i32_const(hook_params_ptr);
        f.instructions().call(func_idx.hook_idx);
        f.instructions().local_set(st);

        canon_async::emit_wait_loop(&mut f, st, ws, async_funcs);

        // Pass-through to handler with original args.
        for j in 0..nparams {
            f.instructions().local_get(j);
        }
        f.instructions().call(func_idx.handler_imp_base + i as u32);
        f.instructions().end();
        code.function(&f);
    }
    code.function(&empty_function());
    for fd in per_func {
        if fd.needs_cabi_post {
            code.function(&empty_function());
        }
    }
    emit_cabi_realloc(&mut code);
    module.section(&code);
}

/// Emit the wasm to lift one param into the cell at `addr_local`.
/// `ext64` / `ext_f64` are scratch widening locals (i64 / f64).
fn emit_lift_param(
    f: &mut Function,
    p: &ParamLift,
    addr_local: u32,
    ext64: u32,
    ext_f64: u32,
) {
    match p.kind {
        LiftKind::Bool => {
            emit_bool_cell(f, addr_local, p.first_local);
        }
        LiftKind::IntegerSignExt => {
            f.instructions().local_get(p.first_local);
            f.instructions().i64_extend_i32_s();
            f.instructions().local_set(ext64);
            emit_integer_cell(f, addr_local, ext64);
        }
        LiftKind::IntegerZeroExt => {
            f.instructions().local_get(p.first_local);
            f.instructions().i64_extend_i32_u();
            f.instructions().local_set(ext64);
            emit_integer_cell(f, addr_local, ext64);
        }
        LiftKind::Integer64 => {
            emit_integer_cell(f, addr_local, p.first_local);
        }
        LiftKind::FloatingF32 => {
            f.instructions().local_get(p.first_local);
            f.instructions().f64_promote_f32();
            f.instructions().local_set(ext_f64);
            emit_floating_cell(f, addr_local, ext_f64);
        }
        LiftKind::FloatingF64 => {
            emit_floating_cell(f, addr_local, p.first_local);
        }
        LiftKind::Text => {
            emit_text_cell(f, addr_local, p.first_local, p.first_local + 1);
        }
        LiftKind::Bytes => {
            emit_bytes_cell(f, addr_local, p.first_local, p.first_local + 1);
        }
    }
}

fn emit_data_section(module: &mut Module, segments: &[(u32, Vec<u8>)]) {
    if segments.is_empty() {
        return;
    }
    let mut data = DataSection::new();
    for (offset, bytes) in segments {
        data.active(0, &ConstExpr::i32_const(*offset as i32), bytes.iter().copied());
    }
    module.section(&data);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tier-2 adapter for a target with a single sync
    /// primitive function (`add(x: u32, y: u32) -> u32`-style), run
    /// it through ComponentEncoder, and validate the resulting
    /// component bytes round-trip through wasmparser. Confirms the
    /// dispatch module — including the on-call invocation + wait
    /// loop — produces structurally valid wasm.
    #[test]
    fn dispatch_module_roundtrips_through_component_encoder() {
        let wat = r#"(component
            (component $inner
                (core module $m
                    (func (export "add") (param i32 i32) (result i32)
                        local.get 0
                        local.get 1
                        i32.add
                    )
                )
                (core instance $i (instantiate $m))
                (alias core export $i "add" (core func $add))
                (type $add-ty (func (param "x" u32) (param "y" u32) (result u32)))
                (func $add-lifted (type $add-ty) (canon lift (core func $add)))
                (instance $api-inst (export "add" (func $add-lifted)))
                (export "my:math/api@1.0.0" (instance $api-inst))
            )
            (instance $api (instantiate $inner))
            (export "my:math/api@1.0.0" (instance $api "my:math/api@1.0.0"))
        )"#;
        let split_bytes = wat::parse_str(wat).expect("WAT must parse");

        let common_wit = include_str!("../../../wit/common/world.wit");
        let tier2_wit = include_str!("../../../wit/tier2/world.wit");

        let bytes = build_tier2_adapter(
            "my:math/api@1.0.0",
            true,
            &split_bytes,
            common_wit,
            tier2_wit,
        )
        .expect("tier-2 adapter generation should succeed");

        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("emitted tier-2 adapter component should validate");
    }
}
