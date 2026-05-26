//! Tier-2 test root. Each submodule covers one slice of the
//! adapter pipeline; the fuzz submodule layers a structural
//! property test on top of the same generator entry point.

mod blob;
mod cells;
mod dispatch_roundtrip;
mod layout;
