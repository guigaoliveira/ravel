//! A generation key is a digest of the tree's content, so undoing an edit brings an earlier key
//! back. Everything a publish writes has to stay distinct from what the current generation still
//! references even then: these replay edit/undo sequences through `sync` and compare what a fresh
//! process reads back against a full `index` of the same final tree.

use ravel_core::{config::Flags, engine::WorkspaceEngine};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

const FILES: usize = 6;

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn engine(root: &Path) -> WorkspaceEngine {
    WorkspaceEngine::load(root, &Flags::default()).unwrap()
}

fn original(index: usize) -> String {
    format!("export function f{index}() {{ return {index}; }}\n")
}

/// Calls into the previous file: changes the file's imports and its outgoing relations.
fn structural(index: usize) -> String {
    let callee = (index + FILES - 1) % FILES;
    format!(
        "import {{ f{callee} }} from './f{callee}';\nexport function f{index}() {{ return f{callee}(); }}\n"
    )
}

/// Same declarations, imports and references: only the bytes move.
fn comment_only(index: usize) -> String {
    format!("{}// edited\n", original(index))
}

fn seed(root: &Path) {
    for index in 0..FILES {
        write(root, &format!("src/f{index}.ts"), &original(index));
    }
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

/// Replays `steps` (file index, new contents) through one `sync` each, then checks the published
/// state against a full index of the final tree.
fn assert_steps_match_full_index(
    scenario: &str,
    steps: &[(usize, String)],
    sync: impl Fn(&WorkspaceEngine, &[PathBuf]),
) {
    let incremental = tempdir().unwrap();
    seed(incremental.path());
    let writer = engine(incremental.path());
    writer.index().unwrap();
    for (index, contents) in steps {
        let rel = format!("src/f{index}.ts");
        write(incremental.path(), &rel, contents);
        sync(&writer, &[incremental.path().join(&rel)]);
    }

    let full = tempdir().unwrap();
    seed(full.path());
    for (index, contents) in steps {
        write(full.path(), &format!("src/f{index}.ts"), contents);
    }
    engine(full.path()).index().unwrap();

    assert_eq!(
        fingerprint(incremental.path()),
        fingerprint(full.path()),
        "sync != full index for scenario: {scenario}"
    );
}

#[test]
fn undoing_a_structural_edit_keeps_the_earlier_overlays() {
    let mut steps: Vec<_> = (0..3).map(|index| (index, structural(index))).collect();
    steps.push((2, original(2)));
    assert_steps_match_full_index("structural edits, then undo the last", &steps, |e, p| {
        e.sync(Some(p)).unwrap();
    });
}

#[test]
fn undoing_a_resident_structural_edit_keeps_the_earlier_overlays() {
    let mut steps: Vec<_> = (0..3).map(|index| (index, structural(index))).collect();
    steps.push((2, original(2)));
    assert_steps_match_full_index(
        "resident structural edits, then undo the last",
        &steps,
        |e, p| {
            e.sync_resident(Some(p)).unwrap();
        },
    );
}

#[test]
fn undoing_a_content_only_edit_keeps_the_earlier_deltas() {
    let mut steps: Vec<_> = (1..FILES)
        .map(|index| (index, comment_only(index)))
        .collect();
    steps.push((FILES - 1, original(FILES - 1)));
    assert_steps_match_full_index("comment-only edits, then undo the last", &steps, |e, p| {
        e.sync(Some(p)).unwrap();
    });
}

/// Every prefix of deterministic edit/undo walks, so a generation key recurs at many different
/// chain shapes rather than just the ones the scenarios above reach.
#[test]
fn edit_undo_walks_match_a_full_index_at_every_step() {
    let seeds = std::env::var("RAVEL_RECURRENCE_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2u64);
    for seed in 1..=seeds {
        edit_undo_walk(seed, 24);
    }
}

fn edit_undo_walk(walk: u64, steps: usize) {
    let mut state: u64 = walk.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let incremental = tempdir().unwrap();
    seed(incremental.path());
    let writer = engine(incremental.path());
    writer.index().unwrap();
    let mut tree: Vec<String> = (0..FILES).map(original).collect();
    let mut history: Vec<(usize, String)> = Vec::new();
    for step in 0..steps {
        let undo = !history.is_empty() && next() % 3 == 0;
        let (index, contents) = if undo {
            history.pop().unwrap()
        } else {
            let index = (next() % FILES as u64) as usize;
            let contents = match next() % 3 {
                0 => original(index),
                1 => structural(index),
                _ => comment_only(index),
            };
            (index, contents)
        };
        let previous = std::mem::replace(&mut tree[index], contents.clone());
        if !undo {
            history.push((index, previous));
        }
        let rel = format!("src/f{index}.ts");
        write(incremental.path(), &rel, &contents);
        let changed = [incremental.path().join(&rel)];
        if next() % 2 == 0 {
            writer.sync(Some(&changed)).unwrap();
        } else {
            writer.sync_resident(Some(&changed)).unwrap();
        }

        let full = tempdir().unwrap();
        for (index, contents) in tree.iter().enumerate() {
            write(full.path(), &format!("src/f{index}.ts"), contents);
        }
        engine(full.path()).index().unwrap();
        assert_eq!(
            fingerprint(incremental.path()),
            fingerprint(full.path()),
            "sync != full index after step {step} of walk {walk}"
        );
    }
}
