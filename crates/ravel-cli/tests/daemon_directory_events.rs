//! A directory that appears, moves or leaves is reported by the file watcher as one event for the
//! directory, never for the files inside it. Outside git nothing else tells the index about them:
//! the daemon's watcher has to find those files itself.
//!
//! Driven through the binary: every query is its own process, answered by one long-lived daemon.

use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn ravel(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ravel"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("ravel must run")
}

/// Where `context SYMBOL` finds a definition with exactly that name, if anywhere.
fn defined_at(root: &Path, symbol: &str) -> Option<String> {
    let output = ravel(root, &["context", symbol]);
    assert!(
        output.status.success(),
        "context {symbol} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answer: Value = serde_json::from_slice(&output.stdout).expect("context prints JSON");
    answer["candidates"].as_array().and_then(|found| {
        found
            .iter()
            .find(|candidate| candidate["name"] == symbol)
            .and_then(|candidate| candidate["path"].as_str())
            .map(str::to_owned)
    })
}

/// The answer to `defined_at` becomes `expected` within a few seconds.
fn settles(root: &Path, symbol: &str, expected: Option<&str>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let found = defined_at(root, symbol);
        if found.as_deref() == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{symbol} is at {found:?}, expected {expected:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Daemon {
    child: Child,
    root: PathBuf,
}

impl Daemon {
    fn start(root: &Path) -> Daemon {
        let child = Command::new(env!("CARGO_BIN_EXE_ravel"))
            .arg("--root")
            .arg(root)
            .arg("daemon-serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Daemon {
            child,
            root: root.to_path_buf(),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = ravel(root, &["daemon", "status"]);
            if serde_json::from_slice::<Value>(&status.stdout)
                .is_ok_and(|status| status["running"] == true)
            {
                return daemon;
            }
            assert!(Instant::now() < deadline, "the daemon never became ready");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = ravel(&self.root, &["daemon", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn files_inside_a_directory_that_moves_reach_the_index_without_git() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    write(&root.join("src/base.ts"), "export const base = 1;\n");
    write(
        &root.join("src/feat/a.ts"),
        "export function featAlpha() { return 1; }\n",
    );
    write(
        &root.join("src/feat/deep/b.ts"),
        "export function featBeta() { return 2; }\n",
    );
    assert!(ravel(&root, &["index"]).status.success());
    assert!(
        !root.join(".git").exists(),
        "the point is a root git cannot speak for"
    );
    // Beside the workspace on the same filesystem, so a rename can move a directory in or out.
    let elsewhere = tempfile::tempdir_in(root.parent().unwrap()).unwrap();

    let _daemon = Daemon::start(&root);
    // An edit is only seen once the watcher is armed; wait for it to see one.
    write(
        &root.join("src/probe.ts"),
        "export const watcherArmed = 1;\n",
    );
    settles(&root, "watcherArmed", Some("src/probe.ts"));

    // Renamed inside the tree.
    fs::rename(root.join("src/feat"), root.join("src/feat2")).unwrap();
    settles(&root, "featAlpha", Some("src/feat2/a.ts"));
    settles(&root, "featBeta", Some("src/feat2/deep/b.ts"));

    // Moved in from elsewhere.
    write(
        &elsewhere.path().join("pkg/lib/c.ts"),
        "export function movedIn() { return 3; }\n",
    );
    fs::rename(elsewhere.path().join("pkg"), root.join("src/pkg")).unwrap();
    settles(&root, "movedIn", Some("src/pkg/lib/c.ts"));

    // Moved out (what deleting a folder to the trash does).
    fs::rename(root.join("src/feat2"), elsewhere.path().join("gone")).unwrap();
    settles(&root, "featAlpha", None);
    settles(&root, "featBeta", None);

    // Made with a file in it at once, before the backend can have installed its watch.
    write(
        &root.join("src/fresh/deeper/still/d.ts"),
        "export function freshDeep() { return 4; }\n",
    );
    settles(&root, "freshDeep", Some("src/fresh/deeper/still/d.ts"));

    // Everything else is still where it was.
    settles(&root, "base", Some("src/base.ts"));
}
