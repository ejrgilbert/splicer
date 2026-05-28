//! `TraceReader`: a high-level reader over a binary event stream.
//!
//! Given the raw bytes of a stream in the [`wire`](crate::wire) format
//! (a `SPLR` header followed by call/return events), `TraceReader`
//! decodes it eagerly into owned [`Event`]s, exposing them both as a
//! whole slice and through a forward cursor over the calls and returns,
//! either of which can be decoded into typed Rust on demand.
//!
//! Scope: one decoded stream per reader (aggregating several is the
//! caller's job), and the cursor is forward-only, so events come back
//! in stream order with no random access or keyed lookup. It is a
//! reader, not a correlator: events are surfaced exactly as decoded,
//! with nothing tying one to another.
//!
//! Decoding is host-agnostic: callers hand in bytes and this module
//! never touches the filesystem itself.

use std::fmt;

use crate::wire::decode::{DecodeError, Event, Reader};
use crate::bridge::{args_to_typed, cells_to_typed, BridgeError, WitTyped};

/// A single decoded stream, plus a forward cursor over its call and
/// return events.
#[derive(Clone, Debug)]
pub struct TraceReader {
    events: Vec<Event>,
    cursor: usize,
}

impl TraceReader {
    /// Decode a whole stream from its raw bytes: the `SPLR` header
    /// followed by call/return events. The header and every event are
    /// validated up front, so a malformed stream fails here rather than
    /// partway through consumption.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let events = Reader::new(bytes)?.collect::<Result<Vec<_>, _>>()?;
        Ok(Self { events, cursor: 0 })
    }

    /// All events in stream order. Use this for whole-trace access;
    /// use the cursor methods for sequential consumption of calls or
    /// returns.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Number of decoded events (calls and returns).
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the trace contains no events.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Reset the cursor to the start of the trace.
    pub fn reset(&mut self) {
        self.cursor = 0;
    }

    /// Advance the cursor to the next call event, skipping any
    /// intervening return events, and return it. `None` once the stream
    /// holds no further calls.
    ///
    /// All cursor methods share one position. To decode the call's
    /// arguments as a tuple, use
    /// [`next_call_typed`](Self::next_call_typed); for per-argument
    /// access, each arg is a `Field` whose `tree` is self-contained, so
    /// [`cells_to_typed`](crate::cells_to_typed) over `&field.tree`
    /// at `field.tree.root` decodes one.
    pub fn next_call(&mut self) -> Option<&Event> {
        self.advance_to(|e| matches!(e, Event::Call { .. }))
    }

    /// Advance the cursor to the next return event, skipping any
    /// intervening call events, and return it. `None` once the stream
    /// holds no further returns.
    pub fn next_return(&mut self) -> Option<&Event> {
        self.advance_to(|e| matches!(e, Event::Return { .. }))
    }

    /// Advance the cursor to the next event satisfying `want`.
    fn advance_to(&mut self, want: fn(&Event) -> bool) -> Option<&Event> {
        while self.cursor < self.events.len() {
            let idx = self.cursor;
            self.cursor += 1;
            if want(&self.events[idx]) {
                return Some(&self.events[idx]);
            }
        }
        None
    }

    /// Advance to the next return and decode its result tree into `R`.
    /// Counterpart to [`next_call_typed`](Self::next_call_typed).
    ///
    /// Errors: [`TraceError::Exhausted`] when no return remains;
    /// [`TraceError::VoidReturn`] when the function returned nothing
    /// (use [`next_return`](Self::next_return) and inspect `result` for
    /// void functions); [`TraceError::Bridge`] when the cells do not
    /// decode into `R` (schema drift between the stream and `R`'s WIT
    /// type).
    pub fn next_return_typed<R: WitTyped>(&mut self) -> Result<R, TraceError> {
        let Event::Return { result, .. } = self.next_return().ok_or(TraceError::Exhausted)? else {
            unreachable!("next_return only yields Event::Return");
        };
        let tree = result.as_ref().ok_or(TraceError::VoidReturn)?;
        cells_to_typed::<R>(tree, tree.root).map_err(TraceError::Bridge)
    }

    /// Advance to the next call and decode its arguments into the tuple
    /// `Args`. Counterpart to
    /// [`next_return_typed`](Self::next_return_typed).
    ///
    /// `Args` is the tuple of the call's argument types (e.g. `(A, B)`
    /// for a two-argument function). A zero-argument call has no tuple
    /// form: read it with [`next_call`](Self::next_call) instead.
    ///
    /// Errors: [`TraceError::Exhausted`] when no call remains;
    /// [`TraceError::Bridge`] when the arguments do not decode into
    /// `Args`, including an arity mismatch.
    pub fn next_call_typed<Args: WitTyped>(&mut self) -> Result<Args, TraceError> {
        let Event::Call { args, .. } = self.next_call().ok_or(TraceError::Exhausted)? else {
            unreachable!("next_call only yields Event::Call");
        };
        args_to_typed::<Args>(args).map_err(TraceError::Bridge)
    }
}

/// Failure modes when pulling a typed value off a trace.
#[derive(Debug)]
pub enum TraceError {
    /// No return event remains in the stream.
    Exhausted,
    /// The return carried no result tree (void function).
    VoidReturn,
    /// Decoding the cells into the requested Rust type failed.
    Bridge(BridgeError),
}

impl fmt::Display for TraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exhausted => write!(f, "stream holds no further return events"),
            Self::VoidReturn => write!(f, "return carried no result tree"),
            Self::Bridge(e) => write!(f, "cells did not decode into the requested type: {e}"),
        }
    }
}

impl std::error::Error for TraceError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::encode::{write_call_event, write_return_event, write_stream_header};
    use crate::types::{Cell, CallId, Field, FieldTree};

    fn call(id: u64) -> CallId {
        CallId {
            interface_name: "ex:i/face@0.1.0".into(),
            function_name: "do-thing".into(),
            id,
        }
    }

    fn int_tree(n: i64) -> FieldTree {
        FieldTree {
            cells: vec![Cell::Integer(n)],
            record_infos: vec![],
            flags_infos: vec![],
            enum_infos: vec![],
            variant_infos: vec![],
            handle_infos: vec![],
            root: 0,
        }
    }

    #[test]
    fn next_return_typed_decodes_return() {
        let mut buf = Vec::new();
        write_stream_header(&mut buf);
        write_call_event(&mut buf, 1, &call(1), &[]);
        write_return_event(&mut buf, 2, &call(1), Some(&int_tree(42)));

        let mut tr = TraceReader::from_bytes(&buf).unwrap();
        let v: u32 = tr.next_return_typed().unwrap();
        assert_eq!(v, 42);
    }

    #[test]
    fn next_call_yields_calls_with_decodable_args() {
        let mut buf = Vec::new();
        write_stream_header(&mut buf);
        let args = vec![Field {
            name: "n".into(),
            tree: int_tree(7),
        }];
        write_call_event(&mut buf, 1, &call(1), &args);
        write_return_event(&mut buf, 2, &call(1), Some(&int_tree(7)));

        let mut tr = TraceReader::from_bytes(&buf).unwrap();
        let Some(Event::Call { args, .. }) = tr.next_call() else {
            panic!("expected a call event");
        };
        let n: u32 = cells_to_typed(&args[0].tree, args[0].tree.root).unwrap();
        assert_eq!(n, 7);
    }

    #[test]
    fn next_call_typed_decodes_args_tuple() {
        let mut buf = Vec::new();
        write_stream_header(&mut buf);
        let args = vec![
            Field {
                name: "a".into(),
                tree: int_tree(3),
            },
            Field {
                name: "b".into(),
                tree: int_tree(4),
            },
        ];
        write_call_event(&mut buf, 1, &call(1), &args);

        let mut tr = TraceReader::from_bytes(&buf).unwrap();
        let (a, b): (u32, u64) = tr.next_call_typed().unwrap();
        assert_eq!((a, b), (3, 4));
    }

    #[test]
    fn next_call_typed_arity_mismatch_errors() {
        let mut buf = Vec::new();
        write_stream_header(&mut buf);
        let args = vec![Field {
            name: "a".into(),
            tree: int_tree(3),
        }];
        write_call_event(&mut buf, 1, &call(1), &args);

        let mut tr = TraceReader::from_bytes(&buf).unwrap();
        assert!(matches!(
            tr.next_call_typed::<(u32, u32)>(),
            Err(TraceError::Bridge(_))
        ));
    }

    #[test]
    fn next_call_skips_return_events() {
        let mut buf = Vec::new();
        write_stream_header(&mut buf);
        write_return_event(&mut buf, 1, &call(1), Some(&int_tree(1)));
        write_call_event(&mut buf, 2, &call(2), &[]);

        let mut tr = TraceReader::from_bytes(&buf).unwrap();
        let Some(Event::Call { call, .. }) = tr.next_call() else {
            panic!("expected a call event");
        };
        assert_eq!(call.id, 2);
    }

    #[test]
    fn next_return_skips_call_events_and_events_keep_order() {
        let mut buf = Vec::new();
        write_stream_header(&mut buf);
        write_call_event(&mut buf, 1, &call(1), &[]);
        write_call_event(&mut buf, 2, &call(2), &[]);
        write_return_event(&mut buf, 3, &call(1), Some(&int_tree(99)));

        let mut tr = TraceReader::from_bytes(&buf).unwrap();
        assert_eq!(tr.events().len(), 3);
        assert!(matches!(tr.next_return(), Some(Event::Return { .. })));
    }

    #[test]
    fn next_return_typed_exhausted_after_last_return() {
        let mut buf = Vec::new();
        write_stream_header(&mut buf);
        write_return_event(&mut buf, 1, &call(1), Some(&int_tree(7)));

        let mut tr = TraceReader::from_bytes(&buf).unwrap();
        assert_eq!(tr.next_return_typed::<u32>().unwrap(), 7);
        assert!(matches!(tr.next_return_typed::<u32>(), Err(TraceError::Exhausted)));
    }

    #[test]
    fn next_return_typed_void_return_errors() {
        let mut buf = Vec::new();
        write_stream_header(&mut buf);
        write_return_event(&mut buf, 1, &call(1), None);

        let mut tr = TraceReader::from_bytes(&buf).unwrap();
        assert!(matches!(tr.next_return_typed::<u32>(), Err(TraceError::VoidReturn)));
    }

    #[test]
    fn reset_rereads_from_start() {
        let mut buf = Vec::new();
        write_stream_header(&mut buf);
        write_return_event(&mut buf, 1, &call(1), Some(&int_tree(5)));

        let mut tr = TraceReader::from_bytes(&buf).unwrap();
        assert_eq!(tr.next_return_typed::<u32>().unwrap(), 5);
        assert!(matches!(tr.next_return_typed::<u32>(), Err(TraceError::Exhausted)));
        tr.reset();
        assert_eq!(tr.next_return_typed::<u32>().unwrap(), 5);
    }

    #[test]
    fn empty_trace_has_no_returns() {
        let mut buf = Vec::new();
        write_stream_header(&mut buf);

        let mut tr = TraceReader::from_bytes(&buf).unwrap();
        assert!(tr.is_empty());
        assert!(tr.next_return().is_none());
    }

    #[test]
    fn from_bytes_rejects_bad_magic() {
        let err = TraceReader::from_bytes(b"XXXX\x01\x00\x00\x00").unwrap_err();
        assert!(matches!(err, DecodeError::BadMagic(_)));
    }
}
