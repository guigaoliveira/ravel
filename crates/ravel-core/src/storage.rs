use crate::durable_io::{atomic_replace, sync_parent_directory};
use crate::{
    analysis::{self, HubEntry},
    generation_pack::{GenerationPackReader, StreamingGenerationPackWriter},
    graph::{CompactGraph, GraphIndex},
    incremental_graph::{
        GraphAdjShard, GraphEdgeShard, GraphFileShard, IncrementalGraphOverlay,
        IncrementalGraphState, OwnedEdge, digest_shard_id, graph_shard_id, owned_edge_digest,
    },
    model::{
        FileArtifact, FileHashIndex, FileList, IndexSnapshot, IndexStats, SnapshotId,
        SymbolMetaDict, SymbolMetaOverlay,
    },
    resolver::{
        LookupSlice, ModuleExport, ResolutionLookup, ResolutionUniverse, ResolutionUniverseOverlay,
        ResolutionUniverseShard, ResolverConfig, SymbolDefinition, resolution_shard_id,
    },
    search::{SearchIndex, SearchTermOverlay, SymbolDict},
    structural::FileContribution,
    structural_reverse::{
        ReverseOverlaySet, ReverseShard, ReverseShardOverlay, ReverseShardSet,
        shard_id as reverse_shard_id,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
};
use thiserror::Error;

const SCHEMA_VERSION: u32 = 7;
const STRUCTURAL_SHARD_BITS: u8 = 12;
// Graph base pack sections shard independently. 12 bits (4096 buckets) keeps each shard small
// enough that a single-file cold sync decodes only a thin slice of the ~785MB graph section.
const GRAPH_FILE_BITS: u8 = 12;
const GRAPH_EDGE_BITS: u8 = 12;
const GRAPH_ADJ_BITS: u8 = 12;
// Corruption/allocation guards belong to the storage format, not workspace tuning. Keep them
// centralized so every reader enforces the same bounds.
const MAX_COMPONENT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_DELTA_COMPONENT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_COMPACT_GRAPH_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_STATS_BYTES: u64 = 16 * 1024 * 1024;
static STRUCTURAL_PUBLISH_FAILPOINT: AtomicU8 = AtomicU8::new(0);
static SEARCH_PUBLISH_SEQUENCE: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static STRUCTURAL_FAILPOINT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn structural_publish_failpoint(stage: u8, path: &Path) -> Result<(), StorageError> {
    if STRUCTURAL_PUBLISH_FAILPOINT.load(Ordering::Relaxed) == stage {
        return Err(StorageError::Invalid {
            path: path.to_path_buf(),
            message: format!("injected structural publish failure at stage {stage}"),
        });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage I/O at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("invalid snapshot {path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("snapshot JSON at {path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("snapshot bincode at {path}: {source}")]
    Bincode {
        path: PathBuf,
        source: bincode::Error,
    },
    #[error("search index at {path}: {message}")]
    Search { path: PathBuf, message: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub snapshot_id: SnapshotId,
    /// Identity embedded in `payload`; structural generations may reuse an older payload while
    /// publishing current global sidecars and artifact overlays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_snapshot_id: Option<SnapshotId>,
    /// Snapshot id embedded in reused immutable sidecars. Present for delta generations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_snapshot_id: Option<SnapshotId>,
    pub schema_version: u32,
    pub checksum: String,
    pub payload: String,
    /// Optional relative path to prebuilt compact graph (bincode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<String>,
    /// Optional relative path to stats sidecar (JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<String>,
    /// Optional relative path to symbol dictionary (bincode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbols: Option<String>,
    /// Optional blake3 of symbols sidecar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbols_checksum: Option<String>,
    /// Optional relative path to on-disk Tantivy directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_dir: Option<String>,
    /// Ordered small search deltas layered over the immutable Tantivy/name base.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search_overlays: Vec<String>,
    /// Optional symbol metadata for node_detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_meta: Option<String>,
    /// Ordered metadata deltas layered over `symbol_meta` for structural sync generations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbol_meta_overlays: Vec<String>,
    /// Optional sorted file path list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<String>,
    /// Optional path→content hash index (fast auto-sync no-op without full snapshot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_hashes: Option<String>,
    /// Optional precomputed top-k hubs (JSON) for O(1) cold hubs CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hubs: Option<String>,
    /// Random-access artifact index used by incremental sync generations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_index: Option<String>,
    /// Ordered small artifact-index deltas layered over `artifact_index`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_deltas: Vec<String>,
    /// Number of incremental generations represented by each size-tiered delta.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_delta_weights: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_store: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_locator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_state: Option<[u8; 32]>,
    /// Sum of live serialized artifact payload lengths; drives amplification compaction without
    /// hydrating the artifact index on every edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_live_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structural_packs: Option<StructuralPackChain>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct StructuralPackChain {
    pub base: String,
    pub current_snapshot: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlays: Vec<String>,
}

/// Test-only convenience bundle: production staging streams parts one at a time through
/// [`StructuralPackStager`] instead of holding all three states at once.
#[cfg(test)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct StructuralPackBase {
    pub snapshot_id: String,
    pub universe: ResolutionUniverse,
    pub reverse: ReverseShardSet,
    pub graph: IncrementalGraphState,
}

/// Streaming writer for a structural base pack. Each `stage_*` call shards, serializes, and
/// appends one part, letting the caller drop that state before building the next — see
/// `begin_structural_pack_base`.
pub(crate) struct StructuralPackStager {
    generation: String,
    name: String,
    path: PathBuf,
    writer: StreamingGenerationPackWriter,
}

impl StructuralPackStager {
    fn add_meta<T: serde::Serialize>(&mut self, key: &str, value: &T) -> Result<(), StorageError> {
        let bytes = bincode::serialize(value).map_err(|source| StorageError::Bincode {
            path: self.path.clone(),
            source,
        })?;
        self.writer
            .add(key, bytes)
            .map_err(|error| StorageError::Invalid {
                path: self.path.clone(),
                message: error.to_string(),
            })
    }

    pub(crate) fn stage_universe(
        &mut self,
        universe: ResolutionUniverse,
    ) -> Result<(), StorageError> {
        let universe =
            universe
                .into_shards(STRUCTURAL_SHARD_BITS)
                .ok_or_else(|| StorageError::Invalid {
                    path: self.path.clone(),
                    message: "invalid resolution universe shard layout".into(),
                })?;
        self.add_meta(
            "meta/universe",
            &(
                universe.format_version,
                universe.resolver_fingerprint.clone(),
                universe.shard_bits,
            ),
        )?;
        add_shards_parallel(&mut self.writer, &self.path, "universe/", universe.shards)
    }

    pub(crate) fn stage_reverse(&mut self, reverse: ReverseShardSet) -> Result<(), StorageError> {
        // Sectioned layout: `meta/reverse2` + one record per non-empty section per shard, so
        // readers decode only the map a lookup touches. Readers that only understand the
        // legacy whole-shard layout see no `meta/reverse` key and fall back to a republish
        // tier — same rollout shape `meta/graph2` used.
        self.add_meta(
            "meta/reverse2",
            &(
                reverse.format_version,
                reverse.resolver_fingerprint.clone(),
                reverse.shard_bits,
            ),
        )?;
        let mut files = BTreeMap::new();
        let mut memberships: [BTreeMap<u16, BTreeMap<String, BTreeSet<String>>>; 4] =
            Default::default();
        for (id, mut shard) in reverse.shards {
            if !shard.files.is_empty() {
                files.insert(id, std::mem::take(&mut shard.files));
            }
            for section in ReverseSection::ALL {
                let map = section.take_from_shard(&mut shard);
                if !map.is_empty() {
                    memberships[section.index()].insert(id, map);
                }
            }
        }
        add_shards_parallel(&mut self.writer, &self.path, "reverse/file/", files)?;
        for section in ReverseSection::ALL {
            add_shards_parallel(
                &mut self.writer,
                &self.path,
                section.key_prefix(),
                std::mem::take(&mut memberships[section.index()]),
            )?;
        }
        Ok(())
    }

    pub(crate) fn stage_graph(&mut self, graph: IncrementalGraphState) -> Result<(), StorageError> {
        let graph = graph
            .into_section_shards(GRAPH_FILE_BITS, GRAPH_EDGE_BITS, GRAPH_ADJ_BITS)
            .ok_or_else(|| StorageError::Invalid {
                path: self.path.clone(),
                message: "invalid graph shard layout".into(),
            })?;
        self.add_meta(
            "meta/graph2",
            &(
                graph.format_version,
                graph.file_bits,
                graph.edge_bits,
                graph.adj_bits,
            ),
        )?;
        add_shards_parallel(&mut self.writer, &self.path, "graph/file/", graph.files)?;
        add_shards_parallel(&mut self.writer, &self.path, "graph/edge/", graph.edges)?;
        add_shards_parallel(&mut self.writer, &self.path, "graph/adj/", graph.adjacency)
    }

    pub(crate) fn finish(self) -> Result<StagedStructuralPack, StorageError> {
        let Self {
            generation,
            name,
            path,
            writer,
        } = self;
        writer.publish().map_err(|error| StorageError::Invalid {
            path,
            message: error.to_string(),
        })?;
        Ok(StagedStructuralPack { generation, name })
    }
}

#[derive(Debug)]
pub(crate) struct StructuralPackReader {
    base_reader: Mutex<GenerationPackReader>,
    universe_format_version: u32,
    resolver_fingerprint: String,
    universe_shard_bits: u8,
    reverse_format_version: u32,
    reverse_shard_bits: u8,
    graph_format_version: u32,
    graph_file_bits: u8,
    graph_edge_bits: u8,
    graph_adj_bits: u8,
    universe_overlays: Vec<BTreeMap<u16, ResolutionUniverseOverlay>>,
    reverse_overlays: Vec<ReverseOverlaySet>,
    graph_overlays: Vec<GraphOverlaySections>,
    /// RwLock, not Mutex: parallel subset resolution hits this cache from every rayon worker
    /// for each import lookup, and warm syncs are all hits — shared read locks keep the hot
    /// path concurrent while the rare miss takes the write lock to insert.
    universe_cache: RwLock<BTreeMap<u16, Arc<ResolutionUniverseShard>>>,
    reverse_files_cache: Mutex<BTreeMap<u16, Arc<BTreeMap<String, FileContribution>>>>,
    /// One cache per membership section, indexed by [`ReverseSection`].
    reverse_membership_caches: [ReverseMembershipCache; 4],
    graph_file_cache: Mutex<BTreeMap<u16, Arc<GraphFileShard>>>,
    graph_edge_cache: Mutex<BTreeMap<u16, Arc<GraphEdgeShard>>>,
    graph_adj_cache: Mutex<BTreeMap<u16, Arc<GraphAdjShard>>>,
    error: Mutex<Option<String>>,
}

/// Lazy per-shard cache of one reverse membership section (`key → member paths`).
type ReverseMembershipCache = Mutex<BTreeMap<u16, Arc<BTreeMap<String, BTreeSet<String>>>>>;

/// The four membership sections of the reverse index. Each is stored, cached, and loaded
/// independently (`reverse/<prefix>/<shard>` pack keys) so a lookup decodes only the map it
/// needs instead of a whole [`ReverseShard`] — cold delta syncs previously paid a five-map
/// decode per touched shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReverseSection {
    ModuleImporters,
    BasenameImporters,
    SymbolDefiners,
    SymbolReferrers,
}

impl ReverseSection {
    const ALL: [Self; 4] = [
        Self::ModuleImporters,
        Self::BasenameImporters,
        Self::SymbolDefiners,
        Self::SymbolReferrers,
    ];

    fn index(self) -> usize {
        match self {
            Self::ModuleImporters => 0,
            Self::BasenameImporters => 1,
            Self::SymbolDefiners => 2,
            Self::SymbolReferrers => 3,
        }
    }

    fn key_prefix(self) -> &'static str {
        match self {
            Self::ModuleImporters => "reverse/mod/",
            Self::BasenameImporters => "reverse/base/",
            Self::SymbolDefiners => "reverse/def/",
            Self::SymbolReferrers => "reverse/ref/",
        }
    }

    fn overlay(self, delta: &ReverseShardOverlay) -> &crate::structural_reverse::MembershipOverlay {
        match self {
            Self::ModuleImporters => &delta.module_importers,
            Self::BasenameImporters => &delta.basename_importers,
            Self::SymbolDefiners => &delta.symbol_definers,
            Self::SymbolReferrers => &delta.symbol_referrers,
        }
    }

    fn take_from_shard(self, shard: &mut ReverseShard) -> BTreeMap<String, BTreeSet<String>> {
        match self {
            Self::ModuleImporters => std::mem::take(&mut shard.module_importers),
            Self::BasenameImporters => std::mem::take(&mut shard.basename_importers),
            Self::SymbolDefiners => std::mem::take(&mut shard.symbol_definers),
            Self::SymbolReferrers => std::mem::take(&mut shard.symbol_referrers),
        }
    }
}

/// One published overlay, pre-partitioned per section so lazy shard loads apply only the
/// relevant slice. Edge keys are pre-hashed once at partition time.
#[derive(Debug)]
struct GraphOverlaySections {
    files: BTreeMap<u16, GraphFileSectionOverlay>,
    edges: BTreeMap<u16, BTreeMap<u128, Option<u32>>>,
    adjacency: BTreeMap<u16, GraphAdjSectionOverlay>,
}

#[derive(Debug, Default)]
struct GraphFileSectionOverlay {
    upserts: BTreeMap<String, BTreeSet<OwnedEdge>>,
    tombstones: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct GraphAdjSectionOverlay {
    forward_refcounts: BTreeMap<String, Option<BTreeMap<String, u32>>>,
    reverse_refcounts: BTreeMap<String, Option<BTreeMap<String, u32>>>,
    forward_changes: BTreeMap<String, BTreeMap<String, Option<u32>>>,
    reverse_changes: BTreeMap<String, BTreeMap<String, Option<u32>>>,
}

pub(crate) struct StagedStructuralPack {
    generation: String,
    name: String,
}

struct StructuralOverlayRecords {
    weight: u64,
    graph: IncrementalGraphOverlay,
    universe: ResolutionUniverseOverlay,
    reverse: ReverseOverlaySet,
}

pub(crate) struct ResidentStructuralDelta<'a> {
    pub(crate) snapshot_id: &'a SnapshotId,
    pub(crate) artifacts: &'a [(String, Option<FileArtifact>)],
    pub(crate) graph: &'a IncrementalGraphOverlay,
    pub(crate) universe: &'a ResolutionUniverseOverlay,
    pub(crate) reverse: &'a ReverseOverlaySet,
    pub(crate) symbol_meta: &'a SymbolMetaOverlay,
    pub(crate) search: Option<&'a SearchTermOverlay>,
    pub(crate) stats: &'a IndexStats,
}

impl StructuralPackReader {
    fn record_error(&self, error: impl ToString) {
        let mut slot = self.error.lock().unwrap();
        if slot.is_none() {
            *slot = Some(error.to_string());
        }
    }

    pub(crate) fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap().take()
    }

    fn read_base<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        // Decode outside the reader lock: concurrent shard lookups (parallel resolution)
        // otherwise serialize on the pack mutex for the duration of every deserialize.
        let bytes = {
            let mut reader = self.base_reader.lock().unwrap();
            match reader.read(key, MAX_DELTA_COMPONENT_BYTES) {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.record_error(error);
                    return None;
                }
            }
        }?;
        match bincode::deserialize(&bytes) {
            Ok(value) => Some(value),
            Err(error) => {
                self.record_error(error);
                None
            }
        }
    }

    fn universe_shard(&self, key: &str) -> Arc<ResolutionUniverseShard> {
        let id = resolution_shard_id(key, self.universe_shard_bits);
        if let Some(shard) = self.universe_cache.read().unwrap().get(&id).cloned() {
            return shard;
        }
        let mut shard: ResolutionUniverseShard = self
            .read_base(&format!("universe/{id:04x}"))
            .unwrap_or_default();
        for overlays in &self.universe_overlays {
            if let Some(overlay) = overlays.get(&id) {
                apply_universe_overlay_to_shard(&mut shard, overlay);
            }
        }
        let shard = Arc::new(shard);
        self.universe_cache
            .write()
            .unwrap()
            .insert(id, Arc::clone(&shard));
        shard
    }

    fn reverse_files_shard(&self, id: u16) -> Arc<BTreeMap<String, FileContribution>> {
        if let Some(section) = self.reverse_files_cache.lock().unwrap().get(&id).cloned() {
            return section;
        }
        let mut files: BTreeMap<String, FileContribution> = self
            .read_base(&format!("reverse/file/{id:04x}"))
            .unwrap_or_default();
        for overlay in &self.reverse_overlays {
            if let Some(delta) = overlay.shards.get(&id) {
                apply_reverse_map(&mut files, &delta.files);
            }
        }
        let section = Arc::new(files);
        self.reverse_files_cache
            .lock()
            .unwrap()
            .insert(id, Arc::clone(&section));
        section
    }

    fn reverse_membership_shard(
        &self,
        section: ReverseSection,
        id: u16,
    ) -> Arc<BTreeMap<String, BTreeSet<String>>> {
        let cache = &self.reverse_membership_caches[section.index()];
        if let Some(map) = cache.lock().unwrap().get(&id).cloned() {
            return map;
        }
        let mut map: BTreeMap<String, BTreeSet<String>> = self
            .read_base(&format!("{}{id:04x}", section.key_prefix()))
            .unwrap_or_default();
        for overlay in &self.reverse_overlays {
            if let Some(delta) = overlay.shards.get(&id) {
                section.overlay(delta).apply_to(&mut map);
            }
        }
        let map = Arc::new(map);
        cache.lock().unwrap().insert(id, Arc::clone(&map));
        map
    }

    fn apply_file_shard_overlays(&self, id: u16, shard: &mut GraphFileShard) {
        for overlay in &self.graph_overlays {
            if let Some(section) = overlay.files.get(&id) {
                apply_file_overlay_to_shard(shard, section);
            }
        }
    }

    fn apply_edge_shard_overlays(&self, id: u16, shard: &mut GraphEdgeShard) {
        for overlay in &self.graph_overlays {
            if let Some(section) = overlay.edges.get(&id) {
                apply_edge_overlay_to_shard(shard, section);
            }
        }
    }

    fn apply_adj_shard_overlays(&self, id: u16, shard: &mut GraphAdjShard) {
        for overlay in &self.graph_overlays {
            if let Some(section) = overlay.adjacency.get(&id) {
                apply_adj_overlay_to_shard(shard, section);
            }
        }
    }

    /// Advance the resident reader to the generation just published by applying the freshly
    /// written overlay in place. Keeps warm shard caches valid across syncs instead of forcing
    /// a full re-hydration + shard re-decode of the base pack on the next update. Returns
    /// `false` when the reader should be dropped instead: either the overlay could not be
    /// partitioned, or the resident chain grew past the cap — the on-disk chain is compacted by
    /// deferred maintenance, so re-hydrating then is cheaper than holding an ever-growing
    /// overlay list in memory.
    pub(crate) fn apply_published_delta(
        &mut self,
        graph: &IncrementalGraphOverlay,
        universe: &ResolutionUniverseOverlay,
        reverse: &ReverseOverlaySet,
    ) -> bool {
        const MAX_RESIDENT_OVERLAYS: usize = 16;
        if self.graph_overlays.len() >= MAX_RESIDENT_OVERLAYS {
            return false;
        }
        let Some(graph_split) = partition_graph_overlay_sections(
            graph,
            self.graph_file_bits,
            self.graph_edge_bits,
            self.graph_adj_bits,
        ) else {
            return false;
        };
        let universe_split = partition_universe_overlay(universe.clone(), self.universe_shard_bits);
        {
            let mut cache = self.graph_file_cache.lock().unwrap();
            for (id, shard) in cache.iter_mut() {
                if let Some(section) = graph_split.files.get(id) {
                    apply_file_overlay_to_shard(Arc::make_mut(shard), section);
                }
            }
        }
        {
            let mut cache = self.graph_edge_cache.lock().unwrap();
            for (id, shard) in cache.iter_mut() {
                if let Some(section) = graph_split.edges.get(id) {
                    apply_edge_overlay_to_shard(Arc::make_mut(shard), section);
                }
            }
        }
        {
            let mut cache = self.graph_adj_cache.lock().unwrap();
            for (id, shard) in cache.iter_mut() {
                if let Some(section) = graph_split.adjacency.get(id) {
                    apply_adj_overlay_to_shard(Arc::make_mut(shard), section);
                }
            }
        }
        {
            let mut cache = self.universe_cache.write().unwrap();
            for (id, shard) in cache.iter_mut() {
                if let Some(overlay) = universe_split.get(id) {
                    apply_universe_overlay_to_shard(Arc::make_mut(shard), overlay);
                }
            }
        }
        {
            let mut cache = self.reverse_files_cache.lock().unwrap();
            for (id, files) in cache.iter_mut() {
                if let Some(delta) = reverse.shards.get(id) {
                    apply_reverse_map(Arc::make_mut(files), &delta.files);
                }
            }
        }
        for section in ReverseSection::ALL {
            let mut cache = self.reverse_membership_caches[section.index()]
                .lock()
                .unwrap();
            for (id, map) in cache.iter_mut() {
                if let Some(delta) = reverse.shards.get(id) {
                    section.overlay(delta).apply_to(Arc::make_mut(map));
                }
            }
        }
        self.graph_overlays.push(graph_split);
        self.universe_overlays.push(universe_split);
        self.reverse_overlays.push(reverse.clone());
        true
    }

    fn graph_file_shard(&self, id: u16) -> Arc<GraphFileShard> {
        if let Some(shard) = self.graph_file_cache.lock().unwrap().get(&id).cloned() {
            return shard;
        }
        let mut shard: GraphFileShard = self
            .read_base(&format!("graph/file/{id:04x}"))
            .unwrap_or_default();
        self.apply_file_shard_overlays(id, &mut shard);
        let shard = Arc::new(shard);
        self.graph_file_cache
            .lock()
            .unwrap()
            .insert(id, Arc::clone(&shard));
        shard
    }

    fn graph_edge_shard(&self, id: u16) -> Arc<GraphEdgeShard> {
        if let Some(shard) = self.graph_edge_cache.lock().unwrap().get(&id).cloned() {
            return shard;
        }
        let mut shard: GraphEdgeShard = self
            .read_base(&format!("graph/edge/{id:04x}"))
            .unwrap_or_default();
        self.apply_edge_shard_overlays(id, &mut shard);
        let shard = Arc::new(shard);
        self.graph_edge_cache
            .lock()
            .unwrap()
            .insert(id, Arc::clone(&shard));
        shard
    }

    fn graph_adj_shard(&self, id: u16) -> Arc<GraphAdjShard> {
        if let Some(shard) = self.graph_adj_cache.lock().unwrap().get(&id).cloned() {
            return shard;
        }
        let mut shard: GraphAdjShard = self
            .read_base(&format!("graph/adj/{id:04x}"))
            .unwrap_or_default();
        self.apply_adj_shard_overlays(id, &mut shard);
        let shard = Arc::new(shard);
        self.graph_adj_cache
            .lock()
            .unwrap()
            .insert(id, Arc::clone(&shard));
        shard
    }

    /// Read raw base-pack bytes for many keys under one reader lock.
    fn read_base_bytes_batch(&self, keys: &[String]) -> Vec<Option<Vec<u8>>> {
        let mut reader = self.base_reader.lock().unwrap();
        keys.iter()
            .map(|key| match reader.read(key, MAX_DELTA_COMPONENT_BYTES) {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.record_error(error.to_string());
                    None
                }
            })
            .collect()
    }

    /// Decode any uncached graph file shards in parallel. Whole-shard bincode decode dominates
    /// structural sync wall-time on large graphs; rayon spreads it across cores.
    fn prefetch_graph_file_shards(&self, ids: impl IntoIterator<Item = u16>) {
        use rayon::prelude::*;
        let missing: Vec<u16> = {
            let cache = self.graph_file_cache.lock().unwrap();
            ids.into_iter()
                .collect::<BTreeSet<u16>>()
                .into_iter()
                .filter(|id| !cache.contains_key(id))
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let keys: Vec<String> = missing
            .iter()
            .map(|id| format!("graph/file/{id:04x}"))
            .collect();
        let read_start = std::time::Instant::now();
        let bytes = self.read_base_bytes_batch(&keys);
        crate::timing::stage("prefetch.gfile.read", read_start, || {
            format!(
                "shards={} bytes={}",
                keys.len(),
                bytes.iter().flatten().map(Vec::len).sum::<usize>()
            )
        });
        let decode_start = std::time::Instant::now();
        let decoded: Vec<(u16, GraphFileShard)> = missing
            .into_par_iter()
            .zip(bytes)
            .map(|(id, bytes)| {
                let mut shard: GraphFileShard = bytes
                    .and_then(|bytes| match bincode::deserialize(&bytes) {
                        Ok(shard) => Some(shard),
                        Err(error) => {
                            self.record_error(error.to_string());
                            None
                        }
                    })
                    .unwrap_or_default();
                self.apply_file_shard_overlays(id, &mut shard);
                (id, shard)
            })
            .collect();
        crate::timing::stage("prefetch.gfile.decode", decode_start, String::new);
        let mut cache = self.graph_file_cache.lock().unwrap();
        for (id, shard) in decoded {
            cache.entry(id).or_insert_with(|| Arc::new(shard));
        }
    }

    /// Parallel-decode counterpart for edge (refcount) shards.
    fn prefetch_graph_edge_shards(&self, ids: impl IntoIterator<Item = u16>) {
        use rayon::prelude::*;
        let missing: Vec<u16> = {
            let cache = self.graph_edge_cache.lock().unwrap();
            ids.into_iter()
                .collect::<BTreeSet<u16>>()
                .into_iter()
                .filter(|id| !cache.contains_key(id))
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let keys: Vec<String> = missing
            .iter()
            .map(|id| format!("graph/edge/{id:04x}"))
            .collect();
        let read_start = std::time::Instant::now();
        let bytes = self.read_base_bytes_batch(&keys);
        crate::timing::stage("prefetch.gedge.read", read_start, || {
            format!(
                "shards={} bytes={}",
                keys.len(),
                bytes.iter().flatten().map(Vec::len).sum::<usize>()
            )
        });
        let decode_start = std::time::Instant::now();
        let decoded: Vec<(u16, GraphEdgeShard)> = missing
            .into_par_iter()
            .zip(bytes)
            .map(|(id, bytes)| {
                let mut shard: GraphEdgeShard = bytes
                    .and_then(|bytes| match bincode::deserialize(&bytes) {
                        Ok(shard) => Some(shard),
                        Err(error) => {
                            self.record_error(error.to_string());
                            None
                        }
                    })
                    .unwrap_or_default();
                self.apply_edge_shard_overlays(id, &mut shard);
                (id, shard)
            })
            .collect();
        crate::timing::stage("prefetch.gedge.decode", decode_start, String::new);
        let mut cache = self.graph_edge_cache.lock().unwrap();
        for (id, shard) in decoded {
            cache.entry(id).or_insert_with(|| Arc::new(shard));
        }
    }

    /// Parallel-decode counterpart for adjacency shards.
    fn prefetch_graph_adj_shards(&self, ids: impl IntoIterator<Item = u16>) {
        use rayon::prelude::*;
        let missing: Vec<u16> = {
            let cache = self.graph_adj_cache.lock().unwrap();
            ids.into_iter()
                .collect::<BTreeSet<u16>>()
                .into_iter()
                .filter(|id| !cache.contains_key(id))
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let keys: Vec<String> = missing
            .iter()
            .map(|id| format!("graph/adj/{id:04x}"))
            .collect();
        let read_start = std::time::Instant::now();
        let bytes = self.read_base_bytes_batch(&keys);
        crate::timing::stage("prefetch.gadj.read", read_start, || {
            format!(
                "shards={} bytes={}",
                keys.len(),
                bytes.iter().flatten().map(Vec::len).sum::<usize>()
            )
        });
        let decode_start = std::time::Instant::now();
        let decoded: Vec<(u16, GraphAdjShard)> = missing
            .into_par_iter()
            .zip(bytes)
            .map(|(id, bytes)| {
                let mut shard: GraphAdjShard = bytes
                    .and_then(|bytes| match bincode::deserialize(&bytes) {
                        Ok(shard) => Some(shard),
                        Err(error) => {
                            self.record_error(error.to_string());
                            None
                        }
                    })
                    .unwrap_or_default();
                self.apply_adj_shard_overlays(id, &mut shard);
                (id, shard)
            })
            .collect();
        crate::timing::stage("prefetch.gadj.decode", decode_start, String::new);
        let mut cache = self.graph_adj_cache.lock().unwrap();
        for (id, shard) in decoded {
            cache.entry(id).or_insert_with(|| Arc::new(shard));
        }
    }

    /// Parallel-decode counterpart of [`Self::prefetch_graph_file_shards`] for the reverse
    /// `files` section.
    fn prefetch_reverse_files_shards(&self, ids: impl IntoIterator<Item = u16>) {
        use rayon::prelude::*;
        let missing: Vec<u16> = {
            let cache = self.reverse_files_cache.lock().unwrap();
            ids.into_iter()
                .collect::<BTreeSet<u16>>()
                .into_iter()
                .filter(|id| !cache.contains_key(id))
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let keys: Vec<String> = missing
            .iter()
            .map(|id| format!("reverse/file/{id:04x}"))
            .collect();
        let read_start = std::time::Instant::now();
        let bytes = self.read_base_bytes_batch(&keys);
        crate::timing::stage("prefetch.reverse.read", read_start, || {
            format!(
                "section=file shards={} bytes={}",
                keys.len(),
                bytes.iter().flatten().map(Vec::len).sum::<usize>()
            )
        });
        let decode_start = std::time::Instant::now();
        let decoded: Vec<(u16, BTreeMap<String, FileContribution>)> = missing
            .into_par_iter()
            .zip(bytes)
            .map(|(id, bytes)| {
                let mut files: BTreeMap<String, FileContribution> = bytes
                    .and_then(|bytes| match bincode::deserialize(&bytes) {
                        Ok(section) => Some(section),
                        Err(error) => {
                            self.record_error(error.to_string());
                            None
                        }
                    })
                    .unwrap_or_default();
                for overlay in &self.reverse_overlays {
                    if let Some(delta) = overlay.shards.get(&id) {
                        apply_reverse_map(&mut files, &delta.files);
                    }
                }
                (id, files)
            })
            .collect();
        crate::timing::stage("prefetch.reverse.decode", decode_start, String::new);
        let mut cache = self.reverse_files_cache.lock().unwrap();
        for (id, section) in decoded {
            cache.entry(id).or_insert_with(|| Arc::new(section));
        }
    }

    /// Parallel-decode one reverse membership section for many shard ids.
    fn prefetch_reverse_membership_shards(
        &self,
        section: ReverseSection,
        ids: impl IntoIterator<Item = u16>,
    ) {
        use rayon::prelude::*;
        let missing: Vec<u16> = {
            let cache = self.reverse_membership_caches[section.index()]
                .lock()
                .unwrap();
            ids.into_iter()
                .collect::<BTreeSet<u16>>()
                .into_iter()
                .filter(|id| !cache.contains_key(id))
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let keys: Vec<String> = missing
            .iter()
            .map(|id| format!("{}{id:04x}", section.key_prefix()))
            .collect();
        let read_start = std::time::Instant::now();
        let bytes = self.read_base_bytes_batch(&keys);
        crate::timing::stage("prefetch.reverse.read", read_start, || {
            format!(
                "section={} shards={} bytes={}",
                section.key_prefix(),
                keys.len(),
                bytes.iter().flatten().map(Vec::len).sum::<usize>()
            )
        });
        let decode_start = std::time::Instant::now();
        let decoded: Vec<(u16, BTreeMap<String, BTreeSet<String>>)> = missing
            .into_par_iter()
            .zip(bytes)
            .map(|(id, bytes)| {
                let mut map: BTreeMap<String, BTreeSet<String>> = bytes
                    .and_then(|bytes| match bincode::deserialize(&bytes) {
                        Ok(section) => Some(section),
                        Err(error) => {
                            self.record_error(error.to_string());
                            None
                        }
                    })
                    .unwrap_or_default();
                for overlay in &self.reverse_overlays {
                    if let Some(delta) = overlay.shards.get(&id) {
                        section.overlay(delta).apply_to(&mut map);
                    }
                }
                (id, map)
            })
            .collect();
        crate::timing::stage("prefetch.reverse.decode", decode_start, String::new);
        let mut cache = self.reverse_membership_caches[section.index()]
            .lock()
            .unwrap();
        for (id, map) in decoded {
            cache.entry(id).or_insert_with(|| Arc::new(map));
        }
    }

    pub(crate) fn affected_files<'a>(
        &self,
        changed_paths: impl IntoIterator<Item = &'a str>,
        changed_symbols: impl IntoIterator<Item = &'a str>,
    ) -> BTreeSet<String> {
        let mut affected = BTreeSet::new();
        for path in changed_paths {
            affected.insert(path.to_owned());
            let importers = self.reverse_membership_shard(
                ReverseSection::ModuleImporters,
                reverse_shard_id(path, self.reverse_shard_bits),
            );
            if let Some(importers) = importers.get(path) {
                affected.extend(importers.iter().cloned());
            }
            if let Some(stem) = Path::new(path).file_stem().and_then(|stem| stem.to_str()) {
                let importers = self.reverse_membership_shard(
                    ReverseSection::BasenameImporters,
                    reverse_shard_id(stem, self.reverse_shard_bits),
                );
                if let Some(importers) = importers.get(stem) {
                    affected.extend(importers.iter().cloned());
                }
            }
        }
        for symbol in changed_symbols {
            let id = reverse_shard_id(symbol, self.reverse_shard_bits);
            if let Some(definers) = self
                .reverse_membership_shard(ReverseSection::SymbolDefiners, id)
                .get(symbol)
            {
                affected.extend(definers.iter().cloned());
            }
            if let Some(referrers) = self
                .reverse_membership_shard(ReverseSection::SymbolReferrers, id)
                .get(symbol)
            {
                affected.extend(referrers.iter().cloned());
            }
        }
        affected
    }

    /// Build a partial [`ReverseShardSet`] holding only the entries `replace_files` will touch:
    /// the `files` records of the updated paths plus the membership sets of every key referenced
    /// by their old/new contributions. Cloning whole shards for a handful of keys dominated
    /// structural sync time and RSS on large workspaces.
    pub(crate) fn reverse_for_updates(
        &self,
        updates: &BTreeMap<String, Option<FileContribution>>,
    ) -> ReverseShardSet {
        self.prefetch_reverse_files_shards(
            updates
                .keys()
                .map(|path| reverse_shard_id(path, self.reverse_shard_bits)),
        );
        let mut set = ReverseShardSet {
            format_version: self.reverse_format_version,
            resolver_fingerprint: self.resolver_fingerprint.clone(),
            shard_bits: self.reverse_shard_bits,
            shards: BTreeMap::new(),
        };
        // Membership keys touched by removals of old contributions and inserts of new ones.
        let mut module_keys = BTreeSet::new();
        let mut basename_keys = BTreeSet::new();
        let mut definer_keys = BTreeSet::new();
        let mut referrer_keys = BTreeSet::new();
        let mut collect_keys = |contribution: &FileContribution| {
            module_keys.extend(contribution.module_candidates.iter().cloned());
            basename_keys.extend(contribution.bare_specifiers.iter().cloned());
            definer_keys.extend(contribution.symbol_definitions.iter().cloned());
            referrer_keys.extend(contribution.symbol_references.iter().cloned());
        };
        for (path, replacement) in updates {
            let id = reverse_shard_id(path, self.reverse_shard_bits);
            let files = self.reverse_files_shard(id);
            if let Some(old) = files.get(path) {
                collect_keys(old);
                set.shards
                    .entry(id)
                    .or_default()
                    .files
                    .insert(path.clone(), old.clone());
            }
            if let Some(new) = replacement {
                collect_keys(new);
            }
        }
        let key_id = |key: &String| reverse_shard_id(key, self.reverse_shard_bits);
        // Each key class prefetches only its own section — a symbol-heavy delta no longer
        // decodes module/basename maps it will never read (and vice versa).
        let classes = [
            (ReverseSection::ModuleImporters, &module_keys),
            (ReverseSection::BasenameImporters, &basename_keys),
            (ReverseSection::SymbolDefiners, &definer_keys),
            (ReverseSection::SymbolReferrers, &referrer_keys),
        ];
        for (section, keys) in &classes {
            self.prefetch_reverse_membership_shards(*section, keys.iter().map(key_id));
        }
        let insert_members = |section: ReverseSection, key: String, set: &mut ReverseShardSet| {
            let id = key_id(&key);
            let members = self.reverse_membership_shard(section, id);
            if let Some(members) = members.get(&key) {
                let shard = set.shards.entry(id).or_default();
                let target = match section {
                    ReverseSection::ModuleImporters => &mut shard.module_importers,
                    ReverseSection::BasenameImporters => &mut shard.basename_importers,
                    ReverseSection::SymbolDefiners => &mut shard.symbol_definers,
                    ReverseSection::SymbolReferrers => &mut shard.symbol_referrers,
                };
                target.insert(key, members.clone());
            }
        };
        for key in module_keys {
            insert_members(ReverseSection::ModuleImporters, key, &mut set);
        }
        for key in basename_keys {
            insert_members(ReverseSection::BasenameImporters, key, &mut set);
        }
        for key in definer_keys {
            insert_members(ReverseSection::SymbolDefiners, key, &mut set);
        }
        for key in referrer_keys {
            insert_members(ReverseSection::SymbolReferrers, key, &mut set);
        }
        set
    }

    /// Build a partial [`IncrementalGraphState`] holding only what `replace_owned_files` will
    /// touch: the updated paths' current edge sets, the refcounts of every old/new edge, and the
    /// adjacency maps of their endpoint nodes. The previous whole-shard clone+merge decoded and
    /// copied nearly the entire graph for a single-file sync.
    pub(crate) fn graph_for_updates(
        &self,
        updates: &BTreeMap<String, Option<BTreeSet<OwnedEdge>>>,
    ) -> IncrementalGraphState {
        let mut partial = IncrementalGraphState {
            format_version: IncrementalGraphState::FORMAT_VERSION,
            ..IncrementalGraphState::default()
        };
        if self.graph_format_version != IncrementalGraphState::FORMAT_VERSION {
            // Mirror the defensive default the shard-set merge used for unknown layouts.
            return partial;
        }
        // File section: current edge ownership of every updated path.
        self.prefetch_graph_file_shards(
            updates
                .keys()
                .map(|path| graph_shard_id(path, self.graph_file_bits)),
        );
        let mut touched_edges: BTreeSet<OwnedEdge> = BTreeSet::new();
        for (path, replacement) in updates {
            let file_shard = self.graph_file_shard(graph_shard_id(path, self.graph_file_bits));
            if let Some(old) = file_shard.by_file.get(path) {
                partial.by_file.insert(path.clone(), old.clone());
                touched_edges.extend(old.iter().cloned());
            }
            if let Some(new) = replacement {
                touched_edges.extend(new.iter().cloned());
            }
        }
        // Hash every touched edge once; the digest is both the edge-shard id and the refcount key.
        let mut edge_keys = Vec::with_capacity(touched_edges.len());
        let mut edge_ids = BTreeSet::new();
        let mut touched_nodes = BTreeSet::new();
        for edge in &touched_edges {
            if let Some(digest) = owned_edge_digest(edge) {
                let id = digest_shard_id(digest, self.graph_edge_bits);
                edge_ids.insert(id);
                edge_keys.push((edge, digest, id));
            } else {
                self.record_error("failed to encode graph edge shard key");
            }
            touched_nodes.insert(edge.from.as_str());
            touched_nodes.insert(edge.to.as_str());
        }
        let node_ids: Vec<(&str, u16)> = touched_nodes
            .into_iter()
            .map(|node| (node, graph_shard_id(node, self.graph_adj_bits)))
            .collect();
        self.prefetch_graph_edge_shards(edge_ids);
        self.prefetch_graph_adj_shards(node_ids.iter().map(|(_, id)| *id));
        for (edge, digest, id) in edge_keys {
            if let Some(count) = self.graph_edge_shard(id).edge_refcounts.get(&digest) {
                partial.edge_refcounts.insert(edge.clone(), *count);
            }
        }
        for (node, id) in node_ids {
            let shard = self.graph_adj_shard(id);
            if let Some(neighbors) = shard.forward_refcounts.get(node) {
                partial
                    .forward_refcounts
                    .insert(node.to_owned(), neighbors.clone());
            }
            if let Some(neighbors) = shard.reverse_refcounts.get(node) {
                partial
                    .reverse_refcounts
                    .insert(node.to_owned(), neighbors.clone());
            }
        }
        partial
    }
}

impl ResolutionLookup for StructuralPackReader {
    fn matches(&self, config: &ResolverConfig) -> bool {
        self.universe_format_version == ResolutionUniverse::FORMAT_VERSION
            && self.resolver_fingerprint == crate::resolver::resolver_fingerprint(config)
    }

    fn contains_file(&self, path: &str) -> bool {
        self.universe_shard(path).files.contains(path)
    }

    fn symbol_definer_count(&self, name: &str) -> u32 {
        self.universe_shard(name)
            .symbol_definitions
            .get(name)
            .map_or(0, |definitions| {
                u32::try_from(definitions.len()).unwrap_or(u32::MAX)
            })
    }

    fn symbol_definitions(&self, name: &str) -> LookupSlice<'_, SymbolDefinition> {
        LookupSlice::Owned(
            self.universe_shard(name)
                .symbol_definitions
                .get(name)
                .cloned()
                .unwrap_or_default(),
        )
    }

    fn module_exports(&self, path: &str) -> LookupSlice<'_, ModuleExport> {
        LookupSlice::Owned(
            self.universe_shard(path)
                .module_exports
                .get(path)
                .cloned()
                .unwrap_or_default(),
        )
    }
}

fn read_pack_value_from_reader<T: serde::de::DeserializeOwned>(
    reader: &mut GenerationPackReader,
    path: &Path,
    key: &str,
    max_bytes: u64,
) -> Result<Option<T>, StorageError> {
    let Some(bytes) = reader
        .read(key, max_bytes)
        .map_err(|error| StorageError::Invalid {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?
    else {
        return Ok(None);
    };
    bincode::deserialize(&bytes)
        .map(Some)
        .map_err(|source| StorageError::Bincode {
            path: path.to_path_buf(),
            source,
        })
}

fn compose_universe_overlay(
    mut older: ResolutionUniverseOverlay,
    newer: ResolutionUniverseOverlay,
) -> ResolutionUniverseOverlay {
    older.files.extend(newer.files);
    older.symbol_definitions.extend(newer.symbol_definitions);
    older.module_exports.extend(newer.module_exports);
    older
}

fn partition_universe_overlay(
    overlay: ResolutionUniverseOverlay,
    bits: u8,
) -> BTreeMap<u16, ResolutionUniverseOverlay> {
    let mut shards = BTreeMap::<u16, ResolutionUniverseOverlay>::new();
    for (path, present) in overlay.files {
        shards
            .entry(resolution_shard_id(&path, bits))
            .or_default()
            .files
            .insert(path, present);
    }
    for (name, definitions) in overlay.symbol_definitions {
        shards
            .entry(resolution_shard_id(&name, bits))
            .or_default()
            .symbol_definitions
            .insert(name, definitions);
    }
    for (path, exports) in overlay.module_exports {
        shards
            .entry(resolution_shard_id(&path, bits))
            .or_default()
            .module_exports
            .insert(path, exports);
    }
    shards
}

/// Partition a published overlay into the three section key-spaces, hashing each edge key once.
/// Returns `None` if an edge cannot be encoded (mirrors `into_section_shards`).
fn partition_graph_overlay_sections(
    overlay: &IncrementalGraphOverlay,
    file_bits: u8,
    edge_bits: u8,
    adj_bits: u8,
) -> Option<GraphOverlaySections> {
    let mut out = GraphOverlaySections {
        files: BTreeMap::new(),
        edges: BTreeMap::new(),
        adjacency: BTreeMap::new(),
    };
    for path in &overlay.file_tombstones {
        out.files
            .entry(graph_shard_id(path, file_bits))
            .or_default()
            .tombstones
            .insert(path.clone());
    }
    for (path, edges) in &overlay.file_upserts {
        out.files
            .entry(graph_shard_id(path, file_bits))
            .or_default()
            .upserts
            .insert(path.clone(), edges.clone());
    }
    for (edge, count) in &overlay.edge_counts {
        let digest = owned_edge_digest(edge)?;
        out.edges
            .entry(digest_shard_id(digest, edge_bits))
            .or_default()
            .insert(digest, *count);
    }
    for (node, counts) in &overlay.forward_refcounts {
        out.adjacency
            .entry(graph_shard_id(node, adj_bits))
            .or_default()
            .forward_refcounts
            .insert(node.clone(), counts.clone());
    }
    for (node, counts) in &overlay.reverse_refcounts {
        out.adjacency
            .entry(graph_shard_id(node, adj_bits))
            .or_default()
            .reverse_refcounts
            .insert(node.clone(), counts.clone());
    }
    for (node, changes) in &overlay.forward_changes {
        out.adjacency
            .entry(graph_shard_id(node, adj_bits))
            .or_default()
            .forward_changes
            .insert(node.clone(), changes.clone());
    }
    for (node, changes) in &overlay.reverse_changes {
        out.adjacency
            .entry(graph_shard_id(node, adj_bits))
            .or_default()
            .reverse_changes
            .insert(node.clone(), changes.clone());
    }
    Some(out)
}

fn compose_graph_overlay(
    mut older: IncrementalGraphOverlay,
    newer: IncrementalGraphOverlay,
) -> IncrementalGraphOverlay {
    for path in newer.file_tombstones {
        older.file_upserts.remove(&path);
        older.file_tombstones.insert(path);
    }
    for (path, edges) in newer.file_upserts {
        older.file_tombstones.remove(&path);
        older.file_upserts.insert(path, edges);
    }
    older.edge_counts.extend(newer.edge_counts);
    compose_adjacency(
        &mut older.forward_refcounts,
        &mut older.forward_changes,
        newer.forward_refcounts,
        newer.forward_changes,
    );
    compose_adjacency(
        &mut older.reverse_refcounts,
        &mut older.reverse_changes,
        newer.reverse_refcounts,
        newer.reverse_changes,
    );
    older
}

/// Fold newer adjacency encodings onto older ones so applying the composition equals applying
/// older then newer. A newer full map supersedes anything older; newer per-neighbor changes
/// merge into an older full map in place, or neighbor-wise into older changes (newer wins).
fn compose_adjacency(
    older_full: &mut BTreeMap<String, Option<BTreeMap<String, u32>>>,
    older_changes: &mut BTreeMap<String, BTreeMap<String, Option<u32>>>,
    newer_full: BTreeMap<String, Option<BTreeMap<String, u32>>>,
    newer_changes: BTreeMap<String, BTreeMap<String, Option<u32>>>,
) {
    for (node, neighbors) in newer_changes {
        match older_full.get_mut(&node) {
            Some(Some(map)) => {
                for (neighbor, count) in neighbors {
                    match count {
                        Some(count) => {
                            map.insert(neighbor, count);
                        }
                        None => {
                            map.remove(&neighbor);
                        }
                    }
                }
                if map.is_empty() {
                    older_full.insert(node, None);
                }
            }
            Some(None) => {
                // Node was removed, then neighbors re-added: the surviving counts are the map.
                let map: BTreeMap<String, u32> = neighbors
                    .into_iter()
                    .filter_map(|(neighbor, count)| count.map(|count| (neighbor, count)))
                    .collect();
                older_full.insert(node, (!map.is_empty()).then_some(map));
            }
            None => {
                older_changes.entry(node).or_default().extend(neighbors);
            }
        }
    }
    for (node, map) in newer_full {
        older_changes.remove(&node);
        older_full.insert(node, map);
    }
}

fn compose_map_overlay<V>(
    older: &mut crate::structural_reverse::MapOverlay<V>,
    newer: crate::structural_reverse::MapOverlay<V>,
) {
    for key in newer.tombstones {
        older.upserts.remove(&key);
        older.tombstones.insert(key);
    }
    for (key, value) in newer.upserts {
        older.tombstones.remove(&key);
        older.upserts.insert(key, value);
    }
}

fn compose_reverse_overlay(
    mut older: ReverseOverlaySet,
    newer: ReverseOverlaySet,
) -> Option<ReverseOverlaySet> {
    if older.format_version != newer.format_version
        || older.resolver_fingerprint != newer.resolver_fingerprint
        || older.shard_bits != newer.shard_bits
    {
        return None;
    }
    for (id, newer) in newer.shards {
        let older = older.shards.entry(id).or_default();
        compose_map_overlay(&mut older.files, newer.files);
        older.module_importers.compose(newer.module_importers);
        older.basename_importers.compose(newer.basename_importers);
        older.symbol_definers.compose(newer.symbol_definers);
        older.symbol_referrers.compose(newer.symbol_referrers);
    }
    Some(older)
}

fn apply_optional_value<K: Ord + Clone, V: Clone>(
    target: &mut BTreeMap<K, V>,
    key: &K,
    value: &Option<V>,
) {
    if let Some(value) = value {
        target.insert(key.clone(), value.clone());
    } else {
        target.remove(key);
    }
}

fn apply_reverse_map<V: Clone>(
    target: &mut BTreeMap<String, V>,
    overlay: &crate::structural_reverse::MapOverlay<V>,
) {
    for key in &overlay.tombstones {
        target.remove(key);
    }
    target.extend(overlay.upserts.clone());
}

fn apply_optional_map_values<V: Clone>(
    target: &mut BTreeMap<String, V>,
    overlay: &BTreeMap<String, Option<V>>,
) {
    for (key, value) in overlay {
        apply_optional_value(target, key, value);
    }
}

fn apply_file_overlay_to_shard(shard: &mut GraphFileShard, overlay: &GraphFileSectionOverlay) {
    for path in &overlay.tombstones {
        shard.by_file.remove(path);
    }
    for (path, edges) in &overlay.upserts {
        shard.by_file.insert(path.clone(), edges.clone());
    }
}

fn apply_edge_overlay_to_shard(shard: &mut GraphEdgeShard, overlay: &BTreeMap<u128, Option<u32>>) {
    for (digest, count) in overlay {
        apply_optional_value(&mut shard.edge_refcounts, digest, count);
    }
}

fn apply_adj_overlay_to_shard(shard: &mut GraphAdjShard, overlay: &GraphAdjSectionOverlay) {
    apply_optional_map_values(&mut shard.forward_refcounts, &overlay.forward_refcounts);
    apply_optional_map_values(&mut shard.reverse_refcounts, &overlay.reverse_refcounts);
    apply_adjacency_changes(&mut shard.forward_refcounts, &overlay.forward_changes);
    apply_adjacency_changes(&mut shard.reverse_refcounts, &overlay.reverse_changes);
}

/// Apply per-neighbor absolute count changes; empty maps drop the node entry entirely so the
/// result matches a full rebuild bit for bit.
fn apply_adjacency_changes(
    target: &mut BTreeMap<String, BTreeMap<String, u32>>,
    changes: &BTreeMap<String, BTreeMap<String, Option<u32>>>,
) {
    for (node, neighbors) in changes {
        let map = target.entry(node.clone()).or_default();
        for (neighbor, count) in neighbors {
            match count {
                Some(count) => {
                    map.insert(neighbor.clone(), *count);
                }
                None => {
                    map.remove(neighbor);
                }
            }
        }
        if map.is_empty() {
            target.remove(node);
        }
    }
}

fn apply_universe_overlay_to_shard(
    shard: &mut ResolutionUniverseShard,
    overlay: &ResolutionUniverseOverlay,
) {
    for (path, present) in &overlay.files {
        if *present {
            shard.files.insert(path.clone());
        } else {
            shard.files.remove(path);
        }
    }
    apply_optional_map_values(&mut shard.symbol_definitions, &overlay.symbol_definitions);
    apply_optional_map_values(&mut shard.module_exports, &overlay.module_exports);
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct ArtifactLocation {
    offset: u64,
    len: u64,
    source_hash: String,
    bytes_read: u64,
    parse_error: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct ArtifactIndex {
    store: String,
    entries: BTreeMap<String, ArtifactLocation>,
    overrides: BTreeSet<String>,
    tombstones: BTreeSet<String>,
    /// XOR of path+content digests supports O(changed paths) generation updates.
    state: [u8; 32],
}

pub trait SnapshotStorage {
    fn publish(&self, snapshot: &IndexSnapshot) -> Result<(), StorageError>;
    fn open_current(&self) -> Result<Option<IndexSnapshot>, StorageError>;
    fn validate(&self) -> Result<(), StorageError>;
}

#[derive(Debug)]
pub struct FileSnapshotStorage {
    root: PathBuf,
    retention: usize,
    manifest_cache: Mutex<Option<(std::time::SystemTime, Manifest)>>,
}

impl Clone for FileSnapshotStorage {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            retention: self.retention,
            manifest_cache: Mutex::new(None),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GenerationGcReport {
    pub retained_manifests: usize,
    pub removed_files: usize,
    pub removed_dirs: usize,
    pub deferred_for_readers: bool,
}
impl FileSnapshotStorage {
    const LOCATOR_RECORD_LEN: u64 = 89;
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self::with_retention(root, 3)
    }

    pub fn with_retention(root: impl AsRef<Path>, retention: usize) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            retention: retention.max(1),
            manifest_cache: Mutex::new(None),
        }
    }

    fn acquire_generation_read_guard(
        &self,
    ) -> Result<crate::generation_gc::GenerationGuard, StorageError> {
        crate::generation_gc::GenerationGuard::shared(&self.root)
            .map_err(|source| self.io(source, crate::generation_gc::lock_path(&self.root)))
    }

    pub fn acquire_generation_gc_guard(
        &self,
    ) -> Result<crate::generation_gc::GenerationGuard, StorageError> {
        crate::generation_gc::GenerationGuard::exclusive(&self.root)
            .map_err(|source| self.io(source, crate::generation_gc::lock_path(&self.root)))
    }

    pub fn retention(&self) -> usize {
        self.retention
    }

    /// Delete unreachable immutable generations. The caller owns the workspace writer lease;
    /// this exclusive barrier waits for readers retaining any older sidecar or Tantivy index.
    pub fn gc_generations(&self) -> Result<GenerationGcReport, StorageError> {
        let Some(_guard) = crate::generation_gc::GenerationGuard::try_exclusive(&self.root)
            .map_err(|source| self.io(source, crate::generation_gc::lock_path(&self.root)))?
        else {
            return Ok(GenerationGcReport {
                deferred_for_readers: true,
                ..GenerationGcReport::default()
            });
        };
        let Some(current_name) = self.current_generation()? else {
            return Ok(GenerationGcReport::default());
        };
        let mut manifests = Vec::new();
        for entry in
            fs::read_dir(&self.root).map_err(|source| self.io(source, self.root.clone()))?
        {
            let entry = entry.map_err(|source| self.io(source, self.root.clone()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("snapshot-") || !name.ends_with(".manifest.json") {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(|source| self.io(source, entry.path()))?;
            let manifest: Manifest =
                serde_json::from_slice(&bytes).map_err(|source| StorageError::Json {
                    path: entry.path(),
                    source,
                })?;
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            manifests.push((name, modified, manifest));
        }
        manifests.sort_by(|left, right| {
            (right.0 == current_name)
                .cmp(&(left.0 == current_name))
                .then_with(|| right.1.cmp(&left.1))
                .then_with(|| right.0.cmp(&left.0))
        });
        let keep = self.retention.min(manifests.len());
        let retained_manifests: BTreeSet<_> = manifests
            .iter()
            .take(keep)
            .map(|(name, _, _)| name.clone())
            .collect();
        if !retained_manifests.contains(&current_name) {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: format!("CURRENT references missing manifest {current_name}"),
            });
        }
        let mut reachable = retained_manifests.clone();
        for (_, _, manifest) in manifests.iter().take(keep) {
            reachable.extend(Self::manifest_component_paths(manifest));
        }
        let mut report = GenerationGcReport {
            retained_manifests: retained_manifests.len(),
            ..GenerationGcReport::default()
        };
        for entry in
            fs::read_dir(&self.root).map_err(|source| self.io(source, self.root.clone()))?
        {
            let entry = entry.map_err(|source| self.io(source, self.root.clone()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let generation_artifact = name.starts_with("snapshot-")
                || name.starts_with("store.tmp-")
                || name.contains(".tmp-");
            if !generation_artifact || reachable.contains(&name) {
                continue;
            }
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(|source| self.io(source, path.clone()))?;
            if metadata.file_type().is_dir() {
                fs::remove_dir_all(&path).map_err(|source| self.io(source, path.clone()))?;
                report.removed_dirs += 1;
            } else {
                fs::remove_file(&path).map_err(|source| self.io(source, path.clone()))?;
                report.removed_files += 1;
            }
        }
        sync_parent_directory(&self.current_path())
            .map_err(|source| self.io(source, self.root.clone()))?;
        Ok(report)
    }

    fn manifest_component_paths(manifest: &Manifest) -> BTreeSet<String> {
        let mut paths = BTreeSet::new();
        let mut add = |reference: &str| {
            let path = reference
                .split_once('#')
                .map_or(reference, |(path, _)| path);
            if !path.is_empty() {
                paths.insert(path.to_owned());
            }
        };
        add(&manifest.payload);
        for reference in [
            manifest.graph.as_deref(),
            manifest.stats.as_deref(),
            manifest.symbols.as_deref(),
            manifest.search_dir.as_deref(),
            manifest.symbol_meta.as_deref(),
            manifest.files.as_deref(),
            manifest.file_hashes.as_deref(),
            manifest.hubs.as_deref(),
            manifest.artifact_index.as_deref(),
            manifest.artifact_store.as_deref(),
            manifest.artifact_locator.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            add(reference);
        }
        for reference in &manifest.artifact_deltas {
            add(reference);
        }
        for reference in &manifest.symbol_meta_overlays {
            add(reference);
        }
        for reference in &manifest.search_overlays {
            add(reference);
        }
        if let Some(chain) = &manifest.structural_packs {
            add(&chain.base);
            for reference in &chain.overlays {
                add(reference);
            }
        }
        paths
    }

    fn publish_or_reuse_search_snapshot(
        &self,
        desired_name: &str,
        snapshot: &IndexSnapshot,
    ) -> Result<String, StorageError> {
        let desired_path = self.root.join(desired_name);
        if desired_path.is_dir() && SearchIndex::open_tantivy_dir(&desired_path).is_ok() {
            return Ok(desired_name.to_owned());
        }
        let sequence = SEARCH_PUBLISH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let actual_name = if desired_path.exists() {
            format!("{desired_name}.recovered-{}-{sequence}", std::process::id())
        } else {
            desired_name.to_owned()
        };
        let search_path = self.root.join(&actual_name);
        let search_tmp = self.root.join(format!(
            "{actual_name}.tmp-{}-{sequence}",
            std::process::id()
        ));
        SearchIndex::publish_tantivy_snapshot(snapshot, &search_tmp).map_err(|error| {
            StorageError::Search {
                path: search_tmp.clone(),
                message: error.to_string(),
            }
        })?;
        atomic_replace(&search_tmp, &search_path)
            .map_err(|source| self.io(source, search_path.clone()))?;
        sync_parent_directory(&search_path)
            .map_err(|source| self.io(source, search_path.clone()))?;
        Ok(actual_name)
    }
    fn current_path(&self) -> PathBuf {
        self.root.join("CURRENT")
    }
    /// Lock order is `update.lock` (writer, owned by Engine) before `artifact-gc.lock`.
    /// Readers never acquire `update.lock`, so an artifact compactor can wait for existing
    /// readers without forming a lock cycle.
    fn acquire_artifact_read_lock(&self) -> Result<fs::File, StorageError> {
        fs::create_dir_all(&self.root).map_err(|source| self.io(source, self.root.clone()))?;
        let path = self.root.join("artifact-gc.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| self.io(source, path.clone()))?;
        fs4::fs_std::FileExt::lock_shared(&file).map_err(|source| self.io(source, path))?;
        Ok(file)
    }

    fn acquire_artifact_gc_lock(&self) -> Result<fs::File, StorageError> {
        fs::create_dir_all(&self.root).map_err(|source| self.io(source, self.root.clone()))?;
        let path = self.root.join("artifact-gc.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| self.io(source, path.clone()))?;
        fs4::fs_std::FileExt::lock_exclusive(&file).map_err(|source| self.io(source, path))?;
        Ok(file)
    }
    /// Cheap cross-process generation marker. `CURRENT` advances atomically only after all
    /// sidecars for a snapshot are durable.
    pub fn current_generation(&self) -> Result<Option<String>, StorageError> {
        if !self.current_path().is_file() {
            return Ok(None);
        }
        let generation = fs::read_to_string(self.current_path())
            .map_err(|source| self.io(source, self.current_path()))?;
        Ok(Some(generation.trim().to_owned()))
    }
    fn manifest_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("snapshot-{id}.manifest.json"))
    }
    fn payload_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("snapshot-{id}.bin"))
    }
    fn graph_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("snapshot-{id}.graph.bin"))
    }
    fn stats_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("snapshot-{id}.stats.json"))
    }
    fn symbols_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("snapshot-{id}.symbols.bin"))
    }
    fn symbol_meta_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("snapshot-{id}.symbol_meta.bin"))
    }
    fn files_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("snapshot-{id}.files.bin"))
    }
    fn hubs_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("snapshot-{id}.hubs.json"))
    }
    fn component_snapshot_id<'a>(&self, manifest: &'a Manifest) -> &'a SnapshotId {
        manifest
            .base_snapshot_id
            .as_ref()
            .unwrap_or(&manifest.snapshot_id)
    }
    fn payload_snapshot_id<'a>(&self, manifest: &'a Manifest) -> &'a SnapshotId {
        manifest
            .payload_snapshot_id
            .as_ref()
            .unwrap_or_else(|| self.component_snapshot_id(manifest))
    }
    fn artifact_store_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("snapshot-{id}.artifacts.store"))
    }

    fn read_component_ref(&self, reference: &str, max_bytes: u64) -> Result<Vec<u8>, StorageError> {
        if let Some((pack_name, record_key)) = reference.split_once('#') {
            let path = self.root.join(pack_name);
            let mut reader =
                GenerationPackReader::open(&path).map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
            return reader
                .read(record_key, max_bytes)
                .map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?
                .ok_or_else(|| StorageError::Invalid {
                    path,
                    message: format!("missing pack record {record_key}"),
                });
        }
        let path = self.root.join(reference);
        fs::read(&path).map_err(|source| self.io(source, path))
    }

    fn component_ref_path<'a>(&self, reference: &'a str) -> &'a str {
        reference
            .split_once('#')
            .map_or(reference, |(path, _)| path)
    }

    fn locator_name(id: &str) -> String {
        format!("snapshot-{id}.artifacts.loc")
    }
    pub fn read_manifest(&self) -> Result<Option<Manifest>, StorageError> {
        if !self.current_path().is_file() {
            *self.manifest_cache.lock().unwrap() = None;
            return Ok(None);
        }
        let name = fs::read_to_string(self.current_path())
            .map_err(|source| self.io(source, self.current_path()))?;
        let path = self.root.join(name.trim());
        if let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) {
            if let Ok(cache) = self.manifest_cache.lock() {
                if let Some((cached_mtime, manifest)) = cache.as_ref() {
                    if *cached_mtime == mtime {
                        return Ok(Some(manifest.clone()));
                    }
                }
            }
        }
        let bytes = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|source| StorageError::Json {
                path: path.clone(),
                source,
            })?;
        if let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) {
            if let Ok(mut cache) = self.manifest_cache.lock() {
                *cache = Some((mtime, manifest.clone()));
            }
        }
        Ok(Some(manifest))
    }

    #[cfg(test)]
    pub(crate) fn stage_structural_pack_base(
        &self,
        base: StructuralPackBase,
    ) -> Result<StagedStructuralPack, StorageError> {
        let mut stager = self.begin_structural_pack_base(base.snapshot_id)?;
        stager.stage_universe(base.universe)?;
        stager.stage_reverse(base.reverse)?;
        stager.stage_graph(base.graph)?;
        stager.finish()
    }

    /// Open a streaming stager for a structural base pack. Parts are staged one at a time so a
    /// caller can build → stage → drop each large state instead of holding universe, reverse,
    /// and graph in memory simultaneously (the full-index RSS plateau).
    pub(crate) fn begin_structural_pack_base(
        &self,
        generation: String,
    ) -> Result<StructuralPackStager, StorageError> {
        // Keep the staged pack outside generation GC until the manifest publication that will
        // reference it has completed. `attach_structural_pack_base` atomically promotes it.
        let name = format!("staged-{generation}.structural.pack");
        let path = self.root.join(&name);
        let writer =
            StreamingGenerationPackWriter::new(&path).map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        Ok(StructuralPackStager {
            generation,
            name,
            path,
            writer,
        })
    }

    pub(crate) fn attach_structural_pack_base(
        &self,
        staged: StagedStructuralPack,
    ) -> Result<(), StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(mut manifest) = self.read_manifest()? else {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "cannot attach structural pack without a current manifest".into(),
            });
        };
        if staged.generation != manifest.snapshot_id.stable_key() {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "structural pack snapshot does not match current manifest".into(),
            });
        }
        let final_name = format!("snapshot-{}.structural.pack", staged.generation);
        let staged_path = self.root.join(&staged.name);
        let final_path = self.root.join(&final_name);
        atomic_replace(&staged_path, &final_path)
            .map_err(|source| self.io(source, final_path.clone()))?;
        sync_parent_directory(&final_path).map_err(|source| self.io(source, self.root.clone()))?;
        manifest.structural_packs = Some(StructuralPackChain {
            base: final_name,
            current_snapshot: staged.generation.clone(),
            overlays: Vec::new(),
        });
        let manifest_path = self.manifest_path(&staged.generation);
        let bytes = serde_json::to_vec(&manifest).map_err(|source| StorageError::Json {
            path: manifest_path.clone(),
            source,
        })?;
        atomic_write(&manifest_path, &bytes).map_err(|source| self.io(source, manifest_path))
    }

    pub(crate) fn open_structural_reader(
        &self,
    ) -> Result<Option<Arc<StructuralPackReader>>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        let Some(chain) = manifest.structural_packs else {
            return Ok(None);
        };
        if chain.current_snapshot != manifest.snapshot_id.stable_key() {
            return Ok(None);
        }
        let base = self.root.join(chain.base);
        let mut base_reader =
            GenerationPackReader::open(&base).map_err(|error| StorageError::Invalid {
                path: base.clone(),
                message: error.to_string(),
            })?;
        let Some((universe_format_version, resolver_fingerprint, universe_shard_bits)) =
            read_pack_value_from_reader::<(u32, String, u8)>(
                &mut base_reader,
                &base,
                "meta/universe",
                64 * 1024,
            )?
        else {
            return Ok(None);
        };
        let Some((reverse_format_version, reverse_fingerprint, reverse_shard_bits)) =
            read_pack_value_from_reader::<(u32, String, u8)>(
                &mut base_reader,
                &base,
                "meta/reverse2",
                64 * 1024,
            )?
        else {
            // Pack predates the sectioned reverse layout (only `meta/reverse`); fall back to a
            // slower tier that re-hydrates and republishes in the new format.
            return Ok(None);
        };
        if reverse_fingerprint != resolver_fingerprint {
            return Ok(None);
        }
        let Some((graph_format_version, graph_file_bits, graph_edge_bits, graph_adj_bits)) =
            read_pack_value_from_reader::<(u32, u8, u8, u8)>(
                &mut base_reader,
                &base,
                "meta/graph2",
                1024,
            )?
        else {
            // Pack predates the sectioned graph layout (only `meta/graph`); fall back to a slower
            // tier that re-hydrates and republishes in the new format.
            return Ok(None);
        };
        if graph_file_bits > 16 || graph_edge_bits > 16 || graph_adj_bits > 16 {
            // Corrupt or unknown layout: shard-id shifts assume bits <= 16 (same defensive
            // default the pre-section reader applied to `graph_shard_bits`).
            return Ok(None);
        }
        let mut universe_overlays = Vec::with_capacity(chain.overlays.len());
        let mut reverse_overlays = Vec::with_capacity(chain.overlays.len());
        let mut graph_overlays = Vec::with_capacity(chain.overlays.len());
        for overlay_name in chain.overlays {
            let path = self.root.join(overlay_name);
            let mut overlay_reader =
                GenerationPackReader::open(&path).map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
            let Some(universe) = read_pack_value_from_reader(
                &mut overlay_reader,
                &path,
                "meta/universe",
                MAX_DELTA_COMPONENT_BYTES,
            )?
            else {
                return Ok(None);
            };
            let Some(reverse) = read_pack_value_from_reader(
                &mut overlay_reader,
                &path,
                "reverse/overlay-v2",
                MAX_DELTA_COMPONENT_BYTES,
            )?
            else {
                return Ok(None);
            };
            let Some(graph) = read_pack_value_from_reader(
                &mut overlay_reader,
                &path,
                "graph/overlay-v2",
                MAX_DELTA_COMPONENT_BYTES,
            )?
            else {
                return Ok(None);
            };
            universe_overlays.push(partition_universe_overlay(universe, universe_shard_bits));
            reverse_overlays.push(reverse);
            let Some(section) = partition_graph_overlay_sections(
                &graph,
                graph_file_bits,
                graph_edge_bits,
                graph_adj_bits,
            ) else {
                return Ok(None);
            };
            graph_overlays.push(section);
        }
        Ok(Some(Arc::new(StructuralPackReader {
            base_reader: Mutex::new(base_reader),
            universe_format_version,
            resolver_fingerprint,
            universe_shard_bits,
            reverse_format_version,
            reverse_shard_bits,
            graph_format_version,
            graph_file_bits,
            graph_edge_bits,
            graph_adj_bits,
            universe_overlays,
            reverse_overlays,
            graph_overlays,
            universe_cache: RwLock::new(BTreeMap::new()),
            reverse_files_cache: Mutex::new(BTreeMap::new()),
            reverse_membership_caches: Default::default(),
            graph_file_cache: Mutex::new(BTreeMap::new()),
            graph_edge_cache: Mutex::new(BTreeMap::new()),
            graph_adj_cache: Mutex::new(BTreeMap::new()),
            error: Mutex::new(None),
        })))
    }

    pub fn open_structural_graph_base(
        &self,
    ) -> Result<Option<IncrementalGraphState>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        let Some(chain) = manifest.structural_packs else {
            return Ok(None);
        };
        if chain.current_snapshot != manifest.snapshot_id.stable_key() {
            return Ok(None);
        }
        let path = self.root.join(chain.base);
        let mut reader =
            GenerationPackReader::open(&path).map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        let Some(meta) =
            reader
                .read("meta/graph2", 1024)
                .map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?
        else {
            // Pack predates the sectioned layout; a slower tier re-hydrates and republishes it.
            return Ok(None);
        };
        let (format_version, _file_bits, _edge_bits, _adj_bits): (u32, u8, u8, u8) =
            bincode::deserialize(&meta).map_err(|source| StorageError::Bincode {
                path: path.clone(),
                source,
            })?;
        if format_version != IncrementalGraphState::FORMAT_VERSION {
            return Ok(None);
        }
        // Only the file section is needed: refcounts and adjacency are a pure function of edge
        // ownership, reconstructed by `from_owned_by_file`.
        let keys: Vec<String> = reader
            .keys()
            .filter(|key| key.starts_with("graph/file/"))
            .map(str::to_owned)
            .collect();
        let mut by_file: BTreeMap<String, BTreeSet<OwnedEdge>> = BTreeMap::new();
        for key in keys {
            let Some(bytes) = reader
                .read(&key, MAX_DELTA_COMPONENT_BYTES)
                .map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?
            else {
                return Ok(None);
            };
            let shard: GraphFileShard =
                bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode {
                    path: path.clone(),
                    source,
                })?;
            by_file.extend(shard.by_file);
        }
        // Apply each overlay's file section over the union, in chain order, before reconstruction.
        for overlay_name in chain.overlays {
            let overlay_path = self.root.join(overlay_name);
            let mut overlay_reader =
                GenerationPackReader::open(&overlay_path).map_err(|error| {
                    StorageError::Invalid {
                        path: overlay_path.clone(),
                        message: error.to_string(),
                    }
                })?;
            let Some(bytes) = overlay_reader
                .read("graph/overlay-v2", MAX_DELTA_COMPONENT_BYTES)
                .map_err(|error| StorageError::Invalid {
                    path: overlay_path.clone(),
                    message: error.to_string(),
                })?
            else {
                return Ok(None);
            };
            let overlay: IncrementalGraphOverlay =
                bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode {
                    path: overlay_path,
                    source,
                })?;
            for path in &overlay.file_tombstones {
                by_file.remove(path);
            }
            for (path, edges) in overlay.file_upserts {
                by_file.insert(path, edges);
            }
        }
        Ok(Some(IncrementalGraphState::from_owned_by_file(by_file)))
    }

    pub fn clear_manifest_cache(&self) {
        *self.manifest_cache.lock().unwrap() = None;
    }

    fn read_structural_overlay_records(
        &self,
        name: &str,
    ) -> Result<StructuralOverlayRecords, StorageError> {
        let path = self.root.join(name);
        let mut reader =
            GenerationPackReader::open(&path).map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        let weight =
            read_pack_value_from_reader(&mut reader, &path, "meta/weight", 1024)?.unwrap_or(1u64);
        let graph = read_pack_value_from_reader(
            &mut reader,
            &path,
            "graph/overlay-v2",
            MAX_DELTA_COMPONENT_BYTES,
        )?
        .ok_or_else(|| StorageError::Invalid {
            path: path.clone(),
            message: "missing graph overlay".into(),
        })?;
        let universe = read_pack_value_from_reader(
            &mut reader,
            &path,
            "meta/universe",
            MAX_DELTA_COMPONENT_BYTES,
        )?
        .ok_or_else(|| StorageError::Invalid {
            path: path.clone(),
            message: "missing universe overlay".into(),
        })?;
        let reverse = read_pack_value_from_reader(
            &mut reader,
            &path,
            "reverse/overlay-v2",
            MAX_DELTA_COMPONENT_BYTES,
        )?
        .ok_or_else(|| StorageError::Invalid {
            path,
            message: "missing reverse overlay".into(),
        })?;
        Ok(StructuralOverlayRecords {
            weight,
            graph,
            universe,
            reverse,
        })
    }

    fn write_structural_overlay_merge(
        &self,
        generation: &str,
        records: &StructuralOverlayRecords,
    ) -> Result<String, StorageError> {
        let name = format!(
            "snapshot-{generation}.structural-merge-{}.pack",
            records.weight
        );
        let path = self.root.join(&name);
        let mut writer =
            StreamingGenerationPackWriter::new(&path).map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        for (key, bytes) in [
            (
                "meta/weight",
                bincode::serialize(&records.weight).map_err(|source| StorageError::Bincode {
                    path: path.clone(),
                    source,
                })?,
            ),
            (
                "graph/overlay-v2",
                bincode::serialize(&records.graph).map_err(|source| StorageError::Bincode {
                    path: path.clone(),
                    source,
                })?,
            ),
            (
                "meta/universe",
                bincode::serialize(&records.universe).map_err(|source| StorageError::Bincode {
                    path: path.clone(),
                    source,
                })?,
            ),
            (
                "reverse/overlay-v2",
                bincode::serialize(&records.reverse).map_err(|source| StorageError::Bincode {
                    path: path.clone(),
                    source,
                })?,
            ),
        ] {
            writer
                .add(key, bytes)
                .map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
        }
        writer.publish().map_err(|error| StorageError::Invalid {
            path: path.clone(),
            message: error.to_string(),
        })?;
        Ok(name)
    }

    fn compact_structural_overlay_chain(
        &self,
        chain: &mut StructuralPackChain,
        generation: &str,
        mut current: StructuralOverlayRecords,
    ) -> Result<(), StorageError> {
        while chain.overlays.len() >= 2 {
            let previous_index = chain.overlays.len() - 2;
            let previous = self.read_structural_overlay_records(&chain.overlays[previous_index])?;
            if previous.weight > current.weight {
                break;
            }
            current = StructuralOverlayRecords {
                weight: previous.weight.saturating_add(current.weight),
                graph: compose_graph_overlay(previous.graph, current.graph),
                universe: compose_universe_overlay(previous.universe, current.universe),
                reverse: compose_reverse_overlay(previous.reverse, current.reverse).ok_or_else(
                    || StorageError::Invalid {
                        path: self.root.clone(),
                        message: "cannot merge incompatible reverse overlays".into(),
                    },
                )?,
            };
            let merged_name = self.write_structural_overlay_merge(generation, &current)?;
            chain.overlays.truncate(previous_index);
            chain.overlays.push(merged_name);
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one atomic generation pack intentionally stages every coupled component"
    )]
    fn stage_structural_overlay_pack(
        &self,
        manifest: &mut Manifest,
        snapshot_id: &SnapshotId,
        graph_overlay: &IncrementalGraphOverlay,
        universe_overlay: &ResolutionUniverseOverlay,
        reverse_overlay: &ReverseOverlaySet,
        artifact_delta: &[u8],
        stats: &[u8],
        symbol_meta_overlay: Option<&SymbolMetaOverlay>,
        search_overlay: Option<&SearchTermOverlay>,
    ) -> Result<String, StorageError> {
        let Some(chain) = manifest.structural_packs.as_mut() else {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "structural overlay requires a base pack".into(),
            });
        };
        let generation = snapshot_id.stable_key();
        let sequence = chain.overlays.len();
        let name = format!("snapshot-{generation}.structural-overlay-{sequence}.pack");
        let path = self.root.join(&name);
        let mut writer =
            StreamingGenerationPackWriter::new(&path).map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        writer
            .add(
                "meta/weight",
                bincode::serialize(&1u64).map_err(|source| StorageError::Bincode {
                    path: path.clone(),
                    source,
                })?,
            )
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        let graph_bytes =
            bincode::serialize(graph_overlay).map_err(|source| StorageError::Bincode {
                path: path.clone(),
                source,
            })?;
        crate::timing::note("overlay.graph_bytes", || graph_bytes.len().to_string());
        writer
            .add("graph/overlay-v2", graph_bytes)
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        writer
            .add("artifact/delta", artifact_delta)
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        writer
            .add("stats/json", stats)
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        let universe_bytes =
            bincode::serialize(universe_overlay).map_err(|source| StorageError::Bincode {
                path: path.clone(),
                source,
            })?;
        crate::timing::note("overlay.universe_bytes", || {
            universe_bytes.len().to_string()
        });
        writer
            .add("meta/universe", universe_bytes)
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        let reverse_bytes =
            bincode::serialize(reverse_overlay).map_err(|source| StorageError::Bincode {
                path: path.clone(),
                source,
            })?;
        crate::timing::note("overlay.reverse_bytes", || reverse_bytes.len().to_string());
        writer
            .add("reverse/overlay-v2", reverse_bytes)
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        if let Some(overlay) = symbol_meta_overlay {
            writer
                .add(
                    "symbol-meta/overlay",
                    bincode::serialize(overlay).map_err(|source| StorageError::Bincode {
                        path: path.clone(),
                        source,
                    })?,
                )
                .map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
        }
        if let Some(overlay) = search_overlay {
            writer
                .add(
                    "search/overlay",
                    bincode::serialize(overlay).map_err(|source| StorageError::Bincode {
                        path: path.clone(),
                        source,
                    })?,
                )
                .map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
        }
        writer.publish().map_err(|error| StorageError::Invalid {
            path: path.clone(),
            message: error.to_string(),
        })?;
        chain.overlays.push(name.clone());
        self.compact_structural_overlay_chain(
            chain,
            &generation,
            StructuralOverlayRecords {
                weight: 1,
                graph: graph_overlay.clone(),
                universe: universe_overlay.clone(),
                reverse: reverse_overlay.clone(),
            },
        )?;
        chain.current_snapshot = generation;
        Ok(name)
    }

    /// Read stats using a manifest already loaded by the caller.
    pub fn open_stats_from_manifest(
        &self,
        manifest: &Manifest,
    ) -> Result<Option<IndexStats>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(stats_name) = manifest.stats.as_ref() else {
            return Ok(None);
        };
        let path = self.root.join(self.component_ref_path(stats_name));
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = self.read_component_ref(stats_name, MAX_STATS_BYTES)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| StorageError::Json { path, source })
    }

    /// Presence check for a manifest reference; never reads or deserializes the sidecar.
    pub fn referenced_path_exists(&self, name: Option<&str>) -> bool {
        name.is_some_and(|name| self.root.join(self.component_ref_path(name)).exists())
    }
    fn io(&self, source: io::Error, path: PathBuf) -> StorageError {
        StorageError::Io { path, source }
    }

    fn artifact_digest(path: &str, source_hash: &str) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&(path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update(source_hash.as_bytes());
        *hasher.finalize().as_bytes()
    }

    fn xor_state(state: &mut [u8; 32], digest: [u8; 32]) {
        for (slot, byte) in state.iter_mut().zip(digest) {
            *slot ^= byte;
        }
    }

    pub(crate) fn prospective_content_state(
        &self,
        changes: &[(String, Option<String>, Option<String>)],
    ) -> Option<String> {
        let mut state = self.read_manifest().ok().flatten()?.artifact_state?;
        for (path, old, new) in changes {
            if let Some(hash) = old {
                Self::xor_state(&mut state, Self::artifact_digest(path, hash));
            }
            if let Some(hash) = new {
                Self::xor_state(&mut state, Self::artifact_digest(path, hash));
            }
        }
        Some(blake3::Hash::from_bytes(state).to_hex().to_string())
    }

    fn read_artifact_index(
        &self,
        manifest: &Manifest,
    ) -> Result<Option<ArtifactIndex>, StorageError> {
        let Some(name) = manifest.artifact_index.as_ref() else {
            return Ok(None);
        };
        let path = self.root.join(name);
        let bytes = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        let mut index: ArtifactIndex = bincode::deserialize(&bytes)
            .map_err(|source| StorageError::Bincode { path, source })?;
        for delta_name in &manifest.artifact_deltas {
            let delta_path = self.root.join(self.component_ref_path(delta_name));
            let bytes = self.read_component_ref(delta_name, MAX_COMPONENT_BYTES)?;
            let delta: ArtifactIndex =
                bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode {
                    path: delta_path,
                    source,
                })?;
            for tombstone in delta.tombstones {
                index.entries.remove(&tombstone);
                index.tombstones.insert(tombstone);
            }
            for (path, location) in delta.entries {
                index.tombstones.remove(&path);
                index.entries.insert(path, location);
            }
            index.overrides.extend(delta.overrides);
            index.state = delta.state;
        }
        Ok(Some(index))
    }

    fn read_artifact_at(
        &self,
        index: &ArtifactIndex,
        location: &ArtifactLocation,
    ) -> Result<FileArtifact, StorageError> {
        let path = self.root.join(&index.store);
        let mut file = fs::File::open(&path).map_err(|source| self.io(source, path.clone()))?;
        file.seek(SeekFrom::Start(location.offset))
            .map_err(|source| self.io(source, path.clone()))?;
        let mut bytes = vec![0; location.len as usize];
        file.read_exact(&mut bytes)
            .map_err(|source| self.io(source, path.clone()))?;
        bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode { path, source })
    }

    fn base_artifact_location(
        &self,
        manifest: &Manifest,
        artifact_path: &str,
    ) -> Result<Option<ArtifactLocation>, StorageError> {
        let Some(locator_name) = manifest.artifact_locator.as_ref() else {
            return Ok(None);
        };
        let path = self.root.join(locator_name);
        let mut file = fs::File::open(&path).map_err(|source| self.io(source, path.clone()))?;
        let records = file
            .metadata()
            .map_err(|source| self.io(source, path.clone()))?
            .len()
            / Self::LOCATOR_RECORD_LEN;
        let wanted = *blake3::hash(artifact_path.as_bytes()).as_bytes();
        let mut low = 0u64;
        let mut high = records;
        let mut record = [0u8; Self::LOCATOR_RECORD_LEN as usize];
        while low < high {
            let mid = low + (high - low) / 2;
            file.seek(SeekFrom::Start(mid * Self::LOCATOR_RECORD_LEN))
                .and_then(|_| file.read_exact(&mut record))
                .map_err(|source| self.io(source, path.clone()))?;
            match record[..32].cmp(&wanted) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => {
                    let u64_at = |start: usize| {
                        u64::from_le_bytes(record[start..start + 8].try_into().unwrap())
                    };
                    return Ok(Some(ArtifactLocation {
                        offset: u64_at(32),
                        len: u64_at(40),
                        bytes_read: u64_at(48),
                        parse_error: record[56] != 0,
                        source_hash: blake3::Hash::from_bytes(record[57..89].try_into().unwrap())
                            .to_hex()
                            .to_string(),
                    }));
                }
            }
        }
        Ok(None)
    }

    fn current_artifact_location(
        &self,
        manifest: &Manifest,
        artifact_path: &str,
    ) -> Result<Option<ArtifactLocation>, StorageError> {
        for delta_name in manifest.artifact_deltas.iter().rev() {
            let delta_path = self.root.join(self.component_ref_path(delta_name));
            let bytes = self.read_component_ref(delta_name, MAX_COMPONENT_BYTES)?;
            let delta: ArtifactIndex =
                bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode {
                    path: delta_path,
                    source,
                })?;
            if delta.tombstones.contains(artifact_path) {
                return Ok(None);
            }
            if let Some(location) = delta.entries.get(artifact_path) {
                return Ok(Some(location.clone()));
            }
        }
        self.base_artifact_location(manifest, artifact_path)
    }

    pub fn open_artifact(&self, path: &str) -> Result<Option<FileArtifact>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        // Hold through both manifest lookup and store payload read; artifact GC may replace and
        // retire both files as one exclusive operation.
        let _read_guard = self.acquire_artifact_read_lock()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        let Some(location) = self.current_artifact_location(&manifest, path)? else {
            return Ok(None);
        };
        let store = manifest.artifact_store.clone().or_else(|| {
            self.read_artifact_index(&manifest)
                .ok()
                .flatten()
                .map(|index| index.store)
        });
        let Some(store) = store else {
            return Ok(None);
        };
        let index = ArtifactIndex {
            store,
            entries: BTreeMap::new(),
            overrides: BTreeSet::new(),
            tombstones: BTreeSet::new(),
            state: manifest.artifact_state.unwrap_or([0; 32]),
        };
        self.read_artifact_at(&index, &location).map(Some)
    }

    fn write_initial_artifact_store(
        &self,
        id: &str,
        snapshot: &IndexSnapshot,
    ) -> Result<(String, String, String, [u8; 32]), StorageError> {
        let store_path = self.artifact_store_path(id);
        let store_tmp = store_path.with_extension(format!("store.tmp-{}", std::process::id()));
        let store_file =
            fs::File::create(&store_tmp).map_err(|source| self.io(source, store_tmp.clone()))?;
        let mut store = io::BufWriter::new(store_file);
        let mut offset = 0u64;
        let mut index = ArtifactIndex {
            store: store_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            entries: BTreeMap::new(),
            overrides: BTreeSet::new(),
            tombstones: BTreeSet::new(),
            state: [0; 32],
        };
        let mut locator_entries = Vec::with_capacity(snapshot.files.len());
        for (path, artifact) in &snapshot.files {
            let bytes = bincode::serialize(artifact).map_err(|source| StorageError::Bincode {
                path: store_tmp.clone(),
                source,
            })?;
            store
                .write_all(&bytes)
                .map_err(|source| self.io(source, store_tmp.clone()))?;
            Self::xor_state(
                &mut index.state,
                Self::artifact_digest(path, &artifact.source_hash),
            );
            let location = ArtifactLocation {
                offset,
                len: bytes.len() as u64,
                source_hash: artifact.source_hash.clone(),
                bytes_read: artifact.bytes_read,
                parse_error: !artifact.diagnostics.is_empty(),
            };
            locator_entries.push((*blake3::hash(path.as_bytes()).as_bytes(), location.clone()));
            index.entries.insert(path.clone(), location);
            offset =
                offset
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| StorageError::Invalid {
                        path: store_tmp.clone(),
                        message: "artifact store offset overflow".into(),
                    })?;
        }
        store
            .flush()
            .map_err(|source| self.io(source, store_tmp.clone()))?;
        store
            .get_ref()
            .sync_all()
            .map_err(|source| self.io(source, store_tmp.clone()))?;
        drop(store);
        atomic_replace(&store_tmp, &store_path)
            .map_err(|source| self.io(source, store_path.clone()))?;
        let index_name = format!("snapshot-{id}.artifacts.bin");
        self.atomic_write_bincode(&self.root.join(&index_name), &index)?;
        locator_entries.sort_unstable_by_key(|(digest, _)| *digest);
        let locator_name = Self::locator_name(id);
        let mut locator_bytes =
            Vec::with_capacity(locator_entries.len() * Self::LOCATOR_RECORD_LEN as usize);
        for (digest, location) in locator_entries {
            locator_bytes.extend_from_slice(&digest);
            locator_bytes.extend_from_slice(&location.offset.to_le_bytes());
            locator_bytes.extend_from_slice(&location.len.to_le_bytes());
            locator_bytes.extend_from_slice(&location.bytes_read.to_le_bytes());
            locator_bytes.push(u8::from(location.parse_error));
            let source_hash = blake3::Hash::from_hex(&location.source_hash).map_err(|error| {
                StorageError::Invalid {
                    path: self.root.join(&locator_name),
                    message: error.to_string(),
                }
            })?;
            locator_bytes.extend_from_slice(source_hash.as_bytes());
        }
        atomic_write(&self.root.join(&locator_name), &locator_bytes)
            .map_err(|source| self.io(source, self.root.join(&locator_name)))?;
        Ok((index_name, index.store, locator_name, index.state))
    }

    /// Publish content-only artifact changes without hydrating or rewriting the global snapshot.
    /// Callers must fall back to a full publish when symbols/imports/references change.
    pub fn publish_artifact_deltas(
        &self,
        artifacts: &[(String, FileArtifact)],
    ) -> Result<Option<IndexStats>, StorageError> {
        self.publish_artifact_deltas_inner(artifacts, true)
    }

    pub(crate) fn publish_artifact_deltas_deferred_gc(
        &self,
        artifacts: &[(String, FileArtifact)],
    ) -> Result<Option<IndexStats>, StorageError> {
        self.publish_artifact_deltas_inner(artifacts, false)
    }

    fn publish_artifact_deltas_inner(
        &self,
        artifacts: &[(String, FileArtifact)],
        run_gc: bool,
    ) -> Result<Option<IndexStats>, StorageError> {
        if artifacts.is_empty() {
            return self.open_stats();
        }
        let generation_guard = self.acquire_generation_read_guard()?;
        let Some(mut manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        let Some(store_name) = manifest.artifact_store.clone() else {
            return Ok(None);
        };
        let Some(mut artifact_state) = manifest.artifact_state else {
            return Ok(None);
        };
        let old_stats =
            self.open_stats_from_manifest(&manifest)?
                .ok_or_else(|| StorageError::Invalid {
                    path: self.current_path(),
                    message: "artifact delta requires stats".into(),
                })?;
        let store_path = self.root.join(&store_name);
        let mut store = fs::OpenOptions::new()
            .append(true)
            .open(&store_path)
            .map_err(|source| self.io(source, store_path.clone()))?;
        store
            .seek(SeekFrom::End(0))
            .map_err(|source| self.io(source, store_path.clone()))?;
        let mut bytes_total = old_stats.bytes;
        let mut parse_errors = old_stats.parse_errors;
        let mut artifact_live_bytes = manifest.artifact_live_bytes.unwrap_or_else(|| {
            self.read_artifact_index(&manifest)
                .ok()
                .flatten()
                .map(|index| index.entries.values().map(|location| location.len).sum())
                .unwrap_or(0)
        });
        let mut delta_entries = BTreeMap::new();
        let mut delta_overrides = BTreeSet::new();
        for (path, artifact) in artifacts {
            let Some(previous) = self.current_artifact_location(&manifest, path)? else {
                return Ok(None);
            };
            let payload = bincode::serialize(artifact).map_err(|source| StorageError::Bincode {
                path: store_path.clone(),
                source,
            })?;
            let offset = store
                .stream_position()
                .map_err(|source| self.io(source, store_path.clone()))?;
            store
                .write_all(&payload)
                .map_err(|source| self.io(source, store_path.clone()))?;
            Self::xor_state(
                &mut artifact_state,
                Self::artifact_digest(path, &previous.source_hash),
            );
            Self::xor_state(
                &mut artifact_state,
                Self::artifact_digest(path, &artifact.source_hash),
            );
            bytes_total = bytes_total - previous.bytes_read + artifact.bytes_read;
            artifact_live_bytes = artifact_live_bytes
                .saturating_sub(previous.len)
                .saturating_add(payload.len() as u64);
            parse_errors = parse_errors - usize::from(previous.parse_error)
                + usize::from(!artifact.diagnostics.is_empty());
            let location = ArtifactLocation {
                offset,
                len: payload.len() as u64,
                source_hash: artifact.source_hash.clone(),
                bytes_read: artifact.bytes_read,
                parse_error: !artifact.diagnostics.is_empty(),
            };
            delta_entries.insert(path.clone(), location);
            delta_overrides.insert(path.clone());
        }
        store
            .sync_data()
            .map_err(|source| self.io(source, store_path.clone()))?;
        drop(store);

        let component_id = self.component_snapshot_id(&manifest).clone();
        let mut snapshot_id = manifest.snapshot_id.clone();
        snapshot_id.content_state = blake3::Hash::from_bytes(artifact_state)
            .to_hex()
            .to_string();
        let generation = snapshot_id.stable_key();
        let delta_name = format!("snapshot-{generation}.artifact-delta.bin");
        if manifest.artifact_delta_weights.len() != manifest.artifact_deltas.len() {
            return Ok(None);
        }
        let mut delta = ArtifactIndex {
            store: store_name,
            entries: delta_entries,
            overrides: delta_overrides,
            tombstones: BTreeSet::new(),
            state: artifact_state,
        };
        // Size-tiered overlay compaction. Each retained tier is strictly heavier than the tier
        // after it, so the manifest contains at most logarithmically many deltas. Merge older
        // entries first and overlay the current entries afterwards: the newest location wins for
        // a path edited repeatedly.
        let mut delta_weight = 1u64;
        while let Some(previous_name) = manifest.artifact_deltas.last() {
            let previous_weight = *manifest
                .artifact_delta_weights
                .last()
                .expect("artifact delta weights were validated");
            if previous_weight > delta_weight {
                break;
            }
            let previous_path = self.root.join(self.component_ref_path(previous_name));
            let previous_bytes = self.read_component_ref(previous_name, MAX_COMPONENT_BYTES)?;
            let mut previous: ArtifactIndex =
                bincode::deserialize(&previous_bytes).map_err(|source| StorageError::Bincode {
                    path: previous_path,
                    source,
                })?;
            manifest.artifact_deltas.pop();
            if !manifest.artifact_delta_weights.is_empty() {
                manifest.artifact_delta_weights.pop();
            }
            for tombstone in delta.tombstones {
                previous.entries.remove(&tombstone);
                previous.tombstones.insert(tombstone);
            }
            for (path, location) in delta.entries {
                previous.tombstones.remove(&path);
                previous.entries.insert(path, location);
            }
            previous.overrides.extend(delta.overrides);
            previous.state = delta.state;
            delta_weight = previous_weight.saturating_add(delta_weight);
            delta = previous;
        }
        self.atomic_write_bincode(&self.root.join(&delta_name), &delta)?;
        let stats = IndexStats {
            files: old_stats.files,
            edges: old_stats.edges,
            bytes: bytes_total,
            parse_errors,
            snapshot_id: generation.clone(),
        };
        let stats_name = format!("snapshot-{generation}.stats.json");
        let stats_bytes = serde_json::to_vec(&stats).map_err(|source| StorageError::Json {
            path: self.root.join(&stats_name),
            source,
        })?;
        atomic_write(&self.root.join(&stats_name), &stats_bytes)
            .map_err(|source| self.io(source, self.root.join(&stats_name)))?;

        manifest.snapshot_id = snapshot_id;
        manifest.base_snapshot_id = Some(component_id);
        manifest.stats = Some(stats_name);
        manifest.artifact_deltas.push(delta_name);
        manifest.artifact_delta_weights.push(delta_weight);
        manifest.artifact_state = Some(artifact_state);
        manifest.artifact_live_bytes = Some(artifact_live_bytes);
        let manifest_name = format!("snapshot-{generation}.manifest.json");
        let manifest_path = self.root.join(&manifest_name);
        let manifest_bytes =
            serde_json::to_vec(&manifest).map_err(|source| StorageError::Json {
                path: manifest_path.clone(),
                source,
            })?;
        atomic_write(&manifest_path, &manifest_bytes)
            .map_err(|source| self.io(source, manifest_path))?;
        atomic_write(&self.current_path(), manifest_name.as_bytes())
            .map_err(|source| self.io(source, self.current_path()))?;
        drop(generation_guard);
        if run_gc {
            self.gc_generations()?;
        }
        Ok(Some(stats))
    }

    /// Publish an exact structural generation while reusing the immutable full payload and
    /// artifact base. Changed artifacts are appended; deleted paths are explicit tombstones.
    /// Global graph/search/meta/files sidecars are rebuilt from `snapshot` before `CURRENT`
    /// advances, so readers never observe a mixed generation.
    pub fn publish_structural_overlay(
        &self,
        snapshot: &IndexSnapshot,
        changed_paths: &BTreeSet<String>,
        structural_overlay: Option<(
            &IncrementalGraphOverlay,
            &ResolutionUniverseOverlay,
            &ReverseOverlaySet,
        )>,
        search_terms_changed: bool,
        stats_totals: Option<(u64, usize)>,
    ) -> Result<bool, StorageError> {
        self.publish_structural_overlay_inner(
            snapshot,
            changed_paths,
            structural_overlay,
            search_terms_changed,
            stats_totals,
            true,
        )
    }

    pub(crate) fn publish_structural_overlay_deferred_gc(
        &self,
        snapshot: &IndexSnapshot,
        changed_paths: &BTreeSet<String>,
        structural_overlay: Option<(
            &IncrementalGraphOverlay,
            &ResolutionUniverseOverlay,
            &ReverseOverlaySet,
        )>,
        search_terms_changed: bool,
        stats_totals: Option<(u64, usize)>,
    ) -> Result<bool, StorageError> {
        self.publish_structural_overlay_inner(
            snapshot,
            changed_paths,
            structural_overlay,
            search_terms_changed,
            stats_totals,
            false,
        )
    }

    fn publish_structural_overlay_inner(
        &self,
        snapshot: &IndexSnapshot,
        changed_paths: &BTreeSet<String>,
        structural_overlay: Option<(
            &IncrementalGraphOverlay,
            &ResolutionUniverseOverlay,
            &ReverseOverlaySet,
        )>,
        search_terms_changed: bool,
        stats_totals: Option<(u64, usize)>,
        run_gc: bool,
    ) -> Result<bool, StorageError> {
        let generation_guard = self.acquire_generation_read_guard()?;
        let Some(mut manifest) = self.read_manifest()? else {
            return Ok(false);
        };
        if manifest.schema_version != SCHEMA_VERSION || changed_paths.is_empty() {
            return Ok(false);
        }
        let Some(store_name) = manifest.artifact_store.clone() else {
            return Ok(false);
        };
        let Some(mut artifact_state) = manifest.artifact_state else {
            return Ok(false);
        };
        let payload_id = self.payload_snapshot_id(&manifest).clone();
        let store_path = self.root.join(&store_name);
        let mut store = fs::OpenOptions::new()
            .append(true)
            .open(&store_path)
            .map_err(|source| self.io(source, store_path.clone()))?;
        store
            .seek(SeekFrom::End(0))
            .map_err(|source| self.io(source, store_path.clone()))?;
        let mut live_bytes = manifest.artifact_live_bytes.unwrap_or(0);
        let mut delta = ArtifactIndex {
            store: store_name,
            entries: BTreeMap::new(),
            overrides: changed_paths.clone(),
            tombstones: BTreeSet::new(),
            state: artifact_state,
        };
        for path in changed_paths {
            let previous = self.current_artifact_location(&manifest, path)?;
            if let Some(previous) = &previous {
                Self::xor_state(
                    &mut artifact_state,
                    Self::artifact_digest(path, &previous.source_hash),
                );
                live_bytes = live_bytes.saturating_sub(previous.len);
            }
            let Some(artifact) = snapshot.files.get(path) else {
                delta.tombstones.insert(path.clone());
                continue;
            };
            let payload = bincode::serialize(artifact).map_err(|source| StorageError::Bincode {
                path: store_path.clone(),
                source,
            })?;
            let offset = store
                .stream_position()
                .map_err(|source| self.io(source, store_path.clone()))?;
            store
                .write_all(&payload)
                .map_err(|source| self.io(source, store_path.clone()))?;
            Self::xor_state(
                &mut artifact_state,
                Self::artifact_digest(path, &artifact.source_hash),
            );
            live_bytes = live_bytes.saturating_add(payload.len() as u64);
            delta.entries.insert(
                path.clone(),
                ArtifactLocation {
                    offset,
                    len: payload.len() as u64,
                    source_hash: artifact.source_hash.clone(),
                    bytes_read: artifact.bytes_read,
                    parse_error: !artifact.diagnostics.is_empty(),
                },
            );
        }
        store
            .sync_data()
            .map_err(|source| self.io(source, store_path.clone()))?;
        delta.state = artifact_state;

        if manifest.artifact_delta_weights.len() != manifest.artifact_deltas.len() {
            return Ok(false);
        }
        let mut delta_weight = 1u64;
        while let Some(previous_name) = manifest.artifact_deltas.last() {
            let previous_weight = *manifest
                .artifact_delta_weights
                .last()
                .expect("artifact delta weights were validated");
            if previous_weight > delta_weight {
                break;
            }
            let previous_path = self.root.join(self.component_ref_path(previous_name));
            let bytes = self.read_component_ref(previous_name, MAX_COMPONENT_BYTES)?;
            let mut previous: ArtifactIndex =
                bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode {
                    path: previous_path,
                    source,
                })?;
            manifest.artifact_deltas.pop();
            manifest.artifact_delta_weights.pop();
            for tombstone in delta.tombstones {
                previous.entries.remove(&tombstone);
                previous.tombstones.insert(tombstone);
            }
            for (path, location) in delta.entries {
                previous.tombstones.remove(&path);
                previous.entries.insert(path, location);
            }
            previous.overrides.extend(delta.overrides);
            previous.state = artifact_state;
            delta = previous;
            delta_weight = previous_weight.saturating_add(delta_weight);
        }

        let id = snapshot.id.stable_key();
        let delta_name = format!("snapshot-{id}.artifact-delta.bin");
        let delta_bytes = bincode::serialize(&delta).map_err(|source| StorageError::Bincode {
            path: self.root.join(&delta_name),
            source,
        })?;
        if structural_overlay.is_none() {
            atomic_write(&self.root.join(&delta_name), &delta_bytes)
                .map_err(|source| self.io(source, self.root.join(&delta_name)))?;
        }

        let stats_name = format!("snapshot-{id}.stats.json");
        let new_search_name = format!("snapshot-{id}.search");
        let symbol_meta_name = format!("snapshot-{id}.symbol_meta.bin");

        let graph_sidecars = if structural_overlay.is_none() {
            let graph_name = format!("snapshot-{id}.graph.bin");
            let hubs_name = format!("snapshot-{id}.hubs.json");
            let graph = GraphIndex::from_snapshot(snapshot);
            self.atomic_write_bincode(&self.root.join(&graph_name), &graph.as_compact_ref())?;
            let hubs = analysis::precompute_hubs(&graph, 1_000);
            let bytes = serde_json::to_vec(&hubs).map_err(|source| StorageError::Json {
                path: self.root.join(&hubs_name),
                source,
            })?;
            atomic_write(&self.root.join(&hubs_name), &bytes)
                .map_err(|source| self.io(source, self.root.join(&hubs_name)))?;
            Some((graph_name, hubs_name))
        } else {
            None
        };

        let (symbols_name, symbols_checksum, search_name) = if structural_overlay.is_some()
            && !search_terms_changed
            && manifest.symbols.is_some()
            && manifest.symbols_checksum.is_some()
            && manifest
                .search_dir
                .as_ref()
                .is_some_and(|name| self.root.join(name).is_dir())
        {
            (
                manifest.symbols.clone().unwrap(),
                manifest.symbols_checksum.clone().unwrap(),
                manifest.search_dir.clone().unwrap(),
            )
        } else {
            let symbols_name = format!("snapshot-{id}.symbols.bin");
            let dict = SymbolDict::from_snapshot_names_only(snapshot);
            let search_name = self.publish_or_reuse_search_snapshot(&new_search_name, snapshot)?;
            let symbols_checksum =
                self.atomic_write_bincode(&self.root.join(&symbols_name), &dict)?;
            (symbols_name, symbols_checksum, search_name)
        };
        // Stable symbol ids are graph node identities. A structural overlay may shift a
        // declaration span, add a homonym, or move a file even when the unique spelling set is
        // unchanged, so the location/id sidecar must describe this generation.
        let symbol_meta = SymbolMetaDict::from_snapshot(snapshot);
        self.atomic_write_bincode(&self.root.join(&symbol_meta_name), &symbol_meta)?;

        let (bytes, parse_errors) = stats_totals.unwrap_or_else(|| {
            snapshot
                .files
                .values()
                .fold((0, 0), |(bytes, errors), artifact| {
                    (
                        bytes + artifact.bytes_read,
                        errors + usize::from(!artifact.diagnostics.is_empty()),
                    )
                })
        });
        let stats = IndexStats {
            files: snapshot.files.len(),
            edges: snapshot.edges.len(),
            bytes,
            parse_errors,
            snapshot_id: id.clone(),
        };
        let stats_bytes = serde_json::to_vec(&stats).map_err(|source| StorageError::Json {
            path: self.root.join(&stats_name),
            source,
        })?;
        if structural_overlay.is_none() {
            atomic_write(&self.root.join(&stats_name), &stats_bytes)
                .map_err(|source| self.io(source, self.root.join(&stats_name)))?;
        }

        let packed_generation =
            if let Some((graph_overlay, universe_overlay, reverse_overlay)) = structural_overlay {
                structural_publish_failpoint(1, &self.current_path())?;
                let name = self.stage_structural_overlay_pack(
                    &mut manifest,
                    &snapshot.id,
                    graph_overlay,
                    universe_overlay,
                    reverse_overlay,
                    &delta_bytes,
                    &stats_bytes,
                    None,
                    None,
                )?;
                structural_publish_failpoint(2, &self.current_path())?;
                Some(name)
            } else {
                manifest.structural_packs = None;
                None
            };
        manifest.snapshot_id = snapshot.id.clone();
        manifest.payload_snapshot_id = Some(payload_id);
        manifest.base_snapshot_id = None;
        if let Some((graph_name, hubs_name)) = graph_sidecars {
            manifest.graph = Some(graph_name);
            manifest.hubs = Some(hubs_name);
        } else {
            // Hubs are derived from the graph and the base sidecar is stale after an overlay.
            manifest.hubs = None;
        }
        manifest.symbols = Some(symbols_name);
        manifest.symbols_checksum = Some(symbols_checksum);
        manifest.symbol_meta = Some(symbol_meta_name);
        manifest.symbol_meta_overlays.clear();
        manifest.stats = Some(
            packed_generation
                .as_ref()
                .map_or(stats_name, |name| format!("{name}#stats/json")),
        );
        manifest.search_dir = Some(search_name);
        manifest.search_overlays.clear();
        manifest.artifact_deltas.push(
            packed_generation
                .as_ref()
                .map_or(delta_name, |name| format!("{name}#artifact/delta")),
        );
        manifest.artifact_delta_weights.push(delta_weight);
        manifest.artifact_state = Some(artifact_state);
        manifest.artifact_live_bytes = Some(live_bytes);
        let manifest_name = format!("snapshot-{id}.manifest.json");
        let manifest_bytes =
            serde_json::to_vec(&manifest).map_err(|source| StorageError::Json {
                path: self.root.join(&manifest_name),
                source,
            })?;
        atomic_write(&self.root.join(&manifest_name), &manifest_bytes)
            .map_err(|source| self.io(source, self.root.join(&manifest_name)))?;
        structural_publish_failpoint(3, &self.current_path())?;
        atomic_write(&self.current_path(), manifest_name.as_bytes())
            .map_err(|source| self.io(source, self.current_path()))?;
        drop(generation_guard);
        if run_gc {
            self.gc_generations()?;
        }
        Ok(true)
    }

    /// Publish an in-place structural edit without materializing the workspace snapshot or the
    /// global edge vector. Search/name sidecars are reused; callers must use the full path when a
    /// declaration's searchable shape or file membership changes.
    pub(crate) fn publish_resident_structural_delta(
        &self,
        delta: ResidentStructuralDelta<'_>,
    ) -> Result<bool, StorageError> {
        let ResidentStructuralDelta {
            snapshot_id,
            artifacts,
            graph: graph_overlay,
            universe: universe_overlay,
            reverse: reverse_overlay,
            symbol_meta: symbol_meta_overlay,
            search: search_overlay,
            stats,
        } = delta;
        if artifacts.is_empty()
            || stats.snapshot_id != snapshot_id.stable_key()
            || symbol_meta_overlay.snapshot_id != stats.snapshot_id
        {
            return Ok(false);
        }
        let generation_guard = self.acquire_generation_read_guard()?;
        let Some(mut manifest) = self.read_manifest()? else {
            return Ok(false);
        };
        if manifest.schema_version != SCHEMA_VERSION || manifest.structural_packs.is_none() {
            return Ok(false);
        }
        let Some(store_name) = manifest.artifact_store.clone() else {
            return Ok(false);
        };
        let Some(mut artifact_state) = manifest.artifact_state else {
            return Ok(false);
        };
        let component_id = self.component_snapshot_id(&manifest).clone();
        let payload_id = self.payload_snapshot_id(&manifest).clone();
        let store_path = self.root.join(&store_name);
        let mut store = fs::OpenOptions::new()
            .append(true)
            .open(&store_path)
            .map_err(|source| self.io(source, store_path.clone()))?;
        store
            .seek(SeekFrom::End(0))
            .map_err(|source| self.io(source, store_path.clone()))?;
        let mut live_bytes = manifest.artifact_live_bytes.unwrap_or(0);
        let mut delta = ArtifactIndex {
            store: store_name,
            entries: BTreeMap::new(),
            overrides: BTreeSet::new(),
            tombstones: BTreeSet::new(),
            state: artifact_state,
        };
        for (path, artifact) in artifacts {
            let previous = self.current_artifact_location(&manifest, path)?;
            if previous.is_none() && artifact.is_none() {
                continue;
            }
            if let Some(previous) = &previous {
                Self::xor_state(
                    &mut artifact_state,
                    Self::artifact_digest(path, &previous.source_hash),
                );
                live_bytes = live_bytes.saturating_sub(previous.len);
            }
            let Some(artifact) = artifact else {
                delta.entries.remove(path);
                delta.tombstones.insert(path.clone());
                delta.overrides.insert(path.clone());
                continue;
            };
            let payload = bincode::serialize(artifact).map_err(|source| StorageError::Bincode {
                path: store_path.clone(),
                source,
            })?;
            let offset = store
                .stream_position()
                .map_err(|source| self.io(source, store_path.clone()))?;
            store
                .write_all(&payload)
                .map_err(|source| self.io(source, store_path.clone()))?;
            Self::xor_state(
                &mut artifact_state,
                Self::artifact_digest(path, &artifact.source_hash),
            );
            live_bytes = live_bytes.saturating_add(payload.len() as u64);
            delta.tombstones.remove(path);
            delta.overrides.insert(path.clone());
            delta.entries.insert(
                path.clone(),
                ArtifactLocation {
                    offset,
                    len: payload.len() as u64,
                    source_hash: artifact.source_hash.clone(),
                    bytes_read: artifact.bytes_read,
                    parse_error: !artifact.diagnostics.is_empty(),
                },
            );
        }
        store
            .sync_data()
            .map_err(|source| self.io(source, store_path.clone()))?;
        drop(store);
        delta.state = artifact_state;

        if manifest.artifact_delta_weights.len() != manifest.artifact_deltas.len() {
            return Ok(false);
        }
        let mut delta_weight = 1u64;
        while let Some(previous_name) = manifest.artifact_deltas.last() {
            let previous_weight = *manifest
                .artifact_delta_weights
                .last()
                .expect("artifact delta weights were validated");
            if previous_weight > delta_weight {
                break;
            }
            let previous_path = self.root.join(self.component_ref_path(previous_name));
            let bytes = self.read_component_ref(previous_name, MAX_COMPONENT_BYTES)?;
            let mut previous: ArtifactIndex =
                bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode {
                    path: previous_path,
                    source,
                })?;
            manifest.artifact_deltas.pop();
            manifest.artifact_delta_weights.pop();
            for tombstone in delta.tombstones {
                previous.entries.remove(&tombstone);
                previous.tombstones.insert(tombstone);
            }
            for (path, location) in delta.entries {
                previous.tombstones.remove(&path);
                previous.entries.insert(path, location);
            }
            previous.overrides.extend(delta.overrides);
            previous.state = artifact_state;
            delta = previous;
            delta_weight = previous_weight.saturating_add(delta_weight);
        }

        let id = snapshot_id.stable_key();
        let delta_bytes = bincode::serialize(&delta).map_err(|source| StorageError::Bincode {
            path: self.root.join(format!("snapshot-{id}.artifact-delta.bin")),
            source,
        })?;
        let stats_bytes = serde_json::to_vec(stats).map_err(|source| StorageError::Json {
            path: self.root.join(format!("snapshot-{id}.stats.json")),
            source,
        })?;
        structural_publish_failpoint(1, &self.current_path())?;
        let pack_name = self.stage_structural_overlay_pack(
            &mut manifest,
            snapshot_id,
            graph_overlay,
            universe_overlay,
            reverse_overlay,
            &delta_bytes,
            &stats_bytes,
            Some(symbol_meta_overlay),
            search_overlay,
        )?;
        structural_publish_failpoint(2, &self.current_path())?;

        manifest.snapshot_id = snapshot_id.clone();
        manifest.payload_snapshot_id = Some(payload_id);
        manifest.base_snapshot_id = Some(component_id);
        manifest.hubs = None;
        manifest.stats = Some(format!("{pack_name}#stats/json"));
        manifest
            .artifact_deltas
            .push(format!("{pack_name}#artifact/delta"));
        manifest.artifact_delta_weights.push(delta_weight);
        manifest
            .symbol_meta_overlays
            .push(format!("{pack_name}#symbol-meta/overlay"));
        if search_overlay.is_some() {
            manifest
                .search_overlays
                .push(format!("{pack_name}#search/overlay"));
        }
        manifest.artifact_state = Some(artifact_state);
        manifest.artifact_live_bytes = Some(live_bytes);
        let manifest_name = format!("snapshot-{id}.manifest.json");
        let manifest_path = self.root.join(&manifest_name);
        let manifest_bytes =
            serde_json::to_vec(&manifest).map_err(|source| StorageError::Json {
                path: manifest_path.clone(),
                source,
            })?;
        atomic_write(&manifest_path, &manifest_bytes)
            .map_err(|source| self.io(source, manifest_path))?;
        structural_publish_failpoint(3, &self.current_path())?;
        atomic_write(&self.current_path(), manifest_name.as_bytes())
            .map_err(|source| self.io(source, self.current_path()))?;
        drop(generation_guard);
        Ok(true)
    }

    /// Rewrite the current append-only artifact store when its physical/live byte ratio reaches
    /// the configured bound. The exclusive artifact lock protects readers across manifest and
    /// payload dereferences. Global graph/search/symbol sidecars are deliberately outside this
    /// GC scope.
    pub fn compact_artifacts_if_amplified(
        &self,
        max_amplification: u32,
        retention: usize,
    ) -> Result<bool, StorageError> {
        // Cheap rejection stays off the reader barrier and does not hydrate index deltas.
        let preliminary_guard = self.acquire_generation_read_guard()?;
        let Some(preliminary) = self.read_manifest()? else {
            return Ok(false);
        };
        let Some(preliminary_store) = preliminary.artifact_store.as_ref() else {
            return Ok(false);
        };
        let Some(preliminary_live) = preliminary.artifact_live_bytes else {
            return Ok(false);
        };
        let preliminary_physical = fs::metadata(self.root.join(preliminary_store))
            .map_err(|source| self.io(source, self.root.join(preliminary_store)))?
            .len();
        if preliminary_live == 0
            || preliminary_physical
                < preliminary_live.saturating_mul(u64::from(max_amplification.max(1)))
        {
            return Ok(false);
        }
        drop(preliminary_guard);
        self.compact_artifacts_if_amplified_locked(max_amplification, retention)
    }

    fn compact_artifacts_if_amplified_locked(
        &self,
        max_amplification: u32,
        retention: usize,
    ) -> Result<bool, StorageError> {
        let Some(_generation_guard) =
            crate::generation_gc::GenerationGuard::try_exclusive(&self.root)
                .map_err(|source| self.io(source, crate::generation_gc::lock_path(&self.root)))?
        else {
            return Ok(false);
        };
        let _gc_guard = self.acquire_artifact_gc_lock()?;
        let Some(mut manifest) = self.read_manifest()? else {
            return Ok(false);
        };
        let Some(index) = self.read_artifact_index(&manifest)? else {
            return Ok(false);
        };
        let Some(old_store) = manifest.artifact_store.clone() else {
            return Ok(false);
        };
        let physical = fs::metadata(self.root.join(&old_store))
            .map_err(|source| self.io(source, self.root.join(&old_store)))?
            .len();
        let live = index
            .entries
            .values()
            .map(|location| location.len)
            .sum::<u64>();
        let ratio = u64::from(max_amplification.max(1));
        if live == 0 || physical < live.saturating_mul(ratio) {
            return Ok(false);
        }

        let mut files = BTreeMap::new();
        for (path, location) in &index.entries {
            files.insert(path.clone(), self.read_artifact_at(&index, location)?);
        }
        let compact_snapshot = IndexSnapshot {
            id: manifest.snapshot_id.clone(),
            files,
            edges: Vec::new(),
        };
        let generation = manifest.snapshot_id.stable_key();
        let (artifact_index, artifact_store, artifact_locator, artifact_state) =
            self.write_initial_artifact_store(&format!("{generation}-compact"), &compact_snapshot)?;
        manifest.artifact_index = Some(artifact_index);
        manifest.artifact_store = Some(artifact_store);
        manifest.artifact_locator = Some(artifact_locator);
        manifest.artifact_state = Some(artifact_state);
        manifest.artifact_live_bytes = Some(live);
        manifest.artifact_deltas.clear();
        manifest.artifact_delta_weights.clear();

        let current_name = fs::read_to_string(self.current_path())
            .map_err(|source| self.io(source, self.current_path()))?;
        let manifest_path = self.root.join(current_name.trim());
        let bytes = serde_json::to_vec(&manifest).map_err(|source| StorageError::Json {
            path: manifest_path.clone(),
            source,
        })?;
        atomic_write(&manifest_path, &bytes)
            .map_err(|source| self.io(source, manifest_path.clone()))?;

        self.gc_unretained_artifact_generations(retention, &current_name)?;

        Ok(true)
    }

    fn gc_unretained_artifact_generations(
        &self,
        retention: usize,
        current_name: &str,
    ) -> Result<(), StorageError> {
        let mut generations = Vec::new();
        for entry in
            fs::read_dir(&self.root).map_err(|source| self.io(source, self.root.clone()))?
        {
            let entry = entry.map_err(|source| self.io(source, self.root.clone()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".manifest.json") {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(|source| self.io(source, entry.path()))?;
            let manifest: Manifest =
                serde_json::from_slice(&bytes).map_err(|source| StorageError::Json {
                    path: entry.path(),
                    source,
                })?;
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            generations.push((name, entry.path(), modified, manifest));
        }
        generations.sort_by(|left, right| {
            let left_current = left.0 == current_name.trim();
            let right_current = right.0 == current_name.trim();
            right_current
                .cmp(&left_current)
                .then_with(|| right.2.cmp(&left.2))
                .then_with(|| right.0.cmp(&left.0))
        });

        let keep = retention.max(1);
        let artifact_refs = |manifest: &Manifest| {
            manifest
                .artifact_deltas
                .iter()
                .chain(manifest.artifact_index.iter())
                .chain(manifest.artifact_locator.iter())
                .chain(manifest.artifact_store.iter())
                .cloned()
                .collect::<BTreeSet<_>>()
        };
        let retained: BTreeSet<String> = generations
            .iter()
            .take(keep)
            .flat_map(|(_, _, _, manifest)| artifact_refs(manifest))
            .collect();
        let candidates: BTreeSet<String> = generations
            .iter()
            .skip(keep)
            .flat_map(|(_, _, _, manifest)| artifact_refs(manifest))
            .collect();
        for name in candidates.difference(&retained) {
            let path = self.root.join(name);
            if path.is_file() {
                fs::remove_file(&path).map_err(|source| self.io(source, path))?;
            }
        }
        for (_, path, _, _) in generations.into_iter().skip(keep) {
            fs::remove_file(&path).map_err(|source| self.io(source, path))?;
        }
        Ok(())
    }

    fn ensure_supported_schema(&self, manifest: &Manifest) -> Result<(), StorageError> {
        if manifest.schema_version == SCHEMA_VERSION {
            return Ok(());
        }
        Err(StorageError::Invalid {
            path: self.current_path(),
            message: format!(
                "unsupported schema {} (expected {SCHEMA_VERSION}); run `ravel index` to rebuild",
                manifest.schema_version
            ),
        })
    }

    /// Fast path: load precomputed stats without deserializing the full snapshot.
    pub fn open_stats(&self) -> Result<Option<IndexStats>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.ensure_supported_schema(&manifest)?;
        self.open_stats_from_manifest(&manifest)
    }

    /// Fast path: load prebuilt compact graph without full snapshot / adjacency rebuild.
    pub fn open_graph(&self) -> Result<Option<GraphIndex>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.ensure_supported_schema(&manifest)?;
        if let Some(chain) = manifest
            .structural_packs
            .as_ref()
            .filter(|chain| chain.current_snapshot == manifest.snapshot_id.stable_key())
        {
            if let Some(graph_name) = manifest.graph.as_ref() {
                let path = self.root.join(self.component_ref_path(graph_name));
                let payload = self.read_component_ref(graph_name, MAX_COMPACT_GRAPH_BYTES)?;
                let compact: CompactGraph = bincode::deserialize(&payload)
                    .map_err(|source| StorageError::Bincode { path, source })?;
                if compact.snapshot_id == self.component_snapshot_id(&manifest).stable_key() {
                    let mut graph = GraphIndex::from_compact(compact);
                    let edge_count = self
                        .open_stats_from_manifest(&manifest)?
                        .map_or_else(|| graph.edge_count(), |stats| stats.edges);
                    for overlay_name in &chain.overlays {
                        let overlay_path = self.root.join(overlay_name);
                        let mut reader =
                            GenerationPackReader::open(&overlay_path).map_err(|error| {
                                StorageError::Invalid {
                                    path: overlay_path.clone(),
                                    message: error.to_string(),
                                }
                            })?;
                        let Some(bytes) = reader
                            .read("graph/overlay-v2", MAX_DELTA_COMPONENT_BYTES)
                            .map_err(|error| StorageError::Invalid {
                                path: overlay_path.clone(),
                                message: error.to_string(),
                            })?
                        else {
                            return Ok(None);
                        };
                        let overlay: IncrementalGraphOverlay = bincode::deserialize(&bytes)
                            .map_err(|source| StorageError::Bincode {
                                path: overlay_path,
                                source,
                            })?;
                        graph.apply_incremental_overlay(
                            &overlay,
                            &manifest.snapshot_id.stable_key(),
                            edge_count,
                        );
                    }
                    graph.finish_incremental_overlays();
                    return Ok(Some(graph));
                }
            }
            if let Some(state) = self.open_structural_graph_base()? {
                return Ok(Some(GraphIndex::from_edges(
                    &state.edges(),
                    manifest.snapshot_id.stable_key(),
                )));
            }
        }
        let Some(graph_name) = manifest.graph.as_ref() else {
            return Ok(None);
        };
        let path = self.root.join(graph_name);
        if !path.is_file() {
            return Ok(None);
        }
        let payload = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        let compact: CompactGraph = bincode::deserialize(&payload)
            .map_err(|source| StorageError::Bincode { path, source })?;
        if compact.snapshot_id != self.component_snapshot_id(&manifest).stable_key() {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "graph snapshot id mismatch".into(),
            });
        }
        Ok(Some(GraphIndex::from_compact(compact)))
    }

    /// Fast path: symbol dictionary for cold search.
    pub fn open_symbols(&self) -> Result<Option<SymbolDict>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.ensure_supported_schema(&manifest)?;
        let Some(symbols_name) = manifest.symbols.as_ref() else {
            return Ok(None);
        };
        let path = self.root.join(symbols_name);
        if !path.is_file() {
            return Ok(None);
        }
        let payload = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        // Hot cold-search path: skip the full-payload blake3 (consistent with `open_graph` /
        // `open_current`, which also skip it). `validate` still verifies the checksum.
        let dict = bincode::deserialize::<SymbolDict>(&payload).map_err(|source| {
            StorageError::Bincode {
                path: path.clone(),
                source,
            }
        })?;
        if dict.snapshot_id != self.component_snapshot_id(&manifest).stable_key() {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "symbols snapshot id mismatch".into(),
            });
        }
        if dict.format_version != SymbolDict::FORMAT_VERSION || !dict.is_well_formed() {
            return Err(StorageError::Invalid {
                path,
                message: "invalid symbol dictionary format".into(),
            });
        }
        Ok(Some(dict))
    }

    /// Optional on-disk Tantivy directory for fuzzy/regex hybrid path.
    pub fn open_search_dir(&self) -> Result<Option<PathBuf>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.ensure_supported_schema(&manifest)?;
        let Some(name) = manifest.search_dir.as_ref() else {
            return Ok(None);
        };
        let path = self.root.join(name);
        if path.is_dir() {
            Ok(Some(path))
        } else {
            Ok(None)
        }
    }

    /// Open dictionary and optional Tantivy reader from one manifest while retaining the
    /// generation lease for the entire on-disk reader lifetime.
    pub fn open_search_index(
        &self,
        needs_tantivy: bool,
    ) -> Result<Option<SearchIndex>, StorageError> {
        let generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.ensure_supported_schema(&manifest)?;
        let overlays = manifest
            .search_overlays
            .iter()
            .map(|reference| {
                let path = self.root.join(self.component_ref_path(reference));
                let bytes = self.read_component_ref(reference, MAX_DELTA_COMPONENT_BYTES)?;
                let overlay: SearchTermOverlay = bincode::deserialize(&bytes)
                    .map_err(|source| StorageError::Bincode { path, source })?;
                Ok(overlay)
            })
            .collect::<Result<Vec<_>, StorageError>>()?;
        let mut dict = self.symbols_from_manifest(&manifest)?;
        if let Some(dict) = dict.as_mut() {
            dict.apply_name_overlays(&overlays);
        }
        let search_path = manifest
            .search_dir
            .as_ref()
            .map(|name| self.root.join(name))
            .filter(|path| path.is_dir());
        let index = match (dict, needs_tantivy, search_path) {
            (Some(dict), true, Some(path)) => SearchIndex::with_dict_and_tantivy_dir(dict, &path),
            (Some(dict), _, _) => Ok(SearchIndex::from_symbol_dict(dict)),
            (None, true, Some(path)) => SearchIndex::open_tantivy_dir(&path),
            (None, _, _) => return Ok(None),
        }
        .map_err(|error| StorageError::Search {
            path: self.root.clone(),
            message: error.to_string(),
        })?;
        Ok(Some(
            index
                .with_term_overlays(overlays)
                .with_generation_guard(generation_guard),
        ))
    }

    fn symbols_from_manifest(
        &self,
        manifest: &Manifest,
    ) -> Result<Option<SymbolDict>, StorageError> {
        let Some(symbols_name) = manifest.symbols.as_ref() else {
            return Ok(None);
        };
        let path = self.root.join(symbols_name);
        if !path.is_file() {
            return Ok(None);
        }
        let payload = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        let dict = bincode::deserialize::<SymbolDict>(&payload).map_err(|source| {
            StorageError::Bincode {
                path: path.clone(),
                source,
            }
        })?;
        if dict.snapshot_id != self.component_snapshot_id(manifest).stable_key()
            || dict.format_version != SymbolDict::FORMAT_VERSION
            || !dict.is_well_formed()
        {
            return Ok(None);
        }
        Ok(Some(dict))
    }

    pub fn open_symbol_meta(&self) -> Result<Option<SymbolMetaDict>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.ensure_supported_schema(&manifest)?;
        let Some(name) = manifest.symbol_meta.as_ref() else {
            return Ok(None);
        };
        let payload = self.read_component_ref(name, MAX_COMPONENT_BYTES)?;
        // Stale sidecars (schema drift) → treat as missing; caller falls back or reindexes.
        let Ok(mut meta) = bincode::deserialize::<SymbolMetaDict>(&payload) else {
            return Ok(None);
        };
        if meta.format_version != SymbolMetaDict::FORMAT_VERSION {
            return Ok(None);
        }
        if !meta.is_well_formed() {
            return Ok(None);
        }
        if meta.snapshot_id != self.component_snapshot_id(&manifest).stable_key() {
            return Ok(None);
        }
        let mut overlays = Vec::with_capacity(manifest.symbol_meta_overlays.len());
        for overlay_name in &manifest.symbol_meta_overlays {
            let overlay_path = self.root.join(self.component_ref_path(overlay_name));
            let bytes = self.read_component_ref(overlay_name, MAX_DELTA_COMPONENT_BYTES)?;
            let overlay: SymbolMetaOverlay =
                bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode {
                    path: overlay_path,
                    source,
                })?;
            overlays.push(overlay);
        }
        meta.apply_overlays(overlays);
        let expected_snapshot = if manifest.symbol_meta_overlays.is_empty() {
            self.component_snapshot_id(&manifest).stable_key()
        } else {
            manifest.snapshot_id.stable_key()
        };
        if meta.snapshot_id != expected_snapshot {
            return Ok(None);
        }
        Ok(Some(meta))
    }

    pub fn open_file_list(&self) -> Result<Option<FileList>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.ensure_supported_schema(&manifest)?;
        if let Some(index) = self.read_artifact_index(&manifest)? {
            return Ok(Some(FileList {
                format_version: FileList::FORMAT_VERSION,
                snapshot_id: manifest.snapshot_id.stable_key(),
                paths: index.entries.into_keys().collect(),
            }));
        }
        let Some(name) = manifest.files.as_ref() else {
            return Ok(None);
        };
        let path = self.root.join(name);
        if !path.is_file() {
            return Ok(None);
        }
        let payload = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        let list: FileList = bincode::deserialize(&payload)
            .map_err(|source| StorageError::Bincode { path, source })?;
        if list.snapshot_id != self.component_snapshot_id(&manifest).stable_key() {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "files list snapshot id mismatch".into(),
            });
        }
        Ok(Some(list))
    }

    /// Path → source_hash sidecar (~small) for auto-sync without hydrating full snapshot.
    pub fn open_file_hashes(&self) -> Result<Option<FileHashIndex>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let _read_guard = self.acquire_artifact_read_lock()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.ensure_supported_schema(&manifest)?;
        if let Some(index) = self.read_artifact_index(&manifest)? {
            let (paths, hashes): (Vec<_>, Vec<_>) = index
                .entries
                .into_iter()
                .map(|(path, location)| (path, location.source_hash))
                .unzip();
            return Ok(Some(FileHashIndex {
                format_version: FileHashIndex::FORMAT_VERSION,
                snapshot_id: manifest.snapshot_id.stable_key(),
                paths,
                hashes,
            }));
        }
        let Some(name) = manifest.file_hashes.as_ref() else {
            return Ok(None);
        };
        let path = self.root.join(name);
        if !path.is_file() {
            return Ok(None);
        }
        let payload = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        let idx: FileHashIndex = bincode::deserialize(&payload)
            .map_err(|source| StorageError::Bincode { path, source })?;
        if idx.snapshot_id != self.component_snapshot_id(&manifest).stable_key() {
            return Ok(None);
        }
        Ok(Some(idx))
    }

    /// Batch point lookup for changed paths without materializing the full path/hash universe.
    pub fn source_hashes_for_paths(
        &self,
        paths: &[String],
    ) -> Result<BTreeMap<String, Option<String>>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let _artifact_guard = self.acquire_artifact_read_lock()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(paths.iter().cloned().map(|path| (path, None)).collect());
        };
        let mut hashes = BTreeMap::new();
        let mut unresolved: BTreeSet<_> = paths.iter().cloned().collect();
        for delta_name in manifest.artifact_deltas.iter().rev() {
            if unresolved.is_empty() {
                break;
            }
            let delta_path = self.root.join(self.component_ref_path(delta_name));
            let bytes = self.read_component_ref(delta_name, MAX_COMPONENT_BYTES)?;
            let delta: ArtifactIndex =
                bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode {
                    path: delta_path,
                    source,
                })?;
            let requested = unresolved.iter().cloned().collect::<Vec<_>>();
            for path in requested {
                if delta.tombstones.contains(&path) {
                    unresolved.remove(&path);
                    hashes.insert(path, None);
                } else if let Some(location) = delta.entries.get(&path) {
                    unresolved.remove(&path);
                    hashes.insert(path, Some(location.source_hash.clone()));
                }
            }
        }
        for path in unresolved {
            let hash = self
                .base_artifact_location(&manifest, &path)?
                .map(|location| location.source_hash);
            hashes.insert(path, hash);
        }
        Ok(hashes)
    }

    /// Precomputed top hubs — O(1) open + O(k) for large graphs.
    pub fn open_hubs(&self) -> Result<Option<Vec<HubEntry>>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.ensure_supported_schema(&manifest)?;
        let Some(name) = manifest.hubs.as_ref() else {
            return Ok(None);
        };
        let path = self.root.join(name);
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        let hubs: Vec<HubEntry> =
            serde_json::from_slice(&bytes).map_err(|source| StorageError::Json { path, source })?;
        Ok(Some(hubs))
    }

    fn open_payload(
        &self,
        path: PathBuf,
        verify_checksum: bool,
        expected: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let payload = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        if verify_checksum && blake3::hash(&payload).to_hex().as_str() != expected {
            return Err(StorageError::Invalid {
                path,
                message: "checksum mismatch".into(),
            });
        }
        Ok(payload)
    }

    fn atomic_write_bincode<T: serde::Serialize + ?Sized>(
        &self,
        path: &Path,
        value: &T,
    ) -> Result<String, StorageError> {
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        let file = fs::File::create(&tmp).map_err(|source| self.io(source, tmp.clone()))?;
        let mut writer = HashingWriter {
            inner: io::BufWriter::new(file),
            hasher: blake3::Hasher::new(),
        };
        bincode::serialize_into(&mut writer, value).map_err(|source| StorageError::Bincode {
            path: path.to_path_buf(),
            source,
        })?;
        writer
            .inner
            .flush()
            .map_err(|source| self.io(source, tmp.clone()))?;
        writer
            .inner
            .get_ref()
            .sync_all()
            .map_err(|source| self.io(source, tmp.clone()))?;
        let checksum = writer.hasher.finalize().to_hex().to_string();
        drop(writer);
        atomic_replace(&tmp, path).map_err(|source| self.io(source, path.to_path_buf()))?;
        Ok(checksum)
    }
}

struct HashingWriter<W> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.hasher.update(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl SnapshotStorage for FileSnapshotStorage {
    fn publish(&self, snapshot: &IndexSnapshot) -> Result<(), StorageError> {
        fs::create_dir_all(&self.root).map_err(|source| self.io(source, self.root.clone()))?;
        let generation_guard = self.acquire_generation_read_guard()?;
        let id = snapshot.id.stable_key();
        let payload_name = format!("snapshot-{id}.bin");
        let graph_name = format!("snapshot-{id}.graph.bin");
        let stats_name = format!("snapshot-{id}.stats.json");
        let symbols_name = format!("snapshot-{id}.symbols.bin");
        let new_search_name = format!("snapshot-{id}.search");
        let symbol_meta_name = format!("snapshot-{id}.symbol_meta.bin");
        let files_name = format!("snapshot-{id}.files.bin");
        let file_hashes_name = format!("snapshot-{id}.file_hashes.bin");
        let hubs_name = format!("snapshot-{id}.hubs.json");
        let manifest_name = format!("snapshot-{id}.manifest.json");

        // Every large sidecar derives independently from `snapshot` and writes to a distinct
        // atomic temp path (temp names are per-final-path), so they publish concurrently instead
        // of serially. Wall time collapses from the serial sum (payload+graph+search+symbols+
        // artifacts, ~30s on the real corpus) toward the slowest single sidecar (Tantivy search).
        // Each closure streams straight to its temp file, so the RSS win of not buffering every
        // serialized Vec is preserved. Manifest assembly below joins their results.
        let payload_task = || -> Result<String, StorageError> {
            let t = std::time::Instant::now();
            let checksum = self.atomic_write_bincode(&self.payload_path(&id), snapshot)?;
            crate::timing::stage("publish.payload", t, || {
                format!(
                    "files={} edges={}",
                    snapshot.files.len(),
                    snapshot.edges.len()
                )
            });
            Ok(checksum)
        };
        // Prebuild compact graph once at index time so cold CLI queries skip rebuild, then derive
        // top-k hubs from it (online hubs must not be O(V) on 1B-node graphs).
        let graph_task = || -> Result<(), StorageError> {
            let t = std::time::Instant::now();
            let graph = GraphIndex::from_snapshot(snapshot);
            self.atomic_write_bincode(&self.graph_path(&id), &graph.as_compact_ref())?;
            crate::timing::stage("publish.graph", t, String::new);
            let t = std::time::Instant::now();
            let hubs = analysis::precompute_hubs(&graph, 1_000);
            let hubs_bytes = serde_json::to_vec(&hubs).map_err(|source| StorageError::Json {
                path: self.hubs_path(&id),
                source,
            })?;
            atomic_write(&self.hubs_path(&id), &hubs_bytes)
                .map_err(|source| self.io(source, self.hubs_path(&id)))?;
            drop(graph);
            crate::timing::stage("publish.hubs", t, String::new);
            Ok(())
        };
        let search_task = || -> Result<String, StorageError> {
            let t = std::time::Instant::now();
            let name = self.publish_or_reuse_search_snapshot(&new_search_name, snapshot)?;
            crate::timing::stage("publish.search", t, String::new);
            Ok(name)
        };
        let symbols_task = || -> Result<String, StorageError> {
            let dict = SymbolDict::from_snapshot_names_only(snapshot);
            self.atomic_write_bincode(&self.symbols_path(&id), &dict)
        };
        let symbol_meta_task = || -> Result<(), StorageError> {
            let symbol_meta = SymbolMetaDict::from_snapshot(snapshot);
            self.atomic_write_bincode(&self.symbol_meta_path(&id), &symbol_meta)?;
            Ok(())
        };
        let file_list_task = || -> Result<(), StorageError> {
            let file_list = FileList::from_snapshot(snapshot);
            self.atomic_write_bincode(&self.files_path(&id), &file_list)?;
            Ok(())
        };
        let file_hashes_task = || -> Result<(), StorageError> {
            let file_hashes = FileHashIndex::from_snapshot(snapshot);
            self.atomic_write_bincode(&self.root.join(&file_hashes_name), &file_hashes)?;
            Ok(())
        };
        let artifact_task = || {
            let t = std::time::Instant::now();
            let out = self.write_initial_artifact_store(&id, snapshot);
            crate::timing::stage("publish.artifact_store", t, String::new);
            out
        };

        // Three lanes, not a full fan-out. Measured on the real corpus: with tokenization
        // parallelized the search build drops to a few seconds, so the serial heavy-sidecar
        // chain (~10s) is the wall-clock ceiling either way — an 8-way fan-out was no faster
        // than this shape but held GraphIndex, SymbolDict, SymbolMetaDict, and the search prep
        // buffers live simultaneously (+1.8 GB peak RSS). The chain keeps the original
        // build→write→drop discipline so at most one heavy sidecar is resident at a time;
        // payload and search stream to their own files and add little residency. Errors
        // surface after the join so every started write finishes before we bail.
        // (index name, store name, locator name, state) as returned by write_initial_artifact_store.
        type ArtifactStoreParts = (String, String, String, [u8; 32]);
        let chain_task = || -> Result<(String, ArtifactStoreParts), StorageError> {
            graph_task()?;
            let symbols_checksum = symbols_task()?;
            symbol_meta_task()?;
            file_list_task()?;
            file_hashes_task()?;
            let artifact_out = artifact_task()?;
            Ok((symbols_checksum, artifact_out))
        };
        let (checksum, (search_name, chain_out)) =
            rayon::join(payload_task, || rayon::join(search_task, chain_task));
        let checksum = checksum?;
        let search_name = search_name?;
        let (
            symbols_checksum,
            (artifact_index_name, artifact_store, artifact_locator, artifact_state),
        ) = chain_out?;

        // Single pass over files for both byte and parse-error totals.
        let (bytes, parse_errors) = snapshot.files.values().fold((0u64, 0usize), |(b, e), a| {
            (b + a.bytes_read, e + usize::from(!a.diagnostics.is_empty()))
        });
        let stats = IndexStats {
            files: snapshot.files.len(),
            edges: snapshot.edges.len(),
            bytes,
            parse_errors,
            snapshot_id: id.clone(),
        };
        let stats_bytes = serde_json::to_vec(&stats).map_err(|source| StorageError::Json {
            path: self.stats_path(&id),
            source,
        })?;
        atomic_write(&self.stats_path(&id), &stats_bytes)
            .map_err(|source| self.io(source, self.stats_path(&id)))?;

        let artifact_live_bytes = fs::metadata(self.root.join(&artifact_store))
            .map_err(|source| self.io(source, self.root.join(&artifact_store)))?
            .len();

        let manifest = Manifest {
            snapshot_id: snapshot.id.clone(),
            payload_snapshot_id: None,
            base_snapshot_id: None,
            schema_version: SCHEMA_VERSION,
            checksum,
            payload: payload_name,
            graph: Some(graph_name),
            stats: Some(stats_name),
            symbols: Some(symbols_name),
            symbols_checksum: Some(symbols_checksum),
            search_dir: Some(search_name),
            search_overlays: Vec::new(),
            symbol_meta: Some(symbol_meta_name),
            symbol_meta_overlays: Vec::new(),
            files: Some(files_name),
            file_hashes: Some(file_hashes_name),
            hubs: Some(hubs_name),
            artifact_index: Some(artifact_index_name),
            artifact_deltas: Vec::new(),
            artifact_delta_weights: Vec::new(),
            artifact_store: Some(artifact_store),
            artifact_locator: Some(artifact_locator),
            artifact_state: Some(artifact_state),
            artifact_live_bytes: Some(artifact_live_bytes),
            structural_packs: None,
        };
        let manifest_bytes =
            serde_json::to_vec(&manifest).map_err(|source| StorageError::Json {
                path: self.manifest_path(&id),
                source,
            })?;
        atomic_write(&self.manifest_path(&id), &manifest_bytes)
            .map_err(|source| self.io(source, self.manifest_path(&id)))?;
        atomic_write(&self.current_path(), manifest_name.as_bytes())
            .map_err(|source| self.io(source, self.current_path()))?;
        drop(generation_guard);
        self.gc_generations()?;
        Ok(())
    }

    fn open_current(&self) -> Result<Option<IndexSnapshot>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let _read_guard = self.acquire_artifact_read_lock()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        if manifest.schema_version != SCHEMA_VERSION {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: format!(
                    "unsupported schema {} (expected {SCHEMA_VERSION}); run `ravel index` to rebuild",
                    manifest.schema_version
                ),
            });
        }
        let path = self.root.join(&manifest.payload);
        // Hot path: skip full blake3 of the payload (validate still checks).
        let payload = self.open_payload(path.clone(), false, &manifest.checksum)?;
        let mut snapshot: IndexSnapshot = bincode::deserialize(&payload)
            .map_err(|source| StorageError::Bincode { path, source })?;
        if snapshot.id != *self.payload_snapshot_id(&manifest) {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "manifest snapshot id mismatch".into(),
            });
        }
        if let Some(index) = self.read_artifact_index(&manifest)? {
            for path in &index.overrides {
                if index.tombstones.contains(path) {
                    snapshot.files.remove(path);
                    continue;
                }
                let location = index
                    .entries
                    .get(path)
                    .ok_or_else(|| StorageError::Invalid {
                        path: self.current_path(),
                        message: format!("missing artifact location for {path}"),
                    })?;
                snapshot
                    .files
                    .insert(path.clone(), self.read_artifact_at(&index, location)?);
            }
        }
        if manifest
            .structural_packs
            .as_ref()
            .is_some_and(|chain| chain.current_snapshot == manifest.snapshot_id.stable_key())
            && let Some(graph) = self.open_structural_graph_base()?
        {
            // The incremental writer deliberately avoids rebuilding the global edge vector.
            // Materialize it only when the full-snapshot API explicitly asks for it.
            snapshot.edges = graph.edges();
        }
        snapshot.id = manifest.snapshot_id;
        Ok(Some(snapshot))
    }

    fn validate(&self) -> Result<(), StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(());
        };
        if manifest.schema_version != SCHEMA_VERSION {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: format!(
                    "unsupported schema {} (expected {SCHEMA_VERSION}); run `ravel index` to rebuild",
                    manifest.schema_version
                ),
            });
        }
        let path = self.root.join(&manifest.payload);
        let payload = self.open_payload(path.clone(), true, &manifest.checksum)?;
        let snapshot: IndexSnapshot = bincode::deserialize(&payload)
            .map_err(|source| StorageError::Bincode { path, source })?;
        if snapshot.id != *self.payload_snapshot_id(&manifest) {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "manifest snapshot id mismatch".into(),
            });
        }
        if let Some(graph_name) = manifest.graph.as_ref() {
            let gpath = self.root.join(graph_name);
            if gpath.is_file() {
                let bytes = fs::read(&gpath).map_err(|source| self.io(source, gpath.clone()))?;
                let compact: CompactGraph =
                    bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode {
                        path: gpath,
                        source,
                    })?;
                if compact.snapshot_id != self.component_snapshot_id(&manifest).stable_key() {
                    return Err(StorageError::Invalid {
                        path: self.current_path(),
                        message: "graph snapshot id mismatch".into(),
                    });
                }
            }
        }
        if let Some(symbols_name) = manifest.symbols.as_ref() {
            let spath = self.root.join(symbols_name);
            if spath.is_file() {
                let bytes = fs::read(&spath).map_err(|source| self.io(source, spath.clone()))?;
                if let Some(expected) = manifest.symbols_checksum.as_ref() {
                    if blake3::hash(&bytes).to_hex().as_str() != expected.as_str() {
                        return Err(StorageError::Invalid {
                            path: spath,
                            message: "symbols checksum mismatch".into(),
                        });
                    }
                }
                let dict: SymbolDict =
                    bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode {
                        path: spath,
                        source,
                    })?;
                if dict.snapshot_id != self.component_snapshot_id(&manifest).stable_key() {
                    return Err(StorageError::Invalid {
                        path: self.current_path(),
                        message: "symbols snapshot id mismatch".into(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Serialize shards in parallel (bincode is pure CPU) and append them to the streaming pack in
/// bounded chunks: the writer stays single-threaded and only one chunk of serialized bytes is
/// resident at a time, so the RSS ceiling is unchanged from the old serial loop. Pack records are
/// key-addressed, so append order does not affect reads.
fn add_shards_parallel<V: serde::Serialize + Sync>(
    writer: &mut StreamingGenerationPackWriter,
    path: &Path,
    prefix: &str,
    shards: std::collections::BTreeMap<u16, V>,
) -> Result<(), StorageError> {
    use rayon::prelude::*;
    const SHARD_SERIALIZE_CHUNK: usize = 256;
    let shards: Vec<(u16, V)> = shards.into_iter().collect();
    for chunk in shards.chunks(SHARD_SERIALIZE_CHUNK) {
        let serialized: Vec<(String, Vec<u8>)> = chunk
            .par_iter()
            .map(|(id, shard)| {
                bincode::serialize(shard)
                    .map(|bytes| (format!("{prefix}{id:04x}"), bytes))
                    .map_err(|source| StorageError::Bincode {
                        path: path.to_path_buf(),
                        source,
                    })
            })
            .collect::<Result<_, _>>()?;
        for (key, bytes) in serialized {
            writer
                .add(key, bytes)
                .map_err(|error| StorageError::Invalid {
                    path: path.to_path_buf(),
                    message: error.to_string(),
                })?;
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    // fsync the temp file before renaming so a crash can never expose a renamed-but-unflushed
    // (truncated/garbage) sidecar — the rename itself is atomic on POSIX.
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    atomic_replace(&tmp, path)?;
    sync_parent_directory(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IndexSnapshot, SnapshotId};
    use std::collections::BTreeMap;
    use std::time::Instant;
    use tempfile::tempdir;

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("CURRENT");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"second");
    }

    /// Hub graph: many files each contribute one edge into the same target node, so the hub's
    /// reverse adjacency map is large enough to force the per-neighbor delta encoding.
    fn hub_graph_edges(importers: usize, extra_target: &str) -> Vec<crate::model::Edge> {
        let mut edges = Vec::new();
        let make = |from: &str, to: &str, source: &str| crate::model::Edge {
            from: from.into(),
            to: to.into(),
            kind: crate::model::EdgeKind::Import,
            confidence: crate::model::EdgeConfidence::Resolved {
                score: 1.0,
                reason: "test".into(),
            },
            type_only: false,
            source_path: Some(source.into()),
            span: None,
            provenance: crate::model::EdgeProvenance::Resolution,
        };
        for index in 0..importers {
            let from = format!("src/imp{index}.ts");
            edges.push(make(&from, "src/hub.ts", &from));
        }
        edges.push(make("src/extra.ts", "src/hub.ts", "src/extra.ts"));
        edges.push(make("src/extra.ts", extra_target, "src/extra.ts"));
        edges
    }

    /// Replay a published overlay onto persisted section shards and reconstruct the full state,
    /// asserting all three sections agree with a from-scratch rebuild: the file section replays to
    /// the exact ownership (from which refcounts/adjacency derive), the edge section holds the
    /// matching per-digest refcounts with no stale keys, and the adjacency section matches too.
    fn replay_sections(
        pre: crate::incremental_graph::GraphSectionShards,
        overlay: &IncrementalGraphOverlay,
        bits: u8,
    ) -> IncrementalGraphState {
        let partitioned = partition_graph_overlay_sections(overlay, bits, bits, bits).unwrap();
        let mut files = pre.files;
        for (id, section) in &partitioned.files {
            apply_file_overlay_to_shard(files.entry(*id).or_default(), section);
        }
        let mut edges = pre.edges;
        for (id, section) in &partitioned.edges {
            apply_edge_overlay_to_shard(edges.entry(*id).or_default(), section);
        }
        let mut adjacency = pre.adjacency;
        for (id, section) in &partitioned.adjacency {
            apply_adj_overlay_to_shard(adjacency.entry(*id).or_default(), section);
        }
        // (a) The file section alone reconstructs the whole state.
        let mut by_file = BTreeMap::new();
        for shard in files.into_values() {
            by_file.extend(shard.by_file);
        }
        let state = IncrementalGraphState::from_owned_by_file(by_file);
        // (b) The edge section holds exactly the reconstructed refcounts, keyed by digest.
        for (edge, count) in &state.edge_refcounts {
            let digest = owned_edge_digest(edge).unwrap();
            let id = digest_shard_id(digest, bits);
            assert_eq!(
                edges
                    .get(&id)
                    .and_then(|shard| shard.edge_refcounts.get(&digest))
                    .copied(),
                Some(*count),
                "edge refcount missing or mismatched in edge shard"
            );
        }
        let total: usize = edges.values().map(|s| s.edge_refcounts.len()).sum();
        assert_eq!(
            total,
            state.edge_refcounts.len(),
            "edge shards retain stale refcounts"
        );
        // The adjacency section replays to the reconstructed maps.
        let mut forward = BTreeMap::new();
        let mut reverse = BTreeMap::new();
        for shard in adjacency.into_values() {
            forward.extend(shard.forward_refcounts);
            reverse.extend(shard.reverse_refcounts);
        }
        assert_eq!(
            forward, state.forward_refcounts,
            "adjacency forward mismatch"
        );
        assert_eq!(
            reverse, state.reverse_refcounts,
            "adjacency reverse mismatch"
        );
        state
    }

    #[test]
    fn graph_overlay_delta_encoding_replays_to_identical_shards() {
        use crate::incremental_graph::IncrementalGraphState;
        let old_edges = hub_graph_edges(20, "src/old.ts");
        let new_edges = hub_graph_edges(20, "src/new.ts");
        let mut state = IncrementalGraphState::from_edges(&old_edges);
        let expected = IncrementalGraphState::from_edges(&new_edges);
        let pre_shards = state.clone().into_section_shards(4, 4, 4).unwrap();

        let new_extra: std::collections::BTreeSet<crate::incremental_graph::OwnedEdge> = new_edges
            .iter()
            .filter(|edge| edge.source_path.as_deref() == Some("src/extra.ts"))
            .map(crate::incremental_graph::OwnedEdge::from)
            .collect();
        let overlay = state.replace_owned_files(BTreeMap::from([(
            "src/extra.ts".to_owned(),
            Some(new_extra),
        )]));
        assert_eq!(state, expected);
        // The hub node has >20 reverse neighbors; a one-file change must be a delta.
        assert!(
            !overlay.reverse_changes.is_empty(),
            "expected per-neighbor delta encoding for the hub node"
        );
        assert!(
            overlay
                .reverse_refcounts
                .values()
                .flatten()
                .all(|map| map.len() <= 2),
            "hub adjacency must not be serialized in full"
        );

        // Replaying the overlay onto the persisted section shards must reproduce the rebuilt state.
        let replayed = replay_sections(pre_shards, &overlay, 4);
        assert_eq!(replayed, expected);
    }

    #[test]
    fn composed_graph_overlays_match_sequential_replay() {
        use crate::incremental_graph::IncrementalGraphState;
        let v0 = hub_graph_edges(20, "src/v0.ts");
        let v1 = hub_graph_edges(20, "src/v1.ts");
        let v2 = hub_graph_edges(20, "src/v2.ts");
        let mut state = IncrementalGraphState::from_edges(&v0);
        let base_shards = state.clone().into_section_shards(4, 4, 4).unwrap();
        let expected = IncrementalGraphState::from_edges(&v2);

        let extra_edges = |edges: &[crate::model::Edge]| {
            edges
                .iter()
                .filter(|edge| edge.source_path.as_deref() == Some("src/extra.ts"))
                .map(crate::incremental_graph::OwnedEdge::from)
                .collect::<std::collections::BTreeSet<_>>()
        };
        let ov1 = state.replace_owned_files(BTreeMap::from([(
            "src/extra.ts".to_owned(),
            Some(extra_edges(&v1)),
        )]));
        let ov2 = state.replace_owned_files(BTreeMap::from([(
            "src/extra.ts".to_owned(),
            Some(extra_edges(&v2)),
        )]));
        assert_eq!(state, expected);

        let composed = compose_graph_overlay(ov1, ov2);
        let replayed = replay_sections(base_shards, &composed, 4);
        assert_eq!(replayed, expected);
    }

    fn snapshot() -> IndexSnapshot {
        IndexSnapshot {
            id: SnapshotId {
                root: "/repo".into(),
                worktree: "main".into(),
                revision: "r1".into(),
                content_state: "c1".into(),
                schema_version: 1,
                grammar_version: "g1".into(),
                config_hash: "cfg".into(),
            },
            files: BTreeMap::new(),
            edges: Vec::new(),
        }
    }

    fn snapshot_with_files(count: usize) -> IndexSnapshot {
        let mut snapshot = snapshot();
        for index in 0..count {
            let path = format!("src/file-{index}.ts");
            let mut artifact = crate::scanner::parse_source(
                &path,
                format!("export const value{index} = 0;\n").as_bytes(),
            );
            artifact.path = path.clone();
            snapshot.files.insert(path, artifact);
        }
        snapshot
    }

    fn disk_bytes(path: &Path) -> u64 {
        fs::read_dir(path)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| {
                let metadata = entry.metadata().unwrap();
                if metadata.is_dir() {
                    disk_bytes(&entry.path())
                } else {
                    metadata.len()
                }
            })
            .sum()
    }

    #[test]
    fn structural_publish_failpoints_never_advance_current() {
        let _failpoint_guard = STRUCTURAL_FAILPOINT_TEST_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::new(dir.path());
        let base = snapshot();
        store.publish(&base).unwrap();
        let staged = store
            .stage_structural_pack_base(StructuralPackBase {
                snapshot_id: base.id.stable_key(),
                universe: ResolutionUniverse::default(),
                reverse: ReverseShardSet {
                    format_version: ReverseShardSet::FORMAT_VERSION,
                    resolver_fingerprint: String::new(),
                    shard_bits: 0,
                    shards: BTreeMap::new(),
                },
                graph: IncrementalGraphState::default(),
            })
            .unwrap();
        store.attach_structural_pack_base(staged).unwrap();
        let current = fs::read(store.current_path()).unwrap();
        let mut next = base.clone();
        next.id.content_state = "c2".into();
        let graph = IncrementalGraphOverlay::default();
        let universe = ResolutionUniverseOverlay::default();
        let reverse = ReverseOverlaySet {
            format_version: ReverseOverlaySet::FORMAT_VERSION,
            resolver_fingerprint: String::new(),
            shard_bits: 0,
            shards: BTreeMap::new(),
        };
        for stage in 1..=3 {
            STRUCTURAL_PUBLISH_FAILPOINT.store(stage, Ordering::Relaxed);
            assert!(
                store
                    .publish_structural_overlay(
                        &next,
                        &BTreeSet::from(["deleted.ts".into()]),
                        Some((&graph, &universe, &reverse)),
                        true,
                        None,
                    )
                    .is_err()
            );
            assert_eq!(fs::read(store.current_path()).unwrap(), current);
            assert_eq!(store.open_current().unwrap().unwrap().id, base.id);
        }
        STRUCTURAL_PUBLISH_FAILPOINT.store(0, Ordering::Relaxed);
    }

    #[test]
    fn structural_overlay_tiers_bound_chain_and_preserve_last_write() {
        let _failpoint_guard = STRUCTURAL_FAILPOINT_TEST_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::new(dir.path());
        let base = snapshot();
        store.publish(&base).unwrap();
        let staged = store
            .stage_structural_pack_base(StructuralPackBase {
                snapshot_id: base.id.stable_key(),
                universe: ResolutionUniverse::default(),
                reverse: ReverseShardSet {
                    format_version: ReverseShardSet::FORMAT_VERSION,
                    resolver_fingerprint: String::new(),
                    shard_bits: 0,
                    shards: BTreeMap::new(),
                },
                graph: IncrementalGraphState::default(),
            })
            .unwrap();
        store.attach_structural_pack_base(staged).unwrap();

        let changed = BTreeSet::from(["changed.ts".to_owned()]);
        let mut current = base;
        for revision in 1..=64 {
            current.id.content_state = format!("revision-{revision}");
            let mut graph = IncrementalGraphOverlay::default();
            let mut universe = ResolutionUniverseOverlay::default();
            let mut reverse_shard = crate::structural_reverse::ReverseShardOverlay::default();
            if revision % 2 == 0 {
                graph
                    .file_upserts
                    .insert("changed.ts".into(), BTreeSet::new());
                universe.files.insert("changed.ts".into(), true);
                reverse_shard
                    .files
                    .upserts
                    .insert("changed.ts".into(), FileContribution::default());
            } else {
                graph.file_tombstones.insert("changed.ts".into());
                universe.files.insert("changed.ts".into(), false);
                reverse_shard.files.tombstones.insert("changed.ts".into());
            }
            let reverse = ReverseOverlaySet {
                format_version: ReverseOverlaySet::FORMAT_VERSION,
                resolver_fingerprint: String::new(),
                shard_bits: 0,
                shards: BTreeMap::from([(0, reverse_shard)]),
            };
            assert!(
                store
                    .publish_structural_overlay(
                        &current,
                        &changed,
                        Some((&graph, &universe, &reverse)),
                        false,
                        None,
                    )
                    .unwrap()
            );
        }

        let manifest = store.read_manifest().unwrap().unwrap();
        let chain = manifest.structural_packs.unwrap();
        assert_eq!(chain.overlays.len(), 1);
        let merged = store
            .read_structural_overlay_records(&chain.overlays[0])
            .unwrap();
        assert_eq!(merged.weight, 64);
        assert!(merged.graph.file_upserts.contains_key("changed.ts"));
        assert!(!merged.graph.file_tombstones.contains("changed.ts"));
        assert_eq!(merged.universe.files.get("changed.ts"), Some(&true));
        let reverse = &merged.reverse.shards[&0].files;
        assert!(reverse.upserts.contains_key("changed.ts"));
        assert!(!reverse.tombstones.contains("changed.ts"));
    }

    fn publish_body_edit(store: &FileSnapshotStorage, path: &str, revision: usize) {
        let source = format!("export const value = 0; // revision {revision}\n");
        let mut artifact = crate::scanner::parse_source(path, source.as_bytes());
        artifact.path = path.to_owned();
        store
            .publish_artifact_deltas(&[(path.to_owned(), artifact)])
            .unwrap()
            .unwrap();
    }
    #[test]
    fn publication_is_atomic_and_validated() {
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::new(dir.path());
        store.publish(&snapshot()).unwrap();
        assert_eq!(store.open_current().unwrap().unwrap(), snapshot());
        store.validate().unwrap();
        assert!(store.open_stats().unwrap().is_some());
        assert!(store.open_graph().unwrap().is_some());
        assert!(store.open_symbols().unwrap().is_some());
        assert!(store.open_search_dir().unwrap().is_some());
        assert!(store.open_symbol_meta().unwrap().is_some());
        assert!(store.open_file_list().unwrap().is_some());
    }

    #[test]
    fn every_cold_sidecar_rejects_old_schema_with_reindex_guidance() {
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::new(dir.path());
        store.publish(&snapshot()).unwrap();
        let current = fs::read_to_string(store.current_path()).unwrap();
        let manifest_path = dir.path().join(current.trim());
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.schema_version = SCHEMA_VERSION - 1;
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let errors = [
            store.open_stats().unwrap_err(),
            store.open_graph().unwrap_err(),
            store.open_symbols().unwrap_err(),
            store.open_search_dir().unwrap_err(),
            store.open_search_index(true).unwrap_err(),
            store.open_symbol_meta().unwrap_err(),
            store.open_file_list().unwrap_err(),
            store.open_file_hashes().unwrap_err(),
            store.open_hubs().unwrap_err(),
        ];
        assert!(
            errors
                .iter()
                .all(|error| error.to_string().contains("run `ravel index`"))
        );
    }
    #[test]
    fn checksum_corruption_is_rejected() {
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::new(dir.path());
        store.publish(&snapshot()).unwrap();
        let current = fs::read_to_string(dir.path().join("CURRENT")).unwrap();
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(dir.path().join(current.trim())).unwrap()).unwrap();
        fs::write(dir.path().join(manifest.payload), b"broken").unwrap();
        assert!(matches!(
            store.validate(),
            Err(StorageError::Invalid { .. })
        ));
    }

    #[test]
    fn artifact_delta_tiers_bound_same_path_churn() {
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::new(dir.path());
        store.publish(&snapshot_with_files(1)).unwrap();

        for revision in 1..=127 {
            publish_body_edit(&store, "src/file-0.ts", revision);
        }

        let manifest = store.read_manifest().unwrap().unwrap();
        assert_eq!(manifest.artifact_deltas.len(), 7);
        assert_eq!(manifest.artifact_delta_weights.iter().sum::<u64>(), 127);
        assert!(
            manifest
                .artifact_delta_weights
                .windows(2)
                .all(|pair| pair[0] > pair[1])
        );
        let artifact = store.open_artifact("src/file-0.ts").unwrap().unwrap();
        let expected = crate::scanner::parse_source(
            "src/file-0.ts",
            b"export const value = 0; // revision 127\n",
        );
        assert_eq!(artifact.source_hash, expected.source_hash);
        assert_eq!(store.open_current().unwrap().unwrap().files.len(), 1);
    }

    #[test]
    fn artifact_delta_tiers_bound_rotating_path_churn() {
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::new(dir.path());
        store.publish(&snapshot_with_files(16)).unwrap();

        for revision in 1..=64 {
            let path = format!("src/file-{}.ts", revision % 16);
            publish_body_edit(&store, &path, revision);
        }

        let manifest = store.read_manifest().unwrap().unwrap();
        assert_eq!(manifest.artifact_deltas.len(), 1);
        assert_eq!(manifest.artifact_delta_weights, vec![64]);
        let snapshot = store.open_current().unwrap().unwrap();
        assert_eq!(snapshot.files.len(), 16);
        for index in 0..16 {
            assert!(
                store
                    .open_artifact(&format!("src/file-{index}.ts"))
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[test]
    fn artifact_store_amplification_compacts_and_respects_retention() {
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::new(dir.path());
        store.publish(&snapshot_with_files(1)).unwrap();
        for revision in 1..=64 {
            publish_body_edit(&store, "src/file-0.ts", revision);
            store.compact_artifacts_if_amplified(2, 2).unwrap();
        }

        let manifest = store.read_manifest().unwrap().unwrap();
        assert!(manifest.artifact_deltas.len() <= 1);
        let stores: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".artifacts.store")
            })
            .collect();
        assert!(
            stores.len() <= 2,
            "retained artifact stores: {}",
            stores.len()
        );
        let artifact = store.open_artifact("src/file-0.ts").unwrap().unwrap();
        let expected = crate::scanner::parse_source(
            "src/file-0.ts",
            b"export const value = 0; // revision 64\n",
        );
        assert_eq!(artifact.source_hash, expected.source_hash);
    }

    #[test]
    fn artifact_gc_waits_for_active_reader() {
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::new(dir.path());
        store.publish(&snapshot_with_files(1)).unwrap();
        for revision in 1..=4 {
            publish_body_edit(&store, "src/file-0.ts", revision);
        }
        let reader = store.acquire_artifact_read_lock().unwrap();
        let worker_store = store.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = worker_store.compact_artifacts_if_amplified(1, 1);
            sent.send(result).unwrap();
        });
        assert!(
            received
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err()
        );
        drop(reader);
        assert!(
            received
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .unwrap()
        );
        worker.join().unwrap();
    }

    #[test]
    fn artifact_compaction_defers_for_long_lived_generation_reader() {
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::new(dir.path());
        store.publish(&snapshot_with_files(1)).unwrap();
        for revision in 1..=4 {
            publish_body_edit(&store, "src/file-0.ts", revision);
        }
        let _reader = store.acquire_generation_read_guard().unwrap();
        let started = Instant::now();
        assert!(!store.compact_artifacts_if_amplified(1, 1).unwrap());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn generation_gc_defers_between_manifest_and_sidecar_dereference() {
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::with_retention(dir.path(), 1);
        let first = snapshot_with_files(1);
        store.publish(&first).unwrap();
        let old_manifest = store.read_manifest().unwrap().unwrap();
        let old_stats = old_manifest.stats.clone().unwrap();
        let old_manifest_name = store.current_generation().unwrap().unwrap();
        let reader_guard = store.acquire_generation_read_guard().unwrap();

        let mut second = first;
        second.id.content_state = "generation-2".into();
        let worker_store = store.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = worker_store.publish(&second);
            sent.send(result).unwrap();
        });
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while store.current_generation().unwrap().as_deref() == Some(&old_manifest_name) {
            match received.try_recv() {
                Ok(result) => {
                    result.unwrap();
                    panic!("writer completed without advancing CURRENT");
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    panic!("writer disconnected before advancing CURRENT");
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            assert!(Instant::now() < deadline, "writer did not advance CURRENT");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        received
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert!(store.root.join(&old_stats).is_file());
        fs::read(store.root.join(&old_stats)).unwrap();
        drop(reader_guard);
        worker.join().unwrap();
        let report = store.gc_generations().unwrap();
        assert!(!report.deferred_for_readers);
        assert!(!store.root.join(old_manifest_name).exists());
        assert!(!store.root.join(old_stats).exists());
    }

    #[test]
    fn generation_churn_has_bounded_disk_entries() {
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::with_retention(dir.path(), 2);
        for generation in 0..16 {
            let mut snapshot = snapshot_with_files(2);
            snapshot.id.content_state = format!("generation-{generation}");
            store.publish(&snapshot).unwrap();
        }
        let count_generation_entries = || {
            fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("snapshot-"))
                .count()
        };
        let first_plateau = count_generation_entries();
        let first_plateau_bytes = disk_bytes(dir.path());
        for generation in 16..32 {
            let mut snapshot = snapshot_with_files(2);
            snapshot.id.content_state = format!("generation-{generation}");
            store.publish(&snapshot).unwrap();
        }
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let manifests = entries
            .iter()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".manifest.json")
            })
            .count();
        let generation_entries = entries
            .iter()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("snapshot-"))
            .count();
        assert_eq!(manifests, 2);
        assert!(
            first_plateau <= 30,
            "unexpected first plateau: {first_plateau}"
        );
        assert!(
            generation_entries <= first_plateau + 1,
            "generation files still grow: {first_plateau} -> {generation_entries}"
        );
        let final_bytes = disk_bytes(dir.path());
        eprintln!(
            "generation_gc_churn generations=32 retention=2 entries={first_plateau}->{generation_entries} bytes={first_plateau_bytes}->{final_bytes}"
        );
        assert!(
            final_bytes <= first_plateau_bytes.saturating_add(64 * 1024),
            "disk still grows with churn: {first_plateau_bytes} -> {final_bytes}"
        );
        store.validate().unwrap();
    }

    #[test]
    fn generation_reachability_keeps_pack_behind_record_reference() {
        let mut manifest = Manifest {
            snapshot_id: snapshot().id,
            payload_snapshot_id: None,
            base_snapshot_id: None,
            schema_version: SCHEMA_VERSION,
            checksum: String::new(),
            payload: "snapshot-base.bin".into(),
            graph: None,
            stats: Some("snapshot-overlay.pack#stats/json".into()),
            symbols: None,
            symbols_checksum: None,
            search_dir: None,
            search_overlays: Vec::new(),
            symbol_meta: None,
            symbol_meta_overlays: Vec::new(),
            files: None,
            file_hashes: None,
            hubs: None,
            artifact_index: None,
            artifact_deltas: vec!["snapshot-overlay.pack#artifact/delta".into()],
            artifact_delta_weights: vec![1],
            artifact_store: None,
            artifact_locator: None,
            artifact_state: None,
            artifact_live_bytes: None,
            structural_packs: None,
        };
        let refs = FileSnapshotStorage::manifest_component_paths(&manifest);
        assert!(refs.contains("snapshot-overlay.pack"));
        assert!(!refs.iter().any(|reference| reference.contains('#')));
        manifest.stats = None;
        assert!(
            FileSnapshotStorage::manifest_component_paths(&manifest)
                .contains("snapshot-overlay.pack")
        );
    }

    #[test]
    fn structural_publish_reuses_search_when_generation_id_reappears() {
        let _failpoint_guard = STRUCTURAL_FAILPOINT_TEST_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let store = FileSnapshotStorage::with_retention(dir.path(), 3);
        let mut first = snapshot_with_files(1);
        first.id.content_state = "state-a".into();
        store.publish(&first).unwrap();
        let changed = BTreeSet::from(["src/file-0.ts".to_owned()]);

        let mut second = first.clone();
        second.id.content_state = "state-b".into();
        assert!(
            store
                .publish_structural_overlay(&second, &changed, None, true, None)
                .unwrap()
        );
        assert!(
            store
                .publish_structural_overlay(&first, &changed, None, true, None)
                .unwrap()
        );
        store.validate().unwrap();
        assert!(store.open_search_index(true).unwrap().is_some());
    }

    /// Manual churn benchmark: `cargo test -p ravel-core artifact_delta_churn_benchmark -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn artifact_delta_churn_benchmark() {
        for rotating_paths in [1usize, 256] {
            let dir = tempdir().unwrap();
            let store = FileSnapshotStorage::new(dir.path());
            store.publish(&snapshot_with_files(rotating_paths)).unwrap();
            let started = Instant::now();
            for revision in 1..=1_024 {
                let path = format!("src/file-{}.ts", revision % rotating_paths);
                publish_body_edit(&store, &path, revision);
                store.compact_artifacts_if_amplified(4, 3).unwrap();
            }
            let publish_elapsed = started.elapsed();
            let lookup_started = Instant::now();
            for index in 0..rotating_paths {
                store
                    .open_artifact(&format!("src/file-{index}.ts"))
                    .unwrap()
                    .unwrap();
            }
            let lookup_elapsed = lookup_started.elapsed();
            let manifest = store.read_manifest().unwrap().unwrap();
            let store_bytes: u64 = fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".artifacts.store")
                })
                .map(|entry| entry.metadata().unwrap().len())
                .sum();
            let live_bytes = manifest.artifact_live_bytes.unwrap();
            eprintln!(
                "paths={rotating_paths} edits=1024 tiers={} publish_ms={:.2} lookup_ms={:.2} disk_amp={:.2}",
                manifest.artifact_deltas.len(),
                publish_elapsed.as_secs_f64() * 1_000.0,
                lookup_elapsed.as_secs_f64() * 1_000.0,
                store_bytes as f64 / live_bytes as f64,
            );
        }
    }
}
