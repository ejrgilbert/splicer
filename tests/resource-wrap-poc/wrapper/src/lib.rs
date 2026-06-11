#[allow(warnings)]
mod bindings;
use bindings::host::kv::store::Bucket as RawBucket;
use bindings::exports::splice::wrap::store::{Guest as StoreGuest, GuestBucket, Bucket as WrappedHandle};
use bindings::exports::splice::wrap::bridge::Guest as BridgeGuest;

struct Component;
struct WrappedBucket { inner: RawBucket }

impl StoreGuest for Component { type Bucket = WrappedBucket; }

impl GuestBucket for WrappedBucket {
    fn new(name: String) -> Self { WrappedBucket { inner: RawBucket::new(&name) } }
    fn get(&self, k: String) -> Option<String> { /* record hook here */ self.inner.get(&k) }
    fn set(&self, k: String, v: String) { self.inner.set(&k, &v) }
}

impl BridgeGuest for Component {
    fn wrap(inner: RawBucket) -> WrappedHandle { WrappedHandle::new(WrappedBucket { inner }) }
    fn unwrap(w: WrappedHandle) -> RawBucket { w.into_inner::<WrappedBucket>().inner }
}

bindings::export!(Component with_types_in bindings);
