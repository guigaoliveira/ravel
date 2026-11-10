//! Incremental `sync` and a full `index` must agree on which graph nodes exist, including the
//! files that only appear as the `source_path` of symbol-level edges. A full index used to
//! intern such a file as a node with no edges while an incremental overlay never created it (and
//! never retired it), so `contains_node` / `node_names` / `node_count` depended on how the
//! index was built.

use ravel_core::{config::Flags, engine::WorkspaceEngine};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

/// A file whose only edges are between its own symbols: no import in or out.
const SELF_CONTAINED: &str = "function g() { return 1; }\nexport function f() { return g(); }\n";

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn seed(root: &Path) {
    write(
        root,
        "src/a.ts",
        "import { b } from './b';\nexport const a = b;\n",
    );
    write(root, "src/b.ts", "export const b = 1;\n");
}

fn nodes(engine: &WorkspaceEngine) -> (Vec<String>, usize, bool) {
    let graph = engine.graph().unwrap();
    let mut names: Vec<String> = graph.node_names().map(str::to_owned).collect();
    names.sort();
    (names, graph.node_count(), graph.contains_node("src/x.ts"))
}

fn assert_sync_matches_full_index(
    scenario: &str,
    initial: impl Fn(&Path),
    mutate: impl Fn(&Path) -> Vec<PathBuf>,
    final_tree: impl Fn(&Path),
) {
    let inc = tempdir().unwrap();
    initial(inc.path());
    let inc_engine = WorkspaceEngine::load(inc.path(), &Flags::default()).unwrap();
    inc_engine.index().unwrap();
    let changed = mutate(inc.path());
    inc_engine.sync(Some(&changed)).unwrap();

    let full = tempdir().unwrap();
    final_tree(full.path());
    let full_engine = WorkspaceEngine::load(full.path(), &Flags::default()).unwrap();
    full_engine.index().unwrap();

    let (inc_names, inc_count, inc_has_x) = nodes(&inc_engine);
    let (full_names, full_count, full_has_x) = nodes(&full_engine);
    assert_eq!(inc_names, full_names, "{scenario}: node names");
    assert_eq!(inc_count, full_count, "{scenario}: node count");
    assert_eq!(inc_has_x, full_has_x, "{scenario}: contains src/x.ts");
    // `node_count` counts the same nodes `node_names` lists.
    assert_eq!(full_count, full_names.len(), "{scenario}");

    // A reloaded engine reads the published graph cold; it must agree too.
    let reloaded = WorkspaceEngine::load(inc.path(), &Flags::default()).unwrap();
    assert_eq!(nodes(&reloaded).0, full_names, "{scenario}: reloaded");
}

#[test]
fn deleting_a_self_contained_file_matches_a_full_index() {
    assert_sync_matches_full_index(
        "delete",
        |root| {
            seed(root);
            write(root, "src/x.ts", SELF_CONTAINED);
        },
        |root| {
            fs::remove_file(root.join("src/x.ts")).unwrap();
            vec![root.join("src/x.ts")]
        },
        seed,
    );
}

#[test]
fn adding_a_self_contained_file_matches_a_full_index() {
    assert_sync_matches_full_index(
        "add",
        seed,
        |root| {
            write(root, "src/x.ts", SELF_CONTAINED);
            vec![root.join("src/x.ts")]
        },
        |root| {
            seed(root);
            write(root, "src/x.ts", SELF_CONTAINED);
        },
    );
}
