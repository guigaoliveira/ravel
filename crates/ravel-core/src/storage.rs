use crate::durable_io::{atomic_replace, sync_parent_directory};
use crate::{
    analysis::{self, HubEntry},
    generation_pack::{GenerationPackReader, GenerationPackWriter},
    graph::{CompactGraph, GraphIndex},
    incremental_graph::{
        IncrementalGraphOverlay, IncrementalGraphShard, IncrementalGraphShardSet,
        IncrementalGraphState,
    },
    model::{
        FileArtifact, FileHashIndex, FileList, IndexSnapshot, IndexStats, SnapshotId,
        SymbolMetaDict,
    },
    resolver::{ResolutionUniverse, ResolutionUniverseOverlay},
    search::{SearchIndex, SymbolDict},
    structural_reverse::{ReverseOverlaySet, ReverseShard, ReverseShardSet},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU8, AtomicU64, Ordering},
};
use thiserror::Error;

const SCHEMA_VERSION: u32 = 2;
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
    /// Optional symbol metadata for node_detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_meta: Option<String>,
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
    /// Older manifests omit this; missing weights are interpreted as one generation each.
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
    /// Exact structural-sync acceleration state. Optional for backward compatibility; readers
    /// must fall back when it is absent, corrupt, or belongs to another resolver generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structural_acceleration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structural_packs: Option<StructuralPackChain>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct StructuralPackChain {
    pub format_version: u32,
    pub base: String,
    pub current_snapshot: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlays: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlay_weights: Vec<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct StructuralAcceleration {
    pub format_version: u32,
    pub snapshot_id: String,
    pub universe: ResolutionUniverse,
    pub reverse: ReverseShardSet,
    pub graph: IncrementalGraphState,
}

impl StructuralAcceleration {
    pub const FORMAT_VERSION: u32 = 1;
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

#[derive(Debug, Clone)]
pub struct FileSnapshotStorage {
    root: PathBuf,
    retention: usize,
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
            manifest.structural_acceleration.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            add(reference);
        }
        for reference in &manifest.artifact_deltas {
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

    fn publish_or_reuse_search<'a>(
        &self,
        desired_name: &str,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<String, StorageError> {
        let desired_path = self.root.join(desired_name);
        if desired_path.is_dir() && SearchIndex::open_tantivy_dir(&desired_path).is_ok() {
            return Ok(desired_name.to_owned());
        }
        // Never remove/replace a path that a retained SearchIndex may still have open. A corrupt
        // or incomplete collision gets a unique recovery name and is later reclaimed by GC.
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
        SearchIndex::publish_tantivy_dir(names, &search_tmp).map_err(|error| {
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
            return Ok(None);
        }
        let name = fs::read_to_string(self.current_path())
            .map_err(|source| self.io(source, self.current_path()))?;
        let path = self.root.join(name.trim());
        let bytes = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| StorageError::Json { path, source })
    }

    pub fn open_structural_acceleration(
        &self,
    ) -> Result<Option<StructuralAcceleration>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        let Some(name) = manifest.structural_acceleration else {
            return Ok(None);
        };
        let path = self.root.join(name);
        let bytes = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        let acceleration = bincode::deserialize(&bytes)
            .map_err(|source| StorageError::Bincode { path, source })?;
        Ok(Some(acceleration))
    }

    /// Publish the complete acceleration component before atomically replacing the manifest.
    /// `CURRENT` already names this manifest, so concurrent readers observe either the old
    /// manifest/component pair or the new complete pair, never a partially written component.
    pub fn publish_structural_acceleration(
        &self,
        acceleration: &StructuralAcceleration,
    ) -> Result<(), StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(mut manifest) = self.read_manifest()? else {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "cannot attach acceleration without a current manifest".into(),
            });
        };
        if acceleration.snapshot_id != manifest.snapshot_id.stable_key() {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "acceleration snapshot does not match current manifest".into(),
            });
        }
        let generation = acceleration.snapshot_id.as_str();
        let name = format!("snapshot-{generation}.structural.bin");
        self.atomic_write_bincode(&self.root.join(&name), acceleration)?;
        manifest.structural_acceleration = Some(name);
        let manifest_path = self.manifest_path(generation);
        let bytes = serde_json::to_vec(&manifest).map_err(|source| StorageError::Json {
            path: manifest_path.clone(),
            source,
        })?;
        atomic_write(&manifest_path, &bytes).map_err(|source| self.io(source, manifest_path))
    }

    pub fn publish_structural_pack_base(
        &self,
        acceleration: StructuralAcceleration,
    ) -> Result<(), StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(mut manifest) = self.read_manifest()? else {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "cannot attach structural pack without a current manifest".into(),
            });
        };
        if acceleration.snapshot_id != manifest.snapshot_id.stable_key() {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "structural pack snapshot does not match current manifest".into(),
            });
        }
        let generation = acceleration.snapshot_id;
        let name = format!("snapshot-{generation}.structural.pack");
        let path = self.root.join(&name);
        let mut writer = GenerationPackWriter::new();
        writer
            .add(
                "meta/universe",
                bincode::serialize(&acceleration.universe).map_err(|source| {
                    StorageError::Bincode {
                        path: path.clone(),
                        source,
                    }
                })?,
            )
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        writer
            .add(
                "meta/reverse",
                bincode::serialize(&(
                    acceleration.reverse.format_version,
                    acceleration.reverse.resolver_fingerprint,
                    acceleration.reverse.shard_bits,
                ))
                .map_err(|source| StorageError::Bincode {
                    path: path.clone(),
                    source,
                })?,
            )
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        for (id, shard) in acceleration.reverse.shards {
            writer
                .add(
                    format!("reverse/{id:04x}"),
                    bincode::serialize(&shard).map_err(|source| StorageError::Bincode {
                        path: path.clone(),
                        source,
                    })?,
                )
                .map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
        }
        let graph = acceleration
            .graph
            .into_shards(8)
            .ok_or_else(|| StorageError::Invalid {
                path: path.clone(),
                message: "invalid graph shard layout".into(),
            })?;
        writer
            .add(
                "meta/graph",
                bincode::serialize(&(graph.format_version, graph.shard_bits)).map_err(
                    |source| StorageError::Bincode {
                        path: path.clone(),
                        source,
                    },
                )?,
            )
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        for (id, shard) in graph.shards {
            writer
                .add(
                    format!("graph/{id:04x}"),
                    bincode::serialize(&shard).map_err(|source| StorageError::Bincode {
                        path: path.clone(),
                        source,
                    })?,
                )
                .map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
        }
        writer
            .publish(&path)
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        manifest.structural_packs = Some(StructuralPackChain {
            format_version: StructuralAcceleration::FORMAT_VERSION,
            base: name,
            current_snapshot: generation.clone(),
            overlays: Vec::new(),
            overlay_weights: Vec::new(),
        });
        let manifest_path = self.manifest_path(&generation);
        let bytes = serde_json::to_vec(&manifest).map_err(|source| StorageError::Json {
            path: manifest_path.clone(),
            source,
        })?;
        atomic_write(&manifest_path, &bytes).map_err(|source| self.io(source, manifest_path))
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
        if chain.format_version != StructuralAcceleration::FORMAT_VERSION {
            return Ok(None);
        }
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
                .read("meta/graph", 1024)
                .map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?
        else {
            return Ok(None);
        };
        let (format_version, shard_bits): (u32, u8) =
            bincode::deserialize(&meta).map_err(|source| StorageError::Bincode {
                path: path.clone(),
                source,
            })?;
        let keys: Vec<String> = reader
            .keys()
            .filter(|key| key.starts_with("graph/"))
            .map(str::to_owned)
            .collect();
        let mut shards = BTreeMap::new();
        for key in keys {
            let Some(bytes) =
                reader
                    .read(&key, 256 * 1024 * 1024)
                    .map_err(|error| StorageError::Invalid {
                        path: path.clone(),
                        message: error.to_string(),
                    })?
            else {
                return Ok(None);
            };
            let shard: IncrementalGraphShard =
                bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode {
                    path: path.clone(),
                    source,
                })?;
            let id = u16::from_str_radix(key.trim_start_matches("graph/"), 16).map_err(|_| {
                StorageError::Invalid {
                    path: path.clone(),
                    message: format!("invalid graph shard key {key}"),
                }
            })?;
            shards.insert(id, shard);
        }
        let Some(mut state) = IncrementalGraphShardSet {
            format_version,
            shard_bits,
            shards,
        }
        .into_state() else {
            return Ok(None);
        };
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
                .read("graph/overlay", 256 * 1024 * 1024)
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
            state.apply_overlay(&overlay);
        }
        Ok(Some(state))
    }

    pub fn open_structural_resolution_base(
        &self,
    ) -> Result<Option<(ResolutionUniverse, ReverseShardSet)>, StorageError> {
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
        let Some(universe_bytes) =
            reader
                .read("meta/universe", 512 * 1024 * 1024)
                .map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?
        else {
            return Ok(None);
        };
        let universe: ResolutionUniverse =
            bincode::deserialize(&universe_bytes).map_err(|source| StorageError::Bincode {
                path: path.clone(),
                source,
            })?;
        let Some(meta) =
            reader
                .read("meta/reverse", 64 * 1024)
                .map_err(|error| StorageError::Invalid {
                    path: path.clone(),
                    message: error.to_string(),
                })?
        else {
            return Ok(None);
        };
        let (format_version, resolver_fingerprint, shard_bits): (u32, String, u8) =
            bincode::deserialize(&meta).map_err(|source| StorageError::Bincode {
                path: path.clone(),
                source,
            })?;
        let keys: Vec<String> = reader
            .keys()
            .filter(|key| key.starts_with("reverse/"))
            .map(str::to_owned)
            .collect();
        let mut shards = BTreeMap::new();
        for key in keys {
            let Some(bytes) =
                reader
                    .read(&key, 256 * 1024 * 1024)
                    .map_err(|error| StorageError::Invalid {
                        path: path.clone(),
                        message: error.to_string(),
                    })?
            else {
                return Ok(None);
            };
            let shard: ReverseShard =
                bincode::deserialize(&bytes).map_err(|source| StorageError::Bincode {
                    path: path.clone(),
                    source,
                })?;
            let id = u16::from_str_radix(key.trim_start_matches("reverse/"), 16).map_err(|_| {
                StorageError::Invalid {
                    path: path.clone(),
                    message: format!("invalid reverse shard key {key}"),
                }
            })?;
            shards.insert(id, shard);
        }
        let mut universe = universe;
        let mut reverse = ReverseShardSet {
            format_version,
            resolver_fingerprint,
            shard_bits,
            shards,
        };
        for overlay_name in chain.overlays {
            let overlay_path = self.root.join(overlay_name);
            let mut overlay_reader =
                GenerationPackReader::open(&overlay_path).map_err(|error| {
                    StorageError::Invalid {
                        path: overlay_path.clone(),
                        message: error.to_string(),
                    }
                })?;
            let Some(universe_bytes) = overlay_reader
                .read("meta/universe", 512 * 1024 * 1024)
                .map_err(|error| StorageError::Invalid {
                    path: overlay_path.clone(),
                    message: error.to_string(),
                })?
            else {
                return Ok(None);
            };
            let universe_overlay: ResolutionUniverseOverlay = bincode::deserialize(&universe_bytes)
                .map_err(|source| StorageError::Bincode {
                    path: overlay_path.clone(),
                    source,
                })?;
            universe.apply_overlay(&universe_overlay);
            let Some(reverse_bytes) = overlay_reader
                .read("reverse/overlay", 512 * 1024 * 1024)
                .map_err(|error| StorageError::Invalid {
                    path: overlay_path.clone(),
                    message: error.to_string(),
                })?
            else {
                return Ok(None);
            };
            let overlay: ReverseOverlaySet =
                bincode::deserialize(&reverse_bytes).map_err(|source| StorageError::Bincode {
                    path: overlay_path.clone(),
                    source,
                })?;
            if reverse.apply(&overlay).is_err() {
                return Ok(None);
            }
        }
        Ok(Some((universe, reverse)))
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
        let mut writer = GenerationPackWriter::new();
        writer
            .add(
                "graph/overlay",
                bincode::serialize(graph_overlay).map_err(|source| StorageError::Bincode {
                    path: path.clone(),
                    source,
                })?,
            )
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        writer
            .add("artifact/delta", artifact_delta.to_vec())
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        writer
            .add("stats/json", stats.to_vec())
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        writer
            .add(
                "meta/universe",
                bincode::serialize(universe_overlay).map_err(|source| StorageError::Bincode {
                    path: path.clone(),
                    source,
                })?,
            )
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        writer
            .add(
                "reverse/overlay",
                bincode::serialize(reverse_overlay).map_err(|source| StorageError::Bincode {
                    path: path.clone(),
                    source,
                })?,
            )
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        writer
            .publish(&path)
            .map_err(|error| StorageError::Invalid {
                path: path.clone(),
                message: error.to_string(),
            })?;
        chain.overlays.push(name.clone());
        chain.overlay_weights.push(1);
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
        let bytes = self.read_component_ref(stats_name, 16 * 1024 * 1024)?;
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
            let bytes = self.read_component_ref(delta_name, 512 * 1024 * 1024)?;
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
            let bytes = self.read_component_ref(delta_name, 512 * 1024 * 1024)?;
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
            manifest.artifact_delta_weights = vec![1; manifest.artifact_deltas.len()];
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
            let previous_weight = manifest.artifact_delta_weights.last().copied().unwrap_or(1);
            if previous_weight > delta_weight {
                break;
            }
            let previous_path = self.root.join(self.component_ref_path(previous_name));
            let previous_bytes = self.read_component_ref(previous_name, 512 * 1024 * 1024)?;
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
        self.gc_generations()?;
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
        symbol_names_changed: bool,
        stats_totals: Option<(u64, usize)>,
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
            manifest.artifact_delta_weights = vec![1; manifest.artifact_deltas.len()];
        }
        let mut delta_weight = 1u64;
        while let Some(previous_name) = manifest.artifact_deltas.last() {
            let previous_weight = manifest.artifact_delta_weights.last().copied().unwrap_or(1);
            if previous_weight > delta_weight {
                break;
            }
            let previous_path = self.root.join(self.component_ref_path(previous_name));
            let bytes = self.read_component_ref(previous_name, 512 * 1024 * 1024)?;
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
            && !symbol_names_changed
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
            let dict = SymbolDict::from_snapshot(snapshot);
            let symbols_checksum =
                self.atomic_write_bincode(&self.root.join(&symbols_name), &dict)?;
            let search_name = self
                .publish_or_reuse_search(&new_search_name, dict.names.iter().map(String::as_str))?;
            (symbols_name, symbols_checksum, search_name)
        };

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
            // Hubs are derived from the graph and the legacy sidecar belongs to the base
            // generation. Never expose it after a structural graph overlay.
            manifest.hubs = None;
        }
        manifest.symbols = Some(symbols_name);
        manifest.symbols_checksum = Some(symbols_checksum);
        manifest.symbol_meta = None;
        manifest.stats = Some(
            packed_generation
                .as_ref()
                .map_or(stats_name, |name| format!("{name}#stats/json")),
        );
        manifest.search_dir = Some(search_name);
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
        self.gc_generations()?;
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
            // Legacy manifests take the correctness path below once, then gain the counter.
            drop(preliminary_guard);
            return self.compact_artifacts_if_amplified_locked(max_amplification, retention);
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

    /// Fast path: load precomputed stats without deserializing the full snapshot.
    pub fn open_stats(&self) -> Result<Option<IndexStats>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.open_stats_from_manifest(&manifest)
    }

    /// Fast path: load prebuilt compact graph without full snapshot / adjacency rebuild.
    pub fn open_graph(&self) -> Result<Option<GraphIndex>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        if manifest
            .structural_packs
            .as_ref()
            .is_some_and(|chain| chain.current_snapshot == manifest.snapshot_id.stable_key())
            && let Some(state) = self.open_structural_graph_base()?
        {
            return Ok(Some(GraphIndex::from_edges(
                &state.edges(),
                manifest.snapshot_id.stable_key(),
            )));
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
        // v2 includes `lower[]`; v1 was names-only — accept both (bincode has no serde default).
        let dict = match bincode::deserialize::<SymbolDict>(&payload) {
            Ok(d) => d,
            Err(_) => {
                #[derive(serde::Deserialize)]
                struct Legacy {
                    format_version: u32,
                    snapshot_id: String,
                    names: Vec<String>,
                }
                let leg: Legacy =
                    bincode::deserialize(&payload).map_err(|source| StorageError::Bincode {
                        path: path.clone(),
                        source,
                    })?;
                SymbolDict {
                    format_version: leg.format_version,
                    snapshot_id: leg.snapshot_id,
                    names: leg.names,
                    lower: Vec::new(),
                }
            }
        };
        if dict.snapshot_id != self.component_snapshot_id(&manifest).stable_key() {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "symbols snapshot id mismatch".into(),
            });
        }
        if dict.format_version > SymbolDict::FORMAT_VERSION {
            return Err(StorageError::Invalid {
                path,
                message: format!("unsupported symbols format {}", dict.format_version),
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
        let dict = self.symbols_from_manifest(&manifest)?;
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
        Ok(Some(index.with_generation_guard(generation_guard)))
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
            || dict.format_version > SymbolDict::FORMAT_VERSION
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
        let Some(name) = manifest.symbol_meta.as_ref() else {
            return Ok(None);
        };
        let path = self.root.join(name);
        if !path.is_file() {
            return Ok(None);
        }
        let payload = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        // Stale sidecars (schema drift) → treat as missing; caller falls back or reindexes.
        let Ok(meta) = bincode::deserialize::<SymbolMetaDict>(&payload) else {
            return Ok(None);
        };
        if meta.format_version != SymbolMetaDict::FORMAT_VERSION {
            return Ok(None);
        }
        if meta.snapshot_id != self.component_snapshot_id(&manifest).stable_key() {
            return Ok(None);
        }
        Ok(Some(meta))
    }

    pub fn open_file_list(&self) -> Result<Option<FileList>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
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
        for path in paths {
            let hash = self
                .current_artifact_location(&manifest, path)?
                .map(|location| location.source_hash);
            hashes.insert(path.clone(), hash);
        }
        Ok(hashes)
    }

    /// Precomputed top hubs — O(1) open + O(k) for large graphs.
    pub fn open_hubs(&self) -> Result<Option<Vec<HubEntry>>, StorageError> {
        let _generation_guard = self.acquire_generation_read_guard()?;
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
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
        let previous_manifest = self.read_manifest().ok().flatten();
        let previous_symbols = self.open_symbols().ok().flatten();
        let id = snapshot.id.stable_key();
        // Stream large bincode values directly to their atomic temp files. Holding every
        // serialized sidecar Vec until the end previously added ~94 MB on the real corpus.
        let checksum = self.atomic_write_bincode(&self.payload_path(&id), snapshot)?;
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

        // Prebuild compact graph once at index time so cold CLI queries skip rebuild.
        let graph = GraphIndex::from_snapshot(snapshot);
        self.atomic_write_bincode(&self.graph_path(&id), &graph.as_compact_ref())?;
        // Top-k hubs at index time: online hubs must not be O(V) on 1B-node graphs.
        // top-k from default analysis config (engine path may use config.hubs_top_k; publish uses 1000)
        let hubs = analysis::precompute_hubs(&graph, 1_000);
        let hubs_bytes = serde_json::to_vec(&hubs).map_err(|source| StorageError::Json {
            path: self.hubs_path(&id),
            source,
        })?;
        atomic_write(&self.hubs_path(&id), &hubs_bytes)
            .map_err(|source| self.io(source, self.hubs_path(&id)))?;
        drop(graph);

        let dict = SymbolDict::from_snapshot(snapshot);
        let symbols_checksum = self.atomic_write_bincode(&self.symbols_path(&id), &dict)?;

        let symbol_meta = SymbolMetaDict::from_snapshot(snapshot);
        self.atomic_write_bincode(&self.symbol_meta_path(&id), &symbol_meta)?;
        drop(symbol_meta);

        let file_list = FileList::from_snapshot(snapshot);
        self.atomic_write_bincode(&self.files_path(&id), &file_list)?;
        drop(file_list);

        let file_hashes = FileHashIndex::from_snapshot(snapshot);
        self.atomic_write_bincode(&self.root.join(&file_hashes_name), &file_hashes)?;
        drop(file_hashes);

        // The Tantivy corpus is only the unique symbol-name set. Most saves change bodies,
        // spans, or references but not names, so reuse the immutable previous directory.
        let reusable_search = previous_symbols
            .as_ref()
            .filter(|previous| previous.names == dict.names)
            .and_then(|_| previous_manifest.as_ref()?.search_dir.clone())
            .filter(|name| self.root.join(name).is_dir());
        let search_name = if let Some(name) = reusable_search {
            name
        } else {
            self.publish_or_reuse_search(&new_search_name, dict.names.iter().map(String::as_str))?
        };
        drop(dict);

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

        // Small JSON sidecars remain buffered; all large bincode sidecars are already durable.
        atomic_write(&self.stats_path(&id), &stats_bytes)
            .map_err(|source| self.io(source, self.stats_path(&id)))?;
        let (artifact_index_name, artifact_store, artifact_locator, artifact_state) =
            self.write_initial_artifact_store(&id, snapshot)?;
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
            symbol_meta: Some(symbol_meta_name),
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
            structural_acceleration: None,
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
                message: format!("unsupported schema {}", manifest.schema_version),
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
                message: format!("unsupported schema {}", manifest.schema_version),
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
        store
            .publish_structural_pack_base(StructuralAcceleration {
                format_version: StructuralAcceleration::FORMAT_VERSION,
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
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while store.current_generation().unwrap().as_deref() == Some(&old_manifest_name) {
            assert!(Instant::now() < deadline, "writer did not advance CURRENT");
            std::thread::yield_now();
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
            symbol_meta: None,
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
            structural_acceleration: None,
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
