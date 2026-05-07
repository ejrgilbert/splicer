//! Running index allocators for the wasm namespaces the dispatch core
//! module emits into.
//!
//! - [`DispatchIndices`] — type and function indices INSIDE the
//!   dispatch core module.
//! - [`LocalsBuilder`] — local allocator for a single wasm function
//!   body. The first `alloc_local` returns index `param_count`;
//!   [`LocalsBuilder::freeze`] hands back a [`FrozenLocals`] whose
//!   `locals` field feeds `wasm_encoder::Function::new_with_locals_types`.
//!   The split is a typestate: callers that hold a [`FrozenLocals`]
//!   can no longer allocate, so "must allocate before locals freeze"
//!   becomes a compile error.

use wasm_encoder::ValType;

/// Running index allocators for the dispatch core module's type and
/// function spaces. Scoped to a single `build_adapter` call.
pub(crate) struct DispatchIndices {
    /// Next free slot in the core module's `TypeSection`.
    pub ty: u32,
    /// Next free index in the core module's function space. Imports
    /// come first (contiguous from 0), then defined wrapper funcs
    /// after.
    pub func: u32,
}

impl DispatchIndices {
    pub fn new() -> Self {
        Self { ty: 0, func: 0 }
    }

    pub fn alloc_ty(&mut self) -> u32 {
        let idx = self.ty;
        self.ty += 1;
        idx
    }

    pub fn alloc_func(&mut self) -> u32 {
        let idx = self.func;
        self.func += 1;
        idx
    }
}

/// Mutable local-index allocator for one wasm function. `base` is the
/// first free slot above the function's parameters; allocated locals
/// count up from there. Consume via [`Self::freeze`] to hand off to
/// `wasm_encoder::Function::new_with_locals_types`.
pub(crate) struct LocalsBuilder {
    base: u32,
    locals: Vec<ValType>,
}

impl LocalsBuilder {
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

    /// Consume the builder and return a frozen handle for
    /// `Function::new_with_locals_types`. After this point the locals
    /// list is closed — there is no way to allocate further indices.
    pub fn freeze(self) -> FrozenLocals {
        FrozenLocals {
            locals: self.locals,
        }
    }
}

/// Frozen output of a [`LocalsBuilder`]. Carries the typed local list
/// in the shape `wasm_encoder::Function::new_with_locals_types` wants.
/// Distinct from `LocalsBuilder` so the type system rejects "allocate
/// after freeze."
pub(crate) struct FrozenLocals {
    pub locals: Vec<ValType>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locals_builder_allocates_contiguous_from_base() {
        let mut idx = LocalsBuilder::new(3);
        assert_eq!(idx.alloc_local(ValType::I32), 3);
        assert_eq!(idx.alloc_local(ValType::I64), 4);
        assert_eq!(idx.alloc_local(ValType::I32), 5);
        assert_eq!(
            idx.freeze().locals,
            vec![ValType::I32, ValType::I64, ValType::I32]
        );
    }

    #[test]
    fn locals_builder_zero_params_starts_at_zero() {
        let mut idx = LocalsBuilder::new(0);
        assert_eq!(idx.alloc_local(ValType::I32), 0);
        assert_eq!(idx.alloc_local(ValType::F64), 1);
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
