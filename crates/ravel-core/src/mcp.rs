//! MCP stdio surface — thin wrapper over `WorkspaceEngine` (same results as CLI).
//!
//! Tool schemas cost tokens every session, so the default surface stays small.
//! Default = **primary** tool set only (`explore`, `status`, `sync`).
//! Set `RAVEL_MCP_TOOLS=all` for the full surface.

use crate::{graph::QueryLimits, query::WorkspaceQuery, search::SearchKind};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
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
    /// Resume offset: the N of the previous page's `more: cursor N`.
    pub cursor: Option<PageCursor>,
    /// Path fragment that picks one definition when a bare name matches several — cheaper than
    /// copying a candidate id back. Only narrows which definition is resolved; it does not filter
    /// the sites of a symbol that already resolved.
    pub scope: Option<String>,
    /// `dir`, or `dir:N` for N directory levels: return counts per directory prefix instead of the
    /// site list. Answers "where is this concentrated" in one bounded response rather than paging
    /// every site. Each bucket carries its edge count and its count of distinct files.
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
    /// Path fragment picking one definition when a name matches several — the same
    /// flag `callers_of` takes. Use it instead of a second call when the first one
    /// comes back with candidates.
    pub scope: Option<String>,
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
    /// Entries per page. Defaults to 20 — a single file in a real monorepo can co-change
    /// with thousands, almost all of them through bulk commits.
    pub limit: Option<usize>,
    pub cursor: Option<usize>,
    /// Commits touching more files than this are skipped as bulk edits. Defaults to 500.
    pub max_commit_files: Option<usize>,
}

#[derive(Debug)]
pub struct RavelMcp {
    tool_router: ToolRouter<Self>,
    daemons: Arc<Mutex<HashMap<String, DaemonBinding>>>,
    cache_clock: AtomicU64,
    max_cached_roots: usize,
    mode: McpToolMode,
    default_root: Option<PathBuf>,
    /// `default_root` with its symlinks resolved, once that has worked.
    resolved_default_root: OnceLock<PathBuf>,
    /// How answers are spelled: compact text unless `RAVEL_FORMAT=json` asks for the JSON.
    format: crate::render::Format,
}

#[derive(Debug)]
struct DaemonBinding {
    /// The session's lease on the daemon; calls travel over its connection.
    lease: Arc<crate::daemon::DaemonClientLease>,
    active: Arc<AtomicUsize>,
    last_used: u64,
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
            daemons: Arc::new(Mutex::new(HashMap::new())),
            cache_clock: AtomicU64::new(0),
            max_cached_roots: max_cached_roots_from_env(),
            mode,
            default_root,
            resolved_default_root: OnceLock::new(),
            format: crate::render::Format::from_env(),
        }
    }

    fn next_cache_tick(&self) -> u64 {
        self.cache_clock.fetch_add(1, Ordering::Relaxed)
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

    fn call_query(&self, root: Option<&str>, query: WorkspaceQuery) -> ToolReply {
        let view = query.view();
        self.reply(
            view,
            self.call_daemon(root, crate::daemon::DaemonOperation::Analysis(query)),
        )
    }

    /// A daemon answer on its way to the client. As JSON it goes out as it came in, never parsed
    /// and written again; as text it is parsed once to be respelled, which is the cheap side of
    /// the trade — the client pays for every token of the result, on every call.
    fn reply(
        &self,
        view: Option<crate::render::View>,
        result: Result<String, String>,
    ) -> ToolReply {
        let json = result.map_err(tool_error)?;
        let (crate::render::Format::Text, Some(view)) = (self.format, view) else {
            return Ok(json);
        };
        Ok(serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|value| crate::render::text(view, &value))
            .unwrap_or(json))
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
        self.reply(
            Some(crate::render::View::Explore),
            self.call_daemon(
                request.root.as_deref(),
                crate::daemon::DaemonOperation::Context {
                    query: request.query.clone(),
                    limit,
                    detail: request.detail.unwrap_or(false),
                    scope: request.scope.clone(),
                },
            ),
        )
    }

    #[tool(
        description = "Every reference to a symbol — the \"what breaks if I change it\" answer. \
                       Each site has file, line, referring symbol, edge kind and a type-only \
                       flag, so sites can be judged without opening files. Resolved edges: never \
                       a comment/string match or a same-named symbol from an unrelated file. \
                       Accepts a name, qualified name, or id; reflects uncommitted edits. Paged: \
                       the total and per-kind counts are exact; when the answer says \
                       `more: cursor N`, pass N as `cursor`.",
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
                       Call once at session start; when it says Ravel does not cover this \
                       code, use text search.",
        annotations(title = "Index status", read_only_hint = true, open_world_hint = false)
    )]
    async fn status(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        self.reply(
            Some(crate::render::View::Status),
            self.call_daemon(
                request.root.as_deref(),
                crate::daemon::DaemonOperation::Status,
            ),
        )
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
        self.reply(
            Some(crate::render::View::Stats),
            self.call_daemon(
                request.root.as_deref(),
                crate::daemon::DaemonOperation::Sync { paths },
            ),
        )
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
        self.call_query(
            request.root.as_deref(),
            WorkspaceQuery::Search {
                query: request.query,
                kind,
                limit: request.limit.unwrap_or(20).max(1),
            },
        )
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
        self.call_query(
            request.root.as_deref(),
            WorkspaceQuery::Impact {
                node: request.node,
                limits: QueryLimits {
                    depth: request.depth.unwrap_or(32),
                    nodes: request.nodes.unwrap_or(10_000),
                    ..Default::default()
                },
            },
        )
    }

    #[tool(
        description = "Graph stats (files/edges/snapshot_id)",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn graph_stats(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        self.call_query(request.root.as_deref(), WorkspaceQuery::Stats)
    }

    #[tool(
        description = "List packages with language and path metadata",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn packages(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        self.call_query(request.root.as_deref(), WorkspaceQuery::Packages)
    }

    #[tool(
        description = "Get detailed information about a symbol",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn node_detail(&self, Parameters(request): Parameters<SymbolDetailRequest>) -> ToolReply {
        self.call_query(
            request.root.as_deref(),
            WorkspaceQuery::NodeDetail {
                symbol: request.symbol,
            },
        )
    }

    #[tool(
        description = "List files belonging to a package (a name from list_packages)",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn files_in_package(&self, Parameters(request): Parameters<PackageRequest>) -> ToolReply {
        self.call_query(
            request.root.as_deref(),
            WorkspaceQuery::FilesInPackage {
                name: request.name,
                limit: request.limit.unwrap_or(50).max(1),
            },
        )
    }

    #[tool(
        description = "Package import cycles (SCC), largest first",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn cycles(&self, Parameters(request): Parameters<LimitRequest>) -> ToolReply {
        self.call_query(
            request.root.as_deref(),
            WorkspaceQuery::Cycles {
                package: request.package,
                files: false,
            },
        )
    }

    #[tool(
        description = "Most depended-upon symbols; optional kind filter",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn hubs(&self, Parameters(request): Parameters<LimitRequest>) -> ToolReply {
        self.call_query(
            request.root.as_deref(),
            WorkspaceQuery::Hubs {
                limit: request.limit.unwrap_or(20).max(1),
                kind: request.kind,
            },
        )
    }

    #[tool(
        description = "Symbols/files with no reverse dependencies",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn orphans(&self, Parameters(request): Parameters<LimitRequest>) -> ToolReply {
        self.call_query(
            request.root.as_deref(),
            WorkspaceQuery::Orphans {
                limit: request.limit.unwrap_or(100).max(1),
            },
        )
    }

    #[tool(
        description = "Impact of files changed between git refs",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn diff_impact(&self, Parameters(request): Parameters<DiffImpactRequest>) -> ToolReply {
        self.call_query(
            request.root.as_deref(),
            WorkspaceQuery::DiffImpact {
                from: request.from,
                to: request.to,
                limits: QueryLimits {
                    depth: request.depth.unwrap_or(16),
                    ..Default::default()
                },
            },
        )
    }

    #[tool(
        description = "CI quality gate: cycles + policy findings",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn ci_check(&self, Parameters(request): Parameters<CiRequest>) -> ToolReply {
        self.call_query(
            request.root.as_deref(),
            WorkspaceQuery::Ci {
                strict: request.strict.unwrap_or(false),
                cycle_threshold: request.cycle_threshold.unwrap_or(2),
            },
        )
    }

    #[tool(
        description = "Export package dependency graph as GraphViz DOT",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn export_dot(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        self.call_query(request.root.as_deref(), WorkspaceQuery::ExportDot)
    }

    #[tool(
        description = "Validate index integrity (dangling edges, unresolved relative imports, declared boundary rules)",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn validate_index(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        self.call_query(
            request.root.as_deref(),
            WorkspaceQuery::Validate { limit: 100 },
        )
    }

    #[tool(
        description = "Files that co-change with a path in recent git history, paged and ranked by confidence. Bulk commits are skipped and counted in commits_skipped_as_bulk.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn cochanged(&self, Parameters(request): Parameters<CoChangeRequest>) -> ToolReply {
        self.call_query(
            request.root.as_deref(),
            WorkspaceQuery::Cochanged {
                file: request.file,
                commits: request.commits.unwrap_or(100),
                min_cooccurrence: request.min_cooccurrence.unwrap_or(2),
                max_commit_files: request
                    .max_commit_files
                    .unwrap_or(crate::git::DEFAULT_MAX_COMMIT_FILES),
                limit: request.limit.unwrap_or(20).max(1),
                cursor: request.cursor.unwrap_or(0),
            },
        )
    }

    #[tool(
        description = "Architecture boundary violations (ravel.boundaries.toml)",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn boundaries(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        self.call_query(request.root.as_deref(), WorkspaceQuery::Boundaries)
    }

    #[tool(
        description = "Schema summary: counts by node/edge kind",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn describe_schema(&self, Parameters(request): Parameters<RootRequest>) -> ToolReply {
        self.call_query(request.root.as_deref(), WorkspaceQuery::Schema)
    }

    #[tool(
        description = "Related test files for a source path using common naming patterns",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn related_tests(
        &self,
        Parameters(request): Parameters<SymbolDetailRequest>,
    ) -> ToolReply {
        self.call_query(
            request.root.as_deref(),
            WorkspaceQuery::RelatedTests {
                path: request.symbol,
            },
        )
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
    mcp.reply(
        Some(crate::render::View::Relations),
        mcp.call_daemon(
            request.root.as_deref(),
            crate::daemon::DaemonOperation::ReferenceSites {
                node: request.node,
                reverse,
                limit,
                cursor,
                scope: request.scope,
                rollup: request.rollup,
            },
        ),
    )
}

async fn query_tool(mcp: &RavelMcp, request: QueryRequest, reverse: bool) -> ToolReply {
    let mut limits = QueryLimits::default();
    if let Some(depth) = request.depth {
        limits.depth = depth;
    }
    if let Some(nodes) = request.nodes {
        limits.nodes = nodes;
    }
    mcp.call_query(
        request.root.as_deref(),
        WorkspaceQuery::Reachable {
            node: request.node,
            reverse,
            limits,
        },
    )
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
                 Answers are compact text, not JSON: a line ending in `/` names a directory and \
                 the indented lines under it are its files; a site reads `line Kind \
                 symbol` (the symbol on the other end of the edge), and lines starting with `!` \
                 are warnings. Relation and impact \
                 results are pages with exact totals; when one says `more: cursor N`, pass N as \
                 cursor rather than assuming a page is the whole answer. Every tool takes an optional \
                 absolute `root`; pass it when the code you are asking about is not in the \
                 server's default workspace. Ravel never edits source files.\n\
                 CLI equivalents: `ravel callers-of X`, `ravel explore X`, `ravel impact X --risk`."
            ))
    }
}

pub async fn serve_stdio(default_root: Option<PathBuf>) -> anyhow::Result<()> {
    let server = Arc::new(match default_root {
        Some(root) => RavelMcp::with_root(root),
        None => RavelMcp::new(),
    });
    // Warm the default-root lease beside the handshake, not before it: a slow or wedged daemon
    // must not hold up `initialize`, which MCP clients time out (Claude Code after 30s).
    let warm = server.clone();
    std::thread::spawn(move || drop(warm.daemon_client(None)));
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

/// A tool's answer. `Ok` is the payload, as compact text or as JSON (see `RavelMcp::reply`);
/// `Err` is a `{"error": …}` body that rmcp sends with `isError: true`.
type ToolReply = Result<String, String>;

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
    use std::time::Duration;

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
        // Refused before any workspace is consulted, so the refusal needs no daemon to be seen.
        // Silently searching for the exact spelling instead answered "nothing found".
        let server = RavelMcp::with_mode(McpToolMode::All);
        for kind in ["contains", "substring", ""] {
            let refused = ready(server.search_symbols(Parameters(SearchRequest {
                root: None,
                query: "prefixTarg".into(),
                kind: Some(kind.into()),
                limit: None,
            })))
            .unwrap_err();
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
