//! Wasm section emitters for the dispatch core module: types,
//! imports + function declarations, exports, code, data. The
//! per-wrapper body lives in [`super::wrapper_body`]; this file
//! drives the section-level structure that surrounds it.

use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, FunctionSection,
    ImportSection, Module, TypeSection, ValType,
};
use wit_parser::abi::WasmSignature;

use super::super::abi::canon_async;
use super::super::abi::emit::{
    emit_cabi_realloc, empty_function, val_types, EXPORT_CABI_REALLOC, EXPORT_INITIALIZE,
    EXPORT_MEMORY,
};
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
}

pub(super) fn emit_imports_and_funcs(
    module: &mut Module,
    per_func: &[FuncDispatch],
    ty: &TypeIndices,
    before_hook: Option<&HookImport>,
    after_hook: Option<&HookImport>,
    event_ptr: i32,
) -> FuncIndices {
    let mut imports = ImportSection::new();
    let mut next_imp: u32 = 0;

    let handler_imp_base = next_imp;
    for (fd, &fty) in per_func.iter().zip(&ty.handler_ty) {
        imports.import(
            &fd.import_module,
            &fd.import_field,
            EntityType::Function(fty),
        );
        next_imp += 1;
    }

    let before_hook_idx = before_hook.map(|h| {
        imports.import(
            &h.module,
            &h.name,
            EntityType::Function(ty.before_hook_ty.unwrap()),
        );
        let idx = next_imp;
        next_imp += 1;
        idx
    });
    let after_hook_idx = after_hook.map(|h| {
        imports.import(
            &h.module,
            &h.name,
            EntityType::Function(ty.after_hook_ty.unwrap()),
        );
        let idx = next_imp;
        next_imp += 1;
        idx
    });

    let async_funcs =
        canon_async::import_intrinsics(&mut imports, &ty.async_types, event_ptr, || {
            let i = next_imp;
            next_imp += 1;
            i
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
            task_return_idx[i] = Some(next_imp);
            next_imp += 1;
        }
    }

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
        before_hook_idx,
        after_hook_idx,
        async_funcs,
        task_return_idx,
        wrapper_base,
        init_idx,
        cabi_realloc_idx,
    }
}

pub(super) fn emit_export_section(
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

pub(super) fn emit_code_section(
    module: &mut Module,
    per_func: &[FuncDispatch],
    func_idx: &FuncIndices,
    ctx: &WrapperCtx<'_>,
) {
    let mut code = CodeSection::new();
    for (i, fd) in per_func.iter().enumerate() {
        emit_wrapper_function(&mut code, func_idx, ctx, i, fd);
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

pub(super) fn emit_data_section(module: &mut Module, segments: &[(u32, Vec<u8>)]) {
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
