//! MCP stdio surface — thin wrapper over `WorkspaceEngine` (same results as CLI).
//!
//! Tool schemas cost tokens every session, so the default surface stays small.
//! Default = **primary** tool set only (`explore`, `status`, `sync`).
//! Set `RAVEL_MCP_TOOLS=all` for the full surface.

use crate::{analysis, engine::WorkspaceEngine, graph::QueryLimits, search::SearchKind};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    ops::Deref,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

const DEFAULT_MCP_MAX_CACHED_ROOTS: usize = 8;

fn max_cached_roots_from_env() -> usize {
    parse_max_cached_roots(std::env::var("RAVEL_MCP_MAX_CACHED_ROOTS").ok().as_deref())
}

fn parse_max_cached_roots(value: Option<&str>) -> usize {
    value
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MCP_MAX_CACHED_ROOTS)
}

/// Which MCP tools to advertise (schema cost ∝ tool count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpToolMode {
    /// Default: 3 high-value tools (minimal schema overhead).
    Primary,
    /// Every tool (search, impact, cycles, hubs, orphans, …) — larger schema.
    All,
}

impl McpToolMode {
    pub fn from_env() -> Self {
        match std::env::var("RAVEL_MCP_TOOLS")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "all" | "full" | "extended" => Self::All,
            _ => Self::Primary,
        }
    }
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RootRequest {
    pub root: Option<String>,
}
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SyncRequest {
    pub root: Option<String>,
    /// Explicit edited paths. Relative paths are resolved from the workspace root.
    pub paths: Option<Vec<String>>,
}
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct QueryRequest {
    pub root: Option<String>,
    pub node: String,
    pub depth: Option<usize>,
    pub nodes: Option<usize>,
}
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SearchRequest {
    pub root: Option<String>,
    pub query: String,
    pub kind: Option<String>,
    pub limit: Option<usize>,
}
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ExploreRequest {
    pub root: Option<String>,
    pub query: String,
    pub limit: Option<usize>,
}
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SymbolDetailRequest {
    pub root: Option<String>,
    pub symbol: String,
}
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct PackageRequest {
    pub root: Option<String>,
    pub name: String,
    pub limit: Option<usize>,
}
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct LimitRequest {
    pub root: Option<String>,
    pub limit: Option<usize>,
    pub package: Option<String>,
    /// Optional kind/path filter for hubs/hot_paths
    pub kind: Option<String>,
}
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct DiffImpactRequest {
    pub root: Option<String>,
    pub from: String,
    pub to: Option<String>,
    pub depth: Option<usize>,
}
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct CiRequest {
    pub root: Option<String>,
    pub strict: Option<bool>,
    pub cycle_threshold: Option<usize>,
}
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct CoChangeRequest {
    pub root: Option<String>,
    pub file: String,
    pub commits: Option<usize>,
    pub min_cooccurrence: Option<u32>,
}

#[derive(Debug)]
pub struct RavelMcp {
    tool_router: ToolRouter<Self>,
    engines: Arc<Mutex<HashMap<String, EngineBinding>>>,
    daemons: Arc<Mutex<HashMap<String, DaemonBinding>>>,
    cache_clock: AtomicU64,
    max_cached_roots: usize,
    mode: McpToolMode,
    default_root: Option<PathBuf>,
}

#[derive(Debug)]
struct DaemonBinding {
    client: crate::daemon::DaemonClient,
    _lease: crate::daemon::DaemonClientLease,
    active: Arc<AtomicUsize>,
    last_used: u64,
}

#[derive(Debug)]
struct EngineBinding {
    engine: Arc<WorkspaceEngine>,
    stop_watcher: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    last_used: u64,
}

impl Drop for EngineBinding {
    fn drop(&mut self) {
        self.stop_watcher.store(true, Ordering::Release);
    }
}

struct EngineUse {
    engine: Arc<WorkspaceEngine>,
    active: Arc<AtomicUsize>,
    cache: Arc<Mutex<HashMap<String, EngineBinding>>>,
    max_cached_roots: usize,
}

impl Deref for EngineUse {
    type Target = WorkspaceEngine;
    fn deref(&self) -> &Self::Target {
        &self.engine
    }
}

impl Drop for EngineUse {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
        evict_inactive_engine(&mut self.cache.lock().unwrap(), self.max_cached_roots);
    }
}

struct DaemonUse {
    client: crate::daemon::DaemonClient,
    active: Arc<AtomicUsize>,
    cache: Arc<Mutex<HashMap<String, DaemonBinding>>>,
    max_cached_roots: usize,
}

impl Drop for DaemonUse {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
        evict_inactive_daemon(&mut self.cache.lock().unwrap(), self.max_cached_roots);
    }
}

impl Default for RavelMcp {
    fn default() -> Self {
        Self::new()
    }
}

impl RavelMcp {
    pub fn new() -> Self {
        Self::with_mode(McpToolMode::from_env())
    }

    pub fn with_mode(mode: McpToolMode) -> Self {
        Self::with_mode_and_root(mode, None)
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self::with_mode_and_root(McpToolMode::from_env(), Some(root))
    }

    fn with_mode_and_root(mode: McpToolMode, default_root: Option<PathBuf>) -> Self {
        let tool_router = match mode {
            McpToolMode::Primary => Self::tool_router_primary(),
            McpToolMode::All => Self::tool_router_primary() + Self::tool_router_extended(),
        };
        Self {
            tool_router,
            engines: Arc::new(Mutex::new(HashMap::new())),
            daemons: Arc::new(Mutex::new(HashMap::new())),
            cache_clock: AtomicU64::new(0),
            max_cached_roots: max_cached_roots_from_env(),
            mode,
            default_root,
        }
    }

    fn next_cache_tick(&self) -> u64 {
        self.cache_clock.fetch_add(1, Ordering::Relaxed)
    }

    fn engine(&self, root: Option<String>) -> anyhow::Result<EngineUse> {
        let base = root
            .map(PathBuf::from)
            .or_else(|| self.default_root.clone())
            .unwrap_or(std::env::current_dir()?);
        // Keep the original path when canonicalize fails — collapsing every failure to "."
        // would alias distinct roots onto one cache entry.
        let root = base.canonicalize().unwrap_or(base);
        let key = root.to_string_lossy().into_owned();
        let mut engines = self.engines.lock().unwrap();
        let tick = self.next_cache_tick();
        if let Some(binding) = engines.get_mut(&key) {
            binding.last_used = tick;
            binding.active.fetch_add(1, Ordering::AcqRel);
            return Ok(EngineUse {
                engine: binding.engine.clone(),
                active: binding.active.clone(),
                cache: self.engines.clone(),
                max_cached_roots: self.max_cached_roots,
            });
        }
        evict_inactive_engine(&mut engines, self.max_cached_roots.saturating_sub(1));
        let engine = Arc::new(WorkspaceEngine::load(&root, &Default::default())?);
        let active = Arc::new(AtomicUsize::new(1));
        let stop_watcher = Arc::new(AtomicBool::new(false));
        spawn_root_watcher(root, engine.clone(), stop_watcher.clone());
        engines.insert(
            key,
            EngineBinding {
                engine: engine.clone(),
                stop_watcher,
                active: active.clone(),
                last_used: tick,
            },
        );
        Ok(EngineUse {
            engine,
            active,
            cache: self.engines.clone(),
            max_cached_roots: self.max_cached_roots,
        })
    }

    fn daemon_client(&self, root: Option<&str>) -> Option<DaemonUse> {
        let base = root
            .map(PathBuf::from)
            .or_else(|| self.default_root.clone())
            .or_else(|| std::env::current_dir().ok())?;
        let root = base.canonicalize().unwrap_or(base);
        let key = root.to_string_lossy().into_owned();
        let mut daemons = self.daemons.lock().unwrap();
        let tick = self.next_cache_tick();
        if let Some(binding) = daemons.get_mut(&key) {
            binding.last_used = tick;
            binding.active.fetch_add(1, Ordering::AcqRel);
            return Some(DaemonUse {
                client: binding.client.clone(),
                active: binding.active.clone(),
                cache: self.daemons.clone(),
                max_cached_roots: self.max_cached_roots,
            });
        }
        evict_inactive_daemon(&mut daemons, self.max_cached_roots.saturating_sub(1));
        let (client, lease) = crate::daemon::ensure_transient(&root).ok()?;
        let active = Arc::new(AtomicUsize::new(1));
        daemons.insert(
            key,
            DaemonBinding {
                client: client.clone(),
                _lease: lease,
                active: active.clone(),
                last_used: tick,
            },
        );
        Some(DaemonUse {
            client,
            active,
            cache: self.daemons.clone(),
            max_cached_roots: self.max_cached_roots,
        })
    }

    fn forget_daemon(&self, root: Option<&str>) {
        let Some(base) = root
            .map(PathBuf::from)
            .or_else(|| self.default_root.clone())
            .or_else(|| std::env::current_dir().ok())
        else {
            return;
        };
        let root = base.canonicalize().unwrap_or(base);
        self.daemons
            .lock()
            .unwrap()
            .remove(root.to_string_lossy().as_ref());
    }

    fn call_daemon(
        &self,
        root: Option<&str>,
        operation: crate::daemon::DaemonOperation,
    ) -> Result<serde_json::Value, String> {
        let client = self
            .daemon_client(root)
            .ok_or_else(|| "shared daemon could not be started".to_owned())?;
        match client.client.call(operation.clone()) {
            Ok(value) => Ok(value),
            Err(crate::daemon::DaemonCallError::Remote(error)) => Err(error),
            Err(crate::daemon::DaemonCallError::Transport(_)) => {
                self.forget_daemon(root);
                let retry = self
                    .daemon_client(root)
                    .ok_or_else(|| "shared daemon could not be restarted".to_owned())?;
                retry
                    .client
                    .call(operation)
                    .map_err(|error| error.to_string())
            }
        }
    }
}

fn evict_inactive_daemon(cache: &mut HashMap<String, DaemonBinding>, target_len: usize) {
    while cache.len() > target_len {
        let candidate = cache
            .iter()
            .filter(|(_, value)| value.active.load(Ordering::Acquire) == 0)
            .min_by_key(|(_, value)| value.last_used)
            .map(|(key, _)| key.clone());
        let Some(key) = candidate else { break };
        cache.remove(&key);
    }
}

fn evict_inactive_engine(cache: &mut HashMap<String, EngineBinding>, target_len: usize) {
    while cache.len() > target_len {
        let candidate = cache
            .iter()
            .filter(|(_, value)| value.active.load(Ordering::Acquire) == 0)
            .min_by_key(|(_, value)| value.last_used)
            .map(|(key, _)| key.clone());
        let Some(key) = candidate else { break };
        cache.remove(&key);
    }
}

fn spawn_root_watcher(root: PathBuf, engine: Arc<WorkspaceEngine>, stop: Arc<AtomicBool>) {
    if !root.is_dir() || engine.config.sync.mode == "none" {
        return;
    }
    let debounce = Duration::from_millis(engine.config.watch.debounce_ms);
    let max_batch = Duration::from_millis(engine.config.watch.max_batch_ms);
    let max_batch_paths = engine.config.watch.max_batch_paths;
    let queue_capacity = engine.config.watch.queue_capacity;
    let watch_config = engine.config.clone();
    let storage_root = root.join(&engine.config.storage.home);
    let _ = thread::Builder::new()
        .name("ravel-mcp-watch".into())
        .spawn(move || {
            // MCP clients normally launch one stdio server each. Keep exactly one filesystem
            // watcher per workspace across those processes; the blocking followers take over
            // automatically when the leader exits and the OS releases its file lock.
            let _watch_leader = match acquire_watcher_leadership(&root, &engine, &stop) {
                Some(lock) => lock,
                None => return,
            };
            let watcher = match crate::watch::PersistentWatcher::new_filtered(
                &root,
                queue_capacity,
                move |path| !path.starts_with(&storage_root) && !watch_config.is_noise(path),
            ) {
                Ok(watcher) => watcher,
                Err(error) => {
                    engine.record_update_error("watch", &error.to_string());
                    return;
                }
            };
            while !stop.load(Ordering::Acquire) {
                let batch = match watcher.next_batch(
                    debounce,
                    Duration::from_secs(1),
                    max_batch_paths,
                    max_batch,
                ) {
                    Ok(batch) => batch,
                    Err(crate::watch::WatchError::Timeout) => continue,
                    Err(crate::watch::WatchError::Closed) => {
                        engine.record_update_error("watch", "watch channel closed");
                        return;
                    }
                    Err(error) => {
                        engine.record_update_error("watch", &error.to_string());
                        return;
                    }
                };
                let extensions = crate::config::effective_extensions(&engine.config);
                let paths: Vec<_> = batch
                    .paths
                    .into_iter()
                    .filter(|path| {
                        engine.config.is_source_with_extensions(path, &extensions)
                            && !engine.config.is_noise(path)
                    })
                    .collect();
                if batch.needs_reconcile {
                    if let Err(error) = engine.reconcile() {
                        engine.record_update_error("watch index", &error.to_string());
                    }
                } else if !paths.is_empty() {
                    if let Err(error) = engine.sync(Some(&paths)) {
                        engine.record_update_error("watch sync", &error.to_string());
                    }
                }
            }
        });
}

fn acquire_watcher_leadership(
    root: &std::path::Path,
    engine: &WorkspaceEngine,
    stop: &AtomicBool,
) -> Option<std::fs::File> {
    use fs4::fs_std::FileExt;
    use std::fs::OpenOptions;

    let storage = root.join(&engine.config.storage.home);
    if let Err(error) = std::fs::create_dir_all(&storage) {
        engine.record_update_error("watch leader", &error.to_string());
        return None;
    }
    let file = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(storage.join("watch.lock"))
    {
        Ok(file) => file,
        Err(error) => {
            engine.record_update_error("watch leader", &error.to_string());
            return None;
        }
    };
    while !stop.load(Ordering::Acquire) {
        match file.try_lock_exclusive() {
            Ok(true) => return Some(file),
            Ok(false) => thread::sleep(Duration::from_millis(100)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                engine.record_update_error("watch leader", &error.to_string());
                return None;
            }
        }
    }
    None
}

// ── Primary tools (default) — fewer tools = less schema overhead ────────────

#[tool_router(router = tool_router_primary, vis = "pub")]
impl RavelMcp {
    #[tool(
        description = "PRIMARY (one call → answers). Exact/qualified symbol or natural-term search, selected source, typed caller/callee sites, and bounded impact. Ambiguous names return candidates instead of guessing."
    )]
    async fn explore(&self, Parameters(request): Parameters<ExploreRequest>) -> String {
        let limit = request.limit.unwrap_or(10).max(1);
        match self.call_daemon(
            request.root.as_deref(),
            crate::daemon::DaemonOperation::Context {
                query: request.query.clone(),
                limit,
            },
        ) {
            Ok(value) => value.to_string(),
            Err(error) => error_json(error),
        }
    }

    #[tool(
        description = "PRIMARY: Index status (indexed?, files/edges/snapshot_id). Session start."
    )]
    async fn status(&self, Parameters(request): Parameters<RootRequest>) -> String {
        match self.call_daemon(
            request.root.as_deref(),
            crate::daemon::DaemonOperation::Status,
        ) {
            Ok(value) => value.to_string(),
            Err(error) => error_json(error),
        }
    }

    #[tool(
        description = "PRIMARY: Incremental reindex. Pass edited paths for immediate, reliable sync; otherwise discovers Git-dirty files."
    )]
    async fn sync(&self, Parameters(request): Parameters<SyncRequest>) -> String {
        let paths = request
            .paths
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        match self.call_daemon(
            request.root.as_deref(),
            crate::daemon::DaemonOperation::Sync { paths },
        ) {
            Ok(value) => value.to_string(),
            Err(error) => error_json(error),
        }
    }
}

// ── Extended tools (RAVEL_MCP_TOOLS=all) ────────────────────────────────────

#[tool_router(router = tool_router_extended, vis = "pub")]
impl RavelMcp {
    #[tool(description = "Search symbols (kind: exact|prefix|fuzzy|regex|terms)")]
    async fn search_symbols(&self, Parameters(request): Parameters<SearchRequest>) -> String {
        let kind = match request.kind.as_deref() {
            Some("prefix") => SearchKind::Prefix,
            Some("fuzzy") => SearchKind::Fuzzy,
            Some("regex") => SearchKind::Regex,
            Some("terms") => SearchKind::Terms,
            _ => SearchKind::Exact,
        };
        let limit = request.limit.unwrap_or(20).max(1);
        match self.engine(request.root) {
            Ok(engine) => match engine.search(&request.query, kind, limit) {
                Ok(hits) => serde_json::to_string(&hits).unwrap_or_else(|_| "[]".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Blast radius + risk scores for a symbol")]
    async fn impact_analysis(&self, Parameters(request): Parameters<QueryRequest>) -> String {
        let mut limits = QueryLimits::default();
        if let Some(depth) = request.depth {
            limits.depth = depth;
        }
        if let Some(nodes) = request.nodes {
            limits.nodes = nodes;
        }
        match self.engine(request.root) {
            Ok(engine) => match engine.impact_risk(&request.node, &limits) {
                Ok(page) => serde_json::to_string(&page).unwrap_or_else(|_| "{}".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Who depends on this symbol (reverse edges)")]
    async fn callers_of(&self, Parameters(request): Parameters<QueryRequest>) -> String {
        query_tool(self, request, true).await
    }

    #[tool(description = "Graph stats (files/edges/snapshot_id)")]
    async fn graph_stats(&self, Parameters(request): Parameters<RootRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.stats() {
                Ok(stats) => serde_json::to_string(&stats).unwrap_or_else(|_| "{}".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "List packages with language and path metadata")]
    async fn packages(&self, Parameters(request): Parameters<RootRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.storage().open_file_list() {
                Ok(Some(files)) => {
                    let packages =
                        analysis::list_packages_from_paths(files.paths.iter().map(String::as_str));
                    serde_json::to_string(&packages).unwrap_or_else(|_| "[]".into())
                }
                Ok(None) => match engine.list_packages() {
                    Ok(packages) => {
                        serde_json::to_string(&packages).unwrap_or_else(|_| "[]".into())
                    }
                    Err(error) => error_json(error.to_string()),
                },
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Forward traversal: what a symbol calls/imports")]
    async fn calls_from(&self, Parameters(request): Parameters<QueryRequest>) -> String {
        query_tool(self, request, false).await
    }

    #[tool(description = "Get detailed information about a symbol")]
    async fn node_detail(&self, Parameters(request): Parameters<SymbolDetailRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.node_detail(&request.symbol) {
                Ok(Some(sym)) => serde_json::to_string(&sym).unwrap_or_else(|_| "{}".into()),
                Ok(None) => error_json(format!("symbol '{}' not found", request.symbol)),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "List files belonging to a package (by path prefix)")]
    async fn files_in_package(&self, Parameters(request): Parameters<PackageRequest>) -> String {
        let limit = request.limit.unwrap_or(50).max(1);
        match self.engine(request.root) {
            Ok(engine) => match engine.storage().open_file_list() {
                Ok(Some(files)) => {
                    serde_json::to_string(&files.in_package_limit(&request.name, limit))
                        .unwrap_or_else(|_| "[]".into())
                }
                Ok(None) => match engine.files_in_package(&request.name) {
                    Ok(files) => {
                        serde_json::to_string(&files.into_iter().take(limit).collect::<Vec<_>>())
                            .unwrap_or_else(|_| "[]".into())
                    }
                    Err(error) => error_json(error.to_string()),
                },
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Package import cycles (SCC), largest first")]
    async fn cycles(&self, Parameters(request): Parameters<LimitRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.cycles(request.package.as_deref()) {
                Ok(c) => serde_json::to_string(&c).unwrap_or_else(|_| "[]".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Most depended-upon symbols; optional kind filter")]
    async fn hubs(&self, Parameters(request): Parameters<LimitRequest>) -> String {
        let limit = request.limit.unwrap_or(20).max(1);
        match self.engine(request.root) {
            Ok(engine) => match engine.hubs(limit, request.kind.as_deref()) {
                Ok(h) => serde_json::to_string(&h).unwrap_or_else(|_| "[]".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Symbols/files with no reverse dependencies")]
    async fn orphans(&self, Parameters(request): Parameters<LimitRequest>) -> String {
        let limit = request.limit.unwrap_or(100).max(1);
        match self.engine(request.root) {
            Ok(engine) => match engine.orphans(limit) {
                Ok(o) => serde_json::to_string(&o).unwrap_or_else(|_| "[]".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Impact of files changed between git refs")]
    async fn diff_impact(&self, Parameters(request): Parameters<DiffImpactRequest>) -> String {
        let limits = QueryLimits {
            depth: request.depth.unwrap_or(16),
            ..Default::default()
        };
        match self.engine(request.root) {
            Ok(engine) => match engine.diff_impact(&request.from, request.to.as_deref(), &limits) {
                Ok(r) => serde_json::to_string(&r).unwrap_or_else(|_| "{}".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "CI quality gate: cycles + policy findings")]
    async fn ci_check(&self, Parameters(request): Parameters<CiRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.ci(
                request.strict.unwrap_or(false),
                request.cycle_threshold.unwrap_or(2),
            ) {
                Ok(r) => serde_json::to_string(&r).unwrap_or_else(|_| "{}".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Export package dependency graph as GraphViz DOT")]
    async fn export_dot(&self, Parameters(request): Parameters<RootRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.export_dot() {
                Ok(dot) => serde_json::json!({"format":"dot","content":dot}).to_string(),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Validate index integrity (dangling edges, cross-package)")]
    async fn validate_index(&self, Parameters(request): Parameters<RootRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.validate() {
                Ok(f) => serde_json::to_string(&f).unwrap_or_else(|_| "[]".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Files that co-change with a path in recent git history")]
    async fn cochanged(&self, Parameters(request): Parameters<CoChangeRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.cochanged(
                &request.file,
                request.commits.unwrap_or(100),
                request.min_cooccurrence.unwrap_or(2),
            ) {
                Ok(e) => serde_json::to_string(&e).unwrap_or_else(|_| "[]".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Architecture boundary violations (ravel.boundaries.toml)")]
    async fn boundaries(&self, Parameters(request): Parameters<RootRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.boundaries() {
                Ok(f) => serde_json::to_string(&f).unwrap_or_else(|_| "[]".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Schema summary: counts by node/edge kind")]
    async fn describe_schema(&self, Parameters(request): Parameters<RootRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.describe_schema() {
                Ok(v) => v.to_string(),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Related test files for a source path using common naming patterns")]
    async fn related_tests(&self, Parameters(request): Parameters<SymbolDetailRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.related_tests(&request.symbol) {
                Ok(p) => serde_json::to_string(&p).unwrap_or_else(|_| "[]".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }
}

async fn query_tool(mcp: &RavelMcp, request: QueryRequest, reverse: bool) -> String {
    let mut limits = QueryLimits::default();
    if let Some(depth) = request.depth {
        limits.depth = depth;
    }
    if let Some(nodes) = request.nodes {
        limits.nodes = nodes;
    }
    match mcp.engine(request.root) {
        Ok(engine) => match engine.query(&request.node, reverse, &limits, None) {
            Ok(page) => serde_json::to_string(&page).unwrap_or_else(|_| "{}".into()),
            Err(error) => error_json(error.to_string()),
        },
        Err(error) => error_json(error.to_string()),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RavelMcp {
    fn get_info(&self) -> ServerInfo {
        let mode = match self.mode {
            McpToolMode::Primary => {
                "primary (3 tools: explore, status, sync; set RAVEL_MCP_TOOLS=all for full)"
            }
            McpToolMode::All => "all tools",
        };
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            format!(
                "Ravel = token-efficient code graph (TS/JS). Mode: {mode}. \
                 Prefer: explore | status | sync. \
                 Explore accepts a symbol/qualified name or natural terms and returns bounded evidence. \
                 Do NOT read whole files to find callers — use tools. \
                 Editing: use agent editor; ravel maps blast radius. \
                 Each requested root is watched unless sync.mode=none; use sync for explicit paths. \
                 CLI: `ravel explore X` / `ravel impact X --risk`."
            ),
        )
    }
}

pub async fn serve_stdio(default_root: Option<PathBuf>) -> anyhow::Result<()> {
    let server = match default_root {
        Some(root) => RavelMcp::with_root(root),
        None => RavelMcp::new(),
    };
    // Establish the default-root lease while stdio is alive. Tool calls remain lazy for any
    // additional roots, but the primary workspace daemon is ready before the MCP client asks.
    drop(server.daemon_client(None));
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

fn error_json(message: String) -> String {
    format!(
        "{{\"error\":{}}}",
        serde_json::to_string(&message).unwrap_or_else(|_| "\"error\"".into())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_mode_defaults_to_primary() {
        // Cannot clear env safely in parallel tests; just exercise parse paths.
        assert_eq!(McpToolMode::from_env(), McpToolMode::from_env());
    }

    #[test]
    fn primary_router_builds() {
        let m = RavelMcp::with_mode(McpToolMode::Primary);
        assert_eq!(m.mode, McpToolMode::Primary);
    }

    #[test]
    fn all_router_builds() {
        let m = RavelMcp::with_mode(McpToolMode::All);
        assert_eq!(m.mode, McpToolMode::All);
    }

    #[test]
    fn cli_root_is_the_default_for_mcp_requests() {
        let root = PathBuf::from("/tmp/ravel-mcp-root");
        let m = RavelMcp::with_root(root.clone());
        assert_eq!(m.default_root, Some(root));
    }

    #[test]
    fn cached_root_limit_is_configurable_and_rejects_zero() {
        assert_eq!(parse_max_cached_roots(Some("3")), 3);
        assert_eq!(
            parse_max_cached_roots(Some("0")),
            DEFAULT_MCP_MAX_CACHED_ROOTS
        );
        assert_eq!(
            parse_max_cached_roots(Some("invalid")),
            DEFAULT_MCP_MAX_CACHED_ROOTS
        );
    }

    #[test]
    fn engine_cache_evicts_oldest_inactive_binding_and_stops_its_watcher() {
        fn binding(root: &std::path::Path, last_used: u64, active: usize) -> EngineBinding {
            EngineBinding {
                engine: Arc::new(WorkspaceEngine::load(root, &Default::default()).unwrap()),
                stop_watcher: Arc::new(AtomicBool::new(false)),
                active: Arc::new(AtomicUsize::new(active)),
                last_used,
            }
        }

        let first_root = tempfile::tempdir().unwrap();
        let busy_root = tempfile::tempdir().unwrap();
        let newest_root = tempfile::tempdir().unwrap();
        let first = binding(first_root.path(), 1, 0);
        let first_stop = first.stop_watcher.clone();
        let mut cache = HashMap::from([
            ("first".to_owned(), first),
            ("busy".to_owned(), binding(busy_root.path(), 0, 1)),
            ("newest".to_owned(), binding(newest_root.path(), 2, 0)),
        ]);

        evict_inactive_engine(&mut cache, 2);

        assert_eq!(cache.len(), 2);
        assert!(!cache.contains_key("first"));
        assert!(
            cache.contains_key("busy"),
            "an active binding must not be evicted"
        );
        assert!(
            first_stop.load(Ordering::Acquire),
            "eviction must stop the root watcher"
        );
    }

    #[test]
    fn watcher_leadership_is_exclusive_and_fails_over() {
        let root = tempfile::tempdir().unwrap();
        let leader =
            crate::watch::acquire_leadership(root.path(), std::path::Path::new(".ravel")).unwrap();
        let follower_root = root.path().to_path_buf();
        let (sender, receiver) = std::sync::mpsc::channel();
        let follower = std::thread::spawn(move || {
            let lock =
                crate::watch::acquire_leadership(&follower_root, std::path::Path::new(".ravel"))
                    .unwrap();
            sender.send(lock).unwrap();
        });

        assert!(
            receiver.recv_timeout(Duration::from_millis(100)).is_err(),
            "a second watcher acquired leadership while the first was alive"
        );
        drop(leader);
        let replacement = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("follower did not take over after leader exit");
        drop(replacement);
        follower.join().unwrap();
    }
}
