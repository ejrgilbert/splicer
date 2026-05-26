//! End-to-end round-trip tests for the wire format: encode known
//! values, decode the bytes back, assert equality. Catches drift
//! between `encode.rs` and `decode.rs` independent of any builtin.

use splicer_tool_sdk::{
    write_call_event, write_return_event, write_stream_header, CallId, Cell, DecodeError, EnumInfo,
    Event, Field, FieldTree, FlagsInfo, HandleInfo, Reader, RecordInfo, VariantInfo,
};

fn sample_call_id() -> CallId {
    CallId {
        interface_name: "wasi:http/handler@0.3.0".into(),
        function_name: "handle".into(),
        id: 42,
    }
}

/// Run `write` against a freshly-headered buffer, then decode every
/// event back. The header step is implicit so each test exercises a
/// full encode + decode cycle without restating it.
fn round_trip<F: FnOnce(&mut Vec<u8>)>(write: F) -> Vec<Event> {
    let mut buf = Vec::new();
    write_stream_header(&mut buf);
    write(&mut buf);
    Reader::new(&buf)
        .expect("valid stream header")
        .collect::<Result<_, _>>()
        .expect("all events decode")
}

/// Round-trip a single event and assert there's exactly one in the
/// stream. Most encode-one-decode-one tests use this.
fn round_trip_one<F: FnOnce(&mut Vec<u8>)>(write: F) -> Event {
    let mut events = round_trip(write);
    assert_eq!(events.len(), 1, "expected exactly one event");
    events.pop().unwrap()
}

/// Encode a call event, decode, assert the decoded fields match the
/// originals. `#[track_caller]` so failures point at the test, not here.
#[track_caller]
fn assert_call_roundtrips(ts_ns: u64, call: &CallId, args: &[Field]) {
    let ev = round_trip_one(|buf| write_call_event(buf, ts_ns, call, args));
    match ev {
        Event::Call { ts_ns: got_ts, call: got_call, args: got_args } => {
            assert_eq!(got_ts, ts_ns);
            assert_eq!(&got_call, call);
            assert_eq!(got_args.as_slice(), args);
        }
        other => panic!("expected Call, got {other:?}"),
    }
}

#[track_caller]
fn assert_return_roundtrips(ts_ns: u64, call: &CallId, result: Option<&FieldTree>) {
    let ev = round_trip_one(|buf| write_return_event(buf, ts_ns, call, result));
    match ev {
        Event::Return { ts_ns: got_ts, call: got_call, result: got_result } => {
            assert_eq!(got_ts, ts_ns);
            assert_eq!(&got_call, call);
            assert_eq!(got_result.as_ref(), result);
        }
        other => panic!("expected Return, got {other:?}"),
    }
}

#[test]
fn empty_call_round_trips() {
    assert_call_roundtrips(1_700_000_000_000_000_000, &sample_call_id(), &[]);
}

#[test]
fn void_return_round_trips() {
    assert_return_roundtrips(999, &sample_call_id(), None);
}

#[test]
fn every_cell_variant_round_trips() {
    // One cell of every variant (or both arms, for OptionSome/None and
    // the with/without-payload variants) packed into a single tree.
    // Indices are contrived but self-consistent.
    let tree = FieldTree {
        cells: vec![
            Cell::Bool(true),
            Cell::Bool(false),
            Cell::Integer(i64::MIN),
            Cell::Integer(i64::MAX),
            Cell::Integer(0),
            Cell::Floating(3.14159),
            Cell::Floating(-0.0),
            Cell::Text("hello, 🦀".into()),
            Cell::Text("".into()),
            Cell::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            Cell::Bytes(vec![]),
            Cell::ListOf(vec![0, 1, 2]),
            Cell::TupleOf(vec![3, 4, 5]),
            Cell::OptionSome(7),
            Cell::OptionNone,
            Cell::ResultOk(Some(0)),
            Cell::ResultOk(None),
            Cell::ResultErr(Some(1)),
            Cell::ResultErr(None),
            Cell::RecordOf(0),
            Cell::FlagsSet(0),
            Cell::EnumCase(0),
            Cell::VariantCase(0),
            Cell::VariantCase(1),
            Cell::ResourceHandle(0),
            Cell::StreamHandle(1),
            Cell::FutureHandle(2),
            Cell::ErrorContextHandle(3),
        ],
        record_infos: vec![RecordInfo {
            type_name: "request".into(),
            fields: vec![("method".into(), 7), ("body".into(), 9)],
        }],
        flags_infos: vec![FlagsInfo {
            type_name: "perms".into(),
            set_flags: vec!["read".into(), "write".into()],
        }],
        enum_infos: vec![EnumInfo {
            type_name: "color".into(),
            case_name: "red".into(),
        }],
        variant_infos: vec![
            VariantInfo {
                type_name: "event".into(),
                case_name: "keypress".into(),
                payload: Some(7),
            },
            VariantInfo {
                type_name: "event".into(),
                case_name: "idle".into(),
                payload: None,
            },
        ],
        handle_infos: vec![
            HandleInfo { type_name: "request".into(), id: 7 },
            HandleInfo { type_name: "output-stream".into(), id: 11 },
            HandleInfo { type_name: "u32".into(), id: 13 },
            HandleInfo { type_name: "".into(), id: 17 },
        ],
        root: 0,
    };

    let arg = Field {
        name: "req".into(),
        tree,
    };
    assert_call_roundtrips(12345, &sample_call_id(), std::slice::from_ref(&arg));
}

#[test]
fn return_with_field_tree_round_trips() {
    let tree = FieldTree {
        cells: vec![Cell::Text("ok".into())],
        record_infos: vec![],
        flags_infos: vec![],
        enum_infos: vec![],
        variant_infos: vec![],
        handle_infos: vec![],
        root: 0,
    };
    assert_return_roundtrips(0, &sample_call_id(), Some(&tree));
}

#[test]
fn multiple_events_round_trip_in_order() {
    let call = sample_call_id();
    let events = round_trip(|buf| {
        write_call_event(buf, 1, &call, &[]);
        write_return_event(buf, 2, &call, None);
        write_call_event(buf, 3, &call, &[]);
        write_return_event(buf, 4, &call, None);
    });
    assert_eq!(events.len(), 4);
    let timestamps: Vec<u64> = events
        .iter()
        .map(|e| match e {
            Event::Call { ts_ns, .. } | Event::Return { ts_ns, .. } => *ts_ns,
        })
        .collect();
    assert_eq!(timestamps, vec![1, 2, 3, 4]);
    assert!(matches!(events[0], Event::Call { .. }));
    assert!(matches!(events[1], Event::Return { .. }));
    assert!(matches!(events[2], Event::Call { .. }));
    assert!(matches!(events[3], Event::Return { .. }));
}

#[test]
fn truncated_stream_returns_none() {
    let call = sample_call_id();
    let mut buf = Vec::new();
    write_stream_header(&mut buf);
    write_call_event(&mut buf, 0, &call, &[]);
    // Drop the last 5 bytes; rec_len now claims more bytes than remain.
    buf.truncate(buf.len() - 5);

    let mut reader = Reader::new(&buf).expect("header still valid");
    assert!(reader.next().is_none(), "truncated mid-event = clean EOF per spec");
}

#[test]
fn bad_magic_rejected() {
    let bytes = b"XXXX\x01\x00\x00\x00";
    match Reader::new(bytes) {
        Err(DecodeError::BadMagic(m)) => assert_eq!(&m, b"XXXX"),
        other => panic!("expected BadMagic, got {other:?}"),
    }
}

#[test]
fn unsupported_version_rejected() {
    let mut bytes = b"SPLR".to_vec();
    bytes.extend_from_slice(&999u32.to_le_bytes());
    match Reader::new(&bytes) {
        Err(DecodeError::UnsupportedVersion(v)) => assert_eq!(v, 999),
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}
