use crate::{
    analysis::{self, CiReport, CycleInfo, HubEntry, ImpactReport, PackageInfo},
    config::{Config, Flags},
    graph::{GraphIndex, QueryLimits, QueryPage},
    incremental_graph::{IncrementalGraphOverlay, IncrementalGraphState, OwnedEdge},
    model::{INDEX_SCHEMA_VERSION, IndexSnapshot, SnapshotId},
    policy::{PolicyFinding, Suppressions, validate_snapshot},
    resolver::{
        OverlayResolutionLookup, ResolutionLookup, ResolutionUniverseOverlay, load_tsconfig,
        resolve_edges_with_structural_data, resolve_subset_with_structural_data,
    },
    scanner::scan_workspace,
    search::{SearchHit, SearchIndex, SearchKind, SearchTermOverlay, SymbolTermDocument},
    storage::{
        FileSnapshotStorage, ResidentStructuralDelta, SnapshotStorage, StorageError,
        StructuralPackReader,
    },
    structural::StructuralReverseIndex,
    structural_reverse::{ReverseOverlaySet, ReverseShardSet},
};

pub use crate::model::IndexStats;
use rustc_hash::FxHashSet;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
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
    symbol_meta_cache: Mutex<Option<Arc<SymbolMetaRuntime>>>,
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
    structural_cache: Mutex<Option<(String, Arc<StructuralPackReader>)>>,
    /// Coalesce best-effort generation cleanup outside agent-facing sync latency.
    maintenance_scheduled: AtomicBool,
}

#[derive(Debug)]
struct SymbolMetaRuntime {
    dict: Arc<crate::model::SymbolMetaDict>,
}

impl SymbolMetaRuntime {
    fn new(dict: crate::model::SymbolMetaDict) -> Self {
        debug_assert!(dict.is_well_formed());
        Self {
            dict: Arc::new(dict),
        }
    }

    fn get_by_id(&self, id: &str) -> Option<&crate::model::SymbolMeta> {
        self.dict.get_by_id(id)
    }

    fn exact_id_or_qualified(&self, query: &str) -> Vec<&crate::model::SymbolMeta> {
        if let Some(entry) = self.get_by_id(query) {
            return vec![entry];
        }
        self.dict.entries_for_qualified(query)
    }
}

struct PreparedPath {
    path: PathBuf,
    relative: String,
    bytes: Option<Vec<u8>>,
    unchanged: bool,
}

const MAX_CONTEXT_SOURCE_BYTES: usize = 12 * 1024;
const MAX_CONTEXT_SOURCE_LINES: usize = 200;

/// Read only the selected declaration. Context must not hydrate every caller body: the agent can
/// open a related site when it decides to follow that edge.
fn symbol_source_excerpt(
    root: &Path,
    symbol: &crate::model::SymbolMeta,
) -> Option<serde_json::Value> {
    let mut file = File::open(root.join(&symbol.path)).ok()?;
    file.seek(SeekFrom::Start(symbol.span.start_byte as u64))
        .ok()?;
    let declared_len = symbol
        .span
        .end_byte
        .saturating_sub(symbol.span.start_byte)
        .max(1) as usize;
    let read_len = declared_len.min(MAX_CONTEXT_SOURCE_BYTES);
    let mut bytes = vec![0; read_len];
    let bytes_read = file.read(&mut bytes).ok()?;
    bytes.truncate(bytes_read);
    let text = String::from_utf8_lossy(&bytes);
    let start_line = symbol.span.start_line as usize + 1;
    let mut numbered = String::with_capacity(text.len().saturating_add(32));
    let mut line_count = 0usize;
    let mut has_more_lines = false;
    for (offset, line) in text.lines().enumerate() {
        if offset == MAX_CONTEXT_SOURCE_LINES {
            has_more_lines = true;
            break;
        }
        if offset > 0 {
            numbered.push('\n');
        }
        let _ = write!(numbered, "{}: {line}", start_line + offset);
        line_count += 1;
    }
    let truncated = declared_len > bytes_read || has_more_lines;
    Some(serde_json::json!({
        "text": numbered,
        "start_line": start_line,
        "end_line": start_line + line_count.saturating_sub(1),
        "truncated": truncated,
    }))
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

fn public_resolution_contract_changed(
    old: Option<&crate::model::FileArtifact>,
    new: Option<&crate::model::FileArtifact>,
) -> bool {
    match (old, new) {
        (Some(old), Some(new)) => {
            let same_symbols = old
                .symbols
                .iter()
                .filter(|symbol| symbol.exported)
                .map(|symbol| {
                    (
                        symbol.id.as_str(),
                        symbol.name.as_str(),
                        symbol.qualified_name.as_str(),
                        symbol.kind.as_ref(),
                    )
                })
                .eq(new
                    .symbols
                    .iter()
                    .filter(|symbol| symbol.exported)
                    .map(|symbol| {
                        (
                            symbol.id.as_str(),
                            symbol.name.as_str(),
                            symbol.qualified_name.as_str(),
                            symbol.kind.as_ref(),
                        )
                    }));
            let same_exports = old
                .exports
                .iter()
                .flat_map(|export| {
                    export.bindings.iter().map(move |binding| {
                        (
                            export.specifier.as_deref(),
                            binding.local.as_str(),
                            binding.exported.as_str(),
                            &binding.kind,
                            binding.type_only,
                        )
                    })
                })
                .eq(new.exports.iter().flat_map(|export| {
                    export.bindings.iter().map(move |binding| {
                        (
                            export.specifier.as_deref(),
                            binding.local.as_str(),
                            binding.exported.as_str(),
                            &binding.kind,
                            binding.type_only,
                        )
                    })
                }));
            !same_symbols || !same_exports
        }
        (None, None) => false,
        _ => true,
    }
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
                maintenance_scheduled: AtomicBool::new(false),
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
        let index_start = std::time::Instant::now();
        let result = self.index_unlocked();
        crate::timing::stage("index.total", index_start, String::new);
        self.finish_update("index", &result);
        result
    }

    fn index_unlocked(&self) -> Result<IndexStats, EngineError> {
        let scan_start = std::time::Instant::now();
        let (artifacts, scan_stats) = scan_workspace(&self.config)?;
        crate::timing::stage("index.scan", scan_start, || {
            format!("files={} bytes={}", artifacts.len(), scan_stats.bytes_read)
        });
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
        self.sync_inner(only_paths)
    }

    /// Daemon entry point retained as an explicit intent marker for callers. Structural state is
    /// shard-lazy for both daemon and CLI syncs, so behavior and cost are otherwise identical.
    pub fn sync_resident(&self, only_paths: Option<&[PathBuf]>) -> Result<IndexStats, EngineError> {
        self.sync_inner(only_paths)
    }

    /// Recover from a watch lost-event (rescan) signal. The signal only means "changed paths are
    /// unknown", not "the index is invalid": on git-backed roots dirty discovery re-enumerates
    /// the changed set in milliseconds and the incremental sync tiers do the rest. A full
    /// reindex here would hold the update lock for seconds per rescan — and its own sidecar
    /// writes can overflow the backend event queue and re-trigger the rescan signal in a loop.
    /// The full rebuild remains the fallback only where discovery cannot see changes.
    pub fn reconcile(&self) -> Result<IndexStats, EngineError> {
        if self.config.sync_allows_git() && self.is_git_repo_cached() {
            self.sync_inner(None)
        } else {
            self.index()
        }
    }

    fn sync_inner(&self, only_paths: Option<&[PathBuf]>) -> Result<IndexStats, EngineError> {
        let sync_total = std::time::Instant::now();
        let queued = only_paths.filter(|paths| !paths.is_empty());
        let lock_wait = std::time::Instant::now();
        let _guard = self.inner.update_lock.lock().unwrap();
        crate::timing::stage("sync.engine_lock_wait", lock_wait, String::new);
        if let Some(paths) = queued
            && let Some(_process_guard) = self.try_acquire_workspace_update_lock()?
        {
            // N=1: no ticket and no timer. Stale tickets are folded into this publication.
            let result = self.sync_queued_locked(paths, None);
            self.finish_update("sync", &result);
            crate::timing::stage("sync.inner_total", sync_total, String::new);
            return result;
        }
        let own_ticket = queued.map(|paths| self.enqueue_sync(paths)).transpose()?;
        let ws_lock_wait = std::time::Instant::now();
        let _process_guard = self.acquire_workspace_update_lock()?;
        crate::timing::stage("sync.workspace_lock_wait", ws_lock_wait, String::new);
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
        crate::timing::stage("sync.inner_total", sync_total, String::new);
        result
    }

    fn hydrate_structural_cache(&self) -> Result<(), EngineError> {
        let storage = self.storage();
        let Some(generation) = storage.current_generation()? else {
            return Ok(());
        };
        if self
            .inner
            .structural_cache
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(cached, ..)| cached == &generation)
        {
            return Ok(());
        }
        let Some(reader) = storage.open_structural_reader()? else {
            return Ok(());
        };
        *self.inner.structural_cache.lock().unwrap() = Some((generation, reader));
        Ok(())
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

    fn schedule_generation_maintenance(&self) {
        if self
            .inner
            .maintenance_scheduled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let storage = self.storage();
        let inner = Arc::clone(&self.inner);
        std::thread::spawn(move || {
            let _ = storage.gc_generations();
            inner.maintenance_scheduled.store(false, Ordering::Release);
        });
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
        let sync_start = std::time::Instant::now();
        let (fast_noop, prepared) = self.prepare_paths(&paths)?;
        crate::timing::stage("sync.prepare_paths", sync_start, || {
            format!("paths={}", paths.len())
        });
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

        let hydrate_start = std::time::Instant::now();
        self.hydrate_structural_cache()?;
        crate::timing::stage("sync.hydrate_structural_cache", hydrate_start, String::new);
        let delta_start = std::time::Instant::now();
        if let Some(stats) = self.try_resident_structural_delta(&prepared)? {
            crate::timing::stage("sync.structural_delta.total", delta_start, String::new);
            return Ok(stats);
        }
        crate::timing::stage("sync.structural_delta.bailed", delta_start, String::new);
        crate::timing::note("sync.fallback", || "partial-rebuild path".into());

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
                Ok(crate::scanner::parse_source(&rel, &bytes))
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
                                .map(|symbol| {
                                    (
                                        symbol.id.as_str(),
                                        symbol.name.as_str(),
                                        symbol.qualified_name.as_str(),
                                        symbol.kind.as_ref(),
                                        symbol.span,
                                        symbol.exported,
                                    )
                                })
                                .ne(artifact.symbols.iter().map(|symbol| {
                                    (
                                        symbol.id.as_str(),
                                        symbol.name.as_str(),
                                        symbol.qualified_name.as_str(),
                                        symbol.kind.as_ref(),
                                        symbol.span,
                                        symbol.exported,
                                    )
                                }))
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
        let symbol_names_changed = symbol_name_set_changed(old_names, new_names, None);
        let old_search_shape: BTreeSet<_> = overlay_paths
            .iter()
            .flat_map(|path| {
                old_changed
                    .get(path)
                    .and_then(Option::as_ref)
                    .into_iter()
                    .flat_map(move |artifact| {
                        artifact.symbols.iter().map(move |symbol| {
                            (
                                path.as_str(),
                                symbol.name.as_str(),
                                symbol.qualified_name.as_str(),
                                symbol.kind.as_ref(),
                            )
                        })
                    })
            })
            .collect();
        let new_search_shape: BTreeSet<_> = overlay_paths
            .iter()
            .flat_map(|path| {
                snapshot
                    .files
                    .get(path)
                    .into_iter()
                    .flat_map(move |artifact| {
                        artifact.symbols.iter().map(move |symbol| {
                            (
                                path.as_str(),
                                symbol.name.as_str(),
                                symbol.qualified_name.as_str(),
                                symbol.kind.as_ref(),
                            )
                        })
                    })
            })
            .collect();
        let search_terms_changed = symbol_names_changed || old_search_shape != new_search_shape;
        let reusable_edges = if !edge_inputs_changed {
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
        self.publish_from_artifacts(
            snapshot.files,
            bytes,
            parse_errors,
            reusable_edges,
            Some(&overlay_paths),
            None,
            search_terms_changed,
            content_state,
        )
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
            let mut artifact = crate::scanner::parse_source(&prepared.relative, bytes);
            artifact.path = prepared.relative.clone();
            // These fields feed global graph/search/detail sidecars. If any changes, the
            // current format must use the full correctness path until those indexes are sharded.
            let symbol_shape_changed = previous.symbols.len() != artifact.symbols.len()
                || previous
                    .symbols
                    .iter()
                    .zip(&artifact.symbols)
                    .any(|(old, new)| {
                        old.id != new.id
                            || old.name != new.name
                            || old.qualified_name != new.qualified_name
                            || old.kind != new.kind
                            || old.exported != new.exported
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
        let stats = storage.publish_artifact_deltas_deferred_gc(&deltas)?;
        self.schedule_generation_maintenance();
        storage.compact_artifacts_if_amplified(
            self.config.storage.artifact_store_max_amplification,
            self.config.storage.retention,
        )?;
        Ok(stats)
    }

    /// Fast structural path for in-place edits. It loads only changed/affected artifacts and
    /// publishes component overlays; the full snapshot is reconstructed only on explicit demand.
    fn try_resident_structural_delta(
        &self,
        prepared: &[PreparedPath],
    ) -> Result<Option<IndexStats>, EngineError> {
        let storage = self.storage();
        let mut changes = Vec::new();
        for prepared in prepared.iter().filter(|prepared| !prepared.unchanged) {
            let old = storage.open_artifact(&prepared.relative)?;
            let new = prepared.bytes.as_ref().map(|bytes| {
                let mut artifact = crate::scanner::parse_source(&prepared.relative, bytes);
                artifact.path = prepared.relative.clone();
                artifact
            });
            if old.is_none() && new.is_none() {
                continue;
            }
            if old
                .as_ref()
                .zip(new.as_ref())
                .is_some_and(|(old, new)| old.language != new.language)
            {
                return Ok(None);
            }
            changes.push((prepared.relative.clone(), old, new));
        }
        if changes.is_empty() {
            return Ok(None);
        }

        let Some(current_generation) = storage.current_generation()? else {
            return Ok(None);
        };
        let reader = self
            .inner
            .structural_cache
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(generation, ..)| generation == &current_generation)
            .map(|(_, reader)| Arc::clone(reader));
        let Some(reader) = reader else {
            return Ok(None);
        };
        let resolver_config = load_tsconfig(&self.root);
        if !reader.matches(&resolver_config) {
            return Ok(None);
        }

        let changed_paths: BTreeSet<_> = changes.iter().map(|(path, ..)| path.clone()).collect();
        let mut changed_symbols = BTreeSet::new();
        let mut contract_changed_paths = BTreeSet::new();
        for (path, old, new) in &changes {
            if public_resolution_contract_changed(old.as_ref(), new.as_ref()) {
                contract_changed_paths.insert(path.clone());
                changed_symbols.extend(
                    old.iter()
                        .flat_map(|artifact| artifact.symbols.iter())
                        .chain(new.iter().flat_map(|artifact| artifact.symbols.iter()))
                        .filter(|symbol| symbol.exported)
                        .map(|symbol| symbol.name.clone()),
                );
            }
        }
        let universe_overlay = ResolutionUniverseOverlay::from_artifact_changes(
            reader.as_ref(),
            changes
                .iter()
                .map(|(_, old, new)| (old.as_ref(), new.as_ref())),
        );
        let universe = OverlayResolutionLookup::new(reader.as_ref(), &universe_overlay);
        let affected_start = std::time::Instant::now();
        let mut affected = reader.affected_files(
            contract_changed_paths.iter().map(String::as_str),
            changed_symbols.iter().map(String::as_str),
        );
        affected.extend(changed_paths.iter().cloned());
        crate::timing::stage("delta.affected_files", affected_start, || {
            format!(
                "affected={} changed_symbols={} contract_changed={}",
                affected.len(),
                changed_symbols.len(),
                contract_changed_paths.len()
            )
        });

        let changed_artifacts: BTreeMap<_, _> = changes
            .iter()
            .map(|(path, _, new)| (path.as_str(), new.as_ref()))
            .collect();
        let mut subset = BTreeMap::new();
        for path in &affected {
            let artifact = if let Some(new) = changed_artifacts.get(path.as_str()) {
                let Some(new) = new else {
                    continue;
                };
                (*new).clone()
            } else if let Some(artifact) = storage.open_artifact(path)? {
                artifact
            } else {
                return Ok(None);
            };
            subset.insert(path.clone(), artifact);
        }
        let resolve_start = std::time::Instant::now();
        let Some((traces, mut contributions)) =
            resolve_subset_with_structural_data(&self.root, &subset, &universe, &resolver_config)
        else {
            return Ok(None);
        };
        crate::timing::stage("delta.resolve_subset", resolve_start, || {
            format!("subset={} traces={}", subset.len(), traces.len())
        });
        let graph_updates: BTreeMap<_, _> = affected
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    subset.contains_key(path).then(|| {
                        contributions
                            .remove(path)
                            .unwrap_or_default()
                            .iter()
                            .map(OwnedEdge::from)
                            .collect()
                    }),
                )
            })
            .collect();
        let graph_start = std::time::Instant::now();
        let mut graph = reader.graph_for_updates(&graph_updates);
        crate::timing::stage("delta.graph_for_updates", graph_start, || {
            format!("updates={}", graph_updates.len())
        });
        let graph_edges_before = graph.edge_count();
        let replace_start = std::time::Instant::now();
        let graph_overlay = graph.replace_owned_files(graph_updates);
        crate::timing::stage("delta.graph_replace", replace_start, || {
            format!(
                "upserts={} edge_counts={}",
                graph_overlay.file_upserts.len(),
                graph_overlay.edge_counts.len()
            )
        });
        let graph_edges_after = graph.edge_count();
        let mut trace_cursor = 0usize;
        let mut reverse_updates = BTreeMap::new();
        for path in &affected {
            let trace_start = trace_cursor;
            while traces
                .get(trace_cursor)
                .is_some_and(|trace| trace.importer == *path)
            {
                trace_cursor += 1;
            }
            let contribution = subset.get(path).map(|artifact| {
                crate::structural::FileContribution::from_artifact(
                    artifact,
                    traces[trace_start..trace_cursor].iter(),
                )
            });
            reverse_updates.insert(path.clone(), contribution);
        }
        let reverse_start = std::time::Instant::now();
        let mut reverse = reader.reverse_for_updates(&reverse_updates);
        let reverse_overlay = reverse.replace_files(reverse_updates);
        crate::timing::stage("delta.reverse", reverse_start, String::new);

        let Some(old_stats) = storage.open_stats()? else {
            return Ok(None);
        };
        let (bytes, parse_errors) =
            changes.iter().fold(
                (old_stats.bytes, old_stats.parse_errors),
                |(bytes, errors), (_, old, new)| {
                    (
                        bytes
                            .saturating_sub(old.as_ref().map_or(0, |artifact| artifact.bytes_read))
                            .saturating_add(new.as_ref().map_or(0, |artifact| artifact.bytes_read)),
                        errors
                            .saturating_sub(old.as_ref().map_or(0, |artifact| {
                                usize::from(!artifact.diagnostics.is_empty())
                            }))
                            .saturating_add(new.as_ref().map_or(0, |artifact| {
                                usize::from(!artifact.diagnostics.is_empty())
                            })),
                    )
                },
            );
        let content_changes = changes
            .iter()
            .map(|(path, old, new)| {
                (
                    path.clone(),
                    old.as_ref().map(|artifact| artifact.source_hash.clone()),
                    new.as_ref().map(|artifact| artifact.source_hash.clone()),
                )
            })
            .collect::<Vec<_>>();
        let Some(content_state) = storage.prospective_content_state(&content_changes) else {
            return Ok(None);
        };
        let identity = self.worktree_identity_cached();
        let mut snapshot_id = storage
            .read_manifest()?
            .ok_or_else(|| StorageError::Invalid {
                path: self.storage_path(),
                message: "structural delta requires a current manifest".into(),
            })?
            .snapshot_id;
        snapshot_id.root = identity.root.to_string_lossy().into_owned();
        snapshot_id.worktree = identity.worktree;
        snapshot_id.revision = identity.revision;
        snapshot_id.content_state = content_state;
        snapshot_id.grammar_version = crate::scanner::GRAMMAR_VERSION.into();
        snapshot_id.config_hash = self.inner.config_hash.clone();
        let generation = snapshot_id.stable_key();
        let stats = IndexStats {
            files: changes
                .iter()
                .fold(old_stats.files, |files, (_, old, new)| {
                    files
                        .saturating_sub(usize::from(old.is_some() && new.is_none()))
                        .saturating_add(usize::from(old.is_none() && new.is_some()))
                }),
            edges: if graph_edges_after >= graph_edges_before {
                old_stats
                    .edges
                    .saturating_add(graph_edges_after - graph_edges_before)
            } else {
                old_stats
                    .edges
                    .saturating_sub(graph_edges_before - graph_edges_after)
            },
            bytes,
            parse_errors,
            snapshot_id: generation.clone(),
        };
        let meta_changes = changes
            .iter()
            .map(|(_, old, new)| (old.as_ref(), new.as_ref()))
            .collect::<Vec<_>>();
        let symbol_meta_overlay = crate::model::SymbolMetaOverlay::from_artifact_changes(
            generation.clone(),
            &meta_changes,
        );
        let search_changed = changes.iter().any(|(_, old, new)| match (old, new) {
            (Some(old), Some(new)) => {
                old.symbols.len() != new.symbols.len()
                    || old.symbols.iter().zip(&new.symbols).any(|(old, new)| {
                        old.id != new.id
                            || old.name != new.name
                            || old.qualified_name != new.qualified_name
                            || old.kind != new.kind
                    })
            }
            _ => true,
        });
        let search_overlay = search_changed.then(|| {
            let removed_ids = changes
                .iter()
                .flat_map(|(_, old, _)| {
                    old.iter()
                        .flat_map(|artifact| artifact.symbols.iter())
                        .map(|symbol| symbol.id.clone())
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let added_names = changes
                .iter()
                .flat_map(|(_, _, new)| {
                    new.iter()
                        .flat_map(|artifact| artifact.symbols.iter())
                        .map(|symbol| symbol.name.clone())
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let removed_names = changes
                .iter()
                .flat_map(|(_, old, _)| {
                    old.iter()
                        .flat_map(|artifact| artifact.symbols.iter())
                        .map(|symbol| symbol.name.clone())
                })
                .filter(|name| universe.symbol_definer_count(name) == 0)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let documents = changes
                .iter()
                .flat_map(|(path, _, new)| {
                    new.iter()
                        .flat_map(|artifact| artifact.symbols.iter())
                        .map(|symbol| SymbolTermDocument::from_symbol(path, symbol))
                })
                .collect();
            SearchTermOverlay {
                snapshot_id: generation.clone(),
                removed_ids,
                added_names,
                removed_names,
                documents,
            }
        });
        let artifacts = changes
            .iter()
            .map(|(path, _, new)| (path.clone(), new.clone()))
            .collect::<Vec<_>>();
        if let Some(error) = reader.take_error() {
            self.record_update_error("structural shard", &error);
            return Ok(None);
        }
        let publish_start = std::time::Instant::now();
        if !storage.publish_resident_structural_delta(ResidentStructuralDelta {
            snapshot_id: &snapshot_id,
            artifacts: &artifacts,
            graph: &graph_overlay,
            universe: &universe_overlay,
            reverse: &reverse_overlay,
            symbol_meta: &symbol_meta_overlay,
            search: search_overlay.as_ref(),
            stats: &stats,
        })? {
            return Ok(None);
        }
        crate::timing::stage("delta.publish", publish_start, String::new);

        // Advance the resident structural reader to the new generation by applying the overlay
        // we just published. Dropping it instead would force the next sync to re-hydrate and
        // re-decode every touched shard from the base pack. Our local Arc must be released
        // first; `try_unwrap` fails only when a concurrent reader still holds one, and then we
        // fall back to invalidation.
        let _ = universe;
        drop(reader);
        let advance_start = std::time::Instant::now();
        {
            let mut cache_slot = self.inner.structural_cache.lock().unwrap();
            *cache_slot = cache_slot.take().and_then(|(_, cached)| {
                let mut owned = Arc::try_unwrap(cached).ok()?;
                let generation = storage.current_generation().ok().flatten()?;
                owned
                    .apply_published_delta(&graph_overlay, &universe_overlay, &reverse_overlay)
                    .then_some(())?;
                Some((generation, Arc::new(owned)))
            });
        }
        crate::timing::stage("delta.advance_reader", advance_start, String::new);
        *self.inner.snapshot_cache.lock().unwrap() = None;
        *self.inner.graph_cache.lock().unwrap() = None;
        *self.inner.symbol_meta_cache.lock().unwrap() = None;
        if search_overlay.is_some() {
            *self.inner.search_cache.lock().unwrap() = None;
        }
        *self.inner.dirty_cache.lock().unwrap() = None;
        self.remember_published_generation();
        self.schedule_generation_maintenance();
        Ok(Some(stats))
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
        reason = "publication inputs explicitly define one atomic generation"
    )]
    fn publish_from_artifacts(
        &self,
        files: std::collections::BTreeMap<String, crate::model::FileArtifact>,
        bytes_read: u64,
        parse_errors: usize,
        reusable_edges: Option<Vec<crate::model::Edge>>,
        overlay_paths: Option<&BTreeSet<String>>,
        structural_overlay: Option<(
            crate::resolver::ResolutionUniverseOverlay,
            ReverseOverlaySet,
            IncrementalGraphOverlay,
        )>,
        search_terms_changed: bool,
        content_state_override: Option<String>,
    ) -> Result<IndexStats, EngineError> {
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
            schema_version: INDEX_SCHEMA_VERSION,
            grammar_version: crate::scanner::GRAMMAR_VERSION.into(),
            config_hash: self.inner.config_hash.clone(),
        };
        let storage = self.storage();
        let (edges, staged_pack) = if overlay_paths.is_some()
            && let Some(edges) = reusable_edges
        {
            (edges, None)
        } else {
            let resolver = load_tsconfig(&self.root);
            let resolve_start = std::time::Instant::now();
            let (resolved_edges, traces, universe) =
                resolve_edges_with_structural_data(&self.root, &files, &resolver);
            crate::timing::stage("index.resolve_edges", resolve_start, || {
                format!("edges={} traces={}", resolved_edges.len(), traces.len())
            });
            // Stream each structural state into the staged pack as soon as it is built and
            // drop it before building the next: universe, reverse, and graph held live
            // together set the full-index RSS plateau (~3.3GB on the real corpus).
            let mut stager = storage.begin_structural_pack_base(id.stable_key())?;
            let stage_start = std::time::Instant::now();
            stager.stage_universe(universe)?;
            crate::timing::stage("index.stage_universe", stage_start, String::new);
            let reverse_start = std::time::Instant::now();
            let reverse_index =
                StructuralReverseIndex::build_from_traces(&files, &resolver, &traces);
            crate::timing::stage("index.reverse_build", reverse_start, String::new);
            drop(traces);
            let reverse_shard_start = std::time::Instant::now();
            let reverse = ReverseShardSet::from_owned_index(reverse_index, 8).map_err(|error| {
                EngineError::Storage(StorageError::Invalid {
                    path: self.root.join(&self.config.storage.home),
                    message: error.to_string(),
                })
            })?;
            crate::timing::stage("index.reverse_shard", reverse_shard_start, String::new);
            let stage_start = std::time::Instant::now();
            stager.stage_reverse(reverse)?;
            crate::timing::stage("index.stage_reverse", stage_start, String::new);
            let graph_start = std::time::Instant::now();
            let graph = IncrementalGraphState::from_edges(&resolved_edges);
            crate::timing::stage("index.graph_from_edges", graph_start, String::new);
            let stage_start = std::time::Instant::now();
            let graph_staged = stager.stage_graph(graph);
            crate::timing::stage("index.stage_graph", stage_start, String::new);
            graph_staged?;
            (resolved_edges, Some(stager.finish()?))
        };
        let snapshot = IndexSnapshot { id, files, edges };
        let staged_overlay = structural_overlay.as_ref().map(
            |(universe_overlay, reverse_overlay, graph_overlay)| {
                (graph_overlay, universe_overlay, reverse_overlay)
            },
        );
        let published_overlay = if staged_pack.is_none() {
            overlay_paths
                .map(|paths| {
                    storage.publish_structural_overlay_deferred_gc(
                        &snapshot,
                        paths,
                        staged_overlay,
                        search_terms_changed,
                        Some((bytes_read, parse_errors)),
                    )
                })
                .transpose()?
                .unwrap_or(false)
        } else {
            false
        };
        if published_overlay {
            self.schedule_generation_maintenance();
        }
        if !published_overlay {
            let publish_start = std::time::Instant::now();
            storage.publish(&snapshot)?;
            crate::timing::stage("index.publish", publish_start, String::new);
            if let Some(staged_pack) = staged_pack {
                let attach_start = std::time::Instant::now();
                storage.attach_structural_pack_base(staged_pack)?;
                crate::timing::stage("index.attach_pack", attach_start, String::new);
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
        // Use raw paths — auto_sync already ran. Context combines deterministic spelling
        // lookup with definition-level term evidence: a one-word concept may occur in a path or
        // qualified name, while prose intent words must not turn the query into a hard AND.
        let mut hits = self.search_raw(query, SearchKind::Prefix, limit.saturating_add(1))?;
        let mut term_hits =
            self.search_raw(query, SearchKind::Terms, limit.saturating_mul(16).max(128))?;
        if !term_hits.is_empty() {
            let ranking_graph = self.graph()?;
            for hit in &mut term_hits {
                if let Some(id) = hit.definition_id.as_deref() {
                    let degree = ranking_graph.direct_relations_limit(id, true, 0).1
                        + ranking_graph.direct_relations_limit(id, false, 0).1;
                    hit.score_micros = hit
                        .score_micros
                        .saturating_add((degree as u64).min(40) * 500)
                        .min(999_999);
                }
            }
        }
        hits.extend(term_hits);
        hits.sort_by(|left, right| {
            right
                .score_micros
                .cmp(&left.score_micros)
                .then_with(|| left.value.cmp(&right.value))
                .then_with(|| left.definition_id.cmp(&right.definition_id))
        });
        hits.dedup_by(|left, right| {
            left.value == right.value && left.definition_id == right.definition_id
        });
        if let Some(best_term_score) = hits
            .iter()
            .filter(|hit| hit.reason.as_deref() == Some("term-coverage"))
            .map(|hit| hit.score_micros)
            .max()
        {
            // Keep close alternatives for honest ambiguity, but do not promote documents that
            // matched only generic residue from a natural-language query.
            let floor = best_term_score.saturating_sub(50_000);
            hits.retain(|hit| {
                hit.reason.as_deref() != Some("term-coverage") || hit.score_micros >= floor
            });
        }
        let symbol_runtime = self.symbol_meta_runtime()?.unwrap_or_else(|| {
            Arc::new(SymbolMetaRuntime::new(crate::model::SymbolMetaDict {
                format_version: crate::model::SymbolMetaDict::FORMAT_VERSION,
                snapshot_id: String::new(),
                entries: Vec::new(),
                duplicates: Vec::new(),
                id_order: Vec::new(),
                qualified_order: Vec::new(),
            }))
        });
        let symbol_meta = Arc::clone(&symbol_runtime.dict);
        let exact_identity = symbol_runtime.exact_id_or_qualified(query);
        let exact_primary = (exact_identity.len() == 1).then(|| exact_identity[0]);
        let required_terms = crate::search::query_tokens(query);
        let mut candidates = Vec::new();
        let mut candidate_entries = Vec::new();
        let mut candidate_scores = Vec::new();
        let mut candidate_reasons = Vec::new();
        let mut candidate_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut seen_ids = BTreeSet::new();
        let mut candidate_total = 0usize;
        for entry in &exact_identity {
            *candidate_counts.entry(entry.name.clone()).or_default() += 1;
            candidate_total += 1;
            seen_ids.insert(entry.id.clone());
            if candidates.len() < limit {
                candidate_entries.push((*entry).clone());
                candidate_scores.push(1_250_000_u64);
                candidate_reasons.push("exact-qualified".to_owned());
                candidates.push(serde_json::json!({
                    "id": entry.id,
                    "name": entry.name,
                    "qualified": entry.qualified_name,
                    "kind": entry.kind,
                    "path": entry.path,
                    "line": entry.span.start_line + 1,
                    "end_line": entry.span.end_line + 1,
                    "score": 1_250_000_u64,
                    "why": "exact-qualified",
                }));
            }
        }
        let mut add_candidate = |entry: &crate::model::SymbolMeta, hit: &SearchHit| {
            if seen_ids.insert(entry.id.clone()) {
                *candidate_counts.entry(entry.name.clone()).or_default() += 1;
                candidate_total += 1;
                if candidates.len() < limit {
                    candidate_entries.push(entry.clone());
                    candidate_scores.push(hit.score_micros);
                    candidate_reasons.push(hit.reason.clone().unwrap_or_default());
                    candidates.push(serde_json::json!({
                        "id": entry.id,
                        "name": entry.name,
                        "qualified": entry.qualified_name,
                        "kind": entry.kind,
                        "path": entry.path,
                        "line": entry.span.start_line + 1,
                        "end_line": entry.span.end_line + 1,
                        "score": hit.score_micros,
                        "why": hit.reason,
                    }));
                }
            }
        };
        for hit in &hits {
            if let Some(id) = hit.definition_id.as_deref() {
                if let Some(entry) = symbol_runtime.get_by_id(id) {
                    add_candidate(entry, hit);
                }
                continue;
            }
            for entry in symbol_meta.entries_for(&hit.value) {
                if hit.reason.as_deref() != Some("term-coverage")
                    || crate::search::symbol_meta_matches_query_tokens(&required_terms, entry)
                {
                    add_candidate(entry, hit);
                }
            }
        }

        let primary = exact_identity
            .first()
            .map(|entry| entry.name.clone())
            .or_else(|| candidate_entries.first().map(|entry| entry.name.clone()))
            .unwrap_or_else(|| query.to_owned());
        let mut matches = Vec::new();
        let mut matched_names: FxHashSet<&str> = FxHashSet::default();
        for entry in &exact_identity {
            if matched_names.insert(entry.name.as_str()) {
                matches.push(entry.name.clone());
            }
        }
        for hit in &hits {
            if candidate_counts.contains_key(&hit.value) && matched_names.insert(hit.value.as_str())
            {
                matches.push(hit.value.clone());
            }
        }

        // A simple spelling can name hundreds of methods. Choosing the first path would create a
        // convincing but false context. Resolve only a unique definition or an exact qualified id.
        let primary_meta = exact_primary.or_else(|| {
            let first = candidate_entries.first()?;
            if candidate_reasons
                .first()
                .is_some_and(|why| why == "term-coverage")
            {
                let uniquely_ranked = candidate_scores
                    .get(1)
                    .is_none_or(|runner_up| candidate_scores[0] > *runner_up);
                return uniquely_ranked.then_some(first);
            }
            (candidate_counts.get(&primary) == Some(&1)).then_some(first)
        });
        let ambiguous = primary_meta.is_none()
            && candidate_counts
                .get(&primary)
                .is_some_and(|count| *count > 1);
        let graph_primary = primary_meta.map(|entry| entry.id.as_str());
        let impact_limits = QueryLimits {
            depth: 2,
            nodes: 50,
            edges: 200,
            page_size: 20,
            ..Default::default()
        };
        let display_node = |node: &str| {
            symbol_runtime
                .get_by_id(node)
                .map(|entry| entry.qualified_name.clone())
                .unwrap_or_else(|| node.to_owned())
        };
        let graph = graph_primary.map(|_| self.graph()).transpose()?;
        let impact = graph_primary.and_then(|node| {
            graph
                .as_deref()
                .and_then(|graph| analysis::impact_with_risk(graph, node, &impact_limits).ok())
        });
        let (incoming_sites, incoming_total) = graph_primary
            .and_then(|node| {
                graph
                    .as_deref()
                    .map(|graph| graph.direct_relations_limit(node, true, limit))
            })
            .unwrap_or_default();
        let (outgoing_sites, outgoing_total) = graph_primary
            .and_then(|node| {
                graph
                    .as_deref()
                    .map(|graph| graph.direct_relations_limit(node, false, limit))
            })
            .unwrap_or_default();
        let relation_names = |relations: &[crate::graph::RelationView]| {
            let mut seen = BTreeSet::new();
            relations
                .iter()
                .filter(|relation| {
                    !matches!(
                        relation.kind,
                        crate::model::EdgeKind::Import | crate::model::EdgeKind::ReExport
                    )
                })
                .filter_map(|relation| {
                    let display = display_node(&relation.node);
                    seen.insert(display.clone()).then_some(display)
                })
                .collect::<Vec<_>>()
        };
        let callers_names = relation_names(&incoming_sites);
        let callees_names = relation_names(&outgoing_sites);
        let impact_top: Vec<_> = impact
            .as_ref()
            .map(|r| {
                r.affected
                    .iter()
                    .take(limit)
                    .map(|i| {
                        serde_json::json!({
                            "s": display_node(&i.symbol),
                            "risk": i.risk,
                            "d": i.depth,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let relation_json = |relations: Vec<crate::graph::RelationView>| {
            relations
                .into_iter()
                .map(|relation| {
                    let related = symbol_runtime.get_by_id(&relation.node);
                    serde_json::json!({
                        "id": relation.node,
                        "name": related.map(|entry| entry.qualified_name.as_str()).unwrap_or(&relation.node),
                        "kind": relation.kind.as_str(),
                        "path": related.map(|entry| entry.path.as_str()),
                        "site": {
                            "path": relation.source_path,
                            "line": relation.span.map(|span| span.start_line + 1),
                            "end_line": relation.span.map(|span| span.end_line + 1),
                        },
                        "confidence": relation.confidence,
                        "provenance": relation.provenance,
                        "type_only": relation.type_only,
                    })
                })
                .collect::<Vec<_>>()
        };
        let incoming_relations = relation_json(incoming_sites);
        let outgoing_relations = relation_json(outgoing_sites);
        let caller_count = callers_names.len();
        let source = primary_meta.and_then(|symbol| symbol_source_excerpt(&self.root, symbol));
        let candidates_truncated = candidate_total > candidates.len();
        let no_result = candidate_total == 0;
        Ok(serde_json::json!({
            "q": query,
            "primary": primary,
            "matches": matches,
            "candidates": candidates,
            "ambiguous": ambiguous,
            "detail": primary_meta.map(|d| serde_json::json!({
                "id": d.id, "name": d.name, "qualified": d.qualified_name,
                "kind": d.kind, "exported": d.exported, "path": d.path,
                "line": d.span.start_line + 1, "end_line": d.span.end_line + 1
            })),
            "source": source,
            "callers": callers_names,
            "callees": callees_names,
            "relations": {
                "incoming": incoming_relations,
                "outgoing": outgoing_relations,
            },
            "impact": impact_top,
            "n_callers": caller_count,
            "n_affected": impact.as_ref().map(|r| r.total_affected).unwrap_or(0),
            "auto_synced": synced.is_some(),
            "sync_warning": self.last_update_error(),
            "warnings": if ambiguous {
                vec!["ambiguous symbol name; select a candidate id or qualified name"]
            } else if no_result {
                vec!["no definition matched search evidence"]
            } else {
                Vec::<&str>::new()
            },
            "truncation": {
                "candidates": candidates_truncated,
                "incoming": incoming_total > limit,
                "outgoing": outgoing_total > limit,
            },
            "coverage": {
                "imports_exports": "static",
                "calls": "static_syntax",
                "types": "static_syntax",
                "instantiation": "static_syntax",
                "decorators": "static_syntax",
                "value_references": "partial",
                "dynamic_dispatch": "unsupported",
                "authoritative_zero": false,
            },
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
            let needs_tantivy = matches!(
                kind,
                SearchKind::Fuzzy | SearchKind::Regex | SearchKind::Terms
            );
            if !needs_tantivy || index.backend_label() != "dict" {
                return Ok(Arc::clone(index));
            }
        }
        let needs_tantivy = matches!(
            kind,
            SearchKind::Fuzzy | SearchKind::Regex | SearchKind::Terms
        );
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
        Ok(self
            .symbol_meta_runtime()?
            .map(|runtime| Arc::clone(&runtime.dict)))
    }

    fn symbol_meta_runtime(&self) -> Result<Option<Arc<SymbolMetaRuntime>>, EngineError> {
        self.refresh_external_generation()?;
        let mut cache = self.inner.symbol_meta_cache.lock().unwrap();
        if let Some(meta) = cache.as_ref() {
            return Ok(Some(Arc::clone(meta)));
        }
        let Some(meta) = self.storage().open_symbol_meta()? else {
            return Ok(None);
        };
        let meta = Arc::new(SymbolMetaRuntime::new(meta));
        *cache = Some(Arc::clone(&meta));
        Ok(Some(meta))
    }

    fn resolve_graph_node(&self, graph: &GraphIndex, node: &str) -> String {
        if graph.contains_node(node) {
            return node.to_owned();
        }
        self.symbol_meta_runtime()
            .ok()
            .flatten()
            .and_then(|runtime| {
                let qualified = runtime.exact_id_or_qualified(node);
                if qualified.len() == 1 {
                    return Some(qualified[0].id.clone());
                }
                let mut by_name = runtime.dict.entries_for(node);
                let first = by_name.next()?;
                by_name.next().is_none().then(|| first.id.clone())
            })
            .unwrap_or_else(|| node.to_owned())
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
        let resolved_node = self.resolve_graph_node(&graph, node);
        Ok(if reverse {
            graph.callers_of(&resolved_node, limits, cancel)?
        } else {
            graph.impact_analysis(&resolved_node, limits, cancel)?
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
        if let Some(runtime) = self.symbol_meta_runtime()? {
            return match runtime
                .get_by_id(symbol)
                .or_else(|| runtime.dict.get(symbol))
            {
                Some(m) => {
                    // Delta generations keep the global name→path sidecar when symbol shape is
                    // unchanged, then read current span/complexity from the artifact overlay.
                    if let Some(artifact) = self.storage().open_artifact(&m.path)?
                        && let Some(current) = artifact
                            .symbols
                            .iter()
                            .filter(|s| s.id == m.id || s.name == symbol)
                            .max_by_key(|s| s.span)
                    {
                        return Ok(Some(current.clone()));
                    }
                    Ok(Some(crate::model::Symbol {
                        id: m.id.clone(),
                        name: m.name.clone(),
                        qualified_name: m.qualified_name.clone(),
                        kind: m.kind.clone(),
                        span: m.span,
                        exported: m.exported,
                        complexity: m.complexity.clone(),
                        scope: None,
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
        let resolved_node = self.resolve_graph_node(&graph, node);
        Ok(analysis::impact_with_risk(&graph, &resolved_node, limits)?)
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
mod resident_sync_tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, WorkspaceEngine, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let service = root.path().join("service.ts");
        std::fs::write(&service, "export const answer = () => 42;\n").unwrap();
        std::fs::write(
            root.path().join("consumer.ts"),
            "import { answer } from './service';\nexport const value = answer();\n",
        )
        .unwrap();
        let engine = WorkspaceEngine::load(root.path(), &Flags::default()).unwrap();
        engine.index().unwrap();
        (root, engine, service)
    }

    #[test]
    fn resident_sync_publishes_structural_overlays_and_keeps_exact_edges() {
        let (root, engine, service) = fixture();
        let base = engine.storage().read_manifest().unwrap().unwrap();
        let base_pack = base.structural_packs.unwrap().base;

        std::fs::write(
            &service,
            "// shifted declaration\nexport const answer = () => 42;\n",
        )
        .unwrap();
        engine
            .sync_resident(Some(std::slice::from_ref(&service)))
            .unwrap();

        let current = engine.storage().read_manifest().unwrap().unwrap();
        let chain = current.structural_packs.unwrap();
        assert_eq!(chain.base, base_pack);
        assert_eq!(chain.overlays.len(), 1);
        assert_eq!(engine.stats().unwrap().edges, 3);

        let cold_reader = WorkspaceEngine::load(root.path(), &Flags::default()).unwrap();
        let context = cold_reader.context("answer", 10).unwrap();
        assert!(
            context["relations"]["incoming"]
                .as_array()
                .unwrap()
                .iter()
                .any(|relation| relation["kind"] == "Calls")
        );
    }

    #[test]
    fn resident_content_only_sync_does_not_hydrate_workspace_structural_state() {
        let (_root, engine, service) = fixture();
        assert!(engine.inner.structural_cache.lock().unwrap().is_none());

        std::fs::write(&service, "export const answer = () => 43;\n").unwrap();
        engine
            .sync_resident(Some(std::slice::from_ref(&service)))
            .unwrap();

        assert!(engine.inner.structural_cache.lock().unwrap().is_none());
        assert_eq!(
            engine.context("answer", 10).unwrap()["detail"]["name"],
            "answer"
        );
    }

    #[test]
    fn resident_rename_is_visible_to_cold_search_without_rebuilding_the_base() {
        let (root, engine, service) = fixture();
        let before = engine.storage().read_manifest().unwrap().unwrap();
        let base_search = before.search_dir.unwrap();

        std::fs::write(&service, "export const result = () => 42;\n").unwrap();
        engine
            .sync_resident(Some(std::slice::from_ref(&service)))
            .unwrap();

        let current = engine.storage().read_manifest().unwrap().unwrap();
        assert_eq!(current.search_dir.as_deref(), Some(base_search.as_str()));
        assert_eq!(current.search_overlays.len(), 1);

        let cold_reader = WorkspaceEngine::load(root.path(), &Flags::default()).unwrap();
        assert!(
            cold_reader
                .search_raw("answer", SearchKind::Exact, 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            cold_reader
                .search_raw("result", SearchKind::Exact, 10)
                .unwrap()[0]
                .value,
            "result"
        );
        assert_eq!(
            cold_reader
                .search_raw("result service", SearchKind::Terms, 10)
                .unwrap()[0]
                .value,
            "result"
        );

        std::fs::write(&service, "export const finalResult = () => 42;\n").unwrap();
        engine
            .sync_resident(Some(std::slice::from_ref(&service)))
            .unwrap();
        let second_reader = WorkspaceEngine::load(root.path(), &Flags::default()).unwrap();
        assert!(
            second_reader
                .search_raw("result", SearchKind::Exact, 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            second_reader
                .search_raw("final result service", SearchKind::Terms, 10)
                .unwrap()[0]
                .value,
            "finalResult"
        );
    }

    #[test]
    fn cold_structural_sync_appends_a_sharded_overlay_without_rebuilding_the_base() {
        let (_root, engine, service) = fixture();
        let base = engine
            .storage()
            .read_manifest()
            .unwrap()
            .unwrap()
            .structural_packs
            .unwrap()
            .base;
        std::fs::write(&service, "export const answer = (value = 42) => value;\n").unwrap();

        engine.sync(Some(&[service])).unwrap();

        let current = engine.storage().read_manifest().unwrap().unwrap();
        let chain = current
            .structural_packs
            .as_ref()
            .unwrap_or_else(|| panic!("sync must retain structural shards: {current:?}"));
        assert_eq!(chain.base, base);
        assert_eq!(chain.overlays.len(), 1);
        assert_eq!(chain.current_snapshot, current.snapshot_id.stable_key());
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

#[cfg(test)]
mod agent_context_tests {
    use super::*;

    #[test]
    fn context_explains_ranked_definition_and_direct_relation_sites() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("service.ts"),
            "export function getPendingLegalPersonOnboarding() { return true; }\n\
             export function find() { return false; }\n\
             export class First { execute() { return 1; } }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("consumer.ts"),
            "import { getPendingLegalPersonOnboarding } from './service';\n\
             export function loadPendingOnboarding() {\n\
               return getPendingLegalPersonOnboarding();\n\
             }\n\
             export class Second { execute() { return 2; } }\n",
        )
        .unwrap();

        let engine = WorkspaceEngine::load(root.path(), &Flags::default()).unwrap();
        engine.index().unwrap();

        let exact = engine
            .context("getPendingLegalPersonOnboarding", 10)
            .unwrap();
        assert_eq!(exact["ambiguous"], false);
        assert_eq!(exact["detail"]["path"], "service.ts");
        assert!(
            exact["source"]["text"]
                .as_str()
                .unwrap()
                .contains("getPendingLegalPersonOnboarding")
        );
        let incoming = exact["relations"]["incoming"].as_array().unwrap();
        assert!(incoming.iter().any(|relation| {
            relation["kind"] == "Calls"
                && relation["site"]["path"] == "consumer.ts"
                && relation["site"]["line"] == 3
        }));
        let impact = engine
            .impact_risk(
                "getPendingLegalPersonOnboarding",
                &QueryLimits {
                    depth: 2,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(impact.total_affected > 0, "{impact:?}");

        let terms = engine
            .context("pending legal person onboarding function", 10)
            .unwrap();
        assert_eq!(
            terms["matches"].as_array().unwrap()[0],
            "getPendingLegalPersonOnboarding"
        );
        let intent_word = engine
            .context("find pending legal person onboarding function", 10)
            .unwrap();
        assert_eq!(
            intent_word["detail"]["name"],
            "getPendingLegalPersonOnboarding"
        );

        let ambiguous = engine.context("execute", 10).unwrap();
        assert_eq!(ambiguous["ambiguous"], true);
        assert!(ambiguous["detail"].is_null());
        assert!(
            ambiguous["relations"]["incoming"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let qualified = engine.context("First.execute", 10).unwrap();
        assert_eq!(qualified["ambiguous"], false);
        assert_eq!(qualified["detail"]["qualified"], "First.execute");
        assert_eq!(qualified["candidates"][0]["why"], "exact-qualified");
    }

    #[test]
    fn incremental_rename_updates_path_terms_without_stale_search_hits() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        let old_path = root.path().join("src/pending_onboarding.ts");
        let new_path = root.path().join("src/archive.ts");
        std::fs::write(&old_path, "export const handler = () => true;\n").unwrap();
        let engine = WorkspaceEngine::load(root.path(), &Flags::default()).unwrap();
        engine.index().unwrap();
        assert_eq!(
            engine
                .search_raw("pending onboarding", SearchKind::Terms, 10)
                .unwrap()[0]
                .value,
            "handler"
        );

        std::fs::rename(&old_path, &new_path).unwrap();
        engine
            .sync(Some(&[old_path.clone(), new_path.clone()]))
            .unwrap();
        assert!(
            engine
                .search_raw("pending onboarding", SearchKind::Terms, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn single_concept_context_uses_path_evidence_after_prefix_stage() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("features/onboarding")).unwrap();
        std::fs::write(
            root.path().join("features/onboarding/handler.ts"),
            "export function processRequest() { return true; }\n",
        )
        .unwrap();
        let engine = WorkspaceEngine::load(root.path(), &Flags::default()).unwrap();
        engine.index().unwrap();

        let context = engine.context("onboarding", 10).unwrap();
        assert_eq!(context["ambiguous"], false);
        assert_eq!(context["detail"]["name"], "processRequest");
        assert_eq!(context["candidates"][0]["why"], "term-coverage");
    }

    #[test]
    fn term_mode_filters_homonyms_at_definition_level() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("apps/admin")).unwrap();
        std::fs::create_dir_all(root.path().join("apps/users/onboarding")).unwrap();
        std::fs::write(
            root.path().join("apps/admin/service.ts"),
            "export class AdminService { execute() {} }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("apps/users/onboarding/service.ts"),
            "export class OnboardingService { execute() {} }\n",
        )
        .unwrap();
        let engine = WorkspaceEngine::load(root.path(), &Flags::default()).unwrap();
        engine.index().unwrap();

        let context = engine
            .context("users onboarding execute method", 10)
            .unwrap();
        assert_eq!(context["ambiguous"], false);
        assert_eq!(context["detail"]["qualified"], "OnboardingService.execute");
        assert_eq!(context["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(
            context["candidates"][0]["path"],
            "apps/users/onboarding/service.ts"
        );
    }
}
