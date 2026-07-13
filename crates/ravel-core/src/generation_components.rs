//! Immutable sorted component shards with sparse generation overlays.
//!
//! This module is storage-agnostic: callers serialize each shard/overlay as an opaque pack
//! record. Overlays are ordered oldest-to-newest; the newest operation for a key wins.

use crate::model::SymbolMeta;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ComponentError {
    #[error("max_entries_per_shard must be greater than zero")]
    ZeroShardSize,
    #[error("component keys must be non-empty and strictly increasing")]
    UnsortedKeys,
    #[error("component serialization failed: {0}")]
    Serialization(String),
    #[error("component record is missing: {0}")]
    MissingRecord(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardRange {
    pub first: String,
    pub last: String,
    pub record_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RangeManifest {
    format_version: u32,
    base: Vec<ShardRange>,
    overlays: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct VersionedPayload<T> {
    format_version: u32,
    payload: T,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SortedShard<V> {
    /// Strictly key-sorted. A key owns all of its values, so duplicate definitions never split.
    pub entries: Vec<(String, Vec<V>)>,
}

impl<V> SortedShard<V> {
    pub fn new(entries: Vec<(String, Vec<V>)>) -> Result<Self, ComponentError> {
        if entries.iter().any(|(key, _)| key.is_empty())
            || entries.windows(2).any(|pair| pair[0].0 >= pair[1].0)
        {
            return Err(ComponentError::UnsortedKeys);
        }
        Ok(Self { entries })
    }

    fn exact(&self, key: &str) -> Option<&[V]> {
        self.entries
            .binary_search_by(|(candidate, _)| candidate.as_str().cmp(key))
            .ok()
            .map(|index| self.entries[index].1.as_slice())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum OverlayValue<V> {
    Upsert(Vec<V>),
    Tombstone,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SparseOverlay<V> {
    pub entries: BTreeMap<String, OverlayValue<V>>,
}

impl<V> Default for SparseOverlay<V> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

impl<V> SparseOverlay<V> {
    pub fn upsert(&mut self, key: impl Into<String>, values: Vec<V>) {
        self.entries
            .insert(key.into(), OverlayValue::Upsert(values));
    }

    pub fn tombstone(&mut self, key: impl Into<String>) {
        self.entries.insert(key.into(), OverlayValue::Tombstone);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardedComponent<V> {
    pub format_version: u32,
    pub base: Vec<SortedShard<V>>,
    /// Oldest to newest.
    pub overlays: Vec<SparseOverlay<V>>,
}

impl<V> ShardedComponent<V> {
    pub const FORMAT_VERSION: u32 = 1;
}

impl<V: Clone> ShardedComponent<V> {
    pub fn from_entries(
        entries: BTreeMap<String, Vec<V>>,
        max_entries_per_shard: usize,
    ) -> Result<Self, ComponentError> {
        if max_entries_per_shard == 0 {
            return Err(ComponentError::ZeroShardSize);
        }
        let entries: Vec<_> = entries.into_iter().collect();
        let base = entries
            .chunks(max_entries_per_shard)
            .map(|chunk| SortedShard::new(chunk.to_vec()))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            format_version: Self::FORMAT_VERSION,
            base,
            overlays: Vec::new(),
        })
    }

    pub fn push_overlay(&mut self, overlay: SparseOverlay<V>) {
        if !overlay.entries.is_empty() {
            self.overlays.push(overlay);
        }
    }

    pub fn exact(&self, key: &str) -> Option<Vec<V>> {
        for overlay in self.overlays.iter().rev() {
            if let Some(value) = overlay.entries.get(key) {
                return match value {
                    OverlayValue::Upsert(values) => Some(values.clone()),
                    OverlayValue::Tombstone => None,
                };
            }
        }
        // Shards have disjoint sorted ranges. Partition by last key, then inspect one shard.
        let index = self.base.partition_point(|shard| {
            shard
                .entries
                .last()
                .is_some_and(|(last, _)| last.as_str() < key)
        });
        self.base
            .get(index)
            .and_then(|shard| shard.exact(key))
            .map(<[V]>::to_vec)
    }

    pub fn prefix(&self, prefix: &str, limit: usize) -> Vec<(String, Vec<V>)> {
        if limit == 0 {
            return Vec::new();
        }
        let mut merged = BTreeMap::new();
        let upper = format!("{prefix}\u{10ffff}");
        for shard in &self.base {
            if shard
                .entries
                .last()
                .is_some_and(|(last, _)| last.as_str() < prefix)
                || shard
                    .entries
                    .first()
                    .is_some_and(|(first, _)| first > &upper)
            {
                continue;
            }
            let start = shard
                .entries
                .partition_point(|(key, _)| key.as_str() < prefix);
            for (key, values) in shard.entries.iter().skip(start) {
                if !key.starts_with(prefix) {
                    break;
                }
                merged.insert(key.clone(), values.clone());
            }
        }
        for overlay in &self.overlays {
            for (key, operation) in overlay.entries.range(prefix.to_owned()..=upper.clone()) {
                if !key.starts_with(prefix) {
                    continue;
                }
                match operation {
                    OverlayValue::Upsert(values) => {
                        merged.insert(key.clone(), values.clone());
                    }
                    OverlayValue::Tombstone => {
                        merged.remove(key);
                    }
                }
            }
        }
        merged.into_iter().take(limit).collect()
    }
}

impl<V> ShardedComponent<V>
where
    V: Serialize + DeserializeOwned,
{
    pub fn to_bytes(&self) -> Result<Vec<u8>, ComponentError> {
        bincode::serialize(self).map_err(|error| ComponentError::Serialization(error.to_string()))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ComponentError> {
        let value: Self = bincode::deserialize(bytes)
            .map_err(|error| ComponentError::Serialization(error.to_string()))?;
        if value.format_version != Self::FORMAT_VERSION
            || value.base.iter().any(|shard| {
                shard.entries.iter().any(|(key, _)| key.is_empty())
                    || shard.entries.windows(2).any(|pair| pair[0].0 >= pair[1].0)
            })
            || value.base.windows(2).any(|pair| {
                pair[0].entries.last().map(|entry| &entry.0)
                    >= pair[1].entries.first().map(|entry| &entry.0)
            })
        {
            return Err(ComponentError::UnsortedKeys);
        }
        Ok(value)
    }

    /// Serialize independently readable payload records plus lexicographic range metadata.
    pub fn to_records(
        &self,
        prefix: &str,
        metadata_key: &str,
    ) -> Result<BTreeMap<String, Vec<u8>>, ComponentError> {
        let mut records = BTreeMap::new();
        let mut ranges = Vec::with_capacity(self.base.len());
        for (index, shard) in self.base.iter().enumerate() {
            let Some((first, _)) = shard.entries.first() else {
                continue;
            };
            let last = &shard.entries.last().expect("non-empty shard").0;
            let record_key = format!("{prefix}/{index:06}");
            let payload = VersionedPayload {
                format_version: Self::FORMAT_VERSION,
                payload: shard,
            };
            records.insert(record_key.clone(), encode(&payload)?);
            ranges.push(ShardRange {
                first: first.clone(),
                last: last.clone(),
                record_key,
            });
        }
        let mut overlays = Vec::with_capacity(self.overlays.len());
        for (index, overlay) in self.overlays.iter().enumerate() {
            let record_key = format!("{prefix}/overlay/{index:06}");
            let payload = VersionedPayload {
                format_version: Self::FORMAT_VERSION,
                payload: overlay,
            };
            records.insert(record_key.clone(), encode(&payload)?);
            overlays.push(record_key);
        }
        records.insert(
            metadata_key.into(),
            encode(&RangeManifest {
                format_version: Self::FORMAT_VERSION,
                base: ranges,
                overlays,
            })?,
        );
        Ok(records)
    }

    pub fn from_records(
        records: &BTreeMap<String, Vec<u8>>,
        metadata_key: &str,
    ) -> Result<Self, ComponentError> {
        let manifest_bytes = records
            .get(metadata_key)
            .ok_or_else(|| ComponentError::MissingRecord(metadata_key.into()))?;
        let manifest: RangeManifest = decode(manifest_bytes)?;
        if manifest.format_version != Self::FORMAT_VERSION
            || manifest
                .base
                .windows(2)
                .any(|pair| pair[0].last >= pair[1].first)
        {
            return Err(ComponentError::UnsortedKeys);
        }
        let mut base = Vec::with_capacity(manifest.base.len());
        for range in manifest.base {
            let bytes = records
                .get(&range.record_key)
                .ok_or_else(|| ComponentError::MissingRecord(range.record_key.clone()))?;
            let payload: VersionedPayload<SortedShard<V>> = decode(bytes)?;
            if payload.format_version != Self::FORMAT_VERSION
                || payload.payload.entries.first().map(|entry| &entry.0) != Some(&range.first)
                || payload.payload.entries.last().map(|entry| &entry.0) != Some(&range.last)
                || payload
                    .payload
                    .entries
                    .windows(2)
                    .any(|pair| pair[0].0 >= pair[1].0)
            {
                return Err(ComponentError::UnsortedKeys);
            }
            base.push(payload.payload);
        }
        let mut overlays = Vec::with_capacity(manifest.overlays.len());
        for record_key in manifest.overlays {
            let bytes = records
                .get(&record_key)
                .ok_or_else(|| ComponentError::MissingRecord(record_key.clone()))?;
            let payload: VersionedPayload<SparseOverlay<V>> = decode(bytes)?;
            if payload.format_version != Self::FORMAT_VERSION {
                return Err(ComponentError::UnsortedKeys);
            }
            overlays.push(payload.payload);
        }
        Ok(Self {
            format_version: Self::FORMAT_VERSION,
            base,
            overlays,
        })
    }
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, ComponentError> {
    bincode::serialize(value).map_err(|error| ComponentError::Serialization(error.to_string()))
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ComponentError> {
    bincode::deserialize(bytes).map_err(|error| ComponentError::Serialization(error.to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct SymbolId(pub String);

impl SymbolId {
    pub const DOMAIN: &'static [u8] = b"ravel-symbol-id-v1\0";

    pub fn from_definition(meta: &SymbolMeta, ordinal: u32) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(Self::DOMAIN);
        for value in [
            meta.path.as_bytes(),
            meta.kind.as_bytes(),
            meta.name.as_bytes(),
        ] {
            hasher.update(&(value.len() as u64).to_le_bytes());
            hasher.update(value);
        }
        for value in [
            meta.span.start_byte,
            meta.span.end_byte,
            meta.span.start_line,
            meta.span.start_column,
            meta.span.end_line,
            meta.span.end_column,
            ordinal,
        ] {
            hasher.update(&value.to_le_bytes());
        }
        Self(hasher.finalize().to_hex().to_string())
    }
}

/// Normalized symbol name -> every matching stable definition id.
pub type SymbolPostings = ShardedComponent<SymbolId>;
pub type SymbolReader = SymbolPostings;
/// Stable definition id -> metadata. The Vec shape shares the generic component format; builders
/// emit exactly one metadata value per id.
pub type SymbolDefinitions = ShardedComponent<SymbolMeta>;
pub type SymbolDefinitionReader = SymbolDefinitions;

pub fn build_symbol_components(
    definitions: impl IntoIterator<Item = (SymbolMeta, u32)>,
    max_entries_per_shard: usize,
) -> Result<(SymbolPostings, SymbolDefinitions), ComponentError> {
    let mut postings: BTreeMap<String, Vec<SymbolId>> = BTreeMap::new();
    let mut metadata = BTreeMap::new();
    for (meta, ordinal) in definitions {
        let id = SymbolId::from_definition(&meta, ordinal);
        postings
            .entry(meta.name.to_lowercase())
            .or_default()
            .push(id.clone());
        metadata.insert(id.0.clone(), vec![meta]);
    }
    for ids in postings.values_mut() {
        ids.sort();
        ids.dedup();
    }
    Ok((
        SymbolPostings::from_entries(postings, max_entries_per_shard)?,
        SymbolDefinitions::from_entries(metadata, max_entries_per_shard)?,
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PathRecord {
    pub path: String,
    pub source_hash: String,
}

pub type PathRecords = ShardedComponent<PathRecord>;
pub type PathReader = PathRecords;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchNameRef {
    pub display_name: String,
    /// Exact key in `SymbolPostings`.
    pub postings_key: String,
}

pub type SearchNameRefs = ShardedComponent<SearchNameRef>;
pub type SearchNameReader = SearchNameRefs;

pub const SYMBOL_RECORD_PREFIX: &str = "symbols";
pub const SYMBOL_RANGE_KEY: &str = "meta/symbol-ranges";
pub const SYMBOL_META_RECORD_PREFIX: &str = "symbol-meta";
pub const SYMBOL_META_RANGE_KEY: &str = "meta/symbol-meta-ranges";
pub const PATH_RECORD_PREFIX: &str = "paths";
pub const PATH_RANGE_KEY: &str = "meta/path-ranges";
pub const SEARCH_RECORD_PREFIX: &str = "search";
pub const SEARCH_RANGE_KEY: &str = "meta/search-ranges";

impl SymbolPostings {
    pub fn definition_ids(&self, name: &str) -> Vec<SymbolId> {
        self.exact(&name.to_lowercase()).unwrap_or_default()
    }
}

impl SymbolDefinitions {
    pub fn definition(&self, id: &SymbolId) -> Option<SymbolMeta> {
        self.exact(&id.0)
            .and_then(|entries| entries.into_iter().next())
    }
}

impl SearchNameRefs {
    pub fn exact_names(&self, normalized_name: &str) -> Vec<SearchNameRef> {
        self.exact(normalized_name).unwrap_or_default()
    }

    pub fn prefix_names(&self, normalized_prefix: &str, limit: usize) -> Vec<SearchNameRef> {
        self.prefix(normalized_prefix, limit)
            .into_iter()
            .flat_map(|(_, refs)| refs)
            .take(limit)
            .collect()
    }
}

impl PathRecords {
    pub fn file_hash(&self, path: &str) -> Option<String> {
        self.exact(path)
            .and_then(|records| records.into_iter().next())
            .map(|record| record.source_hash)
    }

    pub fn file_list(&self, prefix: &str, limit: usize) -> Vec<String> {
        self.prefix(prefix, limit)
            .into_iter()
            .map(|(path, _)| path)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Span;
    use std::sync::Arc;

    fn meta(name: &str, path: &str, start: u32) -> SymbolMeta {
        SymbolMeta {
            name: name.into(),
            kind: Arc::from("function"),
            path: path.into(),
            span: Span {
                start_byte: start,
                end_byte: start + 1,
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 2,
            },
            exported: true,
            complexity: None,
        }
    }

    #[test]
    fn exact_prefix_duplicates_and_overlays_are_deterministic() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "Alpha".into(),
            vec![meta("Alpha", "a.ts", 1), meta("Alpha", "b.ts", 2)],
        );
        entries.insert("Alpine".into(), vec![meta("Alpine", "c.ts", 3)]);
        entries.insert("Beta".into(), vec![meta("Beta", "d.ts", 4)]);
        let mut component = SymbolDefinitions::from_entries(entries, 1).unwrap();
        let mut old = SparseOverlay::default();
        old.upsert("Beta", vec![meta("Beta", "new.ts", 5)]);
        old.upsert("Albatross", vec![meta("Albatross", "e.ts", 6)]);
        component.push_overlay(old);
        let mut newest = SparseOverlay::default();
        newest.tombstone("Alpine");
        component.push_overlay(newest);

        assert_eq!(component.exact("Alpha").unwrap().len(), 2);
        assert_eq!(component.exact("Beta").unwrap()[0].path, "new.ts");
        assert!(component.exact("Alpine").is_none());
        let keys: Vec<_> = component
            .prefix("Al", 10)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, vec!["Albatross", "Alpha"]);
    }

    #[test]
    fn serialization_is_deterministic_and_roundtrips() {
        let entries = BTreeMap::from([
            (
                "a.ts".into(),
                vec![PathRecord {
                    path: "a.ts".into(),
                    source_hash: "aa".into(),
                }],
            ),
            (
                "b.ts".into(),
                vec![PathRecord {
                    path: "b.ts".into(),
                    source_hash: "bb".into(),
                }],
            ),
        ]);
        let component = PathRecords::from_entries(entries, 1).unwrap();
        let first = component.to_bytes().unwrap();
        let second = component.to_bytes().unwrap();
        assert_eq!(first, second);
        let decoded = PathRecords::from_bytes(&first).unwrap();
        assert_eq!(decoded, component);
        assert_eq!(decoded.file_hash("b.ts").as_deref(), Some("bb"));
        assert_eq!(decoded.file_list("", 1), vec!["a.ts"]);
    }

    #[test]
    fn overlay_matches_materialized_map_for_many_operations() {
        let base = BTreeMap::from_iter((0..100).map(|n| (format!("k{n:03}"), vec![n])));
        let mut component = ShardedComponent::from_entries(base.clone(), 7).unwrap();
        let mut expected = base;
        for generation in 0..8 {
            let mut overlay = SparseOverlay::default();
            for n in 0..100 {
                if n % 8 == generation {
                    let key = format!("k{n:03}");
                    if n % 3 == 0 {
                        overlay.tombstone(&key);
                        expected.remove(&key);
                    } else {
                        overlay.upsert(&key, vec![n + 1000]);
                        expected.insert(key, vec![n + 1000]);
                    }
                }
            }
            component.push_overlay(overlay);
        }
        assert_eq!(
            component.prefix("k", usize::MAX),
            expected.into_iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn pack_records_use_stable_prefixes_ranges_and_versioned_payloads() {
        let entries = BTreeMap::from([
            (
                "alpha".into(),
                vec![SearchNameRef {
                    display_name: "Alpha".into(),
                    postings_key: "Alpha".into(),
                }],
            ),
            (
                "beta".into(),
                vec![SearchNameRef {
                    display_name: "Beta".into(),
                    postings_key: "Beta".into(),
                }],
            ),
        ]);
        let mut component = SearchNameRefs::from_entries(entries, 1).unwrap();
        let mut overlay = SparseOverlay::default();
        overlay.upsert(
            "alpine",
            vec![SearchNameRef {
                display_name: "Alpine".into(),
                postings_key: "Alpine".into(),
            }],
        );
        component.push_overlay(overlay);

        let records = component
            .to_records(SEARCH_RECORD_PREFIX, SEARCH_RANGE_KEY)
            .unwrap();
        assert!(records.contains_key("search/000000"));
        assert!(records.contains_key("search/000001"));
        assert!(records.contains_key("search/overlay/000000"));
        assert!(records.contains_key("meta/search-ranges"));
        let reopened = SearchNameRefs::from_records(&records, SEARCH_RANGE_KEY).unwrap();
        assert_eq!(reopened, component);
        assert_eq!(
            reopened
                .prefix_names("al", 10)
                .into_iter()
                .map(|entry| entry.display_name)
                .collect::<Vec<_>>(),
            vec!["Alpha", "Alpine"]
        );
    }

    #[test]
    fn missing_or_mismatched_shard_is_rejected() {
        let component = PathRecords::from_entries(
            BTreeMap::from([(
                "a.ts".into(),
                vec![PathRecord {
                    path: "a.ts".into(),
                    source_hash: "hash".into(),
                }],
            )]),
            1,
        )
        .unwrap();
        let mut records = component
            .to_records(PATH_RECORD_PREFIX, PATH_RANGE_KEY)
            .unwrap();
        records.remove("paths/000000");
        assert_eq!(
            PathRecords::from_records(&records, PATH_RANGE_KEY).unwrap_err(),
            ComponentError::MissingRecord("paths/000000".into())
        );
    }

    #[test]
    fn postings_disambiguate_files_and_overloads_deterministically() {
        let definitions = vec![
            (meta("run", "a.ts", 1), 0),
            (meta("run", "b.ts", 1), 0),
            // Same path/name/span can occur for overload declarations; ordinal disambiguates it.
            (meta("run", "a.ts", 1), 1),
        ];
        let (postings, metadata) = build_symbol_components(definitions.clone(), 2).unwrap();
        let ids = postings.definition_ids("RUN");
        assert_eq!(ids.len(), 3);
        assert_eq!(
            ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
            3
        );
        let paths: Vec<_> = ids
            .iter()
            .map(|id| metadata.definition(id).unwrap().path)
            .collect();
        assert_eq!(paths.iter().filter(|path| *path == "a.ts").count(), 2);
        assert_eq!(paths.iter().filter(|path| *path == "b.ts").count(), 1);

        let mut reversed = definitions;
        reversed.reverse();
        let (postings_reversed, metadata_reversed) = build_symbol_components(reversed, 2).unwrap();
        assert_eq!(postings, postings_reversed);
        assert_eq!(metadata, metadata_reversed);

        let posting_records = postings
            .to_records(SYMBOL_RECORD_PREFIX, SYMBOL_RANGE_KEY)
            .unwrap();
        let metadata_records = metadata
            .to_records(SYMBOL_META_RECORD_PREFIX, SYMBOL_META_RANGE_KEY)
            .unwrap();
        assert!(posting_records.contains_key("meta/symbol-ranges"));
        assert!(metadata_records.contains_key("meta/symbol-meta-ranges"));
        assert!(
            metadata_records
                .keys()
                .any(|key| key.starts_with("symbol-meta/"))
        );
    }
}
