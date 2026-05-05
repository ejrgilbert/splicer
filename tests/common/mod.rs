//! Shared scaffolding for integration tests that drive a tier-1
//! builtin in wasmtime against a synthetic call-id.
//!
//! Each builtin smoke-test (`builtins_otel_bare_spans.rs`,
//! `builtins_otel_metrics.rs`, …) supplies its own `Capture` type and
//! linker-side fake host implementation; everything else (engine
//! config, instantiation, `on-call` → `on-return` drive cycle, `Val`
//! extractors) lives here so the per-test files stay focused on the
//! assertions.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use wasmtime::component::{Component, Linker, ResourceTable, Val};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

pub const SPLICER_BEFORE: &str = "splicer:tier1/before@0.2.0";
pub const SPLICER_AFTER: &str = "splicer:tier1/after@0.2.0";

pub const TARGET_IFACE: &str = "wasi:http/handler@0.3.0";
pub const TARGET_FN: &str = "handle";

pub struct Host<C: Send + 'static> {
    pub wasi: WasiCtx,
    pub table: ResourceTable,
    pub capture: Arc<Mutex<C>>,
}

impl<C: Send + 'static> WasiView for Host<C> {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

pub fn call_id_val(iface: &str, func: &str) -> Val {
    Val::Record(vec![
        ("interface-name".into(), Val::String(iface.into())),
        ("function-name".into(), Val::String(func.into())),
    ])
}

/// Drive a single `on-call` → `on-return` cycle on the embedded
/// builtin against a synthetic call-id targeting `TARGET_IFACE` /
/// `TARGET_FN`.
///
/// `setup` runs against the linker before the host is built; it's the
/// hook for the test to install whatever fake host interfaces the
/// builtin imports (e.g. `wasi:otel/tracing`, `wasi:otel/metrics`).
/// Inside those host fns, capture state is reachable as
/// `store.data().capture.lock()`.
///
/// Returns the capture `Arc<Mutex<C>>` so the caller can inspect it
/// after the cycle completes.
pub fn drive_call_cycle<C, F>(bytes: &[u8], setup: F) -> Result<Arc<Mutex<C>>>
where
    C: Default + Send + 'static,
    F: FnOnce(&mut Linker<Host<C>>) -> Result<()>,
{
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_binary(&engine, bytes)?;

    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    setup(&mut linker)?;

    let capture = Arc::new(Mutex::new(C::default()));
    let stdout = MemoryOutputPipe::new(64 * 1024);
    let host = Host {
        wasi: WasiCtxBuilder::new().stdout(stdout).build(),
        table: ResourceTable::new(),
        capture: capture.clone(),
    };
    let mut store = Store::new(&engine, host);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let instance = linker.instantiate_async(&mut store, &component).await?;

        let before_idx = instance
            .get_export_index(&mut store, None, SPLICER_BEFORE)
            .context("missing before export")?;
        let on_call_idx = instance
            .get_export_index(&mut store, Some(&before_idx), "on-call")
            .context("missing on-call export")?;
        let on_call = instance
            .get_func(&mut store, on_call_idx)
            .context("on-call is not a func")?;

        let after_idx = instance
            .get_export_index(&mut store, None, SPLICER_AFTER)
            .context("missing after export")?;
        let on_return_idx = instance
            .get_export_index(&mut store, Some(&after_idx), "on-return")
            .context("missing on-return export")?;
        let on_return = instance
            .get_func(&mut store, on_return_idx)
            .context("on-return is not a func")?;

        let cid = call_id_val(TARGET_IFACE, TARGET_FN);

        let mut results: Vec<Val> = vec![];
        on_call
            .call_async(&mut store, std::slice::from_ref(&cid), &mut results)
            .await?;

        let mut results: Vec<Val> = vec![];
        on_return
            .call_async(&mut store, &[cid], &mut results)
            .await?;

        Ok::<_, anyhow::Error>(())
    })?;

    Ok(capture)
}

/// Assert an attribute list carries `code.namespace` / `code.function`
/// entries matching `TARGET_IFACE` / `TARGET_FN`, both JSON-encoded as
/// quoted strings (the `wasi:otel/types.value` `AnyValue` wire format).
pub fn assert_call_attrs(attrs: &[Val]) {
    let attr_map: HashMap<String, String> = attrs
        .iter()
        .map(|kv| {
            let r = expect_record(kv);
            (
                expect_string(field(r, "key")).to_string(),
                expect_string(field(r, "value")).to_string(),
            )
        })
        .collect();
    assert_eq!(
        attr_map.get("code.namespace").map(String::as_str),
        Some(format!("\"{TARGET_IFACE}\"").as_str()),
        "code.namespace JSON-encoded; got {attr_map:?}"
    );
    assert_eq!(
        attr_map.get("code.function").map(String::as_str),
        Some(format!("\"{TARGET_FN}\"").as_str()),
        "code.function JSON-encoded; got {attr_map:?}"
    );
}

// ─── Val extractors ────────────────────────────────────────────────

pub fn field<'a>(record: &'a [(String, Val)], name: &str) -> &'a Val {
    record
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("field {name:?} not found in record {record:?}"))
}

pub fn expect_record(v: &Val) -> &[(String, Val)] {
    if let Val::Record(fields) = v {
        fields
    } else {
        panic!("expected record, got {v:?}")
    }
}
pub fn expect_string(v: &Val) -> &str {
    if let Val::String(s) = v {
        s
    } else {
        panic!("expected string, got {v:?}")
    }
}
pub fn expect_u64(v: &Val) -> u64 {
    if let Val::U64(n) = v {
        *n
    } else {
        panic!("expected u64, got {v:?}")
    }
}
pub fn expect_u32(v: &Val) -> u32 {
    if let Val::U32(n) = v {
        *n
    } else {
        panic!("expected u32, got {v:?}")
    }
}
pub fn expect_bool(v: &Val) -> bool {
    if let Val::Bool(b) = v {
        *b
    } else {
        panic!("expected bool, got {v:?}")
    }
}
pub fn expect_list(v: &Val) -> &[Val] {
    if let Val::List(items) = v {
        items
    } else {
        panic!("expected list, got {v:?}")
    }
}
pub fn expect_variant(v: &Val) -> (&str, Option<&Val>) {
    if let Val::Variant(case, payload) = v {
        (case.as_str(), payload.as_deref())
    } else {
        panic!("expected variant, got {v:?}")
    }
}
pub fn expect_enum(v: &Val) -> &str {
    if let Val::Enum(case) = v {
        case.as_str()
    } else {
        panic!("expected enum, got {v:?}")
    }
}
