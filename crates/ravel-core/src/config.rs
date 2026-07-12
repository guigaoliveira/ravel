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

/// Optional analysis knobs. Defaults are automatic monorepo heuristics — leave empty.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AnalysisConfig {
    /// Optional **extra** entry-point markers (merged with built-in Nest/monorepo heuristics).
    /// Leave empty: controllers, modules, main/bootstrap, package entry files are detected automatically.
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

/// Default product extensions when `languages = ["auto"]` (TS/JS monorepos).
/// Override with `parser.extensions = [...]` for any set you want.
pub const DEFAULT_SOURCE_EXTENSIONS: &[&str] =
    &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// Default sibling-emit rules (tsc/Nest leave `*.js` next to `*.ts`).
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
        return config
            .parser
            .extensions
            .iter()
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
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
            sibling_emit: Vec::new(),
        }
    }
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            home: PathBuf::from(".ravel"),
            retention: 3,
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
        Self { debounce_ms: 150 }
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
        if self.watch.debounce_ms > 60_000 {
            return Err(invalid(
                "watch.debounce_ms",
                &self.watch.debounce_ms.to_string(),
                "must not exceed 60000",
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
        assert_eq!(ext, vec!["vue".to_string(), "svelte".to_string()]);
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
}
