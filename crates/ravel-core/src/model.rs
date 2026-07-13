use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
    pub span: Option<Span>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Span {
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Complexity {
    /// McCabe: 1 + branch points (if/for/while/case/catch/ternary/logical).
    pub cyclomatic: u32,
    /// Nesting-weighted branch cost (simplified Sonar-style).
    pub cognitive: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: Arc<str>,
    pub span: Span,
    pub exported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity: Option<Complexity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Import {
    pub specifier: String,
    pub type_only: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Export {
    pub name: Option<String>,
    pub specifier: Option<String>,
    pub type_only: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileArtifact {
    pub path: String,
    pub language: Arc<str>,
    pub source_hash: String,
    pub parser_version: Arc<str>,
    pub extractor_version: Arc<str>,
    pub diagnostics: Vec<Diagnostic>,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<Import>,
    pub exports: Vec<Export>,
    /// Symbol-level references (calls / extends / implements) captured during parse.
    /// NOTE: no `skip_serializing_if` — bincode is positional and would desync.
    #[serde(default)]
    pub symbol_refs: Vec<SymbolRef>,
    pub bytes_read: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeKind {
    Import,
    ReExport,
    /// symbol → symbol: a function/method call (approximate; type-less resolution).
    Calls,
    /// class → base class.
    Extends,
    /// class → interface it implements.
    Implements,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Import => "Import",
            Self::ReExport => "ReExport",
            Self::Calls => "Calls",
            Self::Extends => "Extends",
            Self::Implements => "Implements",
        }
    }
}

/// A raw symbol-level reference captured at scan time (before workspace resolution).
/// `from` = the enclosing symbol's name; `to` = the referenced name (call target / base type).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymbolRef {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EdgeConfidence {
    Resolved { score: f32, reason: Arc<str> },
    Candidate { score: f32, reason: Arc<str> },
    Unresolved { score: f32, reason: Arc<str> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub confidence: EdgeConfidence,
    pub type_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub source_hash: String,
    pub language: String,
    pub grammar_version: String,
    pub extractor_version: String,
    pub resolver_config_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotId {
    pub root: String,
    pub worktree: String,
    pub revision: String,
    pub content_state: String,
    pub schema_version: u32,
    pub grammar_version: String,
    pub config_hash: String,
}

impl SnapshotId {
    pub fn stable_key(&self) -> String {
        // Stream JSON straight into the hasher — no intermediate Vec<u8> allocation.
        let mut hasher = blake3::Hasher::new();
        serde_json::to_writer(&mut hasher, self).expect("snapshot is serializable");
        hasher.finalize().to_hex().to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexSnapshot {
    pub id: SnapshotId,
    pub files: BTreeMap<String, FileArtifact>,
    pub edges: Vec<Edge>,
}

/// Lightweight index summary written as a sidecar for cold `stats` without full deserialize.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexStats {
    pub files: usize,
    pub edges: usize,
    pub bytes: u64,
    pub parse_errors: usize,
    pub snapshot_id: String,
}

/// Precomputed schema counts for cold CLI/MCP reads without hydrating the full snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaSummary {
    pub format_version: u32,
    pub snapshot_id: String,
    pub files: usize,
    pub edges: usize,
    pub packages: usize,
    pub node_kinds: BTreeMap<String, usize>,
    pub edge_kinds: BTreeMap<String, usize>,
}

impl SchemaSummary {
    pub const FORMAT_VERSION: u32 = 1;
}

/// First-seen symbol metadata for cold `node_detail` without full snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymbolMeta {
    pub name: String,
    pub kind: Arc<str>,
    pub path: String,
    pub span: Span,
    pub exported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity: Option<Complexity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymbolMetaDict {
    pub format_version: u32,
    pub snapshot_id: String,
    /// Unique by original name (first file wins), sorted by name for binary search.
    pub entries: Vec<SymbolMeta>,
    /// Additional definitions sharing a name, sorted by name for symbol lookup.
    #[serde(default)]
    pub duplicates: Vec<SymbolMeta>,
}

impl SymbolMetaDict {
    pub const FORMAT_VERSION: u32 = 2;

    pub fn from_snapshot(snapshot: &IndexSnapshot) -> Self {
        use std::collections::BTreeMap;
        let mut by_name: BTreeMap<String, SymbolMeta> = BTreeMap::new();
        let mut duplicates = Vec::new();
        for (path, artifact) in &snapshot.files {
            for symbol in &artifact.symbols {
                let meta = SymbolMeta {
                    name: symbol.name.clone(),
                    kind: symbol.kind.clone(),
                    path: path.clone(),
                    span: symbol.span,
                    exported: symbol.exported,
                    complexity: symbol.complexity.clone(),
                };
                // Keep the first definition in the compact hot lookup and retain only
                // collisions separately, so ordinary node_detail/search sidecars do not grow
                // with repeated names.
                if by_name.contains_key(&symbol.name) {
                    duplicates.push(meta);
                } else {
                    by_name.insert(symbol.name.clone(), meta);
                }
            }
        }
        duplicates.sort_by(|a, b| (&a.name, &a.path, a.span).cmp(&(&b.name, &b.path, b.span)));
        Self {
            format_version: Self::FORMAT_VERSION,
            snapshot_id: snapshot.id.stable_key(),
            entries: by_name.into_values().collect(),
            duplicates,
        }
    }

    pub fn get(&self, name: &str) -> Option<&SymbolMeta> {
        self.entries
            .binary_search_by(|e| e.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.entries[i])
    }

    pub fn entries_for(&self, name: &str) -> impl Iterator<Item = &SymbolMeta> {
        let duplicates_start = self
            .duplicates
            .partition_point(|entry| entry.name.as_str() < name);
        let duplicates_end = self
            .duplicates
            .partition_point(|entry| entry.name.as_str() <= name);
        self.get(name)
            .into_iter()
            .chain(self.duplicates[duplicates_start..duplicates_end].iter())
    }
}

/// Sorted file paths for cold `files_in_package` without full snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileList {
    pub format_version: u32,
    pub snapshot_id: String,
    pub paths: Vec<String>,
}

impl FileList {
    pub const FORMAT_VERSION: u32 = 1;

    pub fn from_snapshot(snapshot: &IndexSnapshot) -> Self {
        // `snapshot.files` is a BTreeMap → keys already yield sorted paths; no re-sort needed.
        let paths: Vec<String> = snapshot.files.keys().cloned().collect();
        Self {
            format_version: Self::FORMAT_VERSION,
            snapshot_id: snapshot.id.stable_key(),
            paths,
        }
    }

    pub fn in_package(&self, package: &str) -> Vec<String> {
        self.in_package_limit(package, usize::MAX)
    }

    pub fn in_package_limit(&self, package: &str, limit: usize) -> Vec<String> {
        let prefix = format!("/{package}/");
        self.paths
            .iter()
            .filter(|path| path.contains(&prefix))
            .take(limit)
            .cloned()
            .collect()
    }
}

/// Compact path → content hash for fast auto-sync no-ops **without** loading the full snapshot.
/// Parallel arrays sorted by path (binary search).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileHashIndex {
    pub format_version: u32,
    pub snapshot_id: String,
    pub paths: Vec<String>,
    /// blake3 hex, same order as `paths`
    pub hashes: Vec<String>,
}

impl FileHashIndex {
    pub const FORMAT_VERSION: u32 = 1;

    pub fn from_snapshot(snapshot: &IndexSnapshot) -> Self {
        // `snapshot.files` is already path-sorted (BTreeMap): fill the parallel arrays in one
        // pass — no intermediate pairs Vec, no sort, no unzip.
        let mut paths = Vec::with_capacity(snapshot.files.len());
        let mut hashes = Vec::with_capacity(snapshot.files.len());
        for (p, a) in &snapshot.files {
            paths.push(p.clone());
            hashes.push(a.source_hash.clone());
        }
        Self {
            format_version: Self::FORMAT_VERSION,
            snapshot_id: snapshot.id.stable_key(),
            paths,
            hashes,
        }
    }

    pub fn get(&self, path: &str) -> Option<&str> {
        self.paths
            .binary_search_by(|p| p.as_str().cmp(path))
            .ok()
            .map(|i| self.hashes[i].as_str())
    }

    pub fn contains(&self, path: &str) -> bool {
        self.get(path).is_some()
    }
}

#[cfg(test)]
mod file_hash_tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn file_hash_index_lookup() {
        let mut files = BTreeMap::new();
        files.insert(
            "a.ts".into(),
            FileArtifact {
                path: "a.ts".into(),
                language: "typescript".into(),
                source_hash: "abc".into(),
                parser_version: "1".into(),
                extractor_version: "1".into(),
                diagnostics: vec![],
                symbols: vec![],
                imports: vec![],
                exports: vec![],
                symbol_refs: vec![],
                bytes_read: 1,
            },
        );
        let snap = IndexSnapshot {
            id: SnapshotId {
                root: "/r".into(),
                worktree: "/r".into(),
                revision: "nogit".into(),
                content_state: "1".into(),
                schema_version: 1,
                grammar_version: "1".into(),
                config_hash: "x".into(),
            },
            files,
            edges: vec![],
        };
        let idx = FileHashIndex::from_snapshot(&snap);
        assert_eq!(idx.get("a.ts"), Some("abc"));
        assert!(!idx.contains("missing.ts"));
    }
}
