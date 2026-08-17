//! Regex content-filtering transform strategy.
//!
//! Rewrites string content in the payloads crossing a wrapped boundary:
//! every `string` leaf of the args and/or result is run through the
//! configured regexes (substring replacement), so PII / secrets / tokens
//! can be masked before they reach downstream or the caller. Pairs with
//! `recorder` to make trace capture safe. Value-typed only: the
//! `WitTyped` bound narrows it to targets whose args and results are
//! value types (no resources/handles).

mod bindings {
    wit_bindgen::generate!({
        world: "consumer",
        generate_all,
    });
}

include!(concat!(env!("OUT_DIR"), "/builtin_config_codegen.rs"));

use regex::{NoExpand, Regex};
use splicer_tool_sdk::{map_strings, CallId, TransformStrategy, WitTyped};

pub struct RedactStrings {
    patterns: Vec<Regex>,
    replacement: String,
    rewrite_args: bool,
    rewrite_result: bool,
}

impl Default for RedactStrings {
    fn default() -> Self {
        let patterns = config::patterns()
            .iter()
            .map(|p| {
                Regex::new(p).unwrap_or_else(|e| {
                    panic!("[redact-strings] invalid regex pattern {p:?}: {e}")
                })
            })
            .collect();
        let (rewrite_args, rewrite_result) = match config::direction() {
            config::Direction::Args => (true, false),
            config::Direction::Result => (false, true),
            config::Direction::Both => (true, true),
        };
        Self {
            patterns,
            replacement: config::replacement().to_string(),
            rewrite_args,
            rewrite_result,
        }
    }
}

impl RedactStrings {
    fn rewrite<T: WitTyped>(&self, val: T) -> T {
        if self.patterns.is_empty() {
            return val;
        }
        let ty = T::wave_type();
        let mapped = map_strings(&val.to_value(), &ty, &|s| self.apply(s));
        T::from_value(&mapped).expect("[redact-strings] rewrite preserves the value's type")
    }

    fn apply(&self, s: &str) -> String {
        let mut cur = s.to_string();
        for re in &self.patterns {
            cur = re
                .replace_all(&cur, NoExpand(self.replacement.as_str()))
                .into_owned();
        }
        cur
    }
}

impl<Args: WitTyped, R: WitTyped> TransformStrategy<Args, R> for RedactStrings {
    async fn handle(
        &self,
        _call: CallId,
        args: Args,
        downstream: impl AsyncFn(Args) -> R,
    ) -> R {
        let args = if self.rewrite_args {
            self.rewrite(args)
        } else {
            args
        };
        let r = downstream(args).await;
        if self.rewrite_result {
            self.rewrite(r)
        } else {
            r
        }
    }
}
