use crate::model::IndexSnapshot;
use rustc_hash::FxHashMap;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

const TANTIVY_WRITER_HEAP_BYTES: usize = 15_000_000;
/// Per-thread heap for the multi-threaded index writer. Tantivy requires ≥15 MB per thread;
/// 32 MB keeps segments large enough to limit merge churn without inflating RSS materially.
const TANTIVY_WRITER_HEAP_PER_THREAD: usize = 32_000_000;
/// Cap writer threads so a large host does not spawn a segment merge fan-out that competes with
/// the rest of the (now parallel) publish for cores and disk bandwidth.
const TANTIVY_MAX_WRITER_THREADS: usize = 8;
const TERM_CANDIDATE_MULTIPLIER: usize = 32;
const TERM_CANDIDATE_FLOOR: usize = 1_024;
const NAME_CANDIDATE_MULTIPLIER: usize = 8;
const NAME_CANDIDATE_FLOOR: usize = 256;
const SCORE_EXACT_CASE: u64 = 1_300_000;
const SCORE_EXACT_CASE_INSENSITIVE: u64 = 1_200_000;
const SCORE_PREFIX_CASE: u64 = 1_100_000;
const SCORE_PREFIX: u64 = 1_000_000;
const FUZZY_MAX_DISTANCE: usize = 2;
const SCORE_FUZZY_EXACT: u64 = 900_000;
const SCORE_FUZZY_DISTANCE_PENALTY: u64 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Exact,
    Prefix,
    Fuzzy,
    Regex,
    Terms,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SearchHit {
    pub value: String,
    /// Present for definition-level term matches; exact/prefix remain spelling dictionary hits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_id: Option<String>,
    pub score_micros: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
/// - Parallel lowercase keys avoid re-normalizing O(N) names on every process open.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SymbolDict {
    pub format_version: u32,
    pub snapshot_id: String,
    /// Unique original spellings, sorted by (lower, original).
    pub names: Vec<String>,
    /// Parallel lowercase keys (same order as `names`).
    pub lower: Vec<String>,
    /// Parallel token text used by the persistent term index. It aggregates segmented names,
    /// qualified names, declaration kinds, and paths for every definition sharing the spelling.
    pub terms: Vec<String>,
    /// Definition-level documents prevent terms from different homonyms satisfying one query.
    term_documents: Vec<SymbolTermDocument>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct SymbolTermDocument {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) name_terms: String,
    pub(crate) qualified_terms: String,
    pub(crate) path_terms: String,
    pub(crate) kind_terms: String,
}

impl SymbolTermDocument {
    pub(crate) fn from_symbol(path: &str, symbol: &crate::model::Symbol) -> Self {
        let field_terms = |value: &str| {
            let mut tokens = BTreeSet::new();
            add_search_tokens(&mut tokens, value);
            tokens.into_iter().collect::<Vec<_>>().join(" ")
        };
        Self {
            id: symbol.id.clone(),
            name: symbol.name.clone(),
            name_terms: field_terms(&symbol.name),
            qualified_terms: field_terms(&symbol.qualified_name),
            path_terms: field_terms(path),
            kind_terms: field_terms(symbol.kind.as_ref()),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct SearchTermOverlay {
    pub(crate) snapshot_id: String,
    pub(crate) removed_ids: Vec<String>,
    pub(crate) added_names: Vec<String>,
    pub(crate) removed_names: Vec<String>,
    pub(crate) documents: Vec<SymbolTermDocument>,
}

impl SymbolDict {
    pub const FORMAT_VERSION: u32 = 5;

    pub fn from_snapshot(snapshot: &IndexSnapshot) -> Self {
        // Aggregate each spelling once while retaining deterministic token order.
        let mut by_name: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut term_documents = Vec::new();
        for (path, artifact) in &snapshot.files {
            for symbol in &artifact.symbols {
                let document = SymbolTermDocument::from_symbol(path, symbol);
                let tokens = by_name.entry(symbol.name.clone()).or_default();
                for terms in [
                    &document.name_terms,
                    &document.qualified_terms,
                    &document.path_terms,
                    &document.kind_terms,
                ] {
                    tokens.extend(terms.split_whitespace().map(ToOwned::to_owned));
                }
                term_documents.push(document);
            }
        }
        term_documents.sort_by(|left, right| (&left.name, &left.id).cmp(&(&right.name, &right.id)));
        let mut dict = Self::from_entries(
            by_name
                .into_iter()
                .map(|(name, terms)| (name, terms.into_iter().collect::<Vec<_>>().join(" ")))
                .collect(),
            snapshot.id.stable_key(),
        );
        dict.term_documents = term_documents;
        dict
    }

    /// Prefix/exact dictionary for persisted readers. Definition-level term documents are
    /// streamed directly into Tantivy and must not be retained alongside the snapshot.
    pub fn from_snapshot_names_only(snapshot: &IndexSnapshot) -> Self {
        let names = snapshot
            .files
            .values()
            .flat_map(|artifact| artifact.symbols.iter().map(|symbol| symbol.name.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Self::from_names(names, snapshot.id.stable_key())
    }

    pub fn from_names(names: Vec<String>, snapshot_id: String) -> Self {
        Self::from_entries(
            names
                .into_iter()
                .map(|name| {
                    let mut tokens = BTreeSet::new();
                    add_search_tokens(&mut tokens, &name);
                    (name, tokens.into_iter().collect::<Vec<_>>().join(" "))
                })
                .collect(),
            snapshot_id,
        )
    }

    fn from_entries(entries: Vec<(String, String)>, snapshot_id: String) -> Self {
        // Lowercase each name exactly once, sort by (lower, original), then split. The old
        // comparator called `to_lowercase()` twice per comparison — O(N log N) allocations.
        let mut paired: Vec<(String, String, String)> = entries
            .into_iter()
            .map(|(name, terms)| (name.to_lowercase(), name, terms))
            .collect();
        paired.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut lower = Vec::with_capacity(paired.len());
        let mut names = Vec::with_capacity(paired.len());
        let mut terms = Vec::with_capacity(paired.len());
        for (low, name, search_terms) in paired {
            lower.push(low);
            names.push(name);
            terms.push(search_terms);
        }
        Self {
            format_version: Self::FORMAT_VERSION,
            snapshot_id,
            names,
            lower,
            terms,
            term_documents: Vec::new(),
        }
    }

    pub(crate) fn is_well_formed(&self) -> bool {
        self.lower.len() == self.names.len() && self.terms.len() == self.names.len()
    }

    pub(crate) fn apply_name_overlays(&mut self, overlays: &[SearchTermOverlay]) {
        let mut names: BTreeSet<_> = std::mem::take(&mut self.names).into_iter().collect();
        let mut snapshot_id = self.snapshot_id.clone();
        for overlay in overlays {
            for name in &overlay.removed_names {
                names.remove(name);
            }
            names.extend(overlay.added_names.iter().cloned());
            snapshot_id = overlay.snapshot_id.clone();
        }
        *self = Self::from_names(names.into_iter().collect(), snapshot_id);
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
    fn build(dict: SymbolDict) -> Self {
        debug_assert!(dict.is_well_formed());
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
                        definition_id: None,
                        score_micros: if self.dict.names[i] == query {
                            SCORE_EXACT_CASE
                        } else {
                            SCORE_EXACT_CASE_INSENSITIVE
                        },
                        reason: Some(
                            if self.dict.names[i] == query {
                                "exact-case"
                            } else {
                                "exact-case-insensitive"
                            }
                            .into(),
                        ),
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
                        definition_id: None,
                        score_micros: if self.dict.names[i] == query {
                            SCORE_EXACT_CASE
                        } else if self.dict.names[i].eq_ignore_ascii_case(query) {
                            SCORE_EXACT_CASE_INSENSITIVE
                        } else if self.dict.names[i].starts_with(query) {
                            SCORE_PREFIX_CASE
                        } else {
                            SCORE_PREFIX
                        },
                        reason: Some(
                            if self.dict.names[i] == query {
                                "exact-case"
                            } else if self.dict.names[i].eq_ignore_ascii_case(query) {
                                "exact-case-insensitive"
                            } else if self.dict.names[i].starts_with(query) {
                                "prefix-case"
                            } else {
                                "prefix-case-insensitive"
                            }
                            .into(),
                        ),
                    });
                }
            }
            SearchKind::Fuzzy => {
                // Collect the query's chars once, not once per candidate.
                let q: Vec<char> = normalized.chars().collect();
                let qlen = q.len();
                let lo = qlen.saturating_sub(FUZZY_MAX_DISTANCE);
                let hi = qlen
                    .saturating_add(FUZZY_MAX_DISTANCE)
                    .min(u16::MAX as usize);
                // Only scan length buckets inside the accepted Levenshtein bound.
                for len in lo..=hi {
                    let key = len as u16;
                    if let Some(idxs) = self.by_len.get(&key) {
                        for &i in idxs {
                            let low = &self.dict.lower[i as usize];
                            if let Some(distance) = levenshtein_at_most(&q, low, FUZZY_MAX_DISTANCE)
                            {
                                hits.push(SearchHit {
                                    value: self.dict.names[i as usize].clone(),
                                    definition_id: None,
                                    // Exact fuzzy matches outrank one- and two-edit matches.
                                    score_micros: SCORE_FUZZY_EXACT
                                        - distance as u64 * SCORE_FUZZY_DISTANCE_PENALTY,
                                    reason: Some(format!("fuzzy-distance-{distance}")),
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
                            definition_id: None,
                            score_micros: 500_000,
                            reason: Some("regex".into()),
                        });
                    }
                }
            }
            SearchKind::Terms => {
                return Err(SearchError::Backend(
                    "term search requires the persistent inverted index".into(),
                ));
            }
        }
        finish_hits(hits, limit)
    }
}

/// Hybrid search backend.
pub struct SearchIndex {
    dict: Option<DictRuntime>,
    tantivy: Option<TantivyBackend>,
    term_overlay: Option<SearchTermOverlay>,
    /// Keeps on-disk generation files alive for the full Tantivy reader lifetime.
    generation_guard: Option<crate::generation_gc::GenerationGuard>,
}

struct TantivyBackend {
    reader: tantivy::IndexReader,
    name: tantivy::schema::Field,
    name_terms: tantivy::schema::Field,
    qualified_terms: tantivy::schema::Field,
    path_terms: tantivy::schema::Field,
    kind_terms: tantivy::schema::Field,
    stored: tantivy::schema::Field,
    stored_id: tantivy::schema::Field,
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
            term_overlay: None,
            generation_guard: None,
        }
    }

    pub fn from_snapshot(snapshot: &IndexSnapshot) -> Result<Self, SearchError> {
        let dict = SymbolDict::from_snapshot(snapshot);
        // Build tantivy by borrowing the names, then hand the dict to the runtime — avoids
        // cloning the entire Vec<String> of names at cold index build.
        let tantivy = Some(TantivyBackend::from_dict(&dict)?);
        Ok(Self {
            dict: Some(DictRuntime::build(dict)),
            tantivy,
            term_overlay: None,
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
        let name_terms = schema
            .get_field("name_terms")
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        let qualified_terms = schema
            .get_field("qualified_terms")
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        let path_terms = schema
            .get_field("path_terms")
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        let kind_terms = schema
            .get_field("kind_terms")
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        let stored_id = schema
            .get_field("stored_id")
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        let reader = index
            .reader()
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        Ok(Self {
            dict: None,
            tantivy: Some(TantivyBackend {
                reader,
                name,
                name_terms,
                qualified_terms,
                path_terms,
                kind_terms,
                stored,
                stored_id,
            }),
            term_overlay: None,
            generation_guard: None,
        })
    }

    pub fn with_generation_guard(mut self, guard: crate::generation_gc::GenerationGuard) -> Self {
        self.generation_guard = Some(guard);
        self
    }

    pub(crate) fn with_term_overlays(mut self, overlays: Vec<SearchTermOverlay>) -> Self {
        let mut removed_ids = BTreeSet::new();
        let mut documents = BTreeMap::new();
        let mut snapshot_id = None;
        for overlay in overlays {
            removed_ids.extend(overlay.removed_ids.iter().cloned());
            for id in overlay.removed_ids {
                documents.remove(&id);
            }
            for document in overlay.documents {
                removed_ids.insert(document.id.clone());
                documents.insert(document.id.clone(), document);
            }
            snapshot_id = Some(overlay.snapshot_id);
        }
        self.term_overlay = snapshot_id.map(|snapshot_id| SearchTermOverlay {
            snapshot_id,
            removed_ids: removed_ids.into_iter().collect(),
            added_names: Vec::new(),
            removed_names: Vec::new(),
            documents: documents.into_values().collect(),
        });
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

    pub fn publish_tantivy_snapshot(
        snapshot: &IndexSnapshot,
        dir: &std::path::Path,
    ) -> Result<(), SearchError> {
        use rayon::prelude::*;
        use tantivy::{
            Index, IndexWriter, doc,
            schema::{STORED, STRING, Schema, TEXT},
        };
        std::fs::create_dir_all(dir).map_err(|e| SearchError::Backend(e.to_string()))?;
        let mut builder = Schema::builder();
        let name = builder.add_text_field("name", TEXT);
        let name_terms = builder.add_text_field("name_terms", TEXT | STORED);
        let qualified_terms = builder.add_text_field("qualified_terms", TEXT | STORED);
        let path_terms = builder.add_text_field("path_terms", TEXT | STORED);
        let kind_terms = builder.add_text_field("kind_terms", TEXT | STORED);
        let stored = builder.add_text_field("stored", STORED);
        let stored_id = builder.add_text_field("stored_id", STRING | STORED);
        let schema = builder.build();
        let index =
            Index::create_in_dir(dir, schema).map_err(|e| SearchError::Backend(e.to_string()))?;

        let field_terms = |value: &str| {
            let mut tokens = BTreeSet::new();
            add_search_tokens(&mut tokens, value);
            tokens.into_iter().collect::<Vec<_>>().join(" ")
        };
        // Tokenizing 5 fields per symbol over the whole corpus is the dominant index-time cost
        // and is pure CPU, so prepare every document's field strings in parallel up front. The
        // single-threaded writer producer loop below then only ships already-built strings.
        let prep_start = std::time::Instant::now();
        let prepared: Vec<PreparedSearchDoc> = snapshot
            .files
            .par_iter()
            .flat_map_iter(|(path, artifact)| {
                let path_terms_value = field_terms(path);
                artifact
                    .symbols
                    .iter()
                    .map(move |symbol| PreparedSearchDoc {
                        name: symbol.name.to_lowercase(),
                        name_terms: field_terms(&symbol.name),
                        qualified_terms: field_terms(&symbol.qualified_name),
                        path_terms: path_terms_value.clone(),
                        kind_terms: field_terms(symbol.kind.as_ref()),
                        stored: symbol.name.clone(),
                        stored_id: symbol.id.clone(),
                    })
            })
            .collect();
        crate::timing::stage("search.prep", prep_start, || {
            format!("docs={}", prepared.len())
        });

        // A 15 MB budget forces tantivy to a single indexing thread. Give it one thread per core
        // (capped) with enough per-thread heap so indexing/merge keeps pace with the producer.
        let threads = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1)
            .clamp(1, TANTIVY_MAX_WRITER_THREADS);
        let mut writer: IndexWriter = index
            .writer_with_num_threads(threads, TANTIVY_WRITER_HEAP_PER_THREAD * threads)
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        // Background auto-merge races the explicit final merge below: segment ids collected
        // after commit can already be merged away ("segments ... could not be found in the
        // SegmentManager"), failing the whole publish. Build with merging disabled and do one
        // deterministic merge ourselves.
        writer.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
        // The writer's worker threads (not the producer) are the throughput floor here, so a
        // single producer loop is as fast as feeding from the whole pool and avoids the extra
        // contention. Tokenization already happened in parallel above.
        let add_start = std::time::Instant::now();
        for prepared_doc in prepared {
            writer
                .add_document(doc!(
                    name => prepared_doc.name,
                    name_terms => prepared_doc.name_terms,
                    qualified_terms => prepared_doc.qualified_terms,
                    path_terms => prepared_doc.path_terms,
                    kind_terms => prepared_doc.kind_terms,
                    stored => prepared_doc.stored,
                    stored_id => prepared_doc.stored_id
                ))
                .map_err(|e| SearchError::Backend(e.to_string()))?;
        }
        crate::timing::stage("search.add_docs", add_start, || {
            format!("threads={threads}")
        });
        let commit_start = std::time::Instant::now();
        writer
            .commit()
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        crate::timing::stage("search.commit", commit_start, String::new);
        // Multi-threaded indexing leaves one segment per worker. Merge to a single segment at
        // publish time: every cold CLI query re-opens this index, and per-query segment fan-out
        // (plus tie-order drift across segment layouts) is paid far more often than this one
        // merge.
        let merge_start = std::time::Instant::now();
        let segments = index
            .searchable_segment_ids()
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        if segments.len() > 1 {
            writer
                .merge(&segments)
                .wait()
                .map_err(|e| SearchError::Backend(e.to_string()))?;
        }
        writer
            .wait_merging_threads()
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        crate::timing::stage("search.merge", merge_start, || {
            format!("segments={}", segments.len())
        });
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
        if kind == SearchKind::Terms {
            return self.search_terms(query, limit);
        }
        if let Some(dict) = &self.dict {
            return dict.search(query, kind, limit);
        }
        if let Some(tv) = &self.tantivy {
            return tv.search(query, kind, limit);
        }
        Err(SearchError::Backend("no search backend available".into()))
    }

    fn search_terms(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        let tokens = query_tokens(query);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let mut shadowed_ids = BTreeSet::new();
        let mut hits = Vec::new();
        if let Some(overlay) = &self.term_overlay {
            for document in &overlay.documents {
                if let Some(score_micros) = score_term_fields(
                    &tokens,
                    &document.name_terms,
                    &document.qualified_terms,
                    &document.path_terms,
                    &document.kind_terms,
                ) {
                    hits.push(SearchHit {
                        value: document.name.clone(),
                        definition_id: Some(document.id.clone()),
                        score_micros,
                        reason: Some("term-coverage".into()),
                    });
                }
            }
            shadowed_ids.extend(overlay.removed_ids.iter().cloned());
        }
        if let Some(tantivy) = &self.tantivy {
            hits.extend(tantivy.search_excluding(
                query,
                SearchKind::Terms,
                limit,
                &shadowed_ids,
            )?);
        }
        finish_hits(hits, limit)
    }

    pub fn backend_label(&self) -> &'static str {
        match (&self.dict, &self.tantivy) {
            (Some(_), Some(_)) => "hybrid",
            (Some(_), None) => "dict",
            (None, Some(_)) => "tantivy",
            (None, None) => "none",
        }
    }
}

impl TantivyBackend {
    fn from_dict(dict: &SymbolDict) -> Result<Self, SearchError> {
        use tantivy::{
            Index, IndexWriter, doc,
            schema::{STORED, STRING, Schema, TEXT},
        };
        let mut builder = Schema::builder();
        let name = builder.add_text_field("name", TEXT);
        let name_terms = builder.add_text_field("name_terms", TEXT | STORED);
        let qualified_terms = builder.add_text_field("qualified_terms", TEXT | STORED);
        let path_terms = builder.add_text_field("path_terms", TEXT | STORED);
        let kind_terms = builder.add_text_field("kind_terms", TEXT | STORED);
        let stored = builder.add_text_field("stored", STORED);
        let stored_id = builder.add_text_field("stored_id", STRING | STORED);
        let schema = builder.build();
        let index = Index::create_in_ram(schema);
        let mut writer: IndexWriter = index
            .writer(TANTIVY_WRITER_HEAP_BYTES)
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        if dict.term_documents.is_empty() {
            for (original, search_terms) in dict.names.iter().zip(&dict.terms) {
                writer
                    .add_document(doc!(
                        name => original.to_lowercase(),
                        name_terms => search_terms.to_owned(),
                        qualified_terms => search_terms.to_owned(),
                        path_terms => String::new(),
                        kind_terms => String::new(),
                        stored => original.to_owned(),
                        stored_id => String::new()
                    ))
                    .map_err(|e| SearchError::Backend(e.to_string()))?;
            }
        } else {
            for document in &dict.term_documents {
                writer
                    .add_document(doc!(
                        name => document.name.to_lowercase(),
                        name_terms => document.name_terms.to_owned(),
                        qualified_terms => document.qualified_terms.to_owned(),
                        path_terms => document.path_terms.to_owned(),
                        kind_terms => document.kind_terms.to_owned(),
                        stored => document.name.to_owned(),
                        stored_id => document.id.to_owned()
                    ))
                    .map_err(|e| SearchError::Backend(e.to_string()))?;
            }
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
            name_terms,
            qualified_terms,
            path_terms,
            kind_terms,
            stored,
            stored_id,
        })
    }

    fn search(
        &self,
        query: &str,
        kind: SearchKind,
        limit: usize,
    ) -> Result<Vec<SearchHit>, SearchError> {
        self.search_excluding(query, kind, limit, &BTreeSet::new())
    }

    fn search_excluding(
        &self,
        query: &str,
        kind: SearchKind,
        limit: usize,
        excluded_ids: &BTreeSet<String>,
    ) -> Result<Vec<SearchHit>, SearchError> {
        use tantivy::{
            Term, collector::TopDocs, query::BooleanQuery, query::BoostQuery,
            query::FuzzyTermQuery, query::Occur, query::Query, query::RegexQuery, query::TermQuery,
            schema::IndexRecordOption, schema::Value,
        };
        let searcher = self.reader.searcher();
        let normalized = query.to_lowercase();
        let term_query_tokens = if kind == SearchKind::Terms {
            query_tokens(query)
        } else {
            Vec::new()
        };
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
            SearchKind::Terms => {
                if term_query_tokens.is_empty() {
                    return Ok(Vec::new());
                }
                let fields = [
                    (self.name_terms, 4.0),
                    (self.qualified_terms, 3.0),
                    (self.path_terms, 1.5),
                    (self.kind_terms, 1.0),
                ];
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = term_query_tokens
                    .iter()
                    .flat_map(|token| {
                        fields.into_iter().map(move |(field, boost)| {
                            let query = TermQuery::new(
                                Term::from_field_text(field, token),
                                IndexRecordOption::WithFreqs,
                            );
                            (
                                Occur::Should,
                                Box::new(BoostQuery::new(Box::new(query), boost)) as Box<dyn Query>,
                            )
                        })
                    })
                    .collect();
                clauses.extend(excluded_ids.iter().map(|id| {
                    (
                        Occur::MustNot,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.stored_id, id),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>,
                    )
                }));
                Box::new(BooleanQuery::new(clauses))
            }
        };
        let collect_limit = if kind == SearchKind::Terms {
            limit
                .max(1)
                .saturating_mul(TERM_CANDIDATE_MULTIPLIER)
                .max(TERM_CANDIDATE_FLOOR)
        } else {
            limit
                .max(1)
                .saturating_mul(NAME_CANDIDATE_MULTIPLIER)
                .max(NAME_CANDIDATE_FLOOR)
        };
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
                let definition_id = (kind == SearchKind::Terms)
                    .then(|| {
                        document
                            .get_first(self.stored_id)
                            .and_then(|value| value.as_str())
                            .filter(|value| !value.is_empty())
                            .map(ToOwned::to_owned)
                    })
                    .flatten();
                let term_score = if kind == SearchKind::Terms {
                    let field_text = |field| {
                        document
                            .get_first(field)
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                    };
                    let Some(score) = score_term_fields(
                        &term_query_tokens,
                        field_text(self.name_terms),
                        field_text(self.qualified_terms),
                        field_text(self.path_terms),
                        field_text(self.kind_terms),
                    ) else {
                        continue;
                    };
                    score
                } else {
                    0
                };
                hits.push(SearchHit {
                    value: value.to_owned(),
                    definition_id,
                    score_micros: match kind {
                        SearchKind::Exact if value == query => SCORE_EXACT_CASE,
                        SearchKind::Exact => SCORE_EXACT_CASE_INSENSITIVE,
                        SearchKind::Prefix if value == query => SCORE_EXACT_CASE,
                        SearchKind::Prefix if value.eq_ignore_ascii_case(query) => {
                            SCORE_EXACT_CASE_INSENSITIVE
                        }
                        SearchKind::Prefix if value.starts_with(query) => SCORE_PREFIX_CASE,
                        SearchKind::Prefix => SCORE_PREFIX,
                        SearchKind::Terms => term_score,
                        _ => (score.max(0.0) * 1_000_000.0) as u64,
                    },
                    reason: Some(match kind {
                        SearchKind::Exact if value == query => "exact-case".into(),
                        SearchKind::Exact => "exact-case-insensitive".into(),
                        SearchKind::Prefix if value == query => "exact-case".into(),
                        SearchKind::Prefix if value.eq_ignore_ascii_case(query) => {
                            "exact-case-insensitive".into()
                        }
                        SearchKind::Prefix if value.starts_with(query) => "prefix-case".into(),
                        SearchKind::Prefix => "prefix-case-insensitive".into(),
                        SearchKind::Fuzzy => "fuzzy".into(),
                        SearchKind::Regex => "regex".into(),
                        SearchKind::Terms => "term-coverage".into(),
                    }),
                });
            }
        }
        finish_hits(hits, limit)
    }
}

/// Field strings for one search document, tokenized off the writer thread so the producer loop
/// only ships owned strings into tantivy.
struct PreparedSearchDoc {
    name: String,
    name_terms: String,
    qualified_terms: String,
    path_terms: String,
    kind_terms: String,
    stored: String,
    stored_id: String,
}

fn add_search_tokens(tokens: &mut BTreeSet<String>, text: &str) {
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0usize;
    for index in 0..=chars.len() {
        let boundary = index == chars.len() || !chars[index].is_alphanumeric();
        if boundary {
            if start < index {
                split_identifier_token(&chars[start..index], tokens);
            }
            start = index.saturating_add(1);
        }
    }
}

fn split_identifier_token(chars: &[char], tokens: &mut BTreeSet<String>) {
    if chars.is_empty() {
        return;
    }
    let mut start = 0usize;
    for index in 1..chars.len() {
        let previous = chars[index - 1];
        let current = chars[index];
        let next = chars.get(index + 1).copied();
        let camel_boundary = previous.is_lowercase() && current.is_uppercase();
        let acronym_boundary = previous.is_uppercase()
            && current.is_uppercase()
            && next.is_some_and(char::is_lowercase);
        let digit_boundary = previous.is_ascii_digit() != current.is_ascii_digit();
        if camel_boundary || acronym_boundary || digit_boundary {
            let token: String = chars[start..index]
                .iter()
                .flat_map(|character| character.to_lowercase())
                .collect();
            if !token.is_empty() {
                tokens.insert(token);
            }
            start = index;
        }
    }
    let token: String = chars[start..]
        .iter()
        .flat_map(|character| character.to_lowercase())
        .collect();
    if !token.is_empty() {
        tokens.insert(token);
    }
}

pub(crate) fn query_tokens(query: &str) -> Vec<String> {
    let mut tokens = BTreeSet::new();
    add_search_tokens(&mut tokens, query);
    tokens.retain(|token| token.len() > 1);
    tokens.into_iter().collect()
}

fn score_term_fields(
    query: &[String],
    name_terms: &str,
    qualified_terms: &str,
    path_terms: &str,
    kind_terms: &str,
) -> Option<u64> {
    fn field_tokens(value: &str) -> BTreeSet<&str> {
        value.split_whitespace().collect()
    }
    let name_tokens = field_tokens(name_terms);
    let qualified_tokens = field_tokens(qualified_terms);
    let path_tokens = field_tokens(path_terms);
    let kind_tokens = field_tokens(kind_terms);
    let count_matches = |available: &BTreeSet<&str>| {
        query
            .iter()
            .filter(|token| available.contains(String::as_str(token)))
            .count() as u64
    };
    let name_matches = count_matches(&name_tokens);
    let qualified_matches = count_matches(&qualified_tokens);
    let path_matches = count_matches(&path_tokens);
    let kind_matches = count_matches(&kind_tokens);
    let matched = query
        .iter()
        .filter(|token| {
            let token = String::as_str(token);
            name_tokens.contains(token)
                || qualified_tokens.contains(token)
                || path_tokens.contains(token)
                || kind_tokens.contains(token)
        })
        .count() as u64;
    if matched == 0 {
        return None;
    }
    let full_coverage = (matched as usize == query.len()) as u64;
    Some(
        (600_000
            + matched.min(6) * 35_000
            + name_matches.min(4) * 25_000
            + qualified_matches.min(4) * 10_000
            + path_matches.min(4) * 3_000
            + kind_matches.min(2) * 1_000
            + full_coverage * 20_000)
            .min(950_000),
    )
}

pub(crate) fn symbol_meta_matches_query_tokens(
    required: &[String],
    symbol: &crate::model::SymbolMeta,
) -> bool {
    if required.is_empty() {
        return false;
    }
    let mut available = BTreeSet::new();
    add_search_tokens(&mut available, &symbol.name);
    add_search_tokens(&mut available, &symbol.qualified_name);
    add_search_tokens(&mut available, &symbol.path);
    add_search_tokens(&mut available, symbol.kind.as_ref());
    required.iter().any(|term| available.contains(term))
}

fn finish_hits(mut hits: Vec<SearchHit>, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
    let limit = limit.min(hits.len());
    if limit == 0 {
        return Ok(Vec::new());
    }
    // Partial sort: use select_nth_unstable_by to partition top-N in O(n), then sort only
    // top-N in O(N log N) instead of O(n log n) for the full sort.
    // This benefits regex and term search where n >> limit.
    let cmp = |left: &SearchHit, right: &SearchHit| {
        right
            .score_micros
            .cmp(&left.score_micros)
            .then_with(|| left.value.cmp(&right.value))
            .then_with(|| left.definition_id.cmp(&right.definition_id))
    };
    if hits.len() > limit {
        hits.select_nth_unstable_by(limit - 1, cmp);
        hits.truncate(limit);
    }
    hits.sort_by(cmp);
    // Backends index unique symbol spellings. Keep this defensive dedup for malformed indexes;
    // equal values also have equal deterministic scores and are adjacent.
    hits.dedup_by(|a, b| a.value == b.value && a.definition_id == b.definition_id);
    Ok(hits)
}

fn levenshtein_at_most(a: &[char], b: &str, max_dist: usize) -> Option<usize> {
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();
    if n.abs_diff(m) > max_dist {
        return None;
    }
    // Only the diagonal band within `max_dist` can contribute to an accepted result. Scanning
    // the full N×M matrix made fuzzy matching quadratic in identifier length even though the
    // product only accepts a distance of two.
    let outside_band = max_dist.saturating_add(1);
    let mut prev = vec![outside_band; m + 1];
    for (column, value) in prev.iter_mut().enumerate().take(m.min(max_dist) + 1) {
        *value = column;
    }
    let mut curr = vec![outside_band; m + 1];
    for i in 1..=n {
        curr[0] = if i <= max_dist { i } else { outside_band };
        let start = i.saturating_sub(max_dist).max(1);
        let end = i.saturating_add(max_dist).min(m);
        if start > 1 {
            curr[start - 1] = outside_band;
        }
        let mut row_min = curr[0];
        for j in start..=end {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = prev[j]
                .saturating_add(1)
                .min(curr[j - 1].saturating_add(1))
                .min(prev[j - 1].saturating_add(cost));
            row_min = row_min.min(curr[j]);
        }
        if end < m {
            curr[end + 1] = outside_band;
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
                id: String::new(),
                name: (*name).into(),
                qualified_name: (*name).into(),
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
                scope: None,
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
                    id: String::new(),
                    name: "Foo".into(),
                    qualified_name: "Foo".into(),
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
                    scope: None,
                }],
                ..artifact
            },
        );
        let dict = SymbolDict::from_snapshot(&snap);
        assert_eq!(dict.names.len(), 2);
        let index = SearchIndex::from_symbol_dict(dict);
        let hits = index.search("foo", SearchKind::Exact, 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].value, "foo");
        assert_eq!(hits[0].reason.as_deref(), Some("exact-case"));
        let upper = index.search("Foo", SearchKind::Exact, 10).unwrap();
        assert_eq!(upper[0].value, "Foo");
    }

    #[test]
    fn term_search_uses_segmented_names_paths_and_kinds_without_workspace_scan() {
        let artifact = parse_source(
            "apps/users/src/application/usecases/onboarding/get_pending_legal_person_onboarding.usecase.ts",
            b"export class GetPendingLegalPersonOnboardingUseCase {}",
        );
        let snapshot = IndexSnapshot {
            id: SnapshotId {
                root: "r".into(),
                worktree: "w".into(),
                revision: "v".into(),
                content_state: "c".into(),
                schema_version: 3,
                grammar_version: "g".into(),
                config_hash: "h".into(),
            },
            files: [(artifact.path.clone(), artifact)].into(),
            edges: Vec::new(),
        };
        let index = SearchIndex::from_snapshot(&snapshot).unwrap();
        let hits = index
            .search(
                "select users onboarding pending legal person class",
                SearchKind::Terms,
                10,
            )
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].value, "GetPendingLegalPersonOnboardingUseCase");
        assert_eq!(hits[0].reason.as_deref(), Some("term-coverage"));
        assert!(hits[0].definition_id.is_some());
    }

    #[test]
    fn term_search_never_combines_evidence_from_distinct_homonyms() {
        let pending = parse_source("pending/worker.ts", b"export class A { execute() {} }");
        let legal = parse_source("legal/worker.ts", b"export class B { execute() {} }");
        let valid = parse_source("worker.ts", b"export class PendingLegalWorker {}");
        let snapshot = IndexSnapshot {
            id: SnapshotId {
                root: "r".into(),
                worktree: "w".into(),
                revision: "v".into(),
                content_state: "c".into(),
                schema_version: 4,
                grammar_version: "g".into(),
                config_hash: "h".into(),
            },
            files: [pending, legal, valid]
                .into_iter()
                .map(|artifact| (artifact.path.clone(), artifact))
                .collect(),
            edges: Vec::new(),
        };
        let hits = SearchIndex::from_snapshot(&snapshot)
            .unwrap()
            .search("encontre pending legal", SearchKind::Terms, 1)
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].value, "PendingLegalWorker");
    }

    #[test]
    fn term_search_does_not_drop_evidence_after_many_intent_tokens() {
        let artifact = parse_source("worker.ts", b"export class NeedleWorker {}");
        let snapshot = IndexSnapshot {
            id: SnapshotId {
                root: "r".into(),
                worktree: "w".into(),
                revision: "v".into(),
                content_state: "c".into(),
                schema_version: 5,
                grammar_version: "g".into(),
                config_hash: "h".into(),
            },
            files: [(artifact.path.clone(), artifact)].into(),
            edges: Vec::new(),
        };
        let mut tokens = (0..65)
            .map(|index| format!("aaa{index}"))
            .collect::<Vec<_>>();
        tokens.push("needle".into());

        let hits = SearchIndex::from_snapshot(&snapshot)
            .unwrap()
            .search(&tokens.join(" "), SearchKind::Terms, 10)
            .unwrap();
        assert_eq!(hits[0].value, "NeedleWorker");
    }

    #[test]
    fn identifier_tokenization_handles_acronyms_snake_case_and_digits() {
        let mut tokens = BTreeSet::new();
        add_search_tokens(&mut tokens, "HTTP2Server_getByUser");
        assert_eq!(
            tokens,
            ["2", "by", "get", "http", "server", "user"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
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

    /// Benchmark: partial sort O(n + L·log(L)) vs full sort O(n·log(n)) in finish_hits.
    /// Directly compares the two approaches on identical synthetic data.
    /// Run with `--release -- --nocapture` for meaningful numbers.
    #[cfg_attr(debug_assertions, ignore)]
    #[test]
    fn finish_hits_partial_sort_outperforms_full_sort() {
        // Generate 50k hits with pseudo-random scores (deterministic, no rand crate needed).
        let n = 50_000usize;
        let limit = 20usize;
        let hits: Vec<SearchHit> = (0..n)
            .map(|i| {
                // Use a simple LCG for pseudo-random scores.
                let score = (i
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407)
                    % 1_000_000) as u64;
                SearchHit {
                    value: format!("sym{i:08}"),
                    definition_id: None,
                    score_micros: score,
                    reason: Some("bench".into()),
                }
            })
            .collect();

        // Benchmark: partial sort (current implementation with select_nth_unstable_by).
        let t0 = Instant::now();
        for _ in 0..5 {
            let _ = super::finish_hits(hits.clone(), limit).unwrap();
        }
        let partial_us = t0.elapsed().as_secs_f64() * 1_000_000.0 / 5.0;

        // Benchmark: full sort (original approach: sort everything, then truncate).
        let t1 = Instant::now();
        for _ in 0..5 {
            let mut h = hits.clone();
            h.sort_by(|left, right| {
                right
                    .score_micros
                    .cmp(&left.score_micros)
                    .then_with(|| left.value.cmp(&right.value))
                    .then_with(|| left.definition_id.cmp(&right.definition_id))
            });
            h.dedup_by(|a, b| a.value == b.value && a.definition_id == b.definition_id);
            h.truncate(limit);
            let _: Vec<SearchHit> = h;
        }
        let full_us = t1.elapsed().as_secs_f64() * 1_000_000.0 / 5.0;

        eprintln!(
            "finish_hits n={n} limit={limit} partial_us={partial_us:.1} full_us={full_us:.1} speedup={:.1}x",
            full_us / partial_us.max(1.0)
        );
        // Partial sort must be faster because O(n + L·log(L)) < O(n·log(n)) when n >> limit.
        assert!(
            partial_us < full_us,
            "partial sort ({partial_us:.1}µs) should be faster than full sort ({full_us:.1}µs)"
        );

        // Correctness check: both approaches produce identical top-N results.
        let result_partial = super::finish_hits(hits.clone(), limit).unwrap();
        let mut result_full = hits.clone();
        result_full.sort_by(|left, right| {
            right
                .score_micros
                .cmp(&left.score_micros)
                .then_with(|| left.value.cmp(&right.value))
                .then_with(|| left.definition_id.cmp(&right.definition_id))
        });
        result_full.dedup_by(|a, b| a.value == b.value && a.definition_id == b.definition_id);
        result_full.truncate(limit);
        assert_eq!(
            result_partial, result_full,
            "partial and full sort must produce identical results"
        );
    }
}
