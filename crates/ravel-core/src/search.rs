use crate::generation_pack::GenerationPackReader;
use crate::model::IndexSnapshot;
use rustc_hash::FxHashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use thiserror::Error;

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
#[derive(
    Debug,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
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
}

#[derive(
    Debug,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub(crate) struct TermIndex {
    pub(crate) format_version: u32,
    pub(crate) snapshot_id: String,
    /// Definition-level documents prevent terms from different homonyms satisfying one query.
    documents: Vec<TermDocument>,
    /// Sorted token dictionary for definition-level term search.
    term_tokens: Vec<String>,
    /// Parallel postings into `term_documents`, sorted and deduplicated per token.
    term_postings: Vec<Vec<TermPosting>>,
}

#[derive(
    Debug,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
struct TermDocument {
    id: String,
    name: String,
}

#[derive(
    Debug,
    Clone,
    Copy,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
struct TermPosting {
    document_index: u32,
    fields: u8,
}

#[derive(
    Debug,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub(crate) struct SymbolTermDocument {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) name_terms: String,
    pub(crate) qualified_terms: String,
    pub(crate) path_terms: String,
    pub(crate) kind_terms: String,
    pub(crate) degree: u32,
}

impl SymbolTermDocument {
    pub(crate) fn from_symbol(path: &str, symbol: &crate::model::Symbol, degree: u32) -> Self {
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
            degree,
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

impl SearchTermOverlay {
    pub(crate) fn compose(overlays: impl IntoIterator<Item = Self>) -> Option<Self> {
        let mut removed_ids = BTreeSet::new();
        let mut documents = BTreeMap::new();
        let mut added_names = BTreeSet::new();
        let mut removed_names = BTreeSet::new();
        let mut snapshot_id = None;
        for overlay in overlays {
            for name in overlay.removed_names {
                added_names.remove(&name);
                removed_names.insert(name);
            }
            for name in overlay.added_names {
                removed_names.remove(&name);
                added_names.insert(name);
            }
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
        snapshot_id.map(|snapshot_id| Self {
            snapshot_id,
            removed_ids: removed_ids.into_iter().collect(),
            added_names: added_names.into_iter().collect(),
            removed_names: removed_names.into_iter().collect(),
            documents: documents.into_values().collect(),
        })
    }
}

impl SymbolDict {
    pub const FORMAT_VERSION: u32 = 6;

    pub fn from_snapshot(snapshot: &IndexSnapshot) -> Self {
        Self::from_snapshot_names_only(snapshot)
    }

    /// Prefix/exact dictionary for persisted readers.
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
        }
    }

    pub(crate) fn is_well_formed(&self) -> bool {
        self.lower.len() == self.names.len() && self.terms.len() == self.names.len()
    }
}

impl TermIndex {
    pub(crate) const FORMAT_VERSION: u32 = 3;

    pub(crate) fn from_snapshot(snapshot: &IndexSnapshot) -> Self {
        let mut term_documents = Vec::new();
        for (path, artifact) in &snapshot.files {
            for symbol in &artifact.symbols {
                let document = SymbolTermDocument::from_symbol(path, symbol, 0);
                term_documents.push(document);
            }
        }
        term_documents.sort_by(|left, right| (&left.name, &left.id).cmp(&(&right.name, &right.id)));
        let mut postings: BTreeMap<String, Vec<TermPosting>> = BTreeMap::new();
        for (document_index, document) in term_documents.iter().enumerate() {
            let mut document_tokens = BTreeMap::<&str, u8>::new();
            for (terms, field) in [
                (&document.name_terms, 1),
                (&document.qualified_terms, 2),
                (&document.path_terms, 4),
                (&document.kind_terms, 8),
            ] {
                for token in terms.split_whitespace() {
                    *document_tokens.entry(token).or_default() |= field;
                }
            }
            for (token, fields) in document_tokens {
                postings
                    .entry(token.to_owned())
                    .or_default()
                    .push(TermPosting {
                        document_index: document_index as u32,
                        fields,
                    });
            }
        }
        Self {
            format_version: Self::FORMAT_VERSION,
            snapshot_id: snapshot.id.stable_key(),
            documents: term_documents
                .into_iter()
                .map(|document| TermDocument {
                    id: document.id,
                    name: document.name,
                })
                .collect(),
            term_tokens: postings.keys().cloned().collect(),
            term_postings: postings.into_values().collect(),
        }
    }

    pub(crate) fn is_well_formed(&self) -> bool {
        self.term_tokens.len() == self.term_postings.len()
            && self.term_tokens.windows(2).all(|pair| pair[0] < pair[1])
            && self.term_postings.iter().all(|posting| {
                posting
                    .windows(2)
                    .all(|pair| pair[0].document_index < pair[1].document_index)
                    && posting
                        .last()
                        .is_none_or(|entry| (entry.document_index as usize) < self.documents.len())
            })
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
enum DictRuntime {
    Owned {
        dict: SymbolDict,
        /// length → indices for fuzzy pruning.
        by_len: FxHashMap<u16, Vec<u32>>,
    },
    Packed {
        reader: Arc<GenerationPackReader>,
        key: String,
        max_bytes: u64,
    },
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
        Self::Owned { dict, by_len }
    }

    fn search(
        &self,
        query: &str,
        kind: SearchKind,
        limit: usize,
    ) -> Result<Vec<SearchHit>, SearchError> {
        if let Self::Packed {
            reader,
            key,
            max_bytes,
        } = self
        {
            let result = reader
                .with_record_for_validation(key, *max_bytes, |bytes| {
                    let dict = rkyv::access::<ArchivedSymbolDict, rkyv::rancor::Error>(bytes)
                        .map_err(|error| SearchError::Backend(error.to_string()))?;
                    search_archived_dict(dict, query, kind, limit)
                })
                .map_err(|error| SearchError::Backend(error.to_string()))?
                .ok_or_else(|| SearchError::Backend("missing packed symbol dictionary".into()))?;
            return result;
        }
        let Self::Owned { dict, by_len } = self else {
            unreachable!()
        };
        let normalized = query.to_lowercase();
        let mut hits = Vec::new();
        match kind {
            SearchKind::Exact => {
                // `lower` is already sorted by (lower, original): binary-search the equal range
                // instead of building an O(N) HashMap that cloned every lowercase name.
                let lower = &dict.lower;
                let start = lower.partition_point(|s| s.as_str() < normalized.as_str());
                for (i, candidate) in lower.iter().enumerate().skip(start) {
                    if candidate != &normalized {
                        break;
                    }
                    hits.push(SearchHit {
                        value: dict.names[i].clone(),
                        definition_id: None,
                        score_micros: if dict.names[i] == query {
                            SCORE_EXACT_CASE
                        } else {
                            SCORE_EXACT_CASE_INSENSITIVE
                        },
                        reason: Some(
                            if dict.names[i] == query {
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
                let lower = &dict.lower;
                let start = lower.partition_point(|s| s.as_str() < normalized.as_str());
                for (i, cand) in lower.iter().enumerate().skip(start) {
                    if !cand.starts_with(&normalized) {
                        break;
                    }
                    hits.push(SearchHit {
                        value: dict.names[i].clone(),
                        definition_id: None,
                        score_micros: if dict.names[i] == query {
                            SCORE_EXACT_CASE
                        } else if dict.names[i].eq_ignore_ascii_case(query) {
                            SCORE_EXACT_CASE_INSENSITIVE
                        } else if dict.names[i].starts_with(query) {
                            SCORE_PREFIX_CASE
                        } else {
                            SCORE_PREFIX
                        },
                        reason: Some(
                            if dict.names[i] == query {
                                "exact-case"
                            } else if dict.names[i].eq_ignore_ascii_case(query) {
                                "exact-case-insensitive"
                            } else if dict.names[i].starts_with(query) {
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
                    if let Some(idxs) = by_len.get(&key) {
                        for &i in idxs {
                            let low = &dict.lower[i as usize];
                            if let Some(distance) = levenshtein_at_most(&q, low, FUZZY_MAX_DISTANCE)
                            {
                                hits.push(SearchHit {
                                    value: dict.names[i as usize].clone(),
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
                for name in dict.names.iter() {
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

fn search_archived_dict(
    dict: &ArchivedSymbolDict,
    query: &str,
    kind: SearchKind,
    limit: usize,
) -> Result<Vec<SearchHit>, SearchError> {
    if !matches!(kind, SearchKind::Exact | SearchKind::Prefix) {
        let owned = rkyv::deserialize::<SymbolDict, rkyv::rancor::Error>(dict)
            .map_err(|error| SearchError::Backend(error.to_string()))?;
        return DictRuntime::build(owned).search(query, kind, limit);
    }
    let normalized = query.to_lowercase();
    let mut low = 0usize;
    let mut high = dict.lower.len();
    while low < high {
        let mid = low + (high - low) / 2;
        if dict.lower[mid].as_str() < normalized.as_str() {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    let mut hits = Vec::new();
    for index in low..dict.lower.len() {
        let candidate = dict.lower[index].as_str();
        let matches = match kind {
            SearchKind::Exact => candidate == normalized,
            SearchKind::Prefix => candidate.starts_with(&normalized),
            _ => unreachable!(),
        };
        if !matches {
            break;
        }
        let name = dict.names[index].as_str();
        let (score_micros, reason) = if name == query {
            (SCORE_EXACT_CASE, "exact-case")
        } else if name.eq_ignore_ascii_case(query) {
            (SCORE_EXACT_CASE_INSENSITIVE, "exact-case-insensitive")
        } else if name.starts_with(query) {
            (SCORE_PREFIX_CASE, "prefix-case")
        } else {
            (SCORE_PREFIX, "prefix-case-insensitive")
        };
        hits.push(SearchHit {
            value: name.to_owned(),
            definition_id: None,
            score_micros,
            reason: Some(reason.into()),
        });
    }
    finish_hits(hits, limit)
}

enum TermRuntime {
    Owned(TermIndex),
    Packed {
        reader: Arc<GenerationPackReader>,
        key: String,
        max_bytes: u64,
    },
}

impl TermRuntime {
    fn search(
        &self,
        tokens: &[String],
        excluded_ids: &BTreeSet<String>,
    ) -> Result<Vec<SearchHit>, SearchError> {
        match self {
            Self::Owned(index) => Ok(search_owned_terms(index, tokens, excluded_ids)),
            Self::Packed {
                reader,
                key,
                max_bytes,
            } => reader
                .with_record_for_validation(key, *max_bytes, |bytes| {
                    let archived = rkyv::access::<ArchivedTermIndex, rkyv::rancor::Error>(bytes)
                        .map_err(|error| SearchError::Backend(error.to_string()))?;
                    Ok(search_archived_terms(archived, tokens, excluded_ids))
                })
                .map_err(|error| SearchError::Backend(error.to_string()))?
                .ok_or_else(|| SearchError::Backend("missing packed term index".into()))?,
        }
    }
}

fn search_owned_terms(
    index: &TermIndex,
    tokens: &[String],
    excluded_ids: &BTreeSet<String>,
) -> Vec<SearchHit> {
    let mut candidates: FxHashMap<u32, [u64; 5]> = FxHashMap::default();
    for token in tokens {
        if let Ok(token_index) = index
            .term_tokens
            .binary_search_by(|candidate| candidate.as_str().cmp(token))
        {
            for posting in &index.term_postings[token_index] {
                accumulate_posting(
                    candidates.entry(posting.document_index).or_default(),
                    posting.fields,
                );
            }
        }
    }
    let mut hits = Vec::with_capacity(candidates.len());
    for (document_index, counts) in candidates {
        // Postings are only guaranteed in-bounds when the index passed
        // `is_well_formed`; skip a stale/corrupt posting rather than panic.
        let Some(document) = index.documents.get(document_index as usize) else {
            continue;
        };
        if excluded_ids.contains(&document.id) {
            continue;
        }
        if let Some(score_micros) = score_term_counts(tokens.len(), counts) {
            hits.push(SearchHit {
                value: document.name.clone(),
                definition_id: Some(document.id.clone()),
                score_micros,
                reason: Some("term-coverage".into()),
            });
        }
    }
    hits
}

fn search_archived_terms(
    index: &ArchivedTermIndex,
    tokens: &[String],
    excluded_ids: &BTreeSet<String>,
) -> Vec<SearchHit> {
    let mut candidates: FxHashMap<u32, [u64; 5]> = FxHashMap::default();
    for token in tokens {
        if let Ok(token_index) = index
            .term_tokens
            .binary_search_by(|candidate| candidate.as_str().cmp(token))
        {
            for posting in index.term_postings[token_index].iter() {
                accumulate_posting(
                    candidates
                        .entry(posting.document_index.to_native())
                        .or_default(),
                    posting.fields,
                );
            }
        }
    }
    let mut hits = Vec::with_capacity(candidates.len());
    for (document_index, counts) in candidates {
        // Archived postings skip the `is_well_formed` gate (only structural
        // rkyv validation runs), so bound-check before indexing an mmap slice.
        let Some(document) = index.documents.get(document_index as usize) else {
            continue;
        };
        if excluded_ids.contains(document.id.as_str()) {
            continue;
        }
        if let Some(score_micros) = score_term_counts(tokens.len(), counts) {
            hits.push(SearchHit {
                value: document.name.as_str().to_owned(),
                definition_id: Some(document.id.as_str().to_owned()),
                score_micros,
                reason: Some("term-coverage".into()),
            });
        }
    }
    hits
}
/// Persistent dictionary plus a compact definition-level inverted term index.
pub struct SearchIndex {
    dict: Option<DictRuntime>,
    terms: Option<TermRuntime>,
    term_overlay: Option<SearchTermOverlay>,
    /// Keeps on-disk generation files alive for the full reader lifetime.
    generation_guard: Option<crate::generation_gc::GenerationGuard>,
}

impl std::fmt::Debug for SearchIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchIndex")
            .field(
                "dict_names",
                &self.dict.as_ref().map(|dict| match dict {
                    DictRuntime::Owned { dict, .. } => dict.names.len(),
                    DictRuntime::Packed { .. } => 0,
                }),
            )
            .field(
                "term_documents",
                &self.terms.as_ref().map(|terms| match terms {
                    TermRuntime::Owned(index) => index.documents.len(),
                    TermRuntime::Packed { .. } => 0,
                }),
            )
            .field("has_generation_guard", &self.generation_guard.is_some())
            .finish()
    }
}

impl SearchIndex {
    pub fn from_symbol_dict(dict: SymbolDict) -> Self {
        Self {
            dict: Some(DictRuntime::build(dict)),
            terms: None,
            term_overlay: None,
            generation_guard: None,
        }
    }

    pub(crate) fn from_packed_dict(
        reader: Arc<GenerationPackReader>,
        key: String,
        max_bytes: u64,
    ) -> Self {
        Self {
            dict: Some(DictRuntime::Packed {
                reader,
                key,
                max_bytes,
            }),
            terms: None,
            term_overlay: None,
            generation_guard: None,
        }
    }

    pub fn from_snapshot(snapshot: &IndexSnapshot) -> Result<Self, SearchError> {
        Ok(Self::from_parts(
            SymbolDict::from_snapshot(snapshot),
            Some(TermIndex::from_snapshot(snapshot)),
        ))
    }

    pub(crate) fn from_parts(dict: SymbolDict, terms: Option<TermIndex>) -> Self {
        Self {
            dict: Some(DictRuntime::build(dict)),
            terms: terms.map(TermRuntime::Owned),
            term_overlay: None,
            generation_guard: None,
        }
    }

    pub fn with_generation_guard(mut self, guard: crate::generation_gc::GenerationGuard) -> Self {
        self.generation_guard = Some(guard);
        self
    }

    pub(crate) fn with_term_overlays(mut self, overlays: Vec<SearchTermOverlay>) -> Self {
        self.term_overlay = SearchTermOverlay::compose(overlays);
        self
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
        let dict = self
            .dict
            .as_ref()
            .ok_or_else(|| SearchError::Backend("no search backend available".into()))?;
        let Some(overlay) = &self.term_overlay else {
            return dict.search(query, kind, limit);
        };
        let expanded_limit = limit
            .saturating_add(overlay.removed_names.len())
            .saturating_add(overlay.added_names.len());
        let removed: BTreeSet<_> = overlay.removed_names.iter().map(String::as_str).collect();
        let mut hits = dict.search(query, kind, expanded_limit)?;
        hits.retain(|hit| !removed.contains(hit.value.as_str()));
        if !overlay.added_names.is_empty() {
            let added = DictRuntime::build(SymbolDict::from_names(
                overlay.added_names.clone(),
                overlay.snapshot_id.clone(),
            ));
            hits.extend(added.search(query, kind, expanded_limit)?);
        }
        finish_hits(hits, limit)
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
        if let Some(terms) = &self.terms {
            hits.extend(terms.search(&tokens, &shadowed_ids)?);
        }
        finish_hits(hits, limit)
    }

    pub fn backend_label(&self) -> &'static str {
        self.dict.as_ref().map_or("none", |_| "dict")
    }

    pub fn has_terms(&self) -> bool {
        self.terms.is_some()
    }

    pub(crate) fn with_packed_terms(
        mut self,
        reader: Arc<GenerationPackReader>,
        key: String,
        max_bytes: u64,
    ) -> Self {
        self.terms = Some(TermRuntime::Packed {
            reader,
            key,
            max_bytes,
        });
        self
    }
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
    let contains = |field: &str, token: &str| field.split_whitespace().any(|item| item == token);
    let count_matches =
        |field: &str| query.iter().filter(|token| contains(field, token)).count() as u64;
    let name_matches = count_matches(name_terms);
    let qualified_matches = count_matches(qualified_terms);
    let path_matches = count_matches(path_terms);
    let kind_matches = count_matches(kind_terms);
    let matched = query
        .iter()
        .filter(|token| {
            let token = String::as_str(token);
            contains(name_terms, token)
                || contains(qualified_terms, token)
                || contains(path_terms, token)
                || contains(kind_terms, token)
        })
        .count() as u64;
    score_term_counts(
        query.len(),
        [
            matched,
            name_matches,
            qualified_matches,
            path_matches,
            kind_matches,
        ],
    )
}

fn accumulate_posting(counts: &mut [u64; 5], fields: u8) {
    counts[0] += 1;
    counts[1] += u64::from(fields & 1 != 0);
    counts[2] += u64::from(fields & 2 != 0);
    counts[3] += u64::from(fields & 4 != 0);
    counts[4] += u64::from(fields & 8 != 0);
}

fn score_term_counts(query_len: usize, counts: [u64; 5]) -> Option<u64> {
    let [
        matched,
        name_matches,
        qualified_matches,
        path_matches,
        kind_matches,
    ] = counts;
    if matched == 0 {
        return None;
    }
    let full_coverage = (matched as usize == query_len) as u64;
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
