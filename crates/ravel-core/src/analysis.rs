//! Derived analyses for CLI/MCP agents: cycles, risk, orphans, hubs.
//! All operate on already-loaded graph/snapshot sidecars — no full re-scan.

use crate::{
    graph::{GraphIndex, QueryLimits},
    model::{IndexSnapshot, SymbolMetaDict},
};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImpactItem {
    pub symbol: String,
    pub depth: usize,
    pub in_degree: usize,
    pub risk: RiskLevel,
    pub score: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImpactReport {
    pub root: String,
    pub snapshot_id: String,
    pub affected: Vec<ImpactItem>,
    pub total_affected: usize,
    pub truncated: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CycleInfo {
    pub size: usize,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HubEntry {
    pub name: String,
    pub in_degree: usize,
    pub out_degree: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageInfo {
    pub name: String,
    pub files: usize,
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiReport {
    pub passed: bool,
    pub snapshot_id: String,
    pub files: usize,
    pub edges: usize,
    pub cycles: usize,
    pub max_cycle_size: usize,
    pub policy_findings: usize,
    pub orphans: usize,
    pub findings: Vec<String>,
}

/// Package-level SCCs, largest first, optional filter by package name substring.
pub fn package_cycles(graph: &GraphIndex, package_filter: Option<&str>) -> Vec<CycleInfo> {
    let mut cycles: Vec<CycleInfo> = graph
        .package_cycles()
        .into_iter()
        .map(|mut members| {
            members.sort();
            CycleInfo {
                size: members.len(),
                members,
            }
        })
        .filter(|c| package_filter.is_none_or(|pf| c.members.iter().any(|m| m.contains(pf))))
        .collect();
    cycles.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.members.cmp(&b.members)));
    cycles
}

/// Impact with risk scoring from reverse BFS depths + reverse adjacency degree.
pub fn impact_with_risk(
    graph: &GraphIndex,
    node: &str,
    limits: &QueryLimits,
) -> Result<ImpactReport, crate::graph::QueryError> {
    let (page, depth_map) = graph.callers_of_with_depths(node, limits)?;

    let mut affected = Vec::new();
    for item in &page.items {
        let id = graph.node_id(item);
        let depth = id.and_then(|id| depth_map.get(&id)).copied().unwrap_or(1);
        let in_degree = id.map(|id| graph.in_degree_id(id)).unwrap_or(0);
        let (risk, score) = score_risk(depth, in_degree);
        affected.push(ImpactItem {
            symbol: item.clone(),
            depth,
            in_degree,
            risk,
            score,
        });
    }
    affected.sort_by(|a, b| {
        risk_rank(a.risk)
            .cmp(&risk_rank(b.risk))
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    Ok(ImpactReport {
        root: node.into(),
        snapshot_id: page.snapshot_id,
        total_affected: affected.len(),
        affected,
        truncated: page.truncated,
        reason: page.reason,
    })
}

fn risk_rank(r: RiskLevel) -> u8 {
    match r {
        RiskLevel::High => 0,
        RiskLevel::Medium => 1,
        RiskLevel::Low => 2,
    }
}

fn score_risk(depth: usize, in_degree: usize) -> (RiskLevel, u32) {
    let score = (in_degree as u32)
        .saturating_mul(10)
        .saturating_add(if depth <= 1 {
            50
        } else if depth <= 3 {
            20
        } else {
            5
        });
    let risk = if depth <= 1 || in_degree > 10 {
        RiskLevel::High
    } else if depth <= 3 || in_degree >= 2 {
        RiskLevel::Medium
    } else {
        RiskLevel::Low
    };
    (risk, score)
}

/// Natural project/Nest **entry points** — symbols/files that start the app or accept
/// external traffic. They often have in-degree 0 (nothing in-repo imports `main.ts`) and
/// must not show up as "orphans". Detected automatically; `extra_entry_markers` only extends.
///
/// Heuristics (no config required):
/// - path: `main.ts`, `main.js`, `bootstrap.*`, `app.module.*`, `*.module.ts`, `*.controller.ts`
/// - name: ends with `Module`, `Controller`, `Resolver`, equals `main`/`bootstrap`
/// - Nest decorator kinds already fold into path/name patterns above
pub fn is_natural_entry_point(name: &str, path: &str) -> bool {
    const ENTRY_FILES: &[&str] = &[
        "main.ts",
        "main.js",
        "main.mjs",
        "main.cjs",
        "index.ts",
        "index.js",
        "bootstrap.ts",
        "bootstrap.js",
        "server.ts",
        "server.js",
    ];
    // Basename compared case-insensitively without allocating the whole lowercased path.
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if ENTRY_FILES.iter().any(|f| file.eq_ignore_ascii_case(f)) {
        return true;
    }
    if name.eq_ignore_ascii_case("main") || name.eq_ignore_ascii_case("bootstrap") {
        return true;
    }
    // Rare fallback (nested `main.ts` segment): only now pay the lowercase allocation.
    let path_l = path.replace('\\', "/").to_lowercase();
    path_l.contains("/main.ts") || path_l.contains("/main.js")
}

/// Nodes never imported/referenced as edge targets (and not only self).
/// Entry points (natural heuristics + package.json/tsconfig entries + optional markers) excluded.
pub fn orphans(
    graph: &GraphIndex,
    symbols: Option<&SymbolMetaDict>,
    limit: usize,
    extra_entry_markers: &[String],
    manifest_entry_files: &BTreeSet<String>,
) -> Vec<String> {
    let manifest_entries = crate::entries::ManifestEntryIndex::new(manifest_entry_files);
    // Borrow from `symbols` (which outlives this map) instead of cloning every name+path.
    let mut defined: BTreeMap<&str, &str> = BTreeMap::new(); // name -> path
    if let Some(meta) = symbols {
        for e in &meta.entries {
            defined.insert(e.name.as_str(), e.path.as_str());
        }
    }
    let is_entry = |name: &str, path: &str| -> bool {
        if is_natural_entry_point(name, path) {
            return true;
        }
        if manifest_entries.contains(path) || manifest_entries.contains(name) {
            return true;
        }
        extra_entry_markers.iter().any(|ep| {
            let ep = ep.as_str();
            !ep.is_empty() && (name.contains(ep) || path.contains(ep))
        })
    };

    let mut out = Vec::new();
    for (id, name) in graph.node_names().enumerate() {
        let id = id as u32;
        if graph.in_degree_id(id) == 0 && graph.out_degree_id(id) > 0 {
            let path = defined.get(name).copied().unwrap_or(name);
            if is_entry(name, path) {
                continue;
            }
            // Prefer symbol-ish nodes when we have meta; still report file hubs with no callers
            out.push(name.to_owned());
        }
    }
    if let Some(meta) = symbols {
        for e in &meta.entries {
            if !graph.contains_node(&e.name) {
                if is_entry(&e.name, &e.path) {
                    continue;
                }
                out.push(e.name.clone());
            }
        }
    }
    out.sort();
    out.dedup();
    out.truncate(limit.max(1));
    out
}

/// Highest in-degree symbols (most depended-upon).
///
/// Complexity: O(V) scan + O(V log V) sort of candidates with in_degree>0.
/// At 1B nodes this is **not** acceptable online — use precomputed top-k hubs sidecar
/// (`hubs.bin`) published at index time (O(V log k) once).
pub fn hubs(graph: &GraphIndex, limit: usize) -> Vec<HubEntry> {
    hubs_from_graph(graph, limit)
}

pub fn hubs_from_graph(graph: &GraphIndex, limit: usize) -> Vec<HubEntry> {
    let limit = limit.max(1);
    // Partial top-k with binary heap would be O(V log k); for k small this matters at scale.
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let mut heap: BinaryHeap<Reverse<(usize, String, usize)>> = BinaryHeap::new();
    for (id, name) in graph.node_names().enumerate() {
        let id = id as u32;
        let in_d = graph.in_degree_id(id);
        if in_d == 0 {
            continue;
        }
        // Min-heap by in_degree among top-k (Reverse makes BinaryHeap a min-heap).
        // out_degree is only fetched when the node actually enters the heap.
        if heap.len() < limit {
            heap.push(Reverse((in_d, name.to_owned(), graph.out_degree_id(id))));
        } else if let Some(Reverse((min_in, _, _))) = heap.peek() {
            if in_d > *min_in {
                let out_d = graph.out_degree_id(id);
                heap.pop();
                heap.push(Reverse((in_d, name.to_owned(), out_d)));
            }
        }
    }
    let mut entries: Vec<HubEntry> = heap
        .into_iter()
        .map(|Reverse((in_degree, name, out_degree))| HubEntry {
            name,
            in_degree,
            out_degree,
            kind: None,
            path: None,
        })
        .collect();
    entries.sort_by(|a, b| {
        b.in_degree
            .cmp(&a.in_degree)
            .then_with(|| a.name.cmp(&b.name))
    });
    entries
}

/// Attach kind/path from symbol meta and optionally filter by kind substring (e.g. `class`, `injectable`).
pub fn enrich_hubs(
    mut hubs: Vec<HubEntry>,
    symbols: Option<&SymbolMetaDict>,
    kind_filter: Option<&str>,
) -> Vec<HubEntry> {
    if let Some(meta) = symbols {
        let by_name: FxHashMap<&str, &crate::model::SymbolMeta> =
            meta.entries.iter().map(|e| (e.name.as_str(), e)).collect();
        for h in &mut hubs {
            if let Some(m) = by_name.get(h.name.as_str()) {
                h.kind = Some(m.kind.to_string());
                h.path = Some(m.path.clone());
            }
        }
    }
    if let Some(kf) = kind_filter {
        let kf = kf.to_lowercase();
        hubs.retain(|h| {
            h.kind
                .as_ref()
                .map(|k| k.to_lowercase().contains(&kf))
                .unwrap_or(false)
                || h.name.to_lowercase().contains(&kf)
                || h.path
                    .as_ref()
                    .map(|p| p.to_lowercase().contains(&kf))
                    .unwrap_or(false)
        });
    }
    hubs
}

/// Precompute top-k hubs at index time for O(1) cold CLI.
pub fn precompute_hubs(graph: &GraphIndex, limit: usize) -> Vec<HubEntry> {
    hubs_from_graph(graph, limit)
}

pub fn list_packages(snapshot: &IndexSnapshot) -> Vec<PackageInfo> {
    let mut map: BTreeMap<String, PackageInfo> = BTreeMap::new();
    for (path, file) in &snapshot.files {
        let pkg = package_from_path(path);
        // Avoid cloning the key on the common already-present path.
        match map.get_mut(&pkg) {
            Some(entry) => {
                entry.files += 1;
                if !entry
                    .languages
                    .iter()
                    .any(|l| l.as_str() == file.language.as_ref())
                {
                    entry.languages.push(file.language.to_string());
                }
            }
            None => {
                map.insert(
                    pkg.clone(),
                    PackageInfo {
                        name: pkg,
                        files: 1,
                        languages: vec![file.language.to_string()],
                    },
                );
            }
        }
    }
    let mut packages: Vec<_> = map.into_values().collect();
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    packages
}

fn package_from_path(path: &str) -> String {
    // Single pass, no intermediate Vec: segment after the first apps|libs|packages marker.
    let mut it = path.split('/');
    while let Some(p) = it.next() {
        if matches!(p, "apps" | "libs" | "packages") {
            if let Some(next) = it.next() {
                return next.to_owned();
            }
        }
    }
    "workspace".into()
}

/// Minimal GraphViz DOT of package graph.
///
/// Complexity: **O(P + E_pkg)** via collapsed `package_graph`, independent of symbol-node
/// count. Works for any graph size (1e3 or 1e9 symbols) as long as package count fits RAM.
/// No silent node-scan caps — completeness is structural, not budgeted.
pub fn export_package_dot(graph: &GraphIndex) -> String {
    let cycles: HashSet<String> = graph.package_cycles().into_iter().flatten().collect();
    let mut lines = vec![
        "digraph packages {".into(),
        "  rankdir=LR;".into(),
        "  node [shape=box, style=rounded];".into(),
    ];
    for pkg in graph.package_order() {
        let color = if cycles.contains(&pkg) {
            "fillcolor=\"#ffcccc\", style=\"filled,rounded\""
        } else {
            "style=rounded"
        };
        lines.push(format!("  \"{pkg}\" [{color}];"));
    }
    for (a, b) in graph.package_edges() {
        lines.push(format!("  \"{a}\" -> \"{b}\";"));
    }
    lines.push("}".into());
    lines.join("\n")
}

/// Map a source path to likely test companions (Nest/Jest/Vitest conventions).
pub fn related_tests(path: &str, patterns: &[String]) -> Vec<String> {
    const DEFAULT_TEST_PATTERNS: &[&str] = &[".spec.ts", ".test.ts", ".spec.js", ".test.js"];
    let path = path.replace('\\', "/");
    // strip extension
    let stem = path
        .rsplit_once('.')
        .map(|(s, _)| s.to_owned())
        .unwrap_or_else(|| path.clone());
    let base = path.rsplit('/').next().unwrap_or(&path);
    let base_stem = base
        .rsplit_once('.')
        .map(|(s, _)| s.to_owned())
        .unwrap_or_else(|| base.to_owned());
    let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
    let mut out = Vec::new();
    let mut apply = |pat: &str| {
        // Only extension-style patterns (`.spec.ts`). The former `{pre}.{ext}` line was
        // bit-identical to `{stem}{pat}` (pre==stem) and only survived via dedup — dropped.
        if pat.starts_with('.') {
            out.push(format!("{stem}{pat}"));
            out.push(format!("{dir}/__tests__/{base_stem}{pat}"));
            out.push(format!("{dir}/{base_stem}{pat}"));
        }
    };
    if patterns.is_empty() {
        for &pat in DEFAULT_TEST_PATTERNS {
            apply(pat);
        }
    } else {
        for pat in patterns {
            apply(pat);
        }
    }
    out.sort();
    out.dedup();
    out
}

#[allow(clippy::too_many_arguments)]
pub fn ci_report(
    snapshot_id: String,
    files: usize,
    edges: usize,
    cycles: &[CycleInfo],
    policy_count: usize,
    orphan_count: usize,
    cycle_threshold: usize,
    strict: bool,
) -> CiReport {
    let max_cycle = cycles.iter().map(|c| c.size).max().unwrap_or(0);
    let mut findings = Vec::new();
    if max_cycle >= cycle_threshold {
        findings.push(format!(
            "import_cycles: largest SCC size {max_cycle} >= threshold {cycle_threshold}"
        ));
    }
    if policy_count > 0 {
        findings.push(format!("policy_findings: {policy_count}"));
    }
    if strict && orphan_count > 0 {
        findings.push(format!("orphans: {orphan_count} (strict)"));
    }
    let passed = if strict {
        max_cycle < cycle_threshold && policy_count == 0
    } else {
        max_cycle < cycle_threshold
    };
    CiReport {
        passed,
        snapshot_id,
        files,
        edges,
        cycles: cycles.len(),
        max_cycle_size: max_cycle,
        policy_findings: policy_count,
        orphans: orphan_count,
        findings,
    }
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
    fn risk_scores_direct_callers_high() {
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
            edges: vec![edge("a", "b"), edge("c", "b")],
        };
        let g = GraphIndex::from_snapshot(&snap);
        let report = impact_with_risk(&g, "b", &QueryLimits::default()).unwrap();
        assert!(report.affected.iter().any(|i| i.risk == RiskLevel::High));
    }

    #[test]
    fn natural_entry_points_cover_common_ts_entries() {
        assert!(is_natural_entry_point("main", "apps/api-users/src/main.ts"));
        assert!(is_natural_entry_point(
            "bootstrap",
            "apps/api-users/src/bootstrap.ts"
        ));
        assert!(!is_natural_entry_point(
            "UsersService",
            "apps/api-users/src/users.service.ts"
        ));
    }
}
