//! Canonical-ABI async machinery shared between tier-1 and tier-2
//! adapter dispatch modules. wit-component injects five
//! `$root/[…]` builtins into any wasm core module that imports an
//! async function; this module owns:
//!
//! - The well-known intrinsic names (must match wit-component's
//!   contract — see `wit-component::dummy::push_root_async_intrinsics`).
//! - Type-section emission for the four signatures the intrinsics use.
//! - Imports-section emission that allocates the five function indices.
//! - The wait-loop emit helper that awaits a packed
//!   `canon lower async` status (the `(handle << 4) | status_tag`
//!   value every async-lowered import returns).

use wasm_encoder::{BlockType, EntityType, Function, ImportSection, TypeSection, ValType};

/// wit-component routes async runtime intrinsics through this
/// module name; not configurable.
pub(crate) const INTRINSIC_MODULE: &str = "$root";

const WAITABLE_SET_NEW: &str = "[waitable-set-new]";
const WAITABLE_JOIN: &str = "[waitable-join]";
const WAITABLE_SET_WAIT: &str = "[waitable-set-wait]";
const WAITABLE_SET_DROP: &str = "[waitable-set-drop]";
const SUBTASK_DROP: &str = "[subtask-drop]";

/// Type-section indices for the four signatures the async intrinsics
/// use. `void_i32_ty` is shared by `[waitable-set-drop]` and
/// `[subtask-drop]` (both `(i32) -> ()`).
pub(crate) struct AsyncTypes {
    pub waitable_new_ty: u32,
    pub waitable_join_ty: u32,
    pub waitable_wait_ty: u32,
    pub void_i32_ty: u32,
}

/// Function-index handles for the five async intrinsics, plus the
/// byte offset of the event scratch slot `[waitable-set-wait]`
/// writes into.
pub(crate) struct AsyncFuncs {
    pub waitable_new: u32,
    pub waitable_join: u32,
    pub waitable_wait: u32,
    pub waitable_drop: u32,
    pub subtask_drop: u32,
    pub event_ptr: i32,
}

/// Emit the four function types into `types`. `alloc_ty` is the
/// caller's "next type index" cursor — invoked once per added type
/// so callers don't need to know the local order.
pub(crate) fn emit_types(types: &mut TypeSection, mut alloc_ty: impl FnMut() -> u32) -> AsyncTypes {
    types.ty().function([], [ValType::I32]);
    let waitable_new_ty = alloc_ty();
    types.ty().function([ValType::I32, ValType::I32], []);
    let waitable_join_ty = alloc_ty();
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);
    let waitable_wait_ty = alloc_ty();
    types.ty().function([ValType::I32], []);
    let void_i32_ty = alloc_ty();
    AsyncTypes {
        waitable_new_ty,
        waitable_join_ty,
        waitable_wait_ty,
        void_i32_ty,
    }
}

/// Add the five intrinsic imports under `$root` and return their
/// allocated function indices. `alloc_func` is the caller's
/// "next function index" cursor.
pub(crate) fn import_intrinsics(
    imports: &mut ImportSection,
    types: &AsyncTypes,
    event_ptr: i32,
    mut alloc_func: impl FnMut() -> u32,
) -> AsyncFuncs {
    let mut import = |name: &str, ty: u32| -> u32 {
        imports.import(INTRINSIC_MODULE, name, EntityType::Function(ty));
        alloc_func()
    };
    let waitable_new = import(WAITABLE_SET_NEW, types.waitable_new_ty);
    let waitable_join = import(WAITABLE_JOIN, types.waitable_join_ty);
    let waitable_wait = import(WAITABLE_SET_WAIT, types.waitable_wait_ty);
    let waitable_drop = import(WAITABLE_SET_DROP, types.void_i32_ty);
    let subtask_drop = import(SUBTASK_DROP, types.void_i32_ty);
    AsyncFuncs {
        waitable_new,
        waitable_join,
        waitable_wait,
        waitable_drop,
        subtask_drop,
        event_ptr,
    }
}

/// Issue an already-set-up `canon lower async` call (params already
/// on the wasm stack) and await its completion: `call hook_idx;
/// local.set st; emit_wait_loop`. The shared tail every tier uses
/// once params are pushed (whether direct flat params or a single
/// indirect-params pointer).
pub(crate) fn emit_call_and_wait(
    f: &mut Function,
    hook_idx: u32,
    st: u32,
    ws: u32,
    art: &AsyncFuncs,
) {
    f.instructions().call(hook_idx);
    f.instructions().local_set(st);
    emit_wait_loop(f, st, ws, art);
}

/// Await a packed `canon lower async` status sitting in local `st`.
/// The packed `i32` is `(handle << 4) | status_tag` (tag 1=Started,
/// 2=Returned). After this helper:
/// - `st` holds the raw subtask handle (or 0 if the call already
///   returned synchronously).
/// - If non-zero, the subtask has been joined into a fresh
///   waitable-set, waited on once, and both handles dropped.
///
/// `ws` is a scratch i32 local for the waitable-set handle; caller
/// allocates it.
pub(crate) fn emit_wait_loop(f: &mut Function, st: u32, ws: u32, art: &AsyncFuncs) {
    f.instructions().local_get(st);
    f.instructions().i32_const(4);
    f.instructions().i32_shr_u();
    f.instructions().local_set(st);
    f.instructions().local_get(st);
    f.instructions().if_(BlockType::Empty);
    f.instructions().call(art.waitable_new);
    f.instructions().local_set(ws);
    f.instructions().local_get(st);
    f.instructions().local_get(ws);
    f.instructions().call(art.waitable_join);
    f.instructions().local_get(ws);
    f.instructions().i32_const(art.event_ptr);
    f.instructions().call(art.waitable_wait);
    f.instructions().drop();
    f.instructions().local_get(st);
    f.instructions().call(art.subtask_drop);
    f.instructions().local_get(ws);
    f.instructions().call(art.waitable_drop);
    f.instructions().end();
}
