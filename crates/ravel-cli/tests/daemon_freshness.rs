//! A daemon whose file watcher has seen nothing change lets a query skip asking git about the whole
//! worktree. The skip must never cost a fresh answer: every edit has to reach the next answers, and
//! stay there, however the quiet stretches before and after it fall.
//!
//! Driven through the binary the way a session runs: every call is its own process, talking to one
//! long-lived daemon whose watcher is armed.

use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(root)
        .status()
        .expect("git must be available");
    assert!(status.success(), "git {args:?} failed");
}

fn ravel(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ravel"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("ravel must run")
}

/// Whether `context SYMBOL` finds a definition with exactly that name.
fn defined(root: &Path, symbol: &str) -> bool {
    let output = ravel(root, &["context", symbol]);
    assert!(
        output.status.success(),
        "context {symbol} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answer: Value = serde_json::from_slice(&output.stdout).expect("context prints JSON");
    answer["candidates"]
        .as_array()
        .is_some_and(|found| found.iter().any(|candidate| candidate["name"] == symbol))
}

/// The answer to `defined` becomes `expected` within a few seconds and then stays that way.
///
/// The daemon remembers git's answer for a few milliseconds, so a query made at the wrong moment
/// can be handed one from just before the edit; that window closes by itself. What must not happen
/// is the watcher's verdict keeping the old answer alive, and that does not close for a minute.
fn settles(root: &Path, symbol: &str, expected: bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while defined(root, symbol) != expected {
        assert!(
            Instant::now() < deadline,
            "{symbol} is still {} after the edit",
            if expected { "missing" } else { "present" }
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    for _ in 0..2 {
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(
            defined(root, symbol),
            expected,
            "{symbol} changed back after it settled"
        );
    }
}

struct Daemon {
    child: Child,
    root: PathBuf,
}

impl Daemon {
    fn start(root: &Path, log: &Path) -> Daemon {
        let child = Command::new(env!("CARGO_BIN_EXE_ravel"))
            .arg("--root")
            .arg(root)
            .arg("daemon-serve")
            .env("RAVEL_TIMING", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(log).unwrap())
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

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn append(root: &Path, relative: &str, text: &str) {
    use std::io::Write as _;
    fs::OpenOptions::new()
        .append(true)
        .open(root.join(relative))
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

#[test]
fn every_edit_is_answered_whether_or_not_the_watcher_was_asked() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    write(&root, ".gitignore", "gen/\n");
    write(&root, "src/a.ts", "export function alpha() { return 1; }\n");
    write(&root, "src/b.ts", "export function beta() { return 2; }\n");
    write(
        &root,
        "gen/g.ts",
        "export function generatedOne() { return 3; }\n",
    );
    git(&root, &["init", "-q", "."]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "seed"]);
    assert!(ravel(&root, &["index"]).status.success());
    assert!(
        !defined(&root, "generatedOne"),
        "ignored files are not indexed"
    );

    // Whether the watcher may vouch for this machine's filesystem at all.
    let trusted = ravel_core::watch::WatchGate::new(&root, &root.join(".ravel")).is_trusted();

    // Beside the workspace, not in it: a file that grows with every query is a change to the tree.
    let elsewhere = tempfile::tempdir().unwrap();
    let log = elsewhere.path().join("daemon.log");
    let _daemon = Daemon::start(&root, &log);
    if trusted {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("autosync.quiet")
        {
            assert!(
                Instant::now() < deadline,
                "the watcher never let a query skip the dirty check"
            );
            assert!(defined(&root, "alpha"));
            std::thread::sleep(Duration::from_millis(80));
        }
    }

    // Edits to a file the watcher already knows, each followed by quiet queries.
    for round in 0..10 {
        let name = format!("added{round}");
        append(
            &root,
            "src/a.ts",
            &format!("export function {name}() {{ return {round}; }}\n"),
        );
        settles(&root, &name, true);
    }

    // The rules that decide what is indexed change underneath the watcher's own filter, which
    // still ignores `gen/`. This comes before anything that creates entries, because those keep
    // the watcher from vouching for the tree for a while and would hide a stale verdict here.
    write(&root, ".gitignore", "");
    settles(&root, "generatedOne", true);
    append(&root, "gen/g.ts", "export function generatedTwo() {}\n");
    settles(&root, "generatedTwo", true);

    // A file appears, moves, and goes away.
    write(&root, "src/new1.ts", "export function created1() {}\n");
    settles(&root, "created1", true);
    fs::rename(root.join("src/new1.ts"), root.join("src/new1_moved.ts")).unwrap();
    settles(&root, "created1", true);
    fs::remove_file(root.join("src/b.ts")).unwrap();
    settles(&root, "beta", false);

    // A directory appears with a file in it, and then a second file joins that file.
    write(
        &root,
        "src/deep/er/n2.ts",
        "export function created2() {}\n",
    );
    settles(&root, "created2", true);
    write(
        &root,
        "src/deep/er/n3.ts",
        "export function created3() {}\n",
    );
    settles(&root, "created3", true);

    // An edit that is taken back by hand is taken back.
    append(&root, "src/a.ts", "export function transient() {}\n");
    settles(&root, "transient", true);
    let restored = fs::read_to_string(root.join("src/a.ts"))
        .unwrap()
        .replace("export function transient() {}\n", "");
    fs::write(root.join("src/a.ts"), restored).unwrap();
    settles(&root, "transient", false);

    let timings = fs::read_to_string(&log).unwrap();
    assert!(
        timings.contains("autosync.discover_dirty"),
        "edits are found by asking git, so it was asked"
    );
    if trusted {
        assert!(
            timings.contains("autosync.quiet"),
            "the watcher answered at least one check"
        );
    }
}
