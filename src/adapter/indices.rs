//! Running index allocators for the wasm namespaces the adapter
//! emits into. Each struct hands out indices and saves every emitter
//! from threading `&mut u32` counters around.
//!
//! - [`ComponentIndices`] — component-level type / instance / func
//!   indices plus the embedded core-module's instance / func indices.
//! - [`DispatchIndices`] — type and function indices INSIDE the
//!   dispatch core module. Disjoint from [`ComponentIndices`]; the
//!   core module only talks to the outer component through named
//!   env imports.
//! - [`FunctionIndices`] — local allocator for a single wasm
//!   function body. `base` is the first free slot above the
//!   function's parameters; each `alloc_local` returns the next
//!   index and records the type so the caller can build the
//!   `Function` from [`FunctionIndices::into_locals`] at the end.

use wasm_encoder::ValType;

/// Running index allocators for the outer component's various
/// core-level namespaces. One instance lives in `build_adapter_bytes`
/// and is threaded through every phase by `&mut`, so phases no longer
/// need to carry "where did we leave the type counter?" state in
/// their outcome structs.
#[derive(Default)]
pub(super) struct ComponentIndices {
    pub ty: u32,
    pub inst: u32,
    pub func: u32,
    pub core_inst: u32,
    pub core_func: u32,
}

impl ComponentIndices {
    pub fn alloc_ty(&mut self) -> u32 {
        let idx = self.ty;
        self.ty += 1;
        idx
    }
    pub fn alloc_inst(&mut self) -> u32 {
        let idx = self.inst;
        self.inst += 1;
        idx
    }
    pub fn alloc_func(&mut self) -> u32 {
        let idx = self.func;
        self.func += 1;
        idx
    }
    pub fn alloc_core_inst(&mut self) -> u32 {
        let idx = self.core_inst;
        self.core_inst += 1;
        idx
    }
    pub fn alloc_core_func(&mut self) -> u32 {
        let idx = self.core_func;
        self.core_func += 1;
        idx
    }
}

/// Running index allocators for the dispatch core module's type and
/// function spaces. Scoped to a single call of `build_dispatch_module`.
///
/// These are CORE-MODULE-INTERNAL indices. The dispatch module is a
/// self-contained core wasm module that only communicates with the
/// outer component through named env imports (`env/mem`,
/// `env/handler_f{i}`, …), so its type and function tables have no
/// relationship to the outer component's indices tracked by
/// [`ComponentIndices`]. Keeping them in a separate struct makes the
/// "two different index spaces" explicit and saves every type/import
/// emitter from threading its own `&mut u32`.
pub(super) struct DispatchIndices {
    /// Next free slot in the core module's `TypeSection`.
    pub ty: u32,
    /// Next free index in the core module's function space. Imports
    /// come first (contiguous from 0), then the defined wrapper funcs
    /// in the code section (contiguous after the last import).
    pub func: u32,
}

impl DispatchIndices {
    pub fn new() -> Self {
        Self { ty: 0, func: 0 }
    }

    /// Reserve the next type-section slot and return its index.
    pub fn alloc_ty(&mut self) -> u32 {
        let idx = self.ty;
        self.ty += 1;
        idx
    }

    /// Reserve the next function-index slot and return its index.
    pub fn alloc_func(&mut self) -> u32 {
        let idx = self.func;
        self.func += 1;
        idx
    }
}

/// Local-index allocator for one wasm function. `base` is the first
/// free slot above the function's parameters; allocated locals count
/// up from there.
pub(super) struct FunctionIndices {
    /// First local index — one past the last parameter.
    base: u32,
    /// Types of locals in allocation order. Fed directly into
    /// [`wasm_encoder::Function::new_with_locals_types`].
    locals: Vec<ValType>,
}

impl FunctionIndices {
    /// New allocator for a function with `param_count` parameters.
    /// The first `alloc_local` will return index `param_count`.
    pub fn new(param_count: u32) -> Self {
        Self {
            base: param_count,
            locals: Vec::new(),
        }
    }

    /// Reserve a new local of the given type and return its index.
    pub fn alloc_local(&mut self, ty: ValType) -> u32 {
        let idx = self.base + self.locals.len() as u32;
        self.locals.push(ty);
        idx
    }

    /// Next local index that would be allocated, without reserving.
    /// Useful for pre-computing where a downstream emitter's locals
    /// will land.
    pub fn next_local_idx(&self) -> u32 {
        self.base + self.locals.len() as u32
    }

    /// Consume the allocator and return the locals vec for
    /// `Function::new_with_locals_types`.
    pub fn into_locals(self) -> Vec<ValType> {
        self.locals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_indices_allocate_contiguous_from_base() {
        let mut idx = FunctionIndices::new(3);
        assert_eq!(idx.next_local_idx(), 3);
        assert_eq!(idx.alloc_local(ValType::I32), 3);
        assert_eq!(idx.alloc_local(ValType::I64), 4);
        assert_eq!(idx.alloc_local(ValType::I32), 5);
        assert_eq!(idx.next_local_idx(), 6);
        assert_eq!(
            idx.into_locals(),
            vec![ValType::I32, ValType::I64, ValType::I32]
        );
    }

    #[test]
    fn function_indices_zero_params_starts_at_zero() {
        let mut idx = FunctionIndices::new(0);
        assert_eq!(idx.alloc_local(ValType::I32), 0);
        assert_eq!(idx.alloc_local(ValType::F64), 1);
    }

    #[test]
    fn component_indices_track_each_namespace_independently() {
        let mut idx = ComponentIndices::default();
        assert_eq!(idx.alloc_ty(), 0);
        assert_eq!(idx.alloc_ty(), 1);
        assert_eq!(idx.alloc_inst(), 0);
        assert_eq!(idx.alloc_func(), 0);
        assert_eq!(idx.alloc_core_inst(), 0);
        assert_eq!(idx.alloc_core_func(), 0);
        assert_eq!(idx.alloc_ty(), 2);
    }

    #[test]
    fn dispatch_indices_track_ty_and_func_independently() {
        let mut idx = DispatchIndices::new();
        assert_eq!(idx.alloc_ty(), 0);
        assert_eq!(idx.alloc_func(), 0);
        assert_eq!(idx.alloc_ty(), 1);
        assert_eq!(idx.alloc_func(), 1);
    }
}
