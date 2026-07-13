use crate::{
    analysis::{self, CiReport, CycleInfo, HubEntry, ImpactReport, PackageInfo},
    config::{Config, Flags},
    graph::{GraphIndex, QueryLimits, QueryPage},
    incremental_graph::IncrementalGraphOverlay,
    incremental_graph::IncrementalGraphState,
    model::{IndexSnapshot, SnapshotId},
    policy::{PolicyFinding, Suppressions, validate_snapshot},
    resolver::{
        load_tsconfig, resolve_edges, resolve_edges_with_structural_data,
        resolve_subset_with_structural_data,
    },
    scanner::scan_workspace,
    search::{SearchHit, SearchIndex, SearchKind},
    storage::{FileSnapshotStorage, SnapshotStorage, StorageError, StructuralAcceleration},
    structural::StructuralReverseIndex,
    structural_reverse::{ReverseOverlaySet, ReverseShardSet},
};

pub use crate::model::IndexStats;
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Scan(#[from] crate::scanner::ScanError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("worktree identity: {0}")]
    Git(String),
    #[error("query: {0}")]
    Query(#[from] crate::graph::QueryError),
    #[error("search: {0}")]
    Search(String),
    #[error("workspace update lock {path}: {source}")]
    UpdateLock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("sync path is outside workspace {root}: {path}")]
    PathOutsideWorkspace { root: PathBuf, path: PathBuf },
    #[error("workspace sync queue {path}: {source}")]
    SyncQueue {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid workspace sync queue entry {path}: {message}")]
    InvalidSyncQueue { path: PathBuf, message: String },
}

static SYNC_TICKET: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct EngineInner {
    snapshot_cache: Mutex<Option<Arc<IndexSnapshot>>>,
    graph_cache: Mutex<Option<Arc<GraphIndex>>>,
    search_cache: Mutex<Option<Arc<SearchIndex>>>,
    symbol_meta_cache: Mutex<Option<Arc<crate::model::SymbolMetaDict>>>,
    /// Cached "is this a git repo?" — avoid probing every tool call.
    git_repo: Mutex<Option<(crate::git::GitMetadataFingerprint, bool)>>,
    worktree_identity: Mutex<
        Option<(
            crate::git::GitMetadataFingerprint,
            crate::git::WorktreeIdentity,
        )>,
    >,
    config_hash: String,
    /// Debounce dirty discovery for concurrent tool calls in the same warm MCP tick.
    dirty_cache: Mutex<Option<(std::time::Instant, Vec<PathBuf>)>>,
    /// Serialize full and incremental publications from MCP watchers and tool calls.
    update_lock: Mutex<()>,
    /// Most recent background/explicit update failure. Queries may keep serving the last
    /// complete snapshot, but agents must be told that freshness is no longer guaranteed.
    last_update_error: Mutex<Option<String>>,
    /// `CURRENT` generation observed by this process. Each MCP client may run its own server.
    observed_generation: Mutex<Option<String>>,
    structural_cache: Mutex<
        Option<(
            String,
            crate::resolver::ResolutionUniverse,
            ReverseShardSet,
            IncrementalGraphState,
        )>,
    >,
}

struct PreparedPath {
    path: PathBuf,
    relative: String,
    bytes: Option<Vec<u8>>,
    unchanged: bool,
}

fn symbol_name_set_changed<'a>(
    old_names: impl Iterator<Item = &'a str>,
    new_names: impl Iterator<Item = &'a str>,
    global_counts: Option<&std::collections::BTreeMap<String, u32>>,
) -> bool {
    let mut delta: std::collections::BTreeMap<&str, (u32, u32)> = std::collections::BTreeMap::new();
    for name in old_names {
        delta.entry(name).or_default().0 += 1;
    }
    for name in new_names {
        delta.entry(name).or_default().1 += 1;
    }
    delta.into_iter().any(|(name, (old, new))| {
        if let Some(counts) = global_counts {
            let before = counts.get(name).copied().unwrap_or(0);
            let after = before.saturating_sub(old).saturating_add(new);
            (before == 0) != (after == 0)
        } else {
            // Without the persisted global counts, unequal batch multiplicity may change the
            // search set. Rebuild conservatively; equal rename/move batches remain reusable.
            old != new
        }
    })
}

/// Shared, cloneable workspace engine with in-memory snapshot and graph caching.
/// All query methods use `&self` (interior mutability via Mutex).
/// Cloning is cheap (`Arc` bump) and shares the same cache.
#[derive(Debug, Clone)]
pub struct WorkspaceEngine {
    pub root: PathBuf,
    pub config: Config,
    inner: Arc<EngineInner>,
}
impl WorkspaceEngine {
    pub fn load(root: &Path, flags: &Flags) -> Result<Self, EngineError> {
        let config = Config::load(root, flags)?;
        let config_hash = config.hash();
        Ok(Self {
            root: root.to_path_buf(),
            config,
            inner: Arc::new(EngineInner {
                snapshot_cache: Mutex::new(None),
                graph_cache: Mutex::new(None),
                search_cache: Mutex::new(None),
                symbol_meta_cache: Mutex::new(None),
                git_repo: Mutex::new(None),
                worktree_identity: Mutex::new(None),
                config_hash,
                dirty_cache: Mutex::new(None),
                update_lock: Mutex::new(()),
                last_update_error: Mutex::new(None),
                observed_generation: Mutex::new(None),
                structural_cache: Mutex::new(None),
            }),
        })
    }

    fn is_git_repo_cached(&self) -> bool {
        let fingerprint = crate::git::metadata_fingerprint(&self.root);
        self.is_git_repo_cached_for(&fingerprint)
    }

    fn is_git_repo_cached_for(&self, fingerprint: &crate::git::GitMetadataFingerprint) -> bool {
        let mut slot = self.inner.git_repo.lock().unwrap();
        if let Some((cached_fingerprint, value)) = slot.as_ref()
            && cached_fingerprint == fingerprint
        {
            return *value;
        }
        let value = crate::git::is_git_repo(&self.root);
        *slot = Some((fingerprint.clone(), value));
        value
    }

    fn worktree_identity_cached(&self) -> crate::git::WorktreeIdentity {
        let fingerprint = crate::git::metadata_fingerprint(&self.root);
        let mut cache = self.inner.worktree_identity.lock().unwrap();
        if let Some((cached_fingerprint, identity)) = cache.as_ref()
            && *cached_fingerprint == fingerprint
        {
            return identity.clone();
        }
        let identity = if self.is_git_repo_cached_for(&fingerprint) {
            crate::git::worktree_identity_or_nogit(&self.root)
        } else {
            let canonical = self
                .root
                .canonicalize()
                .unwrap_or_else(|_| self.root.clone());
            crate::git::WorktreeIdentity {
                root: self.root.clone(),
                worktree: canonical.to_string_lossy().into_owned(),
                revision: "nogit".into(),
            }
        };
        *cache = Some((fingerprint, identity.clone()));
        identity
    }
    pub fn storage(&self) -> FileSnapshotStorage {
        FileSnapshotStorage::with_retention(
            self.root.join(&self.config.storage.home),
            self.config.storage.retention,
        )
    }
    pub fn index(&self) -> Result<IndexStats, EngineError> {
        let _guard = self.inner.update_lock.lock().unwrap();
        let _process_guard = self.acquire_workspace_update_lock()?;
        let result = self.index_unlocked();
        self.finish_update("index", &result);
        result
    }

    fn index_unlocked(&self) -> Result<IndexStats, EngineError> {
        let (artifacts, scan_stats) = scan_workspace(&self.config)?;
        let files: std::collections::BTreeMap<_, _> =
            artifacts.into_iter().map(|a| (a.path.clone(), a)).collect();
        self.publish_from_artifacts(
            files,
            scan_stats.bytes_read,
            scan_stats.parse_errors,
            None,
            None,
            None,
            true,
            None,
        )
    }

    /// Incremental index update for daily edits (save/rename/delete).
    /// Re-parses only the given paths (or dirty discovery if `None`), re-resolves edges, republishes sidecars.
    pub fn sync(&self, only_paths: Option<&[PathBuf]>) -> Result<IndexStats, EngineError> {
        let queued = only_paths.filter(|paths| !paths.is_empty());
        let _guard = self.inner.update_lock.lock().unwrap();
        if let Some(paths) = queued
            && let Some(_process_guard) = self.try_acquire_workspace_update_lock()?
        {
            // N=1: no ticket and no timer. Stale tickets are folded into this publication.
            let result = self.sync_queued_locked(paths, None);
            self.finish_update("sync", &result);
            return result;
        }
        let own_ticket = queued.map(|paths| self.enqueue_sync(paths)).transpose()?;
        let _process_guard = self.acquire_workspace_update_lock()?;
        // Another writer may already have drained our ticket and committed its paths while we
        // waited for the workspace lock. In that case the current generation is our result.
        if own_ticket.as_ref().is_some_and(|ticket| !ticket.exists()) {
            let result =
                self.storage()
                    .open_stats()?
                    .ok_or_else(|| EngineError::InvalidSyncQueue {
                        path: self.sync_queue_dir(),
                        message: "committed sync has no published stats".into(),
                    });
            self.finish_update("sync", &result);
            return result;
        }
        let result = if let Some(paths) = queued {
            self.sync_queued_locked(paths, own_ticket.as_deref())
        } else {
            self.sync_unlocked(only_paths)
        };
        self.finish_update("sync", &result);
        result
    }

    fn sync_queued_locked(
        &self,
        own_paths: &[PathBuf],
        own_ticket: Option<&Path>,
    ) -> Result<IndexStats, EngineError> {
        let (batched_paths, tickets) = self.drain_sync_queue(own_paths)?;
        let result = self.sync_unlocked(Some(&batched_paths));
        if result.is_ok() {
            for ticket in tickets {
                let _ = std::fs::remove_file(ticket);
            }
            if let Some(ticket) = own_ticket {
                let _ = std::fs::remove_file(ticket);
            }
        }
        result
    }

    fn sync_queue_dir(&self) -> PathBuf {
        self.root
            .join(&self.config.storage.home)
            .join("pending-sync")
    }

    fn enqueue_sync(&self, paths: &[PathBuf]) -> Result<PathBuf, EngineError> {
        self.validate_sync_paths(paths)?;
        if paths.len() > self.config.sync.queue_max_paths {
            return Err(EngineError::InvalidSyncQueue {
                path: self.sync_queue_dir(),
                message: format!(
                    "ticket has {} paths, limit is {}",
                    paths.len(),
                    self.config.sync.queue_max_paths
                ),
            });
        }
        let dir = self.sync_queue_dir();
        std::fs::create_dir_all(&dir).map_err(|source| EngineError::SyncQueue {
            path: dir.clone(),
            source,
        })?;
        self.cleanup_sync_queue(&dir)?;
        let payload = serde_json::to_vec(paths).map_err(|error| EngineError::InvalidSyncQueue {
            path: dir.clone(),
            message: error.to_string(),
        })?;
        if payload.len() as u64 > self.config.sync.queue_max_ticket_bytes {
            return Err(EngineError::InvalidSyncQueue {
                path: dir,
                message: format!(
                    "ticket is {} bytes, limit is {}",
                    payload.len(),
                    self.config.sync.queue_max_ticket_bytes
                ),
            });
        }
        let queued = std::fs::read_dir(&dir)
            .map_err(|source| EngineError::SyncQueue {
                path: dir.clone(),
                source,
            })?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            })
            .take(self.config.sync.queue_max_tickets)
            .count();
        if queued >= self.config.sync.queue_max_tickets {
            return Err(EngineError::InvalidSyncQueue {
                path: dir,
                message: format!(
                    "queue ticket limit {} reached",
                    self.config.sync.queue_max_tickets
                ),
            });
        }
        loop {
            let sequence = SYNC_TICKET.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("{}-{sequence}.json", std::process::id()));
            let temporary = dir.join(format!("{}-{sequence}.tmp", std::process::id()));
            if path.exists() || temporary.exists() {
                continue;
            }
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    file.write_all(&payload)
                        .map_err(|source| EngineError::SyncQueue {
                            path: temporary.clone(),
                            source,
                        })?;
                    drop(file);
                    std::fs::rename(&temporary, &path).map_err(|source| {
                        EngineError::SyncQueue {
                            path: path.clone(),
                            source,
                        }
                    })?;
                    return Ok(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(EngineError::SyncQueue { path, source }),
            }
        }
    }

    fn drain_sync_queue(
        &self,
        own_paths: &[PathBuf],
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>), EngineError> {
        let dir = self.sync_queue_dir();
        self.cleanup_sync_queue(&dir)?;
        let mut paths: BTreeSet<PathBuf> = own_paths.iter().cloned().collect();
        let mut tickets = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((paths.into_iter().collect(), tickets));
            }
            Err(source) => return Err(EngineError::SyncQueue { path: dir, source }),
        };
        for entry in entries.take(self.config.sync.queue_max_tickets) {
            let entry = entry.map_err(|source| EngineError::SyncQueue {
                path: dir.clone(),
                source,
            })?;
            let ticket = entry.path();
            if ticket.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let size = entry
                .metadata()
                .map_err(|source| EngineError::SyncQueue {
                    path: ticket.clone(),
                    source,
                })?
                .len();
            if size > self.config.sync.queue_max_ticket_bytes {
                let _ = std::fs::rename(&ticket, ticket.with_extension("invalid"));
                continue;
            }
            let payload = std::fs::read(&ticket).map_err(|source| EngineError::SyncQueue {
                path: ticket.clone(),
                source,
            })?;
            let queued: Vec<PathBuf> = match serde_json::from_slice(&payload) {
                Ok(queued) => queued,
                Err(_) => {
                    // A malformed/stale request must not poison every future sync.
                    let quarantine = ticket.with_extension("invalid");
                    let _ = std::fs::rename(&ticket, quarantine);
                    continue;
                }
            };
            if queued.len() > self.config.sync.queue_max_paths
                || self.validate_sync_paths(&queued).is_err()
            {
                let _ = std::fs::rename(&ticket, ticket.with_extension("invalid"));
                continue;
            }
            paths.extend(queued);
            tickets.push(ticket);
        }
        Ok((paths.into_iter().collect(), tickets))
    }

    fn cleanup_sync_queue(&self, dir: &Path) -> Result<(), EngineError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(EngineError::SyncQueue {
                    path: dir.into(),
                    source,
                });
            }
        };
        let stale_after = std::time::Duration::from_secs(self.config.sync.queue_stale_seconds);
        let now = std::time::SystemTime::now();
        let mut removed = 0;
        for entry in entries.filter_map(Result::ok) {
            if removed >= self.config.sync.queue_cleanup_limit {
                break;
            }
            let path = entry.path();
            let extension = path.extension().and_then(|value| value.to_str());
            if !matches!(extension, Some("tmp" | "invalid")) {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= stale_after);
            if stale && std::fs::remove_file(path).is_ok() {
                removed += 1;
            }
        }
        Ok(())
    }

    fn validate_sync_paths(&self, paths: &[PathBuf]) -> Result<(), EngineError> {
        for path in paths {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                self.root.join(path)
            };
            if absolute
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
                || !absolute.starts_with(&self.root)
            {
                return Err(EngineError::PathOutsideWorkspace {
                    root: self.root.clone(),
                    path: absolute,
                });
            }
        }
        Ok(())
    }

    fn finish_update(&self, operation: &str, result: &Result<IndexStats, EngineError>) {
        let mut slot = self.inner.last_update_error.lock().unwrap();
        *slot = result
            .as_ref()
            .err()
            .map(|error| format!("{operation}: {error}"));
    }

    pub(crate) fn record_update_error(&self, operation: &str, error: &str) {
        *self.inner.last_update_error.lock().unwrap() = Some(format!("{operation}: {error}"));
    }

    fn last_update_error(&self) -> Option<String> {
        self.inner.last_update_error.lock().unwrap().clone()
    }

    fn acquire_workspace_update_lock(&self) -> Result<File, EngineError> {
        use fs4::fs_std::FileExt;
        let file = self.open_workspace_update_lock()?;
        let path = self
            .root
            .join(&self.config.storage.home)
            .join("update.lock");
        file.lock_exclusive()
            .map_err(|source| EngineError::UpdateLock { path, source })?;
        Ok(file)
    }

    fn try_acquire_workspace_update_lock(&self) -> Result<Option<File>, EngineError> {
        use fs4::fs_std::FileExt;
        let file = self.open_workspace_update_lock()?;
        let path = self
            .root
            .join(&self.config.storage.home)
            .join("update.lock");
        file.try_lock_exclusive()
            .map(|locked| locked.then_some(file))
            .map_err(|source| EngineError::UpdateLock { path, source })
    }

    fn open_workspace_update_lock(&self) -> Result<File, EngineError> {
        let storage = self.root.join(&self.config.storage.home);
        std::fs::create_dir_all(&storage).map_err(|source| EngineError::UpdateLock {
            path: storage.clone(),
            source,
        })?;
        let path = storage.join("update.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| EngineError::UpdateLock {
                path: path.clone(),
                source,
            })?;
        Ok(file)
    }

    fn refresh_external_generation(&self) -> Result<(), EngineError> {
        let generation = self.storage().current_generation()?;
        let mut observed = self.inner.observed_generation.lock().unwrap();
        if *observed == generation {
            return Ok(());
        }
        // Publish the observed generation only after every component cache is invalidated.
        // Keeping this coordination lock closes the window where another request could see the
        // new generation marker and still consume a cache populated from the previous one.
        self.clear_cache();
        *observed = generation;
        Ok(())
    }

    fn remember_published_generation(&self) {
        if let Ok(generation) = self.storage().current_generation() {
            *self.inner.observed_generation.lock().unwrap() = generation;
        }
    }

    fn sync_unlocked(&self, only_paths: Option<&[PathBuf]>) -> Result<IndexStats, EngineError> {
        let max_bytes = self.config.parser.max_file_size_kb.saturating_mul(1024);
        let extensions = crate::config::effective_extensions(&self.config);
        if let Some(paths) = only_paths {
            self.validate_sync_paths(paths)?;
        }
        let paths: Vec<PathBuf> = match only_paths {
            Some(p) if !p.is_empty() => p
                .iter()
                .filter(|p| {
                    self.config.is_source_with_extensions(p, &extensions)
                        && !self.config.is_noise(p)
                })
                .cloned()
                .collect(),
            _ => self.discover_dirty_sources(),
        };

        // Read/hash each path once. The prepared bytes are reused below if publication is needed.
        let (fast_noop, prepared) = self.prepare_paths(&paths)?;
        if fast_noop {
            if let Ok(Some(stats)) = self.storage().open_stats() {
                return Ok(stats);
            }
        }
        if let Some(stats) = self.try_artifact_only_sync(&prepared)? {
            *self.inner.snapshot_cache.lock().unwrap() = None;
            *self.inner.symbol_meta_cache.lock().unwrap() = None;
            *self.inner.dirty_cache.lock().unwrap() = None;
            self.remember_published_generation();
            return Ok(stats);
        }

        let resolver_config = load_tsconfig(&self.root);
        let current_generation = self.storage().current_generation().ok().flatten();
        // Structural acceleration is a resident optimization only. Hydrating universe, reverse
        // shards and graph packs in a cold one-shot sync costs more time and over twice the RSS
        // of the exact snapshot fallback. A warm daemon may reuse state it already owns, but a
        // cold CLI never opens global acceleration packs merely to process a small edit.
        let mut acceleration = self
            .inner
            .structural_cache
            .lock()
            .unwrap()
            .take()
            .filter(|(generation, ..)| Some(generation) == current_generation.as_ref())
            .map(|(_, universe, reverse, graph)| (universe, reverse, graph));

        let cache_is_current = *self.inner.observed_generation.lock().unwrap()
            == self.storage().current_generation().ok().flatten();
        let cached_snapshot = cache_is_current
            .then(|| self.inner.snapshot_cache.lock().unwrap().take())
            .flatten()
            .map(|snapshot| {
                Arc::try_unwrap(snapshot).unwrap_or_else(|shared| shared.as_ref().clone())
            });
        let existing = if let Some(snapshot) = cached_snapshot {
            Some(snapshot)
        } else {
            match self.storage().open_current() {
                Ok(snapshot) => snapshot,
                Err(_) => return self.index_unlocked(),
            }
        };
        let Some(mut snapshot) = existing else {
            return self.index_unlocked();
        };
        let mut edge_inputs_changed = false;
        let stats_from = |snapshot: &IndexSnapshot| IndexStats {
            files: snapshot.files.len(),
            edges: snapshot.edges.len(),
            bytes: snapshot.files.values().map(|a| a.bytes_read).sum(),
            parse_errors: snapshot
                .files
                .values()
                .map(|a| usize::from(!a.diagnostics.is_empty()))
                .sum(),
            snapshot_id: snapshot.id.stable_key(),
        };
        if prepared.is_empty() {
            return Ok(stats_from(&snapshot));
        }
        let overlay_paths: BTreeSet<String> = prepared
            .iter()
            .filter(|prepared| !prepared.unchanged)
            .map(|prepared| prepared.relative.clone())
            .collect();
        let mut any_changed = false;
        let old_changed: std::collections::BTreeMap<_, _> = overlay_paths
            .iter()
            .map(|path| (path.clone(), snapshot.files.get(path).cloned()))
            .collect();
        for prepared in prepared {
            let rel = prepared.relative;
            let Some(bytes) = prepared.bytes else {
                if snapshot.files.remove(&rel).is_some() {
                    any_changed = true;
                    edge_inputs_changed = true;
                }
                continue;
            };
            if prepared.unchanged {
                continue;
            }
            let scanned = if bytes.len() as u64 > max_bytes {
                crate::scanner::scan_file(&prepared.path, max_bytes)
            } else {
                Ok(crate::scanner::parse_source(
                    &prepared.path.to_string_lossy(),
                    &bytes,
                ))
            };
            match scanned {
                Ok(mut artifact) => {
                    artifact.path = rel.clone();
                    edge_inputs_changed |= snapshot.files.get(&rel).is_none_or(|previous| {
                        previous.imports != artifact.imports
                            || previous.exports != artifact.exports
                            || previous.symbol_refs != artifact.symbol_refs
                            || previous
                                .symbols
                                .iter()
                                .map(|symbol| symbol.name.as_str())
                                .ne(artifact.symbols.iter().map(|symbol| symbol.name.as_str()))
                    });
                    snapshot.files.insert(rel, artifact);
                    any_changed = true;
                }
                Err(_) => {
                    if snapshot.files.remove(&rel).is_some() {
                        any_changed = true;
                        edge_inputs_changed = true;
                    }
                }
            }
        }
        if !any_changed {
            return Ok(stats_from(&snapshot));
        }
        *self.inner.dirty_cache.lock().unwrap() = None;
        let previous_stats = self.storage().open_stats().ok().flatten();
        let (bytes, parse_errors) = if let Some(stats) = previous_stats {
            overlay_paths.iter().fold(
                (stats.bytes, stats.parse_errors),
                |(bytes, errors), path| {
                    let old = old_changed.get(path).and_then(Option::as_ref);
                    let new = snapshot.files.get(path);
                    (
                        bytes
                            .saturating_sub(old.map_or(0, |artifact| artifact.bytes_read))
                            .saturating_add(new.map_or(0, |artifact| artifact.bytes_read)),
                        errors
                            .saturating_sub(old.map_or(0, |artifact| {
                                usize::from(!artifact.diagnostics.is_empty())
                            }))
                            .saturating_add(new.map_or(0, |artifact| {
                                usize::from(!artifact.diagnostics.is_empty())
                            })),
                    )
                },
            )
        } else {
            snapshot
                .files
                .values()
                .fold((0, 0), |(bytes, errors), artifact| {
                    (
                        bytes + artifact.bytes_read,
                        errors + usize::from(!artifact.diagnostics.is_empty()),
                    )
                })
        };
        let mut acceleration_overlay = None;
        let old_names = overlay_paths.iter().flat_map(|path| {
            old_changed
                .get(path)
                .and_then(Option::as_ref)
                .into_iter()
                .flat_map(|artifact| artifact.symbols.iter().map(|symbol| symbol.name.as_str()))
        });
        let new_names = overlay_paths.iter().flat_map(|path| {
            snapshot
                .files
                .get(path)
                .into_iter()
                .flat_map(|artifact| artifact.symbols.iter().map(|symbol| symbol.name.as_str()))
        });
        let symbol_names_changed = symbol_name_set_changed(
            old_names,
            new_names,
            acceleration
                .as_ref()
                .map(|(universe, _, _)| &universe.symbol_definers),
        );
        let mut next_structural_cache = None;
        let reusable_edges = if edge_inputs_changed
            && let Some((mut universe, mut reverse, mut graph)) = acceleration.take()
        {
            let mut changed_symbols = BTreeSet::new();
            let mut universe_overlay = crate::resolver::ResolutionUniverseOverlay::default();
            for path in &overlay_paths {
                let old = old_changed.get(path).and_then(Option::as_ref);
                let new = snapshot.files.get(path);
                changed_symbols.extend(old.into_iter().flat_map(|artifact| {
                    artifact.symbols.iter().map(|symbol| symbol.name.clone())
                }));
                changed_symbols.extend(new.into_iter().flat_map(|artifact| {
                    artifact.symbols.iter().map(|symbol| symbol.name.clone())
                }));
                universe.replace_artifact_with_overlay(old, new, &mut universe_overlay);
            }
            let affected = reverse.affected_files(
                overlay_paths.iter().map(String::as_str),
                changed_symbols.iter().map(String::as_str),
            );
            let subset = affected
                .iter()
                .filter_map(|path| {
                    snapshot
                        .files
                        .get(path)
                        .cloned()
                        .map(|artifact| (path.clone(), artifact))
                })
                .collect();
            if let Some((traces, contributions)) = resolve_subset_with_structural_data(
                &self.root,
                &subset,
                &universe,
                &resolver_config,
            ) {
                let updates = affected
                    .iter()
                    .map(|path| {
                        (
                            path.clone(),
                            snapshot
                                .files
                                .get(path)
                                .map(|_| contributions.get(path).cloned().unwrap_or_default()),
                        )
                    })
                    .collect();
                let graph_overlay = graph.replace_files(updates);
                let mut traces_by_file: std::collections::BTreeMap<_, Vec<_>> =
                    std::collections::BTreeMap::new();
                for trace in &traces {
                    traces_by_file
                        .entry(trace.importer.as_str())
                        .or_default()
                        .push(trace);
                }
                let reverse_updates = affected
                    .iter()
                    .map(|path| {
                        let contribution = snapshot.files.get(path).map(|artifact| {
                            crate::structural::FileContribution::from_artifact(
                                artifact,
                                traces_by_file
                                    .get(path.as_str())
                                    .into_iter()
                                    .flatten()
                                    .copied(),
                            )
                        });
                        (path.clone(), contribution)
                    })
                    .collect();
                let reverse_overlay = reverse.replace_files(reverse_updates);
                let edges = graph.edges();
                next_structural_cache = Some((universe, reverse, graph));
                acceleration_overlay = Some((universe_overlay, reverse_overlay, graph_overlay));
                Some(edges)
            } else {
                None
            }
        } else if !edge_inputs_changed {
            Some(snapshot.edges)
        } else {
            None
        };
        let content_changes = overlay_paths
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    old_changed
                        .get(path)
                        .and_then(Option::as_ref)
                        .map(|artifact| artifact.source_hash.clone()),
                    snapshot
                        .files
                        .get(path)
                        .map(|artifact| artifact.source_hash.clone()),
                )
            })
            .collect::<Vec<_>>();
        let content_state = self.storage().prospective_content_state(&content_changes);
        let result = self.publish_from_artifacts(
            snapshot.files,
            bytes,
            parse_errors,
            reusable_edges,
            Some(&overlay_paths),
            acceleration_overlay,
            symbol_names_changed,
            content_state,
        );
        if result.is_ok()
            && let Some((universe, reverse, graph)) = next_structural_cache
            && let Ok(Some(generation)) = self.storage().current_generation()
        {
            *self.inner.structural_cache.lock().unwrap() =
                Some((generation, universe, reverse, graph));
        }
        result
    }

    fn try_artifact_only_sync(
        &self,
        prepared: &[PreparedPath],
    ) -> Result<Option<IndexStats>, EngineError> {
        let storage = self.storage();
        let mut deltas = Vec::new();
        for prepared in prepared {
            if prepared.unchanged {
                continue;
            }
            let Some(bytes) = prepared.bytes.as_ref() else {
                return Ok(None);
            };
            let Some(previous) = storage.open_artifact(&prepared.relative)? else {
                return Ok(None);
            };
            let mut artifact =
                crate::scanner::parse_source(&prepared.path.to_string_lossy(), bytes);
            artifact.path = prepared.relative.clone();
            // These fields feed global graph/search/detail sidecars. If any changes, the
            // current format must use the full correctness path until those indexes are sharded.
            let symbol_shape_changed = previous.symbols.len() != artifact.symbols.len()
                || previous
                    .symbols
                    .iter()
                    .zip(&artifact.symbols)
                    .any(|(old, new)| {
                        old.name != new.name || old.kind != new.kind || old.exported != new.exported
                    });
            if previous.language != artifact.language
                || symbol_shape_changed
                || previous.imports != artifact.imports
                || previous.exports != artifact.exports
                || previous.symbol_refs != artifact.symbol_refs
            {
                return Ok(None);
            }
            deltas.push((prepared.relative.clone(), artifact));
        }
        if deltas.is_empty() {
            return Ok(None);
        }
        let stats = storage.publish_artifact_deltas(&deltas)?;
        storage.compact_artifacts_if_amplified(
            self.config.storage.artifact_store_max_amplification,
            self.config.storage.retention,
        )?;
        Ok(stats)
    }

    /// Prepare changed-path bytes once and decide whether the entire sync is a no-op.
    fn prepare_paths(&self, paths: &[PathBuf]) -> Result<(bool, Vec<PreparedPath>), EngineError> {
        if paths.is_empty() {
            return Ok((true, Vec::new()));
        }
        let root_str = self.root.to_string_lossy().replace('\\', "/");
        let root_str = root_str.replace("/./", "/");
        let root_str = root_str.trim_end_matches('/').to_owned();
        let mut candidates = Vec::with_capacity(paths.len());
        for path in paths {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                self.root.join(path)
            };
            let path_str = path.to_string_lossy().replace('\\', "/");
            let Some(rel) = path_str.strip_prefix(&root_str).and_then(|relative| {
                let relative = relative.trim_start_matches('/');
                (!relative.starts_with("../") && relative != "..").then_some(relative.to_owned())
            }) else {
                return Err(EngineError::PathOutsideWorkspace {
                    root: self.root.clone(),
                    path,
                });
            };
            let max_bytes = self.config.parser.max_file_size_kb.saturating_mul(1024);
            let bytes = path
                .is_file()
                .then(|| {
                    std::fs::metadata(&path)
                        .ok()
                        .filter(|metadata| metadata.len() <= max_bytes)
                        .and_then(|_| std::fs::read(&path).ok())
                })
                .flatten();
            candidates.push((path, rel, bytes));
        }
        let storage = self.storage();
        let has_generation = storage.current_generation()?.is_some();
        let requested: Vec<_> = candidates
            .iter()
            .map(|(_, relative, _)| relative.clone())
            .collect();
        let hashes = storage.source_hashes_for_paths(&requested)?;
        let mut all_unchanged = has_generation;
        let mut prepared = Vec::with_capacity(candidates.len());
        for (path, rel, bytes) in candidates {
            let unchanged = bytes.as_ref().is_some_and(|bytes| {
                hashes
                    .get(&rel)
                    .and_then(Option::as_ref)
                    .is_some_and(|old| blake3::hash(bytes).to_hex().as_str() == old)
            }) || (bytes.is_none()
                && hashes.get(&rel).is_some_and(Option::is_none));
            all_unchanged &= unchanged;
            prepared.push(PreparedPath {
                path,
                relative: rel,
                bytes,
                unchanged,
            });
        }
        Ok((all_unchanged, prepared))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "publication inputs are explicit until the component migration removes legacy sidecars"
    )]
    fn publish_from_artifacts(
        &self,
        files: std::collections::BTreeMap<String, crate::model::FileArtifact>,
        bytes_read: u64,
        parse_errors: usize,
        reusable_edges: Option<Vec<crate::model::Edge>>,
        overlay_paths: Option<&BTreeSet<String>>,
        acceleration_overlay: Option<(
            crate::resolver::ResolutionUniverseOverlay,
            ReverseOverlaySet,
            IncrementalGraphOverlay,
        )>,
        symbol_names_changed: bool,
        content_state_override: Option<String>,
    ) -> Result<IndexStats, EngineError> {
        let resolver = load_tsconfig(&self.root);
        let (edges, acceleration_parts) = if overlay_paths.is_some() {
            (
                reusable_edges.unwrap_or_else(|| resolve_edges(&self.root, &files, &resolver)),
                None,
            )
        } else {
            let (resolved_edges, traces, contributions, universe) =
                resolve_edges_with_structural_data(&self.root, &files, &resolver);
            let reverse_index =
                StructuralReverseIndex::build_from_traces(&files, &resolver, &traces);
            let reverse = ReverseShardSet::from_index(&reverse_index, 8).map_err(|error| {
                EngineError::Storage(StorageError::Invalid {
                    path: self.root.join(&self.config.storage.home),
                    message: error.to_string(),
                })
            })?;
            (
                reusable_edges.unwrap_or(resolved_edges),
                Some((
                    universe,
                    reverse,
                    IncrementalGraphState::from_contributions(&contributions),
                )),
            )
        };
        // Works without git — identity falls back to path + "nogit".
        let identity = self.worktree_identity_cached();
        let content_state = content_state_override.unwrap_or_else(|| {
            let mut state = [0u8; 32];
            for (path, artifact) in &files {
                let mut hasher = blake3::Hasher::new();
                hasher.update(&(path.len() as u64).to_le_bytes());
                hasher.update(path.as_bytes());
                hasher.update(artifact.source_hash.as_bytes());
                for (slot, byte) in state.iter_mut().zip(hasher.finalize().as_bytes()) {
                    *slot ^= byte;
                }
            }
            blake3::Hash::from_bytes(state).to_hex().to_string()
        });
        let id = SnapshotId {
            root: identity.root.to_string_lossy().into_owned(),
            worktree: identity.worktree,
            revision: identity.revision,
            content_state,
            schema_version: 2,
            grammar_version: crate::scanner::GRAMMAR_VERSION.into(),
            config_hash: self.inner.config_hash.clone(),
        };
        let snapshot = IndexSnapshot { id, files, edges };
        let storage = self.storage();
        let staged_overlay = acceleration_overlay.as_ref().map(
            |(universe_overlay, reverse_overlay, graph_overlay)| {
                (graph_overlay, universe_overlay, reverse_overlay)
            },
        );
        let published_overlay = overlay_paths
            .map(|paths| {
                storage.publish_structural_overlay(
                    &snapshot,
                    paths,
                    staged_overlay,
                    symbol_names_changed,
                    Some((bytes_read, parse_errors)),
                )
            })
            .transpose()?
            .unwrap_or(false);
        if !published_overlay {
            storage.publish(&snapshot)?;
            if let Some((universe, reverse, graph)) = acceleration_parts {
                storage.publish_structural_pack_base(StructuralAcceleration {
                    format_version: StructuralAcceleration::FORMAT_VERSION,
                    snapshot_id: snapshot.id.stable_key(),
                    universe,
                    reverse,
                    graph,
                })?;
            }
        }
        let stats = IndexStats {
            files: snapshot.files.len(),
            edges: snapshot.edges.len(),
            bytes: bytes_read,
            parse_errors,
            snapshot_id: snapshot.id.stable_key(),
        };
        *self.inner.snapshot_cache.lock().unwrap() = Some(Arc::new(snapshot));
        *self.inner.graph_cache.lock().unwrap() = None;
        *self.inner.search_cache.lock().unwrap() = None;
        *self.inner.symbol_meta_cache.lock().unwrap() = None;
        // Cache state and the generation marker become visible in that order; readers that race
        // before this point invalidate/reload from CURRENT instead of accepting stale entries.
        self.remember_published_generation();
        Ok(stats)
    }

    /// Index health for agents. Cheap: does not spawn git status.
    pub fn status(&self) -> Result<serde_json::Value, EngineError> {
        let store = self.storage();
        // Status reports presence, so read the manifest once and never deserialize graph/symbol
        // sidecars merely to answer whether their referenced paths exist.
        let manifest = store.read_manifest().ok().flatten();
        let stats = manifest
            .as_ref()
            .and_then(|m| store.open_stats_from_manifest(m).ok().flatten());
        let stats_present = stats.is_some();
        let graph_present = manifest
            .as_ref()
            .is_some_and(|m| store.referenced_path_exists(m.graph.as_deref()));
        let has = stats_present || graph_present;
        let home = self.root.join(&self.config.storage.home);
        let git = self.is_git_repo_cached();
        Ok(serde_json::json!({
            "root": self.root,
            "indexed": has,
            "storage": home,
            "stats": stats,
            "git_repo": git,
            "auto_sync": self.config.sync.auto && self.config.sync_allows_git(),
            "sync_mode": self.config.sync.mode,
            "include_untracked": self.config.sync.include_untracked,
            "last_update_error": self.last_update_error(),
            "extensions": crate::config::effective_extensions(&self.config),
            "sidecars": {
                "graph": graph_present,
                "symbols": manifest.as_ref().is_some_and(|m| store.referenced_path_exists(m.symbols.as_deref())),
                "stats": stats_present,
                "hubs": manifest.as_ref().is_some_and(|m| store.referenced_path_exists(m.hubs.as_deref())),
                "file_hashes": manifest.as_ref().is_some_and(|m| store.referenced_path_exists(m.file_hashes.as_deref())),
            },
            "hint": if !has {
                "Run `ravel index` first."
            } else if !self.config.sync.auto {
                "Index ready. Auto-sync off. Use `ravel sync <paths>` or `watch`."
            } else if !git {
                "Index ready (no git). Freshness: `ravel watch` or `ravel sync <paths>`."
            } else {
                "Index ready. Auto-sync: tracked dirty + hash sidecar."
            },
        }))
    }

    /// One-shot agent context: search + callers + impact in a single call (fewer tool hops).
    /// Auto-syncs dirty git sources **once** (nested search/query skip a second pass).
    pub fn context(&self, query: &str, limit: usize) -> Result<serde_json::Value, EngineError> {
        let synced = self.auto_sync_if_dirty()?;
        let limit = limit.clamp(1, 50);
        // Use raw paths — auto_sync already ran.
        let hits = self.search_raw(query, SearchKind::Prefix, limit)?;
        let primary = hits
            .first()
            .map(|h| h.value.clone())
            .unwrap_or_else(|| query.to_owned());
        let limits = QueryLimits {
            depth: 2,
            nodes: 50,
            edges: 200,
            page_size: 20,
            ..Default::default()
        };
        let callers = self.query_raw(&primary, true, &limits, None).ok();
        let callees = self.query_raw(&primary, false, &limits, None).ok();
        let impact = self.impact_risk(&primary, &limits).ok();
        let detail = self.node_detail(&primary).ok().flatten();
        // SymbolMeta keeps the defining path in a sidecar, so expose it here and save the
        // agent a second search/read just to locate the symbol's file.
        let detail_path = self
            .symbol_meta()
            .ok()
            .flatten()
            .and_then(|meta| meta.get(&primary).map(|symbol| symbol.path.clone()));
        // Compact: names only in nested pages (agents don't need full QueryPage fields thrice).
        let callers_names: Vec<String> = callers
            .as_ref()
            .map(|p| p.items.iter().take(limit).cloned().collect())
            .unwrap_or_default();
        let callees_names: Vec<String> = callees
            .as_ref()
            .map(|p| p.items.iter().take(limit).cloned().collect())
            .unwrap_or_default();
        let impact_top: Vec<_> = impact
            .as_ref()
            .map(|r| {
                r.affected
                    .iter()
                    .take(limit)
                    .map(|i| {
                        serde_json::json!({
                            "s": i.symbol,
                            "risk": i.risk,
                            "d": i.depth,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(serde_json::json!({
            "q": query,
            "primary": primary,
            "matches": hits.iter().map(|h| &h.value).collect::<Vec<_>>(),
            "detail": detail.as_ref().map(|d| serde_json::json!({
                "name": d.name, "kind": d.kind, "exported": d.exported, "path": detail_path
            })),
            "callers": callers_names,
            "callees": callees_names,
            "impact": impact_top,
            "n_callers": callers
                .as_ref()
                .map(|p| p.visited_nodes.saturating_sub(1))
                .unwrap_or(0),
            "n_affected": impact.as_ref().map(|r| r.total_affected).unwrap_or(0),
            "auto_synced": synced.is_some(),
            "sync_warning": self.last_update_error(),
            "sid": self.stats().map(|s| s.snapshot_id).ok(),
        }))
    }

    pub fn snapshot(&self) -> Result<Arc<IndexSnapshot>, EngineError> {
        self.refresh_external_generation()?;
        self.snapshot_after_refresh()
    }

    /// Materialize the snapshot after the caller has refreshed the generation, without
    /// re-entering `observed_generation` while another component cache is held.
    fn snapshot_after_refresh(&self) -> Result<Arc<IndexSnapshot>, EngineError> {
        // Keep the cache lock through first materialization. First-use requests commonly arrive
        // as an agent fan-out; check/build/store outside the lock lets every request deserialize
        // the same generation and multiplies peak RSS by N.
        let mut cache = self.inner.snapshot_cache.lock().unwrap();
        if let Some(snapshot) = cache.as_ref() {
            return Ok(Arc::clone(snapshot));
        }
        let snapshot = self.storage().open_current()?.ok_or_else(|| {
            EngineError::Storage(StorageError::Invalid {
                path: self.storage_path(),
                message: "no current snapshot; run `ravel index` first".into(),
            })
        })?;
        let arc = Arc::new(snapshot);
        *cache = Some(Arc::clone(&arc));
        Ok(arc)
    }
    pub fn search_index(&self) -> Result<Arc<SearchIndex>, EngineError> {
        self.search_index_for(SearchKind::Exact)
    }

    /// Materialize search backend for `kind` without loading the full snapshot when sidecars exist.
    /// Exact/prefix open **dict only** (cheapest). Fuzzy/regex open Hybrid (dict + on-disk Tantivy).
    fn search_index_for(&self, kind: SearchKind) -> Result<Arc<SearchIndex>, EngineError> {
        self.refresh_external_generation()?;
        let mut cache = self.inner.search_cache.lock().unwrap();
        if let Some(index) = cache.as_ref() {
            // Upgrade path: cached dict-only but fuzzy/regex needs Tantivy.
            let needs_tantivy = matches!(kind, SearchKind::Fuzzy | SearchKind::Regex);
            if !needs_tantivy || index.backend_label() != "dict" {
                return Ok(Arc::clone(index));
            }
        }
        let needs_tantivy = matches!(kind, SearchKind::Fuzzy | SearchKind::Regex);
        let index = if let Some(index) = self.storage().open_search_index(needs_tantivy)? {
            Arc::new(index)
        } else {
            let snapshot = self.snapshot_after_refresh()?;
            Arc::new(
                SearchIndex::from_snapshot(&snapshot)
                    .map_err(|e| EngineError::Search(e.to_string()))?,
            )
        };
        *cache = Some(Arc::clone(&index));
        Ok(index)
    }
    pub fn graph(&self) -> Result<Arc<GraphIndex>, EngineError> {
        self.refresh_external_generation()?;
        let mut cache = self.inner.graph_cache.lock().unwrap();
        if let Some(graph) = cache.as_ref() {
            return Ok(Arc::clone(graph));
        }
        // Prefer prebuilt compact graph (cold CLI path); fall back to full snapshot rebuild.
        let graph = if let Some(graph) = self.storage().open_graph()? {
            Arc::new(graph)
        } else {
            let snapshot = self.snapshot_after_refresh()?;
            Arc::new(GraphIndex::from_snapshot(&snapshot))
        };
        *cache = Some(Arc::clone(&graph));
        Ok(graph)
    }
    pub fn clear_cache(&self) {
        *self.inner.snapshot_cache.lock().unwrap() = None;
        *self.inner.graph_cache.lock().unwrap() = None;
        *self.inner.search_cache.lock().unwrap() = None;
        *self.inner.symbol_meta_cache.lock().unwrap() = None;
    }

    fn symbol_meta(&self) -> Result<Option<Arc<crate::model::SymbolMetaDict>>, EngineError> {
        self.refresh_external_generation()?;
        let mut cache = self.inner.symbol_meta_cache.lock().unwrap();
        if let Some(meta) = cache.as_ref() {
            return Ok(Some(Arc::clone(meta)));
        }
        let Some(meta) = self.storage().open_symbol_meta()? else {
            return Ok(None);
        };
        let meta = Arc::new(meta);
        *cache = Some(Arc::clone(&meta));
        Ok(Some(meta))
    }
    pub fn query(
        &self,
        node: &str,
        reverse: bool,
        limits: &QueryLimits,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<QueryPage, EngineError> {
        let _ = self.auto_sync_if_dirty()?;
        self.query_raw(node, reverse, limits, cancel)
    }

    /// Query without auto-sync (for compound tools that already synced once).
    pub fn query_raw(
        &self,
        node: &str,
        reverse: bool,
        limits: &QueryLimits,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<QueryPage, EngineError> {
        let graph = self.graph()?;
        Ok(if reverse {
            graph.callers_of(node, limits, cancel)?
        } else {
            graph.impact_analysis(node, limits, cancel)?
        })
    }

    pub fn search(
        &self,
        query: &str,
        kind: SearchKind,
        limit: usize,
    ) -> Result<Vec<SearchHit>, EngineError> {
        let _ = self.auto_sync_if_dirty()?;
        self.search_raw(query, kind, limit)
    }

    /// Search without auto-sync (for compound tools that already synced once).
    pub fn search_raw(
        &self,
        query: &str,
        kind: SearchKind,
        limit: usize,
    ) -> Result<Vec<SearchHit>, EngineError> {
        let index = self.search_index_for(kind)?;
        index
            .search(query, kind, limit)
            .map_err(|e| EngineError::Search(e.to_string()))
    }
    pub fn validate(&self) -> Result<Vec<PolicyFinding>, EngineError> {
        let snapshot = self.snapshot()?;
        self.storage().validate()?;
        let mut findings = validate_snapshot(&snapshot, &Suppressions::default());
        // T018 architecture boundaries (optional file).
        let graph = self.graph()?;
        if let Ok(extra) = crate::boundaries::evaluate_boundaries(
            &self.root,
            &snapshot,
            graph.as_ref(),
            &Suppressions::default(),
        ) {
            findings.extend(extra);
        }
        Ok(findings)
    }

    pub fn boundaries(&self) -> Result<Vec<PolicyFinding>, EngineError> {
        let snapshot = self.snapshot()?;
        let graph = self.graph()?;
        crate::boundaries::evaluate_boundaries(
            &self.root,
            &snapshot,
            graph.as_ref(),
            &Suppressions::default(),
        )
        .map_err(EngineError::Git)
    }

    /// Schema summary: node kinds / edge kinds counts (no full dump).
    pub fn describe_schema(&self) -> Result<serde_json::Value, EngineError> {
        let snapshot = self.snapshot()?;
        let mut node_kinds: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        let mut edge_kinds: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for f in snapshot.files.values() {
            for s in &f.symbols {
                *node_kinds.entry(s.kind.to_string()).or_default() += 1;
            }
        }
        for e in &snapshot.edges {
            let k = format!("{:?}", e.kind);
            *edge_kinds.entry(k).or_default() += 1;
        }
        Ok(serde_json::json!({
            "snapshot_id": snapshot.id.stable_key(),
            "files": snapshot.files.len(),
            "edges": snapshot.edges.len(),
            "node_kinds": node_kinds,
            "edge_kinds": edge_kinds,
            "packages": self.list_packages().map(|p| p.len()).unwrap_or(0),
        }))
    }
    pub fn stats(&self) -> Result<IndexStats, EngineError> {
        // Sidecar avoids deserializing the full 80MB+ snapshot for cold `ravel stats`.
        if let Some(stats) = self.storage().open_stats()? {
            return Ok(stats);
        }
        let snapshot = self.snapshot()?;
        Ok(IndexStats {
            files: snapshot.files.len(),
            edges: snapshot.edges.len(),
            bytes: snapshot
                .files
                .values()
                .map(|artifact| artifact.bytes_read)
                .sum(),
            parse_errors: snapshot
                .files
                .values()
                .map(|artifact| usize::from(!artifact.diagnostics.is_empty()))
                .sum(),
            snapshot_id: snapshot.id.stable_key(),
        })
    }
    pub fn node_detail(&self, symbol: &str) -> Result<Option<crate::model::Symbol>, EngineError> {
        // Prefer compact symbol_meta sidecar (no full snapshot / MCP snapshot_cache).
        if let Some(meta) = self.symbol_meta()? {
            return match meta.get(symbol) {
                Some(m) => {
                    // Delta generations keep the global name→path sidecar when symbol shape is
                    // unchanged, then read current span/complexity from the artifact overlay.
                    if let Some(artifact) = self.storage().open_artifact(&m.path)?
                        && let Some(current) = artifact.symbols.iter().find(|s| s.name == symbol)
                    {
                        return Ok(Some(current.clone()));
                    }
                    Ok(Some(crate::model::Symbol {
                        name: m.name.clone(),
                        kind: m.kind.clone(),
                        span: m.span,
                        exported: m.exported,
                        complexity: m.complexity.clone(),
                    }))
                }
                None => Ok(None),
            };
        }
        let snapshot = self.snapshot()?;
        Ok(snapshot
            .files
            .values()
            .flat_map(|f| f.symbols.iter())
            .find(|s| s.name == symbol)
            .cloned())
    }
    pub fn files_in_package(&self, package: &str) -> Result<Vec<String>, EngineError> {
        if let Some(list) = self.storage().open_file_list()? {
            return Ok(list.in_package(package));
        }
        let snapshot = self.snapshot()?;
        let prefix = format!("/{package}/");
        let mut files: Vec<_> = snapshot
            .files
            .keys()
            .filter(|path| path.contains(&prefix))
            .cloned()
            .collect();
        files.sort();
        Ok(files)
    }
    fn storage_path(&self) -> PathBuf {
        self.root.join(&self.config.storage.home)
    }

    // --- Agent-facing analyses (CLI-first, shared with MCP) ---

    pub fn cycles(&self, package: Option<&str>) -> Result<Vec<CycleInfo>, EngineError> {
        let graph = self.graph()?;
        Ok(analysis::package_cycles(&graph, package))
    }

    pub fn impact_risk(
        &self,
        node: &str,
        limits: &QueryLimits,
    ) -> Result<ImpactReport, EngineError> {
        let graph = self.graph()?;
        Ok(analysis::impact_with_risk(&graph, node, limits)?)
    }

    pub fn hubs(
        &self,
        limit: usize,
        kind_filter: Option<&str>,
    ) -> Result<Vec<HubEntry>, EngineError> {
        // Prefer precomputed top-k (O(k)); fallback to heap top-k over graph O(V log k).
        let top_k = self.config.analysis.hubs_top_k.max(limit).max(1);
        let raw = if let Some(hubs) = self.storage().open_hubs()? {
            hubs
        } else {
            let graph = self.graph()?;
            analysis::hubs(&graph, top_k)
        };
        let meta = self.symbol_meta()?;
        let mut enriched = analysis::enrich_hubs(raw, meta.as_deref(), kind_filter);
        enriched.truncate(limit.max(1));
        Ok(enriched)
    }

    pub fn orphans(&self, limit: usize) -> Result<Vec<String>, EngineError> {
        let graph = self.graph()?;
        let meta = self.symbol_meta()?;
        let manifest_entries = crate::entries::collect_manifest_entry_paths(&self.root);
        Ok(analysis::orphans(
            &graph,
            meta.as_deref(),
            limit,
            &self.config.analysis.entry_points,
            &manifest_entries,
        ))
    }

    /// Discover dirty source paths according to `[sync]` config.
    /// Empty when mode=none, no git available, or clean tree. Never requires git to exist.
    pub fn discover_dirty_sources(&self) -> Vec<PathBuf> {
        // mode=none or auto without .git → empty (caller uses explicit paths / watch).
        if !self.config.sync_allows_git() || !self.is_git_repo_cached() {
            return Vec::new();
        }
        // Same-tick MCP: reuse dirty list ~50ms (not a perf SLA — avoids double git spawn).
        {
            let cache = self.inner.dirty_cache.lock().unwrap();
            if let Some((at, paths)) = cache.as_ref() {
                if at.elapsed()
                    < std::time::Duration::from_millis(self.config.sync.discovery_cache_ms)
                {
                    return paths.clone();
                }
            }
        }
        let discovery = crate::git::DirtyDiscovery {
            include_untracked: self.config.sync.include_untracked,
            skip_sibling_emit: self.config.sync.skip_sibling_emit,
            sibling_emit: self.config.sibling_emit_rules(),
        };
        let extensions = crate::config::effective_extensions(&self.config);
        let paths: Vec<PathBuf> = crate::git::changed_paths_with(&self.root, &discovery)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| {
                self.config.is_source_with_extensions(p, &extensions) && !self.config.is_noise(p)
            })
            .collect();
        *self.inner.dirty_cache.lock().unwrap() = Some((std::time::Instant::now(), paths.clone()));
        paths
    }

    /// Fast freshness: dirty discovery (git if present) + hash-sidecar no-op.
    /// Never hydrates the full snapshot unless content changed and a hash sidecar exists.
    pub fn auto_sync_if_dirty(&self) -> Result<Option<IndexStats>, EngineError> {
        if !self.config.sync.auto {
            return Ok(None);
        }
        if self.storage().open_stats().ok().flatten().is_none() {
            return Ok(None);
        }
        if !self.is_git_repo_cached() || !self.config.sync_allows_git() {
            return Ok(None);
        }
        // Without hash sidecar, skip auto-sync (forces one `ravel index` for new layout).
        // Prevents accidental full-snapshot open on every search.
        let Some(hashes) = self.storage().open_file_hashes().ok().flatten() else {
            return Ok(None);
        };
        let dirty = self.discover_dirty_sources();
        if dirty.is_empty() {
            return Ok(None);
        }
        // Compare only dirty paths against sidecar (small reads).
        let mut need_sync = false;
        for path in &dirty {
            let rel = path
                .strip_prefix(&self.root)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .replace('\\', "/");
            if !path.is_file() {
                if hashes.contains(&rel) {
                    need_sync = true;
                    break;
                }
                continue;
            }
            match hashes.get(&rel) {
                None => {
                    need_sync = true;
                    break;
                }
                Some(old) => {
                    let Ok(bytes) = std::fs::read(path) else {
                        need_sync = true;
                        break;
                    };
                    if blake3::hash(&bytes).to_hex().as_str() != old {
                        need_sync = true;
                        break;
                    }
                }
            }
        }
        if !need_sync {
            return Ok(None);
        }
        match self.sync(Some(&dirty)) {
            Ok(s) => Ok(Some(s)),
            // Keep serving the last complete snapshot; context/status expose the warning.
            Err(_) => Ok(None),
        }
    }

    pub fn list_packages(&self) -> Result<Vec<PackageInfo>, EngineError> {
        let snapshot = self.snapshot()?;
        Ok(analysis::list_packages(&snapshot))
    }

    pub fn export_dot(&self) -> Result<String, EngineError> {
        let graph = self.graph()?;
        Ok(analysis::export_package_dot(&graph))
    }

    pub fn diff_impact(
        &self,
        from_ref: &str,
        _to_ref: Option<&str>,
        limits: &QueryLimits,
    ) -> Result<ImpactReport, EngineError> {
        let paths = crate::git::changed_paths_between(&self.root, Some(from_ref), _to_ref)
            .map_err(|e| EngineError::Git(e.to_string()))?;
        let graph = self.graph()?;
        let mut merged: std::collections::BTreeMap<String, analysis::ImpactItem> =
            std::collections::BTreeMap::new();
        let mut truncated = false;
        let mut reason = None;
        let snapshot_id = graph.snapshot_id().to_owned();
        for path in paths {
            let path_str = path.to_string_lossy().replace('\\', "/");
            if !graph.contains_node(&path_str) {
                continue;
            }
            let report = analysis::impact_with_risk(&graph, &path_str, limits)?;
            truncated |= report.truncated;
            if report.reason.is_some() {
                reason = report.reason;
            }
            for item in report.affected {
                merged
                    .entry(item.symbol.clone())
                    .and_modify(|existing| {
                        if risk_worse(item.risk, existing.risk) {
                            *existing = item.clone();
                        }
                    })
                    .or_insert(item);
            }
        }
        // `merged` is a BTreeMap keyed by symbol → `into_values()` is already symbol-sorted.
        let affected: Vec<_> = merged.into_values().collect();
        Ok(ImpactReport {
            root: format!("diff:{from_ref}"),
            snapshot_id,
            total_affected: affected.len(),
            affected,
            truncated,
            reason,
        })
    }

    pub fn ci(&self, strict: bool, cycle_threshold: usize) -> Result<CiReport, EngineError> {
        let stats = self.stats()?;
        let cycles = self.cycles(None)?;
        let findings = self.validate().unwrap_or_default();
        let orphans = self.orphans(10_000).map(|o| o.len()).unwrap_or(0);
        Ok(analysis::ci_report(
            stats.snapshot_id,
            stats.files,
            stats.edges,
            &cycles,
            findings.len(),
            orphans,
            cycle_threshold,
            strict,
        ))
    }

    pub fn cochanged(
        &self,
        file: &str,
        commits: usize,
        min_cooccurrence: u32,
    ) -> Result<Vec<crate::git::CoChangeEntry>, EngineError> {
        crate::git::cochanged(&self.root, file, commits, min_cooccurrence)
            .map_err(|e| EngineError::Git(e.to_string()))
    }

    /// Candidate test paths for a source file (existence checked on disk).
    pub fn related_tests(&self, path: &str) -> Result<Vec<String>, EngineError> {
        let candidates = analysis::related_tests(path, &[]);
        let mut existing = Vec::new();
        for c in candidates {
            let full = if Path::new(&c).is_absolute() {
                PathBuf::from(&c)
            } else {
                self.root.join(&c)
            };
            if full.is_file() {
                existing.push(c);
            }
        }
        Ok(existing)
    }
}

fn risk_worse(a: analysis::RiskLevel, b: analysis::RiskLevel) -> bool {
    use analysis::RiskLevel::*;
    matches!((a, b), (High, Medium) | (High, Low) | (Medium, Low))
}

#[cfg(test)]
mod sync_queue_tests {
    use super::*;

    #[test]
    fn enqueue_enforces_path_byte_and_ticket_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = WorkspaceEngine::load(dir.path(), &Flags::default()).unwrap();
        engine.config.sync.queue_max_paths = 1;
        assert!(
            engine
                .enqueue_sync(&[PathBuf::from("a.ts"), PathBuf::from("b.ts")])
                .is_err()
        );

        engine.config.sync.queue_max_paths = 8;
        engine.config.sync.queue_max_ticket_bytes = 4;
        assert!(
            engine
                .enqueue_sync(&[PathBuf::from("long-name.ts")])
                .is_err()
        );

        engine.config.sync.queue_max_ticket_bytes = 1024;
        engine.config.sync.queue_max_tickets = 1;
        let first = engine.enqueue_sync(&[PathBuf::from("a.ts")]).unwrap();
        assert!(engine.enqueue_sync(&[PathBuf::from("b.ts")]).is_err());
        std::fs::remove_file(first).unwrap();
    }
}

#[cfg(test)]
mod symbol_name_delta_tests {
    use super::symbol_name_set_changed;
    use std::collections::BTreeMap;

    #[test]
    fn rename_with_the_same_names_reuses_search() {
        assert!(!symbol_name_set_changed(
            ["Shared", "OnlyHere"].into_iter(),
            ["Shared", "OnlyHere"].into_iter(),
            None,
        ));
    }

    #[test]
    fn add_and_delete_change_global_membership() {
        let counts = BTreeMap::from([("Existing".to_owned(), 1)]);
        assert!(symbol_name_set_changed(
            [].into_iter(),
            ["Added"].into_iter(),
            Some(&counts),
        ));
        assert!(symbol_name_set_changed(
            ["Existing"].into_iter(),
            [].into_iter(),
            Some(&counts),
        ));
    }

    #[test]
    fn homonymous_add_or_delete_preserves_the_global_search_set() {
        let counts = BTreeMap::from([("Shared".to_owned(), 2)]);
        assert!(!symbol_name_set_changed(
            [].into_iter(),
            ["Shared"].into_iter(),
            Some(&counts),
        ));
        assert!(!symbol_name_set_changed(
            ["Shared"].into_iter(),
            [].into_iter(),
            Some(&counts),
        ));
    }

    #[test]
    fn rename_that_changes_a_name_updates_search() {
        let counts = BTreeMap::from([("Before".to_owned(), 1)]);
        assert!(symbol_name_set_changed(
            ["Before"].into_iter(),
            ["After"].into_iter(),
            Some(&counts),
        ));
    }
}

#[cfg(test)]
mod generation_cache_lock_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn component_cache_fallback_does_not_reenter_generation_lock() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("entry.ts"), "export const entry = 1;\n").unwrap();
        let engine = WorkspaceEngine::load(root.path(), &Flags::default()).unwrap();
        engine.index().unwrap();

        // Model the lock order used by search/graph fallback: a component cache is already held
        // while the snapshot cache is materialized. Holding the generation marker here makes any
        // accidental call back through `snapshot()` deterministic instead of timing-dependent.
        let observed = engine.inner.observed_generation.lock().unwrap();
        let worker_engine = engine.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _component = worker_engine.inner.search_cache.lock().unwrap();
            sender
                .send(worker_engine.snapshot_after_refresh().is_ok())
                .unwrap();
        });

        let completed_without_reentry = receiver.recv_timeout(Duration::from_millis(250));
        drop(observed);
        worker.join().unwrap();
        assert!(completed_without_reentry.unwrap());
    }
}
