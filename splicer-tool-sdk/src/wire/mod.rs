//! Binary SPLR wire format: a stream of lifted `FieldTree` events (a
//! header followed by call/return events). [`encode`] writes it;
//! [`decode`] reads it back. The layout is specified in
//! `wire-format.md` at the crate root.

pub mod decode;
pub mod encode;
