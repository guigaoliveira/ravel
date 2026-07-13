use crate::model::IndexSnapshot;
use rustc_hash::{FxHashMap, FxHashSet};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Exact,
    Prefix,
    Fuzzy,
    Regex,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SearchHit {
    pub value: String,
    pub score_micros: u64,
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("invalid search query: {0}")]
    Invalid(String),
    #[error("search index: {0}")]
    Backend(String),
}

/// On-disk unique-name dictionary (cold load).
///
/// Scale notes (1M–1B symbols):
/// - This struct is a **single shard**. At huge N, publish multiple shards (by hash prefix)
///   and open only the shards needed for a query — never materialize 1B names in one Vec.
/// - v2 stores parallel `lower` keys so open does not re-lowercase O(N) for every search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SymbolDict {
    pub format_version: u32,
    pub snapshot_id: String,
    /// Unique original spellings, sorted by (lower, original).
    pub names: Vec<String>,
    /// Parallel lowercase keys (same order as `names`). Empty on v1 → computed on open.
    #[serde(default)]
    pub lower: Vec<String>,
}

impl SymbolDict {
    pub const FORMAT_VERSION: u32 = 2;

    pub fn from_snapshot(snapshot: &IndexSnapshot) -> Self {
        // O(N) dedup via hash set (was a BTreeSet whose ordering `from_names` re-sorts anyway).
        let mut seen: FxHashSet<String> = FxHashSet::default();
        for artifact in snapshot.files.values() {
            for symbol in &artifact.symbols {
                if !seen.contains(symbol.name.as_str()) {
                    seen.insert(symbol.name.clone());
                }
            }
        }
        Self::from_names(seen.into_iter().collect(), snapshot.id.stable_key())
    }

    pub fn from_names(names: Vec<String>, snapshot_id: String) -> Self {
        // Lowercase each name exactly once, sort by (lower, original), then split. The old
        // comparator called `to_lowercase()` twice per comparison — O(N log N) allocations.
        let mut paired: Vec<(String, String)> =
            names.into_iter().map(|n| (n.to_lowercase(), n)).collect();
        paired.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut lower = Vec::with_capacity(paired.len());
        let mut names = Vec::with_capacity(paired.len());
        for (low, name) in paired {
            lower.push(low);
            names.push(name);
        }
        Self {
            format_version: Self::FORMAT_VERSION,
            snapshot_id,
            names,
            lower,
        }
    }

    fn ensure_lower(&mut self) {
        if self.lower.len() != self.names.len() {
            self.lower = self.names.iter().map(|n| n.to_lowercase()).collect();
        }
    }
}

/// In-memory indexes over SymbolDict — built once per process open.
///
/// | Op     | Time                         | Memory          |
/// |--------|------------------------------|-----------------|
/// | exact  | O(log N + K)                 | uses lower vec  |
/// | prefix | O(log N + K)                 | uses lower vec  |
/// | fuzzy  | O(B · L) length-bucket scan  | O(N) buckets    |
/// | regex  | O(N) worst, early by limit*  | —               |
///
/// \* regex still scans; at 1B use sharded dict + automata index (future).
struct DictRuntime {
    dict: SymbolDict,
    /// length → indices for fuzzy pruning.
    by_len: FxHashMap<u16, Vec<u32>>,
}

impl DictRuntime {
    fn build(mut dict: SymbolDict) -> Self {
        dict.ensure_lower();
        let mut by_len: FxHashMap<u16, Vec<u32>> = FxHashMap::default();
        for (i, low) in dict.lower.iter().enumerate() {
            let idx = i as u32;
            let len = low.chars().count().min(u16::MAX as usize) as u16;
            by_len.entry(len).or_default().push(idx);
        }
        Self { dict, by_len }
    }

    fn search(
        &self,
        query: &str,
        kind: SearchKind,
        limit: usize,
    ) -> Result<Vec<SearchHit>, SearchError> {
        let normalized = query.to_lowercase();
        let mut hits = Vec::new();
        match kind {
            SearchKind::Exact => {
                // `lower` is already sorted by (lower, original): binary-search the equal range
                // instead of building an O(N) HashMap that cloned every lowercase name.
                let lower = &self.dict.lower;
                let start = lower.partition_point(|s| s.as_str() < normalized.as_str());
                for (i, candidate) in lower.iter().enumerate().skip(start) {
                    if candidate != &normalized {
                        break;
                    }
                    hits.push(SearchHit {
                        value: self.dict.names[i].clone(),
                        score_micros: 1_000_000,
                    });
                }
            }
            SearchKind::Prefix => {
                let lower = &self.dict.lower;
                let start = lower.partition_point(|s| s.as_str() < normalized.as_str());
                for (i, cand) in lower.iter().enumerate().skip(start) {
                    if !cand.starts_with(&normalized) {
                        break;
                    }
                    hits.push(SearchHit {
                        value: self.dict.names[i].clone(),
                        score_micros: 1_000_000,
                    });
                }
            }
            SearchKind::Fuzzy => {
                // Collect the query's chars once, not once per candidate.
                let q: Vec<char> = normalized.chars().collect();
                let qlen = q.len();
                let lo = qlen.saturating_sub(2);
                let hi = qlen.saturating_add(2).min(u16::MAX as usize);
                // Only scan length buckets where |len - qlen| <= 2 (Levenshtein bound).
                for len in lo..=hi {
                    let key = len as u16;
                    if let Some(idxs) = self.by_len.get(&key) {
                        for &i in idxs {
                            let low = &self.dict.lower[i as usize];
                            if let Some(distance) = levenshtein_at_most(&q, low, 2) {
                                hits.push(SearchHit {
                                    value: self.dict.names[i as usize].clone(),
                                    // Exact fuzzy matches outrank one- and two-edit matches.
                                    score_micros: 900_000 - distance as u64 * 100_000,
                                });
                            }
                        }
                    }
                }
            }
            SearchKind::Regex => {
                // Case-insensitive on the ORIGINAL pattern. Lowercasing the pattern text
                // corrupts metacharacter classes (`\D\W\S\B` → `\d\w\s\b`); use the regex
                // engine's own case-insensitive flag and match against the original names.
                let re = regex::RegexBuilder::new(query)
                    .case_insensitive(true)
                    .build()
                    .map_err(|e| SearchError::Invalid(e.to_string()))?;
                // Full scan of the opened shard: O(N_shard). Deterministic: collect all
                // matches, then sort + truncate to `limit`. No silent mid-universe cutoff —
                // at multi-billion scale, open a name-hash **shard** instead of one giant dict.
                for name in self.dict.names.iter() {
                    if re.is_match(name) {
                        hits.push(SearchHit {
                            value: name.clone(),
                            score_micros: 500_000,
                        });
                    }
                }
            }
        }
        finish_hits(hits, limit)
    }
}

/// Hybrid search backend.
pub struct SearchIndex {
    dict: Option<DictRuntime>,
    tantivy: Option<TantivyBackend>,
    /// Keeps on-disk generation files alive for the full Tantivy reader lifetime.
    generation_guard: Option<crate::generation_gc::GenerationGuard>,
}

struct TantivyBackend {
    reader: tantivy::IndexReader,
    name: tantivy::schema::Field,
    stored: tantivy::schema::Field,
}

impl std::fmt::Debug for SearchIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchIndex")
            .field(
                "dict_names",
                &self.dict.as_ref().map(|d| d.dict.names.len()),
            )
            .field("has_tantivy", &self.tantivy.is_some())
            .field("has_generation_guard", &self.generation_guard.is_some())
            .finish()
    }
}

impl SearchIndex {
    pub fn from_symbol_dict(dict: SymbolDict) -> Self {
        Self {
            dict: Some(DictRuntime::build(dict)),
            tantivy: None,
            generation_guard: None,
        }
    }

    pub fn from_snapshot(snapshot: &IndexSnapshot) -> Result<Self, SearchError> {
        let dict = SymbolDict::from_snapshot(snapshot);
        // Build tantivy by borrowing the names, then hand the dict to the runtime — avoids
        // cloning the entire Vec<String> of names at cold index build.
        let tantivy = Some(TantivyBackend::from_names(
            dict.names.iter().map(|s| s.as_str()),
        )?);
        Ok(Self {
            dict: Some(DictRuntime::build(dict)),
            tantivy,
            generation_guard: None,
        })
    }

    pub fn open_tantivy_dir(path: &std::path::Path) -> Result<Self, SearchError> {
        let index =
            tantivy::Index::open_in_dir(path).map_err(|e| SearchError::Backend(e.to_string()))?;
        let schema = index.schema();
        let name = schema
            .get_field("name")
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        let stored = schema
            .get_field("stored")
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        let reader = index
            .reader()
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        Ok(Self {
            dict: None,
            tantivy: Some(TantivyBackend {
                reader,
                name,
                stored,
            }),
            generation_guard: None,
        })
    }

    pub fn with_generation_guard(mut self, guard: crate::generation_gc::GenerationGuard) -> Self {
        self.generation_guard = Some(guard);
        self
    }

    pub fn with_dict_and_tantivy_dir(
        dict: SymbolDict,
        path: &std::path::Path,
    ) -> Result<Self, SearchError> {
        let mut index = Self::open_tantivy_dir(path)?;
        index.dict = Some(DictRuntime::build(dict));
        Ok(index)
    }

    pub fn publish_tantivy_dir(
        names: impl IntoIterator<Item = impl AsRef<str>>,
        dir: &std::path::Path,
    ) -> Result<(), SearchError> {
        use tantivy::{Index, IndexWriter, doc, schema::STORED, schema::Schema, schema::TEXT};
        std::fs::create_dir_all(dir).map_err(|e| SearchError::Backend(e.to_string()))?;
        let mut builder = Schema::builder();
        let name = builder.add_text_field("name", TEXT);
        let stored = builder.add_text_field("stored", STORED);
        let schema = builder.build();
        let index =
            Index::create_in_dir(dir, schema).map_err(|e| SearchError::Backend(e.to_string()))?;
        let mut writer: IndexWriter = index
            .writer(15_000_000)
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        for n in names {
            let original = n.as_ref();
            writer
                .add_document(doc!(name => original.to_lowercase(), stored => original.to_owned()))
                .map_err(|e| SearchError::Backend(e.to_string()))?;
        }
        writer
            .commit()
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        Ok(())
    }

    pub fn search(
        &self,
        query: &str,
        kind: SearchKind,
        limit: usize,
    ) -> Result<Vec<SearchHit>, SearchError> {
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        match kind {
            SearchKind::Exact | SearchKind::Prefix => {
                if let Some(dict) = &self.dict {
                    return dict.search(query, kind, limit);
                }
                if let Some(tv) = &self.tantivy {
                    return tv.search(query, kind, limit);
                }
            }
            SearchKind::Fuzzy | SearchKind::Regex => {
                if let Some(tv) = &self.tantivy {
                    return tv.search(query, kind, limit);
                }
                if let Some(dict) = &self.dict {
                    return dict.search(query, kind, limit);
                }
            }
        }
        Err(SearchError::Backend("no search backend available".into()))
    }

    pub fn index_segments(&self) -> usize {
        self.tantivy
            .as_ref()
            .map(|t| t.reader.searcher().segment_readers().len())
            .unwrap_or(0)
    }

    pub fn backend_label(&self) -> &'static str {
        match (&self.dict, &self.tantivy) {
            (Some(_), Some(_)) => "hybrid",
            (Some(_), None) => "dict",
            (None, Some(_)) => "tantivy",
            (None, None) => "none",
        }
    }

    pub fn name_count(&self) -> usize {
        self.dict.as_ref().map(|d| d.dict.names.len()).unwrap_or(0)
    }
}

impl TantivyBackend {
    fn from_names<'a>(names: impl Iterator<Item = &'a str>) -> Result<Self, SearchError> {
        use tantivy::{
            Index, IndexWriter, doc,
            schema::{STORED, Schema, TEXT},
        };
        let mut builder = Schema::builder();
        let name = builder.add_text_field("name", TEXT);
        let stored = builder.add_text_field("stored", STORED);
        let schema = builder.build();
        let index = Index::create_in_ram(schema);
        let mut writer: IndexWriter = index
            .writer(15_000_000)
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        for original in names {
            writer
                .add_document(doc!(name => original.to_lowercase(), stored => original.to_owned()))
                .map_err(|e| SearchError::Backend(e.to_string()))?;
        }
        writer
            .commit()
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        let reader = index
            .reader()
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        Ok(Self {
            reader,
            name,
            stored,
        })
    }

    fn search(
        &self,
        query: &str,
        kind: SearchKind,
        limit: usize,
    ) -> Result<Vec<SearchHit>, SearchError> {
        use tantivy::{
            Term, collector::TopDocs, query::FuzzyTermQuery, query::Query, query::RegexQuery,
            query::TermQuery, schema::IndexRecordOption, schema::Value,
        };
        let normalized = query.to_lowercase();
        let boxed: Box<dyn Query> = match kind {
            SearchKind::Exact => Box::new(TermQuery::new(
                Term::from_field_text(self.name, &normalized),
                IndexRecordOption::Basic,
            )),
            SearchKind::Prefix => Box::new(FuzzyTermQuery::new_prefix(
                Term::from_field_text(self.name, &normalized),
                0,
                true,
            )),
            SearchKind::Fuzzy => Box::new(FuzzyTermQuery::new(
                Term::from_field_text(self.name, &normalized),
                2,
                true,
            )),
            SearchKind::Regex => Box::new(
                // Case-insensitive via inline flag on the ORIGINAL pattern; lowercasing the
                // pattern text would corrupt metacharacter classes (`\D` → `\d`).
                RegexQuery::from_pattern(&format!("(?i){query}"), self.name)
                    .map_err(|e| SearchError::Invalid(e.to_string()))?,
            ),
        };
        let collect_limit = limit.max(1).saturating_mul(8).max(256);
        let searcher = self.reader.searcher();
        let docs: Vec<(f32, tantivy::DocAddress)> = searcher
            .search(&boxed, &TopDocs::with_limit(collect_limit).order_by_score())
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        let mut hits = Vec::with_capacity(docs.len());
        for (score, address) in docs {
            let document: tantivy::TantivyDocument = searcher
                .doc(address)
                .map_err(|e| SearchError::Backend(e.to_string()))?;
            if let Some(value) = document
                .get_first(self.stored)
                .and_then(|value| value.as_str())
            {
                hits.push(SearchHit {
                    value: value.to_owned(),
                    score_micros: (score.max(0.0) * 1_000_000.0) as u64,
                });
            }
        }
        finish_hits(hits, limit)
    }
}

fn finish_hits(mut hits: Vec<SearchHit>, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
    hits.sort_by(|left, right| {
        right
            .score_micros
            .cmp(&left.score_micros)
            .then_with(|| left.value.cmp(&right.value))
    });
    // Backends index unique symbol spellings. Keep this defensive dedup for malformed/legacy
    // indexes; equal values also have equal deterministic scores and are adjacent.
    hits.dedup_by(|a, b| a.value == b.value);
    hits.truncate(limit);
    Ok(hits)
}

fn levenshtein_at_most(a: &[char], b: &str, max_dist: usize) -> Option<usize> {
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();
    if n.abs_diff(m) > max_dist {
        return None;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0; m + 1];
    for i in 1..=n {
        curr[0] = i;
        let mut row_min = curr[0];
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(curr[j]);
        }
        if row_min > max_dist {
            return None;
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    (prev[m] <= max_dist).then_some(prev[m])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{FileArtifact, IndexSnapshot, SnapshotId, Span, Symbol},
        scanner::parse_source,
    };
    use std::collections::BTreeMap;
    use std::time::Instant;

    fn snapshot_with_names(names: &[&str]) -> IndexSnapshot {
        let artifact = parse_source("a.ts", b"export class UserService {}");
        let symbols: Vec<Symbol> = names
            .iter()
            .map(|name| Symbol {
                name: (*name).into(),
                kind: "class".into(),
                span: Span {
                    start_byte: 0,
                    end_byte: 1,
                    start_line: 0,
                    start_column: 0,
                    end_line: 0,
                    end_column: 1,
                },
                exported: true,
                complexity: None,
            })
            .collect();
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
            files: BTreeMap::from([(
                "a.ts".into(),
                FileArtifact {
                    symbols,
                    ..artifact
                },
            )]),
            edges: Vec::new(),
        }
    }

    #[test]
    fn exact_prefix_fuzzy_and_regex_on_dict() {
        let snapshot = snapshot_with_names(&["UserService"]);
        let index = SearchIndex::from_symbol_dict(SymbolDict::from_snapshot(&snapshot));
        assert_eq!(
            index
                .search("UserService", SearchKind::Exact, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            index.search("User", SearchKind::Prefix, 10).unwrap().len(),
            1
        );
        assert_eq!(
            index
                .search("UserServce", SearchKind::Fuzzy, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            index.search("User.*", SearchKind::Regex, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn regex_metaclass_not_corrupted_by_case_folding() {
        // `\D` = non-digit. Must match FooBar, must NOT match Foo1. The old code lowercased
        // the pattern, turning `\D` into `\d` and inverting the result.
        let dict = SymbolDict::from_names(vec!["Foo1".into(), "FooBar".into()], "s".into());
        let index = SearchIndex::from_symbol_dict(dict);
        let hits: Vec<_> = index
            .search(r"Foo\D+", SearchKind::Regex, 10)
            .unwrap()
            .into_iter()
            .map(|h| h.value)
            .collect();
        assert_eq!(hits, vec!["FooBar".to_string()]);
    }

    #[test]
    fn case_variants_coexist_and_multi_file_collapses() {
        let mut snap = snapshot_with_names(&["Foo", "foo"]);
        let artifact = parse_source("b.ts", b"export class Foo {}");
        snap.files.insert(
            "b.ts".into(),
            FileArtifact {
                symbols: vec![Symbol {
                    name: "Foo".into(),
                    kind: "class".into(),
                    span: Span {
                        start_byte: 0,
                        end_byte: 1,
                        start_line: 0,
                        start_column: 0,
                        end_line: 0,
                        end_column: 1,
                    },
                    exported: true,
                    complexity: None,
                }],
                ..artifact
            },
        );
        let dict = SymbolDict::from_snapshot(&snap);
        assert_eq!(dict.names.len(), 2);
        let index = SearchIndex::from_symbol_dict(dict);
        let hits = index.search("foo", SearchKind::Exact, 10).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn hits_sorted_by_name_then_truncated() {
        let snapshot = snapshot_with_names(&["Zebra", "Apple", "Mango"]);
        let index = SearchIndex::from_symbol_dict(SymbolDict::from_snapshot(&snapshot));
        assert!(index.search("", SearchKind::Prefix, 2).unwrap().is_empty());
        let hits = index.search("a", SearchKind::Prefix, 10).unwrap();
        assert_eq!(hits[0].value, "Apple");
    }

    #[test]
    fn fuzzy_results_rank_by_edit_distance_then_name() {
        let dict =
            SymbolDict::from_names(vec!["cart".into(), "cat".into(), "cast".into()], "s".into());
        let index = SearchIndex::from_symbol_dict(dict);
        let hits = index.search("cat", SearchKind::Fuzzy, 10).unwrap();
        assert_eq!(
            hits.iter()
                .map(|hit| hit.value.as_str())
                .collect::<Vec<_>>(),
            vec!["cat", "cart", "cast"]
        );
        assert!(hits[0].score_micros > hits[1].score_micros);
    }

    #[test]
    fn dict_and_tantivy_snapshot_agree_on_exact_prefix_sets() {
        let snapshot = snapshot_with_names(&["UserService", "UserStore", "PaymentService"]);
        let dict_index = SearchIndex::from_symbol_dict(SymbolDict::from_snapshot(&snapshot));
        let full = SearchIndex::from_snapshot(&snapshot).unwrap();
        for kind in [SearchKind::Exact, SearchKind::Prefix] {
            let q = if kind == SearchKind::Exact {
                "UserService"
            } else {
                "User"
            };
            let mut a: Vec<_> = dict_index
                .search(q, kind, 100)
                .unwrap()
                .into_iter()
                .map(|h| h.value)
                .collect();
            let mut b: Vec<_> = full
                .search(q, kind, 100)
                .unwrap()
                .into_iter()
                .map(|h| h.value)
                .collect();
            a.sort();
            b.sort();
            assert_eq!(a, b, "kind={kind:?}");
        }
    }

    /// Synthetic scale: exact/prefix must stay sub-linear in wall time vs N.
    #[test]
    fn scale_exact_prefix_sublinear_wall() {
        let sizes = [10_000usize, 50_000, 200_000];
        let mut prev_exact = 0.0f64;
        for &n in &sizes {
            let names: Vec<String> = (0..n).map(|i| format!("Sym{i:08}")).collect();
            let dict = SymbolDict::from_names(names, "scale".into());
            let index = SearchIndex::from_symbol_dict(dict);
            // warm
            let _ = index.search("Sym00001234", SearchKind::Exact, 1).unwrap();
            let t0 = Instant::now();
            for _ in 0..50 {
                let _ = index.search("Sym00001234", SearchKind::Exact, 1).unwrap();
            }
            let exact_ms = t0.elapsed().as_secs_f64() * 1000.0 / 50.0;
            let t1 = Instant::now();
            for _ in 0..20 {
                let _ = index.search("Sym0000", SearchKind::Prefix, 50).unwrap();
            }
            let prefix_ms = t1.elapsed().as_secs_f64() * 1000.0 / 20.0;
            eprintln!("N={n} exact_avg_ms={exact_ms:.4} prefix_avg_ms={prefix_ms:.4}");
            // Exact must not grow like O(N): allow mild constant factors only.
            if prev_exact > 0.0 {
                assert!(
                    exact_ms < prev_exact * 8.0 + 0.5,
                    "exact grew too fast: {prev_exact} -> {exact_ms} at N={n}"
                );
            }
            prev_exact = exact_ms;
            // Absolute budgets on this host (warm, synthetic ASCII names).
            assert!(exact_ms < 2.0, "exact too slow: {exact_ms}ms at N={n}");
            assert!(prefix_ms < 15.0, "prefix too slow: {prefix_ms}ms at N={n}");
        }
    }
}
