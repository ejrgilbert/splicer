//! Author tier-3 and tier-4 middleware behavior by implementing
//! [`WrapperStrategy`]. Splicer generates a wrapper component for
//! every target WIT you configure your strategy against; the
//! generated wrapper dispatches every wrapped call into your
//! strategy's [`handle`](WrapperStrategy::handle) method.
//!
//! See `docs/tiers/tier-3.md` and `docs/tiers/tier-4.md` for the
//! tier definitions; `docs/TODO/tier3-tier4-substrate.md` for the
//! substrate design.

use crate::types::CallId;

/// Implement this trait once per strategy.
///
/// Your strategy receives the arguments of each wrapped call and
/// decides what happens next:
///
/// - **tier-3** (forward): call `downstream(args).await` and return
///   what it returned, optionally mutating `args` before or `R` after.
/// - **tier-4** (virtualize): ignore `downstream` and produce `R`
///   from internal state (e.g. a replay trace or a fuzzer).
///
/// `Args` and `R` are generic at the *trait* level, not the method
/// level, so each strategy can narrow which call shapes it accepts
/// via its impl's where-clause (e.g. a replay strategy requires
/// `R: TypedFromCells`). Strategies that accept any shape — like the
/// pass-through example below — leave `Args` and `R` unconstrained.
///
/// # Tier-3 example: log every wrapped call, then forward
///
/// ```ignore
/// use splicer_tool_sdk::{CallId, WrapperStrategy};
///
/// struct LogCalls;
///
/// impl<Args, R> WrapperStrategy<Args, R> for LogCalls {
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
/// # Tier-4 example: synthesize the default value for any return type
///
/// ```ignore
/// use splicer_tool_sdk::{CallId, WrapperStrategy};
///
/// struct ReturnDefault;
///
/// // The `R: Default` bound on the impl narrows which target WIT
/// // shapes this strategy can wrap. Wrappers whose return types do
/// // not satisfy `Default` won't compile, with a precise error.
/// impl<Args, R: Default> WrapperStrategy<Args, R> for ReturnDefault {
///     async fn handle(
///         &self,
///         _call: CallId,
///         _args: Args,
///         _downstream: impl AsyncFnOnce(Args) -> R,
///     ) -> R {
///         R::default()
///     }
/// }
/// ```
///
/// # Per-strategy state
///
/// Strategies are constructed once per wrapper component instance and
/// reused across every wrapped call. Use struct fields for persistent
/// state (an RNG, a trace cursor, a memoization cache); wrap mutable
/// fields in `RefCell` / `Cell` and avoid holding the borrow across
/// `downstream(args).await` so concurrent canon-async calls into the
/// wrapper don't collide.
// We intentionally omit a `Send` bound on the returned future:
// generated wrappers run in single-threaded wasm components.
#[allow(async_fn_in_trait)]
pub trait WrapperStrategy<Args, R> {
    /// Handle one wrapped invocation.
    ///
    /// - `call` identifies the wrapped function (interface name,
    ///   function name, and a per-instance monotonic id correlating
    ///   each call site).
    /// - `args` is the function's arguments, packaged as a tuple
    ///   matching the WIT function's positional parameter order.
    ///   No-argument functions get `()`; one-argument functions get
    ///   `(T,)`. Move into `downstream` to forward.
    /// - `downstream` invokes the wrapped target. Calling it makes
    ///   this a tier-3 strategy; skipping it makes this a tier-4
    ///   strategy. May only be called once per `handle` invocation.
    async fn handle(
        &self,
        call: CallId,
        args: Args,
        downstream: impl AsyncFnOnce(Args) -> R,
    ) -> R;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tier-3 pass-through: forwards args to downstream unchanged.
    /// Demonstrates a strategy that places no bounds on `(Args, R)`.
    struct PassThrough;

    impl<Args, R> WrapperStrategy<Args, R> for PassThrough {
        async fn handle(
            &self,
            _call: CallId,
            args: Args,
            downstream: impl AsyncFnOnce(Args) -> R,
        ) -> R {
            downstream(args).await
        }
    }

    /// Tier-4 stub: ignores downstream, synthesizes a default `R`.
    /// Demonstrates a strategy that narrows `R` via an impl-level
    /// bound (`R: Default`) without touching the trait surface.
    struct DefaultStub;

    impl<Args, R: Default> WrapperStrategy<Args, R> for DefaultStub {
        async fn handle(
            &self,
            _call: CallId,
            _args: Args,
            _downstream: impl AsyncFnOnce(Args) -> R,
        ) -> R {
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
    async fn tier3_pass_through_forwards_args() {
        let strat = PassThrough;
        let r: u32 = strat
            .handle(call(), (10u32, 20u32), |(a, b)| async move { a + b })
            .await;
        assert_eq!(r, 30);
    }

    #[tokio::test]
    async fn tier4_stub_ignores_downstream() {
        let strat = DefaultStub;
        let r: u32 = WrapperStrategy::<(u32, u32), u32>::handle(
            &strat,
            call(),
            (10, 20),
            |_| async { panic!("tier-4 strategy must not call downstream") },
        )
        .await;
        assert_eq!(r, 0);
    }
}
