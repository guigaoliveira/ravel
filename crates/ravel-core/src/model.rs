use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const INDEX_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
    pub span: Option<Span>,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct Span {
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(
    Debug,
    Clone,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct Complexity {
    /// McCabe: 1 + branch points (if/for/while/case/catch/ternary/logical).
    pub cyclomatic: u32,
    /// Nesting-weighted branch cost (simplified Sonar-style).
    pub cognitive: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Symbol {
    /// Stable logical workspace identity. Display/search code must use `name`/`qualified_name`,
    /// not this opaque key. Overload signatures and merged declarations intentionally share it.
    pub id: String,
    pub name: String,
    /// Owner-qualified display name (`CheckoutService.execute`). Equal to `name` for top-level
    /// declarations.
    pub qualified_name: String,
    pub kind: Arc<str>,
    pub span: Span,
    pub exported: bool,
    pub complexity: Option<Complexity>,
    /// Lexical block that owns this declaration. `None` means file/function/member scope.
    pub scope: Option<Span>,
}

pub fn symbol_semantic_namespace(kind: &str) -> &'static str {
    if matches!(kind, "interface_declaration" | "type_alias_declaration") {
        "type"
    } else {
        "value"
    }
}

pub fn stable_symbol_id_for_kind(
    path: &str,
    qualified_name: &str,
    kind: &str,
    scope: Option<Span>,
) -> String {
    let namespace = symbol_semantic_namespace(kind);
    scope.map_or_else(
        || format!("symbol://{path}#{namespace}:{qualified_name}"),
        |scope| {
            format!(
                "symbol://{path}#{namespace}:{qualified_name}@scope:{}:{}",
                scope.start_byte, scope.end_byte
            )
        },
    )
}

pub fn symbol_path_from_id(id: &str) -> Option<&str> {
    id.strip_prefix("symbol://")?
        .split_once('#')
        .map(|(path, _)| path)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ImportBindingKind {
    Default,
    Named,
    Namespace,
    ImportEquals,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportBinding {
    /// Name exported by the target module (`default`, `*`, or a named export).
    pub imported: String,
    /// Name visible in the importing file.
    pub local: String,
    pub kind: ImportBindingKind,
    pub type_only: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Import {
    pub specifier: String,
    pub type_only: bool,
    pub span: Span,
    /// Empty for side-effect-only imports.
    pub bindings: Vec<ImportBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExportBindingKind {
    Declaration,
    Default,
    Named,
    Namespace,
    Star,
    CommonJs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportBinding {
    /// Local/imported name. `*` for star exports and `default` for anonymous defaults.
    pub local: String,
    /// Name exposed by this module.
    pub exported: String,
    pub kind: ExportBindingKind,
    pub type_only: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Export {
    pub name: Option<String>,
    pub specifier: Option<String>,
    pub type_only: bool,
    pub span: Span,
    pub bindings: Vec<ExportBinding>,
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
    pub symbol_refs: Vec<SymbolRef>,
    pub bytes_read: u64,
}

#[derive(
    Debug,
    Clone,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub enum EdgeKind {
    Import,
    ReExport,
    /// symbol → symbol: a function/method call (approximate; type-less resolution).
    Calls,
    /// class → base class.
    Extends,
    /// class → interface it implements.
    Implements,
    /// symbol → constructor called with `new`.
    Instantiates,
    /// symbol → statically named runtime value (for example a JSX component).
    References,
    /// symbol → type used in an annotation/signature.
    TypeOf,
    /// declaration/member → decorator function/class.
    Decorates,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Import => "Import",
            Self::ReExport => "ReExport",
            Self::Calls => "Calls",
            Self::Extends => "Extends",
            Self::Implements => "Implements",
            Self::Instantiates => "Instantiates",
            Self::References => "References",
            Self::TypeOf => "TypeOf",
            Self::Decorates => "Decorates",
        }
    }
}

/// A raw symbol-level reference captured at scan time (before workspace resolution).
/// `from_id` identifies the enclosing declaration; `to` is the referenced name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymbolRef {
    /// Stable id of the enclosing declaration.
    pub from_id: String,
    pub to: String,
    pub kind: EdgeKind,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EdgeConfidence {
    Resolved { score: f32, reason: Arc<str> },
    Candidate { score: f32, reason: Arc<str> },
    Unresolved { score: f32, reason: Arc<str> },
}

#[derive(
    Debug,
    Clone,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub enum EdgeProvenance {
    Ast,
    Resolution,
    Heuristic,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub confidence: EdgeConfidence,
    pub type_only: bool,
    /// File and syntax span that produced the relationship, when available.
    pub source_path: Option<String>,
    pub span: Option<Span>,
    pub provenance: EdgeProvenance,
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
#[derive(
    Debug,
    Clone,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct SymbolMeta {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: Arc<str>,
    pub path: String,
    pub span: Span,
    pub exported: bool,
    pub complexity: Option<Complexity>,
}

#[derive(
    Debug,
    Clone,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct SymbolMetaDict {
    pub format_version: u32,
    pub snapshot_id: String,
    /// Unique by original name (first file wins), sorted by name for binary search.
    pub entries: Vec<SymbolMeta>,
    /// Additional definitions sharing a name, sorted by name for symbol lookup.
    pub duplicates: Vec<SymbolMeta>,
    /// Encoded entry locations sorted by stable id / qualified name for O(log S) cold lookups.
    pub id_order: Vec<u32>,
    pub qualified_order: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SymbolMetaOverlay {
    pub(crate) snapshot_id: String,
    pub(crate) removed_ids: Vec<String>,
    pub(crate) upserts: Vec<SymbolMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SymbolMetaShardIndex {
    pub(crate) format_version: u32,
    pub(crate) snapshot_id: String,
    pub(crate) shard_bits: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SymbolMetaIdShard {
    pub(crate) entries: Vec<SymbolMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SymbolMetaLookupShard {
    pub(crate) entries: Vec<(String, Vec<SymbolMetaLocation>)>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SymbolMetaLocation {
    pub(crate) shard: u8,
    pub(crate) index: u32,
    pub(crate) id_digest: [u8; 32],
}

impl SymbolMetaShardIndex {
    pub(crate) const FORMAT_VERSION: u32 = 1;
}

impl SymbolMetaOverlay {
    pub(crate) fn compose(overlays: impl IntoIterator<Item = Self>) -> Option<Self> {
        let mut removed_ids = std::collections::BTreeSet::new();
        let mut upserts = BTreeMap::new();
        let mut snapshot_id = None;
        for overlay in overlays {
            for id in overlay.removed_ids {
                removed_ids.insert(id.clone());
                upserts.remove(&id);
            }
            for entry in overlay.upserts {
                removed_ids.insert(entry.id.clone());
                upserts.insert(entry.id.clone(), entry);
            }
            snapshot_id = Some(overlay.snapshot_id);
        }
        snapshot_id.map(|snapshot_id| Self {
            snapshot_id,
            removed_ids: removed_ids.into_iter().collect(),
            upserts: upserts.into_values().collect(),
        })
    }

    pub(crate) fn from_artifact_changes(
        snapshot_id: String,
        changes: &[(Option<&FileArtifact>, Option<&FileArtifact>)],
    ) -> Self {
        let removed_ids = changes
            .iter()
            .flat_map(|(old, _)| old.iter().flat_map(|artifact| &artifact.symbols))
            .map(|symbol| symbol.id.clone())
            .collect();
        let upserts = changes
            .iter()
            .flat_map(|(_, new)| {
                new.iter().flat_map(|artifact| {
                    artifact.symbols.iter().map(|symbol| SymbolMeta {
                        id: symbol.id.clone(),
                        name: symbol.name.clone(),
                        qualified_name: symbol.qualified_name.clone(),
                        kind: Arc::clone(&symbol.kind),
                        path: artifact.path.clone(),
                        span: symbol.span,
                        exported: symbol.exported,
                        complexity: symbol.complexity.clone(),
                    })
                })
            })
            .collect();
        Self {
            snapshot_id,
            removed_ids,
            upserts,
        }
    }
}

impl SymbolMetaDict {
    pub const FORMAT_VERSION: u32 = 5;

    pub fn from_snapshot(snapshot: &IndexSnapshot) -> Self {
        use std::collections::BTreeMap;
        let mut by_name: BTreeMap<String, SymbolMeta> = BTreeMap::new();
        let mut duplicates = Vec::new();
        for (path, artifact) in &snapshot.files {
            for symbol in &artifact.symbols {
                let meta = SymbolMeta {
                    id: symbol.id.clone(),
                    name: symbol.name.clone(),
                    qualified_name: symbol.qualified_name.clone(),
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
        let mut result = Self {
            format_version: Self::FORMAT_VERSION,
            snapshot_id: snapshot.id.stable_key(),
            entries: by_name.into_values().collect(),
            duplicates,
            id_order: Vec::new(),
            qualified_order: Vec::new(),
        };
        result.rebuild_orders();
        result
    }

    pub(crate) fn from_all_entries(snapshot_id: String, mut all: Vec<SymbolMeta>) -> Self {
        all.sort_by(|left, right| {
            (&left.name, &left.path, left.span, &left.id).cmp(&(
                &right.name,
                &right.path,
                right.span,
                &right.id,
            ))
        });
        let mut result = Self {
            format_version: Self::FORMAT_VERSION,
            snapshot_id,
            entries: Vec::new(),
            duplicates: Vec::new(),
            id_order: Vec::new(),
            qualified_order: Vec::new(),
        };
        let mut last_name: Option<String> = None;
        for entry in all {
            if last_name.as_deref() == Some(entry.name.as_str()) {
                result.duplicates.push(entry);
            } else {
                last_name = Some(entry.name.clone());
                result.entries.push(entry);
            }
        }
        result.rebuild_orders();
        result
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

    pub fn get_by_id(&self, id: &str) -> Option<&SymbolMeta> {
        let start = self
            .id_order
            .partition_point(|location| self.at(*location).id.as_str() < id);
        self.id_order[start..]
            .iter()
            .map(|location| self.at(*location))
            .take_while(|entry| entry.id == id)
            .last()
    }

    pub fn entries_for_qualified(&self, qualified: &str) -> Vec<&SymbolMeta> {
        let start = self
            .qualified_order
            .partition_point(|location| self.at(*location).qualified_name.as_str() < qualified);
        self.qualified_order[start..]
            .iter()
            .map(|location| self.at(*location))
            .take_while(|entry| entry.qualified_name == qualified)
            .collect()
    }

    pub(crate) fn is_well_formed(&self) -> bool {
        let expected = self.entries.len() + self.duplicates.len();
        self.id_order.len() == expected && self.qualified_order.len() == expected
    }

    pub(crate) fn apply_overlays(&mut self, overlays: Vec<SymbolMetaOverlay>) {
        let mut removed = std::collections::BTreeSet::new();
        let mut upserts = BTreeMap::new();
        let mut snapshot_id = None;
        for overlay in overlays {
            for id in overlay.removed_ids {
                removed.insert(id.clone());
                upserts.remove(&id);
            }
            for entry in overlay.upserts {
                removed.insert(entry.id.clone());
                upserts.insert(entry.id.clone(), entry);
            }
            snapshot_id = Some(overlay.snapshot_id);
        }
        let Some(snapshot_id) = snapshot_id else {
            return;
        };
        let mut all = std::mem::take(&mut self.entries);
        all.extend(std::mem::take(&mut self.duplicates));
        all.retain(|entry| !removed.contains(&entry.id));
        all.extend(upserts.into_values());
        all.sort_by(|left, right| {
            (&left.name, &left.path, left.span, &left.id).cmp(&(
                &right.name,
                &right.path,
                right.span,
                &right.id,
            ))
        });
        let mut last_name: Option<String> = None;
        for entry in all {
            if last_name.as_deref() == Some(entry.name.as_str()) {
                self.duplicates.push(entry);
            } else {
                last_name = Some(entry.name.clone());
                self.entries.push(entry);
            }
        }
        self.snapshot_id = snapshot_id;
        self.rebuild_orders();
    }

    fn rebuild_orders(&mut self) {
        assert!(self.entries.len() < (1usize << 31));
        assert!(self.duplicates.len() < (1usize << 31));
        let mut locations = Vec::with_capacity(self.entries.len() + self.duplicates.len());
        locations.extend((0..self.entries.len()).map(|index| index as u32));
        locations.extend((0..self.duplicates.len()).map(|index| (1 << 31) | index as u32));
        let entries = &self.entries;
        let duplicates = &self.duplicates;
        let at = |encoded: u32| {
            let duplicate = encoded & (1 << 31) != 0;
            let index = (encoded & !(1 << 31)) as usize;
            if duplicate {
                &duplicates[index]
            } else {
                &entries[index]
            }
        };
        self.id_order = locations.clone();
        self.id_order.sort_unstable_by(|left, right| {
            let left = at(*left);
            let right = at(*right);
            (&left.id, left.span).cmp(&(&right.id, right.span))
        });
        self.qualified_order = locations;
        self.qualified_order.sort_unstable_by(|left, right| {
            let left = at(*left);
            let right = at(*right);
            (&left.qualified_name, left.span).cmp(&(&right.qualified_name, right.span))
        });
    }

    fn at(&self, encoded: u32) -> &SymbolMeta {
        let duplicate = encoded & (1 << 31) != 0;
        let index = (encoded & !(1 << 31)) as usize;
        if duplicate {
            &self.duplicates[index]
        } else {
            &self.entries[index]
        }
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
