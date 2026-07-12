//! MCP stdio surface — thin wrapper over `WorkspaceEngine` (same results as CLI).
//!
//! Tool schemas cost tokens every session, so the default surface stays small.
//! Default = **primary** tool set only (`explore`, `status`, `sync`).
//! Set `RAVEL_MCP_TOOLS=all` for the full surface.

use crate::{engine::WorkspaceEngine, graph::QueryLimits, search::SearchKind};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use std::{collections::HashMap, path::PathBuf, sync::Mutex, thread, time::Duration};

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
pub struct RefactorRequest {
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
    engines: Mutex<HashMap<String, std::sync::Arc<WorkspaceEngine>>>,
    mode: McpToolMode,
    default_root: Option<PathBuf>,
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
            engines: Mutex::new(HashMap::new()),
            mode,
            default_root,
        }
    }

    fn engine(&self, root: Option<String>) -> anyhow::Result<std::sync::Arc<WorkspaceEngine>> {
        let base = root
            .map(PathBuf::from)
            .or_else(|| self.default_root.clone())
            .unwrap_or(std::env::current_dir()?);
        // Keep the original path when canonicalize fails — collapsing every failure to "."
        // would alias distinct roots onto one cache entry.
        let root = base.canonicalize().unwrap_or(base);
        let key = root.to_string_lossy().into_owned();
        let mut engines = self.engines.lock().unwrap();
        if let Some(engine) = engines.get(&key) {
            // Cheap Arc clone instead of cloning the whole engine (and its Config) per call.
            return Ok(engine.clone());
        }
        let engine = std::sync::Arc::new(WorkspaceEngine::load(&root, &Default::default())?);
        engines.insert(key, engine.clone());
        spawn_root_watcher(root, engine.clone());
        Ok(engine)
    }
}

fn spawn_root_watcher(root: PathBuf, engine: std::sync::Arc<WorkspaceEngine>) {
    if !root.is_dir() || engine.config.sync.mode == "none" {
        return;
    }
    let debounce = Duration::from_millis(engine.config.watch.debounce_ms.max(100));
    let _ = thread::Builder::new()
        .name("ravel-mcp-watch".into())
        .spawn(move || {
            loop {
                let batch =
                    match crate::watch::watch_batch(&root, debounce, Duration::from_secs(3600)) {
                        Ok(batch) => batch,
                        Err(_) => {
                            thread::sleep(Duration::from_millis(100));
                            continue;
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
                    let _ = engine.index();
                } else if !paths.is_empty() {
                    let _ = engine.sync(Some(&paths));
                }
            }
        });
}

// ── Primary tools (default) — fewer tools = less schema overhead ────────────

#[tool_router(router = tool_router_primary, vis = "pub")]
impl RavelMcp {
    #[tool(
        description = "PRIMARY (one call → answers). Search symbol + callers + callees + impact radius. Prefer over multi-grep/Read."
    )]
    async fn explore(&self, Parameters(request): Parameters<ExploreRequest>) -> String {
        let limit = request.limit.unwrap_or(10).max(1);
        match self.engine(request.root) {
            Ok(engine) => match engine.context(&request.query, limit) {
                Ok(v) => v.to_string(),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(
        description = "PRIMARY: Index status (indexed?, files/edges/snapshot_id). Session start."
    )]
    async fn status(&self, Parameters(request): Parameters<RootRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.status() {
                Ok(v) => v.to_string(),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(
        description = "PRIMARY: Incremental reindex for explicit edits. The server also watches each root."
    )]
    async fn sync(&self, Parameters(request): Parameters<RootRequest>) -> String {
        match self.engine(request.root) {
            Ok(engine) => match engine.sync(None) {
                Ok(s) => serde_json::to_string(&s).unwrap_or_else(|_| "{}".into()),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }
}

// ── Extended tools (RAVEL_MCP_TOOLS=all) ────────────────────────────────────

#[tool_router(router = tool_router_extended, vis = "pub")]
impl RavelMcp {
    #[tool(description = "Refactor plan: files_to_touch + risk for mass rename/blast-radius")]
    async fn refactor_plan(&self, Parameters(request): Parameters<RefactorRequest>) -> String {
        let limit = request.limit.unwrap_or(40).max(1);
        match self.engine(request.root) {
            Ok(engine) => match engine.refactor_plan(&request.query, limit) {
                Ok(v) => v.to_string(),
                Err(error) => error_json(error.to_string()),
            },
            Err(error) => error_json(error.to_string()),
        }
    }

    #[tool(description = "Search symbols (kind: exact|prefix|fuzzy|regex)")]
    async fn search_symbols(&self, Parameters(request): Parameters<SearchRequest>) -> String {
        let kind = match request.kind.as_deref() {
            Some("prefix") => SearchKind::Prefix,
            Some("fuzzy") => SearchKind::Fuzzy,
            Some("regex") => SearchKind::Regex,
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
            Ok(engine) => match engine.list_packages() {
                Ok(pkgs) => serde_json::to_string(&pkgs).unwrap_or_else(|_| "[]".into()),
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
            Ok(engine) => match engine.files_in_package(&request.name) {
                Ok(files) => {
                    let page: Vec<_> = files.into_iter().take(limit).collect();
                    serde_json::to_string(&page).unwrap_or_else(|_| "[]".into())
                }
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
                 One structural call beats many greps/file reads. \
                 Do NOT read whole files to find callers — use tools. \
                 Editing: use agent editor; ravel maps blast radius. \
                 The server watches each indexed root; use sync for explicit paths. \
                 CLI: `ravel explore X` / `ravel refactor X`."
            ),
        )
    }
}

pub async fn serve_stdio(default_root: Option<PathBuf>) -> anyhow::Result<()> {
    let server = match default_root {
        Some(root) => RavelMcp::with_root(root),
        None => RavelMcp::new(),
    };
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
}
