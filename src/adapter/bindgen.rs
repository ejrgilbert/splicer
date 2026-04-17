//! A [`wit_bindgen_core::abi::Bindgen`] implementation that emits
//! [`wasm_encoder`] instructions, used to drive
//! [`wit_bindgen_core::abi::lift_from_memory`] when the adapter needs
//! to load an async task's result from linear memory onto the wasm
//! value stack for `task.return`.
//!
//! ## Operand model
//!
//! `Operand = ()` — the wasm value stack is the source of truth. The
//! generator's internal operand stack tracks counts, not identities;
//! our `emit` pushes/pops placeholders to match the declared arity of
//! each [`Instruction`] variant.
//!
//! ## Address handling
//!
//! The base address for all loads lives in a local (`addr_local`) that
//! the caller sets before invoking `lift_from_memory`. Every load emit
//! prepends `local.get $addr` so the load has the address on top of
//! the wasm stack. The generator's "address operand" can be cloned
//! freely because our impl never pops a wasm value for it — it always
//! re-reads from the local.
//!
//! ## Block-capture IR
//!
//! Variant / option / result lifts require per-arm bodies that later
//! get wrapped in a wasm `block ... br_table ... end` structure. When
//! `push_block` fires we start a fresh buffer; emits redirect to the
//! top-of-stack buffer; `finish_block` pops it to
//! [`CompletedBlock`]s that the variant emit consumes.

use std::borrow::Cow;

use wasm_encoder::{BlockType, Instruction, MemArg, ValType};
use wit_bindgen_core::abi::{Bindgen, Bitcast, Instruction as AbiInst, WasmType};
use wit_parser::{Alignment, ArchitectureSize, Resolve, SizeAlign, Type};

use super::bindgen_compat::{cast, flat_types};
use super::indices::FunctionIndices;

/// Bindgen that accumulates `wasm_encoder::Instruction`s into buffers,
/// ready to be flushed into a `Function` by [`WasmEncoderBindgen::drain_into`].
pub(super) struct WasmEncoderBindgen<'a> {
    /// Top-level instruction buffer — the final output that goes into
    /// the target Function. Emits land here when no block is active.
    main: Vec<Instruction<'static>>,
    /// Stack of active block buffers. When non-empty, emits go to the
    /// top buffer instead of `main`. Populated by `push_block`, drained
    /// by `finish_block`.
    block_buffers: Vec<Vec<Instruction<'static>>>,
    /// Completed blocks waiting to be consumed by `VariantLift` /
    /// `OptionLift` / `ResultLift`. LIFO order — last `finish_block`
    /// is at the top.
    completed_blocks: Vec<CompletedBlock>,
    /// Canonical-ABI sizes, required by `Bindgen::sizes`.
    sizes: &'a SizeAlign,
    /// Local index holding the base address for all loads.
    addr_local: u32,
    /// Shared local allocator — the bindgen routes its dynamic
    /// allocations through the same [`FunctionIndices`] the caller
    /// uses for its own locals, so all of the function's locals land
    /// in one contiguous, correctly-indexed block.
    indices: &'a mut FunctionIndices,
}

/// A block body paired with the number of wasm values it leaves on
/// the value stack. The value stack shape matters for variant emits
/// that must widen each arm to the joined flat signature.
struct CompletedBlock {
    body: Vec<Instruction<'static>>,
    nresults: usize,
}

impl<'a> WasmEncoderBindgen<'a> {
    /// Create a new bindgen. The caller sets up `addr_local` (an
    /// i32 local holding the base address for loads) and hands in a
    /// `&mut FunctionIndices` for all dynamic local allocation the
    /// bindgen needs.
    pub fn new(sizes: &'a SizeAlign, addr_local: u32, indices: &'a mut FunctionIndices) -> Self {
        Self {
            main: Vec::new(),
            block_buffers: Vec::new(),
            completed_blocks: Vec::new(),
            sizes,
            addr_local,
            indices,
        }
    }

    /// Consume the bindgen and return the accumulated wasm
    /// instructions. Locals were allocated through the caller's
    /// [`FunctionIndices`], so they're already tracked there.
    pub fn into_instructions(self) -> Vec<Instruction<'static>> {
        assert!(
            self.block_buffers.is_empty(),
            "into_instructions called mid-block — push_block/finish_block unbalanced"
        );
        assert!(
            self.completed_blocks.is_empty(),
            "into_instructions called with unconsumed completed blocks \
             (variant emit missing?)"
        );
        self.main
    }

    /// Allocate a new local of the given type via the shared
    /// [`FunctionIndices`].
    fn alloc_local(&mut self, ty: ValType) -> u32 {
        self.indices.alloc_local(ty)
    }

    /// Append one instruction to the currently-active buffer (either
    /// the top block buffer, or `main` if no block is active).
    fn emit_one(&mut self, inst: Instruction<'static>) {
        self.active_buf().push(inst);
    }

    fn active_buf(&mut self) -> &mut Vec<Instruction<'static>> {
        self.block_buffers.last_mut().unwrap_or(&mut self.main)
    }

    /// Emit `local.get $addr; <load>` for a memory load at the given
    /// byte offset. All load emits funnel through this helper.
    fn emit_load(&mut self, offset: ArchitectureSize, load: LoadKind) {
        let off = offset.size_wasm32() as u64;
        let mem_arg = MemArg {
            offset: off,
            align: load.natural_align_log2(),
            memory_index: 0,
        };
        let addr_local = self.addr_local;
        self.emit_one(Instruction::LocalGet(addr_local));
        self.emit_one(load.to_instruction(mem_arg));
    }

    /// Emit a bitcast sequence to convert the top-of-stack value's
    /// wasm type. Decomposes `Bitcast::Sequence` recursively and maps
    /// each leaf bitcast to its wasm instruction.
    fn emit_bitcast(&mut self, bc: &Bitcast) {
        use Bitcast::*;
        match bc {
            None => {}
            I32ToI64 => self.emit_one(Instruction::I64ExtendI32U),
            I64ToI32 => self.emit_one(Instruction::I32WrapI64),
            F32ToI32 => self.emit_one(Instruction::I32ReinterpretF32),
            I32ToF32 => self.emit_one(Instruction::F32ReinterpretI32),
            F64ToI64 => self.emit_one(Instruction::I64ReinterpretF64),
            I64ToF64 => self.emit_one(Instruction::F64ReinterpretI64),
            F32ToI64 => {
                self.emit_one(Instruction::I32ReinterpretF32);
                self.emit_one(Instruction::I64ExtendI32U);
            }
            I64ToF32 => {
                self.emit_one(Instruction::I32WrapI64);
                self.emit_one(Instruction::F32ReinterpretI32);
            }
            // Pointer / Length / PointerOrI64 casts collapse to i32 /
            // i64 at the wasm level on wasm32 — the type distinctions
            // matter for provenance in bindings, not for emission.
            PToI32 | I32ToP | I32ToL | LToI32 | PToL | LToP => {}
            I64ToP64 | P64ToI64 | I64ToL | LToI64 => {}
            PToP64 | P64ToP => {}
            Sequence(pair) => {
                let [a, b] = pair.as_ref();
                self.emit_bitcast(a);
                self.emit_bitcast(b);
            }
        }
    }

    /// Push a fresh block buffer onto the stack.
    fn start_block(&mut self) {
        self.block_buffers.push(Vec::new());
    }

    /// Pop the top block buffer and record it as a completed block
    /// with `nresults` wasm values on the stack at block exit.
    fn finish_block_body(&mut self, nresults: usize) {
        let body = self
            .block_buffers
            .pop()
            .expect("finish_block without matching push_block");
        self.completed_blocks
            .push(CompletedBlock { body, nresults });
    }

    /// Emit a zero constant for the given flat wasm type — used to
    /// pad variant arms whose natural flat is shorter than the joined
    /// payload.
    fn emit_const_zero(&mut self, wt: WasmType) {
        use WasmType::*;
        let inst = match wt {
            I32 | Pointer | Length => Instruction::I32Const(0),
            I64 | PointerOrI64 => Instruction::I64Const(0),
            F32 => Instruction::F32Const(0.0f32.into()),
            F64 => Instruction::F64Const(0.0f64.into()),
        };
        self.emit_one(inst);
    }

    /// Emit the block / `br_table` dispatch structure for a variant
    /// lift, plus per-arm widening to the joined flat. Consumes the
    /// top `n` entries of `completed_blocks` (one per arm, in arm
    /// order) and the disc value on the wasm value stack.
    ///
    /// After this runs, the wasm stack holds the full joined flat
    /// `[disc, ...joined_payload]` for the variant.
    ///
    /// ## Structure
    ///
    /// A wasm block with a multi-value result type (e.g. `block
    /// (result i32 i64)`) requires registering a function type in
    /// the module's type section — an awkward cross-cutting concern
    /// for a function-body-only emitter. Instead we route each arm's
    /// widened values through locals: the arm body widens-and-stores
    /// into per-variant locals, every block in the br_table chain
    /// uses `BlockType::Empty`, and after the chain closes we
    /// re-push `disc` followed by the payload locals to form the
    /// joined flat on the stack.
    ///
    /// ## Nested variants
    ///
    /// The `disc_local` is allocated FRESH per call, not shared
    /// across variants. If an arm contains a nested variant, that
    /// nested emit allocates its own disc_local before overwriting
    /// would happen; outer's disc stays intact for the final
    /// re-push.
    fn emit_variant_dispatch(
        &mut self,
        resolve: &Resolve,
        variant_type: &Type,
        arm_types: &[Option<Type>],
    ) {
        // Joined flat: [disc, ...joined_payload].
        let joined = flat_types(resolve, variant_type, None).expect(
            "variant flat must fit in MAX_FLAT_PARAMS — larger variants are invalid per the \
             canonical ABI spec",
        );
        assert!(
            !joined.is_empty(),
            "variant joined flat must include at least a discriminant"
        );
        let joined_payload: Vec<WasmType> = joined[1..].to_vec();

        // Allocate a fresh disc local for this variant. Sharing one
        // across nested variants would cause an inner emit to
        // overwrite the outer's disc before the outer's final
        // re-push reads it back.
        let disc_local = self.alloc_local(ValType::I32);
        self.emit_one(Instruction::LocalSet(disc_local));

        // Allocate fresh locals for each joined payload slot. These
        // are per-variant; nested variants allocate their own sets.
        let payload_locals: Vec<u32> = joined_payload
            .iter()
            .map(|wt| self.alloc_local(wasm_type_to_val(*wt)))
            .collect();

        // Pop the arm blocks (most recent n, in arm order).
        let n = arm_types.len();
        let start = self.completed_blocks.len() - n;
        let arm_blocks: Vec<CompletedBlock> = self.completed_blocks.drain(start..).collect();

        // Compute arm natural flats.
        let arm_flats: Vec<Vec<WasmType>> = arm_types
            .iter()
            .map(|opt_ty| match opt_ty {
                None => Vec::new(),
                Some(ty) => flat_types(resolve, ty, None).expect(
                    "arm flat must fit in MAX_FLAT_PARAMS — larger arms are invalid per the \
                     canonical ABI spec",
                ),
            })
            .collect();

        // Emit nested blocks: $end, $default, $case_n-1, ..., $case_0.
        self.emit_one(Instruction::Block(BlockType::Empty)); // $end
        self.emit_one(Instruction::Block(BlockType::Empty)); // $default
        for _ in 0..n {
            self.emit_one(Instruction::Block(BlockType::Empty)); // $case_i
        }
        // br_table dispatch inside the innermost block.
        self.emit_one(Instruction::LocalGet(disc_local));
        let table: Cow<'static, [u32]> = Cow::Owned((0..n as u32).collect());
        self.emit_one(Instruction::BrTable(table, n as u32));
        self.emit_one(Instruction::End); // close $case_0

        // Emit each arm body, widening + stashing into payload_locals.
        for (i, arm) in arm_blocks.iter().enumerate() {
            let arm_flat = &arm_flats[i];

            // Run the recorded arm body — pushes arm's natural flat.
            for inst in &arm.body {
                self.emit_one(inst.clone());
            }
            debug_assert_eq!(
                arm.nresults,
                arm_flat.len(),
                "arm block nresults must match arm natural flat length"
            );

            // Widen from top of stack down. Each pop-widen-store
            // sequence peels one value off, so the ORDER is reverse
            // of the flat layout (top-of-stack = last-pushed).
            for j in (0..arm_flat.len()).rev() {
                self.emit_bitcast(&cast(arm_flat[j], joined_payload[j]));
                self.emit_one(Instruction::LocalSet(payload_locals[j]));
            }

            // Zero-pad any joined payload slots this arm didn't fill.
            for j in arm_flat.len()..joined_payload.len() {
                self.emit_const_zero(joined_payload[j]);
                self.emit_one(Instruction::LocalSet(payload_locals[j]));
            }

            // br $end. Depth: after case_i's End, we're inside
            //   case_{i+1}, ..., case_{n-1}, $default, $end
            // → (n-1-i) + 2 enclosing blocks, so $end is at depth
            // (n-1-i) + 1 = n - i.
            let depth = (n - i) as u32;
            self.emit_one(Instruction::Br(depth));
            // Close this case's block.
            self.emit_one(Instruction::End);
        }

        // $default body: unreachable (invalid disc traps).
        self.emit_one(Instruction::Unreachable);
        self.emit_one(Instruction::End); // close $default
        self.emit_one(Instruction::End); // close $end

        // Re-push [disc, ...payload] to form the joined flat on the stack.
        self.emit_one(Instruction::LocalGet(disc_local));
        for idx in &payload_locals {
            self.emit_one(Instruction::LocalGet(*idx));
        }
    }
}

/// Map a wit-parser `WasmType` to a `wasm_encoder::ValType`. Splicer
/// targets wasm32, so Pointer/Length collapse to I32 and PointerOrI64
/// collapses to I64.
fn wasm_type_to_val(wt: WasmType) -> ValType {
    use WasmType::*;
    match wt {
        I32 | Pointer | Length => ValType::I32,
        I64 | PointerOrI64 => ValType::I64,
        F32 => ValType::F32,
        F64 => ValType::F64,
    }
}

/// The six load-instruction shapes the canonical ABI reads from
/// memory. Collapses wit-bindgen-core's 10-way split (with Pointer /
/// Length duplicates) to the actual wasm instructions.
#[derive(Clone, Copy)]
enum LoadKind {
    I32Load,
    I32Load8U,
    I32Load8S,
    I32Load16U,
    I32Load16S,
    I64Load,
    F32Load,
    F64Load,
}

impl LoadKind {
    fn to_instruction(self, mem_arg: MemArg) -> Instruction<'static> {
        match self {
            LoadKind::I32Load => Instruction::I32Load(mem_arg),
            LoadKind::I32Load8U => Instruction::I32Load8U(mem_arg),
            LoadKind::I32Load8S => Instruction::I32Load8S(mem_arg),
            LoadKind::I32Load16U => Instruction::I32Load16U(mem_arg),
            LoadKind::I32Load16S => Instruction::I32Load16S(mem_arg),
            LoadKind::I64Load => Instruction::I64Load(mem_arg),
            LoadKind::F32Load => Instruction::F32Load(mem_arg),
            LoadKind::F64Load => Instruction::F64Load(mem_arg),
        }
    }

    /// Natural alignment in log2 bytes, per the canonical ABI's
    /// memory-alignment rules for each load width.
    fn natural_align_log2(self) -> u32 {
        match self {
            LoadKind::I32Load8U | LoadKind::I32Load8S => 0,
            LoadKind::I32Load16U | LoadKind::I32Load16S => 1,
            LoadKind::I32Load | LoadKind::F32Load => 2,
            LoadKind::I64Load | LoadKind::F64Load => 3,
        }
    }
}

impl Bindgen for WasmEncoderBindgen<'_> {
    type Operand = ();

    fn emit(
        &mut self,
        _resolve: &Resolve,
        inst: &AbiInst<'_>,
        operands: &mut Vec<()>,
        results: &mut Vec<()>,
    ) {
        // Most of our arms don't look at operand/results contents —
        // Operand = () carries no info. We still must push the
        // declared number of results, which `produce_n` handles.
        match inst {
            // ── Memory loads ────────────────────────────────────
            AbiInst::I32Load { offset } => {
                self.emit_load(*offset, LoadKind::I32Load);
                produce_n(results, 1);
            }
            AbiInst::I32Load8U { offset } => {
                self.emit_load(*offset, LoadKind::I32Load8U);
                produce_n(results, 1);
            }
            AbiInst::I32Load8S { offset } => {
                self.emit_load(*offset, LoadKind::I32Load8S);
                produce_n(results, 1);
            }
            AbiInst::I32Load16U { offset } => {
                self.emit_load(*offset, LoadKind::I32Load16U);
                produce_n(results, 1);
            }
            AbiInst::I32Load16S { offset } => {
                self.emit_load(*offset, LoadKind::I32Load16S);
                produce_n(results, 1);
            }
            AbiInst::I64Load { offset } => {
                self.emit_load(*offset, LoadKind::I64Load);
                produce_n(results, 1);
            }
            AbiInst::F32Load { offset } => {
                self.emit_load(*offset, LoadKind::F32Load);
                produce_n(results, 1);
            }
            AbiInst::F64Load { offset } => {
                self.emit_load(*offset, LoadKind::F64Load);
                produce_n(results, 1);
            }
            AbiInst::PointerLoad { offset } => {
                // Wasm32: Pointer is i32.
                self.emit_load(*offset, LoadKind::I32Load);
                produce_n(results, 1);
            }
            AbiInst::LengthLoad { offset } => {
                // Wasm32: Length is i32.
                self.emit_load(*offset, LoadKind::I32Load);
                produce_n(results, 1);
            }

            // ── Scalar "lift" instructions: no-op on wasm side ──
            // The wasm value loaded by the preceding Load is already
            // the canonical representation; the interface-type cast
            // is a source-language concept we don't model.
            AbiInst::BoolFromI32
            | AbiInst::S8FromI32
            | AbiInst::U8FromI32
            | AbiInst::S16FromI32
            | AbiInst::U16FromI32
            | AbiInst::S32FromI32
            | AbiInst::U32FromI32
            | AbiInst::S64FromI64
            | AbiInst::U64FromI64
            | AbiInst::CharFromI32
            | AbiInst::F32FromCoreF32
            | AbiInst::F64FromCoreF64 => {
                produce_n(results, 1);
            }

            // ── Bitcasts ───────────────────────────────────────
            AbiInst::Bitcasts { casts } => {
                for bc in casts.iter() {
                    self.emit_bitcast(bc);
                }
                produce_n(results, operands.len());
            }

            // ── Aggregate lifts: no-op ─────────────────────────
            // The N wasm values on the value stack already represent
            // the aggregated value (record / tuple / handle / flags /
            // enum / future / stream / error-context / fixed-list).
            // No wasm emission; just collapse N operands → 1.
            AbiInst::RecordLift { .. }
            | AbiInst::TupleLift { .. }
            | AbiInst::HandleLift { .. }
            | AbiInst::FutureLift { .. }
            | AbiInst::StreamLift { .. }
            | AbiInst::EnumLift { .. }
            | AbiInst::FlagsLift { .. }
            | AbiInst::ErrorContextLift
            | AbiInst::FixedLengthListLift { .. }
            | AbiInst::StringLift
            | AbiInst::ListCanonLift { .. }
            | AbiInst::ListLift { .. } => {
                produce_n(results, 1);
            }

            // ── Variant / option / result lifts ────────────────
            AbiInst::VariantLift { variant, ty, .. } => {
                let arms: Vec<Option<Type>> = variant.cases.iter().map(|c| c.ty).collect();
                self.emit_variant_dispatch(_resolve, &Type::Id(*ty), &arms);
                produce_n(results, 1);
            }
            AbiInst::OptionLift { payload, ty } => {
                let arms = vec![None, Some(**payload)];
                self.emit_variant_dispatch(_resolve, &Type::Id(*ty), &arms);
                produce_n(results, 1);
            }
            AbiInst::ResultLift { result, ty } => {
                let arms = vec![result.ok, result.err];
                self.emit_variant_dispatch(_resolve, &Type::Id(*ty), &arms);
                produce_n(results, 1);
            }

            // ── Instructions we don't expect on the lift-from-memory path ──
            other => unimplemented!(
                "WasmEncoderBindgen::emit hit unsupported instruction: {:?}. \
                 This path is only exercised by lift_from_memory; other entry \
                 points aren't supported.",
                other
            ),
        }
    }

    fn return_pointer(&mut self, _size: ArchitectureSize, _align: Alignment) -> () {
        unimplemented!(
            "return_pointer is only called on lowering paths; \
             lift_from_memory never invokes it"
        );
    }

    fn push_block(&mut self) {
        self.start_block();
    }

    fn finish_block(&mut self, operand: &mut Vec<()>) {
        let nresults = operand.len();
        operand.clear();
        self.finish_block_body(nresults);
    }

    fn sizes(&self) -> &SizeAlign {
        self.sizes
    }

    fn is_list_canonical(&self, _resolve: &Resolve, _element: &Type) -> bool {
        // For lift_from_memory of a list, the canonical representation
        // means we stop at `(ptr, len)` on the stack rather than iterate
        // each element. That's what the adapter wants — a `(ptr, len)`
        // pair is two i32s and matches the joined flat for the list
        // type. Return true unconditionally.
        true
    }
}

/// Push `n` placeholder operands onto a results vec. Mirrors the
/// arity declared in each `Instruction` variant so the generator's
/// bookkeeping stays consistent.
fn produce_n(results: &mut Vec<()>, n: usize) {
    for _ in 0..n {
        results.push(());
    }
}

#[cfg(test)]
impl WasmEncoderBindgen<'_> {
    /// Test-only: inspect the accumulated main-buffer instructions
    /// without draining them into a Function.
    pub(super) fn instructions(&self) -> &[Instruction<'static>] {
        &self.main
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wit_bindgen_core::abi::lift_from_memory;
    use wit_parser::{Docs, Field, Record, Stability, TypeDef, TypeDefKind, TypeOwner};

    fn new_sizes(resolve: &Resolve) -> SizeAlign {
        let mut s = SizeAlign::default();
        s.fill(resolve);
        s
    }

    /// Helper: count instructions matching a predicate.
    fn count<F: Fn(&Instruction<'static>) -> bool>(bg: &WasmEncoderBindgen<'_>, pred: F) -> usize {
        bg.instructions().iter().filter(|i| pred(i)).count()
    }

    #[test]
    fn lift_u32_emits_one_load() {
        let resolve = Resolve::default();
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        let _ = lift_from_memory(&resolve, &mut bg, (), &Type::U32);

        assert_eq!(count(&bg, |i| matches!(i, Instruction::LocalGet(_))), 1);
        assert_eq!(count(&bg, |i| matches!(i, Instruction::I32Load(_))), 1);
    }

    #[test]
    fn lift_u64_emits_i64_load() {
        let resolve = Resolve::default();
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(4);
        let mut bg = WasmEncoderBindgen::new(&sizes, 3, &mut indices);
        let _ = lift_from_memory(&resolve, &mut bg, (), &Type::U64);

        assert_eq!(count(&bg, |i| matches!(i, Instruction::I64Load(_))), 1);
        // addr_local=3 must show up in the LocalGet
        assert!(bg
            .instructions()
            .iter()
            .any(|i| matches!(i, Instruction::LocalGet(3))));
    }

    #[test]
    fn lift_record_emits_one_load_per_field() {
        let mut resolve = Resolve::default();
        let record_id = resolve.types.alloc(TypeDef {
            name: Some("r".to_string()),
            kind: TypeDefKind::Record(Record {
                fields: vec![
                    Field {
                        name: "a".to_string(),
                        ty: Type::U32,
                        docs: Docs::default(),
                    },
                    Field {
                        name: "b".to_string(),
                        ty: Type::U64,
                        docs: Docs::default(),
                    },
                    Field {
                        name: "c".to_string(),
                        ty: Type::U8,
                        docs: Docs::default(),
                    },
                ],
            }),
            owner: TypeOwner::None,
            docs: Docs::default(),
            stability: Stability::default(),
        });
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        let _ = lift_from_memory(&resolve, &mut bg, (), &Type::Id(record_id));

        // 3 fields → 3 load instructions, each paired with a LocalGet
        assert_eq!(count(&bg, |i| matches!(i, Instruction::LocalGet(_))), 3);
        assert_eq!(count(&bg, |i| matches!(i, Instruction::I32Load(_))), 1);
        assert_eq!(count(&bg, |i| matches!(i, Instruction::I64Load(_))), 1);
        assert_eq!(count(&bg, |i| matches!(i, Instruction::I32Load8U(_))), 1);
    }

    #[test]
    fn lift_string_emits_ptr_len_loads() {
        let resolve = Resolve::default();
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        let _ = lift_from_memory(&resolve, &mut bg, (), &Type::String);

        // String lifts as (ptr, len) — both i32 loads.
        assert_eq!(count(&bg, |i| matches!(i, Instruction::I32Load(_))), 2);
    }

    /// `result<u32, u32>` — homogeneous arms (both flatten to `[i32]`).
    /// Joined flat is `[i32 (disc), i32 (payload)]`. No widening
    /// needed; both arms' widening bitcasts are `None`.
    #[test]
    fn lift_homogeneous_result_emits_dispatch_structure() {
        let mut resolve = Resolve::default();
        let result_id = resolve.types.alloc(TypeDef {
            name: Some("r".to_string()),
            kind: TypeDefKind::Result(wit_parser::Result_ {
                ok: Some(Type::U32),
                err: Some(Type::U32),
            }),
            owner: TypeOwner::None,
            docs: Docs::default(),
            stability: Stability::default(),
        });
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        let _ = lift_from_memory(&resolve, &mut bg, (), &Type::Id(result_id));

        // Disc load (1 byte) + one payload load per arm (2).
        assert_eq!(count(&bg, |i| matches!(i, Instruction::I32Load8U(_))), 1);
        // Two block/brtable structure (4 nested blocks: $end, $default, $case_0, $case_1).
        assert_eq!(count(&bg, |i| matches!(i, Instruction::Block(_))), 4);
        assert_eq!(count(&bg, |i| matches!(i, Instruction::BrTable(_, _))), 1);
        assert_eq!(count(&bg, |i| matches!(i, Instruction::Unreachable)), 1);
        let _insts = bg.into_instructions();
        // Bindgen allocated exactly one disc local + one payload local (both i32).
        assert_eq!(indices.into_locals(), vec![ValType::I32, ValType::I32]);
    }

    /// `result<u8, u64>` — heterogeneous arms. Joined flat is
    /// `[i32 (disc), i64 (payload)]`. The `Ok` arm flat is `[i32]`
    /// (u8 loaded as i32), needs widening to i64 via `i64.extend_i32_u`.
    /// The `Err` arm flat is `[i64]` — no widening.
    #[test]
    fn lift_heterogeneous_result_emits_widening() {
        let mut resolve = Resolve::default();
        let result_id = resolve.types.alloc(TypeDef {
            name: Some("r".to_string()),
            kind: TypeDefKind::Result(wit_parser::Result_ {
                ok: Some(Type::U8),
                err: Some(Type::U64),
            }),
            owner: TypeOwner::None,
            docs: Docs::default(),
            stability: Stability::default(),
        });
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        let _ = lift_from_memory(&resolve, &mut bg, (), &Type::Id(result_id));

        // Widening bitcast i32 → i64 for the ok arm.
        assert_eq!(
            count(&bg, |i| matches!(i, Instruction::I64ExtendI32U)),
            1,
            "ok (u8) arm should widen to i64 to match joined payload"
        );
        let _insts = bg.into_instructions();
        // Disc local (i32) + payload local (i64).
        assert_eq!(indices.into_locals(), vec![ValType::I32, ValType::I64]);
    }

    /// `option<u32>` — None arm has no payload, so zero-padding is
    /// emitted for its joined_payload slot.
    #[test]
    fn lift_option_pads_none_arm_with_zero() {
        let mut resolve = Resolve::default();
        let opt_id = resolve.types.alloc(TypeDef {
            name: Some("o".to_string()),
            kind: TypeDefKind::Option(Type::U32),
            owner: TypeOwner::None,
            docs: Docs::default(),
            stability: Stability::default(),
        });
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        let _ = lift_from_memory(&resolve, &mut bg, (), &Type::Id(opt_id));

        // None arm (no payload) emits `i32.const 0` zero-pad.
        assert!(
            bg.instructions()
                .iter()
                .any(|i| matches!(i, Instruction::I32Const(0))),
            "option's None arm should emit i32.const 0 to pad joined payload"
        );
    }
}
