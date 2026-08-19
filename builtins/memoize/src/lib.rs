mod bindings {
    wit_bindgen::generate!({
        world: "consumer",
        generate_all,
    });
}

include!(concat!(env!("OUT_DIR"), "/builtin_config_codegen.rs"));

use std::collections::HashMap;
use std::sync::Mutex;

use bindings::wasi::clocks::wall_clock;
use splicer_tool_sdk::wasm_wave::value::Value as WaveValue;
use splicer_tool_sdk::wasm_wave::wasm::{WasmTypeKind, WasmValue};
use splicer_tool_sdk::{wasm_wave, CallId, TransformStrategy, WitTyped};

struct Entry {
    result: WaveValue,
    stored_at: f64,
    order: u64,
}

#[derive(Default)]
struct Cache {
    map: HashMap<String, Entry>,
    seq: u64,
}

pub struct Memoize {
    /// Capacity before eviction; 0 = unbounded.
    max_entries: usize,
    /// TTL in seconds; `None` if disabled (`ttl_seconds == 0`).
    ttl_seconds: Option<f64>,
    eviction: config::Eviction,
    cache_errors: bool,
    cache: Mutex<Cache>,
}
impl Default for Memoize {
    fn default() -> Self {
        let ttl = config::ttl_seconds();
        Self {
            max_entries: config::max_entries() as usize,
            ttl_seconds: (ttl > 0.0).then_some(ttl),
            eviction: config::eviction(),
            cache_errors: config::cache_errors(),
            cache: Mutex::new(Cache::default()),
        }
    }
}

fn now_seconds() -> f64 {
    let t = wall_clock::now();
    t.seconds as f64 + t.nanoseconds as f64 / 1_000_000_000.0
}

impl Memoize {
    fn should_cache(&self, val: &WaveValue) -> bool {
        if self.cache_errors {
            return true;
        }
        !(val.kind() == WasmTypeKind::Result && val.unwrap_result().is_err())
    }

    /// Evict per policy if the map is at capacity and `key` is new.
    /// Returns `false` when a `reject` policy declines the insert.
    fn make_room(&self, cache: &mut Cache, key: &str) -> bool {
        if self.max_entries == 0 || cache.map.len() < self.max_entries || cache.map.contains_key(key)
        {
            return true;
        }
        match self.eviction {
            config::Eviction::Reject => false,
            config::Eviction::Lru | config::Eviction::Fifo => {
                if let Some(victim) = cache
                    .map
                    .iter()
                    .min_by_key(|(_, e)| e.order)
                    .map(|(k, _)| k.clone())
                {
                    cache.map.remove(&victim);
                }
                true
            }
        }
    }
}

impl<Args: WitTyped, R: WitTyped> TransformStrategy<Args, R> for Memoize {
    async fn handle(
        &self,
        call: CallId,
        args: Args,
        downstream: impl AsyncFn(Args) -> R,
    ) -> R {
        // Key on interface+function identity plus the canonical WAVE text
        // of the arguments. A rendering failure disables caching for this
        // call rather than risking a wrong key.
        let key = match wasm_wave::to_string(&args.to_value()) {
            Ok(rendered) => Some(format!(
                "{}#{}\0{rendered}",
                call.interface_name, call.function_name
            )),
            Err(_) => None,
        };

        if let Some(key) = key.as_deref() {
            let mut cache = self.cache.lock().expect("cache mutex not poisoned");
            let now = now_seconds();
            let live = cache.map.get(key).map(|e| {
                self.ttl_seconds
                    .map_or(true, |ttl| now - e.stored_at < ttl)
            });
            match live {
                Some(true) => {
                    let value = cache.map[key].result.clone();
                    if let config::Eviction::Lru = self.eviction {
                        cache.seq += 1;
                        let seq = cache.seq;
                        cache.map.get_mut(key).expect("just read above").order = seq;
                    }
                    drop(cache);
                    return R::from_value(&value)
                        .expect("cached value round-trips to R");
                }
                Some(false) => {
                    cache.map.remove(key); // expired
                }
                None => {}
            }
        }

        let r = downstream(args).await;

        if let Some(key) = key {
            let value = r.to_value();
            if self.should_cache(&value) {
                let mut cache = self.cache.lock().expect("cache mutex not poisoned");
                if self.make_room(&mut cache, &key) {
                    cache.seq += 1;
                    let order = cache.seq;
                    cache.map.insert(
                        key,
                        Entry {
                            result: value,
                            stored_at: now_seconds(),
                            order,
                        },
                    );
                }
            }
        }

        r
    }
}
