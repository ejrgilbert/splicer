//! Wasm section emitters for the dispatch core module: types,
//! imports + function declarations, exports, code, data. The
//! per-wrapper body lives in [`super::wrapper_body`]; this file
//! drives the section-level structure that surrounds it.

use std::collections::HashMap;

use wasm_encoder::{
    CodeSection, EntityType, FunctionSection, ImportSection, Module, TypeSection, ValType,
};
use wit_parser::abi::WasmSignature;
use wit_parser::{Function as WitFunction, Resolve, TypeId};

use super::super::abi::canon_async;
use super::super::abi::emit::{
    emit_cabi_realloc, emit_resource_drop_imports, empty_function, val_types, GlobalIndices,
    WrapperExport,
};
use super::super::indices::DispatchIndices;
use super::schema::HookImport;
use super::wrapper_body::{emit_wrapper_function, WrapperCtx};
use super::FuncDispatch;

pub(super) struct TypeIndices {
    pub(super) handler_ty: Vec<u32>,
    pub(super) wrapper_ty: Vec<u32>,
    pub(super) before_hook_ty: Option<u32>,
    pub(super) after_hook_ty: Option<u32>,
    pub(super) init_ty: u32,
    pub(super) cabi_post_ty: u32,
    pub(super) cabi_realloc_ty: u32,
    pub(super) async_types: canon_async::AsyncTypes,
    /// Per-async-func `task.return` type; `Some(idx)` iff `is_async`.
    pub(super) task_return_ty: Vec<Option<u32>>,
    /// `(func (param i32))` for `[resource-drop]<R>` imports.
    /// `Some` iff any per-func `borrow_drops` is non-empty.
    pub(super) resource_drop_ty: Option<u32>,
}

pub(super) fn emit_type_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
    before_hook_sig: Option<&WasmSignature>,
    after_hook_sig: Option<&WasmSignature>,
) -> TypeIndices {
    let mut types = TypeSection::new();
    let mut next_ty: u32 = 0;
    let mut alloc_one =
        |ty_section: &mut TypeSection, params: Vec<ValType>, results: Vec<ValType>| -> u32 {
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
                val_types(&fd.import_sig.params),
                val_types(&fd.import_sig.results),
            )
        })
        .collect();
    let wrapper_ty: Vec<u32> = per_func
        .iter()
        .map(|fd| {
            alloc_one(
                &mut types,
                val_types(&fd.export_sig.params),
                val_types(&fd.export_sig.results),
            )
        })
        .collect();

    let before_hook_ty = before_hook_sig
        .map(|sig| alloc_one(&mut types, val_types(&sig.params), val_types(&sig.results)));
    let after_hook_ty = after_hook_sig
        .map(|sig| alloc_one(&mut types, val_types(&sig.params), val_types(&sig.results)));
    let init_ty = alloc_one(&mut types, vec![], vec![]);
    let cabi_post_ty = alloc_one(&mut types, vec![ValType::I32], vec![]);
    let cabi_realloc_ty = alloc_one(
        &mut types,
        vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        vec![ValType::I32],
    );

    // Per-async-fn `task.return` types. Allocated BEFORE the canon-
    // async runtime types so `alloc_one` (which captures `next_ty`)
    // is still the sole borrower; the runtime-types closure also
    // captures `next_ty` and the borrow checker rejects two
    // simultaneous mutable captures.
    let task_return_ty: Vec<Option<u32>> = per_func
        .iter()
        .map(|fd| {
            fd.shape.task_return().map(|tr| {
                alloc_one(
                    &mut types,
                    val_types(&tr.sig.params),
                    val_types(&tr.sig.results),
                )
            })
        })
        .collect();

    let async_types = canon_async::emit_types(&mut types, || {
        let i = next_ty;
        next_ty += 1;
        i
    });

    // `[resource-drop]<R>`: `(func (param i32))`. Reuses the canon-
    // async runtime's `void_i32_ty` slot — same shape, always
    // allocated since tier-2 always emits the async runtime.
    let needs_resource_drop = per_func.iter().any(|fd| !fd.borrow_drops.is_empty());
    let resource_drop_ty = needs_resource_drop.then_some(async_types.void_i32_ty);

    module.section(&types);
    TypeIndices {
        handler_ty,
        wrapper_ty,
        before_hook_ty,
        after_hook_ty,
        init_ty,
        cabi_post_ty,
        cabi_realloc_ty,
        async_types,
        task_return_ty,
        resource_drop_ty,
    }
}

pub(super) struct FuncIndices {
    pub(super) handler_imp_base: u32,
    pub(super) before_hook_idx: Option<u32>,
    pub(super) after_hook_idx: Option<u32>,
    pub(super) async_funcs: canon_async::AsyncFuncs,
    /// Per-async-func `task.return` import index.
    pub(super) task_return_idx: Vec<Option<u32>>,
    pub(super) wrapper_base: u32,
    pub(super) init_idx: u32,
    pub(super) cabi_realloc_idx: u32,
    /// Per-resource `[resource-drop]<R>` import index. Empty when no
    /// borrow params reference any resource.
    pub(super) resource_drop: HashMap<TypeId, u32>,
}

pub(super) fn emit_imports_and_funcs(
    module: &mut Module,
    resolve: &Resolve,
    per_func: &[FuncDispatch],
    ty: &TypeIndices,
    before_hook: Option<&HookImport>,
    after_hook: Option<&HookImport>,
    event_ptr: i32,
) -> FuncIndices {
    let mut imports = ImportSection::new();
    // `idx` tracks both the import-side func indices and the
    // wrapper-side ones (imports come first, then defined funcs);
    // shared across all import-emit paths so each `alloc_func()`
    // hands back the next contiguous slot.
    let mut idx = DispatchIndices::new();

    let handler_imp_base = idx.func;
    for (fd, &fty) in per_func.iter().zip(&ty.handler_ty) {
        imports.import(
            &fd.import_module,
            &fd.import_field,
            EntityType::Function(fty),
        );
        idx.alloc_func();
    }

    // `[resource-drop]<R>` imports — one per unique borrow resource.
    let resource_drop = emit_resource_drop_imports(
        &mut imports,
        resolve,
        per_func,
        |fd| &fd.borrow_drops,
        ty.resource_drop_ty,
        || idx.alloc_func(),
    );

    let before_hook_idx = before_hook.map(|h| {
        imports.import(
            &h.module,
            &h.name,
            EntityType::Function(ty.before_hook_ty.unwrap()),
        );
        idx.alloc_func()
    });
    let after_hook_idx = after_hook.map(|h| {
        imports.import(
            &h.module,
            &h.name,
            EntityType::Function(ty.after_hook_ty.unwrap()),
        );
        idx.alloc_func()
    });

    let async_funcs =
        canon_async::import_intrinsics(&mut imports, &ty.async_types, event_ptr, || {
            idx.alloc_func()
        });

    // Per-async-fn `task.return` imports. Mirrors tier-1's order:
    // imports come AFTER the canon-async runtime intrinsics. `Some`
    // iff the func is async.
    let mut task_return_idx: Vec<Option<u32>> = vec![None; per_func.len()];
    for (i, fd) in per_func.iter().enumerate() {
        if let Some(tr) = fd.shape.task_return() {
            let ty_idx = ty
                .task_return_ty
                .get(i)
                .copied()
                .flatten()
                .expect("async func must have task.return type allocated");
            imports.import(&tr.module, &tr.name, EntityType::Function(ty_idx));
            task_return_idx[i] = Some(idx.alloc_func());
        }
    }

    module.section(&imports);

    let wrapper_base = idx.func;

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
        before_hook_idx,
        after_hook_idx,
        async_funcs,
        task_return_idx,
        wrapper_base,
        init_idx,
        cabi_realloc_idx,
        resource_drop,
    }
}

/// Build the wrapper-export descriptor list. `cabi_post_*` shims are
/// declared right after `_initialize` (`init_idx + 1`), one per
/// wrapper that needs one, in `per_func` order — so we walk `per_func`
/// and bump a running index for each shim we emit.
pub(super) fn wrapper_exports<'a>(
    per_func: &'a [FuncDispatch],
    init_idx: u32,
) -> Vec<WrapperExport<'a>> {
    let mut next_post_idx = init_idx + 1;
    per_func
        .iter()
        .map(|fd| {
            let cabi_post_idx = fd.needs_cabi_post.then(|| {
                let idx = next_post_idx;
                next_post_idx += 1;
                idx
            });
            WrapperExport {
                export_name: &fd.export_name,
                cabi_post_idx,
            }
        })
        .collect()
}

pub(super) fn emit_code_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
    funcs: &[&WitFunction],
    func_idx: &FuncIndices,
    ctx: &WrapperCtx<'_>,
    globals: &GlobalIndices,
) {
    debug_assert_eq!(
        per_func.len(),
        funcs.len(),
        "FuncDispatch list and WitFunction list must be index-aligned",
    );
    let mut code = CodeSection::new();
    for (i, fd) in per_func.iter().enumerate() {
        emit_wrapper_function(&mut code, func_idx, ctx, i, fd, funcs[i]);
    }
    code.function(&empty_function());
    for fd in per_func {
        if fd.needs_cabi_post {
            code.function(&empty_function());
        }
    }
    emit_cabi_realloc(&mut code, globals.bump);
    module.section(&code);
}
