use crate::model::{Edge, IndexSnapshot};
use petgraph::{
    algo::{kosaraju_scc, toposort},
    graph::{DiGraph, NodeIndex},
};
use rustc_hash::FxHashMap;
use std::{
    collections::VecDeque,
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CompactGraph {
    pub snapshot_id: String,
    pub nodes: Vec<String>,
    pub forward: Vec<Vec<u32>>,
    pub reverse: Vec<Vec<u32>>,
    pub edge_count: u32,
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
}

#[derive(Debug)]
pub struct GraphIndex {
    nodes: Vec<Arc<str>>,
    /// Maps node name → index into `nodes` / adjacency vectors.
    node_index: FxHashMap<Arc<str>, u32>,
    forward: Vec<Vec<u32>>,
    reverse: Vec<Vec<u32>>,
    edge_count: usize,
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

        let intern = |name: &str,
                      nodes: &mut Vec<Arc<str>>,
                      node_index: &mut FxHashMap<Arc<str>, u32>,
                      forward: &mut Vec<Vec<u32>>,
                      reverse: &mut Vec<Vec<u32>>|
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
            id
        };

        for edge in edges {
            let from = intern(
                &edge.from,
                &mut nodes,
                &mut node_index,
                &mut forward,
                &mut reverse,
            );
            let to = intern(
                &edge.to,
                &mut nodes,
                &mut node_index,
                &mut forward,
                &mut reverse,
            );
            forward[from as usize].push(to);
            reverse[to as usize].push(from);
        }

        // Neighbor order is not required for correct query pages (items are sorted).
        Self {
            nodes,
            node_index,
            forward,
            reverse,
            edge_count: edges.len(),
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
        }
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
        self.node_index.contains_key(name)
    }

    pub fn node_names(&self) -> impl Iterator<Item = &str> {
        self.nodes.iter().map(AsRef::as_ref)
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
        self.nodes.len()
    }

    /// Node name → compact ID. Returns `None` if the name is not in the graph.
    pub fn node_id(&self, name: &str) -> Option<u32> {
        self.node_index.get(name).copied()
    }

    pub fn node_name(&self, id: u32) -> Option<&str> {
        self.nodes.get(id as usize).map(AsRef::as_ref)
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
                let from_index = match package_nodes.get(node_pkg[from_idx].as_str()) {
                    Some(&idx) => idx,
                    None => {
                        let idx = package_graph.add_node(node_pkg[from_idx].clone());
                        package_nodes.insert(node_pkg[from_idx].as_str(), idx);
                        idx
                    }
                };
                for &to_idx in neighbors {
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
        let Some(&start) = self.node_index.get(node) else {
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
            if !seen.insert(current) {
                continue;
            }
            if capture_depths {
                depths.insert(current, depth);
            }
            if seen.len() > limits.nodes {
                truncated = true;
                reason = Some("nodes".into());
                break;
            }
            if current != start {
                item_ids.push(current);
            }
            if let Some(neighbors) = graph.get(current as usize) {
                for &next in neighbors {
                    visited_edges += 1;
                    if visited_edges > limits.edges {
                        truncated = true;
                        reason = Some("edges".into());
                        break;
                    }
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

fn package_name(path: &str) -> String {
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
        assert_eq!(graph.package_cycles().len(), 1);
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
