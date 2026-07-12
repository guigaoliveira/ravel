//! Guard for incremental `sync`: its published state MUST equal a full `index`
//! of the same final tree, for every mutation kind (edit body, edit imports,
//! add file, delete file). This is the safety net for sidecar reuse and future
//! incremental resolve changes.

use ravel_core::{
    config::{Config, Flags},
    engine::WorkspaceEngine,
};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

/// Structural + sidecar fingerprint of the engine's published state. Node names
/// are workspace-relative, so two different temp roots are directly comparable.
fn fingerprint(engine: &WorkspaceEngine) -> Vec<String> {
    let snapshot = engine.snapshot().expect("snapshot");
    let mut out = vec![format!(
        "snapshot={}",
        serde_json::to_string(&serde_json::json!({
            "files": snapshot.files,
            "edges": snapshot.edges,
        }))
        .unwrap()
    )];
    let stats = engine.stats().expect("stats");
    out.push(format!(
        "stats={}",
        serde_json::to_string(&serde_json::json!({
            "files": stats.files,
            "edges": stats.edges,
            "bytes": stats.bytes,
            "parse_errors": stats.parse_errors,
        }))
        .unwrap()
    ));
    let graph = engine.graph().expect("graph");
    out.push(format!("edges={}", graph.edge_count()));
    let mut names: Vec<String> = graph.node_names().map(str::to_string).collect();
    names.sort();
    for n in &names {
        let mut fwd = graph.neighbors_forward(n);
        fwd.sort();
        out.push(format!("adj {n} -> {}", fwd.join(",")));
    }
    let mut orphans = engine.orphans(1000).expect("orphans");
    orphans.sort();
    out.push(format!("orphans={}", orphans.join("|")));
    let mut hubs: Vec<String> = engine
        .hubs(100, None)
        .expect("hubs")
        .iter()
        .map(|h| format!("{}:{}:{}", h.name, h.in_degree, h.out_degree))
        .collect();
    hubs.sort();
    out.push(format!("hubs={}", hubs.join("|")));
    for (label, kind) in [
        ("exact", ravel_core::search::SearchKind::Exact),
        ("prefix", ravel_core::search::SearchKind::Prefix),
        ("fuzzy", ravel_core::search::SearchKind::Fuzzy),
        ("regex", ravel_core::search::SearchKind::Regex),
    ] {
        out.push(format!(
            "search-{label}={}",
            serde_json::to_string(&engine.search("a", kind, 100).expect("search")).unwrap()
        ));
    }
    out.push(format!(
        "packages={}",
        serde_json::to_string(&engine.list_packages().expect("packages")).unwrap()
    ));
    out.push("endpoints=[]".into());
    let mut schema = engine.describe_schema().expect("schema");
    if let Some(object) = schema.as_object_mut() {
        object.remove("snapshot_id");
    }
    out.push(format!("schema={schema}"));
    out.push(format!(
        "cycles={}",
        serde_json::to_string(&engine.cycles(None).expect("cycles")).unwrap()
    ));
    out.push(format!(
        "policy={}",
        serde_json::to_string(&engine.validate().expect("policy")).unwrap()
    ));
    out
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn engine(root: &Path) -> WorkspaceEngine {
    WorkspaceEngine::load(root, &Flags::default()).unwrap()
}

/// Initial tree shared by every scenario.
fn seed(root: &Path) {
    write(
        root,
        "src/a.ts",
        "import { b } from './b';\nexport const a = b;\n",
    );
    write(root, "src/b.ts", "export const b = 1;\n");
    write(root, "src/c.ts", "export const c = 2;\n");
    write(
        root,
        "src/main.ts",
        "import { a } from './a';\nexport const main = a;\n",
    );
}

/// Apply `mutate` via incremental sync in one dir, and via a full index of the
/// same final tree in another; assert identical published state.
fn assert_sync_matches_full_index(
    scenario: &str,
    mutate: impl Fn(&Path) -> Vec<PathBuf>,
    final_tree: impl Fn(&Path),
) {
    // Incremental path: seed → index → mutate → sync(changed paths).
    let inc = tempdir().unwrap();
    seed(inc.path());
    let inc_engine = engine(inc.path());
    inc_engine.index().unwrap();
    let changed = mutate(inc.path());
    inc_engine.sync(Some(&changed)).unwrap();
    let inc_fp = fingerprint(&inc_engine);

    // Ground truth: fresh dir with the final tree, full index.
    let full = tempdir().unwrap();
    final_tree(full.path());
    let full_engine = engine(full.path());
    full_engine.index().unwrap();
    let full_fp = fingerprint(&full_engine);

    assert_eq!(
        inc_fp, full_fp,
        "sync != full index for scenario: {scenario}"
    );
}

#[test]
fn sync_matches_full_index_on_body_edit() {
    assert_sync_matches_full_index(
        "edit body (no import change)",
        |root| {
            write(
                root,
                "src/b.ts",
                "export const b = 1;\nexport const extra = 42;\n",
            );
            vec![root.join("src/b.ts")]
        },
        |root| {
            seed(root);
            write(
                root,
                "src/b.ts",
                "export const b = 1;\nexport const extra = 42;\n",
            );
        },
    );
}

#[test]
fn index_rebuilds_when_effective_config_changes_without_source_edits() {
    let dir = tempdir().unwrap();
    seed(dir.path());
    let first = engine(dir.path());
    first.index().unwrap();
    let before = first.storage().read_manifest().unwrap().unwrap();

    // The source tree is unchanged; only an analysis input changes. A fresh
    // engine must not let the file-hash fast path return the old sidecars.
    write(
        dir.path(),
        ".ravel.toml",
        "[analysis]\nentry_points = [\"src/main.ts\"]\n",
    );
    let second = engine(dir.path());
    second.index().unwrap();
    let after = second.storage().read_manifest().unwrap().unwrap();

    assert_ne!(
        before.snapshot_id.config_hash,
        after.snapshot_id.config_hash
    );
    assert_ne!(before.snapshot_id, after.snapshot_id);
    assert_eq!(after.snapshot_id.config_hash, second.config.hash());
}

#[test]
fn index_rebuilds_when_ignore_rules_change_without_source_edits() {
    let dir = tempdir().unwrap();
    seed(dir.path());
    let first = engine(dir.path());
    first.index().unwrap();
    assert!(first.snapshot().unwrap().files.contains_key("src/c.ts"));

    write(
        dir.path(),
        ".ravelignore",
        "src/\n!src/a.ts\n!src/b.ts\n!src/main.ts\n",
    );
    let config = Config::load(dir.path(), &Flags::default()).unwrap();
    let discovered = ravel_core::config::discover_files(&config).unwrap();
    assert!(!discovered.iter().any(|path| path.ends_with("src/c.ts")));
    let second = engine(dir.path());
    second.index().unwrap();

    assert!(!second.snapshot().unwrap().files.contains_key("src/c.ts"));
}

#[test]
fn sync_re_resolves_when_workspace_metadata_changes() {
    let inc = tempdir().unwrap();
    seed(inc.path());
    write(
        inc.path(),
        "package.json",
        r#"{"workspaces":["packages/*"]}"#,
    );
    write(
        inc.path(),
        "packages/tool/package.json",
        r#"{"name":"@workspace/tool","main":"src/index.ts"}"#,
    );
    write(
        inc.path(),
        "packages/tool/src/index.ts",
        "export const tool = 3;\n",
    );
    write(
        inc.path(),
        "src/main.ts",
        "import { tool } from '@workspace/tool';\nexport const main = tool;\n",
    );
    let inc_engine = engine(inc.path());
    inc_engine.index().unwrap();

    write(inc.path(), "package.json", r#"{"workspaces":[]}"#);
    inc_engine
        .sync(Some(&[inc.path().join("package.json")]))
        .unwrap();
    let inc_fp = fingerprint(&inc_engine);

    let full = tempdir().unwrap();
    write(full.path(), "package.json", r#"{"workspaces":[]}"#);
    seed(full.path());
    write(
        full.path(),
        "packages/tool/package.json",
        r#"{"name":"@workspace/tool","main":"src/index.ts"}"#,
    );
    write(
        full.path(),
        "packages/tool/src/index.ts",
        "export const tool = 3;\n",
    );
    write(
        full.path(),
        "src/main.ts",
        "import { tool } from '@workspace/tool';\nexport const main = tool;\n",
    );
    let full_engine = engine(full.path());
    full_engine.index().unwrap();

    assert_eq!(inc_fp, fingerprint(&full_engine));
}

#[test]
fn sync_matches_full_index_on_import_edit() {
    assert_sync_matches_full_index(
        "edit imports (edge set changes)",
        |root| {
            // a.ts now also imports c.ts → new edge.
            write(
                root,
                "src/a.ts",
                "import { b } from './b';\nimport { c } from './c';\nexport const a = b + c;\n",
            );
            vec![root.join("src/a.ts")]
        },
        |root| {
            seed(root);
            write(
                root,
                "src/a.ts",
                "import { b } from './b';\nimport { c } from './c';\nexport const a = b + c;\n",
            );
        },
    );
}

#[test]
fn sync_matches_full_index_on_add_file() {
    assert_sync_matches_full_index(
        "add a new imported file",
        |root| {
            write(root, "src/d.ts", "export const d = 4;\n");
            write(
                root,
                "src/main.ts",
                "import { a } from './a';\nimport { d } from './d';\nexport const main = a + d;\n",
            );
            vec![root.join("src/d.ts"), root.join("src/main.ts")]
        },
        |root| {
            seed(root);
            write(root, "src/d.ts", "export const d = 4;\n");
            write(
                root,
                "src/main.ts",
                "import { a } from './a';\nimport { d } from './d';\nexport const main = a + d;\n",
            );
        },
    );
}

#[test]
fn sync_re_resolves_existing_unresolved_import_when_file_is_added() {
    let inc = tempdir().unwrap();
    seed(inc.path());
    write(
        inc.path(),
        "src/main.ts",
        "import { d } from './d';\nexport const main = d;\n",
    );
    let inc_engine = engine(inc.path());
    inc_engine.index().unwrap();
    write(inc.path(), "src/d.ts", "export const d = 4;\n");
    inc_engine
        .sync(Some(&[inc.path().join("src/d.ts")]))
        .unwrap();

    let full = tempdir().unwrap();
    seed(full.path());
    write(
        full.path(),
        "src/main.ts",
        "import { d } from './d';\nexport const main = d;\n",
    );
    write(full.path(), "src/d.ts", "export const d = 4;\n");
    let full_engine = engine(full.path());
    full_engine.index().unwrap();

    assert_eq!(fingerprint(&inc_engine), fingerprint(&full_engine));
}

#[test]
fn sync_matches_full_index_on_delete_file() {
    assert_sync_matches_full_index(
        "delete a file (dangling import remains)",
        |root| {
            fs::remove_file(root.join("src/c.ts")).unwrap();
            vec![root.join("src/c.ts")]
        },
        |root| {
            seed(root);
            fs::remove_file(root.join("src/c.ts")).unwrap();
        },
    );
}

#[test]
fn sync_re_resolves_importer_when_imported_file_is_deleted() {
    let inc = tempdir().unwrap();
    seed(inc.path());
    let inc_engine = engine(inc.path());
    inc_engine.index().unwrap();
    fs::remove_file(inc.path().join("src/b.ts")).unwrap();
    inc_engine
        .sync(Some(&[inc.path().join("src/b.ts")]))
        .unwrap();

    let full = tempdir().unwrap();
    seed(full.path());
    fs::remove_file(full.path().join("src/b.ts")).unwrap();
    let full_engine = engine(full.path());
    full_engine.index().unwrap();

    assert_eq!(fingerprint(&inc_engine), fingerprint(&full_engine));
}
