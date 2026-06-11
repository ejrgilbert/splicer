#[allow(warnings)]
mod bindings;
use bindings::exports::host::kv::store::{Guest, GuestBucket};
use std::cell::RefCell;
use std::collections::HashMap;
struct Component;
struct RealBucket { map: RefCell<HashMap<String, String>> }
impl Guest for Component { type Bucket = RealBucket; }
impl GuestBucket for RealBucket {
    fn new(_name: String) -> Self { RealBucket { map: RefCell::new(HashMap::new()) } }
    fn get(&self, k: String) -> Option<String> { self.map.borrow().get(&k).cloned() }
    fn set(&self, k: String, v: String) { self.map.borrow_mut().insert(k, v); }
}
bindings::export!(Component with_types_in bindings);
