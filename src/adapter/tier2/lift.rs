//! Tier-2 lift codegen: classifying WIT types into cell variants,
//! emitting the wasm that writes one cell per (param | result),
//! and laying out the per-field-tree side tables (`enum-infos` for
//! now; `flags-infos` / `record-infos` / `variant-infos` /
//! `handle-infos` join as their lift codegen lands).
//!
//! Split from `emit.rs` so the dispatch-orchestration code there
//! doesn't pull in every cell-variant detail.
//!
//! Three layers:
//! - **Classify.** [`LiftKind::classify`] maps a `Type` to a single
//!   `cell` variant; [`classify_func_params`] / [`classify_result_lift`]
//!   walk a function's params + result and produce [`ParamLift`] /
//!   [`ResultLift`] descriptors plus any [`SideTableInfo`] the lift
//!   needs.
//! - **Side-table population.** [`register_enum_strings`] +
//!   [`build_enum_info_blob`] precompute the per-field-tree side
//!   tables at adapter-build time (cells store side-table indices
//!   that index directly into these blobs at runtime).
//! - **Codegen.** [`emit_lift_param`] / [`emit_lift_kind`] /
//!   [`emit_lift_result`] emit the wasm that writes one cell, given
//!   the wrapper's [`WrapperLocals`] for scratch.

use std::collections::HashMap;

use wasm_encoder::{Function, MemArg, ValType};
use wit_parser::abi::WasmSignature;
use wit_parser::{Function as WitFunction, Resolve, Type};

use super::super::abi::emit::{
    direct_return_type, RecordLayout, SLICE_LEN_OFFSET, SLICE_PTR_OFFSET,
};
use super::super::indices::FunctionIndices;
use super::cells::CellLayout;
use super::emit::{FuncDispatch, SchemaLayouts};

// ─── WIT names referenced by lift codegen ─────────────────────────

/// Field names within `record enum-info`.
const ENUM_INFO_TYPE_NAME: &str = "type-name";
const ENUM_INFO_CASE_NAME: &str = "case-name";

// ─── Classification + lift descriptors ────────────────────────────

/// How a WIT type maps to a `cell` variant. Wired variants are
/// implemented end-to-end (lift codegen produces real cells);
/// un-wired variants (Phase 2-2b / 2-4) classify here without panic
/// but `todo!()` at the codegen layer (`cells.rs`) when actually
/// reached at adapter-build time.
#[derive(Clone, Copy)]
pub(super) enum LiftKind {
    // ── Phase 2-2a (wired) ────────────────────────────────────────
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

    // ── Phase 2-2b (todo!() in cells.rs) ─────────────────────────
    /// `char` → `cell::text` (utf-8 encode the i32 code point).
    Char,
    /// `list<T>` (non-u8 element) → `cell::list-of`.
    ListOf,
    /// `tuple<...>` → `cell::tuple-of`.
    TupleOf,
    /// `option<T>` → `cell::option-some(u32)` or `cell::option-none`.
    Option,
    /// `result<T, E>` → `cell::result-ok(option<u32>)` or `cell::result-err(option<u32>)`.
    Result,
    /// `record { ... }` → `cell::record-of(u32)` (side-table index).
    Record,
    /// `flags { ... }` → `cell::flags-set(u32)`.
    Flags,
    /// `enum { ... }` → `cell::enum-case(u32)`.
    Enum,
    /// `variant { ... }` → `cell::variant-case(u32)`.
    Variant,

    // ── Phase 2-4 (todo!() in cells.rs) ──────────────────────────
    /// `own<R>` / `borrow<R>` → `cell::resource-handle(u32)`.
    Handle,
    /// `future<T>` → `cell::future-handle(u32)`.
    Future,
    /// `stream<T>` → `cell::stream-handle(u32)`.
    Stream,

    // ── Future work ──────────────────────────────────────────────
    /// `error-context` — no cell variant yet; design TBD.
    ErrorContext,
}

impl LiftKind {
    /// Number of flat wasm slots this param consumes. Hard-coded
    /// for wired primitive kinds; `todo!()` for compound kinds
    /// because their flat-slot count depends on the inner type's
    /// canonical-ABI lowering — driving that off `wit-parser`'s flat
    /// representation lands alongside the actual lift codegen.
    pub(super) fn slot_count(self) -> u32 {
        match self {
            LiftKind::Bool
            | LiftKind::IntegerSignExt
            | LiftKind::IntegerZeroExt
            | LiftKind::Integer64
            | LiftKind::FloatingF32
            | LiftKind::FloatingF64 => 1,
            LiftKind::Text | LiftKind::Bytes => 2,
            LiftKind::Char => todo!("Phase 2-2b: char param slot_count = 1 (i32 code point)"),
            LiftKind::ListOf => todo!("Phase 2-2b: list<T> param slot_count = 2 (ptr, len)"),
            LiftKind::TupleOf => {
                todo!("Phase 2-2b: tuple param slot_count = sum of element flat-slot counts")
            }
            LiftKind::Option => {
                todo!("Phase 2-2b: option<T> param slot_count = 1 (disc) + flat(T)")
            }
            LiftKind::Result => {
                todo!(
                    "Phase 2-2b: result<T,E> param slot_count = 1 (disc) + max(flat(T), flat(E)) joined"
                )
            }
            LiftKind::Record => {
                todo!("Phase 2-2b: record param slot_count = sum of field flat-slot counts")
            }
            LiftKind::Flags => {
                todo!("Phase 2-2b: flags param slot_count = 1 (i32 unless > 32 flags, then more)")
            }
            // Enum lowers to a single i32 disc.
            LiftKind::Enum => 1,
            LiftKind::Variant => {
                todo!(
                    "Phase 2-2b: variant param slot_count = 1 (disc) + max-payload flat-slot count joined"
                )
            }
            LiftKind::Handle => todo!("Phase 2-4: handle param slot_count = 1 (i32 handle index)"),
            LiftKind::Future => todo!("Phase 2-4: future param slot_count = 1 (i32 future handle)"),
            LiftKind::Stream => todo!("Phase 2-4: stream param slot_count = 1 (i32 stream handle)"),
            LiftKind::ErrorContext => todo!("error-context param slot_count = 1 (i32)"),
        }
    }

    /// Classify a WIT param type. Infallible: every `Type` maps to a
    /// `LiftKind`. Codegen for un-wired variants `todo!()`s in
    /// `cells.rs` / `slot_count` when actually reached.
    pub(super) fn classify(ty: &Type, resolve: &Resolve) -> LiftKind {
        match ty {
            Type::Bool => LiftKind::Bool,
            Type::S8 | Type::S16 | Type::S32 => LiftKind::IntegerSignExt,
            Type::U8 | Type::U16 | Type::U32 => LiftKind::IntegerZeroExt,
            Type::S64 | Type::U64 => LiftKind::Integer64,
            Type::F32 => LiftKind::FloatingF32,
            Type::F64 => LiftKind::FloatingF64,
            Type::String => LiftKind::Text,
            Type::Char => LiftKind::Char,
            Type::ErrorContext => LiftKind::ErrorContext,
            Type::Id(id) => match &resolve.types[*id].kind {
                wit_parser::TypeDefKind::List(Type::U8) => LiftKind::Bytes,
                wit_parser::TypeDefKind::List(_) => LiftKind::ListOf,
                wit_parser::TypeDefKind::Tuple(_) => LiftKind::TupleOf,
                wit_parser::TypeDefKind::Record(_) => LiftKind::Record,
                wit_parser::TypeDefKind::Variant(_) => LiftKind::Variant,
                wit_parser::TypeDefKind::Enum(_) => LiftKind::Enum,
                wit_parser::TypeDefKind::Flags(_) => LiftKind::Flags,
                wit_parser::TypeDefKind::Option(_) => LiftKind::Option,
                wit_parser::TypeDefKind::Result(_) => LiftKind::Result,
                wit_parser::TypeDefKind::Handle(_) => LiftKind::Handle,
                wit_parser::TypeDefKind::Future(_) => LiftKind::Future,
                wit_parser::TypeDefKind::Stream(_) => LiftKind::Stream,
                // Type aliases peel through and reclassify the
                // underlying type.
                wit_parser::TypeDefKind::Type(t) => LiftKind::classify(t, resolve),
                wit_parser::TypeDefKind::FixedLengthList(_, _)
                | wit_parser::TypeDefKind::Map(_, _)
                | wit_parser::TypeDefKind::Resource
                | wit_parser::TypeDefKind::Unknown => {
                    todo!(
                        "tier-2 lift: unsupported TypeDefKind {:?}",
                        &resolve.types[*id].kind
                    )
                }
            },
        }
    }
}

/// How to extract the function's return value when lifting it for
/// on-return. `side_table` populates the result tree's side-tables
/// (enum-infos, flags-infos, …) at adapter-build time.
pub(super) struct ResultLift {
    pub source: ResultSource,
    pub side_table: SideTableInfo,
}

#[derive(Clone, Copy)]
pub(super) enum ResultSource {
    /// Direct primitive (no retptr): source is the captured
    /// result_local — emit_code_section resolves the actual local idx.
    Direct(LiftKind),
    /// `(ptr, len)` pair in retptr scratch (string / `list<u8>`).
    RetptrPair { kind: LiftKind, retptr_offset: i32 },
}

impl ResultLift {
    /// Re-anchor the retptr scratch offset back-filled by the layout
    /// phase. No-op for `Direct` results.
    pub(super) fn set_retptr_offset(&mut self, off: i32) {
        if let ResultSource::RetptrPair { retptr_offset, .. } = &mut self.source {
            *retptr_offset = off;
        }
    }
}

/// Per-parameter lift recipe. `first_local` is the wasm local index
/// of the first flat slot for this param (subsequent slots for
/// multi-slot params live at +1, +2, ...). `name_offset` /
/// `name_len` reference the param name in the shared name blob.
pub(super) struct ParamLift {
    pub name_offset: i32,
    pub name_len: i32,
    pub kind: LiftKind,
    pub first_local: u32,
    /// Schema-level side-table contributions populated at classify
    /// time. Empty (`Default::default()`) for primitive params.
    pub side_table: SideTableInfo,
}

/// Schema-level info needed to populate one field-tree's side
/// tables (enum-infos, flags-infos, record-infos, …). Per
/// (param | result), built at classify time and consumed by the
/// layout phase to emit the side-table data segments + patch the
/// field-tree blobs.
#[derive(Default, Clone)]
pub(super) struct SideTableInfo {
    /// `Some` for enum-typed lifts: carries the enum's type-name
    /// plus its case names in disc order.
    pub enum_info: Option<EnumLiftInfo>,
}

#[derive(Clone)]
pub(super) struct EnumLiftInfo {
    pub type_name: String,
    /// Case names in WIT declaration order — the i'th entry is the
    /// name of the case with disc value `i`.
    pub case_names: Vec<String>,
}

// ─── Classifiers ──────────────────────────────────────────────────

pub(super) fn classify_func_params(
    resolve: &Resolve,
    func: &WitFunction,
    name_blob: &mut Vec<u8>,
) -> Vec<ParamLift> {
    let mut params_lift: Vec<ParamLift> = Vec::with_capacity(func.params.len());
    let mut slot_cursor: u32 = 0;
    for param in &func.params {
        let pname = &param.name;
        let kind = LiftKind::classify(&param.ty, resolve);
        let side_table = side_table_info_for(&param.ty, kind, resolve);
        let name_offset = name_blob.len() as i32;
        let name_len = pname.len() as i32;
        name_blob.extend_from_slice(pname.as_bytes());
        params_lift.push(ParamLift {
            name_offset,
            name_len,
            kind,
            first_local: slot_cursor,
            side_table,
        });
        slot_cursor += kind.slot_count();
    }
    params_lift
}

/// Classify the function's return value for on-return lift. Direct
/// primitive returns capture into `result_local`; string / `list<u8>`
/// returns ride retptr. Compound returns route through `LiftKind`
/// variants whose codegen `todo!()`s in `cells.rs` — building an
/// adapter for a record-returning interface panics with a precise
/// message at adapter-build time.
///
/// For async funcs canon-lower-async always retptr's a non-void
/// result, so even primitive results live at the retptr scratch.
/// Returns `None` only for void functions.
pub(super) fn classify_result_lift(
    resolve: &Resolve,
    func: &WitFunction,
    export_sig: &WasmSignature,
    import_sig: &WasmSignature,
    is_async: bool,
) -> Option<ResultLift> {
    let ty = func.result.as_ref()?;
    let kind = LiftKind::classify(ty, resolve);
    let side_table = side_table_info_for(ty, kind, resolve);
    let result_at_retptr = if is_async {
        import_sig.retptr
    } else {
        export_sig.retptr
    };
    let source = if result_at_retptr {
        ResultSource::RetptrPair {
            kind,
            retptr_offset: 0, // back-filled by the layout phase.
        }
    } else {
        ResultSource::Direct(kind)
    };
    Some(ResultLift { source, side_table })
}

/// Build the `SideTableInfo` for a (type, kind) pair. Empty for
/// primitive lifts; populated for compound lifts that need
/// per-tree side-table entries (currently only enum).
fn side_table_info_for(ty: &Type, kind: LiftKind, resolve: &Resolve) -> SideTableInfo {
    let mut info = SideTableInfo::default();
    if matches!(kind, LiftKind::Enum) {
        info.enum_info = enum_lift_info_for_type(ty, resolve);
    }
    info
}

/// Extract `(type-name, case-names)` from an enum-typed `Type::Id`.
/// Returns `None` if the type isn't an enum or lacks a name (the
/// canonical-ABI lower has the disc but the cell can't render
/// without case-names).
fn enum_lift_info_for_type(ty: &Type, resolve: &Resolve) -> Option<EnumLiftInfo> {
    let Type::Id(id) = ty else {
        return None;
    };
    let typedef = &resolve.types[*id];
    let wit_parser::TypeDefKind::Enum(e) = &typedef.kind else {
        return None;
    };
    let type_name = typedef.name.as_ref()?.clone();
    let case_names: Vec<String> = e.cases.iter().map(|c| c.name.clone()).collect();
    Some(EnumLiftInfo {
        type_name,
        case_names,
    })
}

// ─── Side-table population ────────────────────────────────────────

/// Where each enum type's strings live in the name blob, after
/// `register_enum_strings`. Keyed by enum type-name to dedupe across
/// multiple uses of the same enum across params/results/fns.
pub(super) type EnumStringTable = HashMap<String, EnumStringOffsets>;

pub(super) struct EnumStringOffsets {
    type_name: (u32, u32),  // (offset, len) in name_blob
    cases: Vec<(u32, u32)>, // per case, in disc order
}

/// Walk every param / result; for each enum-typed lift, append its
/// type-name + case-names to `name_blob` (deduped per enum type).
/// Returns the per-enum-type string offsets so the side-table builder
/// can stitch entries together without re-scanning name_blob.
pub(super) fn register_enum_strings(
    per_func: &[FuncDispatch],
    name_blob: &mut Vec<u8>,
) -> EnumStringTable {
    let mut table = EnumStringTable::new();
    for fd in per_func {
        for p in &fd.params {
            if let Some(ei) = &p.side_table.enum_info {
                ensure_enum_registered(&mut table, name_blob, ei);
            }
        }
        if let Some(rl) = &fd.result_lift {
            if let Some(ei) = &rl.side_table.enum_info {
                ensure_enum_registered(&mut table, name_blob, ei);
            }
        }
    }
    table
}

fn ensure_enum_registered(
    table: &mut EnumStringTable,
    name_blob: &mut Vec<u8>,
    ei: &EnumLiftInfo,
) {
    if table.contains_key(&ei.type_name) {
        return;
    }
    let type_name_off = name_blob.len() as u32;
    name_blob.extend_from_slice(ei.type_name.as_bytes());
    let type_name = (type_name_off, ei.type_name.len() as u32);
    let cases = ei
        .case_names
        .iter()
        .map(|n| {
            let off = name_blob.len() as u32;
            name_blob.extend_from_slice(n.as_bytes());
            (off, n.len() as u32)
        })
        .collect();
    table.insert(
        ei.type_name.clone(),
        EnumStringOffsets { type_name, cases },
    );
}

/// `(blob_offset, count)` per (fn, param) and (fn, result), with
/// `blob_offset` initially relative to the enum-info data segment's
/// start; the caller translates to absolute after `place_data`.
pub(super) struct EnumInfoBlob {
    pub bytes: Vec<u8>,
    pub per_param: Vec<Vec<(u32, u32)>>,
    pub per_result: Vec<(u32, u32)>,
}

/// Lay out the per-(field-tree) enum-info entries: one
/// `enum-info` record per case of the enum a field carries. The
/// cell at runtime stores the disc as the side-table index, so the
/// disc directly indexes into this blob's contiguous case range.
pub(super) fn build_enum_info_blob(
    per_func: &[FuncDispatch],
    strings: &EnumStringTable,
    schema: &SchemaLayouts,
) -> EnumInfoBlob {
    let entry_size = schema.enum_info_layout.size as usize;
    let type_name_off = schema.enum_info_layout.offset_of(ENUM_INFO_TYPE_NAME) as usize;
    let case_name_off = schema.enum_info_layout.offset_of(ENUM_INFO_CASE_NAME) as usize;

    let mut bytes: Vec<u8> = Vec::new();
    let mut per_param: Vec<Vec<(u32, u32)>> = Vec::with_capacity(per_func.len());
    let mut per_result: Vec<(u32, u32)> = Vec::with_capacity(per_func.len());
    for fd in per_func {
        let mut params_off = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            params_off.push(append_enum_entries(
                &mut bytes,
                strings,
                p.side_table.enum_info.as_ref(),
                entry_size,
                type_name_off,
                case_name_off,
            ));
        }
        per_param.push(params_off);
        let result_ei = fd
            .result_lift
            .as_ref()
            .and_then(|r| r.side_table.enum_info.as_ref());
        per_result.push(append_enum_entries(
            &mut bytes,
            strings,
            result_ei,
            entry_size,
            type_name_off,
            case_name_off,
        ));
    }
    EnumInfoBlob {
        bytes,
        per_param,
        per_result,
    }
}

fn append_enum_entries(
    blob: &mut Vec<u8>,
    strings: &EnumStringTable,
    ei: Option<&EnumLiftInfo>,
    entry_size: usize,
    type_name_off: usize,
    case_name_off: usize,
) -> (u32, u32) {
    let Some(ei) = ei else {
        return (0, 0);
    };
    let s = strings
        .get(&ei.type_name)
        .expect("register_enum_strings ran for every enum_info");
    let blob_off = blob.len() as u32;
    let count = ei.case_names.len() as u32;
    for case_idx in 0..ei.case_names.len() {
        let entry_start = blob.len();
        blob.extend(std::iter::repeat_n(0u8, entry_size));
        let (tn_off, tn_len) = s.type_name;
        let (cn_off, cn_len) = s.cases[case_idx];
        write_le_i32(
            blob,
            entry_start + type_name_off + SLICE_PTR_OFFSET as usize,
            tn_off as i32,
        );
        write_le_i32(
            blob,
            entry_start + type_name_off + SLICE_LEN_OFFSET as usize,
            tn_len as i32,
        );
        write_le_i32(
            blob,
            entry_start + case_name_off + SLICE_PTR_OFFSET as usize,
            cn_off as i32,
        );
        write_le_i32(
            blob,
            entry_start + case_name_off + SLICE_LEN_OFFSET as usize,
            cn_len as i32,
        );
    }
    (blob_off, count)
}

/// Side-table-segment-local helper. Mirrors the one in `emit.rs`;
/// duplicated to avoid widening that module's public surface for
/// what's a 3-line write helper.
fn write_le_i32(buf: &mut [u8], offset: usize, value: i32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

// ─── Wrapper-body locals + lift codegen ───────────────────────────

/// Locals used by the wrapper body. Allocated once up front so all
/// downstream emit phases (param lift, hook calls, result lift, async
/// task.return load) reference the same indices.
pub(super) struct WrapperLocals {
    /// Scratch for the cell write address.
    pub addr: u32,
    /// Packed status from canon-async hook calls.
    pub st: u32,
    /// Waitable-set handle for the wait loop.
    pub ws: u32,
    /// Retptr-loaded ptr for Text/Bytes result lift.
    pub ptr_scratch: u32,
    /// Retptr-loaded len for Text/Bytes result lift.
    pub len_scratch: u32,
    /// i64 widening source for IntegerSignExt/ZeroExt.
    pub ext64: u32,
    /// f64 promoted source for FloatingF32.
    pub ext_f64: u32,
    /// Direct-return value when the export sig has a single flat
    /// result; `None` otherwise.
    pub result: Option<u32>,
    /// Address local that drives `lift_from_memory` for async
    /// `task.return` flat loads. `None` for sync, void async, and
    /// async with retptr-passthrough task.return.
    pub tr_addr: Option<u32>,
}

pub(super) fn alloc_wrapper_locals(
    locals: &mut FunctionIndices,
    fd: &FuncDispatch,
) -> WrapperLocals {
    let addr = locals.alloc_local(ValType::I32);
    let st = locals.alloc_local(ValType::I32);
    let ws = locals.alloc_local(ValType::I32);
    let ptr_scratch = locals.alloc_local(ValType::I32);
    let len_scratch = locals.alloc_local(ValType::I32);
    let ext64 = locals.alloc_local(ValType::I64);
    let ext_f64 = locals.alloc_local(ValType::F64);
    let result = direct_return_type(&fd.export_sig).map(|t| locals.alloc_local(t));
    // Async with a non-retptr-passthrough task.return needs an
    // i32 addr local so `lift_from_memory` can flat-load result
    // values out of the retptr scratch.
    let tr_uses_flat_loads = fd
        .task_return
        .as_ref()
        .is_some_and(|tr| !tr.sig.indirect_params && fd.result_ty.is_some());
    let tr_addr = tr_uses_flat_loads.then(|| locals.alloc_local(ValType::I32));
    WrapperLocals {
        addr,
        st,
        ws,
        ptr_scratch,
        len_scratch,
        ext64,
        ext_f64,
        result,
        tr_addr,
    }
}

/// Emit the wasm to lift one param into the cell at `lcl.addr`.
pub(super) fn emit_lift_param(
    f: &mut Function,
    cell_layout: &CellLayout,
    p: &ParamLift,
    lcl: &WrapperLocals,
) {
    emit_lift_kind(f, cell_layout, p.kind, p.first_local, p.first_local + 1, lcl);
}

/// Shared lift body for params and direct-return results. `slot0` /
/// `slot1` are wasm locals carrying the source value(s); for single-
/// slot kinds only `slot0` is used. Multi-slot kinds (Text/Bytes)
/// expect `(ptr, len)` in (slot0, slot1).
fn emit_lift_kind(
    f: &mut Function,
    cell_layout: &CellLayout,
    kind: LiftKind,
    slot0: u32,
    slot1: u32,
    lcl: &WrapperLocals,
) {
    let addr = lcl.addr;
    match kind {
        // ── Wired primitives ─────────────────────────────────────
        LiftKind::Bool => cell_layout.emit_bool(f, addr, slot0),
        LiftKind::IntegerSignExt => {
            f.instructions().local_get(slot0);
            f.instructions().i64_extend_i32_s();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        LiftKind::IntegerZeroExt => {
            f.instructions().local_get(slot0);
            f.instructions().i64_extend_i32_u();
            f.instructions().local_set(lcl.ext64);
            cell_layout.emit_integer(f, addr, lcl.ext64);
        }
        LiftKind::Integer64 => cell_layout.emit_integer(f, addr, slot0),
        LiftKind::FloatingF32 => {
            f.instructions().local_get(slot0);
            f.instructions().f64_promote_f32();
            f.instructions().local_set(lcl.ext_f64);
            cell_layout.emit_floating(f, addr, lcl.ext_f64);
        }
        LiftKind::FloatingF64 => cell_layout.emit_floating(f, addr, slot0),
        LiftKind::Text => cell_layout.emit_text(f, addr, slot0, slot1),
        LiftKind::Bytes => cell_layout.emit_bytes(f, addr, slot0, slot1),

        // ── Direct one-shot dispatch (single cells.rs call) ──
        LiftKind::Char => cell_layout.emit_char(f, addr, slot0),
        LiftKind::Record => cell_layout.emit_record_of(f, addr, slot0),
        LiftKind::Flags => cell_layout.emit_flags_set(f, addr, slot0),
        LiftKind::Enum => cell_layout.emit_enum_case(f, addr, slot0),
        LiftKind::Handle => cell_layout.emit_resource_handle(f, addr, slot0),
        LiftKind::Future => cell_layout.emit_future_handle(f, addr, slot0),
        LiftKind::Stream => cell_layout.emit_stream_handle(f, addr, slot0),

        // ── Multi-cell or runtime-disc dispatch — orchestration
        // belongs HERE, not at the cells.rs level. Each todo!()
        // names what the implementer needs to wire.
        LiftKind::ListOf => {
            let _ = (slot0, slot1);
            todo!(
                "Phase 2-2b: list<T> lift — recurse on element type, allocate a u32-index \
                 array, populate with child cell indices, then `cell_layout.emit_list_of(...)`"
            )
        }
        LiftKind::TupleOf => todo!(
            "Phase 2-2b: tuple lift — same shape as list, but child indices come from \
             per-element classification + lift, no element recursion"
        ),
        LiftKind::Option => todo!(
            "Phase 2-2b: option<T> lift — read disc; if some, recurse on inner + \
             `emit_option_some`; if none, `emit_option_none`"
        ),
        LiftKind::Result => todo!(
            "Phase 2-2b: result<T,E> lift — read disc; for ok/err, recurse on payload + \
             `emit_result_ok` / `emit_result_err`"
        ),
        LiftKind::Variant => todo!(
            "Phase 2-2b: variant lift — read disc; per-case payload classify + lift; \
             populate variant-info side table; `emit_variant_case(side_table_idx)`"
        ),
        LiftKind::ErrorContext => todo!("error-context lift — design TBD"),
    }
}

/// Emit the wasm to lift one return value into the cell at `addr_local`.
/// Direct primitive returns read from `result_local`; Text/Bytes
/// returns load `(ptr, len)` from the retptr scratch into `ptr_scratch`
/// / `len_scratch` and lift those.
pub(super) fn emit_lift_result(
    f: &mut Function,
    cell_layout: &CellLayout,
    source: ResultSource,
    lcl: &WrapperLocals,
) {
    match source {
        ResultSource::Direct(kind) => {
            let local = lcl
                .result
                .expect("ResultSource::Direct → result local must be allocated");
            emit_lift_kind(f, cell_layout, kind, local, local, lcl);
        }
        ResultSource::RetptrPair {
            kind,
            retptr_offset,
        } => {
            f.instructions().i32_const(retptr_offset);
            f.instructions().i32_load(MemArg {
                offset: SLICE_PTR_OFFSET as u64,
                align: 2,
                memory_index: 0,
            });
            f.instructions().local_set(lcl.ptr_scratch);
            f.instructions().i32_const(retptr_offset);
            f.instructions().i32_load(MemArg {
                offset: SLICE_LEN_OFFSET as u64,
                align: 2,
                memory_index: 0,
            });
            f.instructions().local_set(lcl.len_scratch);
            emit_lift_kind(f, cell_layout, kind, lcl.ptr_scratch, lcl.len_scratch, lcl);
        }
    }
}

/// `RecordLayout` is re-exported here so callers can keep `lift::*`
/// imports tight without dragging in `abi::emit::*`. It's the type
/// `SchemaLayouts::enum_info_layout` exposes; `build_enum_info_blob`
/// reads its `offset_of(...)`.
#[allow(dead_code)]
pub(super) type EnumInfoLayout = RecordLayout;
