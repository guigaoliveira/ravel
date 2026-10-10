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
/// 3. When `gitignore = true`, inside a git repository: every `.gitignore` from a file's directory
///    up to the top of the repository (above the project root too), deepest first, then the
///    repository's `.git/info/exclude`
/// 4. The project root's `.ravelignore` if present (gitignore syntax), below the git rules
///
/// `.ignore` files, global gitignore and `.ravelignore` files below the root are not read. The index
/// walk and the single-path check behind `sync` and the watchers ([`IgnoreChain`]) apply exactly
/// these rules, and a directory they exclude hides everything in it.
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
    /// Include **untracked** files in dirty discovery, so a file created since the last commit
    /// reaches auto-sync and `ravel sync` like an edited one. Default **true**: listing them costs
    /// `git status` about 20 ms more on a 20k-file tree. Set false on a tree with thousands of
    /// untracked build outputs that are not gitignored.
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

/// Formats that *contain* TypeScript and import TS symbols. Unlike the rest of the known-source list,
/// their absence from the graph hides real references -- which is why they, and only they, make an
/// empty relation answer untrustworthy.
pub const COMPONENT_SOURCE_EXTENSIONS: &[&str] = &["vue", "svelte", "astro"];

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
    rel.components()
        .any(|c| is_noise_component(c, use_builtin, extra_dirs))
}

fn is_noise_component(
    component: std::path::Component<'_>,
    use_builtin: bool,
    extra_dirs: &[String],
) -> bool {
    let s = component.as_os_str().to_string_lossy();
    if use_builtin && BUILTIN_NOISE_DIRS.iter().any(|n| *n == s) {
        return true;
    }
    extra_dirs.iter().any(|d| d == s.as_ref())
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

    /// [`Config::is_noise`] for a path a directory walk of the project root produced `depth`
    /// names down. The walk built the path by joining those names onto the root, so they are its
    /// last `depth` components: reading just those answers the same question without comparing the
    /// whole root prefix, which was more than half of what a coverage walk spent per file.
    fn is_noise_below_root(&self, path: &Path, depth: usize) -> bool {
        path.components().rev().take(depth).any(|component| {
            is_noise_component(component, self.ignore.use_builtin_dirs, &self.ignore.dirs)
        })
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
            include_untracked: true,
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
/// Returns the per-extension counts, how many indexable sources the same walk saw, and whether the
/// budget cut the walk short. The counts alone were quoted as fact in prose while being capped, and
/// the "mostly another language" warning compared a capped number against the *uncapped* index total
/// -- so on a large repo the warning could never fire.
pub fn unsupported_source_counts(
    config: &Config,
    budget: usize,
) -> (BTreeMap<String, usize>, usize, bool) {
    /// Extensions worth naming. Anything else is not obviously project source.
    const KNOWN_SOURCE: &[&str] = &[
        // Single-file component formats come first because they are the dangerous ones: they *contain*
        // TypeScript and import TS symbols, so leaving them out let a Vue workspace answer
        // `total: 0` for a function called from every component -- with `authoritative_zero: true`
        // and `orphans` naming it dead code. The rest are languages that never reference TS symbols,
        // where the warning only says "the graph covers part of this repo".
        "vue", "svelte", "astro", //
        "rs", "py", "go", "java", "kt", "rb", "php", "cs", "swift", "c", "cc", "cpp", "h", "hpp",
        "scala", "ex", "exs", "dart", "lua", "zig",
    ];
    let effective = effective_extensions(config);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut supported_seen = 0usize;
    let mut truncated = false;
    let mut seen = 0usize;
    for entry in source_walk(config).flatten() {
        seen += 1;
        if seen > budget {
            truncated = true;
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let depth = entry.depth();
        let path = entry.into_path();
        if config.is_noise_below_root(&path, depth) {
            continue;
        }
        if let Some(extension) = path.extension().and_then(|value| value.to_str())
            && KNOWN_SOURCE.contains(&extension)
        {
            *counts.entry(extension.to_owned()).or_default() += 1;
        }
        // Counted in the same bounded pass, so the comparison that drives the warning is between two
        // numbers gathered under the same cap.
        if ext_matches(&path, &effective) {
            supported_seen += 1;
        }
    }
    (counts, supported_seen, truncated)
}

/// The walk behind `ravel index` and the coverage probes: hidden files included, links not
/// followed, noise directories pruned rather than descended into, and exactly the ignore rules
/// [`IgnoreChain`] applies to a single path:
///
/// - `.gitignore` files, inside a repository only, from the path's directory up to the top of the
///   repository -- including those above the root when the root is a package inside it -- with the
///   deeper file winning;
/// - that repository's `.git/info/exclude`, below every `.gitignore`;
/// - the root `.ravelignore`, below both.
///
/// `.ignore` files, global gitignore and `.ravelignore` files below the root are not read. A
/// directory these rules exclude hides everything in it, as in git.
fn source_walk(config: &Config) -> ignore::Walk {
    let root = &config.project.root;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .ignore(false)
        .parents(true)
        .git_ignore(config.ignore.gitignore)
        .git_global(false)
        .git_exclude(config.ignore.gitignore)
        .follow_links(false);
    let custom = root.join(".ravelignore");
    if custom.is_file() {
        // `add_ignore` anchors the file's patterns at the walk's current directory, the process's
        // own unless told otherwise. Anchored at the root they mean what they say: `src/gen/`
        // matched nothing, while sync and the watchers, which anchor at the root, excluded it.
        if root.is_absolute() {
            builder.current_dir(root);
        }
        builder.add_ignore(custom);
    }
    // Pruning a noise directory, instead of filtering every file under it, keeps the walk out of
    // `node_modules` altogether -- and away from whatever in there it cannot read.
    let use_builtin = config.ignore.use_builtin_dirs;
    let extra_dirs = config.ignore.dirs.clone();
    builder.filter_entry(move |entry| {
        entry.depth() == 0
            || !is_noise_component(
                std::path::Component::Normal(entry.file_name()),
                use_builtin,
                &extra_dirs,
            )
    });
    builder.build()
}

/// How many [`COMPONENT_SOURCE_EXTENSIONS`] files [`unsupported_source_counts`] counts, from the same
/// walk and the same budget but without classifying everything else.
///
/// An empty relation answer is only unreliable when such files exist, and that is all the
/// answer-health check reads. The full probe decides for every file whether it sits under a noise
/// directory -- by far the dearest step of a walk that otherwise only reads names -- to count
/// extensions nobody asked about. Here the check runs for the few files that could change the count.
pub fn component_source_count(config: &Config, budget: usize) -> usize {
    let mut count = 0usize;
    let mut seen = 0usize;
    for entry in source_walk(config).flatten() {
        seen += 1;
        if seen > budget {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| COMPONENT_SOURCE_EXTENSIONS.contains(&extension))
            && !config.is_noise(path)
        {
            count += 1;
        }
    }
    count
}

/// Gitignore rules live at every level, not just the workspace root: `apps/web/.gitignore` holding
/// `dist/` is what excludes `apps/web/dist`, and the index walk honours it. Consulting only the root
/// file made this check pass exactly the paths a monorepo means to exclude.
///
/// Deliberately pure pattern matching rather than asking the walk whether it would collect the file:
/// a *deleted* path is gone from the filesystem, and the watcher still has to process its removal.
///
/// It applies exactly the rules of the index walk ([`source_walk`]), with the walk's precedence:
/// `.gitignore` files inside a repository up to its top (above the root too), the repository's
/// `info/exclude` below them, the root `.ravelignore` below both, and a directory those rules
/// exclude hiding everything under it. Any rule one side honours and the other does not makes the
/// index depend on whether `ravel index` or `ravel sync` ran last.
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
    /// The directories above the root whose rules the walk consults, nearest first: every ancestor
    /// up to and including the top of the repository holding the root. Empty when the root is the
    /// top of its repository, or in none. Fixed when the chain is built.
    ancestors: Vec<PathBuf>,
    per_directory: std::sync::Mutex<BTreeMap<PathBuf, std::sync::Arc<DirectoryRules>>>,
}

/// One directory's ignore rules, read once and cached until [`IgnoreChain::forget_rules`].
struct DirectoryRules {
    gitignore: ignore::gitignore::Gitignore,
    /// `info/exclude` of the repository whose top this directory is.
    exclude: ignore::gitignore::Gitignore,
    /// The top of a repository (holds `.git` or `.jj`): rules above it never apply below it.
    is_repository: bool,
    /// The root's `.ravelignore`; empty in every other directory.
    ravelignore: ignore::gitignore::Gitignore,
}

/// What the walk takes for the top of a repository.
fn is_repository_top(directory: &Path) -> bool {
    directory.join(".git").exists() || directory.join(".jj").exists()
}

/// The directory holding `info/exclude` for the repository whose `.git` is `marker`: `.git`
/// itself, or -- for a linked worktree, whose `.git` is a file -- the common directory it points
/// to. A submodule's `.git` file names no common directory, and the walk reads no exclude for it.
fn git_common_dir(marker: &Path) -> Option<PathBuf> {
    if marker.is_dir() {
        return Some(marker.to_path_buf());
    }
    let pointer = fs::read_to_string(marker).ok()?;
    let git_dir = PathBuf::from(pointer.lines().next()?.strip_prefix("gitdir: ")?);
    let git_dir = marker.parent()?.join(git_dir);
    let common = fs::read_to_string(git_dir.join("commondir")).ok()?;
    Some(git_dir.join(common.lines().next()?))
}

impl IgnoreChain {
    pub fn new(config: &Config) -> Self {
        let root = config
            .project
            .root
            .canonicalize()
            .unwrap_or_else(|_| config.project.root.clone());
        let gitignore_enabled = config.ignore.gitignore;
        // The walk reads the `.gitignore` of every directory above the root up to the top of the
        // repository, and none outside a repository.
        let mut ancestors = Vec::new();
        if gitignore_enabled && !is_repository_top(&root) {
            for ancestor in root.ancestors().skip(1) {
                ancestors.push(ancestor.to_path_buf());
                if is_repository_top(ancestor) {
                    break;
                }
            }
            if !ancestors.last().is_some_and(|top| is_repository_top(top)) {
                ancestors.clear();
            }
        }
        Self {
            root,
            root_as_given: config.project.root.clone(),
            gitignore_enabled,
            ancestors,
            per_directory: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    fn rules_for(&self, directory: &Path) -> std::sync::Arc<DirectoryRules> {
        if let Some(cached) = self
            .per_directory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(directory)
        {
            return cached.clone();
        }
        let read = |file: Option<PathBuf>| {
            let mut builder = ignore::gitignore::GitignoreBuilder::new(directory);
            if let Some(file) = file.filter(|file| file.is_file()) {
                // Like the walk, keep the lines that parse when others in the file do not.
                let _ = builder.add(file);
            }
            builder
                .build()
                .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty())
        };
        let marker = directory.join(".git");
        let has_git = self.gitignore_enabled && marker.exists();
        let rules = std::sync::Arc::new(DirectoryRules {
            gitignore: read(self.gitignore_enabled.then(|| directory.join(".gitignore"))),
            exclude: read(
                has_git
                    .then(|| git_common_dir(&marker))
                    .flatten()
                    .map(|common| common.join("info/exclude")),
            ),
            is_repository: self.gitignore_enabled && (has_git || directory.join(".jj").exists()),
            ravelignore: read((directory == self.root).then(|| directory.join(".ravelignore"))),
        });
        self.per_directory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(directory.to_path_buf(), rules.clone());
        rules
    }

    /// Drop every cached rule file, so the next check reads them again. A long-lived chain (a
    /// watcher's) calls this when a `.gitignore`, `.ravelignore` or `info/exclude` changes;
    /// otherwise it keeps answering with the rules it first read.
    pub fn forget_rules(&self) {
        self.per_directory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// The walk's verdict on `path` given the rules of its own directory and every directory above
    /// it, innermost first.
    fn excludes<'a>(
        &self,
        path: &Path,
        is_dir: bool,
        applicable: impl Iterator<Item = &'a std::sync::Arc<DirectoryRules>> + Clone,
        root: &DirectoryRules,
    ) -> bool {
        let mut gitignore = ignore::Match::None;
        let mut exclude = ignore::Match::None;
        // Gitignore rules count only inside a repository, deepest first, up to its top.
        let in_repository =
            !self.ancestors.is_empty() || applicable.clone().any(|rules| rules.is_repository);
        if self.gitignore_enabled && in_repository {
            for rules in applicable {
                if gitignore.is_none() {
                    gitignore = rules.gitignore.matched(path, is_dir);
                }
                if exclude.is_none() {
                    exclude = rules.exclude.matched(path, is_dir);
                }
                if rules.is_repository {
                    break;
                }
            }
        }
        gitignore
            .or(exclude)
            .or(root.ravelignore.matched(path, is_dir))
            .is_ignore()
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
        let names: Vec<_> = relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        let Some((_, parents)) = names.split_last() else {
            return false;
        };
        // Every path below is re-spelled onto the canonical root: the matchers are built from
        // canonical directories, and a path outside a matcher's root matches nothing there.
        // The rules of the root and of each directory down to the path's own, outermost first.
        let mut directory = self.root.clone();
        let mut inside = Vec::with_capacity(names.len());
        inside.push(self.rules_for(&directory));
        for name in parents {
            directory.push(name);
            inside.push(self.rules_for(&directory));
        }
        let above: Vec<_> = self
            .ancestors
            .iter()
            .map(|directory| self.rules_for(directory))
            .collect();
        // Top down, the way the walk visits: a directory it prunes hides everything below it, even
        // a file a deeper rule re-includes. At each level the deepest rule wins.
        let mut candidate = self.root.clone();
        for (depth, name) in names.iter().enumerate() {
            candidate.push(name);
            let is_dir = depth + 1 < names.len();
            let applicable = inside[..=depth].iter().rev().chain(&above);
            if self.excludes(&candidate, is_dir, applicable, &inside[0]) {
                return true;
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
    // Compute the eligible extension set ONCE, not per file (was a fresh Vec<String> per path).
    let exts = effective_extensions(config);
    let mut files = Vec::new();
    let mut skipped = 0usize;
    let mut first_skipped = None;
    for entry in source_walk(config) {
        let entry = match entry {
            Ok(entry) => entry,
            // The root itself unreadable or missing would index nothing, and an empty answer is
            // not a smaller one: that stays an error.
            Err(source) if source.depth() == Some(0) => {
                return Err(ConfigError::Read {
                    path: root.clone(),
                    source: std::io::Error::other(source.to_string()),
                });
            }
            // Anything deeper -- a directory owned by another user, a malformed ignore file -- costs
            // only what it holds. Failing the whole walk over it made `ravel index` unusable.
            Err(source) => {
                skipped += 1;
                first_skipped.get_or_insert(source);
                continue;
            }
        };
        // Noise directories were pruned by the walk; only the extension is left to check.
        if entry.file_type().is_some_and(|kind| kind.is_file()) && ext_matches(entry.path(), &exts)
        {
            files.push(entry.into_path());
        }
    }
    if let Some(first) = first_skipped {
        eprintln!(
            "ravel: skipped {skipped} unreadable path(s) under {} while discovering sources; first: {first}",
            root.display()
        );
    }
    files.sort();
    Ok(files)
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

    /// A tree with every kind of noise: built-in names at the top, nested and as file stems, a
    /// custom directory, hidden directories, and names that merely contain a noise name.
    fn noisy_tree(root: &Path) {
        for relative in [
            "src/ok.ts",
            "src/deep/er/still.ts",
            "src/build/inside_build.ts",
            "node_modules/pkg/index.js",
            "dist/a.js",
            "build/b.ts",
            ".git/HEAD",
            ".git/objects/ab/cdef",
            ".ravel/CURRENT",
            "packages/p/node_modules/q/x.ts",
            "packages/p/src/tmp/t.ts",
            "packages/p/src/tmpl/t.ts",
            "packages/p/src/distance.ts",
            "packages/p/vendor_like/v.ts",
            "generated/g.ts",
            "src/generated/h.ts",
            "src/lib.rs",
            "src/view.vue",
            "node_modules/pkg/skip.rs",
            "generated/skip.py",
            ".hidden/seen.py",
            "target/debug/skip.rs",
        ] {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x").unwrap();
        }
    }

    /// The coverage walk decides "noise" from the names below the root instead of stripping the
    /// root off every path; the two must agree on every entry the walk can produce.
    #[test]
    fn the_names_below_the_root_decide_noise_like_the_whole_path_does() {
        let dir = tempdir().unwrap();
        noisy_tree(dir.path());
        // A trailing separator must not change any answer.
        let spellings = [
            dir.path().to_path_buf(),
            PathBuf::from(format!("{}/", dir.path().display())),
        ];
        for root in spellings {
            for (builtin, extra) in [
                (true, vec![]),
                (false, vec![]),
                (true, vec!["generated".to_owned()]),
                (false, vec!["generated".to_owned(), "src".to_owned()]),
            ] {
                let mut config = Config::default();
                config.project.root = root.clone();
                config.ignore.use_builtin_dirs = builtin;
                config.ignore.dirs = extra.clone();
                let mut visited = 0;
                let mut walker = ignore::WalkBuilder::new(&root);
                walker.hidden(false).git_ignore(false).follow_links(false);
                for entry in walker.build().flatten() {
                    visited += 1;
                    assert_eq!(
                        config.is_noise(entry.path()),
                        config.is_noise_below_root(entry.path(), entry.depth()),
                        "{} (depth {}) builtin={builtin} extra={extra:?}",
                        entry.path().display(),
                        entry.depth(),
                    );
                }
                assert!(
                    visited > 30,
                    "the walk must actually see the tree: {visited}"
                );
            }
        }
    }

    #[test]
    fn coverage_counts_skip_noise_directories_and_count_the_rest() {
        let dir = tempdir().unwrap();
        noisy_tree(dir.path());
        let mut config = Config::default();
        config.project.root = dir.path().to_path_buf();
        config.ignore.dirs = vec!["generated".to_owned()];
        let (counts, supported_seen, truncated) = unsupported_source_counts(&config, 20_000);
        // `.rs`: only src/lib.rs (node_modules, target and generated are noise); `.vue`: one.
        // `.py`: only .hidden/seen.py (generated/skip.py is under a custom noise directory).
        assert_eq!(counts.get("rs"), Some(&1), "{counts:?}");
        assert_eq!(counts.get("vue"), Some(&1), "{counts:?}");
        assert_eq!(counts.get("py"), Some(&1), "{counts:?}");
        // Indexable sources outside noise: src/ok.ts, src/deep/er/still.ts, src/build is noise,
        // packages/p/src/tmpl/t.ts, packages/p/src/distance.ts, packages/p/vendor_like/v.ts,
        // src/generated is noise.
        assert_eq!(supported_seen, 5, "{counts:?}");
        assert!(!truncated);
        // A budget smaller than the tree stops the walk and says so.
        let (_, _, cut) = unsupported_source_counts(&config, 5);
        assert!(cut);
    }

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

    /// The answer-health check reads only the component-format counts of the coverage probe, and now
    /// asks for just those. The number must stay what the full probe reports, including where
    /// the walk's budget cuts it short and for the files that are not counted (noise directories,
    /// other extensions, extension case).
    #[test]
    fn component_source_count_equals_the_full_probes_component_counts() {
        let dir = tempdir().unwrap();
        for (path, body) in [
            ("src/a.ts", "export {}"),
            ("src/b.vue", "<template/>"),
            ("src/c.svelte", "<script/>"),
            ("src/deep/er/d.astro", "---"),
            ("pages/e.astro", "---"),
            ("docs/f.VUE", "<template/>"),
            ("lib/g.py", "pass"),
            ("node_modules/pkg/h.vue", "<template/>"),
            ("dist/i.astro", "---"),
            (".hidden/j.vue", "<template/>"),
            ("ignored/k.vue", "<template/>"),
            ("l.svelte", "<script/>"),
        ] {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, body).unwrap();
        }
        fs::write(dir.path().join(".ravelignore"), "ignored/\n").unwrap();
        let mut config = Config::default();
        config.project.root = dir.path().to_path_buf();
        for budget in [0, 1, 2, 3, 5, 8, 13, 100] {
            let (counts, _, _) = unsupported_source_counts(&config, budget);
            let expected: usize = COMPONENT_SOURCE_EXTENSIONS
                .iter()
                .filter_map(|extension| counts.get(*extension))
                .sum();
            assert_eq!(
                component_source_count(&config, budget),
                expected,
                "budget {budget}"
            );
        }
        // Not vacuous: the files that count are seen with room to spare.
        assert_eq!(component_source_count(&config, 100), 6);
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

    fn write_tree(root: &Path, files: &[(&str, &str)]) {
        for (relative, text) in files {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    }

    #[test]
    fn one_unreadable_entry_does_not_fail_the_whole_discovery() {
        // A single entry the walk cannot read -- a cache directory owned by another user, a
        // malformed ignore file above the root -- used to fail `ravel index` outright.
        let outer = tempdir().unwrap();
        // An invalid glob in an ancestor's `.gitignore` is an error the walk reports up front (an
        // unclosed `[` is not: gitignore reads it literally).
        write_tree(
            outer.path(),
            &[(".gitignore", "[z-a]\n"), ("ws/src/a.ts", "export {}")],
        );
        let root = outer.path().join("ws");
        #[cfg(unix)]
        let locked = {
            use std::os::unix::fs::PermissionsExt;
            // Unreadable to anyone but root; root reads it anyway, so this half only bites as a
            // regular user, which is where the bug was seen.
            let locked = root.join("src/locked");
            write_tree(&root, &[("src/locked/b.ts", "export {}")]);
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            locked
        };
        let mut config = Config::default();
        config.project.root = root.clone();
        let discovered = discover_files(&config);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let discovered = discovered.expect("an unreadable entry is skipped, not fatal");
        assert!(
            discovered.iter().any(|path| path.ends_with("src/a.ts")),
            "{discovered:?}"
        );

        // The root itself unreadable is still an error: an empty answer is not a smaller one.
        config.project.root = outer.path().join("missing");
        assert!(discover_files(&config).is_err());
    }

    #[test]
    fn the_walk_prunes_noise_directories_instead_of_descending_into_them() {
        let dir = tempdir().unwrap();
        noisy_tree(dir.path());
        let mut config = Config::default();
        config.project.root = dir.path().to_path_buf();
        config.ignore.dirs = vec!["generated".to_owned()];
        let visited: Vec<_> = source_walk(&config)
            .flatten()
            .map(|entry| entry.into_path())
            .collect();
        assert!(
            visited.iter().any(|path| path.ends_with("src/ok.ts")),
            "{visited:?}"
        );
        for path in &visited {
            assert!(
                !config.is_noise(path),
                "the walk entered a noise directory: {}",
                path.display()
            );
        }
    }

    #[test]
    fn the_index_walk_and_the_single_path_chain_apply_the_same_rules() {
        // `ravel sync` and the watchers ask the chain about one path; `ravel index` walks. Every
        // rule one honours and the other does not makes the index depend on which ran last.
        let repo = tempdir().unwrap();
        let root = repo.path().join("packages/web");
        write_tree(
            repo.path(),
            &[
                // Above the root, inside the repository: the walk honours both.
                (".gitignore", "packages/web/generated/\n"),
                (".git/info/exclude", "excluded.ts\n"),
            ],
        );
        write_tree(
            &root,
            &[
                // Only the root `.ravelignore` counts, below the gitignore rules.
                (
                    ".ravelignore",
                    "vendored/\n!vendored/reincluded.ts\nprecedence.ts\nskipped.ts\n",
                ),
                (".gitignore", "!src/precedence.ts\n"),
                // `.ignore` files and nested `.ravelignore` files are not ignore rules here.
                (".ignore", "dot-ignore.ts\n"),
                ("src/nested/.ravelignore", "local.ts\n"),
                ("src/ok.ts", "export {}"),
                ("src/skipped.ts", "export {}"),
                ("src/excluded.ts", "export {}"),
                ("src/dot-ignore.ts", "export {}"),
                ("src/nested/local.ts", "export {}"),
                ("src/precedence.ts", "export {}"),
                ("generated/gen.ts", "export {}"),
                ("vendored/reincluded.ts", "export {}"),
            ],
        );
        let mut config = Config::default();
        config.project.root = root.clone();
        let discovered: std::collections::BTreeSet<_> = discover_files(&config)
            .unwrap()
            .into_iter()
            .map(|path| path.strip_prefix(&root).unwrap().to_path_buf())
            .collect();
        let chain = IgnoreChain::new(&config);
        let expected_kept = [
            "src/ok.ts",
            "src/dot-ignore.ts",
            "src/nested/local.ts",
            "src/precedence.ts",
        ];
        let expected_dropped = [
            "src/skipped.ts",
            "src/excluded.ts",
            "generated/gen.ts",
            // A pruned directory hides a file re-included inside it, as in git.
            "vendored/reincluded.ts",
        ];
        for relative in expected_kept.iter().chain(&expected_dropped) {
            let kept = expected_kept.contains(relative);
            assert_eq!(
                discovered.contains(Path::new(relative)),
                kept,
                "the walk on {relative}: {discovered:?}"
            );
            assert_eq!(
                chain.is_ignored(&root.join(relative)),
                !kept,
                "the chain on {relative}"
            );
        }
    }

    #[test]
    fn an_anchored_ravelignore_pattern_means_the_same_to_the_walk_and_the_chain() {
        // Patterns are relative to the root whatever directory the process runs in. The walk
        // anchored them at the working directory, so `src/gen/` excluded nothing from the index
        // while sync and the watchers excluded it.
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_tree(
            &root,
            &[
                (".ravelignore", "src/gen/\n/top.ts\n"),
                ("src/gen/out.ts", "export {}"),
                ("src/top.ts", "export {}"),
                ("gen/kept.ts", "export {}"),
                ("top.ts", "export {}"),
            ],
        );
        let mut config = Config::default();
        config.project.root = root.clone();
        let discovered: Vec<_> = discover_files(&config)
            .unwrap()
            .into_iter()
            .map(|path| path.strip_prefix(&root).unwrap().to_path_buf())
            .collect();
        assert_eq!(
            discovered,
            [Path::new("gen/kept.ts"), Path::new("src/top.ts")]
        );
        let chain = IgnoreChain::new(&config);
        for (relative, ignored) in [
            ("src/gen/out.ts", true),
            ("top.ts", true),
            ("gen/kept.ts", false),
            ("src/top.ts", false),
        ] {
            assert_eq!(
                chain.is_ignored(&root.join(relative)),
                ignored,
                "{relative}"
            );
        }
    }

    #[test]
    fn a_long_lived_chain_sees_edited_rules_once_told_to_forget_them() {
        let dir = tempdir().unwrap();
        write_tree(dir.path(), &[(".git/HEAD", ""), ("src/a.ts", "export {}")]);
        let mut config = Config::default();
        config.project.root = dir.path().to_path_buf();
        let chain = IgnoreChain::new(&config);
        let source = dir.path().join("src/a.ts");
        assert!(!chain.is_ignored(&source));
        fs::write(dir.path().join(".gitignore"), "src/\n").unwrap();
        fs::write(dir.path().join(".ravelignore"), "other/\n").unwrap();
        chain.forget_rules();
        assert!(chain.is_ignored(&source));
    }
}
