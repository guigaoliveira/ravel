//! Derived analyses for CLI/MCP agents: cycles, risk, orphans, hubs.
//! All operate on already-loaded graph/snapshot sidecars — no full re-scan.

use crate::{
    graph::{GraphIndex, QueryLimits},
    model::{IndexSnapshot, SchemaSummary, SymbolMetaDict},
};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
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
    /// Nodes reached by the traversal. When `exact` is false this is a lower
    /// bound (the walk hit a budget), not a real total.
    pub total_affected: usize,
    /// True when the traversal completed within budgets, so `total_affected`
    /// is the actual count rather than a saturated lower bound.
    pub exact: bool,
    /// True when `affected` omits reached nodes ranked after this page, or the
    /// traversal itself stopped at a budget.
    pub truncated: bool,
    /// The budget the traversal hit, else `page_size` when only the page was cut.
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyReport {
    /// Real number of findings; `findings` below is a bounded page of them.
    pub total: usize,
    /// Complete per-code counts — never truncated.
    pub by_code: BTreeMap<String, usize>,
    pub findings: Vec<crate::policy::PolicyFinding>,
    pub truncated: bool,
}

/// Bounded findings page + complete aggregate: keeps `validate` output usable
/// by agents on repos where the raw list runs to megabytes.
pub fn policy_report(findings: Vec<crate::policy::PolicyFinding>, limit: usize) -> PolicyReport {
    let total = findings.len();
    let mut by_code: BTreeMap<String, usize> = BTreeMap::new();
    for finding in &findings {
        *by_code.entry(finding.code.clone()).or_default() += 1;
    }
    let mut findings = findings;
    findings.truncate(limit);
    PolicyReport {
        total,
        by_code,
        truncated: total > findings.len(),
        findings,
    }
}

/// File-level SCCs, largest first, optional filter by path substring. Finer
/// than `package_cycles`, which collapses a monorepo into path buckets.
pub fn file_cycles(graph: &GraphIndex, path_filter: Option<&str>) -> Vec<CycleInfo> {
    let mut cycles: Vec<CycleInfo> = graph
        .file_cycles()
        .into_iter()
        .map(|mut members| {
            members.sort();
            CycleInfo {
                size: members.len(),
                members,
            }
        })
        .filter(|c| path_filter.is_none_or(|pf| c.members.iter().any(|m| m.contains(pf))))
        .collect();
    cycles.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.members.cmp(&b.members)));
    cycles
}

/// Impact with risk scoring from reverse BFS depths + reverse adjacency degree.
///
/// Every node the walk reached is scored, and the page (`limits.cursor`, `limits.page_size`) is
/// cut from that ranking. The walk pages by name, so ranking only its first page could leave the
/// direct dependents -- the highest risk -- out of the report entirely.
pub fn impact_with_risk(
    graph: &GraphIndex,
    node: &str,
    limits: &QueryLimits,
) -> Result<ImpactReport, crate::graph::QueryError> {
    // The depth map covers the whole walk; a name-ordered page of it would only be discarded.
    let walk_limits = QueryLimits {
        page_size: 0,
        cursor: 0,
        ..limits.clone()
    };
    let (walk, depth_map) = graph.callers_of_with_depths(node, &walk_limits)?;

    let mut ranked: Vec<(&str, usize, usize, RiskLevel, u32)> = depth_map
        .into_iter()
        // Depth 0 is the root itself.
        .filter(|&(_, depth)| depth > 0)
        .filter_map(|(id, depth)| {
            let in_degree = graph.in_degree_id(id);
            let (risk, score) = score_risk(depth, in_degree);
            Some((graph.node_name(id)?, depth, in_degree, risk, score))
        })
        .collect();
    // Names are unique, so the key is total and an unstable sort is deterministic.
    ranked.sort_unstable_by_key(|&(symbol, _, _, risk, score)| {
        (risk_rank(risk), Reverse(score), symbol)
    });
    let total_affected = ranked.len();
    let start = limits.cursor.min(total_affected);
    let end = start.saturating_add(limits.page_size).min(total_affected);
    let more = end < total_affected;
    let affected = ranked[start..end]
        .iter()
        .map(|&(symbol, depth, in_degree, risk, score)| ImpactItem {
            symbol: symbol.to_owned(),
            depth,
            in_degree,
            risk,
            score,
        })
        .collect();
    Ok(ImpactReport {
        root: node.into(),
        snapshot_id: walk.snapshot_id,
        total_affected,
        exact: !walk.truncated,
        affected,
        // `affected` leaves out ranked nodes after this page as surely as a budget-cut walk does.
        truncated: walk.truncated || more,
        reason: walk.reason.or_else(|| more.then(|| "page_size".into())),
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

/// Natural project **entry points** — symbols/files that start the app or accept
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
    // Borrow from `symbols` (which outlives this map) instead of cloning every id/name/path.
    let mut defined: BTreeMap<&str, (&str, &str)> = BTreeMap::new(); // id -> (name, path)
    if let Some(meta) = symbols {
        for e in meta.entries.iter().chain(meta.duplicates.iter()) {
            defined.insert(e.id.as_str(), (e.name.as_str(), e.path.as_str()));
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
    for (id, name) in graph.node_entries() {
        if graph.in_degree_id(id) == 0 && graph.out_degree_id(id) > 0 {
            let (display_name, path) = defined.get(name).copied().unwrap_or((name, name));
            if is_entry(display_name, path) {
                continue;
            }
            // Prefer symbol-ish nodes when we have meta; still report file hubs with no callers
            out.push(display_name.to_owned());
        }
    }
    if let Some(meta) = symbols {
        for e in meta.entries.iter().chain(meta.duplicates.iter()) {
            if !graph.contains_node(&e.id) {
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
    for (id, name) in graph.node_entries() {
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
    hubs: Vec<HubEntry>,
    symbols: Option<&SymbolMetaDict>,
    kind_filter: Option<&str>,
) -> Vec<HubEntry> {
    let by_id: Option<FxHashMap<&str, &crate::model::SymbolMeta>> = symbols.map(|meta| {
        meta.entries
            .iter()
            .chain(meta.duplicates.iter())
            .map(|e| (e.id.as_str(), e))
            .collect()
    });
    enrich_hubs_with(
        hubs,
        |id| by_id.as_ref()?.get(id).map(|meta| (*meta).clone()),
        kind_filter,
    )
}

/// [`enrich_hubs`] against a per-id lookup, so a caller holding an indexed symbol store can annotate
/// the hubs it returns without materializing metadata for every symbol in the workspace.
pub fn enrich_hubs_with(
    mut hubs: Vec<HubEntry>,
    mut lookup: impl FnMut(&str) -> Option<crate::model::SymbolMeta>,
    kind_filter: Option<&str>,
) -> Vec<HubEntry> {
    for h in &mut hubs {
        if let Some(m) = lookup(h.name.as_str()) {
            h.name = m.qualified_name;
            h.kind = Some(m.kind.to_string());
            h.path = Some(m.path);
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

pub fn schema_summary(snapshot: &IndexSnapshot) -> SchemaSummary {
    let mut node_kinds = BTreeMap::new();
    let mut edge_kinds = BTreeMap::new();
    for artifact in snapshot.files.values() {
        for symbol in &artifact.symbols {
            *node_kinds.entry(symbol.kind.to_string()).or_default() += 1;
        }
    }
    for edge in &snapshot.edges {
        *edge_kinds.entry(format!("{:?}", edge.kind)).or_default() += 1;
    }
    SchemaSummary {
        format_version: SchemaSummary::FORMAT_VERSION,
        snapshot_id: snapshot.id.stable_key(),
        files: snapshot.files.len(),
        edges: snapshot.edges.len(),
        packages: list_packages(snapshot).len(),
        node_kinds,
        edge_kinds,
    }
}

/// Package summary from the compact file-list sidecar. Language is fully determined by the
/// supported source extension, so this produces the same result without hydrating artifacts.
pub fn list_packages_from_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<PackageInfo> {
    let mut map: BTreeMap<String, PackageInfo> = BTreeMap::new();
    for path in paths {
        let package = package_from_path(path);
        let language = if [".js", ".jsx", ".mjs", ".cjs"]
            .iter()
            .any(|extension| path.ends_with(extension))
        {
            "javascript"
        } else {
            "typescript"
        };
        let entry = map.entry(package.clone()).or_insert_with(|| PackageInfo {
            name: package,
            files: 0,
            languages: Vec::new(),
        });
        entry.files += 1;
        if !entry.languages.iter().any(|existing| existing == language) {
            entry.languages.push(language.to_owned());
        }
    }
    map.into_values().collect()
}

fn package_from_path(path: &str) -> String {
    crate::graph::package_name(path)
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

/// Map a source path to likely test companions using common naming conventions.
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
            // src→test mirrors (NestJS/Jest monorepo layout): apps/X/src/**/f.ts
            // → apps/X/test/**/f.spec.ts (and tests/).
            if let Some((prefix, suffix)) = stem.split_once("/src/") {
                out.push(format!("{prefix}/test/{suffix}{pat}"));
                out.push(format!("{prefix}/tests/{suffix}{pat}"));
            } else if let Some(suffix) = stem.strip_prefix("src/") {
                out.push(format!("test/{suffix}{pat}"));
                out.push(format!("tests/{suffix}{pat}"));
            }
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
            source_path: None,
            span: None,
            provenance: crate::model::EdgeProvenance::Ast,
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
    fn impact_reports_saturated_count_as_inexact_lower_bound() {
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
            edges: vec![edge("a", "root"), edge("b", "root"), edge("c", "root")],
        };
        let graph = GraphIndex::from_snapshot(&snap);

        // Node budget smaller than the caller set: the count is a lower bound, not a total.
        let saturated = impact_with_risk(
            &graph,
            "root",
            &QueryLimits {
                nodes: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(saturated.truncated);
        assert!(!saturated.exact);

        // Full traversal within budget: the count is exact.
        let complete = impact_with_risk(&graph, "root", &QueryLimits::default()).unwrap();
        assert!(!complete.truncated);
        assert!(complete.exact);
        assert_eq!(complete.total_affected, 3);
    }

    #[test]
    fn impact_total_is_not_reduced_to_first_page_size() {
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
            edges: vec![edge("a", "root"), edge("b", "root")],
        };
        let graph = GraphIndex::from_snapshot(&snap);
        let report = impact_with_risk(
            &graph,
            "root",
            &QueryLimits {
                page_size: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.affected.len(), 1);
        assert_eq!(report.total_affected, 2);
    }

    /// target ← z ← a000..a104: the only direct dependent sorts after the 105 indirect ones by
    /// name. Paging by name before ranking left `z` out of the report while it claimed to be
    /// complete; the page has to be cut from the risk ranking of everything the walk reached.
    #[test]
    fn impact_page_is_cut_from_the_risk_ranking_of_every_reached_node() {
        let mut edges = vec![edge("z", "target")];
        edges.extend((0..105).map(|i| edge(&format!("a{i:03}"), "z")));
        let graph = GraphIndex::from_edges(&edges, "s".into());

        let report = impact_with_risk(&graph, "target", &QueryLimits::default()).unwrap();
        assert_eq!(report.affected.len(), 100);
        assert_eq!(report.affected[0].symbol, "z");
        assert_eq!(report.affected[0].risk, RiskLevel::High);
        assert_eq!(report.total_affected, 106);
        assert!(report.exact, "the walk itself was complete");
        assert!(report.truncated, "six reached nodes are not in `affected`");
        assert_eq!(report.reason.as_deref(), Some("page_size"));

        // The next page continues the same ranking and ends it.
        let rest = impact_with_risk(
            &graph,
            "target",
            &QueryLimits {
                cursor: 100,
                ..Default::default()
            },
        )
        .unwrap();
        let rest: Vec<_> = rest.affected.iter().map(|i| i.symbol.as_str()).collect();
        assert_eq!(rest, ["a099", "a100", "a101", "a102", "a103", "a104"]);

        let complete = impact_with_risk(
            &graph,
            "target",
            &QueryLimits {
                page_size: 200,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(complete.affected.len(), 106);
        assert!(!complete.truncated);
        assert_eq!(complete.reason, None);
    }

    #[test]
    fn policy_report_bounds_findings_but_keeps_complete_counts() {
        let finding = |code: &str, n: usize| crate::policy::PolicyFinding {
            code: code.into(),
            from: format!("from{n}.ts"),
            to: format!("to{n}.ts"),
            message: "m".into(),
        };
        let findings = vec![
            finding("dangling_edge", 0),
            finding("dangling_edge", 1),
            finding("dangling_edge", 2),
            finding("orphan_export", 3),
            finding("orphan_export", 4),
        ];
        let report = policy_report(findings, 2);
        assert_eq!(report.total, 5);
        assert_eq!(report.findings.len(), 2);
        assert!(report.truncated);
        assert_eq!(report.by_code.get("dangling_edge"), Some(&3));
        assert_eq!(report.by_code.get("orphan_export"), Some(&2));

        let complete = policy_report(Vec::new(), 2);
        assert_eq!(complete.total, 0);
        assert!(!complete.truncated);
    }

    #[test]
    fn related_tests_candidates_include_src_to_test_mirror() {
        // NestJS/Jest monorepo convention: apps/X/src/**/f.ts → apps/X/test/**/f.spec.ts
        let candidates = related_tests(
            "apps/permissions/src/application/usecases/get_all.usecase.ts",
            &[],
        );
        assert!(
            candidates.contains(
                &"apps/permissions/test/application/usecases/get_all.usecase.spec.ts".to_owned()
            ),
            "missing src→test mirror candidate, got {candidates:#?}"
        );
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

    #[test]
    fn package_summary_from_paths_preserves_counts_and_languages() {
        let packages = list_packages_from_paths([
            "apps/api/src/main.ts",
            "apps/api/src/legacy.js",
            "libs/core/src/index.ts",
        ]);
        assert_eq!(
            packages,
            vec![
                PackageInfo {
                    name: "api".into(),
                    files: 2,
                    languages: vec!["typescript".into(), "javascript".into()],
                },
                PackageInfo {
                    name: "core".into(),
                    files: 1,
                    languages: vec!["typescript".into()],
                },
            ]
        );
    }

    #[test]
    fn schema_summary_counts_kinds_once_at_publish_time() {
        let mut snapshot = IndexSnapshot {
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
            edges: vec![edge("a", "b")],
        };
        snapshot.files.insert(
            "apps/api/src/service.ts".into(),
            crate::scanner::parse_source("apps/api/src/service.ts", b"export class Service {}"),
        );
        let summary = schema_summary(&snapshot);
        assert_eq!(summary.files, 1);
        assert_eq!(summary.edges, 1);
        assert_eq!(summary.packages, 1);
        assert_eq!(summary.node_kinds["class_declaration"], 1);
        assert_eq!(summary.edge_kinds["Import"], 1);
    }
}
