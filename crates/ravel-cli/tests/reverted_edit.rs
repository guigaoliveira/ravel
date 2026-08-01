//! A reverted edit must stop being answered from its uncommitted content.
//!
//! Auto-sync asks git what changed, which is a valid staleness oracle only while the index matches
//! HEAD. Once a sync publishes a generation built from uncommitted content, a clean tree no longer
//! implies "index equals tree" -- so the ordinary agent loop (edit, ask, `git checkout --`) left the
//! phantom edit in the index permanently, with `parse_errors: 0`, `last_update_error: null` and `ci`
//! green.
//!
//! Exercised through the binary rather than the library: each step must be a separate process, the
//! way a real session runs. An in-process version of this test passed with the fix disabled, because
//! the caches of the instance that performed the sync mask exactly what is under test.

use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("git must be available");
    assert!(status.success(), "git {args:?} failed");
}

fn calls_to_fmt(root: &Path) -> u64 {
    let output = Command::new(env!("CARGO_BIN_EXE_ravel"))
        .args(["callers-of", "fmt", "--root"])
        .arg(root)
        .output()
        .expect("callers-of must run");
    let text = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(text.trim()).unwrap_or_else(|_| panic!("not json: {text}"));
    parsed
        .get("by_kind")
        .and_then(|kinds| kinds.get("Calls"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

#[test]
fn a_reverted_edit_is_not_answered_from_its_uncommitted_content() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let root = workspace.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/money.ts"),
        "export function fmt(v) { return String(v); }\n",
    )
    .unwrap();
    let checkout = root.join("src/checkout.ts");
    std::fs::write(
        &checkout,
        "import { fmt } from './money';\nexport function checkout(v) { return fmt(v); }\n",
    )
    .unwrap();
    git(root, &["init", "-q", "."]);
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "seed",
        ],
    );

    let index = Command::new(env!("CARGO_BIN_EXE_ravel"))
        .args(["index", "--root"])
        .arg(root)
        .output()
        .expect("index must run");
    assert!(index.status.success(), "index failed");
    assert_eq!(calls_to_fmt(root), 1, "the call is there to begin with");

    // Edit the call away, then ask a question: auto-sync absorbs the uncommitted content.
    std::fs::write(
        &checkout,
        "import { fmt } from './money';\nexport function checkout(v) { return String(v); }\n",
    )
    .unwrap();
    assert_eq!(calls_to_fmt(root), 0, "the edit is reflected");

    // Revert. The working tree matches HEAD again, so the answer must too.
    git(root, &["checkout", "--", "src/checkout.ts"]);
    assert_eq!(
        calls_to_fmt(root),
        1,
        "a reverted file must stop being answered from the content the index absorbed"
    );
}
