use crate::model::{
    Edge, EdgeConfidence, EdgeKind, EdgeProvenance, ExportBindingKind, FileArtifact,
    ImportBindingKind, Span, SymbolRef,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

static MATCHED_FILE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("matched workspace file"));
static NO_CANDIDATE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("no workspace candidate"));
static STALE_CANDIDATE: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::from("multiple or stale workspace candidates"));
static UNIQUE_SYMBOL: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("unique workspace symbol"));

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ResolverConfig {
    pub base_url: Option<PathBuf>,
    pub paths: BTreeMap<String, Vec<String>>,
    pub extensions: Vec<String>,
    pub max_candidates: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Resolution {
    pub specifier: String,
    pub target: Option<String>,
    pub candidates: Vec<String>,
    pub confidence: String,
    pub reason: Arc<str>,
}

/// Canonical invalidation keys emitted by the same resolver path that chooses an import target.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ResolutionTrace {
    pub importer: String,
    pub specifier: String,
    pub attempted_paths: BTreeSet<String>,
    pub basename_keys: BTreeSet<String>,
}

struct ResolutionCore {
    target: Option<String>,
    candidates: Vec<String>,
    confidence: &'static str,
    reason: Arc<str>,
    attempted_paths: BTreeSet<String>,
    basename_keys: BTreeSet<String>,
}

impl ResolutionCore {
    fn diagnostic(&self, specifier: &str) -> Resolution {
        Resolution {
            specifier: specifier.to_owned(),
            target: self.target.clone(),
            candidates: self.candidates.clone(),
            confidence: self.confidence.to_owned(),
            reason: Arc::clone(&self.reason),
        }
    }

    fn trace(&self, importer: &str, specifier: &str) -> ResolutionTrace {
        ResolutionTrace {
            importer: importer.to_owned(),
            specifier: specifier.to_owned(),
            attempted_paths: self.attempted_paths.clone(),
            basename_keys: self.basename_keys.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ReverseIndex {
    pub dependents: BTreeMap<String, BTreeSet<String>>,
}

/// Minimal workspace-wide state required to resolve a small artifact subset exactly.
/// It is deliberately independent of `FileArtifact`, so generations can persist and shard it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ResolutionUniverse {
    pub format_version: u32,
    pub resolver_fingerprint: String,
    pub files: BTreeSet<String>,
    /// Collision-safe definitions grouped by local/display name.
    pub symbol_definitions: BTreeMap<String, Vec<SymbolDefinition>>,
    /// Raw module export bindings. Sources are resolved lazily so barrel chains remain exact.
    pub module_exports: BTreeMap<String, Vec<ModuleExport>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SymbolDefinition {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    pub path: String,
    pub kind: Arc<str>,
    pub span: Span,
    pub exported: bool,
    pub scope: Option<Span>,
}

impl SymbolDefinition {
    fn from_symbol(artifact: &FileArtifact, symbol: &crate::model::Symbol) -> Self {
        Self {
            id: symbol.id.clone(),
            name: symbol.name.clone(),
            qualified_name: symbol.qualified_name.clone(),
            path: artifact.path.clone(),
            kind: Arc::clone(&symbol.kind),
            span: symbol.span,
            exported: symbol.exported,
            scope: symbol.scope,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ModuleExport {
    pub local: String,
    pub exported: String,
    pub source: Option<String>,
    pub kind: ExportBindingKind,
    pub type_only: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ResolutionUniverseOverlay {
    pub files: BTreeMap<String, bool>,
    /// `name -> path -> every definition of name in path` after the change; an empty list means
    /// the path no longer defines the name. Each entry replaces that path's run of the name's
    /// sorted list and leaves every other path's run alone.
    ///
    /// The overlay used to carry each touched name's whole workspace-wide list. One edit to a file
    /// declaring `get` or `execute` then serialized, compacted, and re-read every other definition
    /// of that name -- 78MB for a one-line change in a 20k-file workspace.
    pub symbol_definitions: BTreeMap<String, BTreeMap<String, Vec<SymbolDefinition>>>,
    pub module_exports: BTreeMap<String, Option<Vec<ModuleExport>>>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ResolutionUniverseShard {
    pub files: BTreeSet<String>,
    pub symbol_definitions: BTreeMap<String, Vec<SymbolDefinition>>,
    pub module_exports: BTreeMap<String, Vec<ModuleExport>>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ResolutionUniverseShardSet {
    pub format_version: u32,
    pub resolver_fingerprint: String,
    pub shard_bits: u8,
    pub shards: BTreeMap<u16, ResolutionUniverseShard>,
}

pub enum LookupSlice<'a, T> {
    Borrowed(&'a [T]),
    Owned(Vec<T>),
}

impl<T> Deref for LookupSlice<'_, T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Borrowed(value) => value,
            Self::Owned(value) => value,
        }
    }
}

impl<T: Clone> LookupSlice<'_, T> {
    pub fn into_owned(self) -> Vec<T> {
        match self {
            Self::Borrowed(value) => value.to_vec(),
            Self::Owned(value) => value,
        }
    }
}

pub trait ResolutionLookup: Sync {
    fn matches(&self, config: &ResolverConfig) -> bool;
    fn contains_file(&self, path: &str) -> bool;
    fn symbol_definer_count(&self, name: &str) -> u32;
    fn symbol_definitions(&self, name: &str) -> LookupSlice<'_, SymbolDefinition>;
    fn module_exports(&self, path: &str) -> LookupSlice<'_, ModuleExport>;
    /// Definitions of `name` declared in `path`. Nearly every reference asks this question, and a
    /// name such as `get` or `execute` can have tens of thousands of definitions workspace-wide;
    /// answering it from the full list cost O(definers) per reference (and a full clone of the
    /// list on lookups that own their result).
    fn symbol_definitions_in_file(&self, name: &str, path: &str) -> Vec<SymbolDefinition> {
        definitions_in_path(&self.symbol_definitions(name), path).to_vec()
    }
}

/// The contiguous run of `path` in a name's definition list.
///
/// Every list is kept sorted by `(path, span, qualified_name)`: `ResolutionUniverse::build` sorts
/// it, `replace_artifact` inserts at the sorted position, and overlays replace whole per-path runs
/// with runs sorted the same way (`apply_definition_deltas`). A binary search therefore returns
/// exactly what a linear filter on `path` returned, in the same order.
pub(crate) fn definitions_in_path<'a>(
    definitions: &'a [SymbolDefinition],
    path: &str,
) -> &'a [SymbolDefinition] {
    debug_assert!(
        definitions
            .windows(2)
            .all(|pair| pair[0].path.as_str() <= pair[1].path.as_str()),
        "definition lists must stay sorted by path"
    );
    let range = path_run(definitions, path);
    &definitions[range]
}

fn path_run(definitions: &[SymbolDefinition], path: &str) -> std::ops::Range<usize> {
    let start = definitions.partition_point(|definition| definition.path.as_str() < path);
    let len = definitions[start..].partition_point(|definition| definition.path == path);
    start..start + len
}

/// Replace each path's run of a name's sorted list with the overlay's run for that path.
pub(crate) fn apply_definition_deltas(
    definitions: &mut Vec<SymbolDefinition>,
    deltas: &BTreeMap<String, Vec<SymbolDefinition>>,
) {
    for (path, replacement) in deltas {
        let run = path_run(definitions, path);
        definitions.splice(run, replacement.iter().cloned());
    }
}

/// Apply an overlay's per-path definition runs to name-keyed lists, dropping names left empty.
pub(crate) fn apply_definition_overlay(
    lists: &mut BTreeMap<String, Vec<SymbolDefinition>>,
    overlay: &BTreeMap<String, BTreeMap<String, Vec<SymbolDefinition>>>,
) {
    for (name, deltas) in overlay {
        let definitions = match lists.get_mut(name) {
            Some(definitions) => definitions,
            None => lists.entry(name.clone()).or_default(),
        };
        apply_definition_deltas(definitions, deltas);
        if definitions.is_empty() {
            lists.remove(name);
        }
    }
}

/// A file's definitions grouped by name, each run in the order the universe keeps it.
fn definitions_by_name(artifact: &FileArtifact) -> BTreeMap<&str, Vec<SymbolDefinition>> {
    let mut by_name: BTreeMap<&str, Vec<SymbolDefinition>> = BTreeMap::new();
    for symbol in &artifact.symbols {
        by_name
            .entry(symbol.name.as_str())
            .or_default()
            .push(SymbolDefinition::from_symbol(artifact, symbol));
    }
    for run in by_name.values_mut() {
        // Same stable comparator as `ResolutionUniverse::build`; the path is constant here.
        run.sort_by(|left, right| {
            (left.span, &left.qualified_name).cmp(&(right.span, &right.qualified_name))
        });
    }
    by_name
}

pub struct OverlayResolutionLookup<'a> {
    base: &'a dyn ResolutionLookup,
    overlay: &'a ResolutionUniverseOverlay,
}

impl<'a> OverlayResolutionLookup<'a> {
    pub fn new(base: &'a dyn ResolutionLookup, overlay: &'a ResolutionUniverseOverlay) -> Self {
        Self { base, overlay }
    }
}

impl ResolutionLookup for OverlayResolutionLookup<'_> {
    fn matches(&self, config: &ResolverConfig) -> bool {
        self.base.matches(config)
    }

    fn contains_file(&self, path: &str) -> bool {
        self.overlay
            .files
            .get(path)
            .copied()
            .unwrap_or_else(|| self.base.contains_file(path))
    }

    fn symbol_definer_count(&self, name: &str) -> u32 {
        let base = self.base.symbol_definer_count(name);
        let Some(deltas) = self.overlay.symbol_definitions.get(name) else {
            return base;
        };
        let replaced: u64 = deltas
            .keys()
            .map(|path| self.base.symbol_definitions_in_file(name, path).len() as u64)
            .sum();
        let added: u64 = deltas.values().map(|run| run.len() as u64).sum();
        u32::try_from((u64::from(base) + added).saturating_sub(replaced)).unwrap_or(u32::MAX)
    }

    fn symbol_definitions(&self, name: &str) -> LookupSlice<'_, SymbolDefinition> {
        match self.overlay.symbol_definitions.get(name) {
            Some(deltas) => {
                let mut definitions = self.base.symbol_definitions(name).into_owned();
                apply_definition_deltas(&mut definitions, deltas);
                LookupSlice::Owned(definitions)
            }
            None => self.base.symbol_definitions(name),
        }
    }

    fn module_exports(&self, path: &str) -> LookupSlice<'_, ModuleExport> {
        match self.overlay.module_exports.get(path) {
            Some(Some(value)) => LookupSlice::Borrowed(value),
            Some(None) => LookupSlice::Borrowed(&[]),
            None => self.base.module_exports(path),
        }
    }

    fn symbol_definitions_in_file(&self, name: &str, path: &str) -> Vec<SymbolDefinition> {
        match self
            .overlay
            .symbol_definitions
            .get(name)
            .and_then(|deltas| deltas.get(path))
        {
            Some(run) => run.clone(),
            None => self.base.symbol_definitions_in_file(name, path),
        }
    }
}

impl ResolutionUniverseOverlay {
    /// The overlay a set of file changes produces. Only the changed files' own runs are recorded,
    /// so no base is consulted: the result is the same whatever else defines the names.
    pub fn from_artifact_changes<'a>(
        changes: impl IntoIterator<Item = (Option<&'a FileArtifact>, Option<&'a FileArtifact>)>,
    ) -> Self {
        let mut overlay = Self::default();
        for (old, new) in changes {
            if let Some(old) = old {
                overlay.files.insert(old.path.clone(), false);
                overlay.module_exports.insert(old.path.clone(), None);
                for symbol in &old.symbols {
                    overlay
                        .symbol_definitions
                        .entry(symbol.name.clone())
                        .or_default()
                        .insert(old.path.clone(), Vec::new());
                }
            }
            if let Some(new) = new {
                overlay.files.insert(new.path.clone(), true);
                overlay
                    .module_exports
                    .insert(new.path.clone(), Some(module_exports(new)));
                for (name, run) in definitions_by_name(new) {
                    overlay
                        .symbol_definitions
                        .entry(name.to_owned())
                        .or_default()
                        .insert(new.path.clone(), run);
                }
            }
        }
        overlay
    }

    /// Fold a newer overlay into this one. Every entry is an absolute per-key state, so the newer
    /// side wins key by key -- for definitions, per `(name, path)`.
    pub(crate) fn compose(&mut self, newer: Self) {
        self.files.extend(newer.files);
        for (name, deltas) in newer.symbol_definitions {
            self.symbol_definitions
                .entry(name)
                .or_default()
                .extend(deltas);
        }
        self.module_exports.extend(newer.module_exports);
    }
}

impl ResolutionUniverse {
    pub const FORMAT_VERSION: u32 = 6;

    pub fn build(artifacts: &BTreeMap<String, FileArtifact>, config: &ResolverConfig) -> Self {
        use rayon::prelude::*;
        /// Artifacts per worker when collecting definitions. Bounds how many partial
        /// maps the merge below has to fold together.
        const BUILD_CHUNK: usize = 512;

        let mut universe = Self {
            format_version: Self::FORMAT_VERSION,
            resolver_fingerprint: resolver_fingerprint(config),
            ..Self::default()
        };
        let ordered: Vec<&FileArtifact> = artifacts.values().collect();
        // Collect per chunk, then fold in artifact order. That keeps each name's
        // pre-sort order identical to the sequential build, so the stable sort below
        // resolves ties the same way.
        type Collected = (
            Vec<String>,
            BTreeMap<String, Vec<SymbolDefinition>>,
            Vec<(String, Vec<ModuleExport>)>,
        );
        let collected: Vec<Collected> = ordered
            .par_chunks(BUILD_CHUNK)
            .map(|chunk| {
                let mut files = Vec::with_capacity(chunk.len());
                let mut definitions: BTreeMap<String, Vec<SymbolDefinition>> = BTreeMap::new();
                let mut exports = Vec::with_capacity(chunk.len());
                for artifact in chunk {
                    files.push(artifact.path.clone());
                    for symbol in &artifact.symbols {
                        definitions
                            .entry(symbol.name.clone())
                            .or_default()
                            .push(SymbolDefinition::from_symbol(artifact, symbol));
                    }
                    exports.push((artifact.path.clone(), module_exports(artifact)));
                }
                (files, definitions, exports)
            })
            .collect();
        for (files, definitions, exports) in collected {
            universe.files.extend(files);
            for (name, entries) in definitions {
                universe
                    .symbol_definitions
                    .entry(name)
                    .or_default()
                    .extend(entries);
            }
            universe.module_exports.extend(exports);
        }
        universe
            .symbol_definitions
            .par_iter_mut()
            .for_each(|(_, definitions)| {
                definitions.sort_by(|left, right| {
                    (&left.path, left.span, &left.qualified_name).cmp(&(
                        &right.path,
                        right.span,
                        &right.qualified_name,
                    ))
                });
            });
        universe
    }

    pub fn matches(&self, config: &ResolverConfig) -> bool {
        self.format_version == Self::FORMAT_VERSION
            && self.resolver_fingerprint == resolver_fingerprint(config)
    }

    pub fn replace_artifact(&mut self, old: Option<&FileArtifact>, new: Option<&FileArtifact>) {
        if let Some(old) = old {
            self.files.remove(&old.path);
            for symbol in &old.symbols {
                if let Some(definitions) = self.symbol_definitions.get_mut(&symbol.name) {
                    definitions.retain(|definition| definition.id != symbol.id);
                    if definitions.is_empty() {
                        self.symbol_definitions.remove(&symbol.name);
                    }
                }
            }
            self.module_exports.remove(&old.path);
        }
        if let Some(new) = new {
            self.files.insert(new.path.clone());
            for symbol in &new.symbols {
                let definition = SymbolDefinition::from_symbol(new, symbol);
                let definitions = self
                    .symbol_definitions
                    .entry(symbol.name.clone())
                    .or_default();
                // The vector is already sorted, so insert at its place instead of
                // re-sorting it per symbol. A name shared across a large monorepo
                // has thousands of definitions, and a file declaring S symbols
                // re-sorted all of them S times.
                let key = (
                    &definition.path,
                    definition.span,
                    &definition.qualified_name,
                );
                let at = definitions.partition_point(|existing| {
                    (&existing.path, existing.span, &existing.qualified_name) <= key
                });
                definitions.insert(at, definition);
            }
            self.module_exports
                .insert(new.path.clone(), module_exports(new));
        }
    }

    pub fn replace_artifact_with_overlay(
        &mut self,
        old: Option<&FileArtifact>,
        new: Option<&FileArtifact>,
        overlay: &mut ResolutionUniverseOverlay,
    ) {
        let paths: BTreeSet<String> = old
            .into_iter()
            .map(|artifact| artifact.path.clone())
            .chain(new.into_iter().map(|artifact| artifact.path.clone()))
            .collect();
        let symbols: BTreeSet<String> = old
            .into_iter()
            .flat_map(|artifact| artifact.symbols.iter().map(|symbol| symbol.name.clone()))
            .chain(
                new.into_iter()
                    .flat_map(|artifact| artifact.symbols.iter().map(|symbol| symbol.name.clone())),
            )
            .collect();
        self.replace_artifact(old, new);
        for path in &paths {
            overlay
                .files
                .insert(path.clone(), self.files.contains(path));
        }
        for symbol in symbols {
            let definitions = self
                .symbol_definitions
                .get(&symbol)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let deltas = overlay.symbol_definitions.entry(symbol).or_default();
            for path in &paths {
                deltas.insert(
                    path.clone(),
                    definitions_in_path(definitions, path).to_vec(),
                );
            }
        }
        for path in old
            .into_iter()
            .map(|artifact| artifact.path.clone())
            .chain(new.into_iter().map(|artifact| artifact.path.clone()))
        {
            overlay
                .module_exports
                .insert(path.clone(), self.module_exports.get(&path).cloned());
        }
    }

    pub fn apply_overlay(&mut self, overlay: &ResolutionUniverseOverlay) {
        for (path, present) in &overlay.files {
            if *present {
                self.files.insert(path.clone());
            } else {
                self.files.remove(path);
            }
        }
        apply_definition_overlay(&mut self.symbol_definitions, &overlay.symbol_definitions);
        apply_optional_map(&mut self.module_exports, &overlay.module_exports);
    }

    pub fn into_shards(self, shard_bits: u8) -> Option<ResolutionUniverseShardSet> {
        if shard_bits > 16 {
            return None;
        }
        let mut set = ResolutionUniverseShardSet {
            format_version: self.format_version,
            resolver_fingerprint: self.resolver_fingerprint,
            shard_bits,
            ..ResolutionUniverseShardSet::default()
        };
        for path in self.files {
            set.shards
                .entry(resolution_shard_id(&path, shard_bits))
                .or_default()
                .files
                .insert(path);
        }
        for (name, definitions) in self.symbol_definitions {
            set.shards
                .entry(resolution_shard_id(&name, shard_bits))
                .or_default()
                .symbol_definitions
                .insert(name, definitions);
        }
        for (path, exports) in self.module_exports {
            set.shards
                .entry(resolution_shard_id(&path, shard_bits))
                .or_default()
                .module_exports
                .insert(path, exports);
        }
        Some(set)
    }
}

impl ResolutionLookup for ResolutionUniverse {
    fn matches(&self, config: &ResolverConfig) -> bool {
        ResolutionUniverse::matches(self, config)
    }

    fn contains_file(&self, path: &str) -> bool {
        self.files.contains(path)
    }

    fn symbol_definer_count(&self, name: &str) -> u32 {
        self.symbol_definitions.get(name).map_or(0, |definitions| {
            u32::try_from(definitions.len()).unwrap_or(u32::MAX)
        })
    }

    fn symbol_definitions(&self, name: &str) -> LookupSlice<'_, SymbolDefinition> {
        LookupSlice::Borrowed(
            self.symbol_definitions
                .get(name)
                .map(Vec::as_slice)
                .unwrap_or_default(),
        )
    }

    fn module_exports(&self, path: &str) -> LookupSlice<'_, ModuleExport> {
        LookupSlice::Borrowed(
            self.module_exports
                .get(path)
                .map(Vec::as_slice)
                .unwrap_or_default(),
        )
    }
}

pub(crate) fn resolution_shard_id(key: &str, bits: u8) -> u16 {
    if bits == 0 {
        return 0;
    }
    let digest = blake3::hash(key.as_bytes());
    u16::from_be_bytes([digest.as_bytes()[0], digest.as_bytes()[1]]) >> (16 - bits)
}

fn module_exports(artifact: &FileArtifact) -> Vec<ModuleExport> {
    artifact
        .exports
        .iter()
        .flat_map(|export| {
            export.bindings.iter().map(move |binding| ModuleExport {
                local: binding.local.clone(),
                exported: binding.exported.clone(),
                source: export.specifier.clone(),
                kind: binding.kind.clone(),
                type_only: binding.type_only,
            })
        })
        .collect()
}

/// Whether a binding of this kind reads from another module: `export {a} from`, `export * from`
/// and `export * as ns from`. A declaration or default export never has a source, whatever string
/// literal its body holds.
fn is_reexport_kind(kind: &ExportBindingKind) -> bool {
    matches!(
        kind,
        ExportBindingKind::Named | ExportBindingKind::Star | ExportBindingKind::Namespace
    )
}

/// The module a binding re-exports from, if it is a re-export at all.
fn reexport_source(export: &ModuleExport) -> Option<&str> {
    export
        .source
        .as_deref()
        .filter(|_| is_reexport_kind(&export.kind))
}

#[derive(Debug, Default)]
struct ResolvedImports {
    bindings: BTreeMap<String, Vec<SymbolDefinition>>,
    namespaces: BTreeMap<String, String>,
}

fn resolve_exported_symbol(
    root: &Path,
    file: &str,
    exported_name: &str,
    universe: &dyn ResolutionLookup,
    config: &ResolverConfig,
    visited: &mut BTreeSet<String>,
) -> Vec<SymbolDefinition> {
    let visit_key = format!("{file}\0{exported_name}");
    if !visited.insert(visit_key) {
        return Vec::new();
    }
    let mut targets = Vec::new();
    let exports = universe.module_exports(file);
    // ES ResolveExport: the module's own and indirect exports answer first. `export *` is only
    // consulted when none of them names `exported_name`, and never for `default`. Adding the
    // star targets alongside an explicit export made `export {foo} from './a'; export * from
    // './b'` ambiguous whenever `./b` also exported `foo`, and dropped the edge.
    let mut explicit = false;
    for export in exports
        .iter()
        .filter(|export| export.kind != ExportBindingKind::Star)
        .filter(|export| export.exported == exported_name)
    {
        explicit = true;
        if export.kind == ExportBindingKind::Namespace {
            // The namespace itself is a module object, not a declaration. A later member
            // reference can resolve through the source module without fabricating a symbol.
            continue;
        }
        match reexport_source(export) {
            Some(specifier) => {
                if let Some(target_file) =
                    resolve_one(root, file, specifier, universe, config).target
                {
                    targets.extend(resolve_exported_symbol(
                        root,
                        &target_file,
                        &export.local,
                        universe,
                        config,
                        visited,
                    ));
                }
            }
            None => targets.extend(definitions_in_file(universe, file, &export.local)),
        }
    }
    if !explicit && exported_name != "default" {
        for specifier in exports
            .iter()
            .filter(|export| export.kind == ExportBindingKind::Star)
            .filter_map(reexport_source)
        {
            if let Some(target_file) = resolve_one(root, file, specifier, universe, config).target {
                targets.extend(resolve_exported_symbol(
                    root,
                    &target_file,
                    exported_name,
                    universe,
                    config,
                    visited,
                ));
            }
        }
    }
    if targets.is_empty() {
        targets.extend(
            definitions_in_file(universe, file, exported_name)
                .into_iter()
                .filter(|definition| definition.exported),
        );
    }
    targets.sort_by(|left, right| left.id.cmp(&right.id));
    targets.dedup_by(|left, right| left.id == right.id);
    targets
}

fn resolve_exported_namespace(
    root: &Path,
    file: &str,
    exported_name: &str,
    universe: &dyn ResolutionLookup,
    config: &ResolverConfig,
) -> Option<String> {
    let exports = universe.module_exports(file);
    let matches: Vec<_> = exports
        .iter()
        .filter(|export| {
            export.kind == ExportBindingKind::Namespace && export.exported == exported_name
        })
        .filter_map(|export| export.source.as_deref())
        .filter_map(|specifier| resolve_one(root, file, specifier, universe, config).target)
        .collect();
    (matches.len() == 1).then(|| matches[0].clone())
}

fn definitions_in_file(
    universe: &dyn ResolutionLookup,
    file: &str,
    name: &str,
) -> Vec<SymbolDefinition> {
    universe.symbol_definitions_in_file(name, file)
}

/// TypeScript overloads, accessors, and declaration merging may produce several syntax nodes for
/// one logical declaration. They are unambiguous when path and qualified owner are identical.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RequiredNamespace {
    Any,
    Type,
    Value,
}

fn definition_matches_namespace(
    definition: &SymbolDefinition,
    required: RequiredNamespace,
) -> bool {
    required == RequiredNamespace::Any
        || match required {
            RequiredNamespace::Type => matches!(
                definition.kind.as_ref(),
                "interface_declaration"
                    | "type_alias_declaration"
                    | "class_declaration"
                    | "abstract_class_declaration"
                    | "class"
                    | "enum_declaration"
                    | "internal_module"
            ),
            RequiredNamespace::Value => {
                crate::model::symbol_semantic_namespace(definition.kind.as_ref()) == "value"
            }
            RequiredNamespace::Any => true,
        }
}

fn one_logical_definition_for(
    mut matches: Vec<SymbolDefinition>,
    required: RequiredNamespace,
) -> Option<SymbolDefinition> {
    matches.retain(|definition| definition_matches_namespace(definition, required));
    if required == RequiredNamespace::Type
        && matches.iter().any(|definition| {
            crate::model::symbol_semantic_namespace(definition.kind.as_ref()) == "value"
        })
    {
        // A class/enum plus an interface of the same name is declaration merging. Keep the
        // runtime-capable declaration as the shared graph identity for its type side as well.
        matches.retain(|definition| {
            crate::model::symbol_semantic_namespace(definition.kind.as_ref()) == "value"
        });
    }
    let first = matches.first()?;
    if matches.iter().any(|candidate| {
        candidate.path != first.path
            || candidate.qualified_name != first.qualified_name
            || candidate.scope != first.scope
            || crate::model::symbol_semantic_namespace(candidate.kind.as_ref())
                != crate::model::symbol_semantic_namespace(first.kind.as_ref())
    }) {
        return None;
    }
    // Implementations conventionally follow overload signatures; getter/setter source selection
    // is deterministic even though both map to the same logical graph node.
    matches.sort_by_key(|definition| definition.span);
    matches.pop()
}

fn find_qualified_definition_for(
    universe: &dyn ResolutionLookup,
    path: &str,
    qualified_name: &str,
    required: RequiredNamespace,
) -> Option<SymbolDefinition> {
    let leaf = qualified_name.rsplit('.').next().unwrap_or(qualified_name);
    let mut matches = universe.symbol_definitions_in_file(leaf, path);
    matches.retain(|definition| definition.qualified_name == qualified_name);
    one_logical_definition_for(matches, required)
}

fn visible_local_definition(
    definition: &SymbolDefinition,
    source: Option<&crate::model::Symbol>,
    reference_span: Span,
) -> bool {
    if let Some(scope) = definition.scope {
        if reference_span.start_byte < scope.start_byte || reference_span.end_byte > scope.end_byte
        {
            return false;
        }
        if !matches!(
            definition.kind.as_ref(),
            "function_declaration" | "function"
        ) && reference_span.start_byte < definition.span.start_byte
        {
            return false;
        }
    }
    if definition.qualified_name == definition.name {
        return true;
    }
    // Class and interface members (and the members of an object type) are reached through
    // `this`, an instance, or the owner itself -- never as a bare name, even inside the owner.
    // Enum members are the exception: a later initializer names an earlier member bare.
    if matches!(definition.kind.as_ref(), "method" | "property") {
        return false;
    }
    let Some(source) = source else {
        return false;
    };
    let Some(owner) = definition
        .qualified_name
        .strip_suffix(&format!(".{}", definition.name))
    else {
        return false;
    };
    source.qualified_name == owner
        || source
            .qualified_name
            .strip_prefix(owner)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

fn select_visible_local_definition(
    matches: Vec<SymbolDefinition>,
    source: Option<&crate::model::Symbol>,
    reference_span: Span,
    required: RequiredNamespace,
) -> Option<SymbolDefinition> {
    let visible = matches
        .into_iter()
        .filter(|definition| visible_local_definition(definition, source, reference_span))
        .filter(|definition| definition_matches_namespace(definition, required))
        .collect::<Vec<_>>();
    let smallest_scope = visible
        .iter()
        .filter_map(|definition| definition.scope)
        .map(|scope| scope.end_byte.saturating_sub(scope.start_byte))
        .min();
    let closest = visible
        .into_iter()
        .filter(|definition| {
            smallest_scope.is_none_or(|size| {
                definition
                    .scope
                    .is_some_and(|scope| scope.end_byte.saturating_sub(scope.start_byte) == size)
            })
        })
        .collect();
    one_logical_definition_for(closest, required)
}

/// Node-local lookups built once per artifact instead of re-scanned per reference.
/// Every map preserves the selection the linear scans made: `find` kept the first
/// match, so first insert wins; the owner scan took `max_by_key` on qualified-name
/// length, whose ties resolve to the last candidate, so owners let later inserts win.
struct ArtifactLookups<'a> {
    artifact: &'a FileArtifact,
    /// Every reference needs its source symbol, so this one always pays off.
    by_id: FxHashMap<&'a str, &'a crate::model::Symbol>,
    /// The remaining three serve `this.<field>.<member>` only. Most files have no
    /// such reference, and building maps they never consult cost more than it
    /// saved, so they are filled on first use.
    member_maps: std::cell::OnceCell<MemberLookups<'a>>,
}

struct MemberLookups<'a> {
    by_qualified: FxHashMap<&'a str, &'a crate::model::Symbol>,
    owners_by_qualified: FxHashMap<&'a str, &'a crate::model::Symbol>,
    declared_type_by_symbol: FxHashMap<&'a str, &'a str>,
}

impl<'a> ArtifactLookups<'a> {
    fn build(artifact: &'a FileArtifact) -> Self {
        let mut by_id =
            FxHashMap::with_capacity_and_hasher(artifact.symbols.len(), rustc_hash::FxBuildHasher);
        for symbol in &artifact.symbols {
            by_id.entry(symbol.id.as_str()).or_insert(symbol);
        }
        Self {
            artifact,
            by_id,
            member_maps: std::cell::OnceCell::new(),
        }
    }

    fn member_maps(&self) -> &MemberLookups<'a> {
        self.member_maps.get_or_init(|| {
            let mut maps = MemberLookups {
                by_qualified: FxHashMap::default(),
                owners_by_qualified: FxHashMap::default(),
                declared_type_by_symbol: FxHashMap::default(),
            };
            for symbol in &self.artifact.symbols {
                maps.by_qualified
                    .entry(symbol.qualified_name.as_str())
                    .or_insert(symbol);
                if matches!(
                    symbol.kind.as_ref(),
                    "class_declaration" | "interface_declaration"
                ) {
                    maps.owners_by_qualified
                        .insert(symbol.qualified_name.as_str(), symbol);
                }
            }
            for reference in &self.artifact.symbol_refs {
                if reference.kind == EdgeKind::TypeOf {
                    maps.declared_type_by_symbol
                        .entry(reference.from_id.as_str())
                        .or_insert(reference.to.as_str());
                }
            }
            maps
        })
    }

    /// Nearest class/interface ancestor of `qualified_name`, itself included.
    /// Walking the dotted prefixes longest-first is what the old scan achieved by
    /// filtering every symbol and taking the longest qualified-name match.
    fn enclosing_owner(&self, qualified_name: &str) -> Option<&'a crate::model::Symbol> {
        let owners = &self.member_maps().owners_by_qualified;
        let mut candidate = qualified_name;
        loop {
            if let Some(owner) = owners.get(candidate) {
                return Some(owner);
            }
            candidate = candidate.rsplit_once('.')?.0;
        }
    }
}

fn source_definition_is_type_only(lookups: &ArtifactLookups<'_>, id: &str) -> bool {
    lookups.by_id.get(id).is_some_and(|symbol| {
        matches!(
            symbol.kind.as_ref(),
            "interface_declaration" | "type_alias_declaration"
        )
    })
}

fn resolve_symbol_reference(
    root: &Path,
    artifact: &FileArtifact,
    lookups: &ArtifactLookups<'_>,
    reference: &SymbolRef,
    imports: Option<&ResolvedImports>,
    universe: &dyn ResolutionLookup,
    config: &ResolverConfig,
) -> Option<SymbolDefinition> {
    let raw = reference.to.as_str();
    let source_definition = lookups.by_id.get(reference.from_id.as_str()).copied();
    let required = match reference.kind {
        EdgeKind::TypeOf | EdgeKind::Implements => RequiredNamespace::Type,
        EdgeKind::Extends
            if source_definition.is_some_and(|source| {
                matches!(
                    source.kind.as_ref(),
                    "interface_declaration" | "type_alias_declaration"
                )
            }) =>
        {
            RequiredNamespace::Type
        }
        _ => RequiredNamespace::Value,
    };

    if let Some(member) = raw.strip_prefix("this.") {
        let source = source_definition?;
        let owner = lookups.enclosing_owner(&source.qualified_name)?;
        if let Some(definition) = find_qualified_definition_for(
            universe,
            &artifact.path,
            &format!("{}.{}", owner.qualified_name, member),
            required,
        ) {
            return Some(definition);
        }
        // `this.<field>.<member>` where the member is not declared on the owner:
        // hop through the field's declared type annotation (constructor-injected
        // services, typed properties) and resolve the member on that type.
        let (field, rest) = member.split_once('.')?;
        let field_qualified = format!("{}.{}", owner.qualified_name, field);
        let member_maps = lookups.member_maps();
        let field_symbol = member_maps
            .by_qualified
            .get(field_qualified.as_str())
            .copied()?;
        let declared_type = *member_maps
            .declared_type_by_symbol
            .get(field_symbol.id.as_str())?;
        let type_reference = SymbolRef {
            from_id: field_symbol.id.clone(),
            to: declared_type.into(),
            kind: EdgeKind::TypeOf,
            span: reference.span,
        };
        let type_definition = resolve_symbol_reference(
            root,
            artifact,
            lookups,
            &type_reference,
            imports,
            universe,
            config,
        )?;
        return find_qualified_definition_for(
            universe,
            &type_definition.path,
            &format!("{}.{}", type_definition.qualified_name, rest),
            RequiredNamespace::Value,
        );
    }

    if let Some((head, tail)) = raw.split_once('.') {
        let local_head = select_visible_local_definition(
            definitions_in_file(universe, &artifact.path, head),
            source_definition,
            reference.span,
            RequiredNamespace::Value,
        );
        if let Some(local_head) = local_head {
            return find_qualified_definition_for(
                universe,
                &artifact.path,
                &format!("{}.{}", local_head.qualified_name, tail),
                required,
            );
        }
        if let Some(imports) = imports {
            if let Some(namespace_file) = imports.namespaces.get(head) {
                let (exported, remainder) = tail
                    .split_once('.')
                    .map_or((tail, None), |(first, rest)| (first, Some(rest)));
                let targets = resolve_exported_symbol(
                    root,
                    namespace_file,
                    exported,
                    universe,
                    config,
                    &mut BTreeSet::new(),
                );
                if let Some(target) = one_logical_definition_for(targets, required) {
                    return remainder.map_or(Some(target.clone()), |member| {
                        find_qualified_definition_for(
                            universe,
                            &target.path,
                            &format!("{}.{}", target.qualified_name, member),
                            required,
                        )
                    });
                }
                return None;
            }
            if let Some(targets) = imports.bindings.get(head)
                && let Some(target) = one_logical_definition_for(targets.clone(), required)
            {
                return find_qualified_definition_for(
                    universe,
                    &target.path,
                    &format!("{}.{}", target.qualified_name, tail),
                    required,
                );
            }
        }
        if let Some(target) = find_qualified_definition_for(universe, &artifact.path, raw, required)
        {
            return Some(target);
        }
        return None;
    }

    select_visible_local_definition(
        definitions_in_file(universe, &artifact.path, raw),
        source_definition,
        reference.span,
        required,
    )
    .or_else(|| {
        imports
            .and_then(|imports| imports.bindings.get(raw))
            .and_then(|definitions| one_logical_definition_for(definitions.clone(), required))
    })
}

fn apply_optional_map<V: Clone>(
    target: &mut BTreeMap<String, V>,
    overlay: &BTreeMap<String, Option<V>>,
) {
    for (key, value) in overlay {
        if let Some(value) = value {
            target.insert(key.clone(), value.clone());
        } else {
            target.remove(key);
        }
    }
}

impl ReverseIndex {
    pub fn affected_by(&self, changed: &str) -> Vec<String> {
        self.dependents
            .get(changed)
            .map(|values| values.iter().cloned().collect())
            .unwrap_or_default()
    }
    pub fn rebuild(&mut self, edges: &[Edge]) {
        self.dependents.clear();
        for edge in edges {
            self.dependents
                .entry(edge.to.clone())
                .or_default()
                .insert(edge.from.clone());
        }
    }
}

pub fn resolve_artifacts(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
) -> (Vec<Edge>, Vec<Resolution>, ReverseIndex) {
    let (edges, resolutions, reverse, _, _) =
        resolve_artifacts_impl(root, artifacts, config, true, false, false, None);
    (edges, resolutions, reverse)
}

/// Production indexing path: return only the graph edges. Diagnostic resolutions and the
/// reverse string index are intentionally skipped because the engine never consumes them.
pub fn resolve_edges(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
) -> Vec<Edge> {
    resolve_artifacts_impl(root, artifacts, config, false, false, false, None).0
}

/// Resolve graph edges and return the exact path/basename probes used for incremental
/// invalidation. This is the structural-index build path; ordinary full indexing can skip traces.
pub fn resolve_edges_with_traces(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
) -> (Vec<Edge>, Vec<ResolutionTrace>) {
    let (edges, _, _, traces, _) =
        resolve_artifacts_impl(root, artifacts, config, false, true, false, None);
    (edges, traces)
}

pub fn resolve_edges_with_contributions(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
) -> (Vec<Edge>, BTreeMap<String, Vec<Edge>>) {
    let (edges, _, _, _, contributions) =
        resolve_artifacts_impl(root, artifacts, config, false, false, true, None);
    (edges, contributions)
}

pub type StructuralResolutionData = (Vec<Edge>, Vec<ResolutionTrace>, ResolutionUniverse);

pub fn resolve_edges_with_structural_data(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
) -> StructuralResolutionData {
    let universe = ResolutionUniverse::build(artifacts, config);
    let (edges, _, _, traces, _) =
        resolve_artifacts_impl(root, artifacts, config, false, true, false, Some(&universe));
    (edges, traces, universe)
}

pub type EdgeContributions = BTreeMap<String, Vec<Edge>>;
pub type SubsetResolution = Option<(Vec<Edge>, EdgeContributions)>;

/// Resolve only the supplied artifacts against persisted workspace-wide membership/counts.
/// A fingerprint mismatch is explicit so callers can fall back to full resolution.
pub fn resolve_subset_with_contributions(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    universe: &dyn ResolutionLookup,
    config: &ResolverConfig,
) -> SubsetResolution {
    if !universe.matches(config) {
        return None;
    }
    let (edges, _, _, _, contributions) =
        resolve_artifacts_impl(root, artifacts, config, false, false, true, Some(universe));
    Some((edges, contributions))
}

pub fn resolve_subset_with_structural_data(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    universe: &dyn ResolutionLookup,
    config: &ResolverConfig,
) -> Option<(Vec<ResolutionTrace>, EdgeContributions)> {
    if !universe.matches(config) {
        return None;
    }
    let (_, _, _, traces, contributions) =
        resolve_artifacts_impl(root, artifacts, config, false, true, true, Some(universe));
    Some((traces, contributions))
}

pub fn resolver_fingerprint(config: &ResolverConfig) -> String {
    let mut hasher = blake3::Hasher::new();
    // v3: a file's contribution also records each segment of a member reference, so an index
    // built before that must take the rebuild tier once rather than trust its referrer sets.
    hasher.update(b"ravel-resolver-v3\0");
    if let Ok(bytes) = bincode::serialize(config) {
        hasher.update(&bytes);
    }
    hasher.finalize().to_hex().to_string()
}

type ResolveArtifactsOutput = (
    Vec<Edge>,
    Vec<Resolution>,
    ReverseIndex,
    Vec<ResolutionTrace>,
    BTreeMap<String, Vec<Edge>>,
);

fn resolve_artifacts_impl(
    root: &Path,
    artifacts: &BTreeMap<String, FileArtifact>,
    config: &ResolverConfig,
    collect_auxiliary: bool,
    collect_traces: bool,
    collect_contributions: bool,
    persisted_universe: Option<&dyn ResolutionLookup>,
) -> ResolveArtifactsOutput {
    let built_universe;
    let universe: &dyn ResolutionLookup = if let Some(universe) = persisted_universe {
        universe
    } else {
        let universe_started = std::time::Instant::now();
        built_universe = ResolutionUniverse::build(artifacts, config);
        crate::timing::stage("resolve.universe_build", universe_started, String::new);
        &built_universe
    };
    use rayon::prelude::*;
    // Per-artifact resolution only reads the shared universe, so fan it out across cores.
    // Results are merged in BTreeMap order below, which keeps edge/trace/contribution
    // ordering identical to the sequential implementation.
    let imports_started = std::time::Instant::now();
    let per_artifact: Vec<_> = artifacts
        .values()
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|artifact| {
            let mut edges = Vec::new();
            let mut resolutions = Vec::new();
            let mut traces = Vec::new();
            let mut contributions: BTreeMap<String, Vec<Edge>> = BTreeMap::new();
            let mut imported_bindings: BTreeMap<String, ResolvedImports> = BTreeMap::new();
            for import in &artifact.imports {
                let resolution =
                    resolve_one(root, &artifact.path, &import.specifier, universe, config);
                let (confidence, target) = match resolution.target.clone() {
                    Some(target) => (
                        EdgeConfidence::Resolved {
                            score: 1.0,
                            reason: resolution.reason.clone(),
                        },
                        Some(target),
                    ),
                    None if !resolution.candidates.is_empty() => (
                        EdgeConfidence::Candidate {
                            score: 0.5,
                            reason: resolution.reason.clone(),
                        },
                        None,
                    ),
                    None => (
                        EdgeConfidence::Unresolved {
                            score: 0.0,
                            reason: resolution.reason.clone(),
                        },
                        None,
                    ),
                };
                let resolved_target = target.clone();
                let edge = Edge {
                    from: artifact.path.clone(),
                    to: target.unwrap_or_else(|| import.specifier.clone()),
                    kind: EdgeKind::Import,
                    confidence,
                    type_only: import.type_only,
                    source_path: Some(artifact.path.clone()),
                    span: Some(import.span),
                    provenance: EdgeProvenance::Resolution,
                };
                if collect_contributions {
                    contributions
                        .entry(artifact.path.clone())
                        .or_default()
                        .push(edge.clone());
                }
                edges.push(edge);
                if let Some(target_path) = resolved_target {
                    let resolutions_for_file =
                        imported_bindings.entry(artifact.path.clone()).or_default();
                    for binding in &import.bindings {
                        if matches!(
                            binding.kind,
                            ImportBindingKind::Namespace | ImportBindingKind::ImportEquals
                        ) {
                            resolutions_for_file
                                .namespaces
                                .insert(binding.local.clone(), target_path.clone());
                            continue;
                        }
                        let exported = if binding.kind == ImportBindingKind::Default {
                            "default"
                        } else {
                            binding.imported.as_str()
                        };
                        if let Some(namespace_file) = resolve_exported_namespace(
                            root,
                            &target_path,
                            exported,
                            universe,
                            config,
                        ) {
                            resolutions_for_file
                                .namespaces
                                .insert(binding.local.clone(), namespace_file);
                            continue;
                        }
                        let targets = resolve_exported_symbol(
                            root,
                            &target_path,
                            exported,
                            universe,
                            config,
                            &mut BTreeSet::new(),
                        );
                        let mut selected = Vec::new();
                        if let Some(target) =
                            one_logical_definition_for(targets.clone(), RequiredNamespace::Type)
                        {
                            selected.push(target);
                        }
                        if !binding.type_only
                            && let Some(target) =
                                one_logical_definition_for(targets, RequiredNamespace::Value)
                            && selected.iter().all(|existing| existing.id != target.id)
                        {
                            selected.push(target);
                        }
                        if !selected.is_empty() {
                            resolutions_for_file
                                .bindings
                                .insert(binding.local.clone(), selected.clone());
                        }
                        for target in selected {
                            let symbol_edge = Edge {
                                from: artifact.path.clone(),
                                to: target.id,
                                kind: EdgeKind::Import,
                                confidence: EdgeConfidence::Resolved {
                                    score: 1.0,
                                    reason: Arc::clone(&UNIQUE_SYMBOL),
                                },
                                type_only: binding.type_only
                                    || crate::model::symbol_semantic_namespace(
                                        target.kind.as_ref(),
                                    ) == "type",
                                source_path: Some(artifact.path.clone()),
                                span: Some(binding.span),
                                provenance: EdgeProvenance::Resolution,
                            };
                            if collect_contributions {
                                contributions
                                    .entry(artifact.path.clone())
                                    .or_default()
                                    .push(symbol_edge.clone());
                            }
                            edges.push(symbol_edge);
                        }
                    }
                }
                if collect_auxiliary {
                    resolutions.push(resolution.diagnostic(&import.specifier));
                }
                if collect_traces {
                    traces.push(resolution.trace(&artifact.path, &import.specifier));
                }
            }
            for export in &artifact.exports {
                // `export {} from './x'` has no bindings and still names a module; an export whose
                // bindings are all declarations or defaults never does.
                let reads_source = export.bindings.is_empty()
                    || export
                        .bindings
                        .iter()
                        .any(|binding| is_reexport_kind(&binding.kind));
                if let Some(specifier) = export.specifier.as_ref().filter(|_| reads_source) {
                    let resolution = resolve_one(root, &artifact.path, specifier, universe, config);
                    let (confidence, target) = match resolution.target.clone() {
                        Some(target) => (
                            EdgeConfidence::Resolved {
                                score: 1.0,
                                reason: resolution.reason.clone(),
                            },
                            target,
                        ),
                        None if !resolution.candidates.is_empty() => (
                            EdgeConfidence::Candidate {
                                score: 0.5,
                                reason: resolution.reason.clone(),
                            },
                            specifier.clone(),
                        ),
                        None => (
                            EdgeConfidence::Unresolved {
                                score: 0.0,
                                reason: resolution.reason.clone(),
                            },
                            specifier.clone(),
                        ),
                    };
                    let edge = Edge {
                        from: artifact.path.clone(),
                        to: target.clone(),
                        kind: EdgeKind::ReExport,
                        confidence,
                        type_only: export.type_only,
                        source_path: Some(artifact.path.clone()),
                        span: Some(export.span),
                        provenance: EdgeProvenance::Resolution,
                    };
                    if collect_contributions {
                        contributions
                            .entry(artifact.path.clone())
                            .or_default()
                            .push(edge.clone());
                    }
                    edges.push(edge);
                    for binding in &export.bindings {
                        if matches!(
                            binding.kind,
                            ExportBindingKind::Star | ExportBindingKind::Namespace
                        ) {
                            continue;
                        }
                        let targets = resolve_exported_symbol(
                            root,
                            &target,
                            &binding.local,
                            universe,
                            config,
                            &mut BTreeSet::new(),
                        );
                        let required = if binding.type_only {
                            RequiredNamespace::Type
                        } else {
                            RequiredNamespace::Value
                        };
                        if let Some(target) = one_logical_definition_for(targets, required) {
                            let symbol_edge = Edge {
                                from: artifact.path.clone(),
                                to: target.id,
                                kind: EdgeKind::ReExport,
                                confidence: EdgeConfidence::Resolved {
                                    score: 1.0,
                                    reason: Arc::clone(&UNIQUE_SYMBOL),
                                },
                                type_only: binding.type_only,
                                source_path: Some(artifact.path.clone()),
                                span: Some(binding.span),
                                provenance: EdgeProvenance::Resolution,
                            };
                            if collect_contributions {
                                contributions
                                    .entry(artifact.path.clone())
                                    .or_default()
                                    .push(symbol_edge.clone());
                            }
                            edges.push(symbol_edge);
                        }
                    }
                    if collect_auxiliary {
                        resolutions.push(resolution.diagnostic(specifier));
                    }
                    if collect_traces {
                        traces.push(resolution.trace(&artifact.path, specifier));
                    }
                }
            }
            (edges, resolutions, traces, contributions, imported_bindings)
        })
        .collect();
    let mut edges = Vec::new();
    let mut resolutions = Vec::new();
    let mut traces = Vec::new();
    let mut contributions: BTreeMap<String, Vec<Edge>> = BTreeMap::new();
    let mut imported_bindings: BTreeMap<String, ResolvedImports> = BTreeMap::new();
    for (mut file_edges, file_resolutions, file_traces, file_contributions, file_bindings) in
        per_artifact
    {
        edges.append(&mut file_edges);
        resolutions.extend(file_resolutions);
        traces.extend(file_traces);
        contributions.extend(file_contributions);
        imported_bindings.extend(file_bindings);
    }
    // Symbol-level edges use stable declaration ids. Resolution is conservative: explicit import
    // bindings and same-file ownership win; ambiguous workspace names do not become graph edges.
    // References resolve independently per artifact; the cross-file dedup happens on the ordered
    // merge below, so results match the sequential implementation exactly.
    type RefEdgeKey = (String, String, EdgeKind, Span);
    crate::timing::stage("resolve.imports_exports", imports_started, String::new);
    let refs_started = std::time::Instant::now();
    let ref_edges: Vec<Vec<(RefEdgeKey, Edge)>> = artifacts
        .values()
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|artifact| {
            let imports = imported_bindings.get(&artifact.path);
            // One pass over the artifact's symbols and refs, then O(1) lookups per
            // reference: the scans this replaces were O(refs × symbols) per file.
            let lookups = ArtifactLookups::build(artifact);
            let mut out = Vec::new();
            for r in &artifact.symbol_refs {
                let Some(target) = resolve_symbol_reference(
                    root, artifact, &lookups, r, imports, universe, config,
                ) else {
                    continue;
                };
                let from = r.from_id.clone();
                if from == target.id {
                    continue; // self-reference
                }
                let confidence = EdgeConfidence::Resolved {
                    score: 1.0,
                    reason: Arc::clone(&UNIQUE_SYMBOL),
                };
                let type_only = matches!(r.kind, EdgeKind::TypeOf | EdgeKind::Implements)
                    || (r.kind == EdgeKind::Extends
                        && source_definition_is_type_only(&lookups, &from));
                let edge = Edge {
                    from: from.clone(),
                    to: target.id.clone(),
                    kind: r.kind.clone(),
                    confidence,
                    type_only,
                    source_path: Some(artifact.path.clone()),
                    span: Some(r.span),
                    provenance: EdgeProvenance::Ast,
                };
                out.push(((from, target.id, r.kind.clone(), r.span), edge));
            }
            out
        })
        .collect();
    crate::timing::stage("resolve.symbol_refs", refs_started, String::new);
    let merge_started = std::time::Instant::now();
    let mut seen: FxHashSet<(String, String, EdgeKind, Span)> = FxHashSet::default();
    for (artifact, file_refs) in artifacts.values().zip(ref_edges) {
        for (key, edge) in file_refs {
            if collect_contributions {
                contributions
                    .entry(artifact.path.clone())
                    .or_default()
                    .push(edge.clone());
            }
            if seen.insert(key) {
                edges.push(edge);
            }
        }
    }

    crate::timing::stage("resolve.merge_dedup", merge_started, String::new);
    let sort_started = std::time::Instant::now();
    // Same comparator, same stability: rayon's `par_sort_by` is a stable sort, and
    // ordering three quarters of a million edges by their string endpoints is the
    // last sequential step of resolution.
    edges.par_sort_by(|a, b| (&a.from, &a.to, &a.kind).cmp(&(&b.from, &b.to, &b.kind)));
    crate::timing::stage("resolve.sort_edges", sort_started, String::new);
    let mut reverse = ReverseIndex::default();
    if collect_auxiliary {
        reverse.rebuild(&edges);
    }
    for owned in contributions.values_mut() {
        owned.sort_by(|a, b| (&a.from, &a.to, &a.kind).cmp(&(&b.from, &b.to, &b.kind)));
    }
    (edges, resolutions, reverse, traces, contributions)
}

fn resolve_one(
    root: &Path,
    importer: &str,
    specifier: &str,
    universe: &dyn ResolutionLookup,
    config: &ResolverConfig,
) -> ResolutionCore {
    let importer_path = Path::new(importer);
    let mut candidates = Vec::new();
    let mut attempted_paths = BTreeSet::new();
    let basename_keys = BTreeSet::new();
    if specifier.starts_with('.') {
        let base = root
            .join(importer_path)
            .parent()
            .unwrap_or(root)
            .join(specifier);
        let probe = file_candidates(root, &base, config, universe);
        candidates.extend(probe.existing);
        attempted_paths.extend(probe.attempted);
    }
    // tsc consults `baseUrl` only when no `paths` pattern matched: a matched pattern whose
    // targets are all missing is an unresolved import, not a cue to look somewhere else.
    let mut alias_matched = false;
    if candidates.is_empty() && !specifier.starts_with('.') {
        let matched = config
            .paths
            .iter()
            .filter_map(|(alias, targets)| {
                match_path_alias(alias, specifier).map(|capture| (alias, targets, capture))
            })
            .max_by(|(left, ..), (right, ..)| {
                path_alias_specificity(left).cmp(&path_alias_specificity(right))
            });
        if let Some((_, targets, capture)) = matched {
            alias_matched = true;
            for target in targets {
                let path = if target.contains('*') {
                    target.replace('*', &capture)
                } else {
                    target.clone()
                };
                let probe = file_candidates(root, &root.join(path), config, universe);
                candidates.extend(probe.existing);
                attempted_paths.extend(probe.attempted);
            }
        }
    }
    if candidates.is_empty()
        && !alias_matched
        && !specifier.starts_with('.')
        && let Some(base) = &config.base_url
    {
        let probe = file_candidates(root, &root.join(base).join(specifier), config, universe);
        candidates.extend(probe.existing);
        attempted_paths.extend(probe.attempted);
    }
    // Preserve TypeScript-like probe/paths order. Sorting candidates used to turn resolution into
    // an arbitrary lexicographic choice (for example preferring `.js` over `.ts`).
    let mut seen_candidates = FxHashSet::default();
    candidates.retain(|candidate| seen_candidates.insert(candidate.clone()));
    candidates.truncate(config.max_candidates.max(1));
    let target = candidates
        .first()
        .filter(|candidate| universe.contains_file(candidate))
        .cloned();
    let (confidence, reason) = if target.is_some() {
        ("resolved", Arc::clone(&MATCHED_FILE))
    } else if candidates.is_empty() {
        ("unresolved", Arc::clone(&NO_CANDIDATE))
    } else {
        ("candidate", Arc::clone(&STALE_CANDIDATE))
    };
    ResolutionCore {
        target,
        candidates,
        confidence,
        reason,
        attempted_paths,
        basename_keys,
    }
}

fn match_path_alias(alias: &str, specifier: &str) -> Option<String> {
    let Some((prefix, suffix)) = alias.split_once('*') else {
        return (alias == specifier).then(String::new);
    };
    if !specifier.starts_with(prefix)
        || !specifier.ends_with(suffix)
        || specifier.len() < prefix.len() + suffix.len()
    {
        return None;
    }
    Some(specifier[prefix.len()..specifier.len() - suffix.len()].to_owned())
}

fn path_alias_specificity(alias: &str) -> (bool, usize, usize) {
    alias
        .split_once('*')
        .map_or((true, alias.len(), 0), |(prefix, suffix)| {
            (false, prefix.len(), suffix.len())
        })
}

/// Extensions a specifier can carry that name a JS/TS module, and so get replaced rather than
/// appended to.
const DEFAULT_RESOLVE_EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// Extensions appended to an extensionless specifier (and to `index`), in order. Declaration files
/// are indexed, so `./types` must reach `types.d.ts`; TypeScript tries it right after `.tsx`.
const DEFAULT_PROBE_EXTS: &[&str] = &["ts", "tsx", "d.ts", "mts", "cts", "js", "jsx", "mjs", "cjs"];

struct CandidateProbe {
    existing: Vec<String>,
    attempted: BTreeSet<String>,
}

fn file_candidates(
    root: &Path,
    base: &Path,
    config: &ResolverConfig,
    universe: &dyn ResolutionLookup,
) -> CandidateProbe {
    let mut existing = Vec::new();
    let mut attempted = BTreeSet::new();
    // Every probe up to the first hit is recorded: a file appearing at any of them later changes
    // the answer, which is what incremental invalidation keys on.
    let mut probe = |path: &Path| {
        let normalized = normalize_lexical(root, path);
        attempted.insert(normalized.clone());
        let found = universe.contains_file(&normalized);
        if found {
            existing.push(normalized);
        }
        found
    };
    // Iterate config extensions by reference; fall back to a static default set — no per-call
    // `Vec<String>` clone/allocation.
    let default_extensions = config.extensions.is_empty();
    let extensions = || {
        config.extensions.iter().map(String::as_str).chain(
            DEFAULT_PROBE_EXTS
                .iter()
                .copied()
                .filter(move |_| default_extensions),
        )
    };
    let source_extension = base
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| DEFAULT_RESOLVE_EXTS.contains(value));
    let found = match source_extension {
        // A JS/TS extension is replaced the way TypeScript replaces it, so `./util.js` reaches
        // `util.ts` ahead of an emitted `util.js` beside it; every other extension stays a lenient
        // fallback after those.
        Some(original) => {
            let substitutions = typescript_substitutions(original);
            substitutions
                .iter()
                .any(|extension| probe(&base.with_extension(extension)))
                || extensions()
                    .filter(|extension| !substitutions.contains(extension))
                    .any(|extension| probe(&base.with_extension(extension)))
        }
        None => {
            probe(base)
                || extensions().any(|extension| {
                    probe(&PathBuf::from(format!(
                        "{}.{extension}",
                        base.to_string_lossy()
                    )))
                })
        }
    };
    if !found {
        DEFAULT_PROBE_EXTS
            .iter()
            .any(|extension| probe(&base.join(format!("index.{extension}"))));
    }
    CandidateProbe {
        existing,
        attempted,
    }
}

/// What TypeScript tries in place of a specifier's own JS/TS extension (`tryAddingExtensions`):
/// the TypeScript source, then its declaration file, then the JavaScript file.
fn typescript_substitutions(original: &str) -> &'static [&'static str] {
    match original {
        "tsx" | "jsx" => &["tsx", "ts", "d.ts", "jsx", "js"],
        "mts" | "mjs" => &["mts", "d.mts", "mjs"],
        "cts" | "cjs" => &["cts", "d.cts", "cjs"],
        _ => &["ts", "tsx", "d.ts", "js", "jsx"],
    }
}

fn normalize_lexical(root: &Path, path: &Path) -> String {
    let root = normalize_path_components(root);
    let path = normalize_path_components(path);
    let relative = path.strip_prefix(&root).unwrap_or(&path);
    let text = relative.to_string_lossy();
    if text.contains('\\') {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}
fn normalize_path_components(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(value) => normalized.push(value),
            std::path::Component::RootDir => normalized.push(component.as_os_str()),
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
        }
    }
    normalized
}

pub fn load_tsconfig(root: &Path) -> ResolverConfig {
    load_tsconfig_reporting(root).config
}

/// A tsconfig that exists but cannot be parsed and one that is absent produce the same empty
/// config -- no `baseUrl`, no `paths`. On an alias-heavy workspace that means every aliased import
/// resolves to nothing, `callers_of` understates permanently, and the index reports itself healthy:
/// same file count, zero parse errors. One stray character is enough. Reporting the problem is the
/// difference between a caller seeing a smaller answer and a caller seeing a wrong one.
pub fn load_tsconfig_reporting(root: &Path) -> LoadedResolverConfig {
    let path = root.join("tsconfig.json");
    let mut problems = Vec::new();
    let config = load_tsconfig_recursive(root, &path, &mut BTreeSet::new(), &mut problems)
        .map(|layer| layer.into_config(root));
    if path.exists() && !path.is_file() {
        // A directory named `tsconfig.json` reads as "no config" to every layer below.
        problems.push(crate::model::Diagnostic {
            code: "tsconfig_not_a_file".into(),
            message: "tsconfig.json exists but is not a regular file; no baseUrl or path aliases are applied".into(),
            path: Some("tsconfig.json".into()),
            span: None,
        });
    } else if config.is_none() && path.is_file() {
        problems.push(crate::model::Diagnostic {
            code: "tsconfig_unparsed".into(),
            message: "tsconfig.json exists but could not be parsed; path aliases and baseUrl are not applied, so aliased imports resolve to nothing"
                .into(),
            path: Some("tsconfig.json".into()),
            span: None,
        });
    }
    LoadedResolverConfig {
        config: config.unwrap_or_default(),
        problems,
    }
}

/// A resolver config plus whatever went wrong producing it.
#[derive(Debug, Clone, Default)]
pub struct LoadedResolverConfig {
    pub config: ResolverConfig,
    pub problems: Vec<crate::model::Diagnostic>,
}

/// One config of an `extends` chain as tsc merges it, before `paths` are resolved. tsc keeps
/// `paths` as written and resolves them against the *final* `baseUrl` -- or, with none, against the
/// directory of the config that defined them -- so they can only be resolved once the whole chain
/// is merged. Resolving them while loading each base pinned an inherited `paths` to the base's own
/// directory even when the top config set `baseUrl`.
#[derive(Default)]
struct TsconfigLayer {
    /// Root-relative and normalized, like [`ResolverConfig::base_url`].
    base_url: Option<PathBuf>,
    /// `paths` as written, with the directory of the config that defined them.
    paths: Option<(BTreeMap<String, Vec<String>>, PathBuf)>,
    extensions: Vec<String>,
    max_candidates: usize,
}

impl TsconfigLayer {
    fn into_config(self, root: &Path) -> ResolverConfig {
        let base_url = self.base_url;
        let paths = self
            .paths
            .map(|(raw, defined_in)| {
                let target_base = base_url
                    .as_ref()
                    .map(|base| root.join(base))
                    .unwrap_or(defined_in);
                raw.into_iter()
                    .map(|(alias, targets)| {
                        let targets = targets
                            .iter()
                            .map(|target| {
                                normalize_lexical(
                                    root,
                                    &target_base.join(substitute_config_dir(root, target)),
                                )
                            })
                            .collect();
                        (alias, targets)
                    })
                    .collect()
            })
            .unwrap_or_default();
        ResolverConfig {
            base_url,
            paths,
            extensions: self.extensions,
            max_candidates: self.max_candidates,
        }
    }
}

/// tsc's `${configDir}` template: a path option starting with it is taken relative to the
/// directory of the config being compiled -- the workspace's own `tsconfig.json` -- whichever
/// config in the chain wrote it.
fn substitute_config_dir(root: &Path, value: &str) -> PathBuf {
    match value.strip_prefix("${configDir}") {
        Some(rest) => root.join(rest.trim_start_matches(['/', '\\'])),
        None => PathBuf::from(value),
    }
}

/// `stack` holds the configs being loaded right now, not every config loaded so far: two bases
/// may both extend one common config (a diamond), and only a config that reaches itself again is
/// a cycle.
fn load_tsconfig_recursive(
    root: &Path,
    path: &Path,
    stack: &mut BTreeSet<PathBuf>,
    problems: &mut Vec<crate::model::Diagnostic>,
) -> Option<TsconfigLayer> {
    let identity = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !stack.insert(identity.clone()) {
        return None;
    }
    let layer = load_tsconfig_layer(root, path, stack, problems);
    stack.remove(&identity);
    layer
}

fn load_tsconfig_layer(
    root: &Path,
    path: &Path,
    stack: &mut BTreeSet<PathBuf>,
    problems: &mut Vec<crate::model::Diagnostic>,
) -> Option<TsconfigLayer> {
    let text = fs::read_to_string(path).ok()?;
    // tsc drops a UTF-8 byte-order mark, which editors on Windows like to write.
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    let value = parse_jsonc(text)?;
    let directory = path.parent().unwrap_or(root);
    let mut config = TsconfigLayer::default();
    let inherited: Vec<_> = match value.get("extends") {
        Some(serde_json::Value::String(value)) => vec![value.as_str()],
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect(),
        _ => Vec::new(),
    };
    for inherited in inherited {
        // Package-based configs require package.json resolution; unresolved packages remain
        // conservative. Relative/absolute configs cover monorepo config chains deterministically.
        if !inherited.starts_with('.') && !Path::new(inherited).is_absolute() {
            continue;
        }
        let mut inherited_path = directory.join(inherited);
        if !inherited_path.is_file() {
            inherited_path = PathBuf::from(format!("{}.json", inherited_path.to_string_lossy()));
        }
        let base = load_tsconfig_recursive(root, &inherited_path, stack, problems);
        if base.is_none() {
            // The aliases usually live in the base config of a monorepo, so a base that cannot be
            // read takes them all with it -- and the top file parses fine, so nothing else notices.
            // Sparse checkouts, uninitialised submodules and renamed base configs all land here.
            problems.push(crate::model::Diagnostic {
                code: "tsconfig_extends_unresolved".into(),
                message: format!(
                    "tsconfig.json extends `{inherited}`, which could not be read or parsed; any baseUrl and path aliases it defines are not applied, so imports relying on them resolve to nothing"
                ),
                path: Some("tsconfig.json".into()),
                span: None,
            });
        }
        if let Some(base) = base {
            if base.base_url.is_some() {
                config.base_url = base.base_url;
            }
            if base.paths.is_some() {
                config.paths = base.paths;
            }
            if !base.extensions.is_empty() {
                config.extensions = base.extensions;
            }
            config.max_candidates = base.max_candidates;
        }
    }
    let options = value.get("compilerOptions").cloned().unwrap_or_default();
    if let Some(base_url) = options.get("baseUrl").and_then(|value| value.as_str()) {
        config.base_url = Some(PathBuf::from(normalize_lexical(
            root,
            &directory.join(substitute_config_dir(root, base_url)),
        )));
    }
    if let Some(raw_paths) = options.get("paths").and_then(|value| {
        serde_json::from_value::<BTreeMap<String, Vec<String>>>(value.clone()).ok()
    }) {
        config.paths = Some((raw_paths, directory.to_path_buf()));
    }
    Some(config)
}

fn parse_jsonc(text: &str) -> Option<serde_json::Value> {
    let bytes = text.as_bytes();
    let mut without_comments = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            without_comments.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            without_comments.push(byte);
            index += 1;
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"//") {
            without_comments.extend_from_slice(b"  ");
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                without_comments.push(b' ');
                index += 1;
            }
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"/*") {
            without_comments.extend_from_slice(b"  ");
            index += 2;
            while index < bytes.len() {
                if bytes.get(index..index + 2) == Some(b"*/") {
                    without_comments.extend_from_slice(b"  ");
                    index += 2;
                    break;
                }
                without_comments.push(if bytes[index] == b'\n' { b'\n' } else { b' ' });
                index += 1;
            }
            continue;
        }
        without_comments.push(byte);
        index += 1;
    }

    let mut json = Vec::with_capacity(without_comments.len());
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while index < without_comments.len() {
        let byte = without_comments[index];
        if in_string {
            json.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            json.push(byte);
            index += 1;
            continue;
        }
        if byte == b',' {
            let mut next = index + 1;
            while without_comments
                .get(next)
                .is_some_and(u8::is_ascii_whitespace)
            {
                next += 1;
            }
            if matches!(without_comments.get(next), Some(b'}' | b']')) {
                index += 1;
                continue;
            }
        }
        json.push(byte);
        index += 1;
    }
    // tsc reads a config that is empty, or nothing but comments, as `{}`. Reporting it as
    // unparseable turns a harmless placeholder into a permanent hint telling the caller to fix a
    // file that is already fine.
    if json.iter().all(u8::is_ascii_whitespace) {
        return Some(serde_json::Value::Object(serde_json::Map::new()));
    }
    serde_json::from_slice(&json).ok()
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_config_that_parses_but_loses_its_base_is_still_reported() {
        // The aliases usually live in the base config of a monorepo. A missing base -- sparse
        // checkout, uninitialised submodule, renamed file -- takes them all with it while the top
        // file parses fine, so the earlier "did it parse?" check saw nothing wrong.
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("tsconfig.json"),
            r#"{ "extends": "./tsconfig.base.json" }"#,
        )
        .unwrap();
        let loaded = load_tsconfig_reporting(root.path());
        assert_eq!(
            loaded
                .problems
                .iter()
                .map(|problem| problem.code.as_str())
                .collect::<Vec<_>>(),
            ["tsconfig_extends_unresolved"],
            "a base that cannot be read must be named"
        );

        // With the base present there is nothing to report.
        fs::write(
            root.path().join("tsconfig.base.json"),
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@lib/*": ["src/*"] } } }"#,
        )
        .unwrap();
        let healthy = load_tsconfig_reporting(root.path());
        assert!(healthy.problems.is_empty(), "{:?}", healthy.problems);
        assert!(!healthy.config.paths.is_empty(), "the aliases are applied");
    }

    #[test]
    fn a_tsconfig_that_is_not_a_regular_file_is_reported() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("tsconfig.json")).unwrap();
        let loaded = load_tsconfig_reporting(root.path());
        assert_eq!(
            loaded
                .problems
                .iter()
                .map(|problem| problem.code.as_str())
                .collect::<Vec<_>>(),
            ["tsconfig_not_a_file"]
        );
    }

    fn write_configs(root: &Path, files: &[(&str, &str)]) {
        for (path, text) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    }

    #[test]
    fn a_tsconfig_with_a_bom_or_only_comments_is_not_unparsed() {
        // Editors on Windows save `tsconfig.json` with a UTF-8 byte-order mark, and a freshly
        // generated config can be nothing but comments. tsc reads the first normally and the
        // second as `{}`; reporting either as unparsed dropped every alias.
        let root = tempdir().unwrap();
        write_configs(
            root.path(),
            &[
                (
                    "tsconfig.json",
                    "\u{FEFF}{ \"extends\": \"./tsconfig.base.json\" }",
                ),
                (
                    "tsconfig.base.json",
                    "\u{FEFF}{ \"compilerOptions\": { \"paths\": { \"@lib/*\": [\"src/*\"] } } }",
                ),
            ],
        );
        let loaded = load_tsconfig_reporting(root.path());
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(
            loaded.config.paths,
            BTreeMap::from([("@lib/*".to_owned(), vec!["src/*".to_owned()])])
        );

        write_configs(
            root.path(),
            &[("tsconfig.json", "// configured later\n/* { } */\n")],
        );
        let loaded = load_tsconfig_reporting(root.path());
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.config, ResolverConfig::default());
    }

    #[test]
    fn two_bases_extending_one_common_config_is_not_a_cycle() {
        // A diamond: both bases extend `common.json`. Only a config extending itself is a cycle;
        // a global seen-set reported the second visit as unreadable and dropped what it held.
        let root = tempdir().unwrap();
        write_configs(
            root.path(),
            &[
                (
                    "tsconfig.json",
                    r#"{ "extends": ["./configs/a.json", "./configs/b.json"] }"#,
                ),
                ("configs/a.json", r#"{ "extends": "./common.json" }"#),
                (
                    "configs/b.json",
                    r#"{ "extends": "./common.json", "compilerOptions": { "baseUrl": "../src" } }"#,
                ),
                (
                    "configs/common.json",
                    r#"{ "compilerOptions": { "paths": { "@lib/*": ["lib/*"] } } }"#,
                ),
            ],
        );
        let loaded = load_tsconfig_reporting(root.path());
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.config.base_url, Some(PathBuf::from("src")));

        // A real cycle still ends.
        write_configs(
            root.path(),
            &[("configs/common.json", r#"{ "extends": "./b.json" }"#)],
        );
        let cyclic = load_tsconfig_reporting(root.path());
        assert!(!cyclic.problems.is_empty());
    }

    #[test]
    fn inherited_paths_resolve_against_the_final_base_url_or_their_own_config() {
        // tsc keeps `paths` as written and resolves them against the merged `baseUrl`, or, with
        // none, against the directory of the config that defined them. `${configDir}` is the
        // directory of the config being compiled.
        let root = tempdir().unwrap();
        let base = r#"{ "compilerOptions": { "paths": {
            "@lib/*": ["lib/*"],
            "@app/*": ["${configDir}/app/*"]
        } } }"#;
        write_configs(
            root.path(),
            &[
                (
                    "tsconfig.json",
                    r#"{ "extends": "./configs/base.json", "compilerOptions": { "baseUrl": "./src" } }"#,
                ),
                ("configs/base.json", base),
            ],
        );
        let loaded = load_tsconfig_reporting(root.path());
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(
            loaded.config.paths,
            BTreeMap::from([
                ("@app/*".to_owned(), vec!["app/*".to_owned()]),
                ("@lib/*".to_owned(), vec!["src/lib/*".to_owned()]),
            ])
        );

        // No baseUrl anywhere: relative to the config that holds `paths`.
        write_configs(
            root.path(),
            &[("tsconfig.json", r#"{ "extends": "./configs/base.json" }"#)],
        );
        assert_eq!(
            load_tsconfig(root.path()).paths,
            BTreeMap::from([
                ("@app/*".to_owned(), vec!["app/*".to_owned()]),
                ("@lib/*".to_owned(), vec!["configs/lib/*".to_owned()]),
            ])
        );

        // A baseUrl set in the base and paths set on top: the base's baseUrl still applies.
        write_configs(
            root.path(),
            &[
                (
                    "tsconfig.json",
                    r#"{ "extends": "./configs/with-base-url.json",
                         "compilerOptions": { "paths": { "@x/*": ["x/*"] } } }"#,
                ),
                (
                    "configs/with-base-url.json",
                    r#"{ "compilerOptions": { "baseUrl": "${configDir}/packages" } }"#,
                ),
            ],
        );
        let loaded = load_tsconfig(root.path());
        assert_eq!(loaded.base_url, Some(PathBuf::from("packages")));
        assert_eq!(
            loaded.paths,
            BTreeMap::from([("@x/*".to_owned(), vec!["packages/x/*".to_owned()])])
        );
    }
    use super::*;
    use crate::scanner::parse_source;
    use tempfile::tempdir;

    fn write_artifact(root: &Path, path: &str, source: &str) -> FileArtifact {
        let absolute = root.join(path);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&absolute, source).unwrap();
        parse_source(path, source.as_bytes())
    }

    fn symbol_id(artifact: &FileArtifact, qualified_name: &str) -> String {
        artifact
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == qualified_name)
            .unwrap_or_else(|| panic!("missing {qualified_name} in {:?}", artifact.symbols))
            .id
            .clone()
    }

    #[test]
    fn jsonc_trailing_commas_do_not_mutate_string_contents() {
        let parsed = parse_jsonc(
            r#"{
              // commas before braces inside strings are data
              "compilerOptions": {
                "baseUrl": "src,}",
                "paths": { "@x/*": ["lib,]/*",], },
              },
            }"#,
        )
        .unwrap();
        assert_eq!(parsed["compilerOptions"]["baseUrl"], "src,}");
        assert_eq!(parsed["compilerOptions"]["paths"]["@x/*"][0], "lib,]/*");
    }
    #[test]
    fn resolves_call_through_constructor_injected_field_type() {
        let root = tempdir().unwrap();
        let service = "export class FooService { run(): void {} }";
        let controller = "import { FooService } from './foo.service';\n\
            export class FooController {\n\
              constructor(private readonly svc: FooService) {}\n\
              handle() { this.svc.run(); }\n\
            }";
        let a = write_artifact(root.path(), "src/controller.ts", controller);
        let b = write_artifact(root.path(), "src/foo.service.ts", service);
        let map: BTreeMap<String, FileArtifact> = [(a.path.clone(), a), (b.path.clone(), b)].into();
        let edges = resolve_edges(root.path(), &map, &ResolverConfig::default());
        assert!(
            edges.iter().any(|edge| {
                edge.kind == EdgeKind::Calls
                    && edge.to.contains("foo.service.ts")
                    && edge.to.ends_with("FooService.run")
            }),
            "expected Calls edge to FooService.run via declared field type, got: {:#?}",
            edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Calls)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn resolves_call_through_class_property_with_aliased_import_type() {
        // Mirrors the NestJS controller shape: class-body property typed by an
        // aliased import, assigned in the constructor, invoked via `this.<field>`.
        let root = tempdir().unwrap();
        let usecase = "export class GetAllUseCase { execute(): number { return 1; } }";
        let controller = "import { GetAllUseCase as UseCase } from './usecase';\n\
            export class Controller {\n\
              private readonly usecase: UseCase;\n\
              constructor() { this.usecase = new UseCase(); }\n\
              handle() { return this.usecase.execute(); }\n\
            }";
        let a = write_artifact(root.path(), "src/controller.ts", controller);
        let b = write_artifact(root.path(), "src/usecase.ts", usecase);
        let map: BTreeMap<String, FileArtifact> = [(a.path.clone(), a), (b.path.clone(), b)].into();
        let edges = resolve_edges(root.path(), &map, &ResolverConfig::default());
        assert!(
            edges.iter().any(|edge| {
                edge.kind == EdgeKind::Calls
                    && edge.to.contains("usecase.ts")
                    && edge.to.ends_with("GetAllUseCase.execute")
            }),
            "expected Calls edge to GetAllUseCase.execute via aliased declared type, got: {:#?}",
            edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Calls)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn resolves_call_through_interface_typed_field_to_interface_member() {
        let root = tempdir().unwrap();
        let contract = "export interface IUseCase { execute(): Promise<void>; }";
        let controller = "import { IUseCase } from './contract';\n\
            export class Controller {\n\
              constructor(private readonly usecase: IUseCase) {}\n\
              handle() { return this.usecase.execute(); }\n\
            }";
        let a = write_artifact(root.path(), "src/controller.ts", controller);
        let b = write_artifact(root.path(), "src/contract.ts", contract);
        let map: BTreeMap<String, FileArtifact> = [(a.path.clone(), a), (b.path.clone(), b)].into();
        let edges = resolve_edges(root.path(), &map, &ResolverConfig::default());
        assert!(
            edges.iter().any(|edge| {
                edge.kind == EdgeKind::Calls
                    && edge.to.contains("contract.ts")
                    && edge.to.ends_with("IUseCase.execute")
            }),
            "expected Calls edge to IUseCase.execute via declared field type, got: {:#?}",
            edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Calls)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn resolves_relative_import_and_keeps_unresolved_visible() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("src")).unwrap();
        fs::write(
            root.path().join("src/a.ts"),
            "import { B } from './b'; import X from 'missing';",
        )
        .unwrap();
        fs::write(root.path().join("src/b.ts"), "export class B {}").unwrap();
        let a = parse_source(
            "src/a.ts",
            b"import { B } from './b'; import X from 'missing';",
        );
        let b = parse_source("src/b.ts", b"export class B {}");
        let map: BTreeMap<String, FileArtifact> = [(a.path.clone(), a), (b.path.clone(), b)].into();
        let (edges, _, reverse) = resolve_artifacts(root.path(), &map, &ResolverConfig::default());
        assert_eq!(
            edges,
            resolve_edges(root.path(), &map, &ResolverConfig::default())
        );
        assert!(edges.iter().any(|edge| edge.to.ends_with("src/b.ts")));
        assert!(
            edges
                .iter()
                .any(|edge| matches!(edge.confidence, EdgeConfidence::Unresolved { .. }))
        );
        assert!(!reverse.affected_by("src/b.ts").is_empty());
    }

    #[test]
    fn resolves_typescript_module_extension() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/a.ts"), "import { B } from './b';").unwrap();
        fs::write(root.path().join("src/b.mts"), "export class B {}").unwrap();
        let a = parse_source("src/a.ts", b"import { B } from './b';");
        let b = parse_source("src/b.mts", b"export class B {}");
        let map: BTreeMap<String, FileArtifact> = [(a.path.clone(), a), (b.path.clone(), b)].into();
        let edges = resolve_edges(root.path(), &map, &ResolverConfig::default());
        assert!(edges.iter().any(|edge| edge.to.ends_with("src/b.mts")));
    }

    #[test]
    fn resolves_extensionless_specifiers_with_dotted_basenames() {
        let root = tempdir().unwrap();
        let consumer = write_artifact(
            root.path(),
            "src/a.ts",
            "import { helper } from './helper.util'; helper();",
        );
        let dependency = write_artifact(
            root.path(),
            "src/helper.util.ts",
            "export function helper() {}",
        );
        let map = BTreeMap::from([
            (consumer.path.clone(), consumer),
            (dependency.path.clone(), dependency.clone()),
        ]);
        let helper = symbol_id(&dependency, "helper");
        let edges = resolve_edges(root.path(), &map, &ResolverConfig::default());
        assert!(
            edges
                .iter()
                .any(|edge| edge.to == helper && edge.kind == EdgeKind::Calls)
        );
    }

    #[test]
    fn resolves_scanner_style_relative_paths() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/a.ts"), "import { B } from './b';").unwrap();
        fs::write(root.path().join("src/b.ts"), "export class B {}").unwrap();
        let a = parse_source("src/a.ts", b"import { B } from './b';");
        let b = parse_source("src/b.ts", b"export class B {}");
        let map: BTreeMap<String, FileArtifact> = [(a.path.clone(), a), (b.path.clone(), b)].into();
        let (edges, _, reverse) = resolve_artifacts(root.path(), &map, &ResolverConfig::default());
        assert!(edges.iter().any(|edge| {
            edge.from == "src/a.ts"
                && edge.to == "src/b.ts"
                && matches!(edge.confidence, EdgeConfidence::Resolved { .. })
        }));
        assert_eq!(reverse.affected_by("src/b.ts"), vec!["src/a.ts"]);
    }

    #[test]
    fn resolves_alias_default_namespace_type_and_barrel_bindings_to_stable_ids() {
        let root = tempdir().unwrap();
        let dependency = write_artifact(
            root.path(),
            "src/dependency.ts",
            r#"
export class Service { execute() {} }
export function helper() {}
export default Service;
"#,
        );
        let barrel = write_artifact(
            root.path(),
            "src/barrel.ts",
            "export { Service as Renamed } from './dependency';\nexport * from './dependency';\n",
        );
        let consumer = write_artifact(
            root.path(),
            "src/consumer.ts",
            r#"
import DefaultService, { helper as callHelper } from './dependency';
import type { Service as ServiceType } from './dependency';
import { Renamed } from './barrel';
import * as NS from './dependency';
export class Consumer {
  constructor(private service: ServiceType) {}
  direct() { callHelper(); return new DefaultService(); }
  barrel() { return new Renamed(); }
  namespace() { return NS.helper(); }
}
"#,
        );
        let map: BTreeMap<String, FileArtifact> = [
            (dependency.path.clone(), dependency.clone()),
            (barrel.path.clone(), barrel.clone()),
            (consumer.path.clone(), consumer.clone()),
        ]
        .into();
        let edges = resolve_edges(root.path(), &map, &ResolverConfig::default());
        let service = symbol_id(&dependency, "Service");
        let helper = symbol_id(&dependency, "helper");
        let service_property = symbol_id(&consumer, "Consumer.service");
        let direct = symbol_id(&consumer, "Consumer.direct");
        let barrel_method = symbol_id(&consumer, "Consumer.barrel");
        let namespace = symbol_id(&consumer, "Consumer.namespace");

        let has = |from: &str, to: &str, kind: EdgeKind| {
            edges
                .iter()
                .any(|edge| edge.from == from && edge.to == to && edge.kind == kind)
        };
        assert!(
            has(&service_property, &service, EdgeKind::TypeOf),
            "{edges:#?}"
        );
        assert!(has(&direct, &helper, EdgeKind::Calls), "{edges:#?}");
        assert!(has(&direct, &service, EdgeKind::Instantiates), "{edges:#?}");
        assert!(
            has(&barrel_method, &service, EdgeKind::Instantiates),
            "{edges:#?}"
        );
        assert!(has(&namespace, &helper, EdgeKind::Calls), "{edges:#?}");
        assert!(has("src/barrel.ts", &service, EdgeKind::ReExport));
        assert!(edges.iter().any(|edge| {
            edge.from == "src/consumer.ts"
                && edge.to == service
                && edge.kind == EdgeKind::Import
                && edge.type_only
        }));
        assert!(edges.iter().filter(|edge| edge.to == service).all(|edge| {
            edge.from == "src/consumer.ts"
                || edge.from == "src/barrel.ts"
                || edge.from.starts_with("symbol://")
        }));
    }

    #[test]
    fn ambiguous_names_and_untyped_member_calls_do_not_create_false_edges() {
        let root = tempdir().unwrap();
        let first = write_artifact(
            root.path(),
            "src/first.ts",
            "export function target() {}\nexport class First { execute() {} }\n",
        );
        let second = write_artifact(
            root.path(),
            "src/second.ts",
            "export function target() {}\nexport class Second { execute() {} }\n",
        );
        let ambiguous = write_artifact(
            root.path(),
            "src/ambiguous.ts",
            "export function run(obj: unknown) { target(); obj.execute(); }\n",
        );
        let explicit = write_artifact(
            root.path(),
            "src/explicit.ts",
            "import { target } from './first'; export function run() { target(); }\n",
        );
        let unique = write_artifact(
            root.path(),
            "src/unique.ts",
            "export function uniqueHelper() {}\n",
        );
        let shadowed = write_artifact(
            root.path(),
            "src/shadowed.ts",
            "export function run(uniqueHelper: () => void) { uniqueHelper(); }\n",
        );
        let locally_shadowed = write_artifact(
            root.path(),
            "src/locally_shadowed.ts",
            "import { target } from './first'; export function run() { const target = () => 1; target(); }\n",
        );
        let map: BTreeMap<String, FileArtifact> = [
            (first.path.clone(), first.clone()),
            (second.path.clone(), second.clone()),
            (ambiguous.path.clone(), ambiguous.clone()),
            (explicit.path.clone(), explicit.clone()),
            (unique.path.clone(), unique.clone()),
            (shadowed.path.clone(), shadowed.clone()),
            (locally_shadowed.path.clone(), locally_shadowed.clone()),
        ]
        .into();
        let edges = resolve_edges(root.path(), &map, &ResolverConfig::default());
        let first_target = symbol_id(&first, "target");
        let second_target = symbol_id(&second, "target");
        let ambiguous_run = symbol_id(&ambiguous, "run");
        let explicit_run = symbol_id(&explicit, "run");
        let unique_helper = symbol_id(&unique, "uniqueHelper");
        let shadowed_run = symbol_id(&shadowed, "run");
        let locally_shadowed_run = symbol_id(&locally_shadowed, "run");
        let local_target = symbol_id(&locally_shadowed, "run.target");
        let execute_ids: BTreeSet<_> = first
            .symbols
            .iter()
            .chain(second.symbols.iter())
            .filter(|symbol| symbol.name == "execute")
            .map(|symbol| symbol.id.as_str())
            .collect();

        assert!(
            !edges.iter().any(|edge| {
                edge.from == ambiguous_run
                    && (edge.to == first_target
                        || edge.to == second_target
                        || execute_ids.contains(edge.to.as_str()))
            }),
            "{edges:#?}"
        );
        assert!(edges.iter().any(|edge| {
            edge.from == explicit_run && edge.to == first_target && edge.kind == EdgeKind::Calls
        }));
        assert!(!edges.iter().any(|edge| {
            edge.from == explicit_run && edge.to == second_target && edge.kind == EdgeKind::Calls
        }));
        assert!(!edges.iter().any(|edge| {
            edge.from == shadowed_run && edge.to == unique_helper && edge.kind == EdgeKind::Calls
        }));
        assert!(edges.iter().any(|edge| {
            edge.from == locally_shadowed_run
                && edge.to == local_target
                && edge.kind == EdgeKind::Calls
        }));
        assert!(!edges.iter().any(|edge| {
            edge.from == locally_shadowed_run
                && edge.to == first_target
                && edge.kind == EdgeKind::Calls
        }));
    }

    #[test]
    fn does_not_resolve_unique_names_without_import_or_outside_lexical_owner() {
        let root = tempdir().unwrap();
        let dependency = write_artifact(
            root.path(),
            "dependency.ts",
            "export function uniqueTarget() {}",
        );
        let consumer = write_artifact(
            root.path(),
            "consumer.ts",
            "export function run() { uniqueTarget(); hidden(); } function outer() { function hidden() {} }",
        );
        let map = BTreeMap::from([
            (dependency.path.clone(), dependency.clone()),
            (consumer.path.clone(), consumer.clone()),
        ]);
        let edges = resolve_edges(root.path(), &map, &ResolverConfig::default());
        let run = symbol_id(&consumer, "run");
        assert!(
            !edges
                .iter()
                .any(|edge| edge.from == run && edge.kind == EdgeKind::Calls)
        );
    }

    #[test]
    fn overloads_namespace_exports_sites_and_type_only_edges_resolve_logically() {
        let root = tempdir().unwrap();
        let dependency = write_artifact(
            root.path(),
            "dependency.ts",
            r#"
export function parse(value: string): string;
export function parse(value: string) { return value; }
export interface Shape {}
export class Base {}
export class Service { get value(): string { return ''; } set value(next: string) {} }
"#,
        );
        let barrel = write_artifact(
            root.path(),
            "barrel.ts",
            "export * as API from './dependency';",
        );
        let consumer = write_artifact(
            root.path(),
            "consumer.ts",
            r#"
import { parse, Shape, Base, Service } from './dependency';
import { API } from './barrel';
interface Derived extends Shape {}
class Child extends Base implements Shape {
  service!: Service;
  run() { parse('a'); parse('b'); return API.parse('c'); }
}
"#,
        );
        let map = BTreeMap::from([
            (dependency.path.clone(), dependency.clone()),
            (barrel.path.clone(), barrel),
            (consumer.path.clone(), consumer.clone()),
        ]);
        let edges = resolve_edges(root.path(), &map, &ResolverConfig::default());
        let parse = symbol_id(&dependency, "parse");
        let shape = symbol_id(&dependency, "Shape");
        let base = symbol_id(&dependency, "Base");
        let run = symbol_id(&consumer, "Child.run");
        assert_eq!(
            edges
                .iter()
                .filter(|edge| edge.from == run && edge.to == parse && edge.kind == EdgeKind::Calls)
                .count(),
            3,
            "{edges:#?}"
        );
        assert!(edges.iter().any(|edge| {
            edge.to == shape
                && matches!(
                    edge.kind,
                    EdgeKind::TypeOf | EdgeKind::Implements | EdgeKind::Extends
                )
                && edge.type_only
        }));
        assert!(
            edges.iter().any(|edge| {
                edge.to == base && edge.kind == EdgeKind::Extends && !edge.type_only
            })
        );
    }

    #[test]
    fn exact_path_alias_and_extension_priority_are_deterministic() {
        let root = tempdir().unwrap();
        let consumer = write_artifact(
            root.path(),
            "src/consumer.ts",
            "import { target } from '@core'; target();",
        );
        let ts = write_artifact(root.path(), "src/core.ts", "export function target() {} ");
        let js = write_artifact(root.path(), "src/core.js", "export function target() {} ");
        let map = BTreeMap::from([
            (consumer.path.clone(), consumer),
            (ts.path.clone(), ts.clone()),
            (js.path.clone(), js.clone()),
        ]);
        let config = ResolverConfig {
            paths: BTreeMap::from([("@core".into(), vec!["src/core".into()])]),
            max_candidates: 32,
            ..ResolverConfig::default()
        };
        let edges = resolve_edges(root.path(), &map, &config);
        let ts_target = symbol_id(&ts, "target");
        let js_target = symbol_id(&js, "target");
        assert!(edges.iter().any(|edge| edge.to == ts_target));
        assert!(
            !edges
                .iter()
                .any(|edge| edge.to == js_target && edge.kind == EdgeKind::Calls)
        );
    }

    #[test]
    fn wildcard_tsconfig_alias_resolves_dotted_basename() {
        let root = tempdir().unwrap();
        fs::write(
            root.path().join("tsconfig.base.json"),
            r#"{
              // JSONC is TypeScript's native config format.
              "compilerOptions": {
                "paths": { "@scope/common/*": ["./libs/common/src/*"], },
              },
            }"#,
        )
        .unwrap();
        fs::write(
            root.path().join("tsconfig.json"),
            r#"{ "extends": ["./tsconfig.base"], "compilerOptions": {}, }"#,
        )
        .unwrap();
        let consumer = write_artifact(
            root.path(),
            "src/consumer.ts",
            "import { helper } from '@scope/common/utils/helper.util'; helper();",
        );
        let dependency = write_artifact(
            root.path(),
            "libs/common/src/utils/helper.util.ts",
            "export function helper() {}",
        );
        let map = BTreeMap::from([
            (consumer.path.clone(), consumer),
            (dependency.path.clone(), dependency.clone()),
        ]);
        let config = load_tsconfig(root.path());
        let helper = symbol_id(&dependency, "helper");
        let edges = resolve_edges(root.path(), &map, &config);
        assert!(
            edges
                .iter()
                .any(|edge| edge.to == helper && edge.kind == EdgeKind::Calls),
            "{edges:#?}"
        );
    }

    #[test]
    fn overlapping_path_aliases_use_the_most_specific_pattern() {
        let root = tempdir().unwrap();
        let consumer = write_artifact(
            root.path(),
            "src/consumer.ts",
            "import { target } from '@app/special/target'; target();",
        );
        let broad = write_artifact(
            root.path(),
            "src/general/special/target.ts",
            "export function target() {}",
        );
        let specific = write_artifact(
            root.path(),
            "src/special/target.ts",
            "export function target() {}",
        );
        let artifacts = BTreeMap::from([
            (consumer.path.clone(), consumer),
            (broad.path.clone(), broad.clone()),
            (specific.path.clone(), specific.clone()),
        ]);
        let config = ResolverConfig {
            paths: BTreeMap::from([
                ("@app/*".into(), vec!["src/general/*".into()]),
                ("@app/special/*".into(), vec!["src/special/*".into()]),
            ]),
            ..ResolverConfig::default()
        };
        let edges = resolve_edges(root.path(), &artifacts, &config);
        let specific_id = symbol_id(&specific, "target");
        let broad_id = symbol_id(&broad, "target");
        assert!(edges.iter().any(|edge| edge.to == specific_id));
        assert!(!edges.iter().any(|edge| edge.to == broad_id));
    }

    #[test]
    fn block_scoped_bindings_only_shadow_references_inside_their_lexical_range() {
        let root = tempdir().unwrap();
        let dependency = write_artifact(root.path(), "src/dep.ts", "export function helper() {}");
        let consumer = write_artifact(
            root.path(),
            "src/consumer.ts",
            "import { helper } from './dep';\n\
             export function run(flag: boolean) {\n\
               if (flag) { const helper = () => 1; helper(); }\n\
               helper();\n\
               if (!flag) { const helper = () => 2; helper(); }\n\
             }",
        );
        let scoped: Vec<_> = consumer
            .symbols
            .iter()
            .filter(|symbol| symbol.qualified_name == "run.helper")
            .map(|symbol| (symbol.id.clone(), symbol.scope))
            .collect();
        assert_eq!(scoped.len(), 2);
        assert_ne!(scoped[0].0, scoped[1].0);
        assert!(scoped.iter().all(|(_, scope)| scope.is_some()));

        let artifacts = BTreeMap::from([
            (dependency.path.clone(), dependency.clone()),
            (consumer.path.clone(), consumer),
        ]);
        let edges = resolve_edges(root.path(), &artifacts, &ResolverConfig::default());
        let imported = symbol_id(&dependency, "helper");
        let calls_to_import: Vec<_> = edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls && edge.to == imported)
            .collect();
        assert_eq!(calls_to_import.len(), 1);
        assert_eq!(calls_to_import[0].span.unwrap().start_line, 3);
        for (local_id, _) in scoped {
            assert!(
                edges
                    .iter()
                    .any(|edge| { edge.kind == EdgeKind::Calls && edge.to == local_id })
            );
        }
    }

    #[test]
    fn type_and_value_declarations_with_the_same_name_keep_distinct_identities() {
        let root = tempdir().unwrap();
        let dependency = write_artifact(
            root.path(),
            "src/dep.ts",
            "export type User = { id: string }; export const User = () => 1;",
        );
        let type_consumer = write_artifact(
            root.path(),
            "src/type.ts",
            "import type { User } from './dep'; export const current: User = { id: '1' };",
        );
        let value_consumer = write_artifact(
            root.path(),
            "src/value.ts",
            "import { User } from './dep'; export const current = User();",
        );
        let definitions: Vec<_> = dependency
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "User")
            .collect();
        assert_eq!(definitions.len(), 2);
        assert_ne!(definitions[0].id, definitions[1].id);
        let type_id = definitions
            .iter()
            .find(|symbol| symbol.kind.as_ref() == "type_alias_declaration")
            .unwrap()
            .id
            .clone();
        let value_id = definitions
            .iter()
            .find(|symbol| symbol.kind.as_ref() == "function")
            .unwrap()
            .id
            .clone();
        let artifacts = BTreeMap::from([
            (dependency.path.clone(), dependency),
            (type_consumer.path.clone(), type_consumer),
            (value_consumer.path.clone(), value_consumer),
        ]);
        let edges = resolve_edges(root.path(), &artifacts, &ResolverConfig::default());
        assert!(
            edges
                .iter()
                .any(|edge| { edge.kind == EdgeKind::TypeOf && edge.to == type_id })
        );
        assert!(
            edges
                .iter()
                .any(|edge| { edge.kind == EdgeKind::Calls && edge.to == value_id })
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_relative_paths_when_root_is_a_symlink() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let canonical_root = parent.path().join("workspace");
        fs::create_dir_all(canonical_root.join("src")).unwrap();
        fs::write(canonical_root.join("src/a.ts"), "import { B } from './b';").unwrap();
        fs::write(canonical_root.join("src/b.ts"), "export class B {}").unwrap();
        let linked_root = parent.path().join("workspace-link");
        symlink(&canonical_root, &linked_root).unwrap();

        let a = parse_source("src/a.ts", b"import { B } from './b';");
        let b = parse_source("src/b.ts", b"export class B {}");
        let map: BTreeMap<String, FileArtifact> = [(a.path.clone(), a), (b.path.clone(), b)].into();

        let edges = resolve_edges(&linked_root, &map, &ResolverConfig::default());
        assert!(edges.iter().any(|edge| {
            edge.from == "src/a.ts"
                && edge.to == "src/b.ts"
                && matches!(edge.confidence, EdgeConfidence::Resolved { .. })
        }));
    }

    /// Calls edges leaving the symbol with `from_id`, as `(target id, line)` pairs.
    fn calls_from(edges: &[Edge], from_id: &str) -> Vec<String> {
        edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Calls && edge.from == from_id)
            .map(|edge| edge.to.clone())
            .collect()
    }

    #[test]
    fn a_class_member_does_not_shadow_an_import_of_the_same_name() {
        // Class and interface members are reached through `this`, an instance, or the class
        // itself -- never as a bare name. Treating `Logger.format` as lexically visible inside
        // `Logger` sent every `format(x)` there to the method instead of the imported function,
        // and a wrapper method calling the import of its own name resolved to itself and vanished.
        let root = tempdir().unwrap();
        let formatter = write_artifact(
            root.path(),
            "src/fmt.ts",
            "export function format(x: string) { return x; }",
        );
        let logger = write_artifact(
            root.path(),
            "src/logger.ts",
            "import { format } from './fmt';\n\
             export class Logger {\n\
               format(x: string) { return format(x); }\n\
               log(x: string) { return format(x); }\n\
             }\n\
             export interface Shape { format(x: string): string; }\n\
             export class Shaped { run(x: string) { return format(x); } }\n",
        );
        let artifacts = BTreeMap::from([
            (formatter.path.clone(), formatter.clone()),
            (logger.path.clone(), logger.clone()),
        ]);
        let edges = resolve_edges(root.path(), &artifacts, &ResolverConfig::default());
        let imported = symbol_id(&formatter, "format");
        for caller in ["Logger.format", "Logger.log", "Shaped.run"] {
            assert_eq!(
                calls_from(&edges, &symbol_id(&logger, caller)),
                std::slice::from_ref(&imported),
                "{caller} calls the imported function"
            );
        }
    }

    #[test]
    fn an_enum_member_stays_visible_inside_its_own_initializers() {
        // `B = A << 1` names `Flags.A` bare; that is the one kind of member that is in scope.
        let flags = parse_source("src/flags.ts", b"export enum Flags { A = 1, B = A << 1 }");
        let logger = parse_source("src/logger.ts", b"class Logger { format() {} log() {} }");
        let symbol = |artifact: &FileArtifact, qualified: &str| {
            artifact
                .symbols
                .iter()
                .find(|symbol| symbol.qualified_name == qualified)
                .cloned()
                .unwrap_or_else(|| panic!("missing {qualified} in {:?}", artifact.symbols))
        };
        let visible = |artifact: &FileArtifact, definition: &str, from: &str| {
            let source = symbol(artifact, from);
            visible_local_definition(
                &SymbolDefinition::from_symbol(artifact, &symbol(artifact, definition)),
                Some(&source),
                source.span,
            )
        };
        assert!(visible(&flags, "Flags.A", "Flags.B"));
        assert!(visible(&flags, "Flags.A", "Flags"));
        assert!(!visible(&logger, "Logger.format", "Logger.log"));
        assert!(!visible(&logger, "Logger.format", "Logger"));
    }

    #[test]
    fn a_declaration_or_default_export_never_reads_from_another_module() {
        // Only `export {a} from`, `export * from` and `export * as ns from` read another module. A
        // declaration or default export has no source, whatever string literal the scanner may
        // have picked up from its body -- following one sent `helper` to an unrelated module.
        let root = tempdir().unwrap();
        let unrelated = write_artifact(root.path(), "src/a.ts", "export function helper() {}");
        let mut wrapper = write_artifact(
            root.path(),
            "src/b.ts",
            "export function helper() { return load('./a'); }\n\
             export default function main() {}\n",
        );
        // The guard must hold on its own, whatever the scanner records.
        for export in &mut wrapper.exports {
            export.specifier = Some("./a".into());
        }
        let consumer = write_artifact(
            root.path(),
            "src/c.ts",
            "import main, { helper } from './b';\n\
             export function run() { helper(); main(); }\n",
        );
        let artifacts = BTreeMap::from([
            (unrelated.path.clone(), unrelated.clone()),
            (wrapper.path.clone(), wrapper.clone()),
            (consumer.path.clone(), consumer.clone()),
        ]);
        let edges = resolve_edges(root.path(), &artifacts, &ResolverConfig::default());
        let mut calls = calls_from(&edges, &symbol_id(&consumer, "run"));
        calls.sort();
        assert_eq!(
            calls,
            [symbol_id(&wrapper, "helper"), symbol_id(&wrapper, "main")]
        );
        assert!(
            !edges
                .iter()
                .any(|edge| edge.from == "src/b.ts" && edge.kind == EdgeKind::ReExport),
            "{edges:#?}"
        );
    }

    #[test]
    fn explicit_exports_shadow_star_exports_and_default_never_passes_through_a_star() {
        // ES ResolveExport: a module's own and indirect exports are consulted before `export *`,
        // and `export *` never forwards `default`.
        let root = tempdir().unwrap();
        let a = write_artifact(root.path(), "src/a.ts", "export function foo() {}");
        let b = write_artifact(
            root.path(),
            "src/b.ts",
            "export function foo() {}\nexport default function fallback() {}\n",
        );
        let reexporting = write_artifact(
            root.path(),
            "src/reexporting.ts",
            "export { foo } from './a';\nexport * from './b';\n",
        );
        let declaring = write_artifact(
            root.path(),
            "src/declaring.ts",
            "export function foo() {}\nexport * from './b';\n",
        );
        let consumer = write_artifact(
            root.path(),
            "src/consumer.ts",
            "import fallback, { foo } from './reexporting';\n\
             import { foo as local } from './declaring';\n\
             export function viaReexport() { foo(); fallback(); }\n\
             export function viaDeclaration() { local(); }\n",
        );
        let artifacts = BTreeMap::from([
            (a.path.clone(), a.clone()),
            (b.path.clone(), b.clone()),
            (reexporting.path.clone(), reexporting.clone()),
            (declaring.path.clone(), declaring.clone()),
            (consumer.path.clone(), consumer.clone()),
        ]);
        let edges = resolve_edges(root.path(), &artifacts, &ResolverConfig::default());
        assert_eq!(
            calls_from(&edges, &symbol_id(&consumer, "viaReexport")),
            [symbol_id(&a, "foo")],
            "the explicit re-export wins and `default` is not taken from the star"
        );
        assert_eq!(
            calls_from(&edges, &symbol_id(&consumer, "viaDeclaration")),
            [symbol_id(&declaring, "foo")],
            "the local declaration wins over the star"
        );
    }

    /// Where each of `consumer`'s imports resolved, in source order (`None` when unresolved).
    fn import_targets(root: &Path, files: &[(&str, &str)], consumer: &str) -> Vec<Option<String>> {
        let artifacts: BTreeMap<String, FileArtifact> = files
            .iter()
            .map(|(path, source)| {
                let artifact = write_artifact(root, path, source);
                (artifact.path.clone(), artifact)
            })
            .collect();
        let universe = ResolutionUniverse::build(&artifacts, &ResolverConfig::default());
        artifacts[consumer]
            .imports
            .iter()
            .map(|import| {
                resolve_one(
                    root,
                    consumer,
                    &import.specifier,
                    &universe,
                    &ResolverConfig::default(),
                )
                .target
            })
            .collect()
    }

    #[test]
    fn declaration_files_are_probed_where_typescript_probes_them() {
        // `.d.ts` files are indexed, so a type-only import of one has a target to resolve to.
        let root = tempdir().unwrap();
        let targets = import_targets(
            root.path(),
            &[
                (
                    "src/consumer.ts",
                    "import type { W } from './types';\n\
                     import type { W as V } from './types.js';\n\
                     import type { M } from './m.mjs';\n\
                     import type { C } from './c.cjs';\n\
                     import type { L } from './lib';\n",
                ),
                ("src/types.d.ts", "export interface W {}"),
                ("src/m.d.mts", "export interface M {}"),
                ("src/c.d.cts", "export interface C {}"),
                ("src/lib/index.d.ts", "export interface L {}"),
            ],
            "src/consumer.ts",
        );
        assert_eq!(
            targets,
            [
                Some("src/types.d.ts".to_owned()),
                Some("src/types.d.ts".to_owned()),
                Some("src/m.d.mts".to_owned()),
                Some("src/c.d.cts".to_owned()),
                Some("src/lib/index.d.ts".to_owned()),
            ]
        );
    }

    #[test]
    fn a_js_extension_specifier_prefers_the_typescript_source_it_names() {
        // Under TypeScript a `.js` specifier names the `.ts` file compiled to it, even when an
        // emitted `.js` sits next to it; `.mjs` and `.cjs` name `.mts` and `.cts` first.
        let root = tempdir().unwrap();
        let targets = import_targets(
            root.path(),
            &[
                (
                    "src/consumer.ts",
                    "import { u } from './util.js';\n\
                     import { x } from './x.mjs';\n\
                     import { y } from './y.cjs';\n\
                     import { v } from './view.jsx';\n\
                     import { d } from './only-ts.js';\n\
                     import { b } from './b';\n",
                ),
                ("src/util.ts", "export const u = 1;"),
                ("src/util.js", "export const u = 1;"),
                ("src/x.ts", "export const x = 1;"),
                ("src/x.mts", "export const x = 1;"),
                ("src/x.mjs", "export const x = 1;"),
                ("src/y.ts", "export const y = 1;"),
                ("src/y.cts", "export const y = 1;"),
                ("src/y.cjs", "export const y = 1;"),
                ("src/view.tsx", "export const v = 1;"),
                ("src/view.jsx", "export const v = 1;"),
                ("src/only-ts.mts", "export const d = 1;"),
                ("src/b.mts", "export const b = 1;"),
            ],
            "src/consumer.ts",
        );
        assert_eq!(
            targets,
            [
                Some("src/util.ts".to_owned()),
                Some("src/x.mts".to_owned()),
                Some("src/y.cts".to_owned()),
                Some("src/view.tsx".to_owned()),
                // Outside TypeScript's own substitutions the other extensions remain a fallback.
                Some("src/only-ts.mts".to_owned()),
                Some("src/b.mts".to_owned()),
            ]
        );
    }

    #[test]
    fn a_matched_paths_pattern_ends_the_lookup_before_base_url() {
        // tsc tries `baseUrl` only when no `paths` pattern matched. A matched pattern whose
        // targets do not exist is an unresolved import (TS2307), not a cue to look elsewhere.
        let root = tempdir().unwrap();
        let mut files = BTreeMap::new();
        for (path, source) in [
            ("src/consumer.ts", "import '@app/thing';\nimport 'plain';\n"),
            ("@app/thing.ts", "export const stray = 1;"),
            ("plain.ts", "export const plain = 1;"),
        ] {
            let artifact = write_artifact(root.path(), path, source);
            files.insert(artifact.path.clone(), artifact);
        }
        let config = ResolverConfig {
            base_url: Some(PathBuf::from(".")),
            paths: BTreeMap::from([("@app/*".into(), vec!["src/app/*".into()])]),
            ..ResolverConfig::default()
        };
        let universe = ResolutionUniverse::build(&files, &config);
        let resolve = |specifier: &str| {
            resolve_one(
                root.path(),
                "src/consumer.ts",
                specifier,
                &universe,
                &config,
            )
            .target
        };
        assert_eq!(resolve("@app/thing"), None);
        // With no pattern matching, baseUrl still applies.
        assert_eq!(resolve("plain"), Some("plain.ts".to_owned()));
    }

    fn universe_of(files: &[(&str, &str)]) -> (BTreeMap<String, FileArtifact>, ResolutionUniverse) {
        let artifacts: BTreeMap<String, FileArtifact> = files
            .iter()
            .map(|(path, source)| ((*path).to_owned(), parse_source(path, source.as_bytes())))
            .collect();
        let universe = ResolutionUniverse::build(&artifacts, &ResolverConfig::default());
        (artifacts, universe)
    }

    #[test]
    fn per_path_universe_overlays_reproduce_a_rebuilt_universe() {
        // `run` is defined in every file, so each edit touches a name other files also define --
        // exactly what the per-path overlay must leave alone.
        let before = [
            (
                "a.ts",
                "export class A { run() {} }\nexport function run() {}",
            ),
            (
                "b.ts",
                "export class B { run() {} }\nexport const shared = 1;",
            ),
            ("c.ts", "export class C { run() {} run2() {} }"),
        ];
        let (old_artifacts, base) = universe_of(&before);
        let first = [
            // `run` gains a second definition in a.ts and `shared` moves here from b.ts.
            (
                "a.ts",
                "export class A { run() {} }\nexport function run() {}\nexport const shared = 2;\nfunction run3() {}",
            ),
            ("b.ts", "export class B { run() {} }"),
            ("c.ts", "export class C { run() {} run2() {} }"),
        ];
        let (first_artifacts, first_universe) = universe_of(&first);
        let second = [
            (
                "a.ts",
                "export class A { run() {} }\nexport function run() {}\nexport const shared = 2;\nfunction run3() {}",
            ),
            ("b.ts", "export class B { run() {} }"),
            // c.ts is deleted and d.ts added with the same names.
            ("d.ts", "export class C { run() {} run2() {} }"),
        ];
        let (second_artifacts, second_universe) = universe_of(&second);

        let first_overlay = ResolutionUniverseOverlay::from_artifact_changes(
            ["a.ts", "b.ts"]
                .into_iter()
                .map(|path| (old_artifacts.get(path), first_artifacts.get(path))),
        );
        let second_overlay = ResolutionUniverseOverlay::from_artifact_changes(
            ["c.ts", "d.ts"]
                .into_iter()
                .map(|path| (first_artifacts.get(path), second_artifacts.get(path))),
        );

        let mut applied = base.clone();
        applied.apply_overlay(&first_overlay);
        assert_eq!(applied, first_universe);
        applied.apply_overlay(&second_overlay);
        assert_eq!(applied, second_universe);

        // Composition is what overlay-chain compaction stores.
        let mut composed = first_overlay.clone();
        composed.compose(second_overlay.clone());
        let mut applied = base.clone();
        applied.apply_overlay(&composed);
        assert_eq!(applied, second_universe);

        // A lookup over an unapplied overlay answers what the rebuilt universe answers.
        let lookup = OverlayResolutionLookup::new(&first_universe, &second_overlay);
        for name in ["run", "run2", "run3", "shared", "A", "C", "missing"] {
            assert_eq!(
                lookup.symbol_definer_count(name),
                second_universe.symbol_definer_count(name),
                "{name}"
            );
            assert_eq!(
                lookup.symbol_definitions(name).to_vec(),
                second_universe.symbol_definitions(name).to_vec(),
                "{name}"
            );
            for path in ["a.ts", "b.ts", "c.ts", "d.ts"] {
                assert_eq!(
                    lookup.symbol_definitions_in_file(name, path),
                    second_universe.symbol_definitions_in_file(name, path),
                    "{name} in {path}"
                );
            }
        }

        // The overlay carries only the edited files' runs, not other files' definitions.
        assert!(
            first_overlay.symbol_definitions["run"]
                .keys()
                .eq(["a.ts", "b.ts"])
        );

        // And recording replacements on an owned universe yields the same overlay content.
        let mut owned = base;
        let mut recorded = ResolutionUniverseOverlay::default();
        for path in ["a.ts", "b.ts"] {
            owned.replace_artifact_with_overlay(
                old_artifacts.get(path),
                first_artifacts.get(path),
                &mut recorded,
            );
        }
        assert_eq!(owned, first_universe);
        let mut replayed = universe_of(&before).1;
        replayed.apply_overlay(&recorded);
        assert_eq!(replayed, first_universe);
    }

    #[test]
    #[ignore = "performance probe"]
    fn persisted_universe_21k_subset_benchmark() {
        let root = tempdir().unwrap();
        let config = ResolverConfig::default();
        let artifacts: BTreeMap<String, FileArtifact> = (0..21_000)
            .map(|index| {
                let path = format!("src/f{index}.ts");
                let source = format!("export function S{index}() {{}}");
                (path.clone(), parse_source(&path, source.as_bytes()))
            })
            .collect();
        let universe = ResolutionUniverse::build(&artifacts, &config);
        let subset = BTreeMap::from([(
            "src/changed.ts".to_owned(),
            parse_source("src/changed.ts", b"export function changed() { S42(); }"),
        )]);
        let started = std::time::Instant::now();
        for _ in 0..100 {
            std::hint::black_box(resolve_subset_with_contributions(
                root.path(),
                &subset,
                &universe,
                &config,
            ));
        }
        eprintln!(
            "21k persisted-universe subset mean_us={}",
            started.elapsed().as_micros() / 100
        );
    }
}
