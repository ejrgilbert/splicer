#[allow(warnings)]
mod bindings;
use bindings::host::kv::store::Bucket as RawBucket;
use bindings::splice::wrap::bridge::{wrap, unwrap};
use bindings::Guest;

struct Component;
impl Guest for Component {
    fn run() -> String {
        let raw = RawBucket::new("e");
        raw.set("k", "fromraw");      // write through the raw handle
        let w = wrap(raw);            // box the real handle into T' via the bridge
        let via_t = w.get("k");       // T'.get must forward to the same inner raw
        let raw2 = unwrap(w);         // crack T' open, recover the raw handle
        let via_raw = raw2.get("k");  // must still read the same data
        format!("via_t={:?} via_raw={:?}", via_t, via_raw)
    }
}
bindings::export!(Component with_types_in bindings);
