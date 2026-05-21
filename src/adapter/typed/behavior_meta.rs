//! `Behavior` enum: the codegen template's transform/virtualize knob.

/// What the strategy does to the wrapped target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// Strategy transforms the call — receives typed args + result
    /// and may mutate either before forwarding to the wrapped target.
    /// Wrapper imports the target's interface.
    Transform,
    /// Strategy replaces the wrapped target. Wrapper does not import
    /// the target's interface.
    Virtualize,
}
