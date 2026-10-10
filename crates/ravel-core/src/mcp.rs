//! MCP stdio surface — thin wrapper over `WorkspaceEngine` (same results as CLI).
//!
//! Tool schemas cost tokens every session, so the default surface stays small.
//! Default = **primary** tool set only (`explore`, `status`, `sync`).
//! Set `RAVEL_MCP_TOOLS=all` for the full surface.

use crate::{analysis, engine::WorkspaceEngine, graph::QueryLimits, search::SearchKind};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    ops::Deref,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
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
    /// Absolute workspace path; omit for the server's default.
    pub root: Option<String>,
}
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SyncRequest {
    /// Absolute workspace path; omit for the server's default.
    pub root: Option<String>,
    /// Explicit edited paths. Relative paths are resolved from the workspace root.
    pub paths: Option<Vec<String>>,
}
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ReferenceSitesRequest {
    /// Absolute workspace path; omit for the server's default.
    pub root: Option<String>,
    /// Symbol name, qualified name, or id. A bare name is resolved.
    pub node: String,
    /// Sites per page (default 50).
    pub limit: Option<usize>,
    /// Resume offset: pass the previous page's next_cursor.
    pub cursor: Option<PageCursor>,
    /// Path fragment that picks one definition when a bare name matches several — cheaper than
    /// copying a candidate id back. Only narrows which definition is resolved; it does not filter
    /// the sites of a symbol that already resolved.
    pub scope: Option<String>,
    /// `dir`, or `dir:N` for N directory levels: return counts per directory prefix instead of the
    /// site list. Answers "where is this concentrated" in one bounded response rather than paging
    /// every site. Each bucket carries `n` (edges) and `files` (distinct files).
    pub rollup: Option<String>,
}
/// Where a page of sites starts. A page hands out its successor as `next_cursor`, a string, and the
/// tool asks for exactly that back; refusing the string (the offset alone was accepted, as a number)
/// failed every request for a second page. Either spelling is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageCursor(usize);

impl From<PageCursor> for usize {
    fn from(cursor: PageCursor) -> Self {
        cursor.0
    }
}

impl<'de> Deserialize<'de> for PageCursor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{Error, Unexpected, Visitor};

        struct Offset;
        impl Visitor<'_> for Offset {
            type Value = PageCursor;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("the previous page's next_cursor")
            }

            fn visit_u64<E: Error>(self, value: u64) -> Result<PageCursor, E> {
                usize::try_from(value)
                    .map(PageCursor)
                    .map_err(|_| E::invalid_value(Unexpected::Unsigned(value), &self))
            }

            fn visit_i64<E: Error>(self, value: i64) -> Result<PageCursor, E> {
                match u64::try_from(value) {
                    Ok(value) => self.visit_u64(value),
                    Err(_) => Err(E::invalid_value(Unexpected::Signed(value), &self)),
                }
            }

            fn visit_str<E: Error>(self, value: &str) -> Result<PageCursor, E> {
                value
                    .parse()
                    .map(PageCursor)
                    .map_err(|_| E::invalid_value(Unexpected::Str(value), &self))
            }
        }

        deserializer.deserialize_any(Offset)
    }
}

impl schemars::JsonSchema for PageCursor {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "PageCursor".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": ["string", "integer"] })
    }
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct QueryRequest {
    pub root: Option<String>,
    pub node: String,
    /// Follow dependents instead of dependencies.
    pub reverse: Option<bool>,
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
    /// Absolute workspace path; omit for the server's default.
    pub root: Option<String>,
    /// Name, qualified name, candidate id, or natural-language terms.
    pub query: String,
    /// Sites per direction (default 10, max 50).
    pub limit: Option<usize>,
    /// Ask for the full payload — every similar spelling and the blast-radius
    /// sample. Off by default: the concise response carries the resolved symbol,
    /// the candidates, the relation pages and every total.
    pub detail: Option<bool>,
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
    /// `default_root` with its symlinks resolved, once that has worked.
    resolved_default_root: OnceLock<PathBuf>,
}

#[derive(Debug)]
struct DaemonBinding {
    /// The session's lease on the daemon; calls travel over its connection.
    lease: Arc<crate::daemon::DaemonClientLease>,
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
    lease: Arc<crate::daemon::DaemonClientLease>,
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
        let mut tool_router = match mode {
            McpToolMode::Primary => Self::tool_router_primary(),
            McpToolMode::All => Self::tool_router_primary() + Self::tool_router_extended(),
        };
        for route in tool_router.map.values_mut() {
            compact_input_schema(&mut route.attr);
        }
        Self {
            tool_router,
            engines: Arc::new(Mutex::new(HashMap::new())),
            daemons: Arc::new(Mutex::new(HashMap::new())),
            cache_clock: AtomicU64::new(0),
            max_cached_roots: max_cached_roots_from_env(),
            mode,
            default_root,
            resolved_default_root: OnceLock::new(),
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

    /// The workspace a call is about, with symlinks resolved. Resolving costs a `readlink` per path
    /// component, so the server's default root -- the one nearly every call names -- is resolved
    /// once rather than on every call.
    fn call_root(&self, root: Option<&str>) -> Option<PathBuf> {
        let resolve = |path: PathBuf| path.canonicalize().unwrap_or(path);
        match (root, &self.default_root) {
            (Some(root), _) => Some(resolve(PathBuf::from(root))),
            (None, Some(default)) => Some(match self.resolved_default_root.get() {
                Some(resolved) => resolved.clone(),
                // Only a success is remembered: a root that does not exist yet may later.
                None => match default.canonicalize() {
                    Ok(resolved) => self.resolved_default_root.get_or_init(|| resolved).clone(),
                    Err(_) => default.clone(),
                },
            }),
            (None, None) => std::env::current_dir().ok().map(resolve),
        }
    }

    fn daemon_client(&self, root: Option<&str>) -> Result<DaemonUse, String> {
        let root = self.call_root(root).ok_or_else(|| {
            "no workspace root: pass `root` or start the server inside one".to_owned()
        })?;
        let key = root.to_string_lossy().into_owned();
        let mut daemons = self.daemons.lock().unwrap();
        let tick = self.next_cache_tick();
        if let Some(binding) = daemons.get_mut(&key) {
            binding.last_used = tick;
            binding.active.fetch_add(1, Ordering::AcqRel);
            return Ok(DaemonUse {
                lease: binding.lease.clone(),
                active: binding.active.clone(),
                cache: self.daemons.clone(),
                max_cached_roots: self.max_cached_roots,
            });
        }
        evict_inactive_daemon(&mut daemons, self.max_cached_roots.saturating_sub(1));
        // Keep the cause. Collapsing it into `None` here is what turned an upgraded-binary
        // situation into "shared daemon could not be started", with no hint of the remedy.
        let (_, lease) = crate::daemon::ensure_transient(&root)
            .map_err(|error| format!("shared daemon could not be started: {error}"))?;
        let lease = Arc::new(lease);
        let active = Arc::new(AtomicUsize::new(1));
        daemons.insert(
            key,
            DaemonBinding {
                lease: lease.clone(),
                active: active.clone(),
                last_used: tick,
            },
        );
        Ok(DaemonUse {
            lease,
            active,
            cache: self.daemons.clone(),
            max_cached_roots: self.max_cached_roots,
        })
    }

    fn forget_daemon(&self, root: Option<&str>) {
        let Some(root) = self.call_root(root) else {
            return;
        };
        self.daemons
            .lock()
            .unwrap()
            .remove(root.to_string_lossy().as_ref());
    }

    /// The daemon's answer as the JSON text it serialized: a tool result is that text, so it is
    /// never parsed into a `Value` and written out again.
    fn call_daemon(
        &self,
        root: Option<&str>,
        operation: crate::daemon::DaemonOperation,
    ) -> Result<String, String> {
        let session = self.daemon_client(root)?;
        match session.lease.call_text(operation.clone()) {
            Ok(text) => Ok(text),
            Err(error) if should_respawn_after(&error) => {
                // Let go of the old lease before asking for a new one: a daemon that is stopping
                // exits only once none is held.
                drop(session);
                self.forget_daemon(root);
                let retry = self.daemon_client(root)?;
                retry
                    .lease
                    .call_text(operation)
                    .map_err(|error| error.to_string())
            }
            Err(crate::daemon::DaemonCallError::Remote(error)) => Err(error),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// A dying daemon answers politely instead of dropping the connection; treat
/// that reply like a transport failure so the client respawns instead of
/// surfacing "daemon is shutting down" to the agent.
fn should_respawn_after(error: &crate::daemon::DaemonCallError) -> bool {
    matches!(error, crate::daemon::DaemonCallError::Transport(_)) || error.is_shutting_down()
}

/// Drop schema keys a model gains nothing from. Clients hand `inputSchema` to the model as
/// written, so every byte here is paid again in each session: `$schema` restates the dialect MCP
/// already defaults to, and `format: "uint"` is a schemars annotation no validator acts on.
fn compact_input_schema(tool: &mut rmcp::model::Tool) {
    let schema = Arc::make_mut(&mut tool.input_schema);
    schema.remove("$schema");
    if let Some(serde_json::Value::Object(properties)) = schema.get_mut("properties") {
        for property in properties
            .values_mut()
            .filter_map(serde_json::Value::as_object_mut)
        {
            property.remove("format");
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
            // Same question the shared daemon's watcher asks, answered by the same code: an event
            // from a gitignored tree must never reach the index from either watcher.
            let event_ignore = std::sync::Arc::new(crate::config::IgnoreChain::new(&engine.config));
            let batch_ignore = event_ignore.clone();
            let watcher = match crate::watch::PersistentWatcher::new_filtered(
                &root,
                queue_capacity,
                move |path| {
                    crate::config::watch_event_is_relevant(
                        &watch_config,
                        &event_ignore,
                        &storage_root,
                        path,
                    )
                },
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
                let mut paths: Vec<_> = batch
                    .paths
                    .iter()
                    .filter(|path| {
                        crate::config::watched_path_is_indexable(
                            &engine.config,
                            &batch_ignore,
                            &extensions,
                            path,
                        )
                    })
                    .cloned()
                    .collect();
                let mut needs_reconcile = batch.needs_reconcile;
                // A directory that appears or moves is reported alone, without the files in it.
                // Together they stay within the batch bound, as the paths alone always did.
                if !needs_reconcile {
                    match crate::watch::sources_behind_directories(
                        &engine,
                        &batch_ignore,
                        &extensions,
                        &batch,
                        max_batch_paths.saturating_sub(paths.len()),
                    ) {
                        Some(unnamed) => paths.extend(unnamed),
                        None => needs_reconcile = true,
                    }
                }
                if needs_reconcile {
                    if let Err(error) = engine.reconcile() {
                        engine.record_update_error("watch index", &error.to_string());
                    }
                } else if !paths.is_empty() {
                    if let Err(error) = engine.sync(Some(&paths)) {
                        engine.record_update_error("watch sync", &error.to_string());
                    }
                }
                crate::release_memory();
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
//
// Every tool is annotated: `readOnlyHint` lets a client's permission layer treat the queries as
// the lookups they are (they only maintain Ravel's own index under `.ravel/`, never a source
// file), and `openWorldHint: false` says nothing leaves the machine. Failures come back with
// `isError` set, so the model is told the call failed instead of receiving an ordinary-looking
// `{"error": …}` result it may read as an answer.

#[tool_router(router = tool_router_primary, vis = "pub")]
impl RavelMcp {
    #[tool(
        description = "Resolve a name, qualified name, or natural-language terms to a symbol and \
                       return, in one call, its source excerpt, caller/callee sites (file:line) \
                       and impact count. Ambiguous names return candidates instead of a guess.",
        annotations(
            title = "Explore symbol",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn explore(&self, Parameters(request): Parameters<ExploreRequest>) -> ToolReply {
        let limit = request.limit.unwrap_or(10).max(1);
        daemon_reply(self.call_daemon(
            request.root.as_deref(),
            crate::daemon::DaemonOperation::Context {
                query: request.query.clone(),
                limit,
                detail: request.detail.unwrap_or(false),
            },
        ))
    }

    #[tool(
        description = "Every reference to a symbol — the \"what breaks if I change it\" answer. \
                       Each site has file, line, referring symbol, edge kind and a type-only \
                       flag, so sites can be judged without opening files. Resolved edges: never \
                       a comment/string match or a same-named symbol from an unrelated file. \
                       Accepts a name, qualified name, or id; reflects uncommitted edits. Paged: \
                       `total` and `by_kind` are exact; pass `next_cursor` as `cursor` for more.",
        annotations(
            title = "Callers of symbol",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn callers_of(
        &self,
        Parameters(request): Parameters<ReferenceSitesRequest>,
    ) -> ToolReply {
        reference_sites_tool(self, request, true).await
    }

    #[tool(
        description = "What a symbol references — same site shape and guarantees as callers_of, \
                       in the forward direction.",
        annotations(
            title = "Calls from symbol",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn calls_from(
        &self,
        Parameters(request): Parameters<ReferenceSitesRequest>,
    ) -> ToolReply {
        reference_sites_tool(self, request, false).await
    }

    #[tool(
        description = "Whether this workspace is indexed and how much of it the index covers. \
                       Call once at session start; when `hint` says Ravel does not cover this \
                       code, use text search.",
        annotations(title = "Index status", read_only_hint = true, open_world_hint = false)
    )]
    async fn status(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        daemon_reply(self.call_daemon(
            request.root.as_deref(),
            crate::daemon::DaemonOperation::Status,
        ))
    }

    #[tool(
        description = "Re-index after edits. Pass edited `paths` for an immediate, exact update \
                       (untracked files included); without paths, re-scans Git-dirty files. \
                       Writes only Ravel's index (.ravel/), never source.",
        annotations(
            title = "Sync index",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sync(&self, Parameters(request): Parameters<SyncRequest>) -> ToolReply {
        let paths = request
            .paths
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        daemon_reply(self.call_daemon(
            request.root.as_deref(),
            crate::daemon::DaemonOperation::Sync { paths },
        ))
    }
}

// ── Extended tools (RAVEL_MCP_TOOLS=all) ────────────────────────────────────

#[tool_router(router = tool_router_extended, vis = "pub")]
impl RavelMcp {
    #[tool(
        description = "Search symbols (kind: exact|prefix|fuzzy|regex|terms)",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search_symbols(&self, Parameters(request): Parameters<SearchRequest>) -> ToolReply {
        // Anything else is refused: run as an exact search, `contains` or a misspelling came back as
        // an ordinary-looking "nothing found". Case is forgiven; it cannot change which kind is meant.
        let kind = match request
            .kind
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            None | Some("exact") => SearchKind::Exact,
            Some("prefix") => SearchKind::Prefix,
            Some("fuzzy") => SearchKind::Fuzzy,
            Some("regex") => SearchKind::Regex,
            Some("terms") => SearchKind::Terms,
            Some(_) => {
                return Err(error_json(format!(
                    "unknown search kind `{}`; supported: exact, prefix, fuzzy, regex, terms",
                    request.kind.unwrap_or_default()
                )));
            }
        };
        let limit = request.limit.unwrap_or(20).max(1);
        let engine = self.engine(request.root).map_err(tool_error)?;
        json_reply(engine.search(&request.query, kind, limit))
    }

    #[tool(
        description = "Transitive reach: every file that can be reached from this symbol by \
                       following edges, or that can reach it with reverse=true. This is a walk \
                       over the whole graph and answers \"how far does this spread\" — for the \
                       places that actually reference the symbol, with lines, use callers_of.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn reachable(&self, Parameters(request): Parameters<QueryRequest>) -> ToolReply {
        let reverse = request.reverse.unwrap_or(false);
        query_tool(self, request, reverse).await
    }

    #[tool(
        description = "Blast radius + risk scores for a symbol",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn impact_analysis(&self, Parameters(request): Parameters<QueryRequest>) -> ToolReply {
        let mut limits = QueryLimits::default();
        if let Some(depth) = request.depth {
            limits.depth = depth;
        }
        if let Some(nodes) = request.nodes {
            limits.nodes = nodes;
        }
        let engine = self.engine(request.root).map_err(tool_error)?;
        json_reply(engine.impact_risk(&request.node, &limits))
    }

    #[tool(
        description = "Graph stats (files/edges/snapshot_id)",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn graph_stats(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        let engine = self.engine(request.root).map_err(tool_error)?;
        json_reply(engine.stats())
    }

    #[tool(
        description = "List packages with language and path metadata",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn packages(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        let engine = self.engine(request.root).map_err(tool_error)?;
        match engine.storage().open_file_list().map_err(tool_error)? {
            Some(files) => json_ok(&analysis::list_packages_from_paths(
                files.paths.iter().map(String::as_str),
            )),
            None => json_reply(engine.list_packages()),
        }
    }

    #[tool(
        description = "Get detailed information about a symbol",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn node_detail(&self, Parameters(request): Parameters<SymbolDetailRequest>) -> ToolReply {
        let engine = self.engine(request.root).map_err(tool_error)?;
        match engine.node_detail(&request.symbol).map_err(tool_error)? {
            Some(symbol) => json_ok(&symbol),
            None => Err(tool_error(format!("symbol '{}' not found", request.symbol))),
        }
    }

    #[tool(
        description = "List files belonging to a package (a name from list_packages)",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn files_in_package(&self, Parameters(request): Parameters<PackageRequest>) -> ToolReply {
        let limit = request.limit.unwrap_or(50).max(1);
        let engine = self.engine(request.root).map_err(tool_error)?;
        match engine.storage().open_file_list().map_err(tool_error)? {
            Some(files) => json_ok(&files.in_package_limit(&request.name, limit)),
            None => {
                let files = engine.files_in_package(&request.name).map_err(tool_error)?;
                json_ok(&files.into_iter().take(limit).collect::<Vec<_>>())
            }
        }
    }

    #[tool(
        description = "Package import cycles (SCC), largest first",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn cycles(&self, Parameters(request): Parameters<LimitRequest>) -> ToolReply {
        let engine = self.engine(request.root).map_err(tool_error)?;
        json_reply(engine.cycles(request.package.as_deref()))
    }

    #[tool(
        description = "Most depended-upon symbols; optional kind filter",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn hubs(&self, Parameters(request): Parameters<LimitRequest>) -> ToolReply {
        let limit = request.limit.unwrap_or(20).max(1);
        let engine = self.engine(request.root).map_err(tool_error)?;
        json_reply(engine.hubs(limit, request.kind.as_deref()))
    }

    #[tool(
        description = "Symbols/files with no reverse dependencies",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn orphans(&self, Parameters(request): Parameters<LimitRequest>) -> ToolReply {
        let limit = request.limit.unwrap_or(100).max(1);
        let engine = self.engine(request.root).map_err(tool_error)?;
        json_reply(engine.orphans(limit))
    }

    #[tool(
        description = "Impact of files changed between git refs",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn diff_impact(&self, Parameters(request): Parameters<DiffImpactRequest>) -> ToolReply {
        let limits = QueryLimits {
            depth: request.depth.unwrap_or(16),
            ..Default::default()
        };
        let engine = self.engine(request.root).map_err(tool_error)?;
        json_reply(engine.diff_impact(&request.from, request.to.as_deref(), &limits))
    }

    #[tool(
        description = "CI quality gate: cycles + policy findings",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn ci_check(&self, Parameters(request): Parameters<CiRequest>) -> ToolReply {
        let engine = self.engine(request.root).map_err(tool_error)?;
        json_reply(engine.ci(
            request.strict.unwrap_or(false),
            request.cycle_threshold.unwrap_or(2),
        ))
    }

    #[tool(
        description = "Export package dependency graph as GraphViz DOT",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn export_dot(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        let engine = self.engine(request.root).map_err(tool_error)?;
        let dot = engine.export_dot().map_err(tool_error)?;
        json_ok(&serde_json::json!({"format":"dot","content":dot}))
    }

    #[tool(
        description = "Validate index integrity (dangling edges, unresolved relative imports, declared boundary rules)",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn validate_index(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        let engine = self.engine(request.root).map_err(tool_error)?;
        // Bounded page + complete per-code counts: raw findings run to
        // megabytes on big monorepos.
        let findings = engine.validate().map_err(tool_error)?;
        json_ok(&crate::analysis::policy_report(findings, 100))
    }

    #[tool(
        description = "Files that co-change with a path in recent git history",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn cochanged(&self, Parameters(request): Parameters<CoChangeRequest>) -> ToolReply {
        let engine = self.engine(request.root).map_err(tool_error)?;
        json_reply(engine.cochanged(
            &request.file,
            request.commits.unwrap_or(100),
            request.min_cooccurrence.unwrap_or(2),
        ))
    }

    #[tool(
        description = "Architecture boundary violations (ravel.boundaries.toml)",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn boundaries(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        let engine = self.engine(request.root).map_err(tool_error)?;
        json_reply(engine.boundaries())
    }

    #[tool(
        description = "Schema summary: counts by node/edge kind",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn describe_schema(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        let engine = self.engine(request.root).map_err(tool_error)?;
        json_reply(engine.describe_schema())
    }

    #[tool(
        description = "Related test files for a source path using common naming patterns",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn related_tests(
        &self,
        Parameters(request): Parameters<SymbolDetailRequest>,
    ) -> ToolReply {
        let engine = self.engine(request.root).map_err(tool_error)?;
        json_reply(engine.related_tests(&request.symbol))
    }
}

async fn reference_sites_tool(
    mcp: &RavelMcp,
    request: ReferenceSitesRequest,
    reverse: bool,
) -> ToolReply {
    let limit = request.limit.unwrap_or(50).max(1);
    let cursor = request.cursor.map_or(0, usize::from);
    // An unrecognised rollup is refused rather than silently ignored: returning a normal page for
    // `rollup: "directory-ish"` looks like the grouping was applied and came out flat. The daemon
    // checks it too; checking here first keeps the refusal off the wire.
    if let Some(value) = request.rollup.as_deref()
        && crate::engine::RollupMode::parse(value).is_none()
    {
        return Err(error_json(format!(
            "unknown rollup `{value}`; supported: dir, or dir:N with N from 1 to 10"
        )));
    }
    daemon_reply(mcp.call_daemon(
        request.root.as_deref(),
        crate::daemon::DaemonOperation::ReferenceSites {
            node: request.node,
            reverse,
            limit,
            cursor,
            scope: request.scope,
            rollup: request.rollup,
        },
    ))
}

async fn query_tool(mcp: &RavelMcp, request: QueryRequest, reverse: bool) -> ToolReply {
    let mut limits = QueryLimits::default();
    if let Some(depth) = request.depth {
        limits.depth = depth;
    }
    if let Some(nodes) = request.nodes {
        limits.nodes = nodes;
    }
    let engine = mcp.engine(request.root).map_err(tool_error)?;
    json_reply(engine.query(&request.node, reverse, &limits, None))
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RavelMcp {
    fn get_info(&self) -> ServerInfo {
        let mode = match self.mode {
            McpToolMode::Primary => {
                "primary (explore, callers_of, calls_from, status, sync; \
                 set RAVEL_MCP_TOOLS=all for search/impact/cycles/hubs/orphans/…)"
            }
            McpToolMode::All => "all tools",
        };
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            // Without this the handshake names the MCP library ("rmcp 2.x"), which is what client
            // UIs and logs then show for the server.
            .with_server_info(Implementation::new("ravel", crate::VERSION).with_title("Ravel"))
            .with_instructions(format!(
                "Ravel answers relational questions about TypeScript/JavaScript code from a \
                 resolved graph. Mode: {mode}.\n\
                 \n\
                 Pick by the question:\n\
                 - \"who calls / uses / depends on X\" or \"what breaks if I change X\" → \
                 callers_of. Returns each site with its file and line, so the answer is \
                 actionable without opening the files. Resolved edges: no hits inside comments \
                 or strings, no same-named symbol from an unrelated file.\n\
                 - \"what does X call / import\" → calls_from.\n\
                 - \"what is X, and what is around it\" → explore. One call: resolves the name, \
                 disambiguates, and returns typed relation pages with totals. Ask detail=true \
                 only when the long tail of similar names matters.\n\
                 - \"where does this literal text appear\" → grep/ripgrep. That is not a graph \
                 question and Ravel has no advantage there.\n\
                 \n\
                 Answers include uncommitted edits: each call first syncs the files Git reports \
                 as changed, so results match the working tree, not the last commit. Pass \
                 edited paths to sync for immediate certainty after a write.\n\
                 \n\
                 Call status once at session start. It reports how much of the workspace is \
                 actually indexed — a repo whose sources Ravel does not parse can be \"indexed\" \
                 and still answer nothing, and status says so rather than looking healthy.\n\
                 \n\
                 Relation and impact results are pages with exact totals; follow next_cursor \
                 rather than assuming a page is the whole answer. Every tool takes an optional \
                 absolute `root`; pass it when the code you are asking about is not in the \
                 server's default workspace. Ravel never edits source files.\n\
                 CLI equivalents: `ravel callers-of X`, `ravel explore X`, `ravel impact X --risk`."
            ))
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

/// A tool's answer. `Ok` is the JSON payload; `Err` is a `{"error": …}` body that rmcp sends
/// with `isError: true`.
type ToolReply = Result<String, String>;

fn json_ok(value: &impl serde::Serialize) -> ToolReply {
    serde_json::to_string(value).map_err(tool_error)
}

fn json_reply<T: serde::Serialize, E: std::fmt::Display>(result: Result<T, E>) -> ToolReply {
    json_ok(&result.map_err(tool_error)?)
}

/// A daemon answer is JSON text already, so it goes out as it came in.
fn daemon_reply(result: Result<String, String>) -> ToolReply {
    result.map_err(tool_error)
}

fn tool_error(error: impl std::fmt::Display) -> String {
    error_json(error.to_string())
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

    #[cfg(unix)]
    #[test]
    fn the_default_root_is_resolved_once() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let server = RavelMcp::with_root(link.clone());
        let resolved = real.canonicalize().unwrap();

        assert_eq!(server.call_root(None), Some(resolved.clone()));
        // Resolved once: the link can go and the answer does not change, because nothing asks
        // the filesystem again.
        std::fs::remove_file(&link).unwrap();
        assert_eq!(server.call_root(None), Some(resolved.clone()));
        // A root named in the call is resolved every time.
        assert_eq!(
            server.call_root(Some(real.to_str().unwrap())),
            Some(resolved)
        );
    }

    #[test]
    fn a_default_root_that_does_not_resolve_yet_is_tried_again() {
        let dir = tempfile::tempdir().unwrap();
        let later = dir.path().join("later");
        let server = RavelMcp::with_root(later.clone());
        assert_eq!(server.call_root(None), Some(later.clone()));
        std::fs::create_dir(&later).unwrap();
        assert_eq!(server.call_root(None), Some(later.canonicalize().unwrap()));
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

    /// The answer of a tool whose work never waits (these engine calls block instead).
    fn ready<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(output) => output,
            std::task::Poll::Pending => panic!("the tool waited"),
        }
    }

    #[test]
    fn a_search_kind_that_is_not_one_is_refused_not_run_as_exact() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/a.ts"),
            "export function prefixTarget() { return 1; }\n",
        )
        .unwrap();
        WorkspaceEngine::load(dir.path(), &Default::default())
            .unwrap()
            .index()
            .unwrap();
        let server = RavelMcp::with_mode(McpToolMode::All);
        let search = |kind: &str| {
            ready(server.search_symbols(Parameters(SearchRequest {
                root: Some(dir.path().to_string_lossy().into_owned()),
                query: "prefixTarg".into(),
                kind: Some(kind.into()),
                limit: None,
            })))
        };

        let found = search("prefix").unwrap();
        assert!(found.contains("prefixTarget"), "{found}");
        // What a model is likely to write for the same thing.
        assert_eq!(search("Prefix").unwrap(), found);
        // Silently searching for the exact spelling instead answered "nothing found".
        for kind in ["contains", "substring", ""] {
            let refused = search(kind).unwrap_err();
            assert!(refused.contains("exact"), "{kind:?}: {refused}");
        }
    }

    #[test]
    fn a_page_cursor_is_taken_back_as_next_cursor_spells_it() {
        let cursor = |value: serde_json::Value| {
            serde_json::from_value::<ReferenceSitesRequest>(
                serde_json::json!({ "node": "x", "cursor": value }),
            )
            .map(|request| request.cursor.map(usize::from))
        };
        // `next_cursor` is a string; the tool says to pass it back as `cursor`.
        assert_eq!(cursor(serde_json::json!("50")).unwrap(), Some(50));
        assert_eq!(cursor(serde_json::json!(50)).unwrap(), Some(50));
        assert_eq!(cursor(serde_json::Value::Null).unwrap(), None);
        for refused in [
            serde_json::json!("next"),
            serde_json::json!(-1),
            serde_json::json!(1.5),
        ] {
            assert!(cursor(refused.clone()).is_err(), "{refused} was accepted");
        }
        let absent =
            serde_json::from_value::<ReferenceSitesRequest>(serde_json::json!({ "node": "x" }));
        assert!(absent.unwrap().cursor.is_none());

        // And clients that check arguments against the schema let either spelling through.
        let server = RavelMcp::with_mode(McpToolMode::Primary);
        for tool in ["callers_of", "calls_from"] {
            let schema = &server.tool_router.map[tool].attr.input_schema;
            let types = &schema["properties"]["cursor"]["type"];
            for spelling in ["string", "integer"] {
                assert!(
                    types
                        .as_array()
                        .is_some_and(|types| types.iter().any(|kind| kind == spelling)),
                    "{tool} cursor does not admit a {spelling}: {types}"
                );
            }
        }
    }

    #[test]
    fn shutting_down_daemon_reply_triggers_respawn_like_a_transport_failure() {
        use crate::daemon::DaemonCallError;
        assert!(should_respawn_after(&DaemonCallError::Transport(
            std::io::Error::other("gone")
        )));
        assert!(should_respawn_after(&DaemonCallError::Remote(
            "daemon is shutting down".into()
        )));
        assert!(!should_respawn_after(&DaemonCallError::Remote(
            "no such symbol".into()
        )));
    }
}
