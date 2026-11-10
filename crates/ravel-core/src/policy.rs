use crate::model::{EdgeConfidence, EdgeKind, IndexSnapshot};
use rustc_hash::FxHashSet;
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
/// Extensions bundlers load as non-code assets: styles, images, fonts, media, data and documents,
/// and a few other loader-handled formats. This is a list of what is known to be an asset, not
/// "anything that is not a source extension": dotted module names are the norm (`./user.service`,
/// `./app.module`), and reading their last segment as an asset extension hid every one of them that
/// failed to resolve.
const ASSET_EXTENSIONS: &[&str] = &[
    "css", "scss", "sass", "less", "styl", "stylus", "pcss", "svg", "png", "jpg", "jpeg", "gif",
    "webp", "avif", "ico", "bmp", "tif", "tiff", "woff", "woff2", "ttf", "otf", "eot", "mp3",
    "mp4", "webm", "ogg", "wav", "flac", "aac", "m4a", "mov", "json", "json5", "jsonc", "yaml",
    "yml", "toml", "csv", "tsv", "xml", "txt", "md", "mdx", "html", "htm", "graphql", "gql",
    "wasm", "node", "glsl", "vert", "frag", "wgsl",
];

fn is_asset_extension(extension: &str) -> bool {
    ASSET_EXTENSIONS
        .iter()
        .chain(crate::config::COMPONENT_SOURCE_EXTENSIONS)
        .any(|asset| asset.eq_ignore_ascii_case(extension))
}

pub fn validate_snapshot(
    snapshot: &IndexSnapshot,
    suppressions: &Suppressions,
) -> Vec<PolicyFinding> {
    let mut findings = Vec::new();
    let symbol_ids: FxHashSet<&str> = snapshot
        .files
        .values()
        .flat_map(|artifact| artifact.symbols.iter().map(|symbol| symbol.id.as_str()))
        .collect();
    for edge in &snapshot.edges {
        // `snapshot.files` is already a keyed map — no need to collect a second BTreeSet.
        if matches!(edge.confidence, EdgeConfidence::Resolved { .. })
            && !snapshot.files.contains_key(edge.to.as_str())
            && !symbol_ids.contains(edge.to.as_str())
        {
            findings.push(PolicyFinding {
                code: "dangling_edge".into(),
                from: edge.from.clone(),
                to: edge.to.clone(),
                message: "resolved edge points to a missing file".into(),
            });
        }
        // An unresolved import keeps its raw specifier as `to`, so `@lib/pay` versus `src` looked
        // like a boundary violation and every alias the resolver failed on was reported as an
        // architecture problem. That is a confident misdiagnosis of the caller's code when the real
        // fault is a config the resolver could not apply -- and it is the same state where relation
        // answers silently understate.
        // Only specifiers that *must* resolve to a file in this workspace. A bare package name, a
        // Node builtin and an asset import are all unresolved by design, so flagging every
        // unresolved import turned `validate` permanently red on any repo with dependencies -- and
        // buried the case this exists for under every npm import. Alias failures are reported by
        // `status.config_problems` instead, which can see the config that failed.
        // A relative specifier naming an asset (`./styles.css`, `./logo.svg`) or carrying a bundler
        // loader query (`./icon.svg?react`, `./worker.ts?worker`) never resolves to a node in this
        // graph and never should.
        let asset_import = edge.to.contains('?')
            || std::path::Path::new(edge.to.as_str())
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(is_asset_extension);
        let must_resolve = (edge.to.starts_with('.') || edge.to.starts_with('/')) && !asset_import;
        // A broken `export * from './gone'` loses edges exactly like a broken import does.
        if matches!(edge.kind, EdgeKind::Import | EdgeKind::ReExport)
            && must_resolve
            && !matches!(edge.confidence, EdgeConfidence::Resolved { .. })
        {
            findings.push(PolicyFinding {
                code: "unresolved_import".into(),
                from: edge.from.clone(),
                to: edge.to.clone(),
                message: "relative import or re-export did not resolve to a file in the graph; \
                          its edges are missing"
                    .into(),
            });
        }
        // No unconfigured `cross_package` finding.
        //
        // It compared the first path component of each side, so on a monorepo laid out as
        // `apps/<service>` and `libs/<shared>` it reported every descent into a shared library as a
        // boundary violation while staying silent on coupling between peer services -- `apps` vs
        // `apps` compares equal. Measured on a 19,489-file workspace: 122,324 findings, of which
        // 48,220 were app-to-library descents, against 25,258 app-to-app imports it could not see at
        // all. Aggregated to directory pairs the whole report is 15 rows, so most of the volume was
        // one fact repeated -- 64,767 of the findings are `symbol://` edges restating a file-to-file
        // import already counted.
        //
        // The residue that arguably described something -- imports from `libs/*` back up into
        // `apps/*` -- is roughly 1,890 findings, about 1.5% at file granularity. Most of those sit
        // inside an existing 89-package cycle that `cycles` already reports; a minority do not, and
        // for those this removal does lose a signal.
        //
        // Nothing replaces it here. Whether crossing a given boundary is legal depends on the
        // repository's intended layering, which a path prefix cannot express: the same
        // `apps/x -> libs/y` edge is correct in one project and wrong in another. A cycle, by
        // contrast, is wrong under every layering, which is why `cycles` stays config-free.
        //
        // Two claims that were in an earlier draft of this comment are false and are recorded so
        // nobody rebuilds on them: `ci` was *not* burying the cycle (it listed the cycle first, then
        // the policy count), and the rule did not use a notion of package nothing else uses --
        // `graph::package_name` falls back to the first path segment when no `apps|libs|packages`
        // marker is present, so on a marker-less repo the two agreed.
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
    fn artifact(path: &str) -> crate::model::FileArtifact {
        crate::model::FileArtifact {
            path: path.into(),
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
        }
    }
    fn resolved_import(from: &str, to: &str) -> crate::model::Edge {
        crate::model::Edge {
            from: from.into(),
            to: to.into(),
            kind: EdgeKind::Import,
            confidence: EdgeConfidence::Resolved {
                score: 1.0,
                reason: "x".into(),
            },
            type_only: false,
            source_path: None,
            span: None,
            provenance: crate::model::EdgeProvenance::Ast,
        }
    }
    fn snapshot_of(
        files: Vec<crate::model::FileArtifact>,
        edges: Vec<crate::model::Edge>,
    ) -> IndexSnapshot {
        IndexSnapshot {
            id: SnapshotId {
                root: "r".into(),
                worktree: "w".into(),
                revision: "v".into(),
                content_state: "c".into(),
                schema_version: 1,
                grammar_version: "g".into(),
                config_hash: "h".into(),
            },
            files: files
                .into_iter()
                .map(|artifact| (artifact.path.clone(), artifact))
                .collect(),
            edges,
        }
    }

    /// A resolved import that crosses a top-level directory is not, by itself, a finding.
    ///
    /// Guards a removal, so it is worth stating exactly what it forbids -- and what it does not.
    /// Judging `apps/* -> libs/*` by comparing path prefixes reported 122,324 findings on a real
    /// 19,489-file monorepo while missing peer-to-peer coupling entirely, so neither edge below may
    /// be reported without a declared policy.
    ///
    /// Deliberately absent: an edge from `libs/*` back up into `apps/*`. A future rule may well
    /// decide that layer inversion is worth reporting unconfigured, and this test must not stand in
    /// its way; it is not evidence either for or against that.
    #[test]
    fn crossing_a_directory_is_not_a_violation_on_its_own() {
        let snapshot = snapshot_of(
            vec![
                artifact("apps/checkout/src/pay.ts"),
                artifact("libs/money/src/index.ts"),
                artifact("apps/billing/src/invoice.ts"),
            ],
            vec![
                // The direction a layered monorepo is *supposed* to allow, and the bulk of what the
                // removed rule reported.
                resolved_import("apps/checkout/src/pay.ts", "libs/money/src/index.ts"),
                // The direction it was blind to, which is the coupling that actually matters.
                resolved_import("apps/checkout/src/pay.ts", "apps/billing/src/invoice.ts"),
            ],
        );
        let findings = validate_snapshot(&snapshot, &Suppressions::default());
        assert!(
            findings.is_empty(),
            "a resolved cross-directory import must not be reported without a declared policy, got {findings:?}"
        );
    }

    /// The findings that survive are the ones true under any layering: the edge points nowhere.
    #[test]
    fn a_resolved_edge_to_a_missing_file_is_still_reported() {
        let snapshot = snapshot_of(
            vec![artifact("apps/checkout/src/pay.ts")],
            vec![resolved_import(
                "apps/checkout/src/pay.ts",
                "libs/money/src/gone.ts",
            )],
        );
        let codes: Vec<_> = validate_snapshot(&snapshot, &Suppressions::default())
            .into_iter()
            .map(|finding| finding.code)
            .collect();
        assert_eq!(codes, vec!["dangling_edge".to_string()]);
    }

    fn unresolved(from: &str, to: &str, kind: EdgeKind) -> crate::model::Edge {
        crate::model::Edge {
            kind,
            confidence: EdgeConfidence::Unresolved {
                score: 0.0,
                reason: "x".into(),
            },
            ..resolved_import(from, to)
        }
    }

    /// Dotted module names (`./user.service`) are code, not assets, and a broken re-export loses
    /// edges exactly like a broken import does.
    #[test]
    fn unresolved_relative_imports_and_reexports_are_reported_but_assets_are_not() {
        let from = "src/app.ts";
        let snapshot = snapshot_of(
            vec![artifact(from)],
            vec![
                unresolved(from, "./user.service", EdgeKind::Import),
                unresolved(from, "./app.module", EdgeKind::Import),
                unresolved(from, "./gone", EdgeKind::ReExport),
                unresolved(from, "./missing.js", EdgeKind::Import),
                unresolved(from, "./styles.module.css", EdgeKind::Import),
                unresolved(from, "./logo.SVG", EdgeKind::Import),
                unresolved(from, "./data.json", EdgeKind::Import),
                unresolved(from, "./icon.svg?react", EdgeKind::Import),
                unresolved(from, "./worker.ts?worker", EdgeKind::Import),
                unresolved(from, "./App.vue", EdgeKind::Import),
                unresolved(from, "react", EdgeKind::Import),
                unresolved(from, "node:fs", EdgeKind::ReExport),
            ],
        );
        let reported: Vec<_> = validate_snapshot(&snapshot, &Suppressions::default())
            .into_iter()
            .filter(|finding| finding.code == "unresolved_import")
            .map(|finding| finding.to)
            .collect();
        assert_eq!(
            reported,
            ["./user.service", "./app.module", "./gone", "./missing.js"]
        );
    }

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
                source_path: None,
                span: None,
                provenance: crate::model::EdgeProvenance::Ast,
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
