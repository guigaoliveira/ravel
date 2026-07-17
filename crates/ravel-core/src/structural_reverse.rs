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
    pub module_importers: MembershipOverlay,
    pub basename_importers: MembershipOverlay,
    pub symbol_definers: MembershipOverlay,
    pub symbol_referrers: MembershipOverlay,
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

/// Delta-first overlay for membership maps (`key → set of member paths`).
///
/// Each key is encoded exactly one way: full replacement (`upserts`), key removal
/// (`tombstones`), or per-member `added`/`removed` deltas — whichever is smallest. Hub keys with
/// thousands of members previously serialized their entire set into every overlay; deltas keep
/// overlays proportional to the change. All encodings are idempotent (absolute set semantics),
/// so replaying an overlay twice yields the same state.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct MembershipOverlay {
    pub upserts: BTreeMap<String, BTreeSet<String>>,
    pub tombstones: BTreeSet<String>,
    pub added: BTreeMap<String, BTreeSet<String>>,
    pub removed: BTreeMap<String, BTreeSet<String>>,
}

impl MembershipOverlay {
    pub fn apply_to(&self, target: &mut BTreeMap<String, BTreeSet<String>>) {
        for key in &self.tombstones {
            target.remove(key);
        }
        for (key, value) in &self.upserts {
            target.insert(key.clone(), value.clone());
        }
        for (key, members) in &self.added {
            target
                .entry(key.clone())
                .or_default()
                .extend(members.iter().cloned());
        }
        for (key, members) in &self.removed {
            if let Some(set) = target.get_mut(key) {
                for member in members {
                    set.remove(member);
                }
                if set.is_empty() {
                    target.remove(key);
                }
            }
        }
    }

    /// Fold `newer` on top of `self` so that applying the composition equals applying `self`
    /// then `newer`. Keeps the one-encoding-per-key invariant.
    pub fn compose(&mut self, newer: MembershipOverlay) {
        for key in newer.tombstones {
            self.upserts.remove(&key);
            self.added.remove(&key);
            self.removed.remove(&key);
            self.tombstones.insert(key);
        }
        for (key, value) in newer.upserts {
            self.tombstones.remove(&key);
            self.added.remove(&key);
            self.removed.remove(&key);
            self.upserts.insert(key, value);
        }
        for (key, members) in newer.added {
            if let Some(set) = self.upserts.get_mut(&key) {
                set.extend(members);
                continue;
            }
            if self.tombstones.remove(&key) {
                // Emptied then re-added: the final set is exactly the added members.
                self.upserts.insert(key, members);
                continue;
            }
            if let Some(removed) = self.removed.get_mut(&key) {
                for member in &members {
                    removed.remove(member);
                }
                if removed.is_empty() {
                    self.removed.remove(&key);
                }
            }
            self.added.entry(key).or_default().extend(members);
        }
        for (key, members) in newer.removed {
            if let Some(set) = self.upserts.get_mut(&key) {
                for member in &members {
                    set.remove(member);
                }
                if set.is_empty() {
                    self.upserts.remove(&key);
                    self.tombstones.insert(key);
                }
                continue;
            }
            if self.tombstones.contains(&key) {
                continue;
            }
            if let Some(added) = self.added.get_mut(&key) {
                for member in &members {
                    added.remove(member);
                }
                if added.is_empty() {
                    self.added.remove(&key);
                }
            }
            self.removed.entry(key).or_default().extend(members);
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

    /// Shard a freshly built reverse index by moving its maps instead of cloning the complete
    /// workspace representation. Full indexing otherwise keeps two copies of every reverse key
    /// and membership set at the RSS peak.
    pub fn from_owned_index(
        index: StructuralReverseIndex,
        shard_bits: u8,
    ) -> Result<Self, ReverseShardError> {
        validate_bits(shard_bits)?;
        let mut result = Self {
            format_version: Self::FORMAT_VERSION,
            resolver_fingerprint: index.resolver_fingerprint,
            shard_bits,
            shards: BTreeMap::new(),
        };
        distribute_owned(
            index.files,
            shard_bits,
            |shard| &mut shard.files,
            &mut result.shards,
        );
        distribute_owned(
            index.module_importers,
            shard_bits,
            |shard| &mut shard.module_importers,
            &mut result.shards,
        );
        distribute_owned(
            index.basename_importers,
            shard_bits,
            |shard| &mut shard.basename_importers,
            &mut result.shards,
        );
        distribute_owned(
            index.symbol_definers,
            shard_bits,
            |shard| &mut shard.symbol_definers,
            &mut result.shards,
        );
        distribute_owned(
            index.symbol_referrers,
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
                module_importers: membership_diff(&old.module_importers, &new.module_importers),
                basename_importers: membership_diff(
                    &old.basename_importers,
                    &new.basename_importers,
                ),
                symbol_definers: membership_diff(&old.symbol_definers, &new.symbol_definers),
                symbol_referrers: membership_diff(&old.symbol_referrers, &new.symbol_referrers),
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
            delta.module_importers.apply_to(&mut shard.module_importers);
            delta
                .basename_importers
                .apply_to(&mut shard.basename_importers);
            delta.symbol_definers.apply_to(&mut shard.symbol_definers);
            delta.symbol_referrers.apply_to(&mut shard.symbol_referrers);
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
        let mut overlays: BTreeMap<u16, ReverseShardOverlay> = BTreeMap::new();
        // Membership keys touched by any update. Overlay values are snapshotted once per key
        // after all mutations; snapshotting inside the mutation loop cloned large membership
        // sets once per (path, key) pair and dominated structural sync time on hub symbols.
        let mut touched = TouchedMembership::default();
        for (path, replacement) in updates {
            let file_shard = shard_id(&path, self.shard_bits);
            let old = self
                .shards
                .get_mut(&file_shard)
                .and_then(|shard| shard.files.remove(&path));
            if let Some(old) = old.as_ref() {
                self.update_contribution_maps(&path, old, false, &mut touched);
            }
            if let Some(new) = replacement {
                self.update_contribution_maps(&path, &new, true, &mut touched);
                self.shards
                    .entry(file_shard)
                    .or_default()
                    .files
                    .insert(path.clone(), new.clone());
                let files = &mut overlays.entry(file_shard).or_default().files;
                files.tombstones.remove(&path);
                files.upserts.insert(path, new);
            } else {
                let files = &mut overlays.entry(file_shard).or_default().files;
                files.upserts.remove(&path);
                files.tombstones.insert(path);
            }
        }
        self.snapshot_membership(
            touched.module_importers,
            &mut overlays,
            |shard| &shard.module_importers,
            |overlay| &mut overlay.module_importers,
        );
        self.snapshot_membership(
            touched.basename_importers,
            &mut overlays,
            |shard| &shard.basename_importers,
            |overlay| &mut overlay.basename_importers,
        );
        self.snapshot_membership(
            touched.symbol_definers,
            &mut overlays,
            |shard| &shard.symbol_definers,
            |overlay| &mut overlay.symbol_definers,
        );
        self.snapshot_membership(
            touched.symbol_referrers,
            &mut overlays,
            |shard| &shard.symbol_referrers,
            |overlay| &mut overlay.symbol_referrers,
        );
        self.shards.retain(|_, shard| !shard.is_empty());
        overlays.retain(|_, overlay| !overlay.is_empty());
        ReverseOverlaySet {
            format_version: ReverseOverlaySet::FORMAT_VERSION,
            resolver_fingerprint: self.resolver_fingerprint.clone(),
            shard_bits: self.shard_bits,
            shards: overlays,
        }
    }

    /// Encode each touched key adaptively: tombstone when the set is gone, per-member delta when
    /// the delta is smaller than the final set, full upsert otherwise.
    fn snapshot_membership(
        &self,
        touched: BTreeMap<String, MemberDelta>,
        overlays: &mut BTreeMap<u16, ReverseShardOverlay>,
        field: impl Fn(&ReverseShard) -> &BTreeMap<String, BTreeSet<String>>,
        overlay_field: impl Fn(&mut ReverseShardOverlay) -> &mut MembershipOverlay,
    ) {
        for (key, delta) in touched {
            let id = shard_id(&key, self.shard_bits);
            let value = self
                .shards
                .get(&id)
                .and_then(|shard| field(shard).get(&key));
            let overlay = overlay_field(overlays.entry(id).or_default());
            match value {
                None => {
                    overlay.tombstones.insert(key);
                }
                Some(set) if delta.added.len() + delta.removed.len() >= set.len() => {
                    overlay.upserts.insert(key, set.clone());
                }
                Some(_) => {
                    if !delta.added.is_empty() {
                        overlay.added.insert(key.clone(), delta.added);
                    }
                    if !delta.removed.is_empty() {
                        overlay.removed.insert(key, delta.removed);
                    }
                }
            }
        }
    }

    fn update_contribution_maps(
        &mut self,
        path: &str,
        contribution: &FileContribution,
        insert: bool,
        touched: &mut TouchedMembership,
    ) {
        for key in &contribution.module_candidates {
            update_reverse_membership(self, key, path, insert, |shard| &mut shard.module_importers);
            touched
                .module_importers
                .entry(key.clone())
                .or_default()
                .record(path, insert);
        }
        for key in &contribution.bare_specifiers {
            update_reverse_membership(self, key, path, insert, |shard| {
                &mut shard.basename_importers
            });
            touched
                .basename_importers
                .entry(key.clone())
                .or_default()
                .record(path, insert);
        }
        for key in &contribution.symbol_definitions {
            update_reverse_membership(self, key, path, insert, |shard| &mut shard.symbol_definers);
            touched
                .symbol_definers
                .entry(key.clone())
                .or_default()
                .record(path, insert);
        }
        for key in &contribution.symbol_references {
            update_reverse_membership(self, key, path, insert, |shard| &mut shard.symbol_referrers);
            touched
                .symbol_referrers
                .entry(key.clone())
                .or_default()
                .record(path, insert);
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

/// Membership keys touched during `replace_files`, per reverse category, with the exact members
/// added/removed. Snapshotted into the returned overlay once per key after all mutations.
#[derive(Default)]
struct TouchedMembership {
    module_importers: BTreeMap<String, MemberDelta>,
    basename_importers: BTreeMap<String, MemberDelta>,
    symbol_definers: BTreeMap<String, MemberDelta>,
    symbol_referrers: BTreeMap<String, MemberDelta>,
}

#[derive(Default)]
struct MemberDelta {
    added: BTreeSet<String>,
    removed: BTreeSet<String>,
}

impl MemberDelta {
    fn record(&mut self, path: &str, insert: bool) {
        if insert {
            self.removed.remove(path);
            self.added.insert(path.to_owned());
        } else {
            self.added.remove(path);
            self.removed.insert(path.to_owned());
        }
    }
}

fn update_reverse_membership(
    set: &mut ReverseShardSet,
    key: &str,
    path: &str,
    insert: bool,
    field: impl Fn(&mut ReverseShard) -> &mut BTreeMap<String, BTreeSet<String>>,
) {
    let id = shard_id(key, set.shard_bits);
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
    /// v2: membership categories moved from full-set `MapOverlay` to delta-first
    /// [`MembershipOverlay`]. v1 overlays are unreadable and rejected at the pack-key level
    /// (`reverse/overlay-v2`), falling back to a full rebuild once.
    pub const FORMAT_VERSION: u32 = 2;
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

impl MembershipOverlay {
    fn is_empty(&self) -> bool {
        self.upserts.is_empty()
            && self.tombstones.is_empty()
            && self.added.is_empty()
            && self.removed.is_empty()
    }
}

/// Adaptive diff of two membership maps: per-member deltas when smaller, full upserts otherwise.
fn membership_diff(
    old: &BTreeMap<String, BTreeSet<String>>,
    new: &BTreeMap<String, BTreeSet<String>>,
) -> MembershipOverlay {
    let mut overlay = MembershipOverlay::default();
    for (key, new_set) in new {
        let Some(old_set) = old.get(key) else {
            overlay.upserts.insert(key.clone(), new_set.clone());
            continue;
        };
        if old_set == new_set {
            continue;
        }
        let added: BTreeSet<String> = new_set.difference(old_set).cloned().collect();
        let removed: BTreeSet<String> = old_set.difference(new_set).cloned().collect();
        if added.len() + removed.len() >= new_set.len() {
            overlay.upserts.insert(key.clone(), new_set.clone());
        } else {
            if !added.is_empty() {
                overlay.added.insert(key.clone(), added);
            }
            if !removed.is_empty() {
                overlay.removed.insert(key.clone(), removed);
            }
        }
    }
    overlay.tombstones = old
        .keys()
        .filter(|key| !new.contains_key(*key))
        .cloned()
        .collect();
    overlay
}

fn validate_bits(bits: u8) -> Result<(), ReverseShardError> {
    (bits <= 16)
        .then_some(())
        .ok_or(ReverseShardError::InvalidShardBits(bits))
}

pub(crate) fn shard_id(key: &str, bits: u8) -> u16 {
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

fn distribute_owned<V>(
    source: BTreeMap<String, V>,
    bits: u8,
    field: impl Fn(&mut ReverseShard) -> &mut BTreeMap<String, V>,
    shards: &mut BTreeMap<u16, ReverseShard>,
) {
    for (key, value) in source {
        field(shards.entry(shard_id(&key, bits)).or_default()).insert(key, value);
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

    /// Build an index where `hub.ts` is imported by many files, so its membership sets are
    /// large enough to force the delta encoding instead of full upserts.
    fn hub_index(root: &std::path::Path, importers: usize, extra: &str) -> StructuralReverseIndex {
        let mut sources = BTreeMap::new();
        for i in 0..importers {
            let path = format!("src/imp{i}.ts");
            let artifact = parse_source(&path, b"import { hub } from './hub';\n");
            sources.insert(path, artifact);
        }
        let extra_artifact = parse_source(
            "src/extra.ts",
            format!("import {{ hub }} from './hub';\nimport {{ x }} from './{extra}';\n")
                .as_bytes(),
        );
        sources.insert("src/extra.ts".to_owned(), extra_artifact);
        StructuralReverseIndex::build(root, &sources, &ResolverConfig::default())
    }

    #[test]
    fn hub_replacement_uses_delta_encoding_and_matches_rebuild() {
        let root = tempdir().unwrap();
        let old_index = hub_index(root.path(), 20, "old");
        let next_index = hub_index(root.path(), 20, "new");
        let mut current = ReverseShardSet::from_index(&old_index, 4).unwrap();
        let expected = ReverseShardSet::from_index(&next_index, 4).unwrap();
        let overlay = current.replace_files(BTreeMap::from([(
            "src/extra.ts".to_owned(),
            Some(next_index.files["src/extra.ts"].clone()),
        )]));
        assert_eq!(current, expected);
        // The hub membership sets (>20 members) must be encoded as deltas, not full sets.
        let has_delta = overlay.shards.values().any(|shard| {
            !shard.basename_importers.added.is_empty()
                || !shard.basename_importers.removed.is_empty()
                || !shard.module_importers.added.is_empty()
                || !shard.module_importers.removed.is_empty()
        });
        assert!(has_delta, "expected delta encoding for hub membership sets");
        let no_hub_upsert = overlay
            .shards
            .values()
            .flat_map(|shard| shard.basename_importers.upserts.values())
            .all(|set| set.len() <= 2);
        assert!(no_hub_upsert, "hub sets must not be serialized in full");
    }

    #[test]
    fn composed_overlays_match_sequential_application() {
        let root = tempdir().unwrap();
        let s0 = hub_index(root.path(), 20, "v0");
        let s1 = hub_index(root.path(), 20, "v1");
        let s2 = hub_index(root.path(), 20, "v2");
        let base = ReverseShardSet::from_index(&s0, 4).unwrap();

        let mut sequential = base.clone();
        let ov1 = sequential.replace_files(BTreeMap::from([(
            "src/extra.ts".to_owned(),
            Some(s1.files["src/extra.ts"].clone()),
        )]));
        let ov2 = sequential.replace_files(BTreeMap::from([(
            "src/extra.ts".to_owned(),
            Some(s2.files["src/extra.ts"].clone()),
        )]));

        let mut composed_target = base.clone();
        composed_target.apply(&ov1).unwrap();
        composed_target.apply(&ov2).unwrap();
        assert_eq!(composed_target, sequential);

        // Compose the membership categories pairwise and verify equivalence too.
        let mut folded = base;
        let mut merged = ov1;
        for (id, newer) in ov2.shards {
            let older = merged.shards.entry(id).or_default();
            older.module_importers.compose(newer.module_importers);
            older.basename_importers.compose(newer.basename_importers);
            older.symbol_definers.compose(newer.symbol_definers);
            older.symbol_referrers.compose(newer.symbol_referrers);
            for key in newer.files.tombstones {
                older.files.upserts.remove(&key);
                older.files.tombstones.insert(key);
            }
            for (key, value) in newer.files.upserts {
                older.files.tombstones.remove(&key);
                older.files.upserts.insert(key, value);
            }
        }
        folded.apply(&merged).unwrap();
        assert_eq!(folded, sequential);
    }
}
