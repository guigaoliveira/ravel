use crate::{
    incremental_graph::{IncrementalGraphOverlay, OwnedEdge},
    model::{Edge, EdgeConfidence, EdgeKind, EdgeProvenance, IndexSnapshot, Span},
};
use petgraph::{
    algo::{kosaraju_scc, toposort},
    graph::{DiGraph, NodeIndex},
};
use rustc_hash::{FxHashMap, FxHashSet};
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
    /// Offset into the deterministic (sorted) item list where this page
    /// starts. Feed a page's `next_cursor` back here to resume enumeration.
    #[serde(default)]
    pub cursor: usize,
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
            cursor: 0,
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

    #[cfg(test)]
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

    /// The archived form of `graph`, equal to `from_compact(graph.to_compact())` without expanding
    /// every adjacency table into per-node rows and flattening it straight back.
    pub(crate) fn from_index(graph: &GraphIndex) -> Self {
        let (forward_offsets, forward_values) = graph.forward.to_flat();
        let (reverse_offsets, reverse_values) = graph.reverse.to_flat();
        let (forward_relation_offsets, forward_relation_values) =
            graph.forward_relation_ids.to_flat();
        let (reverse_relation_offsets, reverse_relation_values) =
            graph.reverse_relation_ids.to_flat();
        Self {
            snapshot_id: graph.snapshot_id.clone(),
            nodes: graph.nodes.iter().map(ToString::to_string).collect(),
            forward_offsets,
            forward_values,
            reverse_offsets,
            reverse_values,
            edge_count: graph.edge_count as u32,
            relations: graph.relations.clone(),
            forward_relation_offsets,
            forward_relation_values,
            reverse_relation_offsets,
            reverse_relation_values,
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

/// A reference site that points into the index instead of owning its strings. Walking a symbol's
/// sites to render one page, or to fold all of them into counts, then costs no allocation per site;
/// [`RelationRef::to_view`] copies the few that are kept.
#[derive(Debug, Clone, Copy)]
pub struct RelationRef<'a> {
    pub node: &'a str,
    pub kind: &'a EdgeKind,
    pub source_path: Option<&'a str>,
    pub span: Option<Span>,
    pub confidence: &'static str,
    pub type_only: bool,
    pub provenance: &'a EdgeProvenance,
}

impl RelationRef<'_> {
    pub fn to_view(&self) -> RelationView {
        RelationView {
            node: self.node.to_owned(),
            kind: self.kind.clone(),
            source_path: self.source_path.map(str::to_owned),
            span: self.span,
            confidence: self.confidence,
            type_only: self.type_only,
            provenance: self.provenance.clone(),
        }
    }
}

/// Borrowed, serialize-only mirror of [`CompactGraph`] (identical field order/types → same
/// bincode/serde wire bytes) that avoids cloning the graph vectors when publishing.
#[derive(Debug, serde::Serialize)]
pub struct CompactGraphRef<'a> {
    pub snapshot_id: &'a str,
    pub nodes: Vec<&'a str>,
    pub forward: &'a Adjacency,
    pub reverse: &'a Adjacency,
    pub edge_count: u32,
    pub relations: &'a [CompactRelation],
    pub forward_relation_ids: &'a Adjacency,
    pub reverse_relation_ids: &'a Adjacency,
}

/// Per-node `u32` lists (neighbors or relation ids) in one flat allocation:
/// list `id` is `values[offsets[id]..offsets[id + 1]]`.
///
/// The index used to hold `Vec<Vec<u32>>` per table, and a cold load expanded the archived flat
/// arrays back into one heap allocation per node per table -- four tables, half a million nodes on
/// a 20k-file workspace -- before answering a single query. Lists rewritten by incremental overlays,
/// and nodes interned after the base was built, live in `patched`; reads check it only when it is
/// not empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adjacency {
    offsets: Vec<u32>,
    values: Vec<u32>,
    patched: FxHashMap<u32, Vec<u32>>,
    len: usize,
}

impl Default for Adjacency {
    fn default() -> Self {
        Self::from_flat(vec![0], Vec::new())
    }
}

impl Adjacency {
    fn from_flat(offsets: Vec<u32>, values: Vec<u32>) -> Self {
        debug_assert!(!offsets.is_empty());
        let len = offsets.len().saturating_sub(1);
        Self {
            offsets,
            values,
            patched: FxHashMap::default(),
            len,
        }
    }

    fn from_rows(rows: Vec<Vec<u32>>) -> Self {
        let (offsets, values) = FlatCompactGraph::flatten(rows);
        Self::from_flat(offsets, values)
    }

    fn get(&self, id: usize) -> Option<&[u32]> {
        if id >= self.len {
            return None;
        }
        if !self.patched.is_empty()
            && let Some(list) = self.patched.get(&(id as u32))
        {
            return Some(list);
        }
        if id + 1 < self.offsets.len() {
            Some(&self.values[self.offsets[id] as usize..self.offsets[id + 1] as usize])
        } else {
            // Interned after the base was built and never given a list.
            Some(&[])
        }
    }

    fn list(&self, id: usize) -> &[u32] {
        self.get(id).unwrap_or(&[])
    }

    fn push_empty(&mut self) {
        self.len += 1;
    }

    fn set(&mut self, id: usize, list: Vec<u32>) {
        debug_assert!(id < self.len);
        self.patched.insert(id as u32, list);
    }

    /// The list for `id`, copied out of the flat arrays on first write.
    fn get_mut(&mut self, id: usize) -> &mut Vec<u32> {
        debug_assert!(id < self.len);
        if !self.patched.contains_key(&(id as u32)) {
            let base = self.list(id).to_vec();
            self.patched.insert(id as u32, base);
        }
        self.patched.get_mut(&(id as u32)).expect("inserted above")
    }

    fn iter(&self) -> impl Iterator<Item = &[u32]> {
        (0..self.len).map(|id| self.list(id))
    }

    fn to_rows(&self) -> Vec<Vec<u32>> {
        self.iter().map(<[u32]>::to_vec).collect()
    }

    /// Flat arrays for the archived graph, copied as-is when nothing was patched.
    fn to_flat(&self) -> (Vec<u32>, Vec<u32>) {
        if self.patched.is_empty() && self.offsets.len() == self.len + 1 {
            return (self.offsets.clone(), self.values.clone());
        }
        let mut offsets = Vec::with_capacity(self.len + 1);
        let mut values = Vec::new();
        offsets.push(0);
        for list in self.iter() {
            values.extend_from_slice(list);
            offsets.push(values.len() as u32);
        }
        (offsets, values)
    }
}

/// Serialized exactly as the `Vec<Vec<u32>>` it replaces, so bincode sidecars keep their bytes.
impl serde::Serialize for Adjacency {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.len))?;
        for list in self.iter() {
            seq.serialize_element(list)?;
        }
        seq.end()
    }
}

/// Every node name, back to back in one buffer: name `n` is `bytes[ends[n - 1]..ends[n]]`.
///
/// The names used to be one `Arc<str>` allocation apiece. A cold load of a 20k-file workspace made
/// several hundred thousand of them (about 90 instructions and a 16-byte header plus allocator
/// rounding each, and as many frees whenever the daemon reloads a generation) to answer a query that
/// reads a handful. Ids are the positions, exactly as before.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct NodeNames {
    bytes: String,
    ends: Vec<u32>,
}

impl NodeNames {
    fn with_capacity(count: usize, bytes: usize) -> Self {
        Self {
            bytes: String::with_capacity(bytes),
            ends: Vec::with_capacity(count),
        }
    }

    fn len(&self) -> usize {
        self.ends.len()
    }

    fn push(&mut self, name: &str) {
        self.bytes.push_str(name);
        self.ends
            .push(u32::try_from(self.bytes.len()).expect("node names fit in 4 GiB"));
    }

    fn get(&self, id: usize) -> Option<&str> {
        let end = *self.ends.get(id)? as usize;
        let start = id
            .checked_sub(1)
            .map_or(0, |previous| self.ends[previous] as usize);
        self.bytes.get(start..end)
    }

    fn iter(&self) -> impl Iterator<Item = &str> {
        let mut start = 0;
        self.ends.iter().map(move |&end| {
            let name = &self.bytes[start..end as usize];
            start = end as usize;
            name
        })
    }
}

impl std::ops::Index<usize> for NodeNames {
    type Output = str;

    fn index(&self, id: usize) -> &str {
        self.get(id).expect("node id is in range")
    }
}

/// Node name → compact id, as open addressing over `(hash tag, id + 1)` pairs.
///
/// This was an `FxHashMap<Arc<str>, u32>`: a second fat pointer and an atomic refcount bump per
/// node, and 24 bytes a slot under hashbrown's 7/8 load limit -- 13MB and about a hundred
/// instructions per insert for the 380k nodes of a 20k-file workspace, on every cold load. The names
/// already live in `nodes`, so a slot only needs the id; the upper half of the 64-bit hash rides
/// along as a tag so a name is dereferenced only when it very probably matches, and the lower half
/// picks the slot. Slots stay under two-thirds full.
#[derive(Debug, Default)]
struct NameIndex {
    slots: Vec<u64>,
    /// Distinct names indexed.
    len: usize,
}

impl NameIndex {
    fn with_capacity(nodes: usize) -> Self {
        Self {
            slots: vec![0; (nodes.max(8) * 3).div_ceil(2)],
            len: 0,
        }
    }

    fn hash(name: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = rustc_hash::FxHasher::default();
        name.hash(&mut hasher);
        hasher.finish()
    }

    /// Where probing for `hash` begins: the low half of the hash scaled onto the table.
    fn start(&self, hash: u64) -> usize {
        ((u64::from(hash as u32) * self.slots.len() as u64) >> 32) as usize
    }

    fn next(&self, slot: usize) -> usize {
        if slot + 1 == self.slots.len() {
            0
        } else {
            slot + 1
        }
    }

    fn entry(hash: u64, id: u32) -> u64 {
        (hash >> 32) << 32 | (u64::from(id) + 1)
    }

    fn get(&self, nodes: &NodeNames, name: &str) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let hash = Self::hash(name);
        let mut slot = self.start(hash);
        loop {
            let entry = self.slots[slot];
            if entry == 0 {
                return None;
            }
            let id = entry as u32 - 1;
            if entry >> 32 == hash >> 32 && nodes[id as usize] == *name {
                return Some(id);
            }
            slot = self.next(slot);
        }
    }

    /// Index `nodes[id]`. A name that is already indexed moves to the new id, as `HashMap::insert`
    /// did.
    fn insert(&mut self, nodes: &NodeNames, id: u32) {
        if (self.len + 1) * 3 > self.slots.len() * 2 {
            self.grow(nodes);
        }
        let name = &nodes[id as usize];
        let hash = Self::hash(name);
        let mut slot = self.start(hash);
        loop {
            let entry = self.slots[slot];
            if entry == 0 {
                self.slots[slot] = Self::entry(hash, id);
                self.len += 1;
                return;
            }
            if entry >> 32 == hash >> 32 && nodes[entry as u32 as usize - 1] == *name {
                self.slots[slot] = Self::entry(hash, id);
                return;
            }
            slot = self.next(slot);
        }
    }

    fn grow(&mut self, nodes: &NodeNames) {
        let old = std::mem::take(&mut self.slots);
        self.slots = vec![0; old.len() * 3 / 2 + 16];
        for entry in old.into_iter().filter(|entry| *entry != 0) {
            let id = entry as u32 - 1;
            let hash = Self::hash(&nodes[id as usize]);
            let mut slot = self.start(hash);
            while self.slots[slot] != 0 {
                slot = self.next(slot);
            }
            self.slots[slot] = Self::entry(hash, id);
        }
    }
}

#[derive(Debug)]
pub struct GraphIndex {
    nodes: NodeNames,
    /// Maps node name → index into `nodes` / adjacency vectors.
    node_index: NameIndex,
    forward: Adjacency,
    reverse: Adjacency,
    edge_count: usize,
    relations: Vec<CompactRelation>,
    forward_relation_ids: Adjacency,
    reverse_relation_ids: Adjacency,
    relation_file_overlays: BTreeMap<String, Option<BTreeSet<Arc<OwnedEdge>>>>,
    /// Node ids of the files in `relation_file_overlays`, so deciding whether a base relation was
    /// superseded is an integer lookup instead of a string comparison per relation.
    overlaid_paths: FxHashSet<u32>,
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
        let mut node_index = NameIndex::with_capacity(cap);
        let mut nodes = NodeNames::with_capacity(cap, 0);
        let mut forward: Vec<Vec<u32>> = Vec::with_capacity(cap);
        let mut reverse: Vec<Vec<u32>> = Vec::with_capacity(cap);
        let mut forward_relation_ids: Vec<Vec<u32>> = Vec::with_capacity(cap);
        let mut reverse_relation_ids: Vec<Vec<u32>> = Vec::with_capacity(cap);

        let intern = |name: &str,
                      nodes: &mut NodeNames,
                      node_index: &mut NameIndex,
                      forward: &mut Vec<Vec<u32>>,
                      reverse: &mut Vec<Vec<u32>>,
                      forward_relation_ids: &mut Vec<Vec<u32>>,
                      reverse_relation_ids: &mut Vec<Vec<u32>>|
         -> u32 {
            if let Some(id) = node_index.get(nodes, name) {
                return id;
            }
            let id = nodes.len() as u32;
            nodes.push(name);
            node_index.insert(nodes, id);
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
            forward: Adjacency::from_rows(forward),
            reverse: Adjacency::from_rows(reverse),
            edge_count,
            relations,
            forward_relation_ids: Adjacency::from_rows(forward_relation_ids),
            reverse_relation_ids: Adjacency::from_rows(reverse_relation_ids),
            relation_file_overlays: BTreeMap::new(),
            overlaid_paths: FxHashSet::default(),
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
        let mut nodes = NodeNames::with_capacity(
            node_count,
            compact.nodes.iter().map(String::len).sum::<usize>(),
        );
        for name in &compact.nodes {
            nodes.push(name);
        }
        let mut node_index = NameIndex::with_capacity(node_count);
        for id in 0..nodes.len() {
            node_index.insert(&nodes, id as u32);
        }
        let edge_count = compact.edge_count as usize;
        Self {
            nodes,
            node_index,
            forward: Adjacency::from_rows(compact.forward),
            reverse: Adjacency::from_rows(compact.reverse),
            edge_count,
            relations: compact.relations,
            forward_relation_ids: Adjacency::from_rows(compact.forward_relation_ids),
            reverse_relation_ids: Adjacency::from_rows(compact.reverse_relation_ids),
            relation_file_overlays: BTreeMap::new(),
            overlaid_paths: FxHashSet::default(),
            relation_overlay_nodes: BTreeSet::new(),
            overlay_forward_relations: BTreeMap::new(),
            overlay_reverse_relations: BTreeMap::new(),
            inactive_nodes: BTreeSet::new(),
            snapshot_id: compact.snapshot_id,
            package_graph: OnceLock::new(),
        }
    }

    /// Build straight from the archived flat form, skipping two owned copies.
    ///
    /// The previous cold load ran four passes over the record: validate it,
    /// deserialize 90MB into an owned `FlatCompactGraph`, expand that into a
    /// `CompactGraph` with one `Vec` per node, then move those into the index. Only
    /// the last shape is used. Reading the archived offsets and values directly
    /// keeps the expansion — the index traverses `Vec<Vec<u32>>` — and drops the two
    /// intermediate copies.
    pub(crate) fn from_archived_flat(archived: &ArchivedFlatCompactGraph) -> Self {
        fn expand(offsets: &[rkyv::rend::u32_le], values: &[rkyv::rend::u32_le]) -> Adjacency {
            let native = |values: &[rkyv::rend::u32_le]| -> Vec<u32> {
                values.iter().map(|value| value.to_native()).collect()
            };
            if offsets.is_empty() {
                return Adjacency::default();
            }
            Adjacency::from_flat(native(offsets), native(values))
        }
        let node_count = archived.nodes.len();
        let mut nodes = NodeNames::with_capacity(
            node_count,
            archived.nodes.iter().map(|name| name.as_str().len()).sum(),
        );
        for name in archived.nodes.iter() {
            nodes.push(name.as_str());
        }
        let mut node_index = NameIndex::with_capacity(node_count);
        for id in 0..nodes.len() {
            node_index.insert(&nodes, id as u32);
        }
        Self {
            nodes,
            node_index,
            forward: expand(&archived.forward_offsets, &archived.forward_values),
            reverse: expand(&archived.reverse_offsets, &archived.reverse_values),
            edge_count: archived.edge_count.to_native() as usize,
            relations: archived
                .relations
                .iter()
                .map(|relation| {
                    rkyv::deserialize::<CompactRelation, rkyv::rancor::Error>(relation)
                        .expect("archived relation is validated")
                })
                .collect(),
            forward_relation_ids: expand(
                &archived.forward_relation_offsets,
                &archived.forward_relation_values,
            ),
            reverse_relation_ids: expand(
                &archived.reverse_relation_offsets,
                &archived.reverse_relation_values,
            ),
            relation_file_overlays: BTreeMap::new(),
            overlaid_paths: FxHashSet::default(),
            relation_overlay_nodes: BTreeSet::new(),
            overlay_forward_relations: BTreeMap::new(),
            overlay_reverse_relations: BTreeMap::new(),
            inactive_nodes: BTreeSet::new(),
            snapshot_id: archived.snapshot_id.to_string(),
            package_graph: OnceLock::new(),
        }
    }

    pub fn to_compact(&self) -> CompactGraph {
        CompactGraph {
            snapshot_id: self.snapshot_id.clone(),
            nodes: self.nodes.iter().map(ToString::to_string).collect(),
            forward: self.forward.to_rows(),
            reverse: self.reverse.to_rows(),
            edge_count: self.edge_count as u32,
            relations: self.relations.clone(),
            forward_relation_ids: self.forward_relation_ids.to_rows(),
            reverse_relation_ids: self.reverse_relation_ids.to_rows(),
        }
    }

    /// Borrowed view for serialization — same wire layout as [`CompactGraph`] but without
    /// cloning the node/adjacency vectors (used by the publish path).
    pub fn as_compact_ref(&self) -> CompactGraphRef<'_> {
        CompactGraphRef {
            snapshot_id: &self.snapshot_id,
            nodes: self.nodes.iter().collect(),
            forward: &self.forward,
            reverse: &self.reverse,
            edge_count: self.edge_count as u32,
            relations: &self.relations,
            forward_relation_ids: &self.forward_relation_ids,
            reverse_relation_ids: &self.reverse_relation_ids,
        }
    }

    /// Reference sites of `node` in display order, and how many there are.
    ///
    /// Nothing is copied -- each [`RelationRef`] points into the node table -- so rendering one page
    /// (`skip(cursor).take(limit)`) or folding every site into counts allocates nothing per site, and
    /// reaching a late page does not rebuild the earlier ones. A node an incremental overlay touched
    /// has its surviving persisted sites merged with the overlay's, in the order that sorting both
    /// together would give.
    pub fn direct_relations(&self, node: &str, reverse: bool) -> (RelationSites<'_>, usize) {
        let Some(node_id) = self.node_index.get(&self.nodes, node) else {
            return (RelationSites::new(self, reverse, &[], None), 0);
        };
        let relation_ids = self.relation_id_list(node_id, reverse);
        if !self.relation_overlay_nodes.contains(node) {
            let sites = RelationSites::new(self, reverse, relation_ids, None);
            return (sites, relation_ids.len());
        }
        let added = self.overlay_edges(node, reverse);
        let surviving = relation_ids
            .iter()
            .filter_map(|relation_id| self.relations.get(*relation_id as usize))
            .filter(|relation| self.survives_overlays(relation))
            .count();
        let sites = RelationSites::new(self, reverse, relation_ids, Some(added));
        (sites, surviving + added.len())
    }

    /// Return at most `limit` detailed sites while reporting the complete relation count.
    /// This keeps high-degree agent queries O(limit) in allocations instead of O(degree).
    pub fn direct_relations_limit(
        &self,
        node: &str,
        reverse: bool,
        limit: usize,
    ) -> (Vec<RelationView>, usize) {
        let (sites, total) = self.direct_relations(node, reverse);
        (
            sites.take(limit).map(|site| site.to_view()).collect(),
            total,
        )
    }

    /// Incoming plus outgoing relation count of one node: the sum of the totals
    /// `direct_relations_limit` reports, without building the relations.
    ///
    /// Asking `direct_relations_limit` for a limit of zero still turns every site of a page into
    /// an owned view; `direct_relations` counts a node's sites -- an overlaid one's included --
    /// without building any of them.
    pub fn direct_degree(&self, node: &str) -> usize {
        self.direct_relations(node, true).1 + self.direct_relations(node, false).1
    }

    /// Complete per-kind edge counts for one node. Bounded by the number of
    /// edge kinds, so it stays cheap even for hubs where the relation page
    /// itself must truncate.
    pub fn direct_relation_kind_counts(
        &self,
        node: &str,
        reverse: bool,
    ) -> BTreeMap<&'static str, usize> {
        // One slot per kind: a hub has thousands of relations and a map entry per relation was most
        // of the cost of answering a page of it.
        let mut counts = [0usize; EDGE_KINDS.len()];
        if let Some(node_id) = self.node_index.get(&self.nodes, node) {
            let overlaid = self.relation_overlay_nodes.contains(node);
            for relation in self
                .relation_id_list(node_id, reverse)
                .iter()
                .filter_map(|relation_id| self.relations.get(*relation_id as usize))
            {
                if overlaid && !self.survives_overlays(relation) {
                    continue;
                }
                counts[edge_kind_slot(&relation.kind)] += 1;
            }
            if overlaid {
                for edge in self.overlay_edges(node, reverse) {
                    counts[edge_kind_slot(&edge.kind)] += 1;
                }
            }
        }
        EDGE_KINDS
            .iter()
            .zip(counts)
            .filter(|(_, count)| *count > 0)
            .map(|(kind, count)| (kind.as_str(), count))
            .collect()
    }

    fn relation_id_list(&self, node_id: u32, reverse: bool) -> &[u32] {
        if reverse {
            self.reverse_relation_ids.list(node_id as usize)
        } else {
            self.forward_relation_ids.list(node_id as usize)
        }
    }

    /// Sites an overlay added for `node`: the edges of files it rewrote.
    fn overlay_edges(&self, node: &str, reverse: bool) -> &[Arc<OwnedEdge>] {
        let by_node = if reverse {
            &self.overlay_reverse_relations
        } else {
            &self.overlay_forward_relations
        };
        by_node.get(node).map_or(&[], Vec::as_slice)
    }

    /// A persisted relation stands unless an overlay rewrote or removed the file it came from.
    fn survives_overlays(&self, relation: &CompactRelation) -> bool {
        relation
            .source_path
            .is_none_or(|path| !self.overlaid_paths.contains(&path))
    }

    /// The next persisted site of `ids`, skipping ids that name no relation and -- once an overlay
    /// touched the node -- relations the overlay superseded.
    fn next_persisted_site<'a>(
        &'a self,
        ids: &mut std::slice::Iter<'a, u32>,
        reverse: bool,
        drop_superseded: bool,
    ) -> Option<RelationRef<'a>> {
        for &relation_id in ids.by_ref() {
            let Some(relation) = self.relations.get(relation_id as usize) else {
                continue;
            };
            if drop_superseded && !self.survives_overlays(relation) {
                continue;
            }
            return Some(self.relation_ref(relation, reverse));
        }
        None
    }

    fn relation_ref<'a>(&'a self, relation: &'a CompactRelation, reverse: bool) -> RelationRef<'a> {
        let related = if reverse { relation.from } else { relation.to };
        RelationRef {
            node: &self.nodes[related as usize],
            kind: &relation.kind,
            source_path: relation
                .source_path
                .and_then(|id| self.nodes.get(id as usize)),
            span: relation.span,
            confidence: match relation.confidence {
                0 => "resolved",
                1 => "candidate",
                _ => "unresolved",
            },
            type_only: relation.type_only,
            provenance: &relation.provenance,
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
                let list = neighbors
                    .iter()
                    .flat_map(|neighbors| neighbors.keys())
                    .map(|neighbor| self.intern_node(neighbor))
                    .collect();
                self.forward.set(id as usize, list);
            }
            if let Some(neighbors) = overlay.reverse_refcounts.get(node) {
                let list = neighbors
                    .iter()
                    .flat_map(|neighbors| neighbors.keys())
                    .map(|neighbor| self.intern_node(neighbor))
                    .collect();
                self.reverse.set(id as usize, list);
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
                    self.forward.get_mut(id as usize)
                } else {
                    self.reverse.get_mut(id as usize)
                };
                // One pass per node instead of a scan per change. Scanning the
                // adjacency for every changed neighbor is O(changes × degree), and a
                // hub reached by an edit has both terms large. The two sets below are
                // bounded by data already held — the adjacency itself and the change
                // list — and are dropped before the next node.
                let (added, removed): (Vec<_>, Vec<_>) =
                    resolved.into_iter().partition(|(_, present)| *present);
                if !removed.is_empty() {
                    let removed: FxHashSet<u32> =
                        removed.into_iter().map(|(neighbor, _)| neighbor).collect();
                    list.retain(|existing| !removed.contains(existing));
                }
                if !added.is_empty() {
                    let mut present: FxHashSet<u32> = list.iter().copied().collect();
                    for (neighbor_id, _) in added {
                        if present.insert(neighbor_id) {
                            list.push(neighbor_id);
                        }
                    }
                }
            }
        }
        for id in touched_ids {
            if self.forward.list(id as usize).is_empty()
                && self.reverse.list(id as usize).is_empty()
            {
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
        self.overlaid_paths = self
            .relation_file_overlays
            .keys()
            .filter_map(|path| self.node_index.get(&self.nodes, path.as_str()))
            .collect();
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
        if let Some(id) = self.node_index.get(&self.nodes, name) {
            return id;
        }
        let id = self.nodes.len() as u32;
        self.nodes.push(name);
        self.node_index.insert(&self.nodes, id);
        self.forward.push_empty();
        self.reverse.push_empty();
        self.forward_relation_ids.push_empty();
        self.reverse_relation_ids.push_empty();
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

    /// SCCs of the file-collapsed graph. Finer than `package_cycles`: a
    /// monorepo whose top-level buckets all reach each other collapses into
    /// one giant package SCC, while the actionable cycles live between files.
    pub fn file_cycles(&self) -> Vec<Vec<String>> {
        let node_file: Vec<String> = self.nodes.iter().map(file_name).collect();
        let mut file_graph: DiGraph<String, ()> = DiGraph::new();
        let mut file_nodes: FxHashMap<&str, NodeIndex> = FxHashMap::default();
        let mut seen_edges: rustc_hash::FxHashSet<(NodeIndex, NodeIndex)> =
            rustc_hash::FxHashSet::default();

        for (from_idx, neighbors) in self.forward.iter().enumerate() {
            if self.inactive_nodes.contains(&(from_idx as u32)) {
                continue;
            }
            let from_index = match file_nodes.get(node_file[from_idx].as_str()) {
                Some(&idx) => idx,
                None => {
                    let idx = file_graph.add_node(node_file[from_idx].clone());
                    file_nodes.insert(node_file[from_idx].as_str(), idx);
                    idx
                }
            };
            for &to_idx in neighbors {
                if self.inactive_nodes.contains(&to_idx) {
                    continue;
                }
                let to_file = node_file[to_idx as usize].as_str();
                if to_file == node_file[from_idx] {
                    continue; // intra-file edges are not cycles between files
                }
                let to_index = match file_nodes.get(to_file) {
                    Some(&idx) => idx,
                    None => {
                        let idx = file_graph.add_node(node_file[to_idx as usize].clone());
                        file_nodes.insert(node_file[to_idx as usize].as_str(), idx);
                        idx
                    }
                };
                if seen_edges.insert((from_index, to_index)) {
                    file_graph.add_edge(from_index, to_index, ());
                }
            }
        }
        kosaraju_scc(&file_graph)
            .into_iter()
            .filter(|component| component.len() > 1)
            .map(|component| {
                component
                    .into_iter()
                    .map(|index| file_graph[index].clone())
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
            .get(&self.nodes, name)
            .is_some_and(|id| !self.inactive_nodes.contains(&id))
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
            (!self.inactive_nodes.contains(&id)).then_some((id, name))
        })
    }

    pub fn in_degree(&self, name: &str) -> usize {
        self.node_index
            .get(&self.nodes, name)
            .map(|i| self.reverse.list(i as usize).len())
            .unwrap_or(0)
    }

    pub fn out_degree(&self, name: &str) -> usize {
        self.node_index
            .get(&self.nodes, name)
            .map(|i| self.forward.list(i as usize).len())
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
            .get(&self.nodes, name)
            .filter(|id| !self.inactive_nodes.contains(id))
    }

    pub fn node_name(&self, id: u32) -> Option<&str> {
        (!self.inactive_nodes.contains(&id))
            .then(|| self.nodes.get(id as usize))
            .flatten()
    }

    /// In-degree lookup when the caller already has the compact node ID.
    pub fn in_degree_id(&self, id: u32) -> usize {
        self.reverse.list(id as usize).len()
    }

    /// Out-degree lookup when the caller already has the compact node ID.
    pub fn out_degree_id(&self, id: u32) -> usize {
        self.forward.list(id as usize).len()
    }

    /// Reverse adjacency lookup by node ID (zero-alloc — no hash lookup).
    pub fn neighbor_ids_reverse_id(&self, id: u32) -> &[u32] {
        self.reverse.list(id as usize)
    }

    fn neighbor_ids<'a>(&'a self, name: &str, adj: &'a Adjacency) -> &'a [u32] {
        let Some(idx) = self.node_index.get(&self.nodes, name) else {
            return &[];
        };
        adj.list(idx as usize)
    }

    fn neighbors(&self, name: &str, adj: &Adjacency) -> Vec<String> {
        self.neighbor_ids(name, adj)
            .iter()
            .map(|&i| self.nodes[i as usize].to_string())
            .collect()
    }

    fn package_graph(&self) -> &DiGraph<String, ()> {
        self.package_graph.get_or_init(|| {
            // Compute each node's package once (O(N)); the old code recomputed
            // `package_name` — a per-call allocation — for every edge endpoint (O(E)).
            let node_pkg: Vec<String> = self.nodes.iter().map(package_name).collect();
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
        graph: &Adjacency,
        limits: &QueryLimits,
        cancel: Option<&Arc<AtomicBool>>,
        capture_depths: bool,
    ) -> Result<(QueryPage, FxHashMap<u32, usize>), QueryError> {
        if node.is_empty() {
            return Err(QueryError::EmptyNode);
        }
        let deadline = Instant::now() + Duration::from_millis(limits.timeout_ms);

        // Unknown node: empty expansion, still a valid bounded page.
        let Some(start) = self
            .node_index
            .get(&self.nodes, node)
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
        let start = limits.cursor.min(item_ids.len());
        let end = start.saturating_add(limits.page_size).min(item_ids.len());
        let next_cursor = (end < item_ids.len()).then(|| end.to_string());
        let page: Vec<String> = item_ids[start..end]
            .iter()
            .map(|&id| self.nodes[id as usize].to_string())
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

/// The sites of one node in display order. See [`GraphIndex::direct_relations`].
pub struct RelationSites<'a> {
    graph: &'a GraphIndex,
    reverse: bool,
    /// Persisted relation ids not yet visited, already in display order.
    ids: std::slice::Iter<'a, u32>,
    /// Present only when an incremental overlay touched the node.
    overlay: Option<OverlayMerge<'a>>,
}

/// What it takes to put a touched node's sites in display order without sorting all of them: the
/// persisted ids are already ordered, so only the overlay's own sites are sorted and the two streams
/// are merged.
struct OverlayMerge<'a> {
    edges: &'a [Arc<OwnedEdge>],
    /// The overlay's sites in display order, built the first time one is needed.
    added: Option<std::iter::Peekable<std::vec::IntoIter<RelationRef<'a>>>>,
    /// Persisted sites sharing one position (kind, file, span) in display order, next one last.
    /// The persisted order breaks such ties by node id and the display order by node name, so each
    /// run is re-sorted by name.
    run: Vec<RelationRef<'a>>,
    /// First persisted site after `run`.
    ahead: Option<RelationRef<'a>>,
}

impl<'a> RelationSites<'a> {
    fn new(
        graph: &'a GraphIndex,
        reverse: bool,
        ids: &'a [u32],
        added: Option<&'a [Arc<OwnedEdge>]>,
    ) -> Self {
        Self {
            graph,
            reverse,
            ids: ids.iter(),
            overlay: added.map(|edges| OverlayMerge {
                edges,
                added: None,
                run: Vec::new(),
                ahead: None,
            }),
        }
    }
}

impl<'a> Iterator for RelationSites<'a> {
    type Item = RelationRef<'a>;

    fn next(&mut self) -> Option<RelationRef<'a>> {
        let (graph, reverse) = (self.graph, self.reverse);
        let Some(merge) = self.overlay.as_mut() else {
            return graph.next_persisted_site(&mut self.ids, reverse, false);
        };
        if merge.run.is_empty() {
            let first = merge
                .ahead
                .take()
                .or_else(|| graph.next_persisted_site(&mut self.ids, reverse, true));
            if let Some(first) = first {
                merge.run.push(first);
                while let Some(next) = graph.next_persisted_site(&mut self.ids, reverse, true) {
                    if !same_position(&first, &next) {
                        merge.ahead = Some(next);
                        break;
                    }
                    merge.run.push(next);
                }
                if merge.run.len() > 1 {
                    merge.run.sort_by(|left, right| left.node.cmp(right.node));
                    merge.run.reverse();
                }
            }
        }
        let added = merge.added.get_or_insert_with(|| {
            let mut sites: Vec<_> = merge
                .edges
                .iter()
                .map(|edge| RelationRef {
                    node: if reverse { &edge.from } else { &edge.to },
                    kind: &edge.kind,
                    source_path: edge.source_path.as_deref(),
                    span: edge.span,
                    confidence: match edge.confidence_kind {
                        0 => "resolved",
                        1 => "candidate",
                        _ => "unresolved",
                    },
                    type_only: edge.type_only,
                    provenance: &edge.provenance,
                })
                .collect();
            sites.sort_by(site_order);
            sites.into_iter().peekable()
        });
        match (merge.run.last(), added.peek()) {
            (Some(persisted), Some(added_site)) => {
                if site_order(added_site, persisted).is_lt() {
                    added.next()
                } else {
                    merge.run.pop()
                }
            }
            (Some(_), None) => merge.run.pop(),
            (None, Some(_)) => added.next(),
            (None, None) => None,
        }
    }

    /// Skipping to a late page must not cost a visit per skipped site: persisted ids of an untouched
    /// node are already the display order, so they can be stepped over directly.
    fn nth(&mut self, n: usize) -> Option<RelationRef<'a>> {
        if self.overlay.is_none() && n > 0 {
            // A relation id always names a relation, so each id skipped is one site skipped.
            self.ids.nth(n - 1)?;
            return self.next();
        }
        for _ in 0..n {
            self.next()?;
        }
        self.next()
    }
}

/// Display order of two sites: semantic references before module plumbing, then file, position, and
/// finally the name of the node at the other end.
fn site_order(left: &RelationRef<'_>, right: &RelationRef<'_>) -> std::cmp::Ordering {
    relation_display_priority(left.kind)
        .cmp(&relation_display_priority(right.kind))
        .then_with(|| {
            left.source_path
                .unwrap_or("")
                .cmp(right.source_path.unwrap_or(""))
        })
        .then_with(|| left.span.cmp(&right.span))
        .then_with(|| left.node.cmp(right.node))
}

/// Whether two sites sit at one position, i.e. differ in `site_order` only by node name.
fn same_position(left: &RelationRef<'_>, right: &RelationRef<'_>) -> bool {
    relation_display_priority(left.kind) == relation_display_priority(right.kind)
        && left.source_path.unwrap_or("") == right.source_path.unwrap_or("")
        && left.span == right.span
}

/// Every edge kind once, so a per-kind tally is an array indexed by [`edge_kind_slot`].
const EDGE_KINDS: [EdgeKind; 9] = [
    EdgeKind::Import,
    EdgeKind::ReExport,
    EdgeKind::Calls,
    EdgeKind::Extends,
    EdgeKind::Implements,
    EdgeKind::Instantiates,
    EdgeKind::References,
    EdgeKind::TypeOf,
    EdgeKind::Decorates,
];

fn edge_kind_slot(kind: &EdgeKind) -> usize {
    match kind {
        EdgeKind::Import => 0,
        EdgeKind::ReExport => 1,
        EdgeKind::Calls => 2,
        EdgeKind::Extends => 3,
        EdgeKind::Implements => 4,
        EdgeKind::Instantiates => 5,
        EdgeKind::References => 6,
        EdgeKind::TypeOf => 7,
        EdgeKind::Decorates => 8,
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

/// File path a graph node lives in: the path component of a `symbol://` id,
/// or the node name itself for module/path nodes.
fn file_name(node: &str) -> String {
    node.strip_prefix("symbol://")
        .and_then(|value| value.split_once('#').map(|(path, _)| path))
        .unwrap_or(node)
        .to_owned()
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

    #[test]
    fn flat_adjacency_reads_writes_and_serializes_like_nested_lists() {
        let rows = vec![vec![2, 1], vec![], vec![0, 0, 3], vec![]];
        let mut adjacency = Adjacency::from_rows(rows.clone());
        assert_eq!(adjacency.to_rows(), rows);
        assert_eq!(
            bincode::serialize(&adjacency).unwrap(),
            bincode::serialize(&rows).unwrap(),
            "sidecars written from the flat form keep the nested wire layout"
        );
        assert_eq!(adjacency.get(4), None);
        assert_eq!(adjacency.list(9), &[] as &[u32]);

        // Overlay edits: replace one list, extend another in place, intern two new nodes.
        adjacency.set(0, vec![3]);
        adjacency.get_mut(2).push(1);
        adjacency.push_empty();
        adjacency.push_empty();
        adjacency.get_mut(5).push(0);
        let expected = vec![vec![3], vec![], vec![0, 0, 3, 1], vec![], vec![], vec![0]];
        assert_eq!(adjacency.to_rows(), expected);
        assert_eq!(
            bincode::serialize(&adjacency).unwrap(),
            bincode::serialize(&expected).unwrap()
        );
        assert_eq!(adjacency.to_flat(), FlatCompactGraph::flatten(expected));
        assert_eq!(Adjacency::default().to_rows(), Vec::<Vec<u32>>::new());
    }

    #[test]
    fn node_names_in_one_buffer_read_back_as_the_separate_strings_they_replaced() {
        let originals = [
            "",
            "a.ts",
            "symbol://p/src/é.ts#class:Ünï",
            "",
            "日本語/節",
            "z",
        ];
        let mut names = NodeNames::with_capacity(originals.len(), 0);
        for original in originals {
            names.push(original);
        }
        assert_eq!(names.len(), originals.len());
        for (id, original) in originals.iter().enumerate() {
            assert_eq!(names.get(id), Some(*original));
            assert_eq!(&names[id], *original);
        }
        assert_eq!(names.get(originals.len()), None);
        assert_eq!(names.iter().collect::<Vec<_>>(), originals);
        assert_eq!(NodeNames::default().iter().count(), 0);
        assert_eq!(names.clone(), names);
    }

    /// The node index replaced a hash map and has to answer exactly like one: every name found at
    /// the id it was given, absent names absent, a repeated name moving to its newest id, and the
    /// same answers across growth from a table that starts far too small. Names that are prefixes
    /// of each other and ones differing in one byte are the shape a weak tag check would confuse.
    #[test]
    fn node_index_answers_like_the_hash_map_it_replaced() {
        let mut names = NodeNames::default();
        let mut expected: std::collections::HashMap<String, u32> = Default::default();
        let mut index = NameIndex::with_capacity(1);
        assert_eq!(NameIndex::default().get(&names, "anything"), None);
        for n in 0..5_000u32 {
            let name = match n % 4 {
                0 => format!(
                    "symbol://packages/p{}/src/f{}.ts#class:Svc{n}",
                    n % 7,
                    n % 13
                ),
                1 => format!("packages/p{}/src/f{n}", n % 7),
                2 => format!("{n}"),
                _ => format!("packages/p{}/src/f{}", n % 7, n - 1),
            };
            if let Some(&known) = expected.get(&name) {
                assert_eq!(index.get(&names, &name), Some(known));
                continue;
            }
            let id = names.len() as u32;
            names.push(&name);
            index.insert(&names, id);
            expected.insert(name, id);
        }
        for (name, id) in &expected {
            assert_eq!(index.get(&names, name), Some(*id), "{name}");
        }
        for probe in ["", "symbol://", "packages/p0/src/f0.t", "9999999", "Svc1"] {
            assert_eq!(index.get(&names, probe), None, "{probe}");
        }

        // `HashMap::insert` kept the newest id for a name inserted twice.
        let repeated = names[17].to_string();
        names.push(&repeated);
        index.insert(&names, names.len() as u32 - 1);
        assert_eq!(index.get(&names, &repeated), Some(names.len() as u32 - 1));
        assert_eq!(index.len, expected.len());
    }

    /// The cold load builds the index straight from the archived record. That path
    /// replaced a deserialize-then-expand chain, and a subtle mistake in it would not
    /// fail loudly — every query would simply answer from a wrong adjacency. So pin it
    /// against the shape it replaced: same edges in, identical index out.
    #[test]
    fn archived_cold_load_matches_building_from_the_owned_compact_form() {
        let edges = vec![
            edge("a.ts", "b.ts"),
            edge("a.ts", "c.ts"),
            edge("b.ts", "c.ts"),
            edge("c.ts", "a.ts"),
            edge("symbol://a.ts#value:A", "symbol://b.ts#value:B"),
        ];
        let built = GraphIndex::from_edges(&edges, "snap".into());
        let compact = built.to_compact();
        let flat = FlatCompactGraph::from_compact(compact.clone());
        assert_eq!(FlatCompactGraph::from_index(&built), flat);
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&flat).unwrap();
        let archived =
            rkyv::access::<ArchivedFlatCompactGraph, rkyv::rancor::Error>(&bytes).unwrap();

        let from_archive = GraphIndex::from_archived_flat(archived);
        let from_owned = GraphIndex::from_compact(compact);

        assert_eq!(from_archive.snapshot_id(), from_owned.snapshot_id());
        assert_eq!(from_archive.edge_count(), from_owned.edge_count());
        assert_eq!(from_archive.nodes, from_owned.nodes);
        assert_eq!(from_archive.forward, from_owned.forward);
        assert_eq!(from_archive.reverse, from_owned.reverse);
        assert_eq!(from_archive.relations, from_owned.relations);
        assert_eq!(
            from_archive.forward_relation_ids,
            from_owned.forward_relation_ids
        );
        assert_eq!(
            from_archive.reverse_relation_ids,
            from_owned.reverse_relation_ids
        );

        // And it answers queries the same way, which is what actually matters.
        let limits = QueryLimits::default();
        for node in ["a.ts", "b.ts", "c.ts"] {
            for reverse in [true, false] {
                let left = if reverse {
                    from_archive.callers_of(node, &limits, None).unwrap()
                } else {
                    from_archive.impact_analysis(node, &limits, None).unwrap()
                };
                let right = if reverse {
                    from_owned.callers_of(node, &limits, None).unwrap()
                } else {
                    from_owned.impact_analysis(node, &limits, None).unwrap()
                };
                assert_eq!(left.items, right.items, "node={node} reverse={reverse}");
            }
        }
    }

    /// Applying an overlay used to scan the whole adjacency once per changed
    /// neighbour; it now partitions and makes a single pass. Adds and removes landing
    /// in the same batch are where a partitioned rewrite goes wrong, so cover that
    /// directly: an existing neighbour removed, a new one added, one re-added that is
    /// already present, and one removed that was never there.
    #[test]
    fn overlay_applies_adds_and_removes_in_one_batch() {
        let edges = vec![
            edge("hub.ts", "keep.ts"),
            edge("hub.ts", "drop.ts"),
            edge("hub.ts", "already.ts"),
        ];
        let mut graph = GraphIndex::from_edges(&edges, "before".into());
        // Observe through the BFS: `forward_changes` rewrites the u32 adjacency the
        // traversal walks, which is a different structure from the relation views.
        let reachable = |graph: &GraphIndex| -> Vec<String> {
            graph
                .impact_analysis(
                    "hub.ts",
                    &QueryLimits {
                        depth: 1,
                        page_size: 100,
                        ..Default::default()
                    },
                    None,
                )
                .unwrap()
                .items
        };
        let before: BTreeSet<String> = reachable(&graph).into_iter().collect();
        assert_eq!(
            before,
            ["already.ts", "drop.ts", "keep.ts"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );

        let mut overlay = IncrementalGraphOverlay::default();
        overlay.forward_changes.insert(
            "hub.ts".into(),
            [
                ("drop.ts".to_owned(), None),       // present -> removed
                ("fresh.ts".to_owned(), Some(1)),   // absent  -> added
                ("already.ts".to_owned(), Some(1)), // present -> stays, not duplicated
                ("never.ts".to_owned(), None),      // absent  -> no-op
            ]
            .into_iter()
            .collect(),
        );
        graph.apply_incremental_overlay(&overlay, "after", 3);

        let after: Vec<String> = reachable(&graph);
        let unique: BTreeSet<&String> = after.iter().collect();
        assert_eq!(
            unique.len(),
            after.len(),
            "a neighbour already present must not be duplicated: {after:?}"
        );
        let after: BTreeSet<String> = after.into_iter().collect();
        assert!(!after.contains("drop.ts"), "removed neighbour survived");
        assert!(!after.contains("never.ts"), "invented a neighbour");
        assert!(after.contains("keep.ts"), "untouched neighbour lost");
        assert!(after.contains("already.ts"), "re-added neighbour lost");
    }

    /// `direct_degree` stands in for two `direct_relations_limit(.., 0)` calls, which for a node
    /// an overlay touched built every relation just to count it. The count has to agree with them
    /// for plain nodes, for nodes whose file was rewritten, and for nodes whose file was removed.
    #[test]
    fn direct_degree_matches_the_relation_totals_with_and_without_overlays() {
        let from_file = |from: &str, to: &str, file: &str| {
            let mut edge = edge(from, to);
            edge.source_path = Some(file.into());
            edge
        };
        let originals = [
            from_file("a", "hub", "x.ts"),
            from_file("b", "hub", "y.ts"),
            from_file("hub", "c", "x.ts"),
            from_file("c", "a", "z.ts"),
        ];
        let mut graph = GraphIndex::from_edges(&originals, "before".into());
        let totals = |graph: &GraphIndex, node: &str| {
            graph.direct_relations_limit(node, true, 0).1
                + graph.direct_relations_limit(node, false, 0).1
        };
        for node in ["a", "b", "c", "d", "hub", "missing"] {
            assert_eq!(graph.direct_degree(node), totals(&graph, node), "{node}");
        }
        // x.ts is rewritten to a single new edge and y.ts is deleted.
        let replacement = OwnedEdge::from(&from_file("d", "hub", "x.ts"));
        let mut overlay = IncrementalGraphOverlay::default();
        overlay
            .file_upserts
            .insert("x.ts".into(), [replacement.clone()].into_iter().collect());
        overlay.file_tombstones.insert("y.ts".into());
        overlay.edge_counts.insert(replacement, Some(1));
        for removed in [&originals[0], &originals[1], &originals[2]] {
            overlay.edge_counts.insert(OwnedEdge::from(removed), None);
        }
        graph.apply_incremental_overlay(&overlay, "after", 2);
        graph.finish_incremental_overlays();
        let mut overlaid = 0;
        for node in ["a", "b", "c", "d", "hub", "missing"] {
            assert_eq!(graph.direct_degree(node), totals(&graph, node), "{node}");
            overlaid += graph.relation_overlay_nodes.contains(node) as usize;
        }
        assert!(overlaid >= 4, "the overlay path was not exercised");
        // hub kept nothing from x.ts or y.ts and gained the replacement.
        assert_eq!(graph.direct_degree("hub"), 1);
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
    fn walk_pages_resume_from_cursor_until_exhausted() {
        // a ← b, a ← c, a ← d: three callers, page_size 1 → three pages
        // chained by cursor, next_cursor None at the end.
        let edges = vec![edge("b", "a"), edge("c", "a"), edge("d", "a")];
        let graph = GraphIndex::from_edges(&edges, "snapshot".into());
        let mut limits = QueryLimits {
            page_size: 1,
            ..Default::default()
        };
        let mut collected = Vec::new();
        loop {
            let page = graph.callers_of("a", &limits, None).unwrap();
            collected.extend(page.items);
            match page.next_cursor {
                Some(cursor) => limits.cursor = cursor.parse().unwrap(),
                None => break,
            }
        }
        collected.sort();
        assert_eq!(collected, vec!["b".to_owned(), "c".into(), "d".into()]);
    }

    #[test]
    fn file_cycles_find_file_level_sccs_that_package_buckets_hide() {
        // a.ts ↔ b.ts cycle plus an acyclic c.ts, all inside one path bucket:
        // package-level SCC sees a single package (no cycle), file-level must
        // report exactly the {a.ts, b.ts} component.
        let edges = vec![
            edge("symbol://src/a.ts#value:A", "symbol://src/b.ts#value:B"),
            edge("symbol://src/b.ts#value:B", "symbol://src/a.ts#value:A"),
            edge("symbol://src/c.ts#value:C", "symbol://src/a.ts#value:A"),
        ];
        let graph = GraphIndex::from_edges(&edges, "snapshot".into());
        assert_eq!(graph.package_cycles().len(), 0);
        let cycles = graph.file_cycles();
        assert_eq!(cycles.len(), 1);
        let mut members = cycles[0].clone();
        members.sort();
        assert_eq!(members, vec!["src/a.ts".to_owned(), "src/b.ts".to_owned()]);
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

    /// Deterministic pseudo-random edges: a small node set and a handful of files and spans, so the
    /// same (kind, file, span) position repeats and display-order ties actually occur.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % bound
        }
    }

    const KINDS: [EdgeKind; 9] = EDGE_KINDS;

    fn random_edge(rng: &mut Lcg, files: u64) -> Edge {
        let mut edge = edge(
            &format!("n{}", rng.below(14)),
            &format!("n{}", rng.below(14)),
        );
        edge.kind = KINDS[rng.below(9) as usize].clone();
        edge.source_path = Some(format!("src/f{}.ts", rng.below(files)));
        let line = rng.below(4) as u32;
        edge.span = (rng.below(6) != 0).then_some(Span {
            start_byte: line * 40,
            end_byte: line * 40 + 5,
            start_line: line,
            start_column: 0,
            end_line: line,
            end_column: 5,
        });
        edge.confidence = match rng.below(3) {
            0 => EdgeConfidence::Resolved {
                score: 1.0,
                reason: "t".into(),
            },
            1 => EdgeConfidence::Candidate {
                score: 0.5,
                reason: "t".into(),
            },
            _ => EdgeConfidence::Unresolved {
                score: 0.1,
                reason: "t".into(),
            },
        };
        edge.type_only = rng.below(5) == 0;
        edge
    }

    /// The algorithm the relation page used before sites were borrowed: build an owned view of every
    /// surviving persisted relation and every overlay edge, sort them all, then truncate. Kept here
    /// verbatim so the merge that replaced it is held to its answers, ties in node name included.
    fn sorted_views(
        graph: &GraphIndex,
        node: &str,
        reverse: bool,
        limit: usize,
    ) -> (Vec<RelationView>, usize) {
        let Some(node_id) = graph.node_index.get(&graph.nodes, node) else {
            return (Vec::new(), 0);
        };
        let relation_ids = if reverse {
            graph.reverse_relation_ids.get(node_id as usize)
        } else {
            graph.forward_relation_ids.get(node_id as usize)
        };
        let view = |relation: &CompactRelation| graph.relation_ref(relation, reverse).to_view();
        if !graph.relation_overlay_nodes.contains(node) {
            let total = relation_ids.map_or(0, <[u32]>::len);
            let items = relation_ids
                .into_iter()
                .flatten()
                .take(limit)
                .filter_map(|relation_id| graph.relations.get(*relation_id as usize))
                .map(view)
                .collect();
            return (items, total);
        }
        let mut items = relation_ids
            .into_iter()
            .flatten()
            .filter_map(|relation_id| graph.relations.get(*relation_id as usize))
            .filter(|relation| {
                relation
                    .source_path
                    .and_then(|id| graph.nodes.get(id as usize))
                    .is_none_or(|path| !graph.relation_file_overlays.contains_key(path))
            })
            .map(view)
            .collect::<Vec<_>>();
        let overlay_relations = if reverse {
            graph.overlay_reverse_relations.get(node)
        } else {
            graph.overlay_forward_relations.get(node)
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
        items.sort_by(|left, right| {
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
        (items, total)
    }

    /// The per-kind tally the page reported before kinds got an array slot each.
    fn mapped_kind_counts(
        graph: &GraphIndex,
        node: &str,
        reverse: bool,
    ) -> BTreeMap<&'static str, usize> {
        let (views, _) = sorted_views(graph, node, reverse, usize::MAX);
        let mut counts = BTreeMap::new();
        for view in views {
            *counts.entry(view.kind.as_str()).or_default() += 1;
        }
        counts
    }

    fn assert_pages_match(graph: &GraphIndex, nodes: &[String], context: &str) {
        for node in nodes {
            for reverse in [true, false] {
                let (expected, expected_total) = sorted_views(graph, node, reverse, usize::MAX);
                let (sites, total) = graph.direct_relations(node, reverse);
                assert_eq!(total, expected_total, "{context}: total {node} {reverse}");
                let actual: Vec<_> = sites.map(|site| site.to_view()).collect();
                assert_eq!(
                    actual, expected,
                    "{context}: order {node} reverse={reverse}"
                );
                // Any page, reached by skipping, is the same slice of that order.
                for skip in [0, 1, 2, 5, expected.len(), expected.len() + 3] {
                    for take in [0, 1, 4] {
                        let (sites, _) = graph.direct_relations(node, reverse);
                        let page: Vec<_> =
                            sites.skip(skip).take(take).map(|s| s.to_view()).collect();
                        let from = skip.min(expected.len());
                        let to = (skip + take).min(expected.len());
                        assert_eq!(
                            page,
                            expected[from..to],
                            "{context}: page {node} {skip}+{take}"
                        );
                    }
                }
                assert_eq!(
                    graph.direct_relations_limit(node, reverse, 3).0,
                    expected.iter().take(3).cloned().collect::<Vec<_>>(),
                    "{context}: limit {node}"
                );
                assert_eq!(
                    graph.direct_relation_kind_counts(node, reverse),
                    mapped_kind_counts(graph, node, reverse),
                    "{context}: kinds {node} reverse={reverse}"
                );
            }
        }
    }

    #[test]
    fn edge_kind_slots_are_dense_and_named_once() {
        let mut names = BTreeSet::new();
        for (index, kind) in EDGE_KINDS.iter().enumerate() {
            assert_eq!(edge_kind_slot(kind), index);
            assert!(names.insert(kind.as_str()), "{kind:?} shares a name");
        }
    }

    #[test]
    fn borrowed_sites_match_the_owned_views_for_an_untouched_graph() {
        let mut rng = Lcg(7);
        let edges: Vec<Edge> = (0..300).map(|_| random_edge(&mut rng, 9)).collect();
        let graph = GraphIndex::from_edges(&edges, "snapshot".into());
        let nodes: Vec<String> = (0..16).map(|index| format!("n{index}")).collect();
        assert_pages_match(&graph, &nodes, "untouched");
        assert_eq!(graph.direct_relations("missing", true).1, 0);
        assert!(graph.direct_relations("missing", true).0.next().is_none());
    }

    /// A node an incremental overlay touched used to build and sort a view of every site on every
    /// call. The merge that replaced it must give the order that sort gave: surviving persisted
    /// sites and the overlay's together, ties at one position broken by node name.
    #[test]
    fn overlaid_sites_merge_into_the_order_a_full_sort_gives() {
        for seed in 1..=12u64 {
            let mut rng = Lcg(seed);
            let edges: Vec<Edge> = (0..260).map(|_| random_edge(&mut rng, 9)).collect();
            let mut graph = GraphIndex::from_edges(&edges, "before".into());

            let mut overlay = IncrementalGraphOverlay::default();
            // Rewritten files (one of them new), a removed file, and one the overlay never mentions.
            for file in [1u64, 4, 9] {
                let path = format!("src/f{file}.ts");
                let fresh: Vec<Edge> = (0..(5 + rng.below(25)))
                    .map(|_| {
                        let mut edge = random_edge(&mut rng, 1);
                        edge.source_path = Some(path.clone());
                        edge
                    })
                    .collect();
                overlay
                    .file_upserts
                    .insert(path, fresh.iter().map(OwnedEdge::from).collect());
            }
            overlay.file_tombstones.insert("src/f6.ts".to_owned());
            // The edges whose endpoints count as touched: what the rewritten and removed files held
            // before, and what the rewritten ones hold now.
            for edge in &edges {
                let path = edge.source_path.as_deref().unwrap_or("");
                if overlay.file_upserts.contains_key(path) || overlay.file_tombstones.contains(path)
                {
                    overlay.edge_counts.insert(OwnedEdge::from(edge), None);
                }
            }
            for edges in overlay.file_upserts.values() {
                for edge in edges {
                    overlay.edge_counts.insert(edge.clone(), Some(1));
                }
            }
            graph.apply_incremental_overlay(&overlay, "after", 0);
            graph.finish_incremental_overlays();

            let nodes: Vec<String> = (0..16).map(|index| format!("n{index}")).collect();
            assert!(
                nodes
                    .iter()
                    .any(|node| graph.relation_overlay_nodes.contains(node)),
                "seed {seed}: the overlay touched nothing"
            );
            assert_pages_match(&graph, &nodes, &format!("overlaid seed {seed}"));
        }
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
