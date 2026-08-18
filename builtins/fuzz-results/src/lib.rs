//! Type-driven virtualize (fuzz) strategy, the caller is exercised
//! against an adversarial provider.

mod bindings {
    wit_bindgen::generate!({
        world: "consumer",
        generate_all,
    });
}

include!(concat!(env!("OUT_DIR"), "/builtin_config_codegen.rs"));

mod fuzz_builder;

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use fuzz_builder::{FuzzBuilder, GenConfig};
use splicer_tool_sdk::{
    build_typed, sanitize_for_filename, strip_leading_slashes, CallId, VirtualizeStrategy, WitTyped,
};

pub struct FuzzResults {
    base_seed: u64,
    counter: AtomicU64,
    cfg: GenConfig,
    log: config::Log,
    sink: config::Sink,
    log_file: Mutex<Option<std::fs::File>>,
}

impl Default for FuzzResults {
    fn default() -> Self {
        let configured = config::seed();
        // `0` means "no fixed seed": draw a base from the host RNG. The
        // per-call seed is logged regardless, so random runs stay
        // reproducible.
        let base_seed = if configured == 0 {
            random_u64()
        } else {
            configured
        };
        Self {
            base_seed,
            counter: AtomicU64::new(0),
            cfg: GenConfig {
                max_depth: config::max_depth(),
                max_list_len: config::max_list_len(),
                max_string_len: config::max_string_len(),
                boundary_bias: config::boundary_bias(),
            },
            log: config::log(),
            sink: config::sink(),
            log_file: Mutex::new(None),
        }
    }
}

impl<Args, R: WitTyped> VirtualizeStrategy<Args, R> for FuzzResults {
    async fn handle(&self, call: CallId, _args: Args) -> R {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let seed = self.base_seed.wrapping_add(n);
        let mut builder = FuzzBuilder::new(seed, &self.cfg);
        let r: R = build_typed::<R, _>(&mut builder, self.cfg.max_depth).unwrap_or_else(|e| {
            panic!(
                "[fuzz-results] {}#{}: value generation failed: {e}",
                call.interface_name, call.function_name
            )
        });
        self.emit_log(&call, seed, &r);
        r
    }
}

impl FuzzResults {
    fn emit_log<R: WitTyped>(&self, call: &CallId, seed: u64, r: &R) {
        let line = match self.log {
            config::Log::None => return,
            config::Log::Summary => format!(
                "[fuzz-results] {}#{} id={} seed={}",
                call.interface_name, call.function_name, call.id, seed
            ),
            config::Log::Value => {
                let rendered = splicer_tool_sdk::wasm_wave::to_string(&r.to_value())
                    .unwrap_or_else(|_| "<unrenderable>".to_string());
                format!(
                    "[fuzz-results] {}#{} id={} seed={} value={}",
                    call.interface_name, call.function_name, call.id, seed, rendered
                )
            }
        };
        match self.sink {
            config::Sink::Stdout => println!("{line}"),
            config::Sink::Stderr => eprintln!("{line}"),
            config::Sink::File => self.write_to_file(&line),
        }
    }

    /// Append `line` to this edge's log file, opening it lazily. Falls
    /// back to stderr if the file can't be opened (e.g. no filesystem
    /// preopen), so logs are never silently dropped.
    fn write_to_file(&self, line: &str) {
        let mut guard = self.log_file.lock().expect("fuzz-results log file poisoned");
        if guard.is_none() {
            *guard = open_log_file();
        }
        match guard.as_mut() {
            Some(f) => {
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
            }
            None => eprintln!("{line}"),
        }
    }
}

fn open_log_file() -> Option<std::fs::File> {
    let dir = strip_leading_slashes(config::dir());
    let file = format!("{}.log", sanitize_for_filename(&edge_id()));
    let path = if dir.is_empty() {
        file
    } else {
        let _ = std::fs::create_dir_all(dir);
        format!("{dir}/{file}")
    };
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

/// Splicer-injected edge identifier, so each spliced instance writes to
/// its own file. The fallback only matters outside a splice.
fn edge_id() -> String {
    crate::bindings::splicer::builtin_config::get::get("_splicer_edge_id")
        .unwrap_or_else(|| "unknown-edge".to_string())
}

/// Draw a fresh 64-bit base seed from the host RNG.
fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("host random_get is available via the wasip1 adapter");
    u64::from_le_bytes(b)
}
