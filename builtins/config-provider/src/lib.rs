//! Provider template for the `splicer:builtin-config` substrate.
//!
//! Exports `splicer:builtin-config/get`. The KV table lives in
//! `SPLICER_CONFIG_BLOB` — a fixed-capacity `static` prefixed with
//! `MAGIC_BYTES` so splicer's `build_provider` can locate and patch
//! it by byte-scan. Unpatched (`payload_len == 0`), every `get`
//! returns `none` and consumers fall back to their hardcoded
//! defaults. Wire format details: see `wire_format.rs`.

mod bindings {
    // NO `async: true`. `get` is sync at WIT and must lift as plain
    // `canon lift` (sync). Substrate consumers (hello-tier1 etc.)
    // canon-lower this import synchronously to avoid wedging when
    // spliced on a sync-WIT target — see
    // `docs/TODO/sync-wit-suspend-limit.md`.
    wit_bindgen::generate!({
        world: "config-provider-mdl",
        generate_all,
    });
}

use std::collections::HashMap;
use std::sync::OnceLock;

use bindings::exports::splicer::builtin_config::get::Guest;

// Wire-format constants + codec shared with splicer's
// `src/config_provider.rs`. See `wire_format.rs` for the
// "only-in-const-eval" invariant on `MAGIC_BYTES`.
mod wire_format;

use wire_format::{
    deserialize_table, read_u32_le, CAPACITY, LEN_PREFIX_BYTES, MAGIC_BYTES, MAGIC_LEN,
};

/// `MAGIC_BYTES` + `[u32 LE payload_len]` + serialized table + 0xAA
/// padding. Patched in place by splicer at splice time.
#[no_mangle]
pub static SPLICER_CONFIG_BLOB: [u8; CAPACITY] = {
    let mut b = [0xAA_u8; CAPACITY];
    let mut i = 0;
    while i < MAGIC_BYTES.len() {
        b[i] = MAGIC_BYTES[i];
        i += 1;
    }
    // payload_len = 0 — empty table.
    let mut j = 0;
    while j < LEN_PREFIX_BYTES {
        b[MAGIC_LEN + j] = 0;
        j += 1;
    }
    b
};

/// Parsed once per wasm-instance lifetime; subsequent `get` calls
/// reuse the cached `&'static HashMap`.
fn table() -> &'static HashMap<String, String> {
    static T: OnceLock<HashMap<String, String>> = OnceLock::new();
    T.get_or_init(parse_table)
}

fn parse_table() -> HashMap<String, String> {
    // `black_box` keeps the compiler from folding the parse against
    // the compile-time-empty blob.
    let blob: &[u8] = std::hint::black_box(&SPLICER_CONFIG_BLOB);
    let body = &blob[MAGIC_LEN..];
    let Some(payload_len) = read_u32_le(body, 0) else {
        return HashMap::new();
    };
    let payload_len = payload_len as usize;
    let payload_end = LEN_PREFIX_BYTES.saturating_add(payload_len);
    if payload_end > body.len() {
        return HashMap::new();
    }
    deserialize_table(&body[LEN_PREFIX_BYTES..payload_end])
}

pub struct ConfigProvider;

impl Guest for ConfigProvider {
    fn get(key: String) -> Option<String> {
        table().get(&key).cloned()
    }
}

bindings::export!(ConfigProvider with_types_in bindings);
