//! Tier-3 and tier-4 strategy traits. Each user-authored strategy
//! implements **exactly one** of [`TransformStrategy`] (tier-3) or
//! [`VirtualizeStrategy`] (tier-4); splicer's per-target codegen
//! template reads the strategy crate's source to find which trait is
//! impl'd and emits the matching wrapper shape.
//!
//! Picking the wrong trait for your intent is a compile error in the
//! generated wrapper crate, not a runtime panic — the codegen calls
//! the trait it found, so there is no way for behavior and tier
//! classification to diverge.
//!
//! See `docs/tiers/tier-3.md` and `docs/tiers/tier-4.md` for the
//! tier definitions; `docs/TODO/tier3-tier4-substrate.md` for the
//! substrate design.

use crate::types::CallId;

/// **Tier-3 (forward) strategy.** Implement this when your
/// middleware forwards each call to the wrapped target, optionally
/// transforming arguments before or the result after.
///
/// The codegen-emitted wrapper imports the target's interface and
/// gives you a `downstream` closure that invokes it. Retry, latency,
/// rate-limit, redact, normalize, default-fill, clamp, log, and
/// memoize all fit this shape.
///
/// `Args` and `R` are generic at the *trait* level so each strategy
/// can narrow accepted call shapes via its impl's where-clause —
/// e.g. retry requires `R: IntoResult`, memoize requires `Args: Hash`.
/// Strategies that accept any shape leave both unconstrained.
///
/// # Example: log every wrapped call, then forward
///
/// ```ignore
/// use splicer_tool_sdk::{CallId, TransformStrategy};
///
/// struct LogCalls;
///
/// impl<Args, R> TransformStrategy<Args, R> for LogCalls {
///     async fn handle(
///         &self,
///         call: CallId,
///         args: Args,
///         downstream: impl AsyncFnOnce(Args) -> R,
///     ) -> R {
///         println!("calling {}#{}", call.interface_name, call.function_name);
///         downstream(args).await
///     }
/// }
/// ```
///
/// # Per-strategy state
///
/// Strategies are constructed once per wrapper component instance and
/// reused across every wrapped call. Use struct fields for persistent
/// state (an RNG, a retry counter, a memoization cache); wrap mutable
/// fields in `RefCell` / `Cell` and avoid holding the borrow across
/// `downstream(args).await` so concurrent canon-async calls into the
/// wrapper don't collide.
// We intentionally omit a `Send` bound on the returned future:
// generated wrappers run in single-threaded wasm components.
#[allow(async_fn_in_trait)]
pub trait TransformStrategy<Args, R> {
    /// Handle one wrapped invocation. `downstream` invokes the
    /// wrapped target; call it (with original or mutated `args`),
    /// then optionally mutate and return its result.
    async fn handle(
        &self,
        call: CallId,
        args: Args,
        downstream: impl AsyncFnOnce(Args) -> R,
    ) -> R;
}

/// **Tier-4 (virtualize) strategy.** Implement this when your
/// middleware replaces the wrapped target — producing `R` from
/// internal state without ever invoking the target.
///
/// The codegen-emitted wrapper does NOT import the target's
/// interface; replay traces, fuzzers, mocks, and chaos generators
/// are all virtualizers. Because `handle` has no `downstream`
/// parameter, a virtualize strategy *physically cannot* call the
/// target — the tier classification is enforced by the trait
/// signature.
///
/// `Args` and `R` are generic at the *trait* level so each strategy
/// can narrow accepted call shapes via its impl's where-clause —
/// e.g. replay requires `R: WitTyped` (so [`crate::cells_to_typed`]
/// can decode a recorded cells stream into `R`), fuzz requires
/// `Args: Arbitrary`.
///
/// # Example: synthesize the default value for any return type
///
/// ```ignore
/// use splicer_tool_sdk::{CallId, VirtualizeStrategy};
///
/// struct ReturnDefault;
///
/// // The `R: Default` bound narrows which target WIT shapes this
/// // strategy accepts. Wrappers whose return types do not satisfy
/// // `Default` won't compile, with a precise error.
/// impl<Args, R: Default> VirtualizeStrategy<Args, R> for ReturnDefault {
///     async fn handle(&self, _call: CallId, _args: Args) -> R {
///         R::default()
///     }
/// }
/// ```
///
/// # Per-strategy state
///
/// Same lifecycle and concurrency story as [`TransformStrategy`]:
/// one instance per wrapper component, mutate via interior
/// mutability, don't hold borrows across `.await`.
#[allow(async_fn_in_trait)]
pub trait VirtualizeStrategy<Args, R> {
    /// Handle one wrapped invocation. Synthesize `R` from internal
    /// state — there is no downstream to forward to.
    async fn handle(&self, call: CallId, args: Args) -> R;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pass-through forward: hands args to downstream unchanged.
    struct PassThrough;

    impl<Args, R> TransformStrategy<Args, R> for PassThrough {
        async fn handle(
            &self,
            _call: CallId,
            args: Args,
            downstream: impl AsyncFnOnce(Args) -> R,
        ) -> R {
            downstream(args).await
        }
    }

    /// Default-stub virtualize: synthesizes a default `R`. Narrows
    /// `R` via the impl-level bound without touching the trait.
    struct DefaultStub;

    impl<Args, R: Default> VirtualizeStrategy<Args, R> for DefaultStub {
        async fn handle(&self, _call: CallId, _args: Args) -> R {
            R::default()
        }
    }

    fn call() -> CallId {
        CallId {
            interface_name: "example:iface/foo@0.1.0".into(),
            function_name: "do-thing".into(),
            id: 1,
        }
    }

    #[tokio::test]
    async fn forward_pass_through_hands_off_args() {
        let strat = PassThrough;
        let r: u32 = strat
            .handle(call(), (10u32, 20u32), |(a, b)| async move { a + b })
            .await;
        assert_eq!(r, 30);
    }

    #[tokio::test]
    async fn virtualize_stub_synthesizes_default() {
        let strat = DefaultStub;
        let r: u32 = VirtualizeStrategy::<(u32, u32), u32>::handle(
            &strat,
            call(),
            (10, 20),
        )
        .await;
        assert_eq!(r, 0);
    }
}
