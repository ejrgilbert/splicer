//! Recorder: emits a binary stream of every wrapped call's lifted
//! args + result to a configurable sink. The `file` sink is the only
//! one safe for multi-edge splices; `stdout`/`stderr` are single-
//! instance debug aids. Wire format and encoding live in
//! `splicer-tool-sdk`; this crate only owns the hook state, timestamp
//! source, and sink selection.

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
use crate::bindings::wasi::filesystem::preopens as fs_preopens;
use crate::bindings::wasi::filesystem::types::{
    Descriptor, DescriptorFlags, OpenFlags, PathFlags,
};
use crate::bindings::wasi::io::streams::OutputStream;
use splicer_tool_sdk::{CallId, Field, FieldTree};

struct State {
    buf: Vec<u8>,
    header_written: bool,
    /// Lazily-opened append stream for the `file` sink. Held alongside
    /// the descriptor so the file stays open across flushes — opening
    /// per flush would re-create on every call and is expensive.
    file: Option<(Descriptor, OutputStream)>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            buf: Vec::new(),
            header_written: false,
            file: None,
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
        flush(&mut s);
    }
}

fn ensure_header(s: &mut State) {
    if !s.header_written {
        splicer_tool_sdk::write_stream_header(&mut s.buf);
        s.header_written = true;
    }
}

fn flush(s: &mut State) {
    if s.buf.is_empty() {
        return;
    }
    match config::sink() {
        config::Sink::Stdout => drain_to_io(&mut s.buf, std::io::stdout().lock()),
        config::Sink::Stderr => drain_to_io(&mut s.buf, std::io::stderr().lock()),
        config::Sink::File => drain_to_file(s),
    }
}

fn drain_to_io<W: Write>(buf: &mut Vec<u8>, mut out: W) {
    let _ = out.write_all(buf);
    let _ = out.flush();
    shrink_after_flush(buf);
}

fn drain_to_file(s: &mut State) {
    let (_desc, stream) = s
        .file
        .get_or_insert_with(|| open_file_for_edge(edge_id()));
    // Best-effort: a stream error mid-recording isn't recoverable, but
    // also isn't worth panicking over — match the io::Write behavior.
    let _ = stream.blocking_write_and_flush(&s.buf);
    shrink_after_flush(&mut s.buf);
}

fn shrink_after_flush(buf: &mut Vec<u8>) {
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

/// Open (or create) the recording file for this edge under
/// `<preopen>/<config::dir()>/<sanitized_edge_id>.bin` and return an
/// append-mode stream into it. Panics if the host hasn't preopened any
/// directory — the recorder has no fallback story for that misconfig.
fn open_file_for_edge(edge_id: &str) -> (Descriptor, OutputStream) {
    let dirs = fs_preopens::get_directories();
    let (root, _name) = dirs
        .into_iter()
        .next()
        .expect("recorder requires at least one wasi:filesystem preopen");
    let dir = relative_dir(&config::dir());
    // Lazily mkdir -p each segment so nested defaults like
    // `recordings/run-2026-05-20/` work without host pre-seeding. Each
    // create_directory_at is single-level; existing dirs error out and
    // we ignore.
    let mut path = String::new();
    for seg in dir.split('/').filter(|s| !s.is_empty()) {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(seg);
        let _ = root.create_directory_at(&path);
    }
    let file_path = if path.is_empty() {
        format!("{}.bin", sanitize_for_filename(edge_id))
    } else {
        format!("{path}/{}.bin", sanitize_for_filename(edge_id))
    };
    let file = root
        .open_at(
            PathFlags::empty(),
            &file_path,
            OpenFlags::CREATE,
            DescriptorFlags::WRITE,
        )
        .expect("open recording file");
    let stream = file
        .append_via_stream()
        .expect("append-via-stream on recording file");
    (file, stream)
}

/// Strip a leading `./` (and any leading slashes) so the result is
/// safe to feed segment-by-segment into `create_directory_at` against
/// the preopen. wasi:filesystem rejects absolute paths and `.`/`..`
/// components; the manifest's default `./recordings` would otherwise
/// trip the `.` rejection.
fn relative_dir(raw: &str) -> &str {
    raw.trim_start_matches("./").trim_start_matches('/')
}

/// Filesystem-safe form of `edge_id`. Mirrors `splicer::edge_id::
/// sanitize_for_filename` — kept in lockstep manually since both sides
/// must agree (replayer reads what recorder wrote). Replace `[^A-Za-
/// z0-9._@-]` with `_`.
fn sanitize_for_filename(edge_id: &str) -> String {
    edge_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' | '@' => c,
            _ => '_',
        })
        .collect()
}

/// Splicer-injected edge identifier for this recorder instance,
/// fetched once via the config substrate. Splicer guarantees this key
/// is present on every spliced recorder; the fallback only matters if
/// someone wires the recorder up without going through splicer.
fn edge_id() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        crate::bindings::splicer::builtin_config::get::get("_splicer_edge_id")
            .unwrap_or_else(|| "unknown-edge".to_string())
    })
}

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
