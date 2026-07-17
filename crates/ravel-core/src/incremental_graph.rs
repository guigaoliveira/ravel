//! Exact per-file edge ownership and incremental adjacency maintenance.

use crate::model::{Edge, EdgeConfidence, EdgeKind, EdgeProvenance, Span};
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
    pub source_path: Option<String>,
    pub span: Option<Span>,
    pub provenance: EdgeProvenance,
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
            source_path: edge.source_path.clone(),
            span: edge.span,
            provenance: edge.provenance.clone(),
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
            source_path: self.source_path.clone(),
            span: self.span,
            provenance: self.provenance.clone(),
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

/// Per-file graph delta. Adjacency for a touched node is encoded exactly one way: a full
/// replacement in `forward_refcounts`/`reverse_refcounts` (`None` removes the node — also the
/// adaptive fallback when most neighbors changed), or per-neighbor absolute counts in
/// `forward_changes`/`reverse_changes` (`Some(count)` sets, `None` removes the neighbor).
/// Absolute values keep every encoding idempotent. Hub nodes with thousands of neighbors
/// previously serialized their whole adjacency map into every overlay.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct IncrementalGraphOverlay {
    pub file_upserts: BTreeMap<String, BTreeSet<OwnedEdge>>,
    pub file_tombstones: BTreeSet<String>,
    pub edge_counts: BTreeMap<OwnedEdge, Option<u32>>,
    pub forward_refcounts: BTreeMap<String, Option<BTreeMap<String, u32>>>,
    pub reverse_refcounts: BTreeMap<String, Option<BTreeMap<String, u32>>>,
    pub forward_changes: BTreeMap<String, BTreeMap<String, Option<u32>>>,
    pub reverse_changes: BTreeMap<String, BTreeMap<String, Option<u32>>>,
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

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct GraphFileShard {
    pub by_file: BTreeMap<String, BTreeSet<OwnedEdge>>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct GraphEdgeShard {
    /// Refcounts keyed by the 128-bit prefix of blake3(bincode(edge)). Collisions are
    /// accepted by design (~2^-90 at workspace scale); the full edges live in `by_file`.
    pub edge_refcounts: BTreeMap<u128, u32>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct GraphAdjShard {
    pub forward_refcounts: BTreeMap<String, BTreeMap<String, u32>>,
    pub reverse_refcounts: BTreeMap<String, BTreeMap<String, u32>>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct GraphSectionShards {
    pub format_version: u32,
    pub file_bits: u8,
    pub edge_bits: u8,
    pub adj_bits: u8,
    pub files: BTreeMap<u16, GraphFileShard>,
    pub edges: BTreeMap<u16, GraphEdgeShard>,
    pub adjacency: BTreeMap<u16, GraphAdjShard>,
}

impl IncrementalGraphState {
    pub const FORMAT_VERSION: u32 = 2;

    /// Build ownership and adjacency directly from the final edge slice.
    ///
    /// Full indexing already retains that slice for the published snapshot; constructing an
    /// intermediate `BTreeMap<String, Vec<Edge>>` duplicates every edge and dominates peak RSS on
    /// large workspaces.
    pub fn from_edges(edges: &[Edge]) -> Self {
        let mut state = Self {
            format_version: Self::FORMAT_VERSION,
            ..Self::default()
        };
        for edge in edges {
            let path = edge.source_path.as_deref().unwrap_or(&edge.from);
            let owned = OwnedEdge::from(edge);
            state.add_edge(&owned);
            state
                .by_file
                .entry(path.to_owned())
                .or_default()
                .insert(owned);
        }
        state
    }

    pub fn from_contributions(contributions: &BTreeMap<String, Vec<Edge>>) -> Self {
        let mut state = Self {
            format_version: Self::FORMAT_VERSION,
            ..Self::default()
        };
        for (path, edges) in contributions {
            let owned: BTreeSet<_> = edges.iter().map(OwnedEdge::from).collect();
            for edge in &owned {
                state.add_edge(edge);
            }
            state.by_file.insert(path.clone(), owned);
        }
        state
    }

    pub fn replace_files(
        &mut self,
        updates: BTreeMap<String, Option<Vec<Edge>>>,
    ) -> IncrementalGraphOverlay {
        self.replace_owned_files(
            updates
                .into_iter()
                .map(|(path, edges)| {
                    (
                        path,
                        edges.map(|edges| edges.iter().map(OwnedEdge::from).collect()),
                    )
                })
                .collect(),
        )
    }

    pub fn replace_owned_files(
        &mut self,
        updates: BTreeMap<String, Option<BTreeSet<OwnedEdge>>>,
    ) -> IncrementalGraphOverlay {
        let mut overlay = IncrementalGraphOverlay::default();
        let mut touched_edges = BTreeSet::new();
        for (path, replacement) in updates {
            if let Some(old) = self.by_file.remove(&path) {
                for edge in old {
                    touched_edges.insert(edge.clone());
                    self.remove_edge(&edge);
                }
            }
            match replacement {
                Some(owned) => {
                    for edge in &owned {
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
        // Only (node, neighbor) pairs that appear in a touched edge can have changed adjacency
        // counts; everything else in the node's map is untouched.
        let mut touched_forward: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut touched_reverse: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for edge in touched_edges {
            touched_forward
                .entry(edge.from.clone())
                .or_default()
                .insert(edge.to.clone());
            touched_reverse
                .entry(edge.to.clone())
                .or_default()
                .insert(edge.from.clone());
            let count = self.edge_refcounts.get(&edge).copied();
            overlay.edge_counts.insert(edge, count);
        }
        Self::snapshot_adjacency(
            &self.forward_refcounts,
            touched_forward,
            &mut overlay.forward_refcounts,
            &mut overlay.forward_changes,
        );
        Self::snapshot_adjacency(
            &self.reverse_refcounts,
            touched_reverse,
            &mut overlay.reverse_refcounts,
            &mut overlay.reverse_changes,
        );
        overlay
    }

    /// Encode each touched node adaptively: full map (or removal) when most neighbors changed,
    /// per-neighbor absolute counts otherwise.
    fn snapshot_adjacency(
        state: &BTreeMap<String, BTreeMap<String, u32>>,
        touched: BTreeMap<String, BTreeSet<String>>,
        full: &mut BTreeMap<String, Option<BTreeMap<String, u32>>>,
        changes: &mut BTreeMap<String, BTreeMap<String, Option<u32>>>,
    ) {
        for (node, neighbors) in touched {
            match state.get(&node) {
                None => {
                    full.insert(node, None);
                }
                Some(map) if neighbors.len() >= map.len() => {
                    full.insert(node, Some(map.clone()));
                }
                Some(map) => {
                    changes.insert(
                        node,
                        neighbors
                            .into_iter()
                            .map(|neighbor| {
                                let count = map.get(&neighbor).copied();
                                (neighbor, count)
                            })
                            .collect(),
                    );
                }
            }
        }
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

    pub fn edge_count(&self) -> usize {
        self.edge_refcounts.len()
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

    /// Split the state into three independent key-spaces, each shardable with its own bit width.
    /// The file section holds ownership; the edge section holds refcounts keyed by 128-bit edge
    /// digest; the adjacency section holds forward/reverse maps. Any lookup decodes only the
    /// section it needs.
    pub fn into_section_shards(
        self,
        file_bits: u8,
        edge_bits: u8,
        adj_bits: u8,
    ) -> Option<GraphSectionShards> {
        if file_bits > 16 || edge_bits > 16 || adj_bits > 16 {
            return None;
        }
        let mut out = GraphSectionShards {
            format_version: Self::FORMAT_VERSION,
            file_bits,
            edge_bits,
            adj_bits,
            ..GraphSectionShards::default()
        };
        for (key, value) in self.by_file {
            out.files
                .entry(graph_shard_id(&key, file_bits))
                .or_default()
                .by_file
                .insert(key, value);
        }
        for (key, value) in self.edge_refcounts {
            let digest = owned_edge_digest(&key)?;
            out.edges
                .entry(digest_shard_id(digest, edge_bits))
                .or_default()
                .edge_refcounts
                .insert(digest, value);
        }
        for (key, value) in self.forward_refcounts {
            out.adjacency
                .entry(graph_shard_id(&key, adj_bits))
                .or_default()
                .forward_refcounts
                .insert(key, value);
        }
        for (key, value) in self.reverse_refcounts {
            out.adjacency
                .entry(graph_shard_id(&key, adj_bits))
                .or_default()
                .reverse_refcounts
                .insert(key, value);
        }
        Some(out)
    }

    /// Rebuild the full state from per-file edge ownership. Refcounts and adjacency are a pure
    /// function of `by_file` (same accumulation as `from_edges`), so the file section is the only
    /// section needed to reconstruct.
    pub fn from_owned_by_file(by_file: BTreeMap<String, BTreeSet<OwnedEdge>>) -> Self {
        let mut state = Self {
            format_version: Self::FORMAT_VERSION,
            ..Self::default()
        };
        for (path, edges) in by_file {
            for edge in &edges {
                state.add_edge(edge);
            }
            state.by_file.insert(path, edges);
        }
        state
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

pub(crate) fn graph_shard_id(key: &str, bits: u8) -> u16 {
    graph_shard_id_bytes(key.as_bytes(), bits)
}

/// Shard id of an edge-refcount key without materializing the bincode buffer.
/// Streams the exact bytes `bincode::serialize` would produce into the hasher, so the
/// resulting id matches shards written via [`IncrementalGraphState::into_shards`]. Retained as
/// the independent oracle proving [`digest_shard_id`] agrees with the byte-hash id rule.
#[cfg(test)]
pub(crate) fn owned_edge_shard_id(edge: &OwnedEdge, bits: u8) -> Option<u16> {
    if bits == 0 {
        return Some(0);
    }
    if bits > 16 {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    bincode::serialize_into(&mut hasher, edge).ok()?;
    let digest = hasher.finalize();
    Some(u16::from_be_bytes([digest.as_bytes()[0], digest.as_bytes()[1]]) >> (16 - bits))
}

/// 128-bit digest of the edge's bincode bytes, streamed into blake3 (no buffer alloc).
pub(crate) fn owned_edge_digest(edge: &OwnedEdge) -> Option<u128> {
    let mut hasher = blake3::Hasher::new();
    bincode::serialize_into(&mut hasher, edge).ok()?;
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    Some(u128::from_be_bytes(bytes))
}

/// Shard id from a digest: same top-bits rule as `graph_shard_id_bytes`, so one hash serves
/// both the id and the refcount key.
pub(crate) fn digest_shard_id(digest: u128, bits: u8) -> u16 {
    if bits == 0 {
        return 0;
    }
    ((digest >> 112) as u16) >> (16 - bits)
}

pub(crate) fn graph_shard_id_bytes(key: &[u8], bits: u8) -> u16 {
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
                parse_source(
                    "a.ts",
                    b"import { b } from './b'; export function a() { return b(); }",
                ),
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
            parse_source(
                "a.ts",
                b"import { c } from './c'; export function a() { return c(); }",
            ),
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
        assert!(!overlay.forward_refcounts.is_empty());
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

    fn synth_owned(i: usize) -> OwnedEdge {
        OwnedEdge {
            from: format!("mod{i}#sym{i}"),
            to: format!("target{}", i % 7),
            kind: EdgeKind::Calls,
            confidence_kind: 0,
            score_bits: 1.0f32.to_bits(),
            reason: format!("reason{i}"),
            type_only: false,
            source_path: Some(format!("file{}.ts", i % 5)),
            span: None,
            provenance: EdgeProvenance::Ast,
        }
    }

    #[test]
    fn digest_shard_id_matches_owned_edge_shard_id() {
        for i in 0..50 {
            let edge = synth_owned(i);
            let digest = owned_edge_digest(&edge).unwrap();
            for bits in [4u8, 8, 12] {
                assert_eq!(
                    digest_shard_id(digest, bits),
                    owned_edge_shard_id(&edge, bits).unwrap(),
                    "mismatch for edge {i} bits {bits}"
                );
            }
        }
    }

    fn synth_edge(from: &str, to: &str, source: Option<&str>) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            kind: EdgeKind::Import,
            confidence: EdgeConfidence::Resolved {
                score: 1.0,
                reason: std::sync::Arc::from("r"),
            },
            type_only: false,
            source_path: source.map(|s| s.into()),
            span: None,
            provenance: EdgeProvenance::Resolution,
        }
    }

    #[test]
    fn from_owned_by_file_reconstructs_from_edges_state() {
        // Two files importing the same target, plus an edge with source_path: None (keyed by
        // `from`). No exact duplicates, so every refcount is 1 in both constructions.
        let edges = vec![
            synth_edge("a.ts", "t.ts", Some("a.ts")),
            synth_edge("b.ts", "t.ts", Some("b.ts")),
            synth_edge("a.ts", "c.ts", None),
        ];
        let state = IncrementalGraphState::from_edges(&edges);
        assert_eq!(
            IncrementalGraphState::from_owned_by_file(state.by_file.clone()),
            state
        );
    }
}
