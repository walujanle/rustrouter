//! The executor layer.
//!
//! `get_executor` resolves through the special-executor table, then falls back
//! to a cached `DefaultExecutor`: a provider with a special executor gets a
//! fresh one, everything else gets a `DefaultExecutor` built once and reused.
//!
//! The special executors are built per call, not shared. Codex and Grok CLI
//! stash per-request state (compact flag, session id, turn counter) on `self`,
//! so one shared instance would leak that state across concurrent requests.

pub mod codex;
pub mod commandcode;
pub mod default;
pub mod executor;
pub mod grok_cli;
pub mod http;
pub mod identity;
pub mod oauth;
pub mod opencode;
pub mod opencode_go;
pub mod retry;
pub mod simple;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use dashmap::DashMap;

use crate::executors::codex::CodexExecutor;
use crate::executors::commandcode::CommandCodeExecutor;
use crate::executors::default::DefaultExecutor;
use crate::executors::executor::Executor;
use crate::executors::grok_cli::GrokCliExecutor;
use crate::executors::opencode::OpenCodeExecutor;
use crate::executors::opencode_go::OpenCodeGoExecutor;
use crate::executors::simple::CodeBuddyIntlExecutor;

/// A zero-argument constructor for a special executor.
type ExecutorBuilder = fn() -> Arc<dyn Executor>;

/// Builders for the special executors, keyed by provider id.
///
/// `gcli` and `gb` are literal alias keys for `GrokCliExecutor`; other alias
/// registrations are handled by `resolve_alias` before the lookup, but these two
/// are literal keys here.
static SPECIAL: LazyLock<HashMap<&'static str, ExecutorBuilder>> = LazyLock::new(|| {
    let mut map: HashMap<&'static str, ExecutorBuilder> = HashMap::new();
    map.insert("codex", || Arc::new(CodexExecutor::new()));
    map.insert("commandcode", || Arc::new(CommandCodeExecutor::new()));
    map.insert("grok-cli", || Arc::new(GrokCliExecutor::new()));
    map.insert("gcli", || Arc::new(GrokCliExecutor::new()));
    map.insert("gb", || Arc::new(GrokCliExecutor::new()));
    map.insert("opencode", || Arc::new(OpenCodeExecutor::new()));
    map.insert("opencode-go", || Arc::new(OpenCodeGoExecutor::new()));
    map.insert("codebuddy-intl", || Arc::new(CodeBuddyIntlExecutor::new()));
    map
});

/// One `DefaultExecutor` per provider id, built on first use.
static DEFAULT_CACHE: LazyLock<DashMap<String, Arc<dyn Executor>>> = LazyLock::new(DashMap::new);

pub fn get_executor(provider: &str) -> Arc<dyn Executor> {
    if let Some(build) = SPECIAL.get(provider) {
        return build();
    }
    // `entry` holds the shard lock across the check and the insert, so two
    // threads racing on a cold key return the same `Arc`. A separate `get` +
    // `insert` lets both build and insert, and each caller gets its own
    // instance.
    DEFAULT_CACHE
        .entry(provider.to_string())
        .or_insert_with(|| Arc::new(DefaultExecutor::new(provider)))
        .clone()
}

/// Whether the provider has a special executor rather than the default.
pub fn has_specialized_executor(provider: &str) -> bool {
    SPECIAL.contains_key(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_providers_get_a_cached_default_executor() {
        let a = get_executor("deepseek");
        let b = get_executor("deepseek");
        assert!(
            Arc::ptr_eq(&a, &b),
            "the default executor is cached per provider"
        );
        assert_eq!(a.provider(), "deepseek");
        assert!(!has_specialized_executor("deepseek"));
    }

    #[test]
    fn special_executors_are_fresh_per_call() {
        // Codex and Grok CLI hold per-request state on `self`, so a shared
        // instance would leak it across concurrent requests.
        for provider in ["codex", "grok-cli", "gcli", "gb"] {
            let a = get_executor(provider);
            let b = get_executor(provider);
            assert!(
                !Arc::ptr_eq(&a, &b),
                "{provider} must be rebuilt per call, not shared"
            );
        }
    }

    #[test]
    fn different_providers_get_different_executors() {
        let a = get_executor("deepseek");
        let b = get_executor("mistral");
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(b.provider(), "mistral");
    }

    #[test]
    fn the_ported_special_executors_are_registered() {
        // Every special key in the executor map. `gcli` and `gb` are Grok CLI
        // aliases keyed literally.
        for provider in [
            "codex",
            "commandcode",
            "grok-cli",
            "gcli",
            "gb",
            "opencode",
            "opencode-go",
            "codebuddy-intl",
        ] {
            assert!(
                has_specialized_executor(provider),
                "{provider} must have a special executor"
            );
        }
        // `gcli` and `gb` are alias keys: the constructor hardcodes `"grok-cli"`
        // as the provider, so the alias resolves to that.
        for provider in ["grok-cli", "gcli", "gb"] {
            assert_eq!(get_executor(provider).provider(), "grok-cli");
        }
        for provider in [
            "codex",
            "commandcode",
            "opencode",
            "opencode-go",
            "codebuddy-intl",
        ] {
            assert_eq!(get_executor(provider).provider(), provider);
        }
    }
}
