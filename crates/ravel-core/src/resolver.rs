use crate::model::{
    Edge, EdgeConfidence, EdgeKind, EdgeProvenance, ExportBindingKind, FileArtifact,
    ImportBindingKind, Span, SymbolRef,
};
use rustc_hash::FxHashSet;
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
    pub symbol_definitions: BTreeMap<String, Option<Vec<SymbolDefinition>>>,
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
        match self.overlay.symbol_definitions.get(name) {
            Some(Some(definitions)) => u32::try_from(definitions.len()).unwrap_or(u32::MAX),
            Some(None) => 0,
            None => self.base.symbol_definer_count(name),
        }
    }

    fn symbol_definitions(&self, name: &str) -> LookupSlice<'_, SymbolDefinition> {
        match self.overlay.symbol_definitions.get(name) {
            Some(Some(value)) => LookupSlice::Borrowed(value),
            Some(None) => LookupSlice::Borrowed(&[]),
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
}

impl ResolutionUniverseOverlay {
    pub fn from_artifact_changes<'a>(
        base: &dyn ResolutionLookup,
        changes: impl IntoIterator<Item = (Option<&'a FileArtifact>, Option<&'a FileArtifact>)>,
    ) -> Self {
        let mut overlay = Self::default();
        let mut old_ids: BTreeMap<String, FxHashSet<String>> = BTreeMap::new();
        let mut new_definitions: BTreeMap<String, Vec<SymbolDefinition>> = BTreeMap::new();
        for (old, new) in changes {
            if let Some(old) = old {
                overlay.files.insert(old.path.clone(), false);
                overlay.module_exports.insert(old.path.clone(), None);
                for symbol in &old.symbols {
                    old_ids
                        .entry(symbol.name.clone())
                        .or_default()
                        .insert(symbol.id.clone());
                }
            }
            if let Some(new) = new {
                overlay.files.insert(new.path.clone(), true);
                overlay
                    .module_exports
                    .insert(new.path.clone(), Some(module_exports(new)));
                for symbol in &new.symbols {
                    new_definitions
                        .entry(symbol.name.clone())
                        .or_default()
                        .push(SymbolDefinition::from_symbol(new, symbol));
                }
            }
        }
        let names: BTreeSet<_> = old_ids
            .keys()
            .chain(new_definitions.keys())
            .cloned()
            .collect();
        for name in names {
            let mut definitions = base.symbol_definitions(&name).into_owned();
            if let Some(ids) = old_ids.get(&name) {
                definitions.retain(|definition| !ids.contains(&definition.id));
            }
            definitions.extend(new_definitions.remove(&name).unwrap_or_default());
            definitions.sort_by(|left, right| {
                (&left.path, left.span, &left.qualified_name).cmp(&(
                    &right.path,
                    right.span,
                    &right.qualified_name,
                ))
            });
            overlay
                .symbol_definitions
                .insert(name, (!definitions.is_empty()).then_some(definitions));
        }
        overlay
    }
}

impl ResolutionUniverse {
    pub const FORMAT_VERSION: u32 = 6;

    pub fn build(artifacts: &BTreeMap<String, FileArtifact>, config: &ResolverConfig) -> Self {
        let mut universe = Self {
            format_version: Self::FORMAT_VERSION,
            resolver_fingerprint: resolver_fingerprint(config),
            ..Self::default()
        };
        for artifact in artifacts.values() {
            universe.files.insert(artifact.path.clone());
            for symbol in &artifact.symbols {
                universe
                    .symbol_definitions
                    .entry(symbol.name.clone())
                    .or_default()
                    .push(SymbolDefinition::from_symbol(artifact, symbol));
            }
            universe
                .module_exports
                .insert(artifact.path.clone(), module_exports(artifact));
        }
        for definitions in universe.symbol_definitions.values_mut() {
            definitions.sort_by(|left, right| {
                (&left.path, left.span, &left.qualified_name).cmp(&(
                    &right.path,
                    right.span,
                    &right.qualified_name,
                ))
            });
        }
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
                let definitions = self
                    .symbol_definitions
                    .entry(symbol.name.clone())
                    .or_default();
                definitions.push(SymbolDefinition::from_symbol(new, symbol));
                definitions.sort_by(|left, right| {
                    (&left.path, left.span, &left.qualified_name).cmp(&(
                        &right.path,
                        right.span,
                        &right.qualified_name,
                    ))
                });
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
        for path in paths {
            overlay
                .files
                .insert(path.clone(), self.files.contains(&path));
        }
        for symbol in symbols {
            overlay.symbol_definitions.insert(
                symbol.clone(),
                self.symbol_definitions.get(&symbol).cloned(),
            );
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
        apply_optional_map(&mut self.symbol_definitions, &overlay.symbol_definitions);
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
    if !exports.is_empty() {
        for export in exports.iter() {
            let exact = export.exported == exported_name;
            let star = export.kind == ExportBindingKind::Star;
            if !exact && !star {
                continue;
            }
            if export.kind == ExportBindingKind::Namespace && exact {
                // The namespace itself is a module object, not a declaration. A later member
                // reference can resolve through the source module without fabricating a symbol.
                continue;
            }
            if let Some(specifier) = export.source.as_deref() {
                let resolution = resolve_one(root, file, specifier, universe, config);
                if let Some(target_file) = resolution.target {
                    let next_name = if star {
                        exported_name
                    } else {
                        export.local.as_str()
                    };
                    targets.extend(resolve_exported_symbol(
                        root,
                        &target_file,
                        next_name,
                        universe,
                        config,
                        visited,
                    ));
                }
            } else {
                targets.extend(definitions_in_file(universe, file, &export.local));
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
    universe
        .symbol_definitions(name)
        .iter()
        .filter(|definition| definition.path == file)
        .cloned()
        .collect()
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
    let matches: Vec<_> = universe
        .symbol_definitions(leaf)
        .iter()
        .filter(|definition| definition.path == path && definition.qualified_name == qualified_name)
        .cloned()
        .collect();
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

fn source_definition_is_type_only(artifact: &FileArtifact, id: &str) -> bool {
    artifact.symbols.iter().any(|symbol| {
        symbol.id == id
            && matches!(
                symbol.kind.as_ref(),
                "interface_declaration" | "type_alias_declaration"
            )
    })
}

fn resolve_symbol_reference(
    root: &Path,
    artifact: &FileArtifact,
    reference: &SymbolRef,
    imports: Option<&ResolvedImports>,
    universe: &dyn ResolutionLookup,
    config: &ResolverConfig,
) -> Option<SymbolDefinition> {
    let raw = reference.to.as_str();
    let source_definition = artifact
        .symbols
        .iter()
        .find(|symbol| symbol.id == reference.from_id);
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
        let owner = artifact
            .symbols
            .iter()
            .filter(|candidate| {
                matches!(
                    candidate.kind.as_ref(),
                    "class_declaration" | "interface_declaration"
                ) && (source.qualified_name == candidate.qualified_name
                    || source
                        .qualified_name
                        .starts_with(&format!("{}.", candidate.qualified_name)))
            })
            .max_by_key(|candidate| candidate.qualified_name.len())?;
        return find_qualified_definition_for(
            universe,
            &artifact.path,
            &format!("{}.{}", owner.qualified_name, member),
            required,
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
    hasher.update(b"ravel-resolver-v2\0");
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
        built_universe = ResolutionUniverse::build(artifacts, config);
        &built_universe
    };
    use rayon::prelude::*;
    // Per-artifact resolution only reads the shared universe, so fan it out across cores.
    // Results are merged in BTreeMap order below, which keeps edge/trace/contribution
    // ordering identical to the sequential implementation.
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
                if let Some(specifier) = &export.specifier {
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
    let ref_edges: Vec<Vec<(RefEdgeKey, Edge)>> = artifacts
        .values()
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|artifact| {
            let imports = imported_bindings.get(&artifact.path);
            let mut out = Vec::new();
            for r in &artifact.symbol_refs {
                let Some(target) =
                    resolve_symbol_reference(root, artifact, r, imports, universe, config)
                else {
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
                        && source_definition_is_type_only(artifact, &from));
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

    edges.sort_by(|a, b| (&a.from, &a.to, &a.kind).cmp(&(&b.from, &b.to, &b.kind)));
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

const DEFAULT_RESOLVE_EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

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
    let normalized_base = normalize_lexical(root, base);
    attempted.insert(normalized_base.clone());
    if universe.contains_file(&normalized_base) {
        existing.push(normalized_base);
        return CandidateProbe {
            existing,
            attempted,
        };
    }
    // Iterate config extensions by reference; fall back to a static default set — no per-call
    // `Vec<String>` clone/allocation.
    let probe = |ext: &str, existing: &mut Vec<String>, attempted: &mut BTreeSet<String>| {
        let source_extension = base
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| DEFAULT_RESOLVE_EXTS.contains(&value));
        let path = if source_extension {
            base.with_extension(ext)
        } else {
            PathBuf::from(format!("{}.{ext}", base.to_string_lossy()))
        };
        let normalized = normalize_lexical(root, &path);
        attempted.insert(normalized.clone());
        if universe.contains_file(&normalized) {
            existing.push(normalized);
            true
        } else {
            false
        }
    };
    if config.extensions.is_empty() {
        for &ext in DEFAULT_RESOLVE_EXTS {
            if probe(ext, &mut existing, &mut attempted) {
                break;
            }
        }
    } else {
        for ext in &config.extensions {
            if probe(ext, &mut existing, &mut attempted) {
                break;
            }
        }
    }
    if existing.is_empty() {
        for extension in ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"] {
            let path = base.join(format!("index.{extension}"));
            let normalized = normalize_lexical(root, &path);
            attempted.insert(normalized.clone());
            if universe.contains_file(&normalized) {
                existing.push(normalized);
                break;
            }
        }
    }
    CandidateProbe {
        existing,
        attempted,
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
    load_tsconfig_recursive(root, &root.join("tsconfig.json"), &mut BTreeSet::new())
        .unwrap_or_default()
}

fn load_tsconfig_recursive(
    root: &Path,
    path: &Path,
    visited: &mut BTreeSet<PathBuf>,
) -> Option<ResolverConfig> {
    let identity = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(identity) {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let value = parse_jsonc(&text)?;
    let directory = path.parent().unwrap_or(root);
    let mut config = ResolverConfig::default();
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
        if let Some(base) = load_tsconfig_recursive(root, &inherited_path, visited) {
            if base.base_url.is_some() {
                config.base_url = base.base_url;
            }
            if !base.paths.is_empty() {
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
            &directory.join(base_url),
        )));
    }
    if let Some(raw_paths) = options.get("paths").and_then(|value| {
        serde_json::from_value::<BTreeMap<String, Vec<String>>>(value.clone()).ok()
    }) {
        let target_base = config
            .base_url
            .as_ref()
            .map(|base| root.join(base))
            .unwrap_or_else(|| directory.to_path_buf());
        config.paths = raw_paths
            .into_iter()
            .map(|(alias, targets)| {
                (
                    alias,
                    targets
                        .into_iter()
                        .map(|target| normalize_lexical(root, &target_base.join(target)))
                        .collect(),
                )
            })
            .collect();
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
    serde_json::from_slice(&json).ok()
}

#[cfg(test)]
mod tests {
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
