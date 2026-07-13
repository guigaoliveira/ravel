//! Exact per-file edge ownership and incremental adjacency maintenance.

use crate::model::{Edge, EdgeConfidence, EdgeKind};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct OwnedEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub confidence_kind: u8,
    pub score_bits: u32,
    pub reason: String,
    pub type_only: bool,
}

impl From<&Edge> for OwnedEdge {
    fn from(edge: &Edge) -> Self {
        let (confidence_kind, score, reason) = match &edge.confidence {
            EdgeConfidence::Resolved { score, reason } => (0, *score, reason.as_ref()),
            EdgeConfidence::Candidate { score, reason } => (1, *score, reason.as_ref()),
            EdgeConfidence::Unresolved { score, reason } => (2, *score, reason.as_ref()),
        };
        Self {
            from: edge.from.clone(),
            to: edge.to.clone(),
            kind: edge.kind.clone(),
            confidence_kind,
            score_bits: score.to_bits(),
            reason: reason.to_owned(),
            type_only: edge.type_only,
        }
    }
}

impl OwnedEdge {
    pub fn to_edge(&self) -> Edge {
        let score = f32::from_bits(self.score_bits);
        let reason = std::sync::Arc::from(self.reason.as_str());
        let confidence = match self.confidence_kind {
            0 => EdgeConfidence::Resolved { score, reason },
            1 => EdgeConfidence::Candidate { score, reason },
            _ => EdgeConfidence::Unresolved { score, reason },
        };
        Edge {
            from: self.from.clone(),
            to: self.to.clone(),
            kind: self.kind.clone(),
            confidence,
            type_only: self.type_only,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct IncrementalGraphState {
    pub format_version: u32,
    pub by_file: BTreeMap<String, BTreeSet<OwnedEdge>>,
    pub edge_refcounts: BTreeMap<OwnedEdge, u32>,
    pub forward_refcounts: BTreeMap<String, BTreeMap<String, u32>>,
    pub reverse_refcounts: BTreeMap<String, BTreeMap<String, u32>>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct IncrementalGraphOverlay {
    pub file_upserts: BTreeMap<String, BTreeSet<OwnedEdge>>,
    pub file_tombstones: BTreeSet<String>,
    pub edge_counts: BTreeMap<OwnedEdge, Option<u32>>,
    pub adjacency_upserts: BTreeMap<String, Adjacency>,
    pub adjacency_tombstones: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Adjacency {
    pub forward: BTreeSet<String>,
    pub reverse: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct IncrementalGraphShardSet {
    pub format_version: u32,
    pub shard_bits: u8,
    pub shards: BTreeMap<u16, IncrementalGraphShard>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct IncrementalGraphShard {
    pub by_file: BTreeMap<String, BTreeSet<OwnedEdge>>,
    pub edge_refcounts: BTreeMap<OwnedEdge, u32>,
    pub forward_refcounts: BTreeMap<String, BTreeMap<String, u32>>,
    pub reverse_refcounts: BTreeMap<String, BTreeMap<String, u32>>,
}

impl IncrementalGraphState {
    pub const FORMAT_VERSION: u32 = 1;

    pub fn from_contributions(contributions: &BTreeMap<String, Vec<Edge>>) -> Self {
        let mut state = Self {
            format_version: Self::FORMAT_VERSION,
            ..Self::default()
        };
        let updates = contributions
            .iter()
            .map(|(path, edges)| (path.clone(), Some(edges.clone())))
            .collect();
        state.replace_files(updates);
        state
    }

    pub fn replace_files(
        &mut self,
        updates: BTreeMap<String, Option<Vec<Edge>>>,
    ) -> IncrementalGraphOverlay {
        let mut overlay = IncrementalGraphOverlay::default();
        let mut touched_nodes = BTreeSet::new();
        let mut touched_edges = BTreeSet::new();
        for (path, replacement) in updates {
            if let Some(old) = self.by_file.remove(&path) {
                for edge in old {
                    touched_nodes.insert(edge.from.clone());
                    touched_nodes.insert(edge.to.clone());
                    touched_edges.insert(edge.clone());
                    self.remove_edge(&edge);
                }
            }
            match replacement {
                Some(edges) => {
                    let owned: BTreeSet<OwnedEdge> = edges.iter().map(OwnedEdge::from).collect();
                    for edge in &owned {
                        touched_nodes.insert(edge.from.clone());
                        touched_nodes.insert(edge.to.clone());
                        touched_edges.insert(edge.clone());
                        self.add_edge(edge);
                    }
                    self.by_file.insert(path.clone(), owned.clone());
                    overlay.file_upserts.insert(path, owned);
                }
                None => {
                    overlay.file_tombstones.insert(path);
                }
            }
        }
        for edge in touched_edges {
            overlay
                .edge_counts
                .insert(edge.clone(), self.edge_refcounts.get(&edge).copied());
        }
        for node in touched_nodes {
            let adjacency = self.adjacency(&node);
            if adjacency.forward.is_empty() && adjacency.reverse.is_empty() {
                overlay.adjacency_tombstones.insert(node);
            } else {
                overlay.adjacency_upserts.insert(node, adjacency);
            }
        }
        overlay
    }

    pub fn apply_overlay(&mut self, overlay: &IncrementalGraphOverlay) {
        let mut updates = BTreeMap::new();
        for path in &overlay.file_tombstones {
            updates.insert(path.clone(), None);
        }
        for (path, edges) in &overlay.file_upserts {
            updates.insert(
                path.clone(),
                Some(edges.iter().map(OwnedEdge::to_edge).collect()),
            );
        }
        self.replace_files(updates);
    }

    pub fn edges(&self) -> Vec<Edge> {
        self.edge_refcounts.keys().map(OwnedEdge::to_edge).collect()
    }

    pub fn adjacency(&self, node: &str) -> Adjacency {
        Adjacency {
            forward: self
                .forward_refcounts
                .get(node)
                .into_iter()
                .flat_map(|neighbors| neighbors.keys().cloned())
                .collect(),
            reverse: self
                .reverse_refcounts
                .get(node)
                .into_iter()
                .flat_map(|neighbors| neighbors.keys().cloned())
                .collect(),
        }
    }

    fn add_edge(&mut self, edge: &OwnedEdge) {
        let count = self.edge_refcounts.entry(edge.clone()).or_default();
        *count += 1;
        if *count == 1 {
            *self
                .forward_refcounts
                .entry(edge.from.clone())
                .or_default()
                .entry(edge.to.clone())
                .or_default() += 1;
            *self
                .reverse_refcounts
                .entry(edge.to.clone())
                .or_default()
                .entry(edge.from.clone())
                .or_default() += 1;
        }
    }

    fn remove_edge(&mut self, edge: &OwnedEdge) {
        let Some(count) = self.edge_refcounts.get_mut(edge) else {
            return;
        };
        *count -= 1;
        if *count != 0 {
            return;
        }
        self.edge_refcounts.remove(edge);
        remove_neighbor(&mut self.forward_refcounts, &edge.from, &edge.to);
        remove_neighbor(&mut self.reverse_refcounts, &edge.to, &edge.from);
    }

    pub fn into_shards(self, shard_bits: u8) -> Option<IncrementalGraphShardSet> {
        if shard_bits > 16 {
            return None;
        }
        let mut set = IncrementalGraphShardSet {
            format_version: Self::FORMAT_VERSION,
            shard_bits,
            ..IncrementalGraphShardSet::default()
        };
        for (key, value) in self.by_file {
            set.shards
                .entry(graph_shard_id(&key, shard_bits))
                .or_default()
                .by_file
                .insert(key, value);
        }
        for (key, value) in self.edge_refcounts {
            let encoded = bincode::serialize(&key).ok()?;
            set.shards
                .entry(graph_shard_id_bytes(&encoded, shard_bits))
                .or_default()
                .edge_refcounts
                .insert(key, value);
        }
        for (key, value) in self.forward_refcounts {
            set.shards
                .entry(graph_shard_id(&key, shard_bits))
                .or_default()
                .forward_refcounts
                .insert(key, value);
        }
        for (key, value) in self.reverse_refcounts {
            set.shards
                .entry(graph_shard_id(&key, shard_bits))
                .or_default()
                .reverse_refcounts
                .insert(key, value);
        }
        Some(set)
    }
}

impl IncrementalGraphShardSet {
    pub fn into_state(self) -> Option<IncrementalGraphState> {
        if self.format_version != IncrementalGraphState::FORMAT_VERSION || self.shard_bits > 16 {
            return None;
        }
        let mut state = IncrementalGraphState {
            format_version: self.format_version,
            ..IncrementalGraphState::default()
        };
        for shard in self.shards.into_values() {
            state.by_file.extend(shard.by_file);
            state.edge_refcounts.extend(shard.edge_refcounts);
            state.forward_refcounts.extend(shard.forward_refcounts);
            state.reverse_refcounts.extend(shard.reverse_refcounts);
        }
        Some(state)
    }
}

fn graph_shard_id(key: &str, bits: u8) -> u16 {
    graph_shard_id_bytes(key.as_bytes(), bits)
}

fn graph_shard_id_bytes(key: &[u8], bits: u8) -> u16 {
    if bits == 0 {
        return 0;
    }
    let digest = blake3::hash(key);
    u16::from_be_bytes([digest.as_bytes()[0], digest.as_bytes()[1]]) >> (16 - bits)
}

fn remove_neighbor(map: &mut BTreeMap<String, BTreeMap<String, u32>>, node: &str, neighbor: &str) {
    let Some(neighbors) = map.get_mut(node) else {
        return;
    };
    let Some(count) = neighbors.get_mut(neighbor) else {
        return;
    };
    *count -= 1;
    if *count == 0 {
        neighbors.remove(neighbor);
    }
    if neighbors.is_empty() {
        map.remove(node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        resolver::{
            ResolutionUniverse, resolve_edges_with_contributions, resolve_subset_with_contributions,
        },
        scanner::parse_source,
    };
    use tempfile::tempdir;

    #[test]
    fn replacing_owned_file_edges_matches_clean_full_resolution() {
        let root = tempdir().unwrap();
        let old = BTreeMap::from([
            (
                "a.ts".into(),
                parse_source("a.ts", b"export function a() { return b(); }"),
            ),
            (
                "b.ts".into(),
                parse_source("b.ts", b"export function b() {}"),
            ),
        ]);
        let (old_edges, old_owned) =
            resolve_edges_with_contributions(root.path(), &old, &Default::default());
        let mut state = IncrementalGraphState::from_contributions(&old_owned);
        assert_eq!(state.edges(), old_edges);
        assert_eq!(
            state.clone().into_shards(8).unwrap().into_state().unwrap(),
            state
        );

        let mut new = old;
        new.insert(
            "a.ts".into(),
            parse_source("a.ts", b"export function a() { return c(); }"),
        );
        new.insert(
            "c.ts".into(),
            parse_source("c.ts", b"export function c() {}"),
        );
        let (new_edges, new_owned) =
            resolve_edges_with_contributions(root.path(), &new, &Default::default());
        let updates = ["a.ts", "c.ts"]
            .into_iter()
            .map(|path| {
                (
                    path.to_owned(),
                    Some(new_owned.get(path).cloned().unwrap_or_default()),
                )
            })
            .collect();
        let overlay = state.replace_files(updates);
        assert!(!overlay.adjacency_upserts.is_empty());
        assert_eq!(state.edges(), new_edges);
    }

    #[test]
    fn persisted_universe_subset_matches_full_and_rejects_fingerprint_drift() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("a.ts"), b"import { b } from './b'").unwrap();
        std::fs::write(root.path().join("b.ts"), b"export function b() {}").unwrap();
        let artifacts = BTreeMap::from([
            (
                "a.ts".into(),
                parse_source("a.ts", b"import { b } from './b'; export const a = b();"),
            ),
            (
                "b.ts".into(),
                parse_source("b.ts", b"export function b() {}"),
            ),
            (
                "other.ts".into(),
                parse_source("other.ts", b"export const other = 1"),
            ),
        ]);
        let config = Default::default();
        let universe = ResolutionUniverse::build(&artifacts, &config);
        let subset = BTreeMap::from([("a.ts".into(), artifacts["a.ts"].clone())]);
        let (_, full_owned) = resolve_edges_with_contributions(root.path(), &artifacts, &config);
        let (_, subset_owned) =
            resolve_subset_with_contributions(root.path(), &subset, &universe, &config).unwrap();
        assert_eq!(subset_owned["a.ts"], full_owned["a.ts"]);

        let mut drifted = config;
        drifted.max_candidates += 1;
        assert!(
            resolve_subset_with_contributions(root.path(), &subset, &universe, &drifted).is_none()
        );

        let mut updated = universe;
        let old = artifacts.get("other.ts").unwrap();
        let replacement = parse_source("renamed.ts", b"export function renamed() {}");
        updated.replace_artifact(Some(old), Some(&replacement));
        let mut rebuilt = artifacts;
        rebuilt.remove("other.ts");
        rebuilt.insert("renamed.ts".into(), replacement);
        assert_eq!(
            updated,
            ResolutionUniverse::build(&rebuilt, &Default::default())
        );
    }
}
