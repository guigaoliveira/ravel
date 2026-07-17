//! Serializable dependency schema for exact bounded structural updates.
//!
//! This module deliberately does not publish deltas. It records enough conservative reverse
//! information for a writer to identify the files affected by create/delete/rename before an
//! atomic graph/search/storage overlay implementation enables the fast path.

use crate::{
    model::FileArtifact,
    resolver::{ResolutionTrace, ResolverConfig, resolve_edges_with_traces, resolver_fingerprint},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FileContribution {
    /// Every path whose creation/deletion can change one of this file's module resolutions.
    pub module_candidates: BTreeSet<String>,
    /// Canonical basename fallback keys actually attempted by the resolver.
    pub bare_specifiers: BTreeSet<String>,
    pub symbol_definitions: BTreeSet<String>,
    pub symbol_references: BTreeSet<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct StructuralReverseIndex {
    pub format_version: u32,
    pub resolver_fingerprint: String,
    pub files: BTreeMap<String, FileContribution>,
    pub module_importers: BTreeMap<String, BTreeSet<String>>,
    pub basename_importers: BTreeMap<String, BTreeSet<String>>,
    pub symbol_definers: BTreeMap<String, BTreeSet<String>>,
    pub symbol_referrers: BTreeMap<String, BTreeSet<String>>,
}

impl Default for StructuralReverseIndex {
    fn default() -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            resolver_fingerprint: String::new(),
            files: BTreeMap::new(),
            module_importers: BTreeMap::new(),
            basename_importers: BTreeMap::new(),
            symbol_definers: BTreeMap::new(),
            symbol_referrers: BTreeMap::new(),
        }
    }
}

impl StructuralReverseIndex {
    pub const FORMAT_VERSION: u32 = 2;

    pub fn build(
        root: &Path,
        artifacts: &BTreeMap<String, FileArtifact>,
        resolver: &ResolverConfig,
    ) -> Self {
        let (_, traces) = resolve_edges_with_traces(root, artifacts, resolver);
        Self::build_from_traces(artifacts, resolver, &traces)
    }

    pub fn build_from_traces(
        artifacts: &BTreeMap<String, FileArtifact>,
        resolver: &ResolverConfig,
        traces: &[ResolutionTrace],
    ) -> Self {
        let mut index = Self {
            resolver_fingerprint: resolver_fingerprint(resolver),
            ..Self::default()
        };
        // Resolver traces are emitted in the same path order as `artifacts`. Walk the contiguous
        // ranges directly instead of allocating a workspace-wide map of per-file Vecs.
        let mut trace_cursor = 0usize;
        for (path, artifact) in artifacts {
            let trace_start = trace_cursor;
            while traces
                .get(trace_cursor)
                .is_some_and(|trace| trace.importer == *path)
            {
                trace_cursor += 1;
            }
            let contribution =
                FileContribution::from_artifact(artifact, traces[trace_start..trace_cursor].iter());
            for candidate in &contribution.module_candidates {
                index
                    .module_importers
                    .entry(candidate.clone())
                    .or_default()
                    .insert(path.clone());
            }
            for basename in &contribution.bare_specifiers {
                index
                    .basename_importers
                    .entry(basename.clone())
                    .or_default()
                    .insert(path.clone());
            }
            for symbol in &contribution.symbol_definitions {
                index
                    .symbol_definers
                    .entry(symbol.clone())
                    .or_default()
                    .insert(path.clone());
            }
            for symbol in &contribution.symbol_references {
                index
                    .symbol_referrers
                    .entry(symbol.clone())
                    .or_default()
                    .insert(path.clone());
            }
            index.files.insert(path.clone(), contribution);
        }
        index
    }

    /// Conservative affected set for one atomic structural batch. Over-invalidation is allowed;
    /// missing an importer/referrer is not.
    pub fn affected_files<'a>(
        &self,
        changed_paths: impl IntoIterator<Item = &'a str>,
        changed_symbols: impl IntoIterator<Item = &'a str>,
    ) -> BTreeSet<String> {
        let mut affected = BTreeSet::new();
        for path in changed_paths {
            affected.insert(path.to_owned());
            if let Some(importers) = self.module_importers.get(path) {
                affected.extend(importers.iter().cloned());
            }
            if let Some(stem) = Path::new(path).file_stem().and_then(|stem| stem.to_str())
                && let Some(importers) = self.basename_importers.get(stem)
            {
                affected.extend(importers.iter().cloned());
            }
        }
        for symbol in changed_symbols {
            if let Some(definers) = self.symbol_definers.get(symbol) {
                affected.extend(definers.iter().cloned());
            }
            if let Some(referrers) = self.symbol_referrers.get(symbol) {
                affected.extend(referrers.iter().cloned());
            }
        }
        affected
    }
}

impl FileContribution {
    pub fn from_artifact<'a>(
        artifact: &FileArtifact,
        traces: impl IntoIterator<Item = &'a ResolutionTrace>,
    ) -> Self {
        let mut contribution = Self::default();
        for trace in traces {
            contribution
                .module_candidates
                .extend(trace.attempted_paths.iter().cloned());
            contribution
                .bare_specifiers
                .extend(trace.basename_keys.iter().cloned());
        }
        contribution
            .symbol_definitions
            .extend(artifact.symbols.iter().map(|symbol| symbol.name.clone()));
        contribution.symbol_references.extend(
            artifact
                .symbol_refs
                .iter()
                .map(|reference| reference.to.clone()),
        );
        contribution
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::parse_source;

    fn artifact(path: &str, source: &str) -> FileArtifact {
        let mut artifact = parse_source(path, source.as_bytes());
        artifact.path = path.to_owned();
        artifact
    }

    #[test]
    fn indexes_nonexistent_relative_candidates_for_future_creates() {
        let root = tempfile::tempdir().unwrap();
        let mut artifacts = BTreeMap::new();
        artifacts.insert(
            "src/caller.ts".into(),
            artifact("src/caller.ts", "import { future } from './future';\n"),
        );
        let index =
            StructuralReverseIndex::build(root.path(), &artifacts, &ResolverConfig::default());
        assert_eq!(
            index.module_importers.get("src/future.ts"),
            Some(&BTreeSet::from(["src/caller.ts".to_owned()])),
            "keys={:?}",
            index.module_importers.keys().collect::<Vec<_>>()
        );
        assert!(
            index
                .affected_files(["src/future.ts"], std::iter::empty())
                .contains("src/caller.ts")
        );
    }

    #[test]
    fn symbol_definition_changes_find_existing_referrers() {
        let root = tempfile::tempdir().unwrap();
        let mut artifacts = BTreeMap::new();
        artifacts.insert(
            "src/caller.ts".into(),
            artifact(
                "src/caller.ts",
                "export function caller() { return target(); }\n",
            ),
        );
        artifacts.insert(
            "src/target.ts".into(),
            artifact("src/target.ts", "export function target() {}\n"),
        );
        let index =
            StructuralReverseIndex::build(root.path(), &artifacts, &ResolverConfig::default());
        let affected = index.affected_files(std::iter::empty(), ["target"]);
        assert!(affected.contains("src/caller.ts"));
        assert!(affected.contains("src/target.ts"));
    }

    #[test]
    fn rename_batch_unions_old_and_new_dependents_and_roundtrips() {
        let root = tempfile::tempdir().unwrap();
        let mut artifacts = BTreeMap::new();
        artifacts.insert(
            "src/a.ts".into(),
            artifact("src/a.ts", "import './old';\nimport './new';\n"),
        );
        let index =
            StructuralReverseIndex::build(root.path(), &artifacts, &ResolverConfig::default());
        let affected = index.affected_files(["src/old.ts", "src/new.ts"], std::iter::empty());
        assert!(affected.contains("src/a.ts"));
        let bytes = bincode::serialize(&index).unwrap();
        let decoded: StructuralReverseIndex = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded, index);
    }

    #[cfg(unix)]
    #[test]
    fn nonexistent_and_rename_candidates_stay_relative_through_symlink_root() {
        use std::fs;
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        let linked_root = parent.path().join("workspace-link");
        symlink(&workspace, &linked_root).unwrap();
        let artifacts = BTreeMap::from([(
            "src/caller.ts".into(),
            artifact(
                "src/caller.ts",
                "import './future';\nimport './old';\nimport './new';\n",
            ),
        )]);

        let index =
            StructuralReverseIndex::build(&linked_root, &artifacts, &ResolverConfig::default());
        for candidate in ["src/future.ts", "src/old.ts", "src/new.ts"] {
            assert_eq!(
                index.module_importers.get(candidate),
                Some(&BTreeSet::from(["src/caller.ts".to_owned()])),
                "candidate={candidate} keys={:?}",
                index.module_importers.keys().collect::<Vec<_>>()
            );
        }
        assert!(
            index
                .affected_files(["src/old.ts", "src/new.ts"], std::iter::empty())
                .contains("src/caller.ts")
        );
    }
}
