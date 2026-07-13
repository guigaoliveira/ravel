use ravel_core::{
    model::{Edge, EdgeKind, FileArtifact},
    resolver::{ResolverConfig, resolve_edges},
    scanner::parse_source,
    structural::StructuralReverseIndex,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};
use tempfile::tempdir;

fn materialize(root: &Path, files: &[(&str, &str)]) -> BTreeMap<String, FileArtifact> {
    let mut artifacts = BTreeMap::new();
    for (path, source) in files {
        let absolute = root.join(path);
        fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        fs::write(&absolute, source).unwrap();
        let artifact = parse_source(path, source.as_bytes());
        artifacts.insert((*path).to_owned(), artifact);
    }
    artifacts
}

fn module_edges_by_owner(edges: Vec<Edge>) -> BTreeMap<String, BTreeSet<String>> {
    let mut result = BTreeMap::new();
    for edge in edges {
        if matches!(edge.kind, EdgeKind::Import | EdgeKind::ReExport) {
            result
                .entry(edge.from.clone())
                .or_insert_with(BTreeSet::new)
                .insert(format!("{:?}:{}", edge.kind, edge.to));
        }
    }
    result
}

fn assert_oracle(
    root: &Path,
    old: &BTreeMap<String, FileArtifact>,
    new: &BTreeMap<String, FileArtifact>,
    changed_paths: &[&str],
    config: &ResolverConfig,
) {
    let reverse = StructuralReverseIndex::build(root, old, config);
    let old_symbols: BTreeSet<&str> = old
        .values()
        .flat_map(|artifact| artifact.symbols.iter().map(|symbol| symbol.name.as_str()))
        .collect();
    let new_symbols: BTreeSet<&str> = new
        .values()
        .flat_map(|artifact| artifact.symbols.iter().map(|symbol| symbol.name.as_str()))
        .collect();
    let changed_symbols: BTreeSet<&str> = old_symbols
        .symmetric_difference(&new_symbols)
        .copied()
        .collect();
    let affected = reverse.affected_files(
        changed_paths.iter().copied(),
        changed_symbols.iter().copied(),
    );

    let old_edges = module_edges_by_owner(resolve_edges(root, old, config));
    let new_edges = module_edges_by_owner(resolve_edges(root, new, config));
    let owners: BTreeSet<&str> = old_edges
        .keys()
        .chain(new_edges.keys())
        .map(String::as_str)
        .collect();
    for owner in owners {
        if old_edges.get(owner) != new_edges.get(owner) {
            assert!(
                affected.contains(owner),
                "affected-set missed module-edge owner {owner}; changed={changed_paths:?}"
            );
        }
    }
    for artifact in old.values() {
        if artifact
            .symbol_refs
            .iter()
            .any(|reference| changed_symbols.contains(reference.to.as_str()))
        {
            assert!(
                affected.contains(&artifact.path),
                "affected-set missed symbol referrer {}",
                artifact.path
            );
        }
    }
}

#[test]
fn relative_create_delete_matrix_has_full_oracle_recall() {
    for extension in ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"] {
        let root = tempdir().unwrap();
        let importer = "import { target } from './target';\nexport const caller = target();\n";
        let old = materialize(root.path(), &[("src/caller.ts", importer)]);
        let target_path = format!("src/target.{extension}");
        let mut new = old.clone();
        let source = "export function target() { return 1; }\n";
        fs::write(root.path().join(&target_path), source).unwrap();
        new.insert(
            target_path.clone(),
            parse_source(&target_path, source.as_bytes()),
        );
        assert_oracle(
            root.path(),
            &old,
            &new,
            &[target_path.as_str()],
            &ResolverConfig::default(),
        );

        let reverse_before_delete =
            StructuralReverseIndex::build(root.path(), &new, &ResolverConfig::default());
        fs::remove_file(root.path().join(&target_path)).unwrap();
        let affected = reverse_before_delete.affected_files([target_path.as_str()], ["target"]);
        assert!(affected.contains("src/caller.ts"));
    }
}

#[test]
fn alias_basename_and_symbol_changes_have_full_oracle_recall() {
    let alias_root = tempdir().unwrap();
    let alias_old = materialize(
        alias_root.path(),
        &[(
            "src/caller.ts",
            "import { target } from '@/target';\ntarget();\n",
        )],
    );
    let mut alias_new = alias_old.clone();
    let target = "export function target() {}\n";
    fs::write(alias_root.path().join("src/target.ts"), target).unwrap();
    alias_new.insert(
        "src/target.ts".into(),
        parse_source("src/target.ts", target.as_bytes()),
    );
    let alias_config = ResolverConfig {
        paths: BTreeMap::from([("@/*".into(), vec!["src/*".into()])]),
        ..ResolverConfig::default()
    };
    assert_oracle(
        alias_root.path(),
        &alias_old,
        &alias_new,
        &["src/target.ts"],
        &alias_config,
    );

    let basename_root = tempdir().unwrap();
    let basename_old = materialize(
        basename_root.path(),
        &[(
            "src/caller.ts",
            "import { target } from 'target';\ntarget();\n",
        )],
    );
    let mut basename_new = basename_old.clone();
    fs::create_dir_all(basename_root.path().join("lib")).unwrap();
    fs::write(basename_root.path().join("lib/target.ts"), target).unwrap();
    basename_new.insert(
        "lib/target.ts".into(),
        parse_source("lib/target.ts", target.as_bytes()),
    );
    assert_oracle(
        basename_root.path(),
        &basename_old,
        &basename_new,
        &["lib/target.ts"],
        &ResolverConfig::default(),
    );
}
