//! Incremental sync must publish what a full index of the same tree would, whatever the edit.
//! A file's change can re-point resolution in files nobody touched: through a class member, a
//! name an export clause publishes, or a barrel that forwards it. This walks random edits over
//! shapes that exercise those paths -- syncing each, alternately cold and resident -- and compares
//! what a fresh process reads back with a full index after every step.
//!
//! `RAVEL_WALK_SEEDS` and `RAVEL_WALK_STEPS` lengthen the walk.

use ravel_core::{config::Flags, engine::WorkspaceEngine};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

const FILES: usize = 6;
const SHAPES: u64 = 15;

fn engine(root: &Path) -> WorkspaceEngine {
    WorkspaceEngine::load(root, &Flags::default()).unwrap()
}

/// What a fresh process reads back: the snapshot, the stats, and every node's direct relations
/// (the data `callers-of` and `calls-from` answer from).
fn fingerprint(root: &Path) -> Vec<String> {
    let engine = engine(root);
    let snapshot = engine.snapshot().expect("snapshot");
    let mut out = vec![
        format!(
            "files={}",
            serde_json::to_string(&snapshot.files).expect("files")
        ),
        format!(
            "edges={}",
            serde_json::to_string(&snapshot.edges).expect("edges")
        ),
    ];
    let stats = engine.stats().expect("stats");
    out.push(format!(
        "stats={} {} {} {}",
        stats.files, stats.edges, stats.bytes, stats.parse_errors
    ));
    let graph = engine.graph().expect("graph");
    let mut names: Vec<String> = graph.node_names().map(str::to_owned).collect();
    names.sort();
    for name in &names {
        for (direction, reverse) in [("out", false), ("in", true)] {
            let mut relations = graph
                .direct_relations_limit(name, reverse, usize::MAX)
                .0
                .into_iter()
                .map(|relation| serde_json::to_string(&relation).unwrap())
                .collect::<Vec<_>>();
            relations.sort();
            out.push(format!("{direction} {name}={}", relations.join("|")));
        }
    }
    out
}

/// File `i` in one of the shapes, built over its neighbours `p` and `q`; `None` is deleted.
fn shape(i: usize, shape: u64) -> Option<String> {
    let p = (i + FILES - 1) % FILES;
    let q = (i + FILES - 2) % FILES;
    let own = format!("export function f{i}() {{ return {i}; }}\n");
    Some(match shape {
        0 => own,
        1 => format!(
            "import {{ f{p} }} from './f{p}';\nexport function f{i}() {{ return f{p}(); }}\n"
        ),
        // Barrels: a named re-export, a star, a re-export of a re-export, an import exported again.
        2 => format!("export {{ f{p} }} from './f{p}';\n{own}"),
        3 => format!("export * from './f{p}';\n{own}"),
        4 => format!("export {{ f{q} }} from './f{p}';\n{own}"),
        5 => format!("import {{ f{p} }} from './f{p}';\nexport {{ f{p} }};\n{own}"),
        // A name bound through a barrel, used and unused.
        6 => format!(
            "import {{ f{q} }} from './f{p}';\nexport function f{i}() {{ return f{q}(); }}\n"
        ),
        7 => format!("import {{ f{q} }} from './f{p}';\n{own}"),
        // A class whose members change while it stays, and a user of one of them.
        8 => format!("export class C{i} {{ static make() {{ return {i}; }} }}\n{own}"),
        9 => format!("export class C{i} {{ static other() {{ return {i}; }} }}\n{own}"),
        10 => format!(
            "import {{ C{p} }} from './f{p}';\nexport function f{i}() {{ return C{p}.make(); }}\n"
        ),
        // A declaration only an export clause publishes, and one a namespace hides.
        11 => format!("function f{i}() {{ return {i}; }}\nexport {{ f{i} }};\n"),
        12 => format!("export namespace N{i} {{ export function f{p}() {{ return 0; }} }}\n{own}"),
        13 => format!(
            "import * as ns from './f{p}';\nexport function f{i}() {{ return ns.f{p}(); }}\n"
        ),
        _ => return None,
    })
}

fn put(root: &Path, i: usize, contents: Option<&str>) {
    let path = root.join(format!("src/f{i}.ts"));
    match contents {
        Some(text) => {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        None => {
            let _ = fs::remove_file(path);
        }
    }
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[test]
fn random_edits_match_a_full_index_at_every_step() {
    for walk in 1..=env("RAVEL_WALK_SEEDS", 6) {
        random_walk(walk, env("RAVEL_WALK_STEPS", 20) as usize);
    }
}

fn random_walk(walk: u64, steps: usize) {
    // xorshift: deterministic per seed, no dependency.
    let mut state = walk.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let incremental = tempdir().unwrap();
    let mut tree: Vec<Option<String>> = (0..FILES).map(|i| shape(i, 0)).collect();
    for (i, contents) in tree.iter().enumerate() {
        put(incremental.path(), i, contents.as_deref());
    }
    let writer = engine(incremental.path());
    writer.index().unwrap();
    let mut history = Vec::new();
    for _ in 0..steps {
        let i = (next() % FILES as u64) as usize;
        let chosen = next() % SHAPES;
        let contents = shape(i, chosen);
        // Deleting what is already gone is not an edit, and `sync` rightly refuses it.
        if contents.is_none() && tree[i].is_none() {
            continue;
        }
        put(incremental.path(), i, contents.as_deref());
        tree[i] = contents;
        history.push(format!("f{i}:{chosen}"));
        let changed = [incremental.path().join(format!("src/f{i}.ts"))];
        if next() % 2 == 0 {
            writer.sync_resident(Some(&changed)).unwrap();
        } else {
            writer.sync(Some(&changed)).unwrap();
        }

        let full = tempdir().unwrap();
        for (i, contents) in tree.iter().enumerate() {
            put(full.path(), i, contents.as_deref());
        }
        engine(full.path()).index().unwrap();
        assert_eq!(
            fingerprint(incremental.path()),
            fingerprint(full.path()),
            "walk {walk} diverged after {history:?}"
        );
    }
}
