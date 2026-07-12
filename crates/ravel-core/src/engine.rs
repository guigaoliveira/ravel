use crate::{
    analysis::{self, CiReport, CycleInfo, HubEntry, ImpactReport, PackageInfo},
    config::{Config, Flags},
    graph::{GraphIndex, QueryLimits, QueryPage},
    model::{IndexSnapshot, SnapshotId},
    policy::{PolicyFinding, Suppressions, validate_snapshot},
    resolver::{load_tsconfig, resolve_edges},
    scanner::scan_workspace,
    search::{SearchHit, SearchIndex, SearchKind},
    storage::{FileSnapshotStorage, SnapshotStorage, StorageError},
};

pub use crate::model::IndexStats;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
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
}

#[derive(Debug)]
struct EngineInner {
    snapshot_cache: Mutex<Option<Arc<IndexSnapshot>>>,
    graph_cache: Mutex<Option<Arc<GraphIndex>>>,
    search_cache: Mutex<Option<Arc<SearchIndex>>>,
    /// Cached "is this a git repo?" — avoid probing every tool call.
    git_repo: Mutex<Option<bool>>,
    /// Debounce dirty discovery for concurrent tool calls in the same tick (MCP warm).
    /// Window is short (~50ms) — **not** a latency budget; commands target &lt;200ms wall.
    dirty_cache: Mutex<Option<(std::time::Instant, Vec<PathBuf>)>>,
}

struct PreparedPath {
    path: PathBuf,
    relative: String,
    bytes: Option<Vec<u8>>,
    unchanged: bool,
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
        Ok(Self {
            root: root.to_path_buf(),
            config,
            inner: Arc::new(EngineInner {
                snapshot_cache: Mutex::new(None),
                graph_cache: Mutex::new(None),
                search_cache: Mutex::new(None),
                git_repo: Mutex::new(None),
                dirty_cache: Mutex::new(None),
            }),
        })
    }

    fn is_git_repo_cached(&self) -> bool {
        let mut slot = self.inner.git_repo.lock().unwrap();
        if let Some(v) = *slot {
            return v;
        }
        let v = crate::git::is_git_repo(&self.root);
        *slot = Some(v);
        v
    }
    pub fn storage(&self) -> FileSnapshotStorage {
        FileSnapshotStorage::new(self.root.join(&self.config.storage.home))
    }
    pub fn index(&self) -> Result<IndexStats, EngineError> {
        let (artifacts, scan_stats) = scan_workspace(&self.config)?;
        let files: std::collections::BTreeMap<_, _> =
            artifacts.into_iter().map(|a| (a.path.clone(), a)).collect();
        self.publish_from_artifacts(files, scan_stats.bytes_read, scan_stats.parse_errors)
    }

    /// Incremental index update for daily edits (save/rename/delete).
    /// Re-parses only the given paths (or dirty discovery if `None`), re-resolves edges, republishes sidecars.
    pub fn sync(&self, only_paths: Option<&[PathBuf]>) -> Result<IndexStats, EngineError> {
        let max_bytes = self.config.parser.max_file_size_kb.saturating_mul(1024);
        let extensions = crate::config::effective_extensions(&self.config);
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

        let existing = match self.storage().open_current() {
            Ok(s) => s,
            Err(_) => {
                return self.index();
            }
        };
        let Some(mut snapshot) = existing else {
            return self.index();
        };
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
        let mut any_changed = false;
        for prepared in prepared {
            let rel = prepared.relative;
            let Some(bytes) = prepared.bytes else {
                if snapshot.files.remove(&rel).is_some() {
                    any_changed = true;
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
                    snapshot.files.insert(rel, artifact);
                    any_changed = true;
                }
                Err(_) => {
                    if snapshot.files.remove(&rel).is_some() {
                        any_changed = true;
                    }
                }
            }
        }
        if !any_changed {
            return Ok(stats_from(&snapshot));
        }
        *self.inner.dirty_cache.lock().unwrap() = None;
        let bytes: u64 = snapshot.files.values().map(|a| a.bytes_read).sum();
        let parse_errors = snapshot
            .files
            .values()
            .map(|a| usize::from(!a.diagnostics.is_empty()))
            .sum();
        self.publish_from_artifacts(snapshot.files, bytes, parse_errors)
    }

    /// Prepare changed-path bytes once and decide whether the entire sync is a no-op.
    fn prepare_paths(&self, paths: &[PathBuf]) -> Result<(bool, Vec<PreparedPath>), EngineError> {
        if paths.is_empty() {
            return Ok((true, Vec::new()));
        }
        let hashes = self.storage().open_file_hashes().ok().flatten();
        let root_str = self.root.to_string_lossy().replace('\\', "/");
        let root_str = root_str.replace("/./", "/");
        let root_str = root_str.trim_end_matches('/').to_owned();
        let mut all_unchanged = hashes.is_some();
        let mut prepared = Vec::with_capacity(paths.len());
        for path in paths {
            let path_str = path.to_string_lossy().replace('\\', "/");
            let rel = path_str
                .strip_prefix(&root_str)
                .unwrap_or(&path_str)
                .trim_start_matches('/')
                .to_owned();
            if !path.is_file() {
                let unchanged = hashes.as_ref().is_some_and(|h| !h.contains(&rel));
                all_unchanged &= unchanged;
                prepared.push(PreparedPath {
                    path: path.clone(),
                    relative: rel,
                    bytes: None,
                    unchanged,
                });
                continue;
            }
            let bytes = std::fs::read(path).ok();
            let unchanged = bytes.as_ref().is_some_and(|bytes| {
                hashes
                    .as_ref()
                    .and_then(|h| h.get(&rel))
                    .is_some_and(|old| blake3::hash(bytes).to_hex().as_str() == old)
            });
            all_unchanged &= unchanged;
            prepared.push(PreparedPath {
                path: path.clone(),
                relative: rel,
                bytes,
                unchanged,
            });
        }
        Ok((all_unchanged, prepared))
    }

    fn publish_from_artifacts(
        &self,
        files: std::collections::BTreeMap<String, crate::model::FileArtifact>,
        bytes_read: u64,
        parse_errors: usize,
    ) -> Result<IndexStats, EngineError> {
        let resolver = load_tsconfig(&self.root);
        let edges = resolve_edges(&self.root, &files, &resolver);
        // Works without git — identity falls back to path + "nogit".
        let identity = crate::git::worktree_identity_or_nogit(&self.root);
        let id = SnapshotId {
            root: identity.root.to_string_lossy().into_owned(),
            worktree: identity.worktree,
            revision: identity.revision,
            content_state: format!("{}:{}", files.len(), bytes_read),
            schema_version: 1,
            grammar_version: crate::scanner::GRAMMAR_VERSION.into(),
            config_hash: self.config.hash(),
        };
        let snapshot = IndexSnapshot { id, files, edges };
        self.storage().publish(&snapshot)?;
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
        Ok(stats)
    }

    /// Index health for agents. **Cheap:** no git status spawn (keeps status ≪ 200ms).
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
            "extensions": crate::config::effective_extensions(&self.config),
            "sidecars": {
                "graph": graph_present,
                "symbols": manifest.as_ref().is_some_and(|m| store.referenced_path_exists(m.symbols.as_deref())),
                "stats": stats_present,
                "hubs": manifest.as_ref().is_some_and(|m| store.referenced_path_exists(m.hubs.as_deref())),
                "file_hashes": manifest.as_ref().is_some_and(|m| store.referenced_path_exists(m.file_hashes.as_deref())),
            },
            "perf_sla_ms": 200,
            "hint": if !has {
                "Run `ravel index` first."
            } else if !self.config.sync.auto {
                "Index ready. Auto-sync off. Use `ravel sync <paths>` or `watch`."
            } else if !git {
                "Index ready (no git). Freshness: `ravel watch` or `ravel sync <paths>`."
            } else {
                "Index ready. Auto-sync: tracked dirty + hash sidecar (SLA <200ms hot path)."
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
            .storage()
            .open_symbol_meta()
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
            "sid": self.stats().map(|s| s.snapshot_id).ok(),
        }))
    }

    /// Token-efficient refactor briefing: files to touch + risk counts + top edges.
    /// One call replaces search + impact + multi-file discovery for renames/blast-radius work.
    pub fn refactor_plan(
        &self,
        symbol: &str,
        limit: usize,
    ) -> Result<serde_json::Value, EngineError> {
        let _ = self.auto_sync_if_dirty()?;
        let limit = limit.clamp(1, 100);
        let hits = self.search_raw(symbol, SearchKind::Prefix, limit.min(20))?;
        let primary = hits
            .first()
            .map(|h| h.value.clone())
            .filter(|v| v.eq_ignore_ascii_case(symbol) || v.contains(symbol))
            .or_else(|| hits.first().map(|h| h.value.clone()))
            .unwrap_or_else(|| symbol.to_owned());

        let limits = QueryLimits {
            depth: 3,
            nodes: 200,
            edges: 500,
            page_size: 100,
            ..Default::default()
        };
        let impact = self.impact_risk(&primary, &limits)?;
        let callers = self.query_raw(&primary, true, &limits, None)?;

        // Precompute the `.ext` suffixes once — the closure ran per affected item / caller and
        // rebuilt `effective_extensions()` (a Vec alloc) plus a `format!` per extension each time.
        let ext_suffixes: Vec<String> = crate::config::effective_extensions(&self.config)
            .iter()
            .map(|e| format!(".{e}"))
            .collect();
        let mut files: BTreeSet<String> = BTreeSet::new();
        let push_file = |s: &str, files: &mut BTreeSet<String>| {
            let s = s.replace('\\', "/");
            if s.contains('/') && ext_suffixes.iter().any(|suf| s.ends_with(suf)) {
                files.insert(s);
            }
        };
        for item in &impact.affected {
            push_file(&item.symbol, &mut files);
        }
        for item in &callers.items {
            push_file(item, &mut files);
        }
        if let Some(meta) = self.storage().open_symbol_meta().ok().flatten() {
            for symbol in meta.entries_for(&primary) {
                let path = symbol.path.replace('\\', "/");
                files.insert(path.clone());
                // Refactor impact includes file importers even when the parser cannot prove a
                // symbol-level call (e.g. imported DTOs, interfaces, and type-only contracts).
                if let Ok(importers) = self.query_raw(&path, true, &limits, None) {
                    for importer in importers.items {
                        push_file(&importer, &mut files);
                    }
                }
            }
        }

        let mut high = 0u32;
        let mut med = 0u32;
        let mut low = 0u32;
        for i in &impact.affected {
            match i.risk {
                analysis::RiskLevel::High => high += 1,
                analysis::RiskLevel::Medium => med += 1,
                analysis::RiskLevel::Low => low += 1,
            }
        }

        let files_vec: Vec<_> = files.into_iter().take(limit).collect();
        let tests: Vec<String> = files_vec
            .first()
            .and_then(|f| self.related_tests(f).ok())
            .unwrap_or_default();

        // Compact keys (same spirit as `context`) — agents parse short JSON.
        Ok(serde_json::json!({
            "s": primary,
            "alias": hits.iter().map(|h| &h.value).take(8).collect::<Vec<_>>(),
            "risk": { "h": high, "m": med, "l": low },
            "files": files_vec,
            "tests": tests.into_iter().take(12).collect::<Vec<_>>(),
            "callers": callers.items.iter().take(12).cloned().collect::<Vec<_>>(),
            "hit": impact.affected.iter().take(12).map(|i| {
                serde_json::json!({"s": i.symbol, "r": i.risk, "d": i.depth})
            }).collect::<Vec<_>>(),
            "n_aff": impact.total_affected,
            "sid": impact.snapshot_id,
        }))
    }
    pub fn snapshot(&self) -> Result<Arc<IndexSnapshot>, EngineError> {
        if let Some(snapshot) = self.inner.snapshot_cache.lock().unwrap().as_ref() {
            return Ok(Arc::clone(snapshot));
        }
        let snapshot = self.storage().open_current()?.ok_or_else(|| {
            EngineError::Storage(StorageError::Invalid {
                path: self.storage_path(),
                message: "no current snapshot; run `ravel index` first".into(),
            })
        })?;
        let arc = Arc::new(snapshot);
        *self.inner.snapshot_cache.lock().unwrap() = Some(Arc::clone(&arc));
        Ok(arc)
    }
    pub fn search_index(&self) -> Result<Arc<SearchIndex>, EngineError> {
        self.search_index_for(SearchKind::Exact)
    }

    /// Materialize search backend for `kind` without loading the full snapshot when sidecars exist.
    /// Exact/prefix open **dict only** (cheapest). Fuzzy/regex open Hybrid (dict + on-disk Tantivy).
    fn search_index_for(&self, kind: SearchKind) -> Result<Arc<SearchIndex>, EngineError> {
        if let Some(index) = self.inner.search_cache.lock().unwrap().as_ref() {
            // Upgrade path: cached dict-only but fuzzy/regex needs Tantivy.
            let needs_tantivy = matches!(kind, SearchKind::Fuzzy | SearchKind::Regex);
            if !needs_tantivy || index.backend_label() != "dict" {
                return Ok(Arc::clone(index));
            }
        }
        let storage = self.storage();
        let needs_tantivy = matches!(kind, SearchKind::Fuzzy | SearchKind::Regex);
        let index = if let Some(dict) = storage.open_symbols()? {
            let built = if needs_tantivy {
                if let Some(dir) = storage.open_search_dir()? {
                    SearchIndex::with_dict_and_tantivy_dir(dict, &dir)
                        .map_err(|e| EngineError::Search(e.to_string()))?
                } else {
                    SearchIndex::from_symbol_dict(dict)
                }
            } else {
                SearchIndex::from_symbol_dict(dict)
            };
            Arc::new(built)
        } else if needs_tantivy {
            if let Some(dir) = storage.open_search_dir()? {
                Arc::new(
                    SearchIndex::open_tantivy_dir(&dir)
                        .map_err(|e| EngineError::Search(e.to_string()))?,
                )
            } else {
                let snapshot = self.snapshot()?;
                Arc::new(
                    SearchIndex::from_snapshot(&snapshot)
                        .map_err(|e| EngineError::Search(e.to_string()))?,
                )
            }
        } else {
            let snapshot = self.snapshot()?;
            Arc::new(
                SearchIndex::from_snapshot(&snapshot)
                    .map_err(|e| EngineError::Search(e.to_string()))?,
            )
        };
        *self.inner.search_cache.lock().unwrap() = Some(Arc::clone(&index));
        Ok(index)
    }
    pub fn graph(&self) -> Result<Arc<GraphIndex>, EngineError> {
        if let Some(graph) = self.inner.graph_cache.lock().unwrap().as_ref() {
            return Ok(Arc::clone(graph));
        }
        // Prefer prebuilt compact graph (cold CLI path); fall back to full snapshot rebuild.
        let graph = if let Some(graph) = self.storage().open_graph()? {
            Arc::new(graph)
        } else {
            let snapshot = self.snapshot()?;
            Arc::new(GraphIndex::from_snapshot(&snapshot))
        };
        *self.inner.graph_cache.lock().unwrap() = Some(Arc::clone(&graph));
        Ok(graph)
    }
    pub fn clear_cache(&self) {
        *self.inner.snapshot_cache.lock().unwrap() = None;
        *self.inner.graph_cache.lock().unwrap() = None;
        *self.inner.search_cache.lock().unwrap() = None;
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
        if let Some(meta) = self.storage().open_symbol_meta()? {
            return Ok(meta.get(symbol).map(|m| crate::model::Symbol {
                name: m.name.clone(),
                kind: m.kind.clone(),
                span: m.span,
                exported: m.exported,
                complexity: m.complexity.clone(),
            }));
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
        let meta = self.storage().open_symbol_meta()?;
        let mut enriched = analysis::enrich_hubs(raw, meta.as_ref(), kind_filter);
        enriched.truncate(limit.max(1));
        Ok(enriched)
    }

    pub fn orphans(&self, limit: usize) -> Result<Vec<String>, EngineError> {
        let graph = self.graph()?;
        let meta = self.storage().open_symbol_meta()?;
        let manifest_entries = crate::entries::collect_manifest_entry_paths(&self.root);
        Ok(analysis::orphans(
            &graph,
            meta.as_ref(),
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
                if at.elapsed() < std::time::Duration::from_millis(50) {
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
    /// Budget: must not push hot-path commands over **200ms**. Never hydrates full snapshot
    /// unless content actually changed **and** `file_hashes` sidecar exists.
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
