//! Architecture boundaries (T018) — optional `ravel.boundaries.toml`.
//! Missing file ⇒ no findings (not an error).

use crate::{
    graph::GraphIndex,
    model::IndexSnapshot,
    policy::{PolicyFinding, Suppressions},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct BoundariesConfig {
    pub layers: Vec<LayerRule>,
    pub coupling: CouplingConfig,
    pub cross_package: CrossPackageConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LayerRule {
    pub name: String,
    /// Glob-like substring match on package path (simple `*` suffix/prefix).
    pub packages: Vec<String>,
    pub allow_deps: Vec<String>,
    pub forbidden_deps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CouplingConfig {
    pub max_dependencies_per_module: Option<usize>,
    pub max_incoming_edges: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CrossPackageConfig {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

pub fn boundaries_path(root: &Path) -> PathBuf {
    let preferred = root.join("ravel.boundaries.toml");
    if preferred.is_file() {
        return preferred;
    }
    root.join("boundaries.toml")
}

/// Load boundaries file if present. Ok(None) when missing (not an error).
pub fn load_boundaries(root: &Path) -> Result<Option<BoundariesConfig>, String> {
    let path = boundaries_path(root);
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let cfg: BoundariesConfig =
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(cfg))
}

pub fn evaluate_boundaries(
    root: &Path,
    snapshot: &IndexSnapshot,
    graph: &GraphIndex,
    suppressions: &Suppressions,
) -> Result<Vec<PolicyFinding>, String> {
    let Some(cfg) = load_boundaries(root)? else {
        return Ok(Vec::new());
    };
    let mut findings = Vec::new();
    // Compute package→package edges ONCE (each call clones + sorts + dedups the whole Vec);
    // the body iterated it five times.
    let package_edges = graph.package_edges();

    // Map package → layer name
    let mut package_layer: BTreeMap<String, String> = BTreeMap::new();
    for pkg in graph.package_order() {
        if let Some(layer) = layer_for_package(&cfg.layers, &pkg) {
            package_layer.insert(pkg, layer);
        }
    }
    // Also packages that only appear as edge endpoints
    for (a, b) in &package_edges {
        package_layer
            .entry(a.clone())
            .or_insert_with(|| layer_for_package(&cfg.layers, a).unwrap_or_default());
        package_layer
            .entry(b.clone())
            .or_insert_with(|| layer_for_package(&cfg.layers, b).unwrap_or_default());
    }

    // Layer allow/forbidden deps
    for (from_pkg, to_pkg) in &package_edges {
        let from_layer = package_layer.get(from_pkg).cloned().unwrap_or_default();
        let to_layer = package_layer.get(to_pkg).cloned().unwrap_or_default();
        if from_layer.is_empty() || to_layer.is_empty() {
            continue;
        }
        if let Some(rule) = cfg.layers.iter().find(|l| l.name == from_layer) {
            if !rule.forbidden_deps.is_empty() && rule.forbidden_deps.iter().any(|d| d == &to_layer)
            {
                findings.push(PolicyFinding {
                    code: "layer_bypass".into(),
                    from: from_pkg.clone(),
                    to: to_pkg.clone(),
                    message: format!(
                        "layer `{from_layer}` must not depend on `{to_layer}` (forbidden_deps)"
                    ),
                });
            }
            if !rule.allow_deps.is_empty()
                && from_layer != to_layer
                && !rule.allow_deps.iter().any(|d| d == &to_layer)
            {
                findings.push(PolicyFinding {
                    code: "layer_bypass".into(),
                    from: from_pkg.clone(),
                    to: to_pkg.clone(),
                    message: format!(
                        "layer `{from_layer}` may only depend on {:?}, not `{to_layer}`",
                        rule.allow_deps
                    ),
                });
            }
        }
    }

    // Cross-package deny globs
    for (from_pkg, to_pkg) in &package_edges {
        for deny in &cfg.cross_package.deny {
            if glob_match(deny, to_pkg) || glob_match(deny, from_pkg) {
                // deny targets packages matching pattern as import target
                if glob_match(deny, to_pkg) {
                    let allowed = cfg
                        .cross_package
                        .allow
                        .iter()
                        .any(|a| glob_match(a, from_pkg) || glob_match(a, to_pkg));
                    if !allowed {
                        findings.push(PolicyFinding {
                            code: "cross_package_deny".into(),
                            from: from_pkg.clone(),
                            to: to_pkg.clone(),
                            message: format!("import of denied package pattern `{deny}`"),
                        });
                    }
                }
            }
        }
    }

    // Coupling ceilings on package graph degrees
    if let Some(max_out) = cfg.coupling.max_dependencies_per_module {
        let mut out_deg: BTreeMap<String, usize> = BTreeMap::new();
        for (from, _) in package_edges.iter().cloned() {
            *out_deg.entry(from).or_default() += 1;
        }
        for (pkg, deg) in out_deg {
            if deg > max_out {
                findings.push(PolicyFinding {
                    code: "coupling_ceiling".into(),
                    from: pkg.clone(),
                    to: format!("out_degree={deg}"),
                    message: format!("package has {deg} outgoing package deps (max {max_out})"),
                });
            }
        }
    }
    if let Some(max_in) = cfg.coupling.max_incoming_edges {
        let mut in_deg: BTreeMap<String, usize> = BTreeMap::new();
        for (_, to) in package_edges.iter().cloned() {
            *in_deg.entry(to).or_default() += 1;
        }
        for (pkg, deg) in in_deg {
            if deg > max_in {
                findings.push(PolicyFinding {
                    code: "coupling_ceiling".into(),
                    from: pkg.clone(),
                    to: format!("in_degree={deg}"),
                    message: format!("package has {deg} incoming package deps (max {max_in})"),
                });
            }
        }
    }

    // Optional: also count symbol-level coupling if package graph empty but snapshot huge
    let _ = snapshot;

    Ok(findings
        .into_iter()
        .filter(|f| !suppressions.keys.contains(&Suppressions::key(f)))
        .collect())
}

fn layer_for_package(layers: &[LayerRule], pkg: &str) -> Option<String> {
    for layer in layers {
        for pat in &layer.packages {
            if glob_match(pat, pkg) {
                return Some(layer.name.clone());
            }
        }
    }
    None
}

/// Minimal glob: `*` only as prefix/suffix/anywhere substring via split.
fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.is_empty() {
        return true;
    }
    let mut rest = value;
    if !parts[0].is_empty() {
        if let Some(idx) = rest.find(parts[0]) {
            if pattern.starts_with('*') {
                rest = &rest[idx + parts[0].len()..];
            } else if idx != 0 {
                return false;
            } else {
                rest = &rest[parts[0].len()..];
            }
        } else {
            return false;
        }
    }
    for (i, part) in parts.iter().enumerate().skip(1) {
        if part.is_empty() {
            continue;
        }
        if i == parts.len() - 1 && !pattern.ends_with('*') {
            return rest.ends_with(part);
        }
        if let Some(idx) = rest.find(part) {
            rest = &rest[idx + part.len()..];
        } else {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphIndex;
    use crate::model::{Edge, EdgeConfidence, EdgeKind, IndexSnapshot, SnapshotId};
    use std::collections::BTreeMap;

    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            kind: EdgeKind::Import,
            confidence: EdgeConfidence::Resolved {
                score: 1.0,
                reason: "t".into(),
            },
            type_only: false,
        }
    }

    #[test]
    fn layer_forbidden_dep_detected() {
        let snap = IndexSnapshot {
            id: SnapshotId {
                root: "r".into(),
                worktree: "w".into(),
                revision: "v".into(),
                content_state: "c".into(),
                schema_version: 1,
                grammar_version: "g".into(),
                config_hash: "h".into(),
            },
            files: BTreeMap::new(),
            // package_from_path uses apps/<pkg>/
            edges: vec![edge(
                "apps/service-a/src/x.ts",
                "apps/controller-b/src/y.ts",
            )],
        };
        let g = GraphIndex::from_snapshot(&snap);
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("ravel.boundaries.toml"),
            r#"
[[layers]]
name = "service"
packages = ["service-a"]
forbidden_deps = ["controller"]

[[layers]]
name = "controller"
packages = ["controller-b"]
"#,
        )
        .unwrap();
        let findings =
            evaluate_boundaries(dir.path(), &snap, &g, &Suppressions::default()).unwrap();
        assert!(
            findings.iter().any(|f| f.code == "layer_bypass"),
            "{findings:?}"
        );
    }

    #[test]
    fn missing_file_is_ok_empty() {
        let dir = tempfile::tempdir().unwrap();
        let snap = IndexSnapshot {
            id: SnapshotId {
                root: "r".into(),
                worktree: "w".into(),
                revision: "v".into(),
                content_state: "c".into(),
                schema_version: 1,
                grammar_version: "g".into(),
                config_hash: "h".into(),
            },
            files: BTreeMap::new(),
            edges: vec![],
        };
        let g = GraphIndex::from_snapshot(&snap);
        let findings =
            evaluate_boundaries(dir.path(), &snap, &g, &Suppressions::default()).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn literal_pattern_is_exact_not_substring() {
        assert!(glob_match("api", "api"));
        assert!(!glob_match("api", "rapidapi"));
        assert!(glob_match("*api", "rapidapi"));
    }

    #[test]
    fn glob_match_basic() {
        assert!(glob_match("legacy/*", "legacy/foo"));
        assert!(glob_match("*legacy*", "apps/legacy-api"));
        assert!(!glob_match("legacy/*", "apps/api"));
    }
}
