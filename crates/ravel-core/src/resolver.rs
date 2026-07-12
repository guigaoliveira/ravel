use crate::model::{Edge, EdgeConfidence, EdgeKind, FileArtifact};
use rustc_hash::{FxHashMap, FxHashSet};
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

struct ResolutionCore {
    target: Option<String>,
    candidates: Vec<String>,
    confidence: &'static str,
    reason: Arc<str>,
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
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ReverseIndex {
    pub dependents: BTreeMap<String, BTreeSet<String>>,
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
    resolve_artifacts_impl(root, artifacts, config, true)
}

/// Production indexing path: return only the graph edges. Diagnostic resolutions and the
/// reverse string index are intentionally skipped because the engine never consumes them.
pub fn resolve_edges(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
) -> Vec<Edge> {
    resolve_artifacts_impl(root, artifacts, config, false).0
}

fn resolve_artifacts_impl(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
    collect_auxiliary: bool,
) -> (Vec<Edge>, Vec<Resolution>, ReverseIndex) {
    let mut by_basename: FxHashMap<&str, Vec<&str>> = FxHashMap::default();
    for artifact in artifacts.values() {
        let stem = Path::new(&artifact.path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        by_basename
            .entry(stem)
            .or_default()
            .push(artifact.path.as_str());
    }
    let mut edges = Vec::new();
    let mut resolutions = Vec::new();
    for artifact in artifacts.values() {
        for import in &artifact.imports {
            let resolution = resolve_one(
                root,
                &artifact.path,
                &import.specifier,
                artifacts,
                &by_basename,
                config,
            );
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
            edges.push(Edge {
                from: artifact.path.clone(),
                to: target.unwrap_or_else(|| import.specifier.clone()),
                kind: EdgeKind::Import,
                confidence,
                type_only: import.type_only,
            });
            if collect_auxiliary {
                resolutions.push(resolution.diagnostic(&import.specifier));
            }
        }
        for export in &artifact.exports {
            if let Some(specifier) = &export.specifier {
                let resolution = resolve_one(
                    root,
                    &artifact.path,
                    specifier,
                    artifacts,
                    &by_basename,
                    config,
                );
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
                edges.push(Edge {
                    from: artifact.path.clone(),
                    to: target,
                    kind: EdgeKind::ReExport,
                    confidence,
                    type_only: export.type_only,
                });
                if collect_auxiliary {
                    resolutions.push(resolution.diagnostic(specifier));
                }
            }
        }
    }
    // Symbol-level edges (calls / extends / implements). Nodes are symbol names — the same
    // names the agent already searches for, so `callers_of`/`impact` work with no extra plumbing.
    // Only emit edges to names DEFINED in the workspace (external/builtin targets are dropped),
    // and tag confidence by definition uniqueness — the type-less resolver can't disambiguate
    // overloads/same-name methods, so ambiguous targets are `Candidate`, not `Resolved`.
    let mut symbol_defs: FxHashMap<&str, u32> = FxHashMap::default();
    for artifact in artifacts.values() {
        for sym in &artifact.symbols {
            *symbol_defs.entry(sym.name.as_str()).or_default() += 1;
        }
    }
    let mut seen: FxHashSet<(&str, &str, EdgeKind)> = FxHashSet::default();
    for artifact in artifacts.values() {
        for r in &artifact.symbol_refs {
            if r.from == r.to {
                continue; // self-reference
            }
            let Some(&count) = symbol_defs.get(r.to.as_str()) else {
                continue; // target not defined in workspace → skip
            };
            if !seen.insert((r.from.as_str(), r.to.as_str(), r.kind.clone())) {
                continue; // dedup repeated calls to the same target
            }
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
            edges.push(Edge {
                from: r.from.clone(),
                to: r.to.clone(),
                kind: r.kind.clone(),
                confidence,
                type_only: false,
            });
        }
    }

    edges.sort_by(|a, b| (&a.from, &a.to, &a.kind).cmp(&(&b.from, &b.to, &b.kind)));
    let mut reverse = ReverseIndex::default();
    if collect_auxiliary {
        reverse.rebuild(&edges);
    }
    (edges, resolutions, reverse)
}

fn resolve_one(
    root: &Path,
    importer: &str,
    specifier: &str,
    artifacts: &BTreeMap<String, FileArtifact>,
    by_basename: &FxHashMap<&str, Vec<&str>>,
    config: &ResolverConfig,
) -> ResolutionCore {
    let importer_path = Path::new(importer);
    let mut candidates = Vec::new();
    if specifier.starts_with('.') {
        let base = root
            .join(importer_path)
            .parent()
            .unwrap_or(root)
            .join(specifier);
        candidates.extend(file_candidates(root, &base, config));
    } else if let Some(base) = &config.base_url {
        candidates.extend(file_candidates(
            root,
            &root.join(base).join(specifier),
            config,
        ));
    }
    if candidates.is_empty() {
        for (alias, targets) in &config.paths {
            // `strip_suffix('*')` yields the alias prefix once (was `trim_end_matches` twice).
            if let Some(prefix) = alias.strip_suffix('*') {
                if let Some(suffix) = specifier.strip_prefix(prefix) {
                    for target in targets {
                        candidates.extend(file_candidates(
                            root,
                            &root.join(target.replace('*', suffix)),
                            config,
                        ));
                    }
                }
            }
        }
    }
    if candidates.is_empty() {
        if let Some(paths) = by_basename.get(specifier) {
            candidates.extend(paths.iter().map(|path| (*path).to_owned()));
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates.truncate(config.max_candidates.max(1));
    let target = candidates
        .first()
        .filter(|candidate| artifacts.contains_key(candidate.as_str()))
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
    }
}

const DEFAULT_RESOLVE_EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

fn file_candidates(root: &Path, base: &Path, config: &ResolverConfig) -> Vec<String> {
    let mut result = Vec::new();
    if base.is_file() {
        result.push(normalize(root, base));
    }
    // Iterate config extensions by reference; fall back to a static default set — no per-call
    // `Vec<String>` clone/allocation.
    let probe = |ext: &str, result: &mut Vec<String>| {
        let path = base.with_extension(ext);
        if path.is_file() {
            result.push(normalize(root, &path));
        }
    };
    if config.extensions.is_empty() {
        for &ext in DEFAULT_RESOLVE_EXTS {
            probe(ext, &mut result);
        }
    } else {
        for ext in &config.extensions {
            probe(ext, &mut result);
        }
    }
    for extension in ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"] {
        let path = base.join(format!("index.{extension}"));
        if path.is_file() {
            result.push(normalize(root, &path));
        }
    }
    result
}
fn normalize(root: &Path, path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // Scanner artifacts use root-relative paths. Keep resolver candidates in that same
    // namespace; absolute candidates made every relative import look unresolved in a real
    // workspace even though the file existed on disk.
    let relative = canonical.strip_prefix(root).unwrap_or(&canonical);
    let s = relative.to_string_lossy();
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
}
