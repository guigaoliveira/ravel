//! Optional git helpers. Index/query never require git.
//! Dirty discovery must stay **fast** (tracked changes only by default).

use crate::config::SiblingEmitRule;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("not a Git worktree at {0}")]
    NotWorktree(PathBuf),
    #[error("Git operation failed: {0}")]
    Operation(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct WorktreeIdentity {
    pub root: PathBuf,
    pub worktree: String,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMetadataFingerprint(blake3::Hash);

/// Cheap identity invalidation key. It reads only the small Git control files that can change
/// HEAD identity; it never spawns Git or walks the worktree.
pub fn metadata_fingerprint(root: &Path) -> GitMetadataFingerprint {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"ravel-git-metadata-v1\0");
    let Some(dot_git) = git_marker(root) else {
        hasher.update(b"nogit");
        return GitMetadataFingerprint(hasher.finalize());
    };
    let git_dir = if dot_git.is_dir() {
        Some(dot_git)
    } else {
        std::fs::read_to_string(&dot_git).ok().and_then(|value| {
            value
                .trim()
                .strip_prefix("gitdir:")
                .map(str::trim)
                .map(PathBuf::from)
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    }
                })
        })
    };
    let Some(git_dir) = git_dir else {
        hasher.update(b"nogit");
        return GitMetadataFingerprint(hasher.finalize());
    };
    hash_control_file(&mut hasher, &git_dir.join("HEAD"));
    let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|value| git_dir.join(value.trim()))
        .unwrap_or_else(|| git_dir.clone());
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD"))
        && let Some(reference) = head.trim().strip_prefix("ref: ")
    {
        let local_ref = git_dir.join(reference);
        if local_ref.is_file() {
            hash_control_file(&mut hasher, &local_ref);
        } else {
            hash_control_file(&mut hasher, &common_dir.join(reference));
        }
    }
    hash_control_file(&mut hasher, &common_dir.join("packed-refs"));
    GitMetadataFingerprint(hasher.finalize())
}

fn hash_control_file(hasher: &mut blake3::Hasher, path: &Path) {
    hasher.update(path.to_string_lossy().as_bytes());
    match std::fs::read(path) {
        Ok(bytes) => {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
        Err(_) => {
            hasher.update(&u64::MAX.to_le_bytes());
        }
    }
}

pub fn identify_worktree(root: &Path) -> Result<WorktreeIdentity, GitError> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &root.to_string_lossy(),
            "rev-parse",
            "--verify",
            "HEAD",
        ])
        .output()
        .map_err(|error| GitError::Operation(error.to_string()))?;
    let revision = if output.status.success() {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    } else {
        // Distinguish an unborn repository from an arbitrary non-Git directory.
        let probe = std::process::Command::new("git")
            .args([
                "-C",
                &root.to_string_lossy(),
                "rev-parse",
                "--is-inside-work-tree",
            ])
            .output()
            .map_err(|error| GitError::Operation(error.to_string()))?;
        if !probe.status.success() || String::from_utf8_lossy(&probe.stdout).trim() != "true" {
            return Err(GitError::NotWorktree(root.to_path_buf()));
        }
        "unborn".into()
    };
    let worktree = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned();
    Ok(WorktreeIdentity {
        root: root.to_path_buf(),
        worktree,
        revision,
    })
}

/// Cheap probe — **no process spawn**, no libgit walk. False for non-git trees.
pub fn is_git_repo(root: &Path) -> bool {
    git_marker(root).is_some()
}

fn git_marker(root: &Path) -> Option<PathBuf> {
    root.ancestors()
        .map(|ancestor| ancestor.join(".git"))
        .find(|path| path.exists())
}

/// Snapshot identity that never fails on non-git trees.
pub fn worktree_identity_or_nogit(root: &Path) -> WorktreeIdentity {
    identify_worktree(root).unwrap_or_else(|_| {
        let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        WorktreeIdentity {
            root: root.to_path_buf(),
            worktree: canon.to_string_lossy().into_owned(),
            revision: "nogit".into(),
        }
    })
}

/// Options for dirty-path discovery (from `[sync]` config).
#[derive(Debug, Clone)]
pub struct DirtyDiscovery {
    /// Include untracked files (`??`). **Default false** — untracked scans dominate latency
    /// on TypeScript projects with tsc emit / build leftovers.
    pub include_untracked: bool,
    pub skip_sibling_emit: bool,
    pub sibling_emit: Vec<SiblingEmitRule>,
}

impl Default for DirtyDiscovery {
    fn default() -> Self {
        Self {
            include_untracked: false,
            skip_sibling_emit: true,
            sibling_emit: crate::config::default_sibling_emit_rules(),
        }
    }
}

/// Working-tree dirty paths for incremental sync.
///
/// **Default (fast):** tracked changes only (`git status --untracked-files=no`).
/// Untracked is opt-in — it is correct for brand-new files but expensive and noisy.
pub fn changed_paths(root: &Path) -> Result<Vec<PathBuf>, GitError> {
    changed_paths_with(root, &DirtyDiscovery::default())
}

pub fn changed_paths_with(
    root: &Path,
    discovery: &DirtyDiscovery,
) -> Result<Vec<PathBuf>, GitError> {
    // Fail fast without spawning when not a repo.
    if !is_git_repo(root) {
        return Err(GitError::NotWorktree(root.to_path_buf()));
    }

    let mut args = vec![
        "-C".to_owned(),
        root.to_string_lossy().into_owned(),
        "status".into(),
        "--porcelain=v1".into(),
        "-z".into(),
        "--no-renames".into(),
    ];
    // Critical perf switch: never list thousands of untracked emit files by default.
    if discovery.include_untracked {
        args.push("-u".into());
    } else {
        args.push("--untracked-files=no".into());
    }

    let output = std::process::Command::new("git")
        .args(&args)
        .output()
        .map_err(|error| GitError::Operation(error.to_string()))?;
    if !output.status.success() {
        // Fallback: tracked-only diffs (still no untracked).
        return dirty_tracked_diff(root);
    }

    let mut paths = Vec::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.len() < 4 {
            continue;
        }
        let xy = &record[..2];
        let path_part = &record[3..];
        if path_part.is_empty() {
            continue;
        }
        let abs = root.join(git_path(path_part));
        let untracked = xy == b"??";
        if untracked {
            if !discovery.include_untracked {
                continue;
            }
            if discovery.skip_sibling_emit && is_sibling_emit(&abs, &discovery.sibling_emit) {
                continue;
            }
            let name = abs.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.ends_with(".map") || name.ends_with(".d.ts") {
                continue;
            }
        }
        paths.push(abs);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Tracked-only dirty list via `git diff` (no porcelain, no untracked).
fn dirty_tracked_diff(root: &Path) -> Result<Vec<PathBuf>, GitError> {
    let mut paths = Vec::new();
    let root_s = root.to_string_lossy();
    // `git diff HEAD` is working-tree-vs-HEAD → already includes both staged and unstaged
    // changes, so the separate `--cached` spawn was redundant. One process, not two.
    let output = std::process::Command::new("git")
        .args(["-C", root_s.as_ref(), "diff", "--name-only", "-z", "HEAD"])
        .output()
        .map_err(|e| GitError::Operation(e.to_string()))?;
    if output.status.success() {
        for path in output.stdout.split(|byte| *byte == 0) {
            if !path.is_empty() {
                paths.push(root.join(git_path(path)));
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Diff file list between refs (for `diff-impact`).
pub fn changed_paths_between(
    root: &Path,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Vec<PathBuf>, GitError> {
    let mut args = vec![
        "-C".to_owned(),
        root.to_string_lossy().into_owned(),
        "diff".into(),
        "--name-only".into(),
        "-z".into(),
        "--diff-filter=ACMRTUXB".into(),
    ];
    if let Some(from) = from {
        if let Some(to) = to {
            args.push(format!("{from}...{to}"));
        } else {
            args.push(format!("{from}...HEAD"));
        }
    }
    let output = std::process::Command::new("git")
        .args(&args)
        .output()
        .map_err(|error| GitError::Operation(error.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Operation(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let mut paths: Vec<_> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| root.join(git_path(path)))
        .collect();
    paths.sort();
    Ok(paths)
}

#[cfg(unix)]
fn git_path(bytes: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes.to_vec())
}

#[cfg(not(unix))]
fn git_path(bytes: &[u8]) -> std::ffi::OsString {
    std::ffi::OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Files that co-changed with `file` in the last `commits` commits.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CoChangeEntry {
    pub file: String,
    pub cooccurrence_count: u32,
}

pub fn cochanged(
    root: &Path,
    file: &str,
    commits: usize,
    min_cooccurrence: u32,
) -> Result<Vec<CoChangeEntry>, GitError> {
    let commits = commits.clamp(1, 5_000);
    // First select commits that touched `file`. A pathspec on the later
    // `--name-only` command would hide every co-changed path and always return
    // an empty result.
    let revisions = std::process::Command::new("git")
        .args([
            "-C",
            &root.to_string_lossy(),
            "log",
            &format!("--max-count={commits}"),
            "--format=%H",
            "--",
            file,
        ])
        .output()
        .map_err(|error| GitError::Operation(error.to_string()))?;
    if !revisions.status.success() {
        return Err(GitError::Operation(
            String::from_utf8_lossy(&revisions.stderr).trim().to_owned(),
        ));
    }
    if revisions.stdout.is_empty() {
        return Ok(Vec::new());
    }
    use std::io::Write;
    use std::process::Stdio;
    let mut child = std::process::Command::new("git")
        .args([
            "-C",
            &root.to_string_lossy(),
            "show",
            "--stdin",
            "--format=format:--",
            "--name-only",
            "--no-renames",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| GitError::Operation(error.to_string()))?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(&revisions.stdout)
        .map_err(|error| GitError::Operation(error.to_string()))?;
    let output = child
        .wait_with_output()
        .map_err(|error| GitError::Operation(error.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Operation(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    use std::collections::HashMap;
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut in_commit = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line == "--" {
            in_commit = true;
            continue;
        }
        if !in_commit || line.is_empty() {
            continue;
        }
        if line == file {
            continue;
        }
        *counts.entry(line.to_owned()).or_default() += 1;
    }
    let mut entries: Vec<_> = counts
        .into_iter()
        .filter(|(_, c)| *c >= min_cooccurrence)
        .map(|(file, cooccurrence_count)| CoChangeEntry {
            file,
            cooccurrence_count,
        })
        .collect();
    entries.sort_by(|a, b| {
        b.cooccurrence_count
            .cmp(&a.cooccurrence_count)
            .then_with(|| a.file.cmp(&b.file))
    });
    Ok(entries)
}

/// Configurable sibling-emit: untracked `stem.emit` skipped if `stem.source` exists.
pub fn is_sibling_emit(path: &Path, rules: &[SiblingEmitRule]) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for rule in rules {
        let suffix = format!(".{}", rule.emit.trim_start_matches('.'));
        let Some(stem_name) = name.strip_suffix(&suffix) else {
            continue;
        };
        for src in &rule.sources {
            let src = src.trim_start_matches('.');
            if parent.join(format!("{stem_name}.{src}")).is_file() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod artifact_tests {
    use super::*;
    use crate::config::default_sibling_emit_rules;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn multi_dot_js_next_to_ts_is_artifact() {
        let dir = tempdir().unwrap();
        let ts = dir.path().join("get_x.usecase.ts");
        let js = dir.path().join("get_x.usecase.js");
        fs::write(&ts, "export {}").unwrap();
        fs::write(&js, "exports={}").unwrap();
        let rules = default_sibling_emit_rules();
        assert!(is_sibling_emit(&js, &rules));
        assert!(!is_sibling_emit(&ts, &rules));
    }

    #[test]
    fn is_git_repo_false_without_dot_git() {
        let path = Path::new("/ravel-non-git-test-does-not-exist");
        assert!(!is_git_repo(path));
        assert_eq!(metadata_fingerprint(path), metadata_fingerprint(path));
    }

    #[test]
    fn metadata_fingerprint_tracks_head_and_nested_worktrees() {
        let dir = tempdir().unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let nested = dir.path().join("packages/app");
        fs::create_dir_all(&nested).unwrap();
        assert!(is_git_repo(&nested));
        let unborn = metadata_fingerprint(&nested);
        fs::write(dir.path().join("tracked.ts"), "export {}\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["add", "."])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["commit", "--quiet", "-m", "initial"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert_ne!(unborn, metadata_fingerprint(&nested));
    }

    #[test]
    fn worktree_identity_supports_unborn_repo_and_nested_root() {
        let dir = tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        let nested = dir.path().join("packages/app");
        fs::create_dir_all(&nested).unwrap();
        let identity = identify_worktree(&nested).unwrap();
        assert_eq!(identity.root, nested);
        assert_eq!(identity.revision, "unborn");
    }

    #[test]
    fn dirty_paths_preserve_unicode_names() {
        let dir = tempdir().unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let relative = PathBuf::from("src/café.ts");
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join(&relative), "export const value = 1;\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["add", "."])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["commit", "--quiet", "-m", "initial"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        fs::write(dir.path().join(&relative), "export const value = 2;\n").unwrap();

        assert_eq!(
            changed_paths(dir.path()).unwrap(),
            vec![dir.path().join(relative)]
        );
    }

    #[test]
    fn cochanged_reports_other_files_from_matching_commits() {
        let dir = tempdir().unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        fs::write(dir.path().join("a.ts"), "a1").unwrap();
        fs::write(dir.path().join("b.ts"), "b1").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["add", "."])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["commit", "--quiet", "-m", "both"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );

        let entries = cochanged(dir.path(), "a.ts", 10, 1).unwrap();
        assert_eq!(
            entries,
            vec![CoChangeEntry {
                file: "b.ts".into(),
                cooccurrence_count: 1
            }]
        );
    }
}
