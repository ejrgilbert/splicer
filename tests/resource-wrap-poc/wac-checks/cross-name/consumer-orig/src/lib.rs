#[allow(warnings)]
mod bindings;
use bindings::test::orig::store::Bucket;
use bindings::Guest;
struct Component;
impl Guest for Component {
    fn run() -> String {
        let b = Bucket::new("x");
        b.set("k", "v");
        b.get("k").unwrap_or_default()
    }
}
bindings::export!(Component with_types_in bindings);
