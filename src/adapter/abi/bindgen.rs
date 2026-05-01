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
//! get wrapped in a wasm `block ... br_table ... end` structure.
//! Fixed-size lists (see below) need the element-read body replayed
//! N times with an advancing base address. Both cases use the same
//! mechanism: when `push_block` fires we start a fresh buffer; emits
//! redirect to the top-of-stack buffer; `finish_block` pops it to
//! [`CompletedBlock`]s that the variant / list-lift emit consumes.
//!
//! ## Fixed-size vs dynamic lists
//!
//! `list<T>` (dynamic) flattens to `[i32 ptr, i32 len]` — a
//! heap-like reference. `list<T, N>` (fixed-size) flattens to
//! `N × flat(T)` inlined on the wasm stack, semantically like
//! `tuple<T, …, T>`. When a fixed-size list is stored in memory
//! we walk the N contiguous slots and push each element's flat
//! values. See the `FixedLengthListLiftFromMemory` emit arm for
//! the full rationale and emission strategy.
//!
//! ## Authoritative canonical-ABI references
//!
//! When in doubt, these docs are ground truth for flatten / load /
//! store semantics that this Bindgen implements:
//!
//! - Spec narrative:
//!   <https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md>
//! - Python reference implementation (precise semantics):
//!   <https://github.com/WebAssembly/component-model/blob/main/design/mvp/canonical-abi/definitions.py>
//!
//! Individual emit arms below link to the specific spec functions
//! (`flatten_type`, `load`, `lift_flat`, etc.) they correspond to
//! when the mapping isn't obvious from the Instruction name.

use std::borrow::Cow;

use wasm_encoder::{BlockType, Instruction, MemArg, ValType};
use wit_bindgen_core::abi::{Bindgen, Bitcast, Instruction as AbiInst, WasmType};
use wit_parser::{Alignment, ArchitectureSize, Resolve, SizeAlign, Type};

use super::super::indices::FunctionIndices;
use super::compat::{cast, flat_types};

/// Bindgen that accumulates `wasm_encoder::Instruction`s into buffers,
/// ready to be flushed into a `Function` by [`WasmEncoderBindgen::drain_into`].
pub(crate) struct WasmEncoderBindgen<'a> {
    /// Top-level instruction buffer — the final output that goes into
    /// the target Function. Emits land here when no block is active.
    main: Vec<Instruction<'static>>,
    /// Stack of active blocks. When non-empty, emits go to the top
    /// block's buffer instead of `main`. Populated by `push_block`,
    /// drained by `finish_block`.
    block_buffers: Vec<ActiveBlock>,
    /// Completed blocks waiting to be consumed by `VariantLift` /
    /// `OptionLift` / `ResultLift` / `FixedLengthListLiftFromMemory`.
    /// LIFO order — last `finish_block` is at the top.
    completed_blocks: Vec<CompletedBlock>,
    /// Canonical-ABI sizes, required by `Bindgen::sizes`.
    sizes: &'a SizeAlign,
    /// Local index holding the base address for all loads at the
    /// outermost scope. Iteration blocks override this via their own
    /// `iter_addr_local`; see [`WasmEncoderBindgen::current_addr_local`].
    addr_local: u32,
    /// Shared local allocator — the bindgen routes its dynamic
    /// allocations through the same [`FunctionIndices`] the caller
    /// uses for its own locals, so all of the function's locals land
    /// in one contiguous, correctly-indexed block.
    indices: &'a mut FunctionIndices,
}

/// An active block being captured. Tracks its instruction buffer and
/// — for list / fixed-size-list iteration blocks — the i32 local that
/// holds the current element's base address. When the block body
/// emits loads, they read from this local (if set) rather than the
/// outer `addr_local`.
struct ActiveBlock {
    buffer: Vec<Instruction<'static>>,
    /// Allocated lazily on the first `IterBasePointer` emitted inside
    /// this block. `None` for blocks that aren't iteration bodies
    /// (e.g. variant arm blocks).
    iter_addr_local: Option<u32>,
}

/// A captured block body — the wasm instructions emitted between a
/// `push_block` / `finish_block` pair. The variant-lift emit splices
/// these into the `block ... br_table ... end` dispatch structure;
/// the fixed-size-list emit replays them N times with per-iteration
/// base-address advancement.
///
/// We don't track the Bindgen operand count (`finish_block`'s
/// `operand.len()`) here because it counts the generator's abstract
/// operand stack — which collapses compound types via aggregate lifts
/// like `RecordLift`. For our purposes the *wasm* stack count is what
/// matters for widening, and that's driven by `push_flat(arm_type)`
/// at variant-emit time, not by the block itself.
struct CompletedBlock {
    body: Vec<Instruction<'static>>,
    /// The iteration local the body's loads read from, if this was
    /// an iteration block. `None` for variant-arm blocks.
    iter_addr_local: Option<u32>,
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
        match self.block_buffers.last_mut() {
            Some(block) => &mut block.buffer,
            None => &mut self.main,
        }
    }

    /// Return the local index holding the current address for loads:
    /// the innermost active iteration block's `iter_addr_local` if
    /// any, otherwise the outer `addr_local`. Walks the block stack
    /// so nested iteration / variant structures pick up the correct
    /// scope — e.g. a variant arm inside a fixed-size list sees the
    /// list's iter local, not the bindgen's top-level addr_local.
    fn current_addr_local(&self) -> u32 {
        for block in self.block_buffers.iter().rev() {
            if let Some(idx) = block.iter_addr_local {
                return idx;
            }
        }
        self.addr_local
    }

    /// Emit `local.get $addr; <load>` for a memory load at the given
    /// byte offset. All load emits funnel through this helper. The
    /// address local comes from [`Self::current_addr_local`], so
    /// loads inside a list iteration block read from the per-element
    /// iter local rather than the outer base.
    fn emit_load(&mut self, offset: ArchitectureSize, load: LoadKind) {
        let off = offset.size_wasm32() as u64;
        let mem_arg = MemArg {
            offset: off,
            align: load.natural_align_log2(),
            memory_index: 0,
        };
        let addr_local = self.current_addr_local();
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
            // Wasm32 mapping: `Pointer` and `Length` are `i32`,
            // `PointerOrI64` is `i64`. Casts between types that
            // collapse to the same wasm type are genuine no-ops; the
            // ones that cross the i32/i64 boundary need the
            // corresponding wasm extend/wrap.
            PToI32 | I32ToP | I32ToL | LToI32 | PToL | LToP => {}
            I64ToP64 | P64ToI64 => {}
            PToP64 | LToI64 => self.emit_one(Instruction::I64ExtendI32U),
            P64ToP | I64ToL => self.emit_one(Instruction::I32WrapI64),
            Sequence(pair) => {
                let [a, b] = pair.as_ref();
                self.emit_bitcast(a);
                self.emit_bitcast(b);
            }
        }
    }

    /// Push a fresh block onto the stack. `iter_addr_local` is
    /// allocated lazily — only if this block turns out to be an
    /// iteration body (i.e. emits an `IterBasePointer`).
    fn start_block(&mut self) {
        self.block_buffers.push(ActiveBlock {
            buffer: Vec::new(),
            iter_addr_local: None,
        });
    }

    /// Pop the top active block and record it as a completed block.
    fn finish_block_body(&mut self) {
        let active = self
            .block_buffers
            .pop()
            .expect("finish_block without matching push_block");
        self.completed_blocks.push(CompletedBlock {
            body: active.buffer,
            iter_addr_local: active.iter_addr_local,
        });
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
        // The widening loop's bounds come from `arm_flat.len()` — the
        // count of values the arm body leaves on the *wasm* value
        // stack — not from the generator's operand-stack view, which
        // collapses compound types via aggregate lifts like
        // `RecordLift`.
        for (i, arm) in arm_blocks.iter().enumerate() {
            let arm_flat = &arm_flats[i];

            // Run the recorded arm body — pushes arm's natural flat
            // on the wasm value stack.
            for inst in &arm.body {
                self.emit_one(inst.clone());
            }

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

        // After the loop's n Ends, $case_0 / ... / $case_{n-1} /
        // $default are all closed; control falls into $end's body
        // area. Emit the default-path trap here (runs when disc was
        // out of range, since all valid cases br'd to $end), then
        // close $end.
        self.emit_one(Instruction::Unreachable);
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

            // ── Fixed-size list lift ───────────────────────────
            //
            // The Canonical ABI treats `list<T, N>` (fixed-size)
            // fundamentally differently from `list<T>` (dynamic):
            //
            // | Type       | Flat form        | In memory            |
            // |------------|------------------|----------------------|
            // | list<T>    | `[i32 ptr, i32 len]` | elements at `*ptr` |
            // | list<T, N> | `N × flat(T)` inlined | N contiguous elements |
            //
            // The fixed-size variant is semantically a
            // `tuple<T, …, T>` (N times), so it flattens the same
            // way tuples do — every element becomes a value on the
            // wasm stack (or in a retptr buffer if `N × flat(T)` >
            // `MAX_FLAT_PARAMS`). The payoff is zero-copy passing
            // of small fixed arrays (hashes, UUIDs, 3D vectors,
            // small buffers) without the realloc + pointer-chase
            // that dynamic lists require.
            //
            // When a fixed-size list lives inside a container
            // (record field, async result buffer, …) it's stored
            // as N contiguous element slots in memory. This
            // instruction materializes the inlined flat form by
            // reading N elements out.
            //
            // Emission strategy: the generator captures the
            // per-element read as a block body (with
            // `IterBasePointer` marking where the element base
            // address is used), then fires this instruction to
            // iterate. We unroll at emission time: allocate an
            // iter local, initialize it to the list's base
            // address, and replay the block body once per element
            // with the local advanced by `elem_size` each step.
            // Loads inside the block body reference the iter
            // local via [`current_addr_local`] so they hit the
            // right element.
            //
            // Dynamic lists (`list<T>` / `TypeDefKind::List`) hit
            // the `PointerLoad` + `LengthLoad` pair in
            // `read_list_from_memory` and then `ListCanonLift`
            // above, which is a no-op in our emit — the `(ptr,
            // len)` pair is already the flat form.
            AbiInst::IterBasePointer => {
                // Lazily allocate the iteration address local on the
                // current active block — `FixedLengthListLiftFromMemory`
                // reads it off the completed block below.
                let need_alloc = self
                    .block_buffers
                    .last()
                    .expect("IterBasePointer must fire inside a block")
                    .iter_addr_local
                    .is_none();
                if need_alloc {
                    let idx = self.indices.alloc_local(ValType::I32);
                    self.block_buffers
                        .last_mut()
                        .expect("IterBasePointer must fire inside a block")
                        .iter_addr_local = Some(idx);
                }
                produce_n(results, 1);
            }
            AbiInst::FixedLengthListLiftFromMemory { element, size, .. } => {
                let elem_size = self.sizes.size(element).size_wasm32() as u32;
                let block = self
                    .completed_blocks
                    .pop()
                    .expect("FixedLengthListLiftFromMemory without a matching block");
                let iter_addr = block.iter_addr_local.expect(
                    "fixed-size-list block must have allocated an iter_addr_local via \
                     IterBasePointer",
                );
                // Initialize iter_addr_local to the current base —
                // the parent's address (outer addr_local, or a
                // parent iteration's iter local for nested lists).
                let parent_addr = self.current_addr_local();
                self.emit_one(Instruction::LocalGet(parent_addr));
                self.emit_one(Instruction::LocalSet(iter_addr));
                for i in 0..*size {
                    if i > 0 {
                        // Advance by elem_size. `elem_size == 0` is
                        // possible for zero-sized records; the add is
                        // a no-op in that case but harmless.
                        self.emit_one(Instruction::LocalGet(iter_addr));
                        self.emit_one(Instruction::I32Const(elem_size as i32));
                        self.emit_one(Instruction::I32Add);
                        self.emit_one(Instruction::LocalSet(iter_addr));
                    }
                    for inst in &block.body {
                        self.emit_one(inst.clone());
                    }
                }
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

    fn return_pointer(&mut self, _size: ArchitectureSize, _align: Alignment) {
        unimplemented!(
            "return_pointer is only called on lowering paths; \
             lift_from_memory never invokes it"
        );
    }

    fn push_block(&mut self) {
        self.start_block();
    }

    fn finish_block(&mut self, operand: &mut Vec<()>) {
        // The generator's operand-stack count at block exit isn't
        // meaningful for our wasm emission — see `CompletedBlock`.
        operand.clear();
        self.finish_block_body();
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
    pub(crate) fn instructions(&self) -> &[Instruction<'static>] {
        &self.main
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wit_bindgen_core::abi::lift_from_memory;
    use wit_parser::{Docs, Field, Record, Span, Stability, TypeDef, TypeDefKind, TypeOwner};

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
        lift_from_memory(&resolve, &mut bg, (), &Type::U32);

        assert_eq!(count(&bg, |i| matches!(i, Instruction::LocalGet(_))), 1);
        assert_eq!(count(&bg, |i| matches!(i, Instruction::I32Load(_))), 1);
    }

    #[test]
    fn lift_u64_emits_i64_load() {
        let resolve = Resolve::default();
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(4);
        let mut bg = WasmEncoderBindgen::new(&sizes, 3, &mut indices);
        lift_from_memory(&resolve, &mut bg, (), &Type::U64);

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
                        span: Span::default(),
                    },
                    Field {
                        name: "b".to_string(),
                        ty: Type::U64,
                        docs: Docs::default(),
                        span: Span::default(),
                    },
                    Field {
                        name: "c".to_string(),
                        ty: Type::U8,
                        docs: Docs::default(),
                        span: Span::default(),
                    },
                ],
            }),
            owner: TypeOwner::None,
            docs: Docs::default(),
            stability: Stability::default(),
            span: Span::default(),
        });
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        lift_from_memory(&resolve, &mut bg, (), &Type::Id(record_id));

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
        lift_from_memory(&resolve, &mut bg, (), &Type::String);

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
            span: Span::default(),
        });
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        lift_from_memory(&resolve, &mut bg, (), &Type::Id(result_id));

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
            span: Span::default(),
        });
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        lift_from_memory(&resolve, &mut bg, (), &Type::Id(result_id));

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
            span: Span::default(),
        });
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        lift_from_memory(&resolve, &mut bg, (), &Type::Id(opt_id));

        // None arm (no payload) emits `i32.const 0` zero-pad.
        assert!(
            bg.instructions()
                .iter()
                .any(|i| matches!(i, Instruction::I32Const(0))),
            "option's None arm should emit i32.const 0 to pad joined payload"
        );
    }

    /// `result<string, u64>` forces a `Pointer → PointerOrI64` cast
    /// at payload position 0: ok's flat is `[Pointer, Length]`, err's
    /// flat is `[I64]`, and their positional join is
    /// `[PointerOrI64, Length]`. The ok arm must emit
    /// `i64.extend_i32_u` to widen its i32 pointer up to the joined
    /// i64 slot — without that, the stack type disagrees with the
    /// joined-flat block signature and wasm validation rejects with
    /// "expected i64, found i32".
    #[test]
    fn lift_result_string_u64_widens_pointer_to_pointer_or_i64() {
        let mut resolve = Resolve::default();
        let result_id = resolve.types.alloc(TypeDef {
            name: Some("r".to_string()),
            kind: TypeDefKind::Result(wit_parser::Result_ {
                ok: Some(Type::String),
                err: Some(Type::U64),
            }),
            owner: TypeOwner::None,
            docs: Docs::default(),
            stability: Stability::default(),
            span: Span::default(),
        });
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        lift_from_memory(&resolve, &mut bg, (), &Type::Id(result_id));

        // Exactly one `i64.extend_i32_u`: ok arm widens its Pointer
        // (i32) to the joined PointerOrI64 (i64) at position 0.
        // (Length at position 1 stays i32 on both sides; err arm's
        // I64 → PointerOrI64 is i64→i64, no instruction.)
        assert_eq!(
            count(&bg, |i| matches!(i, Instruction::I64ExtendI32U)),
            1,
            "ok (string) arm should widen Pointer to PointerOrI64"
        );
        let _insts = bg.into_instructions();
        // Joined flat: [disc=i32, PointerOrI64→i64, Length→i32].
        // Locals: disc(i32), payload[0]=i64, payload[1]=i32.
        assert_eq!(
            indices.into_locals(),
            vec![ValType::I32, ValType::I64, ValType::I32]
        );
    }

    /// `list<u32, 4>` — fixed-size list of 4 u32s. Should emit the
    /// iteration init (`LocalGet $parent; LocalSet $iter`) plus 4
    /// unrolled element loads with the iter local advanced by 4
    /// bytes each time.
    #[test]
    fn lift_fixed_size_list_unrolls_n_loads() {
        let mut resolve = Resolve::default();
        let list_id = resolve.types.alloc(TypeDef {
            name: Some("l".to_string()),
            kind: TypeDefKind::FixedLengthList(Type::U32, 4),
            owner: TypeOwner::None,
            docs: Docs::default(),
            stability: Stability::default(),
            span: Span::default(),
        });
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        lift_from_memory(&resolve, &mut bg, (), &Type::Id(list_id));

        // Four i32 loads, one per element.
        assert_eq!(count(&bg, |i| matches!(i, Instruction::I32Load(_))), 4);
        // Three `I32Add`s — advance iter_addr between iterations 0→1,
        // 1→2, 2→3 (the first iteration reads at base, no advance).
        assert_eq!(count(&bg, |i| matches!(i, Instruction::I32Add)), 3);
        // One `I32Const(4)` per advance (elem_size = 4 for u32).
        assert_eq!(count(&bg, |i| matches!(i, Instruction::I32Const(4))), 3);
        // Bindgen allocated one i32 local for the iteration address.
        let _insts = bg.into_instructions();
        assert_eq!(indices.into_locals(), vec![ValType::I32]);
    }

    /// Dynamic `list<T>` flattens to `[Pointer, Length]`, the same
    /// shape as `string`, so `result<list<T>, u64>` exercises the
    /// same `Pointer → PointerOrI64` widening as the string case.
    /// Kept as a separate test so the assertion isolates the list
    /// path in case list and string lowering evolve independently.
    #[test]
    fn lift_result_list_u64_widens_pointer_to_pointer_or_i64() {
        let mut resolve = Resolve::default();
        let list_id = resolve.types.alloc(TypeDef {
            name: Some("l".to_string()),
            kind: TypeDefKind::List(Type::U8),
            owner: TypeOwner::None,
            docs: Docs::default(),
            stability: Stability::default(),
            span: Span::default(),
        });
        let result_id = resolve.types.alloc(TypeDef {
            name: Some("r".to_string()),
            kind: TypeDefKind::Result(wit_parser::Result_ {
                ok: Some(Type::Id(list_id)),
                err: Some(Type::U64),
            }),
            owner: TypeOwner::None,
            docs: Docs::default(),
            stability: Stability::default(),
            span: Span::default(),
        });
        let sizes = new_sizes(&resolve);
        let mut indices = FunctionIndices::new(1);
        let mut bg = WasmEncoderBindgen::new(&sizes, 0, &mut indices);
        lift_from_memory(&resolve, &mut bg, (), &Type::Id(result_id));

        assert_eq!(
            count(&bg, |i| matches!(i, Instruction::I64ExtendI32U)),
            1,
            "ok (list) arm should widen Pointer to PointerOrI64"
        );
    }
}
