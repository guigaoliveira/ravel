//! Cross-process proof for the common multi-agent shape: each MCP client starts its own
//! Ravel process, while all of them publish into the same workspace index.

use ravel_core::{config::Flags, engine::WorkspaceEngine};
use std::{fs, path::Path, process::Command};
use tempfile::tempdir;

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn separate_processes_do_not_lose_concurrent_syncs() {
    let dir = tempdir().unwrap();
    write(dir.path(), "src/base.ts", "export const base = 1;\n");
    let binary = env!("CARGO_BIN_EXE_ravel");
    let root = dir.path().to_str().unwrap();
    assert!(
        Command::new(binary)
            .args(["--root", root, "index"])
            .status()
            .unwrap()
            .success()
    );

    write(
        dir.path(),
        "src/agent-a.ts",
        "export function fromAgentA() { return 1; }\n",
    );
    write(
        dir.path(),
        "src/agent-b.ts",
        "export function fromAgentB() { return 2; }\n",
    );

    let mut first = Command::new(binary)
        .args(["--root", root, "sync", "src/agent-a.ts"])
        .spawn()
        .unwrap();
    let mut second = Command::new(binary)
        .args(["--root", root, "sync", "src/agent-b.ts"])
        .spawn()
        .unwrap();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());

    let engine = WorkspaceEngine::load(dir.path(), &Flags::default()).unwrap();
    let snapshot = engine.snapshot().unwrap();
    assert!(snapshot.files.contains_key("src/agent-a.ts"));
    assert!(snapshot.files.contains_key("src/agent-b.ts"));
}
