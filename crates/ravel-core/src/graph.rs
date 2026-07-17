use crate::{
    incremental_graph::{IncrementalGraphOverlay, OwnedEdge},
    model::{Edge, EdgeConfidence, EdgeKind, EdgeProvenance, IndexSnapshot, Span},
};
use petgraph::{
    algo::{kosaraju_scc, toposort},
    graph::{DiGraph, NodeIndex},
};
use rustc_hash::FxHashMap;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use thiserror::Error;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct QueryLimits {
    pub depth: usize,
    pub nodes: usize,
    pub edges: usize,
    pub bytes: u64,
    pub timeout_ms: u64,
    pub page_size: usize,
}
impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            depth: 32,
            nodes: 10_000,
            edges: 50_000,
            bytes: 32 * 1024 * 1024,
            timeout_ms: 5_000,
            page_size: 100,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct QueryPage {
    pub snapshot_id: String,
    pub items: Vec<String>,
    pub next_cursor: Option<String>,
    pub truncated: bool,
    pub reason: Option<String>,
    pub visited_nodes: usize,
    pub visited_edges: usize,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum QueryError {
    #[error("query cancelled")]
    Cancelled,
    #[error("query requires a non-empty node")]
    EmptyNode,
}

/// Compact on-disk adjacency (node strings + u32 neighbor lists).
/// Much smaller/faster than re-deserializing full `Edge` rows + rebuilding maps.
#[derive(
    Debug,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct CompactGraph {
    pub snapshot_id: String,
    pub nodes: Vec<String>,
    pub forward: Vec<Vec<u32>>,
    pub reverse: Vec<Vec<u32>>,
    pub edge_count: u32,
    pub relations: Vec<CompactRelation>,
    pub forward_relation_ids: Vec<Vec<u32>>,
    pub reverse_relation_ids: Vec<Vec<u32>>,
}

#[derive(Debug, Clone, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub(crate) struct FlatCompactGraph {
    pub(crate) snapshot_id: String,
    pub(crate) nodes: Vec<String>,
    pub(crate) forward_offsets: Vec<u32>,
    pub(crate) forward_values: Vec<u32>,
    pub(crate) reverse_offsets: Vec<u32>,
    pub(crate) reverse_values: Vec<u32>,
    pub(crate) edge_count: u32,
    pub(crate) relations: Vec<CompactRelation>,
    pub(crate) forward_relation_offsets: Vec<u32>,
    pub(crate) forward_relation_values: Vec<u32>,
    pub(crate) reverse_relation_offsets: Vec<u32>,
    pub(crate) reverse_relation_values: Vec<u32>,
}

impl FlatCompactGraph {
    fn flatten(rows: Vec<Vec<u32>>) -> (Vec<u32>, Vec<u32>) {
        let total = rows.iter().map(Vec::len).sum();
        let mut offsets = Vec::with_capacity(rows.len() + 1);
        let mut values = Vec::with_capacity(total);
        offsets.push(0);
        for row in rows {
            values.extend(row);
            offsets.push(values.len() as u32);
        }
        (offsets, values)
    }

    fn expand(offsets: &[u32], values: &[u32]) -> Vec<Vec<u32>> {
        offsets
            .windows(2)
            .map(|window| values[window[0] as usize..window[1] as usize].to_vec())
            .collect()
    }

    pub(crate) fn from_compact(compact: CompactGraph) -> Self {
        let (forward_offsets, forward_values) = Self::flatten(compact.forward);
        let (reverse_offsets, reverse_values) = Self::flatten(compact.reverse);
        let (forward_relation_offsets, forward_relation_values) =
            Self::flatten(compact.forward_relation_ids);
        let (reverse_relation_offsets, reverse_relation_values) =
            Self::flatten(compact.reverse_relation_ids);
        Self {
            snapshot_id: compact.snapshot_id,
            nodes: compact.nodes,
            forward_offsets,
            forward_values,
            reverse_offsets,
            reverse_values,
            edge_count: compact.edge_count,
            relations: compact.relations,
            forward_relation_offsets,
            forward_relation_values,
            reverse_relation_offsets,
            reverse_relation_values,
        }
    }

    pub(crate) fn into_compact(self) -> CompactGraph {
        CompactGraph {
            snapshot_id: self.snapshot_id,
            nodes: self.nodes,
            forward: Self::expand(&self.forward_offsets, &self.forward_values),
            reverse: Self::expand(&self.reverse_offsets, &self.reverse_values),
            edge_count: self.edge_count,
            relations: self.relations,
            forward_relation_ids: Self::expand(
                &self.forward_relation_offsets,
                &self.forward_relation_values,
            ),
            reverse_relation_ids: Self::expand(
                &self.reverse_relation_offsets,
                &self.reverse_relation_values,
            ),
        }
    }
}

#[derive(
    Debug,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct CompactRelation {
    pub from: u32,
    pub to: u32,
    pub kind: EdgeKind,
    pub source_path: Option<u32>,
    pub span: Option<Span>,
    /// 0 resolved, 1 candidate, 2 unresolved.
    pub confidence: u8,
    pub type_only: bool,
    pub provenance: EdgeProvenance,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct RelationView {
    pub node: String,
    pub kind: EdgeKind,
    pub source_path: Option<String>,
    pub span: Option<Span>,
    pub confidence: &'static str,
    pub type_only: bool,
    pub provenance: EdgeProvenance,
}

/// Borrowed, serialize-only mirror of [`CompactGraph`] (identical field order/types → same
/// bincode/serde wire bytes) that avoids cloning the graph vectors when publishing.
#[derive(Debug, serde::Serialize)]
pub struct CompactGraphRef<'a> {
    pub snapshot_id: &'a str,
    pub nodes: Vec<&'a str>,
    pub forward: &'a [Vec<u32>],
    pub reverse: &'a [Vec<u32>],
    pub edge_count: u32,
    pub relations: &'a [CompactRelation],
    pub forward_relation_ids: &'a [Vec<u32>],
    pub reverse_relation_ids: &'a [Vec<u32>],
}

#[derive(Debug)]
pub struct GraphIndex {
    nodes: Vec<Arc<str>>,
    /// Maps node name → index into `nodes` / adjacency vectors.
    node_index: FxHashMap<Arc<str>, u32>,
    forward: Vec<Vec<u32>>,
    reverse: Vec<Vec<u32>>,
    edge_count: usize,
    relations: Vec<CompactRelation>,
    forward_relation_ids: Vec<Vec<u32>>,
    reverse_relation_ids: Vec<Vec<u32>>,
    relation_file_overlays: BTreeMap<String, Option<BTreeSet<Arc<OwnedEdge>>>>,
    relation_overlay_nodes: BTreeSet<String>,
    overlay_forward_relations: BTreeMap<String, Vec<Arc<OwnedEdge>>>,
    overlay_reverse_relations: BTreeMap<String, Vec<Arc<OwnedEdge>>>,
    inactive_nodes: BTreeSet<u32>,
    snapshot_id: String,
    package_graph: OnceLock<DiGraph<String, ()>>,
}

impl GraphIndex {
    pub fn from_snapshot(snapshot: &IndexSnapshot) -> Self {
        Self::from_edges(&snapshot.edges, snapshot.id.stable_key())
    }

    pub fn from_edges(edges: &[Edge], snapshot_id: String) -> Self {
        // ~2 endpoints per edge; reserve to cut rehash cost.
        let cap = (edges.len().saturating_mul(2) / 3).max(16);
        let mut node_index: FxHashMap<Arc<str>, u32> =
            FxHashMap::with_capacity_and_hasher(cap, Default::default());
        let mut nodes: Vec<Arc<str>> = Vec::with_capacity(cap);
        let mut forward: Vec<Vec<u32>> = Vec::with_capacity(cap);
        let mut reverse: Vec<Vec<u32>> = Vec::with_capacity(cap);
        let mut forward_relation_ids: Vec<Vec<u32>> = Vec::with_capacity(cap);
        let mut reverse_relation_ids: Vec<Vec<u32>> = Vec::with_capacity(cap);

        let intern = |name: &str,
                      nodes: &mut Vec<Arc<str>>,
                      node_index: &mut FxHashMap<Arc<str>, u32>,
                      forward: &mut Vec<Vec<u32>>,
                      reverse: &mut Vec<Vec<u32>>,
                      forward_relation_ids: &mut Vec<Vec<u32>>,
                      reverse_relation_ids: &mut Vec<Vec<u32>>|
         -> u32 {
            if let Some(&id) = node_index.get(name) {
                return id;
            }
            let id = nodes.len() as u32;
            let name: Arc<str> = Arc::from(name);
            nodes.push(Arc::clone(&name));
            node_index.insert(name, id);
            forward.push(Vec::new());
            reverse.push(Vec::new());
            forward_relation_ids.push(Vec::new());
            reverse_relation_ids.push(Vec::new());
            id
        };

        let mut relations = Vec::with_capacity(edges.len());

        for edge in edges {
            let from = intern(
                &edge.from,
                &mut nodes,
                &mut node_index,
                &mut forward,
                &mut reverse,
                &mut forward_relation_ids,
                &mut reverse_relation_ids,
            );
            let to = intern(
                &edge.to,
                &mut nodes,
                &mut node_index,
                &mut forward,
                &mut reverse,
                &mut forward_relation_ids,
                &mut reverse_relation_ids,
            );
            let source_path = edge.source_path.as_deref().map(|path| {
                intern(
                    path,
                    &mut nodes,
                    &mut node_index,
                    &mut forward,
                    &mut reverse,
                    &mut forward_relation_ids,
                    &mut reverse_relation_ids,
                )
            });
            forward[from as usize].push(to);
            reverse[to as usize].push(from);
            let relation_id = relations.len() as u32;
            relations.push(CompactRelation {
                from,
                to,
                kind: edge.kind.clone(),
                source_path,
                span: edge.span,
                confidence: match edge.confidence {
                    EdgeConfidence::Resolved { .. } => 0,
                    EdgeConfidence::Candidate { .. } => 1,
                    EdgeConfidence::Unresolved { .. } => 2,
                },
                type_only: edge.type_only,
                provenance: edge.provenance.clone(),
            });
            forward_relation_ids[from as usize].push(relation_id);
            reverse_relation_ids[to as usize].push(relation_id);
        }

        // Multiple AST sites may connect the same two declarations. Traversal and risk operate
        // on unique neighbors; counting duplicate sites as separate dependencies inflates degree
        // and can change risk without changing the actual blast radius.
        for neighbors in forward.iter_mut().chain(reverse.iter_mut()) {
            neighbors.sort_unstable();
            neighbors.dedup();
        }
        // Agent context consumes a bounded prefix. Persist semantic sites before module plumbing
        // so a high-import symbol still exposes its calls/instantiations without scanning or
        // allocating the full degree at query time.
        let relation_key = |relation_id: &u32| {
            let relation = &relations[*relation_id as usize];
            (
                relation_display_priority(&relation.kind),
                relation
                    .source_path
                    .and_then(|path| nodes.get(path as usize))
                    .map(AsRef::as_ref)
                    .unwrap_or(""),
                relation.span,
                relation.from,
                relation.to,
            )
        };
        for relation_ids in forward_relation_ids
            .iter_mut()
            .chain(reverse_relation_ids.iter_mut())
        {
            relation_ids.sort_unstable_by_key(&relation_key);
        }
        let edge_count = relations.len();

        // Neighbor order is not required for correct query pages (items are sorted).
        Self {
            nodes,
            node_index,
            forward,
            reverse,
            edge_count,
            relations,
            forward_relation_ids,
            reverse_relation_ids,
            relation_file_overlays: BTreeMap::new(),
            relation_overlay_nodes: BTreeSet::new(),
            overlay_forward_relations: BTreeMap::new(),
            overlay_reverse_relations: BTreeMap::new(),
            inactive_nodes: BTreeSet::new(),
            snapshot_id,
            package_graph: OnceLock::new(),
        }
    }

    pub fn from_compact(compact: CompactGraph) -> Self {
        // Pre-size to node count so the cold-load rebuild does not rehash.
        let node_count = compact.nodes.len();
        let nodes: Vec<Arc<str>> = compact.nodes.into_iter().map(Arc::from).collect();
        let mut node_index: FxHashMap<Arc<str>, u32> =
            FxHashMap::with_capacity_and_hasher(node_count, Default::default());
        for (i, name) in nodes.iter().enumerate() {
            node_index.insert(Arc::clone(name), i as u32);
        }
        let edge_count = compact.edge_count as usize;
        Self {
            nodes,
            node_index,
            forward: compact.forward,
            reverse: compact.reverse,
            edge_count,
            relations: compact.relations,
            forward_relation_ids: compact.forward_relation_ids,
            reverse_relation_ids: compact.reverse_relation_ids,
            relation_file_overlays: BTreeMap::new(),
            relation_overlay_nodes: BTreeSet::new(),
            overlay_forward_relations: BTreeMap::new(),
            overlay_reverse_relations: BTreeMap::new(),
            inactive_nodes: BTreeSet::new(),
            snapshot_id: compact.snapshot_id,
            package_graph: OnceLock::new(),
        }
    }

    pub fn to_compact(&self) -> CompactGraph {
        CompactGraph {
            snapshot_id: self.snapshot_id.clone(),
            nodes: self.nodes.iter().map(ToString::to_string).collect(),
            forward: self.forward.clone(),
            reverse: self.reverse.clone(),
            edge_count: self.edge_count as u32,
            relations: self.relations.clone(),
            forward_relation_ids: self.forward_relation_ids.clone(),
            reverse_relation_ids: self.reverse_relation_ids.clone(),
        }
    }

    /// Borrowed view for serialization — same wire layout as [`CompactGraph`] but without
    /// cloning the node/adjacency vectors (used by the publish path).
    pub fn as_compact_ref(&self) -> CompactGraphRef<'_> {
        CompactGraphRef {
            snapshot_id: &self.snapshot_id,
            nodes: self.nodes.iter().map(AsRef::as_ref).collect(),
            forward: &self.forward,
            reverse: &self.reverse,
            edge_count: self.edge_count as u32,
            relations: &self.relations,
            forward_relation_ids: &self.forward_relation_ids,
            reverse_relation_ids: &self.reverse_relation_ids,
        }
    }

    /// Return at most `limit` detailed sites while reporting the complete relation count.
    /// This keeps high-degree agent queries O(limit) in allocations instead of O(degree).
    pub fn direct_relations_limit(
        &self,
        node: &str,
        reverse: bool,
        limit: usize,
    ) -> (Vec<RelationView>, usize) {
        let Some(&node_id) = self.node_index.get(node) else {
            return (Vec::new(), 0);
        };
        let relation_ids = if reverse {
            self.reverse_relation_ids.get(node_id as usize)
        } else {
            self.forward_relation_ids.get(node_id as usize)
        };
        if self.relation_overlay_nodes.contains(node) {
            let mut items = relation_ids
                .into_iter()
                .flatten()
                .filter_map(|relation_id| self.relations.get(*relation_id as usize))
                .filter(|relation| {
                    relation
                        .source_path
                        .and_then(|id| self.nodes.get(id as usize))
                        .is_none_or(|path| !self.relation_file_overlays.contains_key(path.as_ref()))
                })
                .map(|relation| self.relation_view(relation, reverse))
                .collect::<Vec<_>>();
            let overlay_relations = if reverse {
                self.overlay_reverse_relations.get(node)
            } else {
                self.overlay_forward_relations.get(node)
            };
            items.extend(
                overlay_relations
                    .into_iter()
                    .flatten()
                    .map(|edge| RelationView {
                        node: if reverse {
                            edge.from.clone()
                        } else {
                            edge.to.clone()
                        },
                        kind: edge.kind.clone(),
                        source_path: edge.source_path.clone(),
                        span: edge.span,
                        confidence: match edge.confidence_kind {
                            0 => "resolved",
                            1 => "candidate",
                            _ => "unresolved",
                        },
                        type_only: edge.type_only,
                        provenance: edge.provenance.clone(),
                    }),
            );
            items.sort_unstable_by(|left, right| {
                (
                    relation_display_priority(&left.kind),
                    left.source_path.as_deref().unwrap_or(""),
                    left.span,
                    left.node.as_str(),
                )
                    .cmp(&(
                        relation_display_priority(&right.kind),
                        right.source_path.as_deref().unwrap_or(""),
                        right.span,
                        right.node.as_str(),
                    ))
            });
            let total = items.len();
            items.truncate(limit);
            return (items, total);
        }
        let total = relation_ids.map_or(0, Vec::len);
        let items = relation_ids
            .into_iter()
            .flatten()
            .take(limit)
            .filter_map(|relation_id| self.relations.get(*relation_id as usize))
            .map(|relation| self.relation_view(relation, reverse))
            .collect();
        (items, total)
    }

    fn relation_view(&self, relation: &CompactRelation, reverse: bool) -> RelationView {
        let related = if reverse { relation.from } else { relation.to };
        RelationView {
            node: self.nodes[related as usize].to_string(),
            kind: relation.kind.clone(),
            source_path: relation
                .source_path
                .and_then(|id| self.nodes.get(id as usize))
                .map(ToString::to_string),
            span: relation.span,
            confidence: match relation.confidence {
                0 => "resolved",
                1 => "candidate",
                _ => "unresolved",
            },
            type_only: relation.type_only,
            provenance: relation.provenance.clone(),
        }
    }

    /// Apply a persisted per-file graph delta without expanding the global edge set.
    pub(crate) fn apply_incremental_overlay(
        &mut self,
        overlay: &IncrementalGraphOverlay,
        snapshot_id: &str,
        edge_count: usize,
    ) {
        for path in &overlay.file_tombstones {
            self.relation_file_overlays.insert(path.clone(), None);
        }
        for (path, edges) in &overlay.file_upserts {
            self.relation_file_overlays.insert(
                path.clone(),
                Some(edges.iter().cloned().map(Arc::new).collect()),
            );
        }
        self.relation_overlay_nodes.extend(
            overlay
                .edge_counts
                .keys()
                .flat_map(|edge| [edge.from.clone(), edge.to.clone()]),
        );

        let mut touched_ids = BTreeSet::new();
        let full_nodes: BTreeSet<_> = overlay
            .forward_refcounts
            .keys()
            .chain(overlay.reverse_refcounts.keys())
            .map(String::as_str)
            .collect();
        for node in full_nodes {
            let id = self.intern_node(node);
            touched_ids.insert(id);
            if let Some(neighbors) = overlay.forward_refcounts.get(node) {
                self.forward[id as usize] = neighbors
                    .iter()
                    .flat_map(|neighbors| neighbors.keys())
                    .map(|neighbor| self.intern_node(neighbor))
                    .collect();
            }
            if let Some(neighbors) = overlay.reverse_refcounts.get(node) {
                self.reverse[id as usize] = neighbors
                    .iter()
                    .flat_map(|neighbors| neighbors.keys())
                    .map(|neighbor| self.intern_node(neighbor))
                    .collect();
            }
        }
        for (changes, forward) in [
            (&overlay.forward_changes, true),
            (&overlay.reverse_changes, false),
        ] {
            for (node, neighbors) in changes {
                let id = self.intern_node(node);
                touched_ids.insert(id);
                let resolved: Vec<(u32, bool)> = neighbors
                    .iter()
                    .map(|(neighbor, count)| (self.intern_node(neighbor), count.is_some()))
                    .collect();
                let list = if forward {
                    &mut self.forward[id as usize]
                } else {
                    &mut self.reverse[id as usize]
                };
                for (neighbor_id, present) in resolved {
                    if present {
                        if !list.contains(&neighbor_id) {
                            list.push(neighbor_id);
                        }
                    } else {
                        list.retain(|&existing| existing != neighbor_id);
                    }
                }
            }
        }
        for id in touched_ids {
            if self.forward[id as usize].is_empty() && self.reverse[id as usize].is_empty() {
                self.inactive_nodes.insert(id);
            } else {
                self.inactive_nodes.remove(&id);
            }
        }
        self.edge_count = edge_count;
        self.snapshot_id.clear();
        self.snapshot_id.push_str(snapshot_id);
        self.package_graph = OnceLock::new();
    }

    /// Build node-local relation lookups once after all persisted overlays have been applied.
    pub(crate) fn finish_incremental_overlays(&mut self) {
        self.overlay_forward_relations.clear();
        self.overlay_reverse_relations.clear();
        for edge in self.relation_file_overlays.values().flatten().flatten() {
            self.overlay_forward_relations
                .entry(edge.from.clone())
                .or_default()
                .push(Arc::clone(edge));
            self.overlay_reverse_relations
                .entry(edge.to.clone())
                .or_default()
                .push(Arc::clone(edge));
        }
    }

    fn intern_node(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.node_index.get(name) {
            return id;
        }
        let id = self.nodes.len() as u32;
        let name: Arc<str> = Arc::from(name);
        self.nodes.push(Arc::clone(&name));
        self.node_index.insert(name, id);
        self.forward.push(Vec::new());
        self.reverse.push(Vec::new());
        self.forward_relation_ids.push(Vec::new());
        self.reverse_relation_ids.push(Vec::new());
        id
    }

    pub fn callers_of(
        &self,
        node: &str,
        limits: &QueryLimits,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<QueryPage, QueryError> {
        self.walk_internal(node, &self.reverse, limits, cancel, false)
            .map(|(page, _)| page)
    }

    pub(crate) fn callers_of_with_depths(
        &self,
        node: &str,
        limits: &QueryLimits,
    ) -> Result<(QueryPage, FxHashMap<u32, usize>), QueryError> {
        self.walk_internal(node, &self.reverse, limits, None, true)
    }

    pub fn impact_analysis(
        &self,
        node: &str,
        limits: &QueryLimits,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<QueryPage, QueryError> {
        self.walk_internal(node, &self.forward, limits, cancel, false)
            .map(|(page, _)| page)
    }

    pub fn package_cycles(&self) -> Vec<Vec<String>> {
        let graph = self.package_graph();
        kosaraju_scc(graph)
            .into_iter()
            .filter(|component| component.len() > 1)
            .map(|component| {
                component
                    .into_iter()
                    .map(|index| graph[index].clone())
                    .collect()
            })
            .collect()
    }

    pub fn package_order(&self) -> Vec<String> {
        let graph = self.package_graph();
        toposort(graph, None)
            .unwrap_or_default()
            .into_iter()
            .map(|index| graph[index].clone())
            .collect()
    }

    /// Package→package edges. O(P + E_pkg) — independent of symbol-node count.
    pub fn package_edges(&self) -> Vec<(String, String)> {
        let graph = self.package_graph();
        let mut edges: Vec<(String, String)> = graph
            .edge_indices()
            .filter_map(|e| {
                let (a, b) = graph.edge_endpoints(e)?;
                Some((graph[a].clone(), graph[b].clone()))
            })
            .collect();
        edges.sort();
        edges.dedup();
        edges
    }

    /// Number of package nodes in the collapsed graph.
    pub fn package_count(&self) -> usize {
        self.package_graph().node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.edge_count
    }

    pub fn snapshot_id(&self) -> &str {
        &self.snapshot_id
    }

    pub fn contains_node(&self, name: &str) -> bool {
        self.node_index
            .get(name)
            .is_some_and(|id| !self.inactive_nodes.contains(id))
    }

    pub fn node_names(&self) -> impl Iterator<Item = &str> {
        self.node_entries().map(|(_, name)| name)
    }

    /// Active compact node IDs paired with their names.
    ///
    /// Consumers that use ID-based adjacency must not derive IDs with
    /// `node_names().enumerate()`: inactive overlay nodes make the dense
    /// iterator position diverge from the stable compact ID.
    pub fn node_entries(&self) -> impl Iterator<Item = (u32, &str)> {
        self.nodes.iter().enumerate().filter_map(|(id, name)| {
            let id = id as u32;
            (!self.inactive_nodes.contains(&id)).then_some((id, name.as_ref()))
        })
    }

    pub fn in_degree(&self, name: &str) -> usize {
        self.node_index
            .get(name)
            .map(|&i| self.reverse.get(i as usize).map(Vec::len).unwrap_or(0))
            .unwrap_or(0)
    }

    pub fn out_degree(&self, name: &str) -> usize {
        self.node_index
            .get(name)
            .map(|&i| self.forward.get(i as usize).map(Vec::len).unwrap_or(0))
            .unwrap_or(0)
    }

    pub fn neighbors_forward(&self, name: &str) -> Vec<String> {
        self.neighbors(name, &self.forward)
    }

    pub fn neighbors_reverse(&self, name: &str) -> Vec<String> {
        self.neighbors(name, &self.reverse)
    }

    /// Zero-alloc neighbor ids for hot loops (analysis/export).
    pub fn neighbor_ids_forward(&self, name: &str) -> &[u32] {
        self.neighbor_ids(name, &self.forward)
    }

    pub fn neighbor_ids_reverse(&self, name: &str) -> &[u32] {
        self.neighbor_ids(name, &self.reverse)
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len().saturating_sub(self.inactive_nodes.len())
    }

    /// Node name → compact ID. Returns `None` if the name is not in the graph.
    pub fn node_id(&self, name: &str) -> Option<u32> {
        self.node_index
            .get(name)
            .copied()
            .filter(|id| !self.inactive_nodes.contains(id))
    }

    pub fn node_name(&self, id: u32) -> Option<&str> {
        (!self.inactive_nodes.contains(&id))
            .then(|| self.nodes.get(id as usize).map(AsRef::as_ref))
            .flatten()
    }

    /// In-degree lookup when the caller already has the compact node ID.
    pub fn in_degree_id(&self, id: u32) -> usize {
        self.reverse.get(id as usize).map(Vec::len).unwrap_or(0)
    }

    /// Out-degree lookup when the caller already has the compact node ID.
    pub fn out_degree_id(&self, id: u32) -> usize {
        self.forward.get(id as usize).map(Vec::len).unwrap_or(0)
    }

    /// Reverse adjacency lookup by node ID (zero-alloc — no hash lookup).
    pub fn neighbor_ids_reverse_id(&self, id: u32) -> &[u32] {
        self.reverse
            .get(id as usize)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn neighbor_ids<'a>(&'a self, name: &str, adj: &'a [Vec<u32>]) -> &'a [u32] {
        let Some(&idx) = self.node_index.get(name) else {
            return &[];
        };
        adj.get(idx as usize).map(Vec::as_slice).unwrap_or(&[])
    }

    fn neighbors(&self, name: &str, adj: &[Vec<u32>]) -> Vec<String> {
        self.neighbor_ids(name, adj)
            .iter()
            .map(|&i| self.nodes[i as usize].to_string())
            .collect()
    }

    fn package_graph(&self) -> &DiGraph<String, ()> {
        self.package_graph.get_or_init(|| {
            // Compute each node's package once (O(N)); the old code recomputed
            // `package_name` — a per-call allocation — for every edge endpoint (O(E)).
            let node_pkg: Vec<String> = self.nodes.iter().map(|n| package_name(n)).collect();
            let mut package_graph = DiGraph::new();
            let mut package_nodes: FxHashMap<&str, NodeIndex> = FxHashMap::default();
            let mut seen_edges: rustc_hash::FxHashSet<(NodeIndex, NodeIndex)> =
                rustc_hash::FxHashSet::default();

            for (from_idx, neighbors) in self.forward.iter().enumerate() {
                if self.inactive_nodes.contains(&(from_idx as u32)) {
                    continue;
                }
                let from_index = match package_nodes.get(node_pkg[from_idx].as_str()) {
                    Some(&idx) => idx,
                    None => {
                        let idx = package_graph.add_node(node_pkg[from_idx].clone());
                        package_nodes.insert(node_pkg[from_idx].as_str(), idx);
                        idx
                    }
                };
                for &to_idx in neighbors {
                    if self.inactive_nodes.contains(&to_idx) {
                        continue;
                    }
                    let to_pkg = node_pkg[to_idx as usize].as_str();
                    let to_index = match package_nodes.get(to_pkg) {
                        Some(&idx) => idx,
                        None => {
                            let idx = package_graph.add_node(node_pkg[to_idx as usize].clone());
                            package_nodes.insert(node_pkg[to_idx as usize].as_str(), idx);
                            idx
                        }
                    };
                    if seen_edges.insert((from_index, to_index)) {
                        package_graph.add_edge(from_index, to_index, ());
                    }
                }
            }
            package_graph
        })
    }

    fn walk_internal(
        &self,
        node: &str,
        graph: &[Vec<u32>],
        limits: &QueryLimits,
        cancel: Option<&Arc<AtomicBool>>,
        capture_depths: bool,
    ) -> Result<(QueryPage, FxHashMap<u32, usize>), QueryError> {
        if node.is_empty() {
            return Err(QueryError::EmptyNode);
        }
        let deadline = Instant::now() + Duration::from_millis(limits.timeout_ms);

        // Unknown node: empty expansion, still a valid bounded page.
        let Some(&start) = self
            .node_index
            .get(node)
            .filter(|id| !self.inactive_nodes.contains(id))
        else {
            return Ok((
                QueryPage {
                    snapshot_id: self.snapshot_id.clone(),
                    items: Vec::new(),
                    next_cursor: None,
                    truncated: false,
                    reason: None,
                    visited_nodes: 1,
                    visited_edges: 0,
                },
                FxHashMap::default(),
            ));
        };

        let mut queue = VecDeque::from([(start, 0usize)]);
        let mut seen: rustc_hash::FxHashSet<u32> = rustc_hash::FxHashSet::default();
        let mut depths: FxHashMap<u32, usize> = FxHashMap::default();
        // Accumulate node IDs, not cloned names — we clone only the returned page below.
        let mut item_ids: Vec<u32> = Vec::new();
        let mut visited_edges = 0usize;
        let mut output_bytes = 0u64;
        let mut truncated = false;
        let mut reason = None;
        let mut steps = 0usize;

        while let Some((current, depth)) = queue.pop_front() {
            if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                return Err(QueryError::Cancelled);
            }
            // Clock reads are amortized: check the deadline every 512 pops, not every one.
            steps += 1;
            if steps % 512 == 0 && Instant::now() >= deadline {
                truncated = true;
                reason = Some("deadline".into());
                break;
            }
            if depth > limits.depth {
                truncated = true;
                reason = Some("depth".into());
                break;
            }
            if seen.contains(&current) {
                continue;
            }
            // Enforce hard budgets before admitting work. The previous post-insert checks
            // reported `visited_nodes = limit + 1` / `visited_edges = limit + 1`.
            if seen.len() >= limits.nodes {
                truncated = true;
                reason = Some("nodes".into());
                break;
            }
            if current != start {
                let node_bytes = self.nodes[current as usize].len() as u64;
                if output_bytes.saturating_add(node_bytes) > limits.bytes {
                    truncated = true;
                    reason = Some("bytes".into());
                    break;
                }
                output_bytes += node_bytes;
            }
            seen.insert(current);
            if capture_depths {
                depths.insert(current, depth);
            }
            if current != start {
                item_ids.push(current);
            }
            if let Some(neighbors) = graph.get(current as usize) {
                for &next in neighbors {
                    if visited_edges >= limits.edges {
                        truncated = true;
                        reason = Some("edges".into());
                        break;
                    }
                    visited_edges += 1;
                    queue.push_back((next, depth + 1));
                }
            }
            if truncated {
                break;
            }
        }
        item_ids.sort_by(|&a, &b| self.nodes[a as usize].cmp(&self.nodes[b as usize]));
        let cursor = limits.page_size.min(item_ids.len());
        let next_cursor = (cursor < item_ids.len()).then(|| cursor.to_string());
        let page: Vec<String> = item_ids
            .into_iter()
            .take(cursor)
            .map(|id| self.nodes[id as usize].to_string())
            .collect();
        Ok((
            QueryPage {
                snapshot_id: self.snapshot_id.clone(),
                items: page,
                next_cursor,
                truncated,
                reason,
                visited_nodes: seen.len(),
                visited_edges,
            },
            depths,
        ))
    }
}

fn relation_display_priority(kind: &EdgeKind) -> u8 {
    match kind {
        EdgeKind::Calls => 0,
        EdgeKind::Instantiates => 1,
        EdgeKind::Decorates => 2,
        EdgeKind::References => 3,
        EdgeKind::TypeOf => 4,
        EdgeKind::Extends | EdgeKind::Implements => 5,
        EdgeKind::Import => 6,
        EdgeKind::ReExport => 7,
    }
}

fn package_name(path: &str) -> String {
    let path = path
        .strip_prefix("symbol://")
        .and_then(|value| value.split_once('#').map(|(path, _)| path))
        .unwrap_or(path);
    // No intermediate Vec: return the segment after the first apps|libs|packages
    // marker, else the first segment, else "workspace". `split` is lazy/zero-alloc.
    let first = path.split('/').next();
    let mut scan = path.split('/');
    while let Some(part) = scan.next() {
        if matches!(part, "apps" | "libs" | "packages") {
            if let Some(next) = scan.next() {
                return next.to_owned();
            }
        }
    }
    first
        .map(str::to_owned)
        .unwrap_or_else(|| "workspace".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, EdgeConfidence, EdgeKind, IndexSnapshot, SnapshotId};
    use std::collections::BTreeMap;

    fn graph() -> GraphIndex {
        let edges = vec![edge("a", "b"), edge("b", "c"), edge("c", "a")];
        GraphIndex::from_snapshot(&IndexSnapshot {
            id: SnapshotId {
                root: "r".into(),
                worktree: "w".into(),
                revision: "v".into(),
                content_state: "c".into(),
                schema_version: 1,
                grammar_version: "g".into(),
                config_hash: "h".into(),
            },
            files: BTreeMap::new(),
            edges,
        })
    }

    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            kind: EdgeKind::Import,
            confidence: EdgeConfidence::Resolved {
                score: 1.0,
                reason: "test".into(),
            },
            type_only: false,
            source_path: Some("site.ts".into()),
            span: Some(Span {
                start_byte: 10,
                end_byte: 11,
                start_line: 2,
                start_column: 4,
                end_line: 2,
                end_column: 5,
            }),
            provenance: EdgeProvenance::Ast,
        }
    }

    #[test]
    fn reverse_walk_is_bounded_and_cycles_are_detected() {
        let graph = graph();
        let limits = QueryLimits {
            nodes: 1,
            ..Default::default()
        };
        let result = graph.callers_of("a", &limits, None).unwrap();
        assert!(result.truncated);
        assert_eq!(result.visited_nodes, 1);
        assert_eq!(graph.package_cycles().len(), 1);
    }

    #[test]
    fn bounded_relation_views_prioritize_semantic_sites_over_import_plumbing() {
        let mut import = edge("consumer.ts", "target");
        import.kind = EdgeKind::Import;
        let mut call = edge("caller", "target");
        call.kind = EdgeKind::Calls;
        let graph = GraphIndex::from_edges(&[import, call], "snapshot".into());
        let (relations, total) = graph.direct_relations_limit("target", true, 1);
        assert_eq!(total, 2);
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].kind, EdgeKind::Calls);
    }

    #[test]
    fn edge_budget_is_a_hard_limit() {
        let graph = graph();
        let limits = QueryLimits {
            edges: 1,
            ..Default::default()
        };
        let result = graph.impact_analysis("a", &limits, None).unwrap();
        assert!(result.truncated);
        assert_eq!(result.reason.as_deref(), Some("edges"));
        assert_eq!(result.visited_edges, 1);
    }

    #[test]
    fn byte_budget_is_enforced_before_materializing_names() {
        let graph = graph();
        let limits = QueryLimits {
            bytes: 0,
            ..Default::default()
        };
        let result = graph.impact_analysis("a", &limits, None).unwrap();
        assert!(result.items.is_empty());
        assert!(result.truncated);
        assert_eq!(result.reason.as_deref(), Some("bytes"));
    }

    #[test]
    fn cancellation_is_observed() {
        let graph = graph();
        let flag = Arc::new(AtomicBool::new(true));
        assert_eq!(
            graph.impact_analysis("a", &QueryLimits::default(), Some(&flag)),
            Err(QueryError::Cancelled)
        );
    }

    #[test]
    fn compact_roundtrip_preserves_walk() {
        let graph = graph();
        let restored = GraphIndex::from_compact(graph.to_compact());
        let page = restored
            .impact_analysis("a", &QueryLimits::default(), None)
            .unwrap();
        assert_eq!(page.items, vec!["b".to_string(), "c".to_string()]);
        assert_eq!(restored.edge_count(), 3);
        let (relations, total) = restored.direct_relations_limit("a", false, usize::MAX);
        assert_eq!(total, 1);
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].node, "b");
        assert_eq!(relations[0].source_path.as_deref(), Some("site.ts"));
        assert_eq!(relations[0].span.unwrap().start_line, 2);
    }

    #[test]
    fn unknown_node_returns_empty_page() {
        let graph = graph();
        let page = graph
            .callers_of("missing", &QueryLimits::default(), None)
            .unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.visited_nodes, 1);
    }
}
