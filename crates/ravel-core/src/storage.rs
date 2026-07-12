use crate::{
    analysis::{self, HubEntry},
    graph::{CompactGraph, GraphIndex},
    model::{FileHashIndex, FileList, IndexSnapshot, IndexStats, SnapshotId, SymbolMetaDict},
    search::{SearchIndex, SymbolDict},
};
use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};
use thiserror::Error;

const SCHEMA_VERSION: u32 = 1;

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
}

pub trait SnapshotStorage {
    fn publish(&self, snapshot: &IndexSnapshot) -> Result<(), StorageError>;
    fn open_current(&self) -> Result<Option<IndexSnapshot>, StorageError>;
    fn validate(&self) -> Result<(), StorageError>;
}

#[derive(Debug, Clone)]
pub struct FileSnapshotStorage {
    root: PathBuf,
}
impl FileSnapshotStorage {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }
    fn current_path(&self) -> PathBuf {
        self.root.join("CURRENT")
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
    fn search_dir_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("snapshot-{id}.search"))
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

    /// Read stats using a manifest already loaded by the caller.
    pub fn open_stats_from_manifest(
        &self,
        manifest: &Manifest,
    ) -> Result<Option<IndexStats>, StorageError> {
        let Some(stats_name) = manifest.stats.as_ref() else {
            return Ok(None);
        };
        let path = self.root.join(stats_name);
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = fs::read(&path).map_err(|source| self.io(source, path.clone()))?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| StorageError::Json { path, source })
    }

    /// Presence check for a manifest reference; never reads or deserializes the sidecar.
    pub fn referenced_path_exists(&self, name: Option<&str>) -> bool {
        name.is_some_and(|name| self.root.join(name).exists())
    }
    fn io(&self, source: io::Error, path: PathBuf) -> StorageError {
        StorageError::Io { path, source }
    }

    /// Fast path: load precomputed stats without deserializing the full snapshot.
    pub fn open_stats(&self) -> Result<Option<IndexStats>, StorageError> {
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.open_stats_from_manifest(&manifest)
    }

    /// Fast path: load prebuilt compact graph without full snapshot / adjacency rebuild.
    pub fn open_graph(&self) -> Result<Option<GraphIndex>, StorageError> {
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
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
        if compact.snapshot_id != manifest.snapshot_id.stable_key() {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "graph snapshot id mismatch".into(),
            });
        }
        Ok(Some(GraphIndex::from_compact(compact)))
    }

    /// Fast path: symbol dictionary for cold search.
    pub fn open_symbols(&self) -> Result<Option<SymbolDict>, StorageError> {
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
        if dict.snapshot_id != manifest.snapshot_id.stable_key() {
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

    pub fn open_symbol_meta(&self) -> Result<Option<SymbolMetaDict>, StorageError> {
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
        if meta.snapshot_id != manifest.snapshot_id.stable_key() {
            return Ok(None);
        }
        Ok(Some(meta))
    }

    pub fn open_file_list(&self) -> Result<Option<FileList>, StorageError> {
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
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
        if list.snapshot_id != manifest.snapshot_id.stable_key() {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "files list snapshot id mismatch".into(),
            });
        }
        Ok(Some(list))
    }

    /// Path → source_hash sidecar (~small) for auto-sync without hydrating full snapshot.
    pub fn open_file_hashes(&self) -> Result<Option<FileHashIndex>, StorageError> {
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
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
        if idx.snapshot_id != manifest.snapshot_id.stable_key() {
            return Ok(None);
        }
        Ok(Some(idx))
    }

    /// Precomputed top hubs — O(1) open + O(k) for large graphs.
    pub fn open_hubs(&self) -> Result<Option<Vec<HubEntry>>, StorageError> {
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
        fs::rename(&tmp, path).map_err(|source| self.io(source, path.to_path_buf()))?;
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
        let id = snapshot.id.stable_key();
        // Stream large bincode values directly to their atomic temp files. Holding every
        // serialized sidecar Vec until the end previously added ~94 MB on the real corpus.
        let checksum = self.atomic_write_bincode(&self.payload_path(&id), snapshot)?;
        let payload_name = format!("snapshot-{id}.bin");
        let graph_name = format!("snapshot-{id}.graph.bin");
        let stats_name = format!("snapshot-{id}.stats.json");
        let symbols_name = format!("snapshot-{id}.symbols.bin");
        let search_name = format!("snapshot-{id}.search");
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

        // On-disk Tantivy for hybrid fuzzy/regex (built from unique names only).
        let search_path = self.search_dir_path(&id);
        let search_tmp = self
            .root
            .join(format!("snapshot-{id}.search.tmp-{}", std::process::id()));
        if search_tmp.exists() {
            let _ = fs::remove_dir_all(&search_tmp);
        }
        SearchIndex::publish_tantivy_dir(dict.names.iter().map(|s| s.as_str()), &search_tmp)
            .map_err(|e| StorageError::Search {
                path: search_tmp.clone(),
                message: e.to_string(),
            })?;
        if search_path.exists() {
            let _ = fs::remove_dir_all(&search_path);
        }
        fs::rename(&search_tmp, &search_path)
            .map_err(|source| self.io(source, search_path.clone()))?;
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

        let manifest = Manifest {
            snapshot_id: snapshot.id.clone(),
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
        Ok(())
    }

    fn open_current(&self) -> Result<Option<IndexSnapshot>, StorageError> {
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
        let snapshot: IndexSnapshot = bincode::deserialize(&payload)
            .map_err(|source| StorageError::Bincode { path, source })?;
        if snapshot.id != manifest.snapshot_id {
            return Err(StorageError::Invalid {
                path: self.current_path(),
                message: "manifest snapshot id mismatch".into(),
            });
        }
        Ok(Some(snapshot))
    }

    fn validate(&self) -> Result<(), StorageError> {
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
        if snapshot.id != manifest.snapshot_id {
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
                if compact.snapshot_id != snapshot.id.stable_key() {
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
                if dict.snapshot_id != snapshot.id.stable_key() {
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
    fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IndexSnapshot, SnapshotId};
    use std::collections::BTreeMap;
    use tempfile::tempdir;

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
}
