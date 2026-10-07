//! Contracts and bounded building blocks shared by Ravel's interfaces.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod analysis;
pub mod boundaries;
pub mod cache;
pub mod config;
pub mod daemon;
mod durable_io;
pub mod engine;
pub mod entries;
pub mod generation_gc;
pub mod generation_pack;
pub mod git;
pub mod graph;
pub mod incremental_graph;
pub mod install;
pub mod mcp;
pub mod model;
pub mod policy;
pub mod resolver;
pub mod scanner;
pub mod search;
pub mod storage;
pub mod structural;
pub mod structural_reverse;
pub mod timing;
pub mod watch;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Health {
    pub name: &'static str,
    pub version: &'static str,
}

static MEMORY_RELEASE_HOOK: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// How this process hands freed heap back to the operating system. The binary that chooses
/// the global allocator registers it (mimalloc keeps everything a sync allocated committed
/// until the next allocation pressure, which an idle daemon never produces); long-lived
/// servers call [`release_memory`] after each publication. Without a hook it is a no-op.
pub fn set_memory_release_hook(hook: fn()) {
    let _ = MEMORY_RELEASE_HOOK.set(hook);
}

pub(crate) fn release_memory() {
    if let Some(hook) = MEMORY_RELEASE_HOOK.get() {
        hook();
    }
}

pub fn health() -> Health {
    Health {
        name: "ravel",
        version: VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn health_contract_is_stable() {
        assert_eq!(health().name, "ravel");
        assert!(!health().version.is_empty());
    }
}
