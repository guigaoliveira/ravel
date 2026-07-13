//! Storage-neutral sharded base/overlay representation for structural reverse lookups.
//!
//! Each [`ReverseShard`] is independently serializable. Storage can place bases and overlays in
//! immutable generation packs without hydrating unrelated shards.

use crate::structural::{FileContribution, StructuralReverseIndex};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReverseShardError {
    #[error("reverse shard bits must be in 0..=16, got {0}")]
    InvalidShardBits(u8),
    #[error("reverse overlay resolver fingerprint does not match its base")]
    ResolverMismatch,
    #[error("reverse overlay shard layout does not match its base")]
    LayoutMismatch,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ReverseShardSet {
    pub format_version: u32,
    pub resolver_fingerprint: String,
    pub shard_bits: u8,
    /// Only non-empty shards are materialized.
    pub shards: BTreeMap<u16, ReverseShard>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ReverseShard {
    pub files: BTreeMap<String, FileContribution>,
    pub module_importers: BTreeMap<String, BTreeSet<String>>,
    pub basename_importers: BTreeMap<String, BTreeSet<String>>,
    pub symbol_definers: BTreeMap<String, BTreeSet<String>>,
    pub symbol_referrers: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ReverseOverlaySet {
    pub format_version: u32,
    pub resolver_fingerprint: String,
    pub shard_bits: u8,
    pub shards: BTreeMap<u16, ReverseShardOverlay>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ReverseShardOverlay {
    pub files: MapOverlay<FileContribution>,
    pub module_importers: MapOverlay<BTreeSet<String>>,
    pub basename_importers: MapOverlay<BTreeSet<String>>,
    pub symbol_definers: MapOverlay<BTreeSet<String>>,
    pub symbol_referrers: MapOverlay<BTreeSet<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct MapOverlay<V> {
    pub upserts: BTreeMap<String, V>,
    pub tombstones: BTreeSet<String>,
}

impl<V> Default for MapOverlay<V> {
    fn default() -> Self {
        Self {
            upserts: BTreeMap::new(),
            tombstones: BTreeSet::new(),
        }
    }
}

impl ReverseShardSet {
    pub const FORMAT_VERSION: u32 = 1;

    pub fn from_index(
        index: &StructuralReverseIndex,
        shard_bits: u8,
    ) -> Result<Self, ReverseShardError> {
        validate_bits(shard_bits)?;
        let mut result = Self {
            format_version: Self::FORMAT_VERSION,
            resolver_fingerprint: index.resolver_fingerprint.clone(),
            shard_bits,
            shards: BTreeMap::new(),
        };
        distribute(
            &index.files,
            shard_bits,
            |shard| &mut shard.files,
            &mut result.shards,
        );
        distribute(
            &index.module_importers,
            shard_bits,
            |shard| &mut shard.module_importers,
            &mut result.shards,
        );
        distribute(
            &index.basename_importers,
            shard_bits,
            |shard| &mut shard.basename_importers,
            &mut result.shards,
        );
        distribute(
            &index.symbol_definers,
            shard_bits,
            |shard| &mut shard.symbol_definers,
            &mut result.shards,
        );
        distribute(
            &index.symbol_referrers,
            shard_bits,
            |shard| &mut shard.symbol_referrers,
            &mut result.shards,
        );
        Ok(result)
    }

    pub fn diff(&self, next: &Self) -> Result<ReverseOverlaySet, ReverseShardError> {
        if self.resolver_fingerprint != next.resolver_fingerprint {
            return Err(ReverseShardError::ResolverMismatch);
        }
        if self.shard_bits != next.shard_bits {
            return Err(ReverseShardError::LayoutMismatch);
        }
        let ids: BTreeSet<u16> = self
            .shards
            .keys()
            .chain(next.shards.keys())
            .copied()
            .collect();
        let mut shards = BTreeMap::new();
        for id in ids {
            let empty = ReverseShard::default();
            let old = self.shards.get(&id).unwrap_or(&empty);
            let new = next.shards.get(&id).unwrap_or(&empty);
            let overlay = ReverseShardOverlay {
                files: map_diff(&old.files, &new.files),
                module_importers: map_diff(&old.module_importers, &new.module_importers),
                basename_importers: map_diff(&old.basename_importers, &new.basename_importers),
                symbol_definers: map_diff(&old.symbol_definers, &new.symbol_definers),
                symbol_referrers: map_diff(&old.symbol_referrers, &new.symbol_referrers),
            };
            if !overlay.is_empty() {
                shards.insert(id, overlay);
            }
        }
        Ok(ReverseOverlaySet {
            format_version: ReverseOverlaySet::FORMAT_VERSION,
            resolver_fingerprint: self.resolver_fingerprint.clone(),
            shard_bits: self.shard_bits,
            shards,
        })
    }

    pub fn apply(&mut self, overlay: &ReverseOverlaySet) -> Result<(), ReverseShardError> {
        if self.resolver_fingerprint != overlay.resolver_fingerprint {
            return Err(ReverseShardError::ResolverMismatch);
        }
        if self.shard_bits != overlay.shard_bits {
            return Err(ReverseShardError::LayoutMismatch);
        }
        for (&id, delta) in &overlay.shards {
            let shard = self.shards.entry(id).or_default();
            apply_map(&mut shard.files, &delta.files);
            apply_map(&mut shard.module_importers, &delta.module_importers);
            apply_map(&mut shard.basename_importers, &delta.basename_importers);
            apply_map(&mut shard.symbol_definers, &delta.symbol_definers);
            apply_map(&mut shard.symbol_referrers, &delta.symbol_referrers);
            if shard.is_empty() {
                self.shards.remove(&id);
            }
        }
        Ok(())
    }

    pub fn replace_files(
        &mut self,
        updates: BTreeMap<String, Option<FileContribution>>,
    ) -> ReverseOverlaySet {
        let mut before: BTreeMap<u16, ReverseShard> = BTreeMap::new();
        for (path, replacement) in updates {
            let file_shard = shard_id(&path, self.shard_bits);
            before
                .entry(file_shard)
                .or_insert_with(|| self.shards.get(&file_shard).cloned().unwrap_or_default());
            let old = self
                .shards
                .get_mut(&file_shard)
                .and_then(|shard| shard.files.remove(&path));
            if let Some(old) = old.as_ref() {
                self.update_contribution_maps(&path, old, false, &mut before);
            }
            if let Some(new) = replacement {
                self.update_contribution_maps(&path, &new, true, &mut before);
                self.shards
                    .entry(file_shard)
                    .or_default()
                    .files
                    .insert(path, new);
            }
        }
        self.shards.retain(|_, shard| !shard.is_empty());
        let mut shards = BTreeMap::new();
        for (id, old) in before {
            let empty = ReverseShard::default();
            let new = self.shards.get(&id).unwrap_or(&empty);
            let overlay = ReverseShardOverlay {
                files: map_diff(&old.files, &new.files),
                module_importers: map_diff(&old.module_importers, &new.module_importers),
                basename_importers: map_diff(&old.basename_importers, &new.basename_importers),
                symbol_definers: map_diff(&old.symbol_definers, &new.symbol_definers),
                symbol_referrers: map_diff(&old.symbol_referrers, &new.symbol_referrers),
            };
            if !overlay.is_empty() {
                shards.insert(id, overlay);
            }
        }
        ReverseOverlaySet {
            format_version: ReverseOverlaySet::FORMAT_VERSION,
            resolver_fingerprint: self.resolver_fingerprint.clone(),
            shard_bits: self.shard_bits,
            shards,
        }
    }

    fn update_contribution_maps(
        &mut self,
        path: &str,
        contribution: &FileContribution,
        insert: bool,
        before: &mut BTreeMap<u16, ReverseShard>,
    ) {
        for key in &contribution.module_candidates {
            update_reverse_membership(self, key, path, insert, before, |shard| {
                &mut shard.module_importers
            });
        }
        for key in &contribution.bare_specifiers {
            update_reverse_membership(self, key, path, insert, before, |shard| {
                &mut shard.basename_importers
            });
        }
        for key in &contribution.symbol_definitions {
            update_reverse_membership(self, key, path, insert, before, |shard| {
                &mut shard.symbol_definers
            });
        }
        for key in &contribution.symbol_references {
            update_reverse_membership(self, key, path, insert, before, |shard| {
                &mut shard.symbol_referrers
            });
        }
    }

    pub fn module_importers(&self, key: &str) -> Option<&BTreeSet<String>> {
        self.lookup(key, |shard| &shard.module_importers)
    }

    pub fn affected_files<'a>(
        &self,
        changed_paths: impl IntoIterator<Item = &'a str>,
        changed_symbols: impl IntoIterator<Item = &'a str>,
    ) -> BTreeSet<String> {
        let mut affected = BTreeSet::new();
        for path in changed_paths {
            affected.insert(path.to_owned());
            if let Some(importers) = self.lookup(path, |shard| &shard.module_importers) {
                affected.extend(importers.iter().cloned());
            }
            if let Some(stem) = std::path::Path::new(path)
                .file_stem()
                .and_then(|stem| stem.to_str())
                && let Some(importers) = self.lookup(stem, |shard| &shard.basename_importers)
            {
                affected.extend(importers.iter().cloned());
            }
        }
        for symbol in changed_symbols {
            if let Some(definers) = self.lookup(symbol, |shard| &shard.symbol_definers) {
                affected.extend(definers.iter().cloned());
            }
            if let Some(referrers) = self.lookup(symbol, |shard| &shard.symbol_referrers) {
                affected.extend(referrers.iter().cloned());
            }
        }
        affected
    }

    fn lookup<'a, V>(
        &'a self,
        key: &str,
        field: impl Fn(&'a ReverseShard) -> &'a BTreeMap<String, V>,
    ) -> Option<&'a V> {
        let shard = self.shards.get(&shard_id(key, self.shard_bits))?;
        field(shard).get(key)
    }
}

fn update_reverse_membership(
    set: &mut ReverseShardSet,
    key: &str,
    path: &str,
    insert: bool,
    before: &mut BTreeMap<u16, ReverseShard>,
    field: impl Fn(&mut ReverseShard) -> &mut BTreeMap<String, BTreeSet<String>>,
) {
    let id = shard_id(key, set.shard_bits);
    before
        .entry(id)
        .or_insert_with(|| set.shards.get(&id).cloned().unwrap_or_default());
    let shard = set.shards.entry(id).or_default();
    let map = field(shard);
    if insert {
        map.entry(key.to_owned())
            .or_default()
            .insert(path.to_owned());
    } else if let Some(paths) = map.get_mut(key) {
        paths.remove(path);
        if paths.is_empty() {
            map.remove(key);
        }
    }
}

impl ReverseOverlaySet {
    pub const FORMAT_VERSION: u32 = 1;
}

impl ReverseShard {
    fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.module_importers.is_empty()
            && self.basename_importers.is_empty()
            && self.symbol_definers.is_empty()
            && self.symbol_referrers.is_empty()
    }
}

impl ReverseShardOverlay {
    fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.module_importers.is_empty()
            && self.basename_importers.is_empty()
            && self.symbol_definers.is_empty()
            && self.symbol_referrers.is_empty()
    }
}

impl<V> MapOverlay<V> {
    fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.tombstones.is_empty()
    }
}

fn validate_bits(bits: u8) -> Result<(), ReverseShardError> {
    (bits <= 16)
        .then_some(())
        .ok_or(ReverseShardError::InvalidShardBits(bits))
}

fn shard_id(key: &str, bits: u8) -> u16 {
    if bits == 0 {
        return 0;
    }
    let digest = blake3::hash(key.as_bytes());
    let raw = u16::from_be_bytes([digest.as_bytes()[0], digest.as_bytes()[1]]);
    raw >> (16 - bits)
}

fn distribute<V: Clone>(
    source: &BTreeMap<String, V>,
    bits: u8,
    field: impl Fn(&mut ReverseShard) -> &mut BTreeMap<String, V>,
    shards: &mut BTreeMap<u16, ReverseShard>,
) {
    for (key, value) in source {
        field(shards.entry(shard_id(key, bits)).or_default()).insert(key.clone(), value.clone());
    }
}

fn map_diff<V: Clone + PartialEq>(
    old: &BTreeMap<String, V>,
    new: &BTreeMap<String, V>,
) -> MapOverlay<V> {
    MapOverlay {
        upserts: new
            .iter()
            .filter(|(key, value)| old.get(*key) != Some(*value))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        tombstones: old
            .keys()
            .filter(|key| !new.contains_key(*key))
            .cloned()
            .collect(),
    }
}

fn apply_map<V: Clone>(target: &mut BTreeMap<String, V>, overlay: &MapOverlay<V>) {
    for key in &overlay.tombstones {
        target.remove(key);
    }
    target.extend(overlay.upserts.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        resolver::ResolverConfig, scanner::parse_source, structural::StructuralReverseIndex,
    };
    use tempfile::tempdir;

    fn index(root: &std::path::Path, target_name: &str) -> StructuralReverseIndex {
        let source = parse_source(
            "src/a.ts",
            format!("import {{ x }} from './{target_name}';\n").as_bytes(),
        );
        StructuralReverseIndex::build(
            root,
            &BTreeMap::from([(source.path.clone(), source)]),
            &ResolverConfig::default(),
        )
    }

    #[test]
    fn overlay_roundtrip_matches_rebuilt_shards_and_is_independently_serializable() {
        let root = tempdir().unwrap();
        let base = ReverseShardSet::from_index(&index(root.path(), "old"), 4).unwrap();
        let next = ReverseShardSet::from_index(&index(root.path(), "new"), 4).unwrap();
        let overlay = base.diff(&next).unwrap();
        assert!(!overlay.shards.is_empty());
        for shard in overlay.shards.values() {
            let bytes = bincode::serialize(shard).unwrap();
            let _: ReverseShardOverlay = bincode::deserialize(&bytes).unwrap();
        }
        let mut applied = base;
        applied.apply(&overlay).unwrap();
        assert_eq!(applied, next);
    }

    #[test]
    fn resolver_fingerprint_prevents_cross_config_overlay() {
        let root = tempdir().unwrap();
        let base = ReverseShardSet::from_index(&index(root.path(), "old"), 2).unwrap();
        let mut changed = index(root.path(), "new");
        changed.resolver_fingerprint = "other".into();
        let next = ReverseShardSet::from_index(&changed, 2).unwrap();
        assert_eq!(base.diff(&next), Err(ReverseShardError::ResolverMismatch));
    }

    #[test]
    fn sharded_affected_lookup_matches_unsharded_index() {
        let root = tempdir().unwrap();
        let index = index(root.path(), "future");
        let sharded = ReverseShardSet::from_index(&index, 6).unwrap();
        assert_eq!(
            sharded.affected_files(["src/future.ts"], std::iter::empty()),
            index.affected_files(["src/future.ts"], std::iter::empty())
        );
    }

    #[test]
    fn direct_file_replacement_matches_full_sharded_rebuild() {
        let root = tempdir().unwrap();
        let old_index = index(root.path(), "old");
        let next_index = index(root.path(), "new");
        let mut current = ReverseShardSet::from_index(&old_index, 6).unwrap();
        let expected = ReverseShardSet::from_index(&next_index, 6).unwrap();
        let overlay = current.replace_files(BTreeMap::from([(
            "src/a.ts".to_owned(),
            Some(next_index.files["src/a.ts"].clone()),
        )]));
        assert!(!overlay.shards.is_empty());
        assert_eq!(current, expected);
    }
}
