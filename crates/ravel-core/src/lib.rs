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
pub mod generation_components;
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
pub mod watch;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Health {
    pub name: &'static str,
    pub version: &'static str,
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
