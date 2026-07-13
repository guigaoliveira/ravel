use crate::model::{CacheKey, FileArtifact};
use moka::sync::Cache;
use std::sync::Arc;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CacheMetrics {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub bytes: u64,
}

pub struct ArtifactCache {
    cache: Cache<CacheKey, Arc<FileArtifact>>,
}
impl ArtifactCache {
    pub fn new(max_bytes: u64) -> Self {
        Self {
            cache: Cache::builder()
                .support_invalidation_closures()
                .max_capacity(max_bytes.max(1))
                .weigher(|_, value: &Arc<FileArtifact>| {
                    value.bytes_read.clamp(1, u32::MAX as u64) as u32
                })
                .build(),
        }
    }
    pub fn get(&self, key: &CacheKey) -> Option<Arc<FileArtifact>> {
        self.cache.get(key)
    }
    pub fn insert(&self, key: CacheKey, value: Arc<FileArtifact>) {
        self.cache.insert(key, value);
    }
    pub fn invalidate_context(&self, grammar_version: &str, extractor_version: &str) {
        let grammar_version = grammar_version.to_owned();
        let extractor_version = extractor_version.to_owned();
        let _ = self.cache.invalidate_entries_if(move |key, _| {
            key.grammar_version != grammar_version || key.extractor_version != extractor_version
        });
        self.cache.run_pending_tasks();
    }
    pub fn invalidate_all(&self) {
        self.cache.invalidate_all();
    }
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::parse_source;
    #[test]
    fn same_key_reuses_artifact_and_context_can_be_invalidated() {
        let cache = ArtifactCache::new(1024 * 1024);
        let artifact = Arc::new(parse_source("a.ts", b"export const a = 1"));
        let key = CacheKey {
            source_hash: artifact.source_hash.clone(),
            language: "typescript".into(),
            grammar_version: "g1".into(),
            extractor_version: "e1".into(),
            resolver_config_hash: "c1".into(),
        };
        cache.insert(key.clone(), artifact.clone());
        assert_eq!(cache.get(&key), Some(artifact));
        cache.invalidate_context("g2", "e1");
        assert!(cache.get(&key).is_none());
    }
}
