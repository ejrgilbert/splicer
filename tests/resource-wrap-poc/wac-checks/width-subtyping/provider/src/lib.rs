#[allow(warnings)]
mod bindings;

use bindings::exports::test::shared::store::{Guest, GuestBucket, Bucket};
use std::cell::RefCell;
use std::collections::HashMap;

struct Component;
struct MyBucket { map: RefCell<HashMap<String, String>> }

impl Guest for Component {
    type Bucket = MyBucket;
}

impl GuestBucket for MyBucket {
    fn new(_name: String) -> Self { MyBucket { map: RefCell::new(HashMap::new()) } }
    fn get(&self, k: String) -> Option<String> { self.map.borrow().get(&k).cloned() }
    fn set(&self, k: String, v: String) { self.map.borrow_mut().insert(k, v); }
    fn wrap(inner: Bucket) -> Bucket { inner }
    fn tag(&self) -> u32 { 7 }
}

bindings::export!(Component with_types_in bindings);
