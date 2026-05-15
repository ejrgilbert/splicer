//! Recorder: emits a binary stream of every wrapped call's lifted
//! args + result to stdout or stderr (configurable). Wire format and
//! encoding live in `splicer-tool-sdk`; this crate only owns the hook
//! state, timestamp source, and sink selection.

mod bindings {
    splicer_tool_sdk::wit_bindgen!({
        world: "recorder-mdl",
        async: [
            "export:splicer:tier2/before@0.1.0#on-call",
            "export:splicer:tier2/after@0.1.0#on-return",
        ],
        generate_all,
    });
}

include!(concat!(env!("OUT_DIR"), "/builtin_config_codegen.rs"));

use std::io::Write;
use std::sync::{Mutex, OnceLock};

use crate::bindings::exports::splicer::tier2::after::Guest as AfterGuest;
use crate::bindings::exports::splicer::tier2::before::Guest as BeforeGuest;
use crate::bindings::wasi::clocks::wall_clock::now as wall_now;
use splicer_tool_sdk::{CallId, Field, FieldTree};

struct State {
    buf: Vec<u8>,
    header_written: bool,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            buf: Vec::new(),
            header_written: false,
        })
    })
}

pub struct Recorder;

impl BeforeGuest for Recorder {
    async fn on_call(call: CallId, args: Vec<Field>) {
        let mut s = state().lock().unwrap();
        ensure_header(&mut s);
        splicer_tool_sdk::write_call_event(&mut s.buf, wall_now_ns(), &call, &args);
    }
}

impl AfterGuest for Recorder {
    async fn on_return(call: CallId, result: Option<FieldTree>) {
        let mut s = state().lock().unwrap();
        ensure_header(&mut s);
        splicer_tool_sdk::write_return_event(&mut s.buf, wall_now_ns(), &call, result.as_ref());
        flush(&mut s.buf);
    }
}

fn ensure_header(s: &mut State) {
    if !s.header_written {
        splicer_tool_sdk::write_stream_header(&mut s.buf);
        s.header_written = true;
    }
}

fn flush(buf: &mut Vec<u8>) {
    if buf.is_empty() {
        return;
    }
    match config::sink() {
        config::Sink::Stdout => drain_to(buf, std::io::stdout().lock()),
        config::Sink::Stderr => drain_to(buf, std::io::stderr().lock()),
    }
}

fn drain_to<W: Write>(buf: &mut Vec<u8>, mut out: W) {
    let _ = out.write_all(buf);
    let _ = out.flush();
    buf.clear();
    // Bound the high-water-mark so one giant tree doesn't pin memory
    // for the rest of the instance's life. Keep a small floor capacity
    // so typical small-event traffic isn't reallocating each flush.
    if buf.capacity() > BUF_FLOOR_CAPACITY {
        buf.shrink_to(BUF_FLOOR_CAPACITY);
    }
}

/// Capacity the recorder buffer is shrunk back to after flush, when a
/// preceding event temporarily grew it past this.
const BUF_FLOOR_CAPACITY: usize = 8 * 1024;

/// Convert `wasi:clocks/wall-clock.datetime` to u64 ns since epoch.
/// Saturating is defensive; for any plausible wall-clock value the
/// product fits in u64 (u64::MAX ns is ~584 years).
fn wall_now_ns() -> u64 {
    let dt = wall_now();
    dt.seconds
        .saturating_mul(1_000_000_000)
        .saturating_add(dt.nanoseconds as u64)
}

bindings::export!(Recorder with_types_in bindings);
