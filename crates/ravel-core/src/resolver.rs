use crate::model::{Edge, EdgeConfidence, EdgeKind, FileArtifact};
use rustc_hash::FxHashSet;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

static MATCHED_FILE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("matched workspace file"));
static NO_CANDIDATE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("no workspace candidate"));
static STALE_CANDIDATE: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::from("multiple or stale workspace candidates"));
static UNIQUE_SYMBOL: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("unique workspace symbol"));
static AMBIGUOUS_SYMBOL: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::from("ambiguous workspace symbol"));

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ResolverConfig {
    pub base_url: Option<PathBuf>,
    pub paths: BTreeMap<String, Vec<String>>,
    pub extensions: Vec<String>,
    pub max_candidates: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Resolution {
    pub specifier: String,
    pub target: Option<String>,
    pub candidates: Vec<String>,
    pub confidence: String,
    pub reason: Arc<str>,
}

/// Canonical invalidation keys emitted by the same resolver path that chooses an import target.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ResolutionTrace {
    pub importer: String,
    pub specifier: String,
    pub attempted_paths: BTreeSet<String>,
    pub basename_keys: BTreeSet<String>,
}

struct ResolutionCore {
    target: Option<String>,
    candidates: Vec<String>,
    confidence: &'static str,
    reason: Arc<str>,
    attempted_paths: BTreeSet<String>,
    basename_keys: BTreeSet<String>,
}

impl ResolutionCore {
    fn diagnostic(&self, specifier: &str) -> Resolution {
        Resolution {
            specifier: specifier.to_owned(),
            target: self.target.clone(),
            candidates: self.candidates.clone(),
            confidence: self.confidence.to_owned(),
            reason: Arc::clone(&self.reason),
        }
    }

    fn trace(&self, importer: &str, specifier: &str) -> ResolutionTrace {
        ResolutionTrace {
            importer: importer.to_owned(),
            specifier: specifier.to_owned(),
            attempted_paths: self.attempted_paths.clone(),
            basename_keys: self.basename_keys.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ReverseIndex {
    pub dependents: BTreeMap<String, BTreeSet<String>>,
}

/// Minimal workspace-wide state required to resolve a small artifact subset exactly.
/// It is deliberately independent of `FileArtifact`, so generations can persist and shard it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ResolutionUniverse {
    pub format_version: u32,
    pub resolver_fingerprint: String,
    pub files: BTreeSet<String>,
    pub by_basename: BTreeMap<String, Vec<String>>,
    pub symbol_definers: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ResolutionUniverseOverlay {
    pub files: BTreeMap<String, bool>,
    pub by_basename: BTreeMap<String, Option<Vec<String>>>,
    pub symbol_definers: BTreeMap<String, Option<u32>>,
}

impl ResolutionUniverse {
    pub const FORMAT_VERSION: u32 = 1;

    pub fn build(artifacts: &BTreeMap<String, FileArtifact>, config: &ResolverConfig) -> Self {
        let mut universe = Self {
            format_version: Self::FORMAT_VERSION,
            resolver_fingerprint: resolver_fingerprint(config),
            ..Self::default()
        };
        for artifact in artifacts.values() {
            universe.files.insert(artifact.path.clone());
            let stem = Path::new(&artifact.path)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_owned();
            universe
                .by_basename
                .entry(stem)
                .or_default()
                .push(artifact.path.clone());
            for symbol in &artifact.symbols {
                *universe
                    .symbol_definers
                    .entry(symbol.name.clone())
                    .or_default() += 1;
            }
        }
        universe
    }

    pub fn matches(&self, config: &ResolverConfig) -> bool {
        self.format_version == Self::FORMAT_VERSION
            && self.resolver_fingerprint == resolver_fingerprint(config)
    }

    pub fn replace_artifact(&mut self, old: Option<&FileArtifact>, new: Option<&FileArtifact>) {
        if let Some(old) = old {
            self.files.remove(&old.path);
            let stem = artifact_stem(&old.path);
            if let Some(paths) = self.by_basename.get_mut(&stem) {
                paths.retain(|path| path != &old.path);
                if paths.is_empty() {
                    self.by_basename.remove(&stem);
                }
            }
            for symbol in &old.symbols {
                decrement_count(&mut self.symbol_definers, &symbol.name);
            }
        }
        if let Some(new) = new {
            self.files.insert(new.path.clone());
            let paths = self
                .by_basename
                .entry(artifact_stem(&new.path))
                .or_default();
            if !paths.contains(&new.path) {
                paths.push(new.path.clone());
                paths.sort();
            }
            for symbol in &new.symbols {
                *self.symbol_definers.entry(symbol.name.clone()).or_default() += 1;
            }
        }
    }

    pub fn replace_artifact_with_overlay(
        &mut self,
        old: Option<&FileArtifact>,
        new: Option<&FileArtifact>,
        overlay: &mut ResolutionUniverseOverlay,
    ) {
        let paths: BTreeSet<String> = old
            .into_iter()
            .map(|artifact| artifact.path.clone())
            .chain(new.into_iter().map(|artifact| artifact.path.clone()))
            .collect();
        let basenames: BTreeSet<String> = paths.iter().map(|path| artifact_stem(path)).collect();
        let symbols: BTreeSet<String> = old
            .into_iter()
            .flat_map(|artifact| artifact.symbols.iter().map(|symbol| symbol.name.clone()))
            .chain(
                new.into_iter()
                    .flat_map(|artifact| artifact.symbols.iter().map(|symbol| symbol.name.clone())),
            )
            .collect();
        self.replace_artifact(old, new);
        for path in paths {
            overlay
                .files
                .insert(path.clone(), self.files.contains(&path));
        }
        for basename in basenames {
            overlay
                .by_basename
                .insert(basename.clone(), self.by_basename.get(&basename).cloned());
        }
        for symbol in symbols {
            overlay
                .symbol_definers
                .insert(symbol.clone(), self.symbol_definers.get(&symbol).copied());
        }
    }

    pub fn apply_overlay(&mut self, overlay: &ResolutionUniverseOverlay) {
        for (path, present) in &overlay.files {
            if *present {
                self.files.insert(path.clone());
            } else {
                self.files.remove(path);
            }
        }
        apply_optional_map(&mut self.by_basename, &overlay.by_basename);
        apply_optional_map(&mut self.symbol_definers, &overlay.symbol_definers);
    }
}

fn apply_optional_map<V: Clone>(
    target: &mut BTreeMap<String, V>,
    overlay: &BTreeMap<String, Option<V>>,
) {
    for (key, value) in overlay {
        if let Some(value) = value {
            target.insert(key.clone(), value.clone());
        } else {
            target.remove(key);
        }
    }
}

fn artifact_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_owned()
}

fn decrement_count(counts: &mut BTreeMap<String, u32>, key: &str) {
    let Some(count) = counts.get_mut(key) else {
        return;
    };
    *count -= 1;
    if *count == 0 {
        counts.remove(key);
    }
}

impl ReverseIndex {
    pub fn affected_by(&self, changed: &str) -> Vec<String> {
        self.dependents
            .get(changed)
            .map(|values| values.iter().cloned().collect())
            .unwrap_or_default()
    }
    pub fn rebuild(&mut self, edges: &[Edge]) {
        self.dependents.clear();
        for edge in edges {
            self.dependents
                .entry(edge.to.clone())
                .or_default()
                .insert(edge.from.clone());
        }
    }
}

pub fn resolve_artifacts(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
) -> (Vec<Edge>, Vec<Resolution>, ReverseIndex) {
    let (edges, resolutions, reverse, _, _) =
        resolve_artifacts_impl(root, artifacts, config, true, false, false, None);
    (edges, resolutions, reverse)
}

/// Production indexing path: return only the graph edges. Diagnostic resolutions and the
/// reverse string index are intentionally skipped because the engine never consumes them.
pub fn resolve_edges(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
) -> Vec<Edge> {
    resolve_artifacts_impl(root, artifacts, config, false, false, false, None).0
}

/// Resolve graph edges and return the exact path/basename probes used for incremental
/// invalidation. This is the structural-index build path; ordinary full indexing can skip traces.
pub fn resolve_edges_with_traces(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
) -> (Vec<Edge>, Vec<ResolutionTrace>) {
    let (edges, _, _, traces, _) =
        resolve_artifacts_impl(root, artifacts, config, false, true, false, None);
    (edges, traces)
}

pub fn resolve_edges_with_contributions(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
) -> (Vec<Edge>, BTreeMap<String, Vec<Edge>>) {
    let (edges, _, _, _, contributions) =
        resolve_artifacts_impl(root, artifacts, config, false, false, true, None);
    (edges, contributions)
}

pub type StructuralResolutionData = (
    Vec<Edge>,
    Vec<ResolutionTrace>,
    EdgeContributions,
    ResolutionUniverse,
);

pub fn resolve_edges_with_structural_data(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
) -> StructuralResolutionData {
    let universe = ResolutionUniverse::build(artifacts, config);
    let (edges, _, _, traces, contributions) =
        resolve_artifacts_impl(root, artifacts, config, false, true, true, Some(&universe));
    (edges, traces, contributions, universe)
}

pub type EdgeContributions = BTreeMap<String, Vec<Edge>>;
pub type SubsetResolution = Option<(Vec<Edge>, EdgeContributions)>;

/// Resolve only the supplied artifacts against persisted workspace-wide membership/counts.
/// A fingerprint mismatch is explicit so callers can fall back to full resolution.
pub fn resolve_subset_with_contributions(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    universe: &ResolutionUniverse,
    config: &ResolverConfig,
) -> SubsetResolution {
    if !universe.matches(config) {
        return None;
    }
    let (edges, _, _, _, contributions) =
        resolve_artifacts_impl(root, artifacts, config, false, false, true, Some(universe));
    Some((edges, contributions))
}

pub fn resolve_subset_with_structural_data(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    universe: &ResolutionUniverse,
    config: &ResolverConfig,
) -> Option<(Vec<ResolutionTrace>, EdgeContributions)> {
    if !universe.matches(config) {
        return None;
    }
    let (_, _, _, traces, contributions) =
        resolve_artifacts_impl(root, artifacts, config, false, true, true, Some(universe));
    Some((traces, contributions))
}

pub fn resolver_fingerprint(config: &ResolverConfig) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"ravel-resolver-v2\0");
    if let Ok(bytes) = bincode::serialize(config) {
        hasher.update(&bytes);
    }
    hasher.finalize().to_hex().to_string()
}

type ResolveArtifactsOutput = (
    Vec<Edge>,
    Vec<Resolution>,
    ReverseIndex,
    Vec<ResolutionTrace>,
    BTreeMap<String, Vec<Edge>>,
);

fn resolve_artifacts_impl(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
    collect_auxiliary: bool,
    collect_traces: bool,
    collect_contributions: bool,
    persisted_universe: Option<&ResolutionUniverse>,
) -> ResolveArtifactsOutput {
    let built_universe;
    let universe = if let Some(universe) = persisted_universe {
        universe
    } else {
        built_universe = ResolutionUniverse::build(artifacts, config);
        &built_universe
    };
    let mut edges = Vec::new();
    let mut resolutions = Vec::new();
    let mut traces = Vec::new();
    let mut contributions: BTreeMap<String, Vec<Edge>> = BTreeMap::new();
    for artifact in artifacts.values() {
        for import in &artifact.imports {
            let resolution = resolve_one(root, &artifact.path, &import.specifier, universe, config);
            let (confidence, target) = match resolution.target.clone() {
                Some(target) => (
                    EdgeConfidence::Resolved {
                        score: 1.0,
                        reason: resolution.reason.clone(),
                    },
                    Some(target),
                ),
                None if !resolution.candidates.is_empty() => (
                    EdgeConfidence::Candidate {
                        score: 0.5,
                        reason: resolution.reason.clone(),
                    },
                    None,
                ),
                None => (
                    EdgeConfidence::Unresolved {
                        score: 0.0,
                        reason: resolution.reason.clone(),
                    },
                    None,
                ),
            };
            let edge = Edge {
                from: artifact.path.clone(),
                to: target.unwrap_or_else(|| import.specifier.clone()),
                kind: EdgeKind::Import,
                confidence,
                type_only: import.type_only,
            };
            if collect_contributions {
                contributions
                    .entry(artifact.path.clone())
                    .or_default()
                    .push(edge.clone());
            }
            edges.push(edge);
            if collect_auxiliary {
                resolutions.push(resolution.diagnostic(&import.specifier));
            }
            if collect_traces {
                traces.push(resolution.trace(&artifact.path, &import.specifier));
            }
        }
        for export in &artifact.exports {
            if let Some(specifier) = &export.specifier {
                let resolution = resolve_one(root, &artifact.path, specifier, universe, config);
                let (confidence, target) = match resolution.target.clone() {
                    Some(target) => (
                        EdgeConfidence::Resolved {
                            score: 1.0,
                            reason: resolution.reason.clone(),
                        },
                        target,
                    ),
                    None if !resolution.candidates.is_empty() => (
                        EdgeConfidence::Candidate {
                            score: 0.5,
                            reason: resolution.reason.clone(),
                        },
                        specifier.clone(),
                    ),
                    None => (
                        EdgeConfidence::Unresolved {
                            score: 0.0,
                            reason: resolution.reason.clone(),
                        },
                        specifier.clone(),
                    ),
                };
                let edge = Edge {
                    from: artifact.path.clone(),
                    to: target,
                    kind: EdgeKind::ReExport,
                    confidence,
                    type_only: export.type_only,
                };
                if collect_contributions {
                    contributions
                        .entry(artifact.path.clone())
                        .or_default()
                        .push(edge.clone());
                }
                edges.push(edge);
                if collect_auxiliary {
                    resolutions.push(resolution.diagnostic(specifier));
                }
                if collect_traces {
                    traces.push(resolution.trace(&artifact.path, specifier));
                }
            }
        }
    }
    // Symbol-level edges (calls / extends / implements). Nodes are symbol names — the same
    // names the agent already searches for, so `callers_of`/`impact` work with no extra plumbing.
    // Only emit edges to names DEFINED in the workspace (external/builtin targets are dropped),
    // and tag confidence by definition uniqueness — the type-less resolver can't disambiguate
    // overloads/same-name methods, so ambiguous targets are `Candidate`, not `Resolved`.
    let mut seen: FxHashSet<(&str, &str, EdgeKind)> = FxHashSet::default();
    for artifact in artifacts.values() {
        for r in &artifact.symbol_refs {
            if r.from == r.to {
                continue; // self-reference
            }
            let Some(&count) = universe.symbol_definers.get(r.to.as_str()) else {
                continue; // target not defined in workspace → skip
            };
            let confidence = if count == 1 {
                EdgeConfidence::Resolved {
                    score: 1.0,
                    reason: Arc::clone(&UNIQUE_SYMBOL),
                }
            } else {
                EdgeConfidence::Candidate {
                    score: 0.5,
                    reason: Arc::clone(&AMBIGUOUS_SYMBOL),
                }
            };
            let edge = Edge {
                from: r.from.clone(),
                to: r.to.clone(),
                kind: r.kind.clone(),
                confidence,
                type_only: false,
            };
            if collect_contributions {
                contributions
                    .entry(artifact.path.clone())
                    .or_default()
                    .push(edge.clone());
            }
            if seen.insert((r.from.as_str(), r.to.as_str(), r.kind.clone())) {
                edges.push(edge);
            }
        }
    }

    edges.sort_by(|a, b| (&a.from, &a.to, &a.kind).cmp(&(&b.from, &b.to, &b.kind)));
    let mut reverse = ReverseIndex::default();
    if collect_auxiliary {
        reverse.rebuild(&edges);
    }
    for owned in contributions.values_mut() {
        owned.sort_by(|a, b| (&a.from, &a.to, &a.kind).cmp(&(&b.from, &b.to, &b.kind)));
    }
    (edges, resolutions, reverse, traces, contributions)
}

fn resolve_one(
    root: &Path,
    importer: &str,
    specifier: &str,
    universe: &ResolutionUniverse,
    config: &ResolverConfig,
) -> ResolutionCore {
    let importer_path = Path::new(importer);
    let mut candidates = Vec::new();
    let mut attempted_paths = BTreeSet::new();
    let mut basename_keys = BTreeSet::new();
    if specifier.starts_with('.') {
        let base = root
            .join(importer_path)
            .parent()
            .unwrap_or(root)
            .join(specifier);
        let probe = file_candidates(root, &base, config);
        candidates.extend(probe.existing);
        attempted_paths.extend(probe.attempted);
    } else if let Some(base) = &config.base_url {
        let probe = file_candidates(root, &root.join(base).join(specifier), config);
        candidates.extend(probe.existing);
        attempted_paths.extend(probe.attempted);
    }
    if candidates.is_empty() {
        for (alias, targets) in &config.paths {
            // `strip_suffix('*')` yields the alias prefix once (was `trim_end_matches` twice).
            if let Some(prefix) = alias.strip_suffix('*') {
                if let Some(suffix) = specifier.strip_prefix(prefix) {
                    for target in targets {
                        let probe =
                            file_candidates(root, &root.join(target.replace('*', suffix)), config);
                        candidates.extend(probe.existing);
                        attempted_paths.extend(probe.attempted);
                    }
                }
            }
        }
    }
    if candidates.is_empty() {
        basename_keys.insert(specifier.to_owned());
        if let Some(paths) = universe.by_basename.get(specifier) {
            candidates.extend(paths.iter().cloned());
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates.truncate(config.max_candidates.max(1));
    let target = candidates
        .first()
        .filter(|candidate| universe.files.contains(candidate.as_str()))
        .cloned();
    let (confidence, reason) = if target.is_some() {
        ("resolved", Arc::clone(&MATCHED_FILE))
    } else if candidates.is_empty() {
        ("unresolved", Arc::clone(&NO_CANDIDATE))
    } else {
        ("candidate", Arc::clone(&STALE_CANDIDATE))
    };
    ResolutionCore {
        target,
        candidates,
        confidence,
        reason,
        attempted_paths,
        basename_keys,
    }
}

const DEFAULT_RESOLVE_EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

struct CandidateProbe {
    existing: Vec<String>,
    attempted: BTreeSet<String>,
}

fn file_candidates(root: &Path, base: &Path, config: &ResolverConfig) -> CandidateProbe {
    let mut existing = Vec::new();
    let mut attempted = BTreeSet::new();
    attempted.insert(normalize(root, base));
    if base.is_file() {
        existing.push(normalize(root, base));
    }
    // Iterate config extensions by reference; fall back to a static default set — no per-call
    // `Vec<String>` clone/allocation.
    let probe = |ext: &str, existing: &mut Vec<String>, attempted: &mut BTreeSet<String>| {
        let path = base.with_extension(ext);
        attempted.insert(normalize(root, &path));
        if path.is_file() {
            existing.push(normalize(root, &path));
        }
    };
    if config.extensions.is_empty() {
        for &ext in DEFAULT_RESOLVE_EXTS {
            probe(ext, &mut existing, &mut attempted);
        }
    } else {
        for ext in &config.extensions {
            probe(ext, &mut existing, &mut attempted);
        }
    }
    for extension in ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"] {
        let path = base.join(format!("index.{extension}"));
        attempted.insert(normalize(root, &path));
        if path.is_file() {
            existing.push(normalize(root, &path));
        }
    }
    CandidateProbe {
        existing,
        attempted,
    }
}
fn normalize(root: &Path, path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // Scanner artifacts use root-relative paths. Keep resolver candidates in that same
    // namespace; absolute candidates made every relative import look unresolved in a real
    // workspace even though the file existed on disk.
    let relative = canonical.strip_prefix(root).unwrap_or(&canonical);
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(value) => normalized.push(value),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
        }
    }
    let s = normalized.to_string_lossy();
    // Only pay the replace scan/alloc when a backslash is actually present (never on Unix).
    if s.contains('\\') {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

pub fn load_tsconfig(root: &Path) -> ResolverConfig {
    let path = root.join("tsconfig.json");
    let Ok(text) = fs::read_to_string(path) else {
        return ResolverConfig::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return ResolverConfig::default();
    };
    let options = value.get("compilerOptions").cloned().unwrap_or_default();
    let base_url = options
        .get("baseUrl")
        .and_then(|v| v.as_str())
        .map(PathBuf::from);
    let paths = options
        .get("paths")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    ResolverConfig {
        base_url,
        paths,
        ..ResolverConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::parse_source;
    use tempfile::tempdir;
    #[test]
    fn resolves_relative_import_and_keeps_unresolved_visible() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("src")).unwrap();
        fs::write(
            root.path().join("src/a.ts"),
            "import { B } from './b'; import X from 'missing';",
        )
        .unwrap();
        fs::write(root.path().join("src/b.ts"), "export class B {}").unwrap();
        let a = parse_source(
            "src/a.ts",
            b"import { B } from './b'; import X from 'missing';",
        );
        let b = parse_source("src/b.ts", b"export class B {}");
        let map: BTreeMap<String, FileArtifact> = [(a.path.clone(), a), (b.path.clone(), b)].into();
        let (edges, _, reverse) = resolve_artifacts(root.path(), &map, &ResolverConfig::default());
        assert_eq!(
            edges,
            resolve_edges(root.path(), &map, &ResolverConfig::default())
        );
        assert!(edges.iter().any(|edge| edge.to.ends_with("src/b.ts")));
        assert!(
            edges
                .iter()
                .any(|edge| matches!(edge.confidence, EdgeConfidence::Unresolved { .. }))
        );
        assert!(!reverse.affected_by("src/b.ts").is_empty());
    }

    #[test]
    fn resolves_typescript_module_extension() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/a.ts"), "import { B } from './b';").unwrap();
        fs::write(root.path().join("src/b.mts"), "export class B {}").unwrap();
        let a = parse_source("src/a.ts", b"import { B } from './b';");
        let b = parse_source("src/b.mts", b"export class B {}");
        let map: BTreeMap<String, FileArtifact> = [(a.path.clone(), a), (b.path.clone(), b)].into();
        let edges = resolve_edges(root.path(), &map, &ResolverConfig::default());
        assert!(edges.iter().any(|edge| edge.to.ends_with("src/b.mts")));
    }

    #[test]
    fn resolves_scanner_style_relative_paths() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/a.ts"), "import { B } from './b';").unwrap();
        fs::write(root.path().join("src/b.ts"), "export class B {}").unwrap();
        let a = parse_source("src/a.ts", b"import { B } from './b';");
        let b = parse_source("src/b.ts", b"export class B {}");
        let map: BTreeMap<String, FileArtifact> = [(a.path.clone(), a), (b.path.clone(), b)].into();
        let (edges, _, reverse) = resolve_artifacts(root.path(), &map, &ResolverConfig::default());
        assert!(edges.iter().any(|edge| {
            edge.from == "src/a.ts"
                && edge.to == "src/b.ts"
                && matches!(edge.confidence, EdgeConfidence::Resolved { .. })
        }));
        assert_eq!(reverse.affected_by("src/b.ts"), vec!["src/a.ts"]);
    }

    #[test]
    #[ignore = "performance probe"]
    fn persisted_universe_21k_subset_benchmark() {
        let root = tempdir().unwrap();
        let config = ResolverConfig::default();
        let artifacts: BTreeMap<String, FileArtifact> = (0..21_000)
            .map(|index| {
                let path = format!("src/f{index}.ts");
                let source = format!("export function S{index}() {{}}");
                (path.clone(), parse_source(&path, source.as_bytes()))
            })
            .collect();
        let universe = ResolutionUniverse::build(&artifacts, &config);
        let subset = BTreeMap::from([(
            "src/changed.ts".to_owned(),
            parse_source("src/changed.ts", b"export function changed() { S42(); }"),
        )]);
        let started = std::time::Instant::now();
        for _ in 0..100 {
            std::hint::black_box(resolve_subset_with_contributions(
                root.path(),
                &subset,
                &universe,
                &config,
            ));
        }
        eprintln!(
            "21k persisted-universe subset mean_us={}",
            started.elapsed().as_micros() / 100
        );
    }
}
