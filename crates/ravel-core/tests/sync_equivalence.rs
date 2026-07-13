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

#[test]
fn batched_rename_matches_full_index() {
    assert_sync_matches_full_index(
        "rename old and new paths in one watcher batch",
        |root| {
            fs::rename(root.join("src/b.ts"), root.join("src/renamed.ts")).unwrap();
            vec![root.join("src/b.ts"), root.join("src/renamed.ts")]
        },
        |root| {
            seed(root);
            fs::rename(root.join("src/b.ts"), root.join("src/renamed.ts")).unwrap();
        },
    );
}

#[test]
fn structural_add_updates_ambiguous_symbol_confidence_exactly() {
    assert_sync_matches_full_index(
        "adding a duplicate definition changes existing reference confidence",
        |root| {
            write(
                root,
                "src/definition.ts",
                "export function targetFn() { return 1; }\n",
            );
            write(
                root,
                "src/caller.ts",
                "export function callerFn() { return targetFn(); }\n",
            );
            let engine = engine(root);
            engine.index().unwrap();
            write(
                root,
                "src/duplicate.ts",
                "export function targetFn() { return 2; }\n",
            );
            vec![root.join("src/duplicate.ts")]
        },
        |root| {
            seed(root);
            write(
                root,
                "src/definition.ts",
                "export function targetFn() { return 1; }\n",
            );
            write(
                root,
                "src/caller.ts",
                "export function callerFn() { return targetFn(); }\n",
            );
            write(
                root,
                "src/duplicate.ts",
                "export function targetFn() { return 2; }\n",
            );
        },
    );
}

#[test]
fn collapsed_rename_storm_matches_final_tree() {
    assert_sync_matches_full_index(
        "many rename events collapsed into one final-state batch",
        |root| {
            let mut changed = Vec::new();
            let mut old = root.join("src/c.ts");
            changed.push(old.clone());
            for generation in 0..32 {
                let new = root.join(format!("src/churn-{generation}.ts"));
                fs::rename(&old, &new).unwrap();
                changed.push(new.clone());
                old = new;
            }
            changed
        },
        |root| {
            seed(root);
            fs::rename(root.join("src/c.ts"), root.join("src/churn-31.ts")).unwrap();
        },
    );
}

#[test]
fn structural_tombstone_recreate_then_content_delta_matches_full_index() {
    let inc = tempdir().unwrap();
    seed(inc.path());
    let inc_engine = engine(inc.path());
    inc_engine.index().unwrap();
    let path = inc.path().join("src/c.ts");
    fs::remove_file(&path).unwrap();
    inc_engine.sync(Some(std::slice::from_ref(&path))).unwrap();
    assert!(
        !inc_engine
            .snapshot()
            .unwrap()
            .files
            .contains_key("src/c.ts")
    );
    write(inc.path(), "src/c.ts", "export const recreated = 3;\n");
    inc_engine.sync(Some(std::slice::from_ref(&path))).unwrap();
    write(inc.path(), "src/c.ts", "export const recreated = 33;\n");
    inc_engine.sync(Some(std::slice::from_ref(&path))).unwrap();

    let full = tempdir().unwrap();
    seed(full.path());
    write(full.path(), "src/c.ts", "export const recreated = 33;\n");
    let full_engine = engine(full.path());
    full_engine.index().unwrap();
    assert_eq!(fingerprint(&inc_engine), fingerprint(&full_engine));
    assert!(inc_engine.validate().is_ok());
}

/// Manual baseline for structural-index work:
/// `cargo test -p ravel-core --test sync_equivalence structural_rename_churn_benchmark -- --ignored --nocapture`
#[test]
#[ignore]
fn structural_rename_churn_benchmark() {
    let dir = tempdir().unwrap();
    let file_count = std::env::var("RAVEL_BENCH_FILES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1_000);
    let iterations = std::env::var("RAVEL_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    for index in 0..file_count {
        write(
            dir.path(),
            &format!("src/file-{index}.ts"),
            &format!("export const value{index} = {index};\n"),
        );
    }
    let engine = engine(dir.path());
    engine.index().unwrap();
    let started = std::time::Instant::now();
    for generation in 0..iterations {
        let (old, new) = if generation % 2 == 0 {
            ("src/file-0.ts", "src/file-zero.ts")
        } else {
            ("src/file-zero.ts", "src/file-0.ts")
        };
        fs::rename(dir.path().join(old), dir.path().join(new)).unwrap();
        engine
            .sync(Some(&[dir.path().join(old), dir.path().join(new)]))
            .unwrap();
    }
    let elapsed = started.elapsed();
    eprintln!(
        "structural rename churn: files={file_count} iterations={iterations} total_ms={} mean_ms={:.2}",
        elapsed.as_millis(),
        elapsed.as_secs_f64() * 1_000.0 / f64::from(iterations)
    );
    assert_eq!(engine.stats().unwrap().files, file_count);
}

#[test]
fn concurrent_syncs_serialize_without_losing_changes() {
    let inc = tempdir().unwrap();
    seed(inc.path());
    let inc_engine = engine(inc.path());
    inc_engine.index().unwrap();
    write(inc.path(), "src/d.ts", "export const d = 4;\n");
    write(inc.path(), "src/e.ts", "export const e = 5;\n");

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let handles: Vec<_> = ["src/d.ts", "src/e.ts"]
        .into_iter()
        .map(|relative| {
            let engine = inc_engine.clone();
            let barrier = barrier.clone();
            let path = inc.path().join(relative);
            std::thread::spawn(move || {
                barrier.wait();
                engine.sync(Some(&[path])).unwrap();
            })
        })
        .collect();
    barrier.wait();
    for handle in handles {
        handle.join().unwrap();
    }

    let full = tempdir().unwrap();
    seed(full.path());
    write(full.path(), "src/d.ts", "export const d = 4;\n");
    write(full.path(), "src/e.ts", "export const e = 5;\n");
    let full_engine = engine(full.path());
    full_engine.index().unwrap();

    assert_eq!(fingerprint(&inc_engine), fingerprint(&full_engine));
}

#[test]
fn independent_engines_serialize_updates_without_losing_changes() {
    let inc = tempdir().unwrap();
    seed(inc.path());
    let first = engine(inc.path());
    first.index().unwrap();
    let second = engine(inc.path());
    write(inc.path(), "src/d.ts", "export const d = 4;\n");
    write(inc.path(), "src/e.ts", "export const e = 5;\n");

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let handles = [
        (first, inc.path().join("src/d.ts")),
        (second, inc.path().join("src/e.ts")),
    ]
    .into_iter()
    .map(|(engine, path)| {
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            engine.sync(Some(&[path])).unwrap();
        })
    })
    .collect::<Vec<_>>();
    barrier.wait();
    for handle in handles {
        handle.join().unwrap();
    }

    let snapshot = engine(inc.path()).snapshot().unwrap();
    assert!(snapshot.files.contains_key("src/d.ts"));
    assert!(snapshot.files.contains_key("src/e.ts"));
}

#[test]
fn resident_engine_observes_snapshot_published_by_another_engine() {
    let dir = tempdir().unwrap();
    seed(dir.path());
    write(
        dir.path(),
        "src/swap.ts",
        "export function oldSymbol() { return 1; }\n",
    );
    let writer = engine(dir.path());
    writer.index().unwrap();
    let reader = engine(dir.path());

    // Populate the reader's resident graph and search caches before the external update.
    assert!(reader.graph().unwrap().contains_node("src/a.ts"));
    assert!(
        reader
            .search("newSymbol", ravel_core::search::SearchKind::Exact, 10)
            .unwrap()
            .is_empty()
    );
    let before_generation = reader.storage().current_generation().unwrap();

    // Same-size edit: generation identity must depend on content, not byte count.
    write(
        dir.path(),
        "src/swap.ts",
        "export function newSymbol() { return 1; }\n",
    );
    writer
        .sync(Some(&[dir.path().join("src/swap.ts")]))
        .unwrap();
    let after_generation = reader.storage().current_generation().unwrap();
    assert_ne!(before_generation, after_generation);
    let writer_hits = writer
        .search("newSymbol", ravel_core::search::SearchKind::Exact, 10)
        .unwrap();
    assert!(!writer_hits.is_empty(), "writer hits={writer_hits:?}");

    let hits = reader
        .search("newSymbol", ravel_core::search::SearchKind::Exact, 10)
        .unwrap();
    assert!(
        hits.iter().any(|hit| hit.value == "newSymbol"),
        "reader did not refresh external symbol; hits={hits:?}, generation={:?}",
        reader.storage().current_generation().unwrap()
    );
}

#[test]
fn explicit_sync_rejects_paths_outside_workspace() {
    let dir = tempdir().unwrap();
    let outside = tempdir().unwrap();
    seed(dir.path());
    write(
        outside.path(),
        "outside.ts",
        "export const outside = true;\n",
    );
    let engine = engine(dir.path());
    engine.index().unwrap();

    let error = engine
        .sync(Some(&[outside.path().join("outside.ts")]))
        .unwrap_err();
    assert!(
        matches!(
            error,
            ravel_core::engine::EngineError::PathOutsideWorkspace { .. }
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn sync_recovers_and_drains_a_persisted_pending_batch() {
    let dir = tempdir().unwrap();
    seed(dir.path());
    let engine = engine(dir.path());
    engine.index().unwrap();
    let queued = dir.path().join("src/queued.ts");
    let direct = dir.path().join("src/direct.ts");
    write(dir.path(), "src/queued.ts", "export const queued = true;\n");
    write(dir.path(), "src/direct.ts", "export const direct = true;\n");

    let pending = dir.path().join(".ravel/pending-sync/stale.json");
    std::fs::create_dir_all(pending.parent().unwrap()).unwrap();
    std::fs::write(&pending, serde_json::to_vec(&vec![queued]).unwrap()).unwrap();

    engine.sync(Some(&[direct])).unwrap();
    let snapshot = engine.snapshot().unwrap();
    assert!(snapshot.files.contains_key("src/queued.ts"));
    assert!(snapshot.files.contains_key("src/direct.ts"));
    assert!(!pending.exists());
}

#[test]
fn uncontended_sync_has_no_configured_coalesce_delay() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join(".ravel.toml"),
        "[sync]\ncoalesce_ms = 500\n",
    )
    .unwrap();
    seed(dir.path());
    let engine = engine(dir.path());
    engine.index().unwrap();
    write(dir.path(), "src/fast.ts", "export const fast = true;\n");
    let started = std::time::Instant::now();
    engine
        .sync(Some(&[dir.path().join("src/fast.ts")]))
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_millis(400));
}

#[test]
fn invalid_ticket_is_quarantined_without_poisoning_valid_sync() {
    let dir = tempdir().unwrap();
    seed(dir.path());
    let engine = engine(dir.path());
    engine.index().unwrap();
    let queue = dir.path().join(".ravel/pending-sync");
    fs::create_dir_all(&queue).unwrap();
    fs::write(
        queue.join("poison.json"),
        serde_json::to_vec(&vec![PathBuf::from("../outside.ts")]).unwrap(),
    )
    .unwrap();
    write(dir.path(), "src/valid.ts", "export const valid = true;\n");
    engine
        .sync(Some(&[dir.path().join("src/valid.ts")]))
        .unwrap();
    assert!(
        engine
            .snapshot()
            .unwrap()
            .files
            .contains_key("src/valid.ts")
    );
    assert!(queue.join("poison.invalid").exists());
}

#[test]
fn eight_independent_agents_converge_without_lost_paths() {
    let dir = tempdir().unwrap();
    seed(dir.path());
    engine(dir.path()).index().unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
    let mut handles = Vec::new();
    for agent in 0..8 {
        let path = dir.path().join(format!("src/agent-{agent}.ts"));
        fs::write(&path, format!("export const agent{agent} = {agent};\n")).unwrap();
        let root = dir.path().to_path_buf();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            let engine = engine(&root);
            barrier.wait();
            engine.sync(Some(&[path])).unwrap();
        }));
    }
    barrier.wait();
    for handle in handles {
        handle.join().unwrap();
    }
    let snapshot = engine(dir.path()).snapshot().unwrap();
    for agent in 0..8 {
        assert!(
            snapshot
                .files
                .contains_key(&format!("src/agent-{agent}.ts"))
        );
    }
}

#[test]
fn repeated_content_only_syncs_keep_matching_a_full_index() {
    let inc = tempdir().unwrap();
    let full = tempdir().unwrap();
    seed(inc.path());
    seed(full.path());
    let incremental_engine = engine(inc.path());
    incremental_engine.index().unwrap();

    for suffix in ["// first\n", "// second\n", "// third\n"] {
        let inc_path = inc.path().join("src/a.ts");
        let full_path = full.path().join("src/a.ts");
        let mut inc_source = std::fs::read_to_string(&inc_path).unwrap();
        let mut full_source = std::fs::read_to_string(&full_path).unwrap();
        inc_source.push_str(suffix);
        full_source.push_str(suffix);
        std::fs::write(&inc_path, inc_source).unwrap();
        std::fs::write(&full_path, full_source).unwrap();
        incremental_engine.sync(Some(&[inc_path])).unwrap();
    }

    let incremental = incremental_engine.snapshot().unwrap();
    let full_engine = engine(full.path());
    full_engine.index().unwrap();
    let rebuilt = full_engine.snapshot().unwrap();
    assert_eq!(incremental.files, rebuilt.files);
    assert_eq!(incremental.edges, rebuilt.edges);
}

#[test]
fn artifact_delta_exposes_current_symbol_complexity_to_context() {
    let dir = tempdir().unwrap();
    write(
        dir.path(),
        "src/service.ts",
        "export function calculate(value: number) { return value; }\n",
    );
    let engine = engine(dir.path());
    engine.index().unwrap();
    write(
        dir.path(),
        "src/service.ts",
        "export function calculate(value: number) { if (value > 0) return value; return 0; }\n",
    );
    engine
        .sync(Some(&[dir.path().join("src/service.ts")]))
        .unwrap();

    let detail = engine.node_detail("calculate").unwrap().unwrap();
    assert_eq!(detail.complexity.unwrap().cyclomatic, 2);
}
