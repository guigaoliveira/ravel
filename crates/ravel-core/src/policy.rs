use crate::model::{EdgeConfidence, EdgeKind, IndexSnapshot};
use std::collections::BTreeSet;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PolicyFinding {
    pub code: String,
    pub from: String,
    pub to: String,
    pub message: String,
}
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Suppressions {
    pub keys: BTreeSet<String>,
}
impl Suppressions {
    pub fn key(finding: &PolicyFinding) -> String {
        format!("{}:{}:{}", finding.code, finding.from, finding.to)
    }
}
pub fn validate_snapshot(
    snapshot: &IndexSnapshot,
    suppressions: &Suppressions,
) -> Vec<PolicyFinding> {
    let mut findings = Vec::new();
    for edge in &snapshot.edges {
        // `snapshot.files` is already a keyed map — no need to collect a second BTreeSet.
        if matches!(edge.confidence, EdgeConfidence::Resolved { .. })
            && !snapshot.files.contains_key(edge.to.as_str())
        {
            findings.push(PolicyFinding {
                code: "dangling_edge".into(),
                from: edge.from.clone(),
                to: edge.to.clone(),
                message: "resolved edge points to a missing file".into(),
            });
        }
        if edge.kind == EdgeKind::Import && edge.from.split('/').next() != edge.to.split('/').next()
        {
            findings.push(PolicyFinding {
                code: "cross_package".into(),
                from: edge.from.clone(),
                to: edge.to.clone(),
                message: "import crosses package boundary".into(),
            });
        }
    }
    // Common case: nothing suppressed → skip building a `key()` string per finding.
    if suppressions.keys.is_empty() {
        return findings;
    }
    findings
        .into_iter()
        .filter(|finding| !suppressions.keys.contains(&Suppressions::key(finding)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IndexSnapshot, SnapshotId};
    use std::collections::BTreeMap;
    #[test]
    fn suppression_is_stable() {
        let snapshot = IndexSnapshot {
            id: SnapshotId {
                root: "r".into(),
                worktree: "w".into(),
                revision: "v".into(),
                content_state: "c".into(),
                schema_version: 1,
                grammar_version: "g".into(),
                config_hash: "h".into(),
            },
            files: BTreeMap::from([(
                "a.ts".into(),
                crate::model::FileArtifact {
                    path: "a.ts".into(),
                    language: "typescript".into(),
                    source_hash: "x".into(),
                    parser_version: "g".into(),
                    extractor_version: "e".into(),
                    diagnostics: vec![],
                    symbols: vec![],
                    imports: vec![],
                    exports: vec![],
                    symbol_refs: vec![],
                    bytes_read: 1,
                },
            )]),
            edges: vec![crate::model::Edge {
                from: "a.ts".into(),
                to: "missing.ts".into(),
                kind: EdgeKind::Import,
                confidence: EdgeConfidence::Resolved {
                    score: 1.0,
                    reason: "x".into(),
                },
                type_only: false,
            }],
        };
        let findings = validate_snapshot(&snapshot, &Suppressions::default());
        let dangling = findings
            .iter()
            .find(|finding| finding.code == "dangling_edge")
            .unwrap();
        let mut s = Suppressions::default();
        s.keys.insert(Suppressions::key(dangling));
        assert!(
            !validate_snapshot(&snapshot, &s)
                .iter()
                .any(|finding| finding.code == "dangling_edge")
        );
    }
}
