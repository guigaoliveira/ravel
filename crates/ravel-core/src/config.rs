use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read configuration {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid configuration at {field}={value}: {message}")]
    Invalid {
        field: String,
        value: String,
        message: String,
    },
    #[error("invalid TOML in {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub project: ProjectConfig,
    pub log_level: String,
    pub packages: PackagesConfig,
    pub parser: ParserConfig,
    /// What not to index / treat as noise (defaults + user extras).
    pub ignore: IgnoreConfig,
    /// How incremental `sync` / auto-sync discovers changed files.
    pub sync: SyncConfig,
    pub storage: StorageConfig,
    pub cache: CacheConfig,
    pub watch: WatchConfig,
    pub limits: LimitsConfig,
    pub agents: AgentsConfig,
    /// Analysis knobs (orphans entry points, etc.)
    pub analysis: AnalysisConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProjectConfig {
    pub root: PathBuf,
    pub worktree: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PackagesConfig {
    pub globs: Vec<String>,
    pub manifests: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ParserConfig {
    pub max_file_size_kb: u64,
    /// High-level language tokens: `auto` | `typescript` | `javascript` | raw extension names.
    /// Prefer `extensions` when you need full control.
    pub languages: Vec<String>,
    /// Explicit file extensions to index (without dots), e.g. `["ts", "tsx", "vue"]`.
    /// **When non-empty, this list wins** over `languages` — fully user-defined.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
}

/// Ignore / noise configuration. Layered:
/// 1. Built-in dir names (node_modules, dist, …) unless `use_builtin_dirs = false`
/// 2. `dirs` extras from user
/// 3. `.gitignore` when `gitignore = true` (via walk builder)
/// 4. `.ravelignore` if present
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct IgnoreConfig {
    /// Extra directory **names** (any path segment) to skip, e.g. `["storybook-static", "generated"]`.
    pub dirs: Vec<String>,
    /// Use built-in noise dir list (node_modules, dist, .git, .ravel, …). Default true.
    pub use_builtin_dirs: bool,
    /// Respect `.gitignore` during discover. Default true.
    pub gitignore: bool,
}

/// Incremental freshness: git is **optional** and only answers “what changed?”.
/// Full `index` never needs git. Non-git repos use `sync` with explicit paths or `watch`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SyncConfig {
    /// `auto` = use git only if `.git` exists · `git` = prefer git · `none` = never.
    pub mode: String,
    /// Auto re-sync dirty sources on query/search/context.
    pub auto: bool,
    /// Include **untracked** files in dirty discovery. Default **false** (perf).
    /// Enable when you create brand-new files and want auto-sync without `watch`.
    pub include_untracked: bool,
    /// When untracked is on: skip emit next to a source sibling (`sibling_emit` rules).
    pub skip_sibling_emit: bool,
    /// Reuse dirty-path discovery across near-simultaneous warm MCP calls.
    pub discovery_cache_ms: u64,
    pub queue_max_ticket_bytes: u64,
    pub queue_max_tickets: usize,
    pub queue_max_paths: usize,
    pub queue_cleanup_limit: usize,
    pub queue_stale_seconds: u64,
    /// Pairs: untracked emit extension → source extensions that mark it as junk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sibling_emit: Vec<SiblingEmitRule>,
}

/// e.g. untracked `foo.js` ignored if `foo.ts` or `foo.tsx` exists beside it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SiblingEmitRule {
    /// Extension of the untracked emit file (no dot), e.g. `js`.
    pub emit: String,
    /// Source extensions that, if present as siblings, cause emit to be skipped.
    pub sources: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct StorageConfig {
    pub home: PathBuf,
    pub retention: usize,
    /// Rewrite the append-only artifact store when physical/live bytes reaches this ratio.
    pub artifact_store_max_amplification: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CacheConfig {
    pub size_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WatchConfig {
    pub debounce_ms: u64,
    /// Maximum filesystem events buffered per workspace. Overflow triggers a full reconcile.
    pub queue_capacity: usize,
    /// Maximum distinct paths retained in one exact incremental batch.
    pub max_batch_paths: usize,
    /// Maximum time spent coalescing one batch, even when events never become quiet.
    pub max_batch_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LimitsConfig {
    pub max_nodes: usize,
    pub max_edges: usize,
    pub max_bytes: u64,
    pub query_timeout_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentsConfig {
    pub mcp_tools: Vec<String>,
}

/// Optional analysis knobs. Defaults use automatic project heuristics — leave empty.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AnalysisConfig {
    /// Optional **extra** entry-point markers (merged with built-in project heuristics).
    /// Leave empty: application entry files, controllers, main/bootstrap, and package entries are detected automatically.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_points: Vec<String>,
    /// Precomputed hubs top-k written at index time.
    pub hubs_top_k: usize,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            entry_points: Vec::new(), // pure auto heuristics
            hubs_top_k: 1_000,
        }
    }
}

/// Built-in dir **names** skipped by default (any path segment).
/// Users add more via `[ignore].dirs` or disable with `use_builtin_dirs = false`.
pub const BUILTIN_NOISE_DIRS: &[&str] = &[
    "node_modules",
    "dist",
    "build",
    "out",
    "coverage",
    ".git",
    ".next",
    ".nuxt",
    ".turbo",
    ".cache",
    "tmp",
    "temp",
    "vendor",
    "allure-reports",
    "allure-results",
    ".ravel",
    "target", // rust
    "__pycache__",
    ".venv",
    "venv",
];

/// Default product extensions when `languages = ["auto"]` (TypeScript/JavaScript projects).
/// Override with `parser.extensions = [...]` for any set you want.
pub const DEFAULT_SOURCE_EXTENSIONS: &[&str] =
    &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// Default sibling-emit rules (TypeScript compilers may leave `*.js` next to `*.ts`).
pub fn default_sibling_emit_rules() -> Vec<SiblingEmitRule> {
    vec![
        SiblingEmitRule {
            emit: "js".into(),
            sources: vec!["ts".into(), "tsx".into(), "mts".into(), "cts".into()],
        },
        SiblingEmitRule {
            emit: "mjs".into(),
            sources: vec!["ts".into(), "mts".into(), "js".into()],
        },
        SiblingEmitRule {
            emit: "cjs".into(),
            sources: vec!["ts".into(), "cts".into(), "js".into()],
        },
    ]
}

/// True if `path` (under `root`) hits a noise directory segment.
/// Always strip `root` first so host `/tmp/...` is not treated as noise.
pub fn is_noise_path(root: &Path, path: &Path) -> bool {
    is_noise_path_with(root, path, true, &[])
}

/// Config-aware noise check: builtins (optional) + user `ignore.dirs`.
pub fn is_noise_path_with(
    root: &Path,
    path: &Path,
    use_builtin: bool,
    extra_dirs: &[String],
) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        if use_builtin && BUILTIN_NOISE_DIRS.iter().any(|n| *n == s) {
            return true;
        }
        extra_dirs.iter().any(|d| d == s.as_ref())
    })
}

/// Extensions that will be discovered/indexed for this config (owned strings, user-extensible).
pub fn effective_extensions(config: &Config) -> Vec<String> {
    // Explicit extensions always win — full user control.
    if !config.parser.extensions.is_empty() {
        let mut extensions: Vec<_> = config
            .parser
            .extensions
            .iter()
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty() && !e.contains(['/', '\\']) && e.len() <= 16)
            .collect();
        extensions.sort();
        extensions.dedup();
        return extensions;
    }
    let langs = &config.parser.languages;
    let auto = langs.is_empty() || langs.iter().any(|l| l == "auto" || l == "*");
    if auto {
        return DEFAULT_SOURCE_EXTENSIONS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
    }
    let mut ext: Vec<String> = Vec::new();
    for language in langs {
        match language.as_str() {
            "typescript" => {
                ext.push("ts".into());
                ext.push("tsx".into());
                ext.push("mts".into());
                ext.push("cts".into());
            }
            "javascript" => {
                for e in ["js", "jsx", "mjs", "cjs"] {
                    ext.push(e.into());
                }
            }
            // Treat unknown tokens as raw extensions (e.g. "vue", "svelte", "mts").
            other => {
                let e = other.trim_start_matches('.').to_ascii_lowercase();
                if !e.is_empty() && !e.contains('/') && e.len() <= 16 {
                    ext.push(e);
                }
            }
        }
    }
    ext.sort();
    ext.dedup();
    if ext.is_empty() {
        return DEFAULT_SOURCE_EXTENSIONS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
    }
    ext
}

impl Config {
    pub fn is_noise(&self, path: &Path) -> bool {
        is_noise_path_with(
            &self.project.root,
            path,
            self.ignore.use_builtin_dirs,
            &self.ignore.dirs,
        )
    }

    /// Convenience single-path check. Hot discovery precomputes the extension set once via
    /// [`discover_files`] instead of paying [`effective_extensions`] per path.
    pub fn is_source(&self, path: &Path) -> bool {
        ext_matches(path, &effective_extensions(self))
    }

    /// Hot-loop variant for callers that already computed the effective extensions.
    pub fn is_source_with_extensions(&self, path: &Path, extensions: &[String]) -> bool {
        ext_matches(path, extensions)
    }

    pub fn sibling_emit_rules(&self) -> Vec<SiblingEmitRule> {
        if self.sync.sibling_emit.is_empty() {
            default_sibling_emit_rules()
        } else {
            self.sync.sibling_emit.clone()
        }
    }

    /// Config allows consulting git for dirty files (`git` or `auto`).
    pub fn sync_allows_git(&self) -> bool {
        matches!(self.sync.mode.as_str(), "git" | "auto" | "")
    }

    /// Runtime: actually use git for this root (mode allows + `.git` present).
    pub fn sync_uses_git_at(&self, root: &Path) -> bool {
        // `git` and `auto` both soft-check for `.git` (no spawn thrash) — same call either way.
        self.sync_allows_git() && crate::git::is_git_repo(root)
    }

    pub fn sync_auto_enabled(&self) -> bool {
        self.sync.auto && self.sync_allows_git()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            project: ProjectConfig::default(),
            log_level: "info".into(),
            packages: PackagesConfig::default(),
            parser: ParserConfig::default(),
            ignore: IgnoreConfig::default(),
            sync: SyncConfig::default(),
            storage: StorageConfig::default(),
            cache: CacheConfig::default(),
            watch: WatchConfig::default(),
            limits: LimitsConfig::default(),
            agents: AgentsConfig::default(),
            analysis: AnalysisConfig::default(),
        }
    }
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            worktree: None,
        }
    }
}
impl Default for PackagesConfig {
    fn default() -> Self {
        Self {
            globs: vec!["**/package.json".into()],
            manifests: vec!["package.json".into()],
        }
    }
}
impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            max_file_size_kb: 1024,
            // "auto" = DEFAULT_SOURCE_EXTENSIONS; override with `extensions = [...]`
            languages: vec!["auto".into()],
            extensions: Vec::new(),
        }
    }
}
impl Default for IgnoreConfig {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            use_builtin_dirs: true,
            gitignore: true,
        }
    }
}
impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            auto: true,
            include_untracked: false, // tracked-only dirty = sub-200ms auto-sync path
            skip_sibling_emit: true,
            discovery_cache_ms: 50,
            queue_max_ticket_bytes: 1024 * 1024,
            queue_max_tickets: 1024,
            queue_max_paths: 4096,
            queue_cleanup_limit: 64,
            queue_stale_seconds: 3600,
            sibling_emit: Vec::new(),
        }
    }
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            home: PathBuf::from(".ravel"),
            retention: 3,
            artifact_store_max_amplification: 4,
        }
    }
}
impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            size_bytes: 256 * 1024 * 1024,
        }
    }
}
impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 150,
            queue_capacity: 4_096,
            max_batch_paths: 4_096,
            max_batch_ms: 1_000,
        }
    }
}
impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_nodes: 10_000,
            max_edges: 50_000,
            max_bytes: 32 * 1024 * 1024,
            query_timeout_ms: 5_000,
        }
    }
}
impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            mcp_tools: vec![
                "packages".into(),
                "search_symbols".into(),
                "callers_of".into(),
                "impact_analysis".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Flags {
    pub root: Option<PathBuf>,
    pub max_nodes: Option<usize>,
    pub max_edges: Option<usize>,
    pub max_bytes: Option<u64>,
}

impl Config {
    pub fn load(root: &Path, flags: &Flags) -> Result<Self, ConfigError> {
        // Single source of truth: collect the process env once and delegate.
        Self::load_with_env(root, flags, &env::vars().collect())
    }

    pub fn load_with_env(
        root: &Path,
        flags: &Flags,
        values: &BTreeMap<String, String>,
    ) -> Result<Self, ConfigError> {
        let path = root.join(".ravel.toml");
        let mut config = if path.is_file() {
            let text = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                path: path.clone(),
                source,
            })?;
            toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?
        } else {
            Self::default()
        };
        if let Some(flag_root) = &flags.root {
            config.project.root = flag_root.clone();
        } else if config.project.root.is_relative() {
            config.project.root = root.join(&config.project.root);
        }
        apply_env(&mut config, values)?;
        if let Some(value) = flags.max_nodes {
            config.limits.max_nodes = value;
        }
        if let Some(value) = flags.max_edges {
            config.limits.max_edges = value;
        }
        if let Some(value) = flags.max_bytes {
            config.limits.max_bytes = value;
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.parser.max_file_size_kb == 0 {
            return Err(invalid(
                "parser.max_file_size_kb",
                "0",
                "must be greater than zero",
            ));
        }
        if self.cache.size_bytes == 0 {
            return Err(invalid(
                "cache.size_bytes",
                "0",
                "must be greater than zero",
            ));
        }
        if self.limits.max_nodes == 0 || self.limits.max_edges == 0 || self.limits.max_bytes == 0 {
            return Err(invalid(
                "limits",
                "zero",
                "node, edge and byte limits must be greater than zero",
            ));
        }
        if self.watch.queue_capacity == 0
            || self.watch.max_batch_paths == 0
            || self.watch.max_batch_ms == 0
        {
            return Err(invalid(
                "watch",
                "0",
                "queue_capacity, max_batch_paths and max_batch_ms must be greater than zero",
            ));
        }
        if self.sync.queue_max_ticket_bytes == 0
            || self.sync.queue_max_tickets == 0
            || self.sync.queue_max_paths == 0
            || self.sync.queue_cleanup_limit == 0
        {
            return Err(invalid(
                "sync.queue_limits",
                "0",
                "ticket bytes, tickets, paths and cleanup limit must be greater than zero",
            ));
        }
        if self.storage.retention == 0 || self.storage.artifact_store_max_amplification == 0 {
            return Err(invalid(
                "storage",
                "0",
                "retention and artifact_store_max_amplification must be greater than zero",
            ));
        }
        if self.project.root.as_os_str().is_empty() {
            return Err(invalid("project.root", "", "must not be empty"));
        }
        match self.sync.mode.as_str() {
            "git" | "auto" | "none" | "" => {}
            other => {
                return Err(invalid("sync.mode", other, "must be auto | git | none"));
            }
        }
        Ok(())
    }

    pub fn effective_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("config is serializable")
    }
    pub fn hash(&self) -> String {
        blake3::hash(&serde_json::to_vec(self).expect("config serializes"))
            .to_hex()
            .to_string()
    }
}

fn invalid(field: &str, value: &str, message: &str) -> ConfigError {
    ConfigError::Invalid {
        field: field.into(),
        value: value.into(),
        message: message.into(),
    }
}

fn apply_env(config: &mut Config, values: &BTreeMap<String, String>) -> Result<(), ConfigError> {
    if let Some(value) = values.get("RAVEL_HOME") {
        config.storage.home = PathBuf::from(value);
    }
    if let Some(value) = values.get("RAVEL_LOG_LEVEL") {
        config.log_level = value.clone();
    }
    if let Some(value) = values.get("RAVEL_CACHE_SIZE") {
        config.cache.size_bytes = parse_num("RAVEL_CACHE_SIZE", value)?;
    }
    if let Some(value) = values.get("RAVEL_WATCH_DEBOUNCE") {
        config.watch.debounce_ms = parse_num("RAVEL_WATCH_DEBOUNCE", value)?;
    }
    if let Some(value) = values.get("RAVEL_WATCH_QUEUE_CAPACITY") {
        config.watch.queue_capacity = parse_num("RAVEL_WATCH_QUEUE_CAPACITY", value)?;
    }
    if let Some(value) = values.get("RAVEL_WATCH_MAX_BATCH_PATHS") {
        config.watch.max_batch_paths = parse_num("RAVEL_WATCH_MAX_BATCH_PATHS", value)?;
    }
    if let Some(value) = values.get("RAVEL_WATCH_MAX_BATCH_MS") {
        config.watch.max_batch_ms = parse_num("RAVEL_WATCH_MAX_BATCH_MS", value)?;
    }
    if let Some(value) = values.get("RAVEL_MAX_NODES") {
        config.limits.max_nodes = parse_num("RAVEL_MAX_NODES", value)?;
    }
    if let Some(value) = values.get("RAVEL_MAX_EDGES") {
        config.limits.max_edges = parse_num("RAVEL_MAX_EDGES", value)?;
    }
    if let Some(value) = values.get("RAVEL_MAX_BYTES") {
        config.limits.max_bytes = parse_num("RAVEL_MAX_BYTES", value)?;
    }
    if let Some(value) = values.get("RAVEL_MCP_TOOLS") {
        config.agents.mcp_tools = value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
    }
    Ok(())
}
fn parse_num<T: std::str::FromStr>(field: &str, value: &str) -> Result<T, ConfigError> {
    value
        .parse()
        .map_err(|_| invalid(field, value, "expected a non-negative integer"))
}

/// Source files present that this indexer cannot parse, as `extension -> count`.
///
/// An agent that asks whether a workspace is indexed needs to know when the answer
/// is "yes, and it covers almost nothing" — a Rust repo with three stray `.js`
/// files reports as healthy otherwise, which reads as "the graph is empty" rather
/// than "the graph does not apply here". The walk stops after `budget` entries so
/// status stays cheap; the counts are a signal, not a census.
pub fn unsupported_source_counts(config: &Config, budget: usize) -> BTreeMap<String, usize> {
    /// Extensions worth naming. Anything else is not obviously project source.
    const KNOWN_SOURCE: &[&str] = &[
        "rs", "py", "go", "java", "kt", "rb", "php", "cs", "swift", "c", "cc", "cpp", "h", "hpp",
        "scala", "ex", "exs", "dart", "lua", "zig",
    ];
    let root = &config.project.root;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(config.ignore.gitignore)
        .git_global(false)
        .git_exclude(config.ignore.gitignore)
        .follow_links(false);
    let custom = root.join(".ravelignore");
    if custom.is_file() {
        builder.add_ignore(custom);
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut seen = 0usize;
    for entry in builder.build().flatten() {
        seen += 1;
        if seen > budget {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if config.is_noise(&path) {
            continue;
        }
        if let Some(extension) = path.extension().and_then(|value| value.to_str())
            && KNOWN_SOURCE.contains(&extension)
        {
            *counts.entry(extension.to_owned()).or_default() += 1;
        }
    }
    counts
}

/// Gitignore rules live at every level, not just the workspace root: `apps/web/.gitignore` holding
/// `dist/` is what excludes `apps/web/dist`, and the index walk honours it. Consulting only the root
/// file made this check pass exactly the paths a monorepo means to exclude.
///
/// Deliberately pure pattern matching rather than asking the walk whether it would collect the file:
/// a *deleted* path is gone from the filesystem, and the watcher still has to process its removal.
pub struct IgnoreChain {
    /// Canonical form, used to build the matchers.
    root: PathBuf,
    /// The root exactly as configured. Callers hand paths spelled the way *they* got them -- from a
    /// filesystem event, from a CLI argument -- and those keep the non-canonical spelling. macOS
    /// resolves `/var/folders/...` to `/private/var/folders/...` and Windows canonicalizes to
    /// `\\?\C:\...`, so comparing against the canonical root alone made every check answer
    /// "outside the workspace" on both platforms: the filter went inert while Linux stayed green.
    root_as_given: PathBuf,
    gitignore_enabled: bool,
    per_directory:
        std::sync::Mutex<BTreeMap<PathBuf, std::sync::Arc<ignore::gitignore::Gitignore>>>,
}

impl IgnoreChain {
    pub fn new(config: &Config) -> Self {
        let root = config
            .project
            .root
            .canonicalize()
            .unwrap_or_else(|_| config.project.root.clone());
        // `WalkBuilder` only honours gitignore inside a repository; matching that keeps the two in
        // step. Applying the rules anyway would invert the bug -- a workspace with no git but a
        // stray `.gitignore` would drop files the index collects.
        let gitignore_enabled = config.ignore.gitignore && crate::git::is_git_repo(&root);
        Self {
            root,
            root_as_given: config.project.root.clone(),
            gitignore_enabled,
            per_directory: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    fn matcher_for(&self, directory: &Path) -> std::sync::Arc<ignore::gitignore::Gitignore> {
        if let Some(cached) = self
            .per_directory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(directory)
        {
            return cached.clone();
        }
        let mut builder = ignore::gitignore::GitignoreBuilder::new(directory);
        if self.gitignore_enabled {
            builder.add(directory.join(".gitignore"));
            if directory == self.root {
                builder.add(directory.join(".git/info/exclude"));
            }
        }
        let custom = directory.join(".ravelignore");
        if custom.is_file() {
            builder.add(custom);
        }
        let matcher = std::sync::Arc::new(
            builder
                .build()
                .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty()),
        );
        self.per_directory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(directory.to_path_buf(), matcher.clone());
        matcher
    }

    pub fn is_ignored(&self, path: &Path) -> bool {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        // Try both spellings of the root. `strip_prefix` is a byte comparison, so a path that came
        // in through a symlinked or non-canonical root matches only the spelling it arrived with.
        // Deliberately not canonicalizing `path`: a deleted file cannot be canonicalized, and the
        // watcher still has to process its removal.
        let stripped = absolute
            .strip_prefix(&self.root)
            .or_else(|_| absolute.strip_prefix(&self.root_as_given))
            .ok()
            .map(Path::to_path_buf)
            .or_else(|| {
                // A third spelling of the same directory -- another symlink, or the platform's own
                // alias -- matches neither root. Canonicalize the *parent* and retry: the parent
                // still exists even when the file itself was just deleted, which the watcher has to
                // keep handling. Only reached when both cheap comparisons failed, so the common path
                // pays no syscall.
                let parent = absolute.parent()?.canonicalize().ok()?;
                let name = absolute.file_name()?;
                parent
                    .join(name)
                    .strip_prefix(&self.root)
                    .ok()
                    .map(Path::to_path_buf)
            });
        let Some(relative) = stripped else {
            // Outside the workspace: not this workspace's call to make.
            return false;
        };
        let relative = relative.as_path();
        // Re-spell the path onto the canonical root before matching. The matchers are built from
        // canonical directories, and `ignore` *panics* ("path is expected to be under the root") when
        // handed a path outside the matcher root -- so passing the incoming spelling through would
        // turn a symlinked workspace into a crash rather than a wrong answer.
        let absolute = self.root.join(relative);
        // Deepest rules win in git, and a negation there can re-include a path an outer file
        // excluded, so walk inward-out and stop at the first decisive verdict.
        let mut directories: Vec<PathBuf> = Vec::new();
        let mut current = relative.parent();
        while let Some(parent) = current {
            directories.push(self.root.join(parent));
            current = parent.parent();
        }
        if directories.is_empty() {
            directories.push(self.root.clone());
        }
        for directory in directories {
            let matcher = self.matcher_for(&directory);
            match matcher.matched_path_or_any_parents(&absolute, false) {
                ignore::Match::Ignore(_) => return true,
                ignore::Match::Whitelist(_) => return false,
                ignore::Match::None => {}
            }
        }
        false
    }
}

/// Whether a raw filesystem event is worth queueing at all. Both watchers -- the shared daemon's
/// and the one an MCP server runs when it holds watch leadership -- must ask this exact question,
/// which is why it lives here: the same predicate was inlined in two places, and fixing one of them
/// left the other indexing gitignored trees.
pub fn watch_event_is_relevant(
    config: &Config,
    ignore: &IgnoreChain,
    storage_root: &Path,
    path: &Path,
) -> bool {
    // Cheap tests first: the chain may read `.gitignore` files, so only ask it about paths that
    // could otherwise be indexed.
    !path.starts_with(storage_root) && !config.is_noise(path) && !ignore.is_ignored(path)
}

/// Whether a watched path should actually be reindexed: an indexable source, and nothing a full
/// index walk would have skipped.
pub fn watched_path_is_indexable(
    config: &Config,
    ignore: &IgnoreChain,
    extensions: &[String],
    path: &Path,
) -> bool {
    config.is_source_with_extensions(path, extensions)
        && !config.is_noise(path)
        && !ignore.is_ignored(path)
}

pub fn discover_files(config: &Config) -> Result<Vec<PathBuf>, ConfigError> {
    let root = &config.project.root;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(config.ignore.gitignore)
        .git_global(false)
        .git_exclude(config.ignore.gitignore)
        .follow_links(false);
    // Soft ignores: gitignore + optional .ravelignore. Hard filter: noise dirs.
    let custom = root.join(".ravelignore");
    if custom.is_file() {
        builder.add_ignore(custom);
    }
    // Compute the eligible extension set ONCE, not per file (was a fresh Vec<String> per path).
    let exts = effective_extensions(config);
    let mut files = Vec::new();
    for entry in builder.build() {
        let entry = entry.map_err(|source| ConfigError::Read {
            path: root.clone(),
            source: std::io::Error::other(source.to_string()),
        })?;
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            let path = entry.into_path();
            if is_eligible(&path, config, &exts) {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn is_eligible(path: &Path, config: &Config, exts: &[String]) -> bool {
    if config.is_noise(path) {
        return false;
    }
    ext_matches(path, exts)
}

/// Lowercased file-extension membership test against a precomputed set.
fn ext_matches(path: &Path, exts: &[String]) -> bool {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    exts.iter().any(|e| e == &ext)
}

/// Back-compat: default-extension check without full config (CLI watch filter).
/// Prefer `config.is_source(path)` when a Config is available.
pub fn is_source_path(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    DEFAULT_SOURCE_EXTENSIONS.iter().any(|e| *e == ext)
}

pub type EffectiveConfig = BTreeMap<String, serde_json::Value>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn defaults_are_deterministic() {
        assert_eq!(Config::default(), Config::default());
    }

    #[test]
    fn explicit_extensions_win_over_languages() {
        let mut c = Config::default();
        c.parser.languages = vec!["typescript".into()];
        c.parser.extensions = vec!["vue".into(), ".Svelte".into()];
        let ext = effective_extensions(&c);
        assert_eq!(ext, vec!["svelte".to_string(), "vue".to_string()]);
    }

    #[test]
    fn explicit_extensions_are_normalized_and_deduplicated() {
        let mut c = Config::default();
        c.parser.extensions = vec![".TS".into(), "ts".into(), "../secret".into()];
        assert_eq!(effective_extensions(&c), vec!["ts"]);
    }

    #[test]
    fn raw_language_token_becomes_extension() {
        let mut c = Config::default();
        c.parser.languages = vec!["mts".into(), "cts".into()];
        let ext = effective_extensions(&c);
        assert!(ext.contains(&"mts".into()));
        assert!(ext.contains(&"cts".into()));
    }

    #[test]
    fn auto_includes_typescript_module_extensions() {
        let ext = effective_extensions(&Config::default());
        for expected in ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"] {
            assert!(
                ext.iter().any(|actual| actual == expected),
                "missing {expected}"
            );
        }
    }

    #[test]
    fn user_ignore_dirs_merge_with_builtins() {
        let dir = tempdir().unwrap();
        let mut c = Config::default();
        c.project.root = dir.path().to_path_buf();
        c.ignore.dirs = vec!["storybook-static".into()];
        let noise = dir.path().join("storybook-static/x.ts");
        let ok = dir.path().join("src/x.ts");
        assert!(c.is_noise(&noise));
        assert!(!c.is_noise(&ok));
    }

    #[test]
    fn discover_respects_extensions_and_extra_ignore() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("generated")).unwrap();
        fs::write(dir.path().join("src/a.ts"), "export {}").unwrap();
        fs::write(dir.path().join("src/b.vue"), "<template/>").unwrap();
        fs::write(dir.path().join("generated/c.ts"), "export {}").unwrap();
        let mut c = Config::default();
        c.project.root = dir.path().to_path_buf();
        c.parser.extensions = vec!["ts".into(), "vue".into()];
        c.ignore.dirs = vec!["generated".into()];
        let files = discover_files(&c).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"a.ts".into()));
        assert!(names.contains(&"b.vue".into()));
        assert!(!names.iter().any(|n| n == "c.ts"));
    }

    #[test]
    fn the_watcher_filter_excludes_what_a_full_index_walk_excludes() {
        let dir = tempdir().unwrap();
        // The shape that broke a real workspace: an agent worktree parked inside the repo and
        // gitignored, holding a second copy of every source file.
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".gitignore"), ".claude/*\n").unwrap();
        let worktree = dir.path().join(".claude/worktrees/wt/apps/admin/src");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(worktree.join("service.ts"), "export class S {}").unwrap();
        let tracked = dir.path().join("apps/admin/src");
        fs::create_dir_all(&tracked).unwrap();
        fs::write(tracked.join("service.ts"), "export class S {}").unwrap();

        let mut config = Config::default();
        config.project.root = dir.path().to_path_buf();
        config.parser.extensions = vec!["ts".into()];

        // What the full index collects is the reference the watcher has to agree with.
        let discovered: Vec<_> = discover_files(&config)
            .unwrap()
            .iter()
            .map(|path| path.strip_prefix(dir.path()).unwrap().to_path_buf())
            .collect();
        assert!(
            discovered.iter().any(|path| path.starts_with("apps")),
            "the tracked copy must be indexed: {discovered:?}"
        );
        assert!(
            !discovered.iter().any(|path| path.starts_with(".claude")),
            "a full walk must not collect the ignored worktree: {discovered:?}"
        );

        let matcher = IgnoreChain::new(&config);
        assert!(
            matcher.is_ignored(&worktree.join("service.ts")),
            "the watcher must drop an event from inside the ignored worktree"
        );
        assert!(
            !matcher.is_ignored(&tracked.join("service.ts")),
            "the watcher must keep an event for a tracked source"
        );
    }

    #[test]
    fn ignored_paths_are_recognised_by_absolute_path_and_anchored_rule() {
        // The CLI hands `sync` absolute paths, and real .gitignore files use anchored,
        // directory-only rules like `/reports/`. Both have to work or the check silently passes
        // everything through.
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".gitignore"), "/reports/\n").unwrap();
        fs::create_dir_all(dir.path().join("reports")).unwrap();
        let ignored = dir.path().join("reports/probe.ts");
        fs::write(&ignored, "export const x = 1;").unwrap();

        let mut config = Config::default();
        config.project.root = dir.path().to_path_buf();
        let matcher = IgnoreChain::new(&config);
        assert!(
            matcher.is_ignored(&ignored),
            "absolute path under an anchored directory rule must be ignored"
        );
        assert!(
            matcher.is_ignored(Path::new("reports/probe.ts")),
            "the relative spelling must be ignored too"
        );

        // The combination that actually shipped broken: a relative configured root (`--root .`)
        // with the absolute paths the CLI resolves. Nothing stripped, so every check answered
        // "not ignored" and gitignored files sailed into the index.
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let mut relative_config = Config::default();
        relative_config.project.root = PathBuf::from(".");
        let relative_matcher = IgnoreChain::new(&relative_config);
        let ignored_under_relative_root = relative_matcher
            .is_ignored(&dir.path().canonicalize().unwrap().join("reports/probe.ts"));
        std::env::set_current_dir(previous).unwrap();
        assert!(
            ignored_under_relative_root,
            "a relative root must still recognise an absolute ignored path"
        );
    }

    #[test]
    fn a_nested_gitignore_excludes_as_much_as_the_walk_does() {
        // Rules live at every level. Consulting only the root `.gitignore` passed exactly the paths
        // a monorepo means to exclude -- `apps/web/.gitignore` with `dist/` is what hides
        // `apps/web/dist`, and the index walk honours it.
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".gitignore"), "/reports/\n").unwrap();
        let web = dir.path().join("apps/web");
        fs::create_dir_all(web.join("src")).unwrap();
        fs::create_dir_all(web.join("dist")).unwrap();
        fs::write(web.join(".gitignore"), "dist/\n").unwrap();
        fs::write(web.join("src/app.ts"), "export const a = 1;").unwrap();
        fs::write(web.join("dist/app.js"), "export const a = 1;").unwrap();

        let mut config = Config::default();
        config.project.root = dir.path().to_path_buf();
        config.parser.extensions = vec!["ts".into(), "js".into()];

        let discovered: Vec<_> = discover_files(&config)
            .unwrap()
            .iter()
            .map(|path| path.strip_prefix(dir.path()).unwrap().to_path_buf())
            .collect();
        assert!(
            !discovered
                .iter()
                .any(|path| path.starts_with("apps/web/dist")),
            "the walk honours the nested rule: {discovered:?}"
        );

        let chain = IgnoreChain::new(&config);
        assert!(
            chain.is_ignored(&web.join("dist/app.js")),
            "the watcher must honour the nested rule too"
        );
        assert!(
            !chain.is_ignored(&web.join("src/app.ts")),
            "a nested rule must not spill onto sibling directories"
        );
    }

    #[test]
    fn a_negation_in_a_nested_gitignore_re_includes_the_path() {
        // Deeper rules win in git, including negations, so the chain has to stop at the first
        // decisive verdict walking outward rather than OR-ing every level together.
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".gitignore"), "*.gen.ts\n").unwrap();
        let keep = dir.path().join("packages/keep");
        fs::create_dir_all(&keep).unwrap();
        fs::write(keep.join(".gitignore"), "!*.gen.ts\n").unwrap();
        fs::write(keep.join("schema.gen.ts"), "export const a = 1;").unwrap();

        let mut config = Config::default();
        config.project.root = dir.path().to_path_buf();
        config.parser.extensions = vec!["ts".into()];

        let discovered = discover_files(&config).unwrap();
        let walk_keeps = discovered
            .iter()
            .any(|path| path.ends_with("schema.gen.ts"));
        assert_eq!(
            walk_keeps,
            !IgnoreChain::new(&config).is_ignored(&keep.join("schema.gen.ts")),
            "the chain must agree with the walk about a negated nested rule"
        );
    }

    // Symlinks only: Windows needs privileges to create them, and an early `return` in the test
    // body trips `-D warnings` with unreachable code. The behaviour under review -- accepting a path
    // spelled differently from the configured root -- is exercised on Windows by the canonicalized
    // `\\?\C:\...` form, which the same code path handles.
    #[cfg(unix)]
    #[test]
    fn a_root_reached_through_a_symlink_still_applies_gitignore() {
        // macOS hands out `/var/folders/...`, a symlink to `/private/var/folders/...`, and Windows
        // canonicalizes to `\\?\C:\...`. Canonicalizing only the root and then byte-comparing
        // prefixes makes every check answer "outside the workspace", so the filter goes inert on
        // both platforms while passing on Linux.
        let real = tempdir().unwrap();
        fs::create_dir_all(real.path().join(".git")).unwrap();
        fs::write(real.path().join(".gitignore"), "/generated/\n").unwrap();
        fs::create_dir_all(real.path().join("generated")).unwrap();
        let ignored_real = real.path().join("generated/out.ts");
        fs::write(&ignored_real, "export const a = 1;").unwrap();

        let link_parent = tempdir().unwrap();
        let link = link_parent.path().join("link");
        let other_link = link_parent.path().join("other");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        // A second alias for the same directory. macOS exposed this shape on its own: there the
        // tempdir's "real" path (`/var/folders/...`) is itself an alias for the canonical
        // `/private/var/folders/...`, so a path can arrive spelled a third way.
        std::os::unix::fs::symlink(real.path(), &other_link).unwrap();

        let mut config = Config::default();
        config.project.root = link.clone();
        let chain = IgnoreChain::new(&config);
        assert!(
            chain.is_ignored(&link.join("generated/out.ts")),
            "a path spelled through the symlinked root must still be ignored"
        );
        assert!(
            chain.is_ignored(&ignored_real),
            "and so must the same file spelled through the real path"
        );
        assert!(
            chain.is_ignored(&other_link.join("generated/out.ts")),
            "and so must a third spelling of the same directory"
        );
        assert!(
            !chain.is_ignored(&link.join("src/keep.ts")),
            "while a normal path stays indexable"
        );
        assert!(
            !chain.is_ignored(&other_link.join("src/keep.ts")),
            "through any spelling"
        );
    }

    #[test]
    fn a_stray_gitignore_outside_a_repo_does_not_shrink_the_watcher() {
        // `WalkBuilder` only applies gitignore rules inside a repository. If the single-path
        // matcher applied them anyway, the watcher would drop files the full index collects --
        // the same divergence as the original bug, pointing the other way.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "src/*\n").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        let source = dir.path().join("src/a.ts");
        fs::write(&source, "export {}").unwrap();

        let mut config = Config::default();
        config.project.root = dir.path().to_path_buf();
        config.parser.extensions = vec!["ts".into()];

        let discovered = discover_files(&config).unwrap();
        assert!(
            discovered.iter().any(|path| path.ends_with("a.ts")),
            "no repository here, so the walk keeps the file: {discovered:?}"
        );
        assert!(
            !IgnoreChain::new(&config).is_ignored(&source),
            "the watcher must keep it too"
        );
    }
}
