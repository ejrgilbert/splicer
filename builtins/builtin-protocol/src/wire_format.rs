//! Wire format for the `splicer:builtin-config` substrate's per-edge
//! provider blob. Splicer's host-side patcher (`src/config_provider.rs`)
//! serializes the table; the `config-provider` builtin's wasm
//! component deserializes it at runtime. Both sides depend on this
//! module so the format has one source of truth.
//!
//! IMPORTANT: in the provider template, `MAGIC_BYTES` may only be
//! referenced in const-eval contexts. A runtime `&MAGIC_BYTES` forces
//! rustc/lld to emit a separately-addressable copy next to the
//! byte-identical prefix of `SPLICER_CONFIG_BLOB`, which trips the
//! patcher's "magic appears exactly once" check.

pub const MAGIC_BYTES: [u8; 29] = *b"\x00\xefSPLICER_BUILTIN_CONFIG_V1\xef\x00";
pub const MAGIC_LEN: usize = MAGIC_BYTES.len();

/// Magic + length header + serialized table + padding. Patching
/// fails when the serialized table doesn't fit.
pub const CAPACITY: usize = 16 * 1024;

/// Width of every length-prefix field (`payload_len`, `count`,
/// `key_len`, `val_len`).
pub const LEN_PREFIX_BYTES: usize = std::mem::size_of::<u32>();

/// Wire format: `[u32 LE count][u32 LE key_len][key bytes][u32 LE val_len][val bytes]...`
/// `BTreeMap` iteration order gives byte-deterministic output across
/// runs with identical inputs.
pub fn serialize_table(values: &std::collections::BTreeMap<String, String>) -> Vec<u8> {
    let count = values.len() as u32;
    let mut buf = Vec::new();
    buf.extend_from_slice(&count.to_le_bytes());
    for (k, v) in values {
        let kb = k.as_bytes();
        let vb = v.as_bytes();
        buf.extend_from_slice(&(kb.len() as u32).to_le_bytes());
        buf.extend_from_slice(kb);
        buf.extend_from_slice(&(vb.len() as u32).to_le_bytes());
        buf.extend_from_slice(vb);
    }
    buf
}

/// Deserialize an on-wire payload back to a `HashMap`. Malformed
/// framing returns an empty map -- the format is splicer-internal, so
/// a bad table signals a build/patch bug and consumers fall back to
/// per-builtin defaults either way.
pub fn deserialize_table(payload: &[u8]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(count) = read_u32_le(payload, 0) else {
        return out;
    };
    let mut cursor = LEN_PREFIX_BYTES;
    for _ in 0..count {
        let Some(key_len) = read_u32_le(payload, cursor) else {
            return out;
        };
        cursor += LEN_PREFIX_BYTES;
        let key_end = cursor + key_len as usize;
        if key_end > payload.len() {
            return out;
        }
        let Ok(key) = std::str::from_utf8(&payload[cursor..key_end]) else {
            return out;
        };
        cursor = key_end;

        let Some(val_len) = read_u32_le(payload, cursor) else {
            return out;
        };
        cursor += LEN_PREFIX_BYTES;
        let val_end = cursor + val_len as usize;
        if val_end > payload.len() {
            return out;
        }
        let Ok(val) = std::str::from_utf8(&payload[cursor..val_end]) else {
            return out;
        };
        cursor = val_end;

        out.insert(key.to_string(), val.to_string());
    }
    out
}

/// Read a little-endian u32 at `off`. Returns `None` if the slice is
/// too short.
pub fn read_u32_le(buf: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(LEN_PREFIX_BYTES)?;
    if end > buf.len() {
        return None;
    }
    Some(u32::from_le_bytes(buf[off..end].try_into().ok()?))
}
