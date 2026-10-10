use crate::generation_pack::GenerationPackReader;
use crate::model::IndexSnapshot;
use rustc_hash::FxHashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;

/// A match on a symbol's exact id or fully qualified name outranks every scored search hit.
/// `explore` deliberately lists those first, so this has to sit above the ceiling below or the
/// reported `score` contradicts the order the candidates arrive in.
pub(crate) const SCORE_EXACT_IDENTITY: u64 = 1_400_000;
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
        use rayon::prelude::*;
        /// Documents per parallel chunk when inverting the postings. Small enough to
        /// keep every worker's temporary map cheap, large enough to amortize the merge.
        const POSTING_CHUNK: usize = 4_096;

        /// One symbol awaiting inversion, borrowed from the snapshot. Its path and kind are
        /// shared by many symbols, so their tokens are computed once (per file, per kind) and
        /// referenced by index instead of being re-tokenized and stored for every symbol.
        struct Pending<'a> {
            symbol: &'a crate::model::Symbol,
            file: u32,
        }

        let files: Vec<(&String, &crate::model::FileArtifact)> = snapshot.files.iter().collect();
        let path_tokens: Vec<BTreeSet<String>> = files
            .par_iter()
            .map(|(path, _)| search_tokens(path))
            .collect();
        let kind_tokens: BTreeMap<&str, BTreeSet<String>> = files
            .iter()
            .flat_map(|(_, artifact)| artifact.symbols.iter().map(|symbol| symbol.kind.as_ref()))
            .collect::<BTreeSet<&str>>()
            .into_iter()
            .map(|kind| (kind, search_tokens(kind)))
            .collect();
        // Flattened in BTreeMap order, then the same stable sort as before: ties keep insertion
        // order, so every document lands at the index the previous construction gave it.
        let mut pending: Vec<Pending<'_>> = files
            .iter()
            .enumerate()
            .flat_map(|(file, (_, artifact))| {
                artifact.symbols.iter().map(move |symbol| Pending {
                    symbol,
                    file: file as u32,
                })
            })
            .collect();
        pending.par_sort_by(|left, right| {
            (&left.symbol.name, &left.symbol.id).cmp(&(&right.symbol.name, &right.symbol.id))
        });

        // Invert in document-index order per chunk, then merge chunks in order. Each
        // token's postings therefore stay ascending by document_index, which
        // `is_well_formed` requires and the binary-search readers rely on.
        let chunks: Vec<BTreeMap<String, Vec<TermPosting>>> = pending
            .par_chunks(POSTING_CHUNK)
            .enumerate()
            .map(|(chunk_index, documents)| {
                let base = chunk_index * POSTING_CHUNK;
                let mut chunk_postings: BTreeMap<String, Vec<TermPosting>> = BTreeMap::new();
                for (offset, document) in documents.iter().enumerate() {
                    let name = search_tokens(&document.symbol.name);
                    let qualified = search_tokens(&document.symbol.qualified_name);
                    let mut document_tokens = BTreeMap::<&str, u8>::new();
                    for (tokens, field) in [
                        (&name, 1),
                        (&qualified, 2),
                        (&path_tokens[document.file as usize], 4),
                        (&kind_tokens[document.symbol.kind.as_ref()], 8),
                    ] {
                        for token in tokens {
                            *document_tokens.entry(token).or_default() |= field;
                        }
                    }
                    let posting_index = (base + offset) as u32;
                    for (token, fields) in document_tokens {
                        let posting = TermPosting {
                            document_index: posting_index,
                            fields,
                        };
                        match chunk_postings.get_mut(token) {
                            Some(postings) => postings.push(posting),
                            None => {
                                chunk_postings.insert(token.to_owned(), vec![posting]);
                            }
                        }
                    }
                }
                chunk_postings
            })
            .collect();
        let mut postings: BTreeMap<String, Vec<TermPosting>> = BTreeMap::new();
        for chunk_postings in chunks {
            for (token, entries) in chunk_postings {
                match postings.get_mut(&token) {
                    Some(existing) => existing.extend(entries),
                    None => {
                        postings.insert(token, entries);
                    }
                }
            }
        }
        let (term_tokens, term_postings): (Vec<String>, Vec<Vec<TermPosting>>) =
            postings.into_iter().unzip();

        Self {
            format_version: Self::FORMAT_VERSION,
            snapshot_id: snapshot.id.stable_key(),
            documents: pending
                .into_iter()
                .map(|document| TermDocument {
                    id: document.symbol.id.clone(),
                    name: document.symbol.name.clone(),
                })
                .collect(),
            term_tokens,
            term_postings,
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
        /// Set once `rkyv::access` has validated this record in this process.
        validated: AtomicBool,
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
            validated,
        } = self
        {
            let result = reader
                .with_record_for_validation(key, *max_bytes, |bytes| {
                    let dict = access_validated_once::<ArchivedSymbolDict>(bytes, validated)?;
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
                    // Equal up to case by the same folding the match used, so `École` is an
                    // exact hit for `école`; ASCII-only folding scored it as a mere prefix.
                    hits.push(SearchHit {
                        value: dict.names[i].clone(),
                        definition_id: None,
                        score_micros: if dict.names[i] == query {
                            SCORE_EXACT_CASE
                        } else if *cand == normalized {
                            SCORE_EXACT_CASE_INSENSITIVE
                        } else if dict.names[i].starts_with(query) {
                            SCORE_PREFIX_CASE
                        } else {
                            SCORE_PREFIX
                        },
                        reason: Some(
                            if dict.names[i] == query {
                                "exact-case"
                            } else if *cand == normalized {
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
        } else if candidate == normalized {
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
        /// Set once `rkyv::access` has validated this record in this process.
        validated: AtomicBool,
    },
}

impl TermRuntime {
    fn search(
        &self,
        tokens: &[String],
        excluded_ids: &BTreeSet<String>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, SearchError> {
        match self {
            Self::Owned(index) => Ok(search_owned_terms(index, tokens, excluded_ids, limit)),
            Self::Packed {
                reader,
                key,
                max_bytes,
                validated,
            } => reader
                .with_record_for_validation(key, *max_bytes, |bytes| {
                    let archived = access_validated_once::<ArchivedTermIndex>(bytes, validated)?;
                    Ok(search_archived_terms(archived, tokens, excluded_ids, limit))
                })
                .map_err(|error| SearchError::Backend(error.to_string()))?
                .ok_or_else(|| SearchError::Backend("missing packed term index".into()))?,
        }
    }
}

/// Validate a packed archive on first access, then borrow it directly.
///
/// `rkyv::access` walks the whole archive to check it, which costs time
/// proportional to the record — over 100MB for the term index on a large
/// workspace, paid on every single query. The bytes cannot change underneath
/// us: generation packs are immutable once published, a new generation is a
/// new file, and observing one drops the runtime holding this flag. So the
/// check is worth exactly as much the first time as the thousandth.
fn access_validated_once<'bytes, T>(
    bytes: &'bytes [u8],
    validated: &AtomicBool,
) -> Result<&'bytes T, SearchError>
where
    T: rkyv::Portable
        + for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, rkyv::rancor::Error>>,
{
    if validated.load(Ordering::Acquire) {
        // SAFETY: these exact bytes passed `rkyv::access` earlier in this
        // process, and the record they come from is immutable for the lifetime
        // of this runtime (see above).
        return Ok(unsafe { rkyv::access_unchecked::<T>(bytes) });
    }
    let archived = rkyv::access::<T, rkyv::rancor::Error>(bytes)
        .map_err(|error| SearchError::Backend(error.to_string()))?;
    validated.store(true, Ordering::Release);
    Ok(archived)
}

fn search_owned_terms(
    index: &TermIndex,
    tokens: &[String],
    excluded_ids: &BTreeSet<String>,
    limit: usize,
) -> Vec<SearchHit> {
    let lists: Vec<usize> = tokens
        .iter()
        .filter_map(|token| {
            index
                .term_tokens
                .binary_search_by(|candidate| candidate.as_str().cmp(token))
                .ok()
        })
        .collect();
    let scanned: usize = lists
        .iter()
        .map(|&token_index| index.term_postings[token_index].len())
        .sum();
    let mut candidates: FxHashMap<u32, [u64; 5]> =
        FxHashMap::with_capacity_and_hasher(scanned, rustc_hash::FxBuildHasher);
    for token_index in lists {
        for posting in &index.term_postings[token_index] {
            accumulate_posting(
                candidates.entry(posting.document_index).or_default(),
                posting.fields,
            );
        }
    }
    let mut scored: Vec<ScoredTerm<'_>> = Vec::with_capacity(candidates.len());
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
            scored.push(ScoredTerm {
                score_micros,
                name: document.name.as_str(),
                id: document.id.as_str(),
            });
        }
    }
    top_term_hits(scored, limit)
}

fn search_archived_terms(
    index: &ArchivedTermIndex,
    tokens: &[String],
    excluded_ids: &BTreeSet<String>,
    limit: usize,
) -> Vec<SearchHit> {
    let lists: Vec<usize> = tokens
        .iter()
        .filter_map(|token| {
            index
                .term_tokens
                .binary_search_by(|candidate| candidate.as_str().cmp(token))
                .ok()
        })
        .collect();
    // Try to answer from the documents carrying the rarest token alone. When that
    // succeeds it is provably the same answer for a fraction of the work.
    if let Some(hits) = intersection_top_hits(index, tokens.len(), &lists, excluded_ids, limit) {
        return hits;
    }
    let entries = merged_term_entries(index, tokens.len(), &lists)
        .unwrap_or_else(|| hashed_term_entries(index, tokens.len(), &lists));
    crate::timing::note("terms.postings", || {
        let scanned: usize = lists
            .iter()
            .map(|&token_index| index.term_postings[token_index].len())
            .sum();
        format!(
            "tokens={} scanned={scanned} scored={} limit={limit}",
            tokens.len(),
            entries.len()
        )
    });
    top_term_documents(index, entries, excluded_ids, limit)
}

/// A scored candidate that has not been looked up yet: `(score_micros, document_index)`.
/// Scores top out at 950_000, so they fit a `u32`, and eight bytes per candidate keeps a
/// six-figure candidate set cheap to hold and to partition.
type TermEntry = (u32, u32);

/// Most posting lists the merge folds together; a longer query accumulates through the hash map.
const MAX_MERGED_LISTS: usize = 8;

/// Scores every document in the union of `lists` without a hash map and without touching the
/// documents themselves, in ascending document order.
///
/// Posting lists are ascending by document index, so lists can be merged by walking them side by
/// side. A common token like "service" matches six figures of definitions: accumulating them in a
/// hash map and then reading each one's name and id out of the archive was most of what a query on
/// such a word cost.
///
/// Returns `None` when the lists cannot be merged — too many of them, or one that is not strictly
/// ascending in a malformed archive — and the caller falls back to `hashed_term_entries`.
fn merged_term_entries(
    index: &ArchivedTermIndex,
    query_len: usize,
    lists: &[usize],
) -> Option<Vec<TermEntry>> {
    if lists.len() > MAX_MERGED_LISTS {
        return None;
    }
    let documents = index.documents.len();
    let postings: Vec<&[ArchivedTermPosting]> = lists
        .iter()
        .map(|&token_index| index.term_postings[token_index].as_slice())
        .collect();
    // A document found through one token alone scores by which fields that token hit, so score
    // each of the sixteen field combinations once instead of once per document.
    let mut by_fields = [0u32; 16];
    for (fields, score) in by_fields.iter_mut().enumerate() {
        let mut counts = [0u64; 5];
        accumulate_posting(&mut counts, fields as u8);
        *score = score_term_counts(query_len, counts).unwrap_or(0) as u32;
    }
    if let [only] = postings.as_slice() {
        let mut entries = Vec::with_capacity(only.len());
        let mut previous: Option<u32> = None;
        for posting in only.iter() {
            let document_index = posting.document_index.to_native();
            if previous.is_some_and(|before| document_index <= before) {
                return None;
            }
            previous = Some(document_index);
            // Archived postings skip the `is_well_formed` gate (only structural rkyv
            // validation runs), so bound-check before indexing an mmap slice.
            if (document_index as usize) < documents {
                entries.push((by_fields[usize::from(posting.fields & 0xf)], document_index));
            }
        }
        return Some(entries);
    }
    // The longest list holds most of the documents. Merge the others into a short side table of
    // counts, then walk the long list once: a document the side table lacks is found through the
    // long list alone and scores straight from `by_fields`, with no cursor bookkeeping. Merging
    // all the lists with one cursor each cost about ten times as much per document.
    let longest = postings
        .iter()
        .enumerate()
        .max_by_key(|(_, list)| list.len())?
        .0;
    let side = merge_posting_counts(&postings, longest)?;
    let long = postings[longest];
    let mut entries = Vec::with_capacity(long.len() + side.len());
    let mut at = 0usize;
    let mut previous: Option<u32> = None;
    for (document_index, mut counts) in side {
        while let Some(posting) = long.get(at) {
            let before = posting.document_index.to_native();
            if before >= document_index {
                break;
            }
            if previous.is_some_and(|last| before <= last) {
                return None;
            }
            previous = Some(before);
            if (before as usize) < documents {
                entries.push((by_fields[usize::from(posting.fields & 0xf)], before));
            }
            at += 1;
        }
        if let Some(posting) = long.get(at)
            && posting.document_index.to_native() == document_index
        {
            if previous.is_some_and(|last| document_index <= last) {
                return None;
            }
            previous = Some(document_index);
            bump_counts(&mut counts, posting.fields);
            at += 1;
        }
        if (document_index as usize) < documents {
            let wide = counts.map(u64::from);
            entries.push((
                score_term_counts(query_len, wide).unwrap_or(0) as u32,
                document_index,
            ));
        }
    }
    for posting in &long[at..] {
        let after = posting.document_index.to_native();
        if previous.is_some_and(|last| after <= last) {
            return None;
        }
        previous = Some(after);
        if (after as usize) < documents {
            entries.push((by_fields[usize::from(posting.fields & 0xf)], after));
        }
    }
    Some(entries)
}

/// `accumulate_posting` for the small per-document counts of the merge's side table.
fn bump_counts(counts: &mut [u8; 5], fields: u8) {
    counts[0] += 1;
    counts[1] += u8::from(fields & 1 != 0);
    counts[2] += u8::from(fields & 2 != 0);
    counts[3] += u8::from(fields & 4 != 0);
    counts[4] += u8::from(fields & 8 != 0);
}

/// Every document of the posting lists other than `skip`, ascending, with the counts those lists
/// give it. `None` when one of them is not strictly ascending.
///
/// Lists are folded in shortest first, so the table stays small while the long ones arrive.
fn merge_posting_counts(
    postings: &[&[ArchivedTermPosting]],
    skip: usize,
) -> Option<Vec<(u32, [u8; 5])>> {
    let mut lists: Vec<&[ArchivedTermPosting]> = postings
        .iter()
        .enumerate()
        .filter(|(at, _)| *at != skip)
        .map(|(_, list)| *list)
        .collect();
    lists.sort_by_key(|list| list.len());
    let mut table: Vec<(u32, [u8; 5])> = Vec::new();
    for list in lists {
        let mut merged = Vec::with_capacity(table.len() + list.len());
        let mut previous: Option<u32> = None;
        let mut tabled = table.iter().copied().peekable();
        for posting in list {
            let document_index = posting.document_index.to_native();
            if previous.is_some_and(|before| document_index <= before) {
                return None;
            }
            previous = Some(document_index);
            // Documents the table has that this list skips go through unchanged.
            while let Some(entry) = tabled.next_if(|entry| entry.0 < document_index) {
                merged.push(entry);
            }
            let mut counts = tabled
                .next_if(|entry| entry.0 == document_index)
                .map_or([0u8; 5], |entry| entry.1);
            bump_counts(&mut counts, posting.fields);
            merged.push((document_index, counts));
        }
        merged.extend(tabled);
        table = merged;
    }
    Some(table)
}

/// The order-agnostic form of `merged_term_entries`: one hash map entry per document.
fn hashed_term_entries(
    index: &ArchivedTermIndex,
    query_len: usize,
    lists: &[usize],
) -> Vec<TermEntry> {
    let scanned: usize = lists
        .iter()
        .map(|&token_index| index.term_postings[token_index].len())
        .sum();
    // One allocation instead of ~18 doubling rehashes: the posting totals are
    // known before accumulating.
    let mut candidates: FxHashMap<u32, [u64; 5]> =
        FxHashMap::with_capacity_and_hasher(scanned, rustc_hash::FxBuildHasher);
    for &token_index in lists {
        for posting in index.term_postings[token_index].iter() {
            accumulate_posting(
                candidates
                    .entry(posting.document_index.to_native())
                    .or_default(),
                posting.fields,
            );
        }
    }
    let documents = index.documents.len();
    candidates
        .into_iter()
        .filter(|(document_index, _)| (*document_index as usize) < documents)
        .filter_map(|(document_index, counts)| {
            score_term_counts(query_len, counts).map(|score| (score as u32, document_index))
        })
        .collect()
}

/// The name and id of a candidate, borrowed from the archive. `entry` must have been
/// bound-checked against `documents` when it was collected.
fn term_entry_keys(index: &ArchivedTermIndex, entry: TermEntry) -> (&str, &str) {
    let document = &index.documents[entry.1 as usize];
    (document.name.as_str(), document.id.as_str())
}

/// The `want` best candidates under (score descending, name, id) — the order `top_term_hits`
/// selects by, which is a total order because ids are unique — in no particular order.
///
/// Almost every candidate loses on score alone, so scores are compared as integers and the
/// strings of only the documents that tie on the cut-off score are ever read: a common token
/// leaves thousands of identically scored definitions, and ordering all of them by name and id
/// was the larger half of what such a query cost.
fn select_top_entries(
    index: &ArchivedTermIndex,
    entries: Vec<TermEntry>,
    want: usize,
) -> Vec<TermEntry> {
    if want == 0 {
        return Vec::new();
    }
    if entries.len() <= want {
        return entries;
    }
    let cutoff = kth_best_score(&entries, want);
    let mut chosen: Vec<TermEntry> = Vec::with_capacity(want);
    let mut tied: Vec<TermEntry> = Vec::new();
    for entry in entries {
        match entry.0.cmp(&cutoff) {
            std::cmp::Ordering::Greater => chosen.push(entry),
            std::cmp::Ordering::Equal => tied.push(entry),
            std::cmp::Ordering::Less => {}
        }
    }
    // Fewer than `want` candidates beat the cut-off and at least `want` reach it.
    let need = want - chosen.len();
    if tied.len() > need {
        // Documents are stored sorted by (name, id) and candidates are collected in document
        // order, so the first `need` tied candidates are normally already the smallest. That is
        // checked rather than assumed, so an archive in any other order still gets the exact
        // selection, and it is checked without holding the names of a six-figure tie.
        let mut previous = term_entry_keys(index, tied[0]);
        let in_order = tied[1..].iter().all(|&entry| {
            let keys = term_entry_keys(index, entry);
            let ordered = previous <= keys;
            previous = keys;
            ordered
        });
        if in_order {
            tied.truncate(need);
        } else {
            let mut keyed: Vec<(&str, &str, TermEntry)> = tied
                .into_iter()
                .map(|entry| {
                    let (name, id) = term_entry_keys(index, entry);
                    (name, id, entry)
                })
                .collect();
            keyed.select_nth_unstable_by(need, |left, right| {
                left.0.cmp(right.0).then_with(|| left.1.cmp(right.1))
            });
            keyed.truncate(need);
            tied = keyed.into_iter().map(|(_, _, entry)| entry).collect();
        }
    }
    chosen.extend(tied);
    chosen
}

/// The `k`-th highest score among `entries` (`1 <= k <= entries.len()`).
fn kth_best_score(entries: &[TermEntry], k: usize) -> u32 {
    let mut scores: Vec<u32> = entries.iter().map(|entry| entry.0).collect();
    *scores
        .select_nth_unstable_by(k - 1, |left, right| right.cmp(left))
        .1
}

/// The best `limit` candidates that no overlay shadows, as owned hits.
///
/// Shadowed documents are dropped after selection rather than before it: reading every
/// candidate's id to test it against `excluded_ids` costs as much as the scan itself, while at
/// most `excluded_ids.len()` of the best candidates can be shadowed, so selecting that many extra
/// leaves `limit` survivors.
fn top_term_documents(
    index: &ArchivedTermIndex,
    entries: Vec<TermEntry>,
    excluded_ids: &BTreeSet<String>,
    limit: usize,
) -> Vec<SearchHit> {
    let hit = |(score, name, id): (u32, &str, &str)| SearchHit {
        value: name.to_owned(),
        definition_id: Some(id.to_owned()),
        score_micros: u64::from(score),
        reason: Some("term-coverage".into()),
    };
    let want = limit.saturating_add(excluded_ids.len());
    let selected = select_top_entries(index, entries, want);
    let mut keyed: Vec<(u32, &str, &str)> = selected
        .into_iter()
        .map(|entry| {
            let (name, id) = term_entry_keys(index, entry);
            (entry.0, name, id)
        })
        .filter(|(_, _, id)| !excluded_ids.contains(*id))
        .collect();
    if !excluded_ids.is_empty() {
        // Which `limit` survive depends on the order, so it only matters once something is dropped.
        keyed.sort_unstable_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.cmp(right.1))
                .then_with(|| left.2.cmp(right.2))
        });
    }
    keyed.into_iter().take(limit).map(hit).collect()
}

/// Highest score any document matching exactly `matched` of the query's tokens can
/// reach. Mirrors `score_term_counts`, using the fact that a token contributes at
/// most one match to each field: with `matched` tokens, no field count can exceed
/// `matched`. `terms_upper_bound_dominates_scores` pins the two together.
fn terms_upper_bound(query_len: usize, matched: usize) -> u64 {
    let matched = matched as u64;
    let full_coverage = (matched as usize == query_len) as u64;
    (600_000
        + matched.min(6) * 35_000
        + matched.min(4) * 25_000
        + matched.min(4) * 10_000
        + matched.min(4) * 3_000
        + matched.min(2) * 1_000
        + full_coverage * 20_000)
        .min(950_000)
}

/// The first position at or after `from` in `list` whose document index is `target` or later
/// (`list.len()` when there is none). Gallops forward from `from` and then bisects, so a cursor
/// that jumps across a common token's six-figure list costs a few dozen probes instead of one per
/// skipped posting.
fn first_at_or_after(list: &[ArchivedTermPosting], from: usize, target: u32) -> usize {
    let before = |posting: &ArchivedTermPosting| posting.document_index.to_native() < target;
    if from >= list.len() || !before(&list[from]) {
        return from;
    }
    // `list[low]` is before the target; `high` is the first probe that is not (or the end).
    let mut low = from;
    let mut step = 1usize;
    let high = loop {
        let probe = low.saturating_add(step);
        if probe >= list.len() {
            break list.len();
        }
        if before(&list[probe]) {
            low = probe;
            step = step.saturating_mul(2);
        } else {
            break probe;
        }
    };
    low + 1 + list[low + 1..high].partition_point(before)
}

/// Answer a multi-token query from the documents containing its rarest token.
///
/// Every posting list is ascending by document index, so one forward pass with a
/// cursor per list scores those documents exactly, without the hash map the full
/// scan needs. A document missing the rarest token can match at most
/// `present - 1` tokens, so once `limit` candidates score strictly above that
/// ceiling, no skipped document could have entered the result and the answer is
/// the same set the full scan would produce.
///
/// Returns `None` when that cannot be shown — a single-token query, too few
/// candidates to establish the k-th score, or a ceiling the candidates do not
/// clear — and the caller falls back to the full scan.
fn intersection_top_hits(
    index: &ArchivedTermIndex,
    query_len: usize,
    lists: &[usize],
    excluded_ids: &BTreeSet<String>,
    limit: usize,
) -> Option<Vec<SearchHit>> {
    if lists.len() < 2 || limit == 0 {
        return None;
    }
    let ceiling = terms_upper_bound(query_len, lists.len() - 1);
    let driver_at = lists
        .iter()
        .enumerate()
        .min_by_key(|&(_, &token_index)| index.term_postings[token_index].len())?
        .0;
    let driver = &index.term_postings[lists[driver_at]];
    let others: Vec<_> = lists
        .iter()
        .enumerate()
        .filter(|(at, _)| *at != driver_at)
        .map(|(_, &token_index)| &index.term_postings[token_index])
        .collect();
    let documents = index.documents.len();
    let mut cursors = vec![0usize; others.len()];
    let mut entries: Vec<TermEntry> = Vec::with_capacity(driver.len());
    for posting in driver.iter() {
        let document_index = posting.document_index.to_native();
        let mut counts = [0u64; 5];
        accumulate_posting(&mut counts, posting.fields);
        for (other, cursor) in others.iter().zip(cursors.iter_mut()) {
            *cursor = first_at_or_after(other, *cursor, document_index);
            if *cursor < other.len() && other[*cursor].document_index.to_native() == document_index
            {
                accumulate_posting(&mut counts, other[*cursor].fields);
            }
        }
        // Archived postings skip the `is_well_formed` gate, so bound-check before
        // indexing an mmap slice.
        if (document_index as usize) >= documents {
            continue;
        }
        if let Some(score_micros) = score_term_counts(query_len, counts) {
            entries.push((score_micros as u32, document_index));
        }
    }
    if entries.len() < limit {
        return None;
    }
    // Shadowed documents can only lower the k-th score, so a cut-off at or under the ceiling
    // rules this path out before a single name or id is read.
    if u64::from(kth_best_score(&entries, limit)) <= ceiling {
        return None;
    }
    let scanned = driver.len() + others.iter().map(|other| other.len()).sum::<usize>();
    let candidates = entries.len();
    let hits = top_term_documents(index, entries, excluded_ids, limit);
    if hits.len() < limit {
        return None;
    }
    // Strictly above the ceiling: an equal score could still outrank the k-th by
    // the name/id tie-break that `finish_hits` applies.
    let kth = hits.iter().map(|hit| hit.score_micros).min()?;
    if kth <= ceiling {
        return None;
    }
    crate::timing::note("terms.pruned", || {
        format!("scanned={scanned} candidates={candidates} kth={kth} ceiling={ceiling}")
    });
    Some(hits)
}

/// A scored candidate that still borrows its name and id from the index.
struct ScoredTerm<'index> {
    score_micros: u64,
    name: &'index str,
    id: &'index str,
}

/// Select the best `limit` candidates, then own only those.
///
/// The order matches `finish_hits` exactly — score descending, then name, then
/// id — and ids are unique, so the comparator is a total order and the selected
/// set is identical to scoring everything and truncating afterwards.
fn top_term_hits(mut scored: Vec<ScoredTerm<'_>>, limit: usize) -> Vec<SearchHit> {
    let cmp = |left: &ScoredTerm<'_>, right: &ScoredTerm<'_>| {
        right
            .score_micros
            .cmp(&left.score_micros)
            .then_with(|| left.name.cmp(right.name))
            .then_with(|| left.id.cmp(right.id))
    };
    if limit < scored.len() {
        scored.select_nth_unstable_by(limit, cmp);
        scored.truncate(limit);
    }
    scored
        .into_iter()
        .map(|candidate| SearchHit {
            value: candidate.name.to_owned(),
            definition_id: Some(candidate.id.to_owned()),
            score_micros: candidate.score_micros,
            reason: Some("term-coverage".into()),
        })
        .collect()
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
                validated: AtomicBool::new(false),
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
            // A name the edited file kept is listed by both the dictionary and the overlay.
            // `finish_hits` removes duplicates only after truncating, so each one cost a slot
            // and a prefix search after a sync came back short of `limit`.
            hits.sort_by(|left, right| {
                left.value
                    .cmp(&right.value)
                    .then_with(|| left.definition_id.cmp(&right.definition_id))
            });
            hits.dedup_by(|left, right| {
                left.value == right.value && left.definition_id == right.definition_id
            });
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
            // Pre-selecting the index's own best `limit` is safe: anything it
            // drops already has `limit` better candidates ahead of it, so it
            // could not survive the merge with the overlay either.
            hits.extend(terms.search(&tokens, &shadowed_ids, limit)?);
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
            validated: AtomicBool::new(false),
        });
        self
    }
}

/// The distinct search tokens of `text`. Tokens are lowercase alphanumeric runs, so they never
/// contain whitespace: joining them with spaces and splitting again (what the stored `*_terms`
/// fields do) yields exactly this set.
fn search_tokens(text: &str) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    add_search_tokens(&mut tokens, text);
    tokens
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

    #[test]
    fn a_name_kept_by_an_edited_file_does_not_cost_a_prefix_search_slot() {
        // After a sync, the dictionary and the overlay both list the names the edited file kept.
        let dict =
            SymbolDict::from_names((0..6).map(|i| format!("parse{i}")).collect(), "snap".into());
        let index =
            SearchIndex::from_parts(dict, None).with_term_overlays(vec![SearchTermOverlay {
                snapshot_id: "snap".into(),
                removed_ids: Vec::new(),
                added_names: vec!["parse0".into(), "parse1".into(), "parse2".into()],
                removed_names: Vec::new(),
                documents: Vec::new(),
            }]);
        let hits = index.search("parse", SearchKind::Prefix, 5).unwrap();
        let values: Vec<_> = hits.iter().map(|hit| hit.value.as_str()).collect();
        assert_eq!(values, ["parse0", "parse1", "parse2", "parse3", "parse4"]);
    }
    use super::*;
    use crate::{
        model::{FileArtifact, IndexSnapshot, SnapshotId, Span, Symbol},
        scanner::parse_source,
    };
    use std::collections::BTreeMap;
    use std::time::Instant;

    #[test]
    fn exact_identity_outranks_every_scored_hit() {
        // `explore` lists id/qualified matches ahead of scored hits, and stamps them with
        // SCORE_EXACT_IDENTITY. If any scoring tier ever reaches past it, the candidate list
        // starts reporting a score that disagrees with its own order.
        for ceiling in [
            SCORE_EXACT_CASE,
            SCORE_EXACT_CASE_INSENSITIVE,
            SCORE_PREFIX_CASE,
            SCORE_PREFIX,
            SCORE_FUZZY_EXACT,
        ] {
            assert!(
                SCORE_EXACT_IDENTITY > ceiling,
                "SCORE_EXACT_IDENTITY ({SCORE_EXACT_IDENTITY}) must top every tier, found {ceiling}"
            );
        }
    }

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

    /// A prefix query that equals a name up to case is an exact case-insensitive match, also when
    /// the case differs outside ASCII. Scored as a mere prefix, `École` lost to the longer `écoles`.
    #[test]
    fn non_ascii_case_insensitive_exact_outranks_a_longer_prefix() {
        let dict = SymbolDict::from_names(vec!["École".into(), "écoles".into()], "s".into());
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&dict).unwrap();
        let archived = rkyv::access::<ArchivedSymbolDict, rkyv::rancor::Error>(&bytes).unwrap();
        let owned = SearchIndex::from_symbol_dict(dict)
            .search("école", SearchKind::Prefix, 10)
            .unwrap();
        let packed = search_archived_dict(archived, "école", SearchKind::Prefix, 10).unwrap();
        for hits in [owned, packed] {
            assert_eq!(hits[0].value, "École", "{hits:?}");
            assert_eq!(
                hits[0].reason.as_deref(),
                Some("exact-case-insensitive"),
                "{hits:?}"
            );
            assert_eq!(hits[0].score_micros, SCORE_EXACT_CASE_INSENSITIVE);
        }
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
    ///
    /// Asserts growth ratios, never absolute milliseconds. An absolute budget here
    /// encodes the speed of whichever machine and profile happened to run it — this
    /// test previously carried release-calibrated budgets while `cargo test` runs a
    /// debug build, so it failed on a loaded machine while the property it exists to
    /// protect was intact. N grows 5x then 4x below, so a genuinely linear
    /// implementation shows up as a matching multiple; the bounds leave room for
    /// scheduler noise without leaving room for O(N).
    #[test]
    fn scale_exact_prefix_sublinear_wall() {
        let sizes = [10_000usize, 50_000, 200_000];
        let mut prev_exact = 0.0f64;
        let mut prev_prefix = 0.0f64;
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
            // Beyond the first size the prefix query matches the same 10k names — only
            // where the binary search lands changes — so its cost must stay flat. A
            // linear scan would track N and grow 4-5x per step.
            if prev_prefix > 0.0 {
                assert!(
                    prefix_ms < prev_prefix * 3.0 + 0.5,
                    "prefix grew with N: {prev_prefix} -> {prefix_ms} at N={n}"
                );
            }
            prev_prefix = prefix_ms;
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

    /// The rarest-token pruning is only exact while `terms_upper_bound` really is an
    /// upper bound on `score_term_counts`. Brute-force every reachable field-count
    /// combination so a change to the scorer that breaks the bound fails here rather
    /// than silently dropping results from term search.
    #[test]
    fn terms_upper_bound_dominates_scores() {
        for query_len in 1..=6usize {
            for matched in 1..=query_len {
                let bound = terms_upper_bound(query_len, matched);
                let matched64 = matched as u64;
                // A token contributes at most one match per field, so each field
                // count ranges over 0..=matched independently.
                for name in 0..=matched64 {
                    for qualified in 0..=matched64 {
                        for path in 0..=matched64 {
                            for kind in 0..=matched64 {
                                let counts = [matched64, name, qualified, path, kind];
                                let score = score_term_counts(query_len, counts)
                                    .expect("matched >= 1 always scores");
                                assert!(
                                    score <= bound,
                                    "score {score} exceeds bound {bound} for \
                                     query_len={query_len} counts={counts:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// The ceiling must separate coverage levels, otherwise the pruning can never
    /// fire: a fully-covering document has to be able to outscore the best possible
    /// partially-covering one.
    #[test]
    fn full_coverage_can_outscore_the_partial_ceiling() {
        for query_len in 2..=6usize {
            let partial_ceiling = terms_upper_bound(query_len, query_len - 1);
            let full = terms_upper_bound(query_len, query_len);
            assert!(
                full > partial_ceiling,
                "query_len={query_len}: full {full} must exceed partial {partial_ceiling}"
            );
        }
    }

    /// Deterministic xorshift so a failing seed reproduces.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    /// A term index over `document_count` synthetic definitions. Few distinct names, tokens and
    /// field combinations, so almost every query ties heavily on score and on name.
    fn synthetic_term_index(
        rng: &mut Rng,
        document_count: usize,
        vocabulary: &[String],
        keep_documents_sorted: bool,
    ) -> TermIndex {
        let names = ["alpha", "beta", "gamma", "delta", "alpha2", "Alpha"];
        let mut documents: Vec<TermDocument> = (0..document_count)
            .map(|n| {
                let name = names[rng.below(names.len())];
                TermDocument {
                    id: format!("symbol://pkg{}/f{n}.ts#kind:{name}", rng.below(5)),
                    name: name.to_owned(),
                }
            })
            .collect();
        if keep_documents_sorted {
            documents.sort_by(|left, right| (&left.name, &left.id).cmp(&(&right.name, &right.id)));
        }
        let mut postings: Vec<Vec<TermPosting>> = vec![Vec::new(); vocabulary.len()];
        for document_index in 0..document_count {
            for list in postings.iter_mut() {
                // Skewed so some tokens are common and some rare.
                if rng.below(100) < 8 + 6 * (list.len() % 7).min(5) + rng.below(20) {
                    list.push(TermPosting {
                        document_index: document_index as u32,
                        fields: (1 + rng.below(15)) as u8,
                    });
                }
            }
        }
        // Dictionary order: the vocabulary is sorted by construction.
        TermIndex {
            format_version: TermIndex::FORMAT_VERSION,
            snapshot_id: "synthetic".into(),
            documents,
            term_tokens: vocabulary.to_vec(),
            term_postings: postings,
        }
    }

    /// The scoring and selection exactly as the hash-map implementation did it: accumulate every
    /// posting, drop shadowed and out-of-range documents, order by (score desc, name, id), cut.
    fn reference_term_hits(
        index: &ArchivedTermIndex,
        tokens: &[String],
        excluded_ids: &BTreeSet<String>,
        limit: usize,
    ) -> Vec<(u64, String, String)> {
        let mut candidates: std::collections::HashMap<u32, [u64; 5]> = Default::default();
        for token in tokens {
            if let Ok(at) = index
                .term_tokens
                .binary_search_by(|candidate| candidate.as_str().cmp(token))
            {
                for posting in index.term_postings[at].iter() {
                    accumulate_posting(
                        candidates
                            .entry(posting.document_index.to_native())
                            .or_default(),
                        posting.fields,
                    );
                }
            }
        }
        let mut scored: Vec<(u64, String, String)> = candidates
            .into_iter()
            .filter_map(|(document_index, counts)| {
                let document = index.documents.get(document_index as usize)?;
                if excluded_ids.contains(document.id.as_str()) {
                    return None;
                }
                let score = score_term_counts(tokens.len(), counts)?;
                Some((score, document.name.to_string(), document.id.to_string()))
            })
            .collect();
        scored.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        scored.truncate(limit);
        scored
    }

    fn summarize(hits: Vec<SearchHit>) -> Vec<(u64, String, String)> {
        let mut rows: Vec<_> = hits
            .into_iter()
            .map(|hit| {
                assert_eq!(hit.reason.as_deref(), Some("term-coverage"));
                (hit.score_micros, hit.value, hit.definition_id.unwrap())
            })
            .collect();
        rows.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        rows
    }

    /// The cursor merge, the integer-first selection and the shadowed-document handling must pick
    /// exactly the documents the hash-map scan picked, in every shape of archive: documents in
    /// storage order and out of it, a malformed posting list, and queries wider than the merge.
    #[test]
    fn term_search_selects_exactly_what_a_full_scan_would() {
        let vocabulary: Vec<String> = ["ab", "bc", "cd", "de", "ef", "fg", "gh", "hi", "ij", "jk"]
            .iter()
            .map(|token| (*token).to_owned())
            .collect();
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut compared = 0usize;
        let mut pruned = 0usize;
        let mut shadowed = 0usize;
        for trial in 0..160 {
            let document_count = [0, 1, 7, 60, 300, 900][trial % 6];
            let keep_documents_sorted = trial % 4 != 3;
            let mut term_index =
                synthetic_term_index(&mut rng, document_count, &vocabulary, keep_documents_sorted);
            match trial % 8 {
                // A posting list that repeats a document: only the order-agnostic path counts it
                // the way the hash map always did.
                5 => {
                    if let Some(list) = term_index.term_postings.iter_mut().find(|l| l.len() > 2) {
                        let repeated = list[1];
                        list.insert(1, repeated);
                    }
                }
                // Postings that point past the document table.
                6 => {
                    if let Some(list) = term_index.term_postings.first_mut() {
                        list.push(TermPosting {
                            document_index: document_count as u32 + 3,
                            fields: 15,
                        });
                    }
                }
                _ => {}
            }
            let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&term_index).unwrap();
            let archived = rkyv::access::<ArchivedTermIndex, rkyv::rancor::Error>(&bytes).unwrap();
            for _ in 0..24 {
                // 1..=10 tokens, some absent from the dictionary, some repeated draws.
                let width = 1 + rng.below(if trial % 5 == 0 { 10 } else { 4 });
                let mut tokens: Vec<String> = (0..width)
                    .map(|_| {
                        if rng.below(10) == 0 {
                            format!("zz{}", rng.below(3))
                        } else {
                            vocabulary[rng.below(vocabulary.len())].clone()
                        }
                    })
                    .collect();
                tokens.sort();
                tokens.dedup();
                let limit = [1, 2, 5, 10, 40, 128, 400][rng.below(7)];
                let mut excluded: BTreeSet<String> = BTreeSet::new();
                if rng.below(3) == 0 && !term_index.documents.is_empty() {
                    for _ in 0..rng.below(1 + document_count.min(60)) {
                        let pick = rng.below(term_index.documents.len());
                        excluded.insert(term_index.documents[pick].id.clone());
                    }
                }
                let expected = reference_term_hits(archived, &tokens, &excluded, limit);
                let lists: Vec<usize> = tokens
                    .iter()
                    .filter_map(|token| {
                        archived
                            .term_tokens
                            .binary_search_by(|candidate| candidate.as_str().cmp(token))
                            .ok()
                    })
                    .collect();
                let describe = || {
                    format!(
                        "trial {trial} tokens={tokens:?} limit={limit} excluded={}",
                        excluded.len()
                    )
                };
                // The order-agnostic collection is exact for any archive.
                let hashed = hashed_term_entries(archived, tokens.len(), &lists);
                assert_eq!(
                    summarize(top_term_documents(archived, hashed, &excluded, limit)),
                    expected,
                    "hashed {}",
                    describe()
                );
                let ascending = lists.iter().all(|&at| {
                    archived.term_postings[at]
                        .as_slice()
                        .windows(2)
                        .all(|pair| {
                            pair[0].document_index.to_native() < pair[1].document_index.to_native()
                        })
                });
                if ascending {
                    let actual =
                        summarize(search_archived_terms(archived, &tokens, &excluded, limit));
                    assert_eq!(actual, expected, "{}", describe());
                    compared += 1;
                    if intersection_top_hits(archived, tokens.len(), &lists, &excluded, limit)
                        .is_some()
                    {
                        pruned += 1;
                    }
                    if !excluded.is_empty() {
                        shadowed += 1;
                    }
                } else {
                    // A list that repeats a document cannot be merged.
                    assert!(
                        merged_term_entries(archived, tokens.len(), &lists).is_none(),
                        "{}",
                        describe()
                    );
                }
            }
        }
        assert!(compared > 3000);
        // Both the pruned answer and the full scan, and shadowed documents, were really covered.
        assert!(pruned > 200, "pruned path ran {pruned} times");
        assert!(
            compared - pruned > 1000,
            "full scan ran {} times",
            compared - pruned
        );
        assert!(shadowed > 500, "shadowed documents in {shadowed} queries");
    }

    #[test]
    fn galloping_cursor_lands_where_a_linear_scan_would() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for length in [0usize, 1, 2, 3, 7, 64, 500, 5000] {
            let mut next = 0u32;
            let postings: Vec<TermPosting> = (0..length)
                .map(|_| {
                    next += 1 + rng.below(4) as u32;
                    TermPosting {
                        document_index: next,
                        fields: 1,
                    }
                })
                .collect();
            let term_index = TermIndex {
                format_version: TermIndex::FORMAT_VERSION,
                snapshot_id: "synthetic".into(),
                documents: Vec::new(),
                term_tokens: vec!["tok".to_owned()],
                term_postings: vec![postings],
            };
            let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&term_index).unwrap();
            let archived = rkyv::access::<ArchivedTermIndex, rkyv::rancor::Error>(&bytes).unwrap();
            let list = archived.term_postings[0].as_slice();
            for _ in 0..400 {
                let from = rng.below(length + 2);
                let target = rng.below(next as usize + 8) as u32;
                let expected = (from..list.len())
                    .find(|&at| list[at].document_index.to_native() >= target)
                    .unwrap_or(list.len().max(from));
                assert_eq!(
                    first_at_or_after(list, from, target),
                    expected,
                    "length {length} from {from} target {target}"
                );
            }
        }
    }

    /// Documents stored out of (name, id) order must still yield the exact selection among
    /// candidates that tie on score: the shortcut of taking the first ones in storage order is
    /// only taken when the order is verified.
    #[test]
    fn tie_selection_is_exact_when_documents_are_not_stored_in_name_order() {
        let names = ["delta", "alpha", "charlie", "bravo", "alpha", "echo"];
        let term_index = TermIndex {
            format_version: TermIndex::FORMAT_VERSION,
            snapshot_id: "synthetic".into(),
            documents: names
                .iter()
                .enumerate()
                .map(|(n, name)| TermDocument {
                    id: format!("id{n}"),
                    name: (*name).to_owned(),
                })
                .collect(),
            term_tokens: vec!["tok".to_owned()],
            term_postings: vec![
                (0..names.len() as u32)
                    .map(|document_index| TermPosting {
                        document_index,
                        fields: 1,
                    })
                    .collect(),
            ],
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&term_index).unwrap();
        let archived = rkyv::access::<ArchivedTermIndex, rkyv::rancor::Error>(&bytes).unwrap();
        let tokens = vec!["tok".to_owned()];
        for limit in 1..=names.len() + 1 {
            let hits = search_archived_terms(archived, &tokens, &BTreeSet::new(), limit);
            let mut got: Vec<_> = hits.iter().map(|hit| hit.value.as_str()).collect();
            got.sort_unstable();
            let mut sorted = names.to_vec();
            sorted.sort_unstable();
            sorted.truncate(limit);
            assert_eq!(got, sorted, "limit {limit}");
        }
    }
}
