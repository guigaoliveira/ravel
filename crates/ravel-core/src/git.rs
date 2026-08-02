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
/// Every path git would consider part of the worktree, ignored files excluded.
///
/// The coverage probe used to walk the filesystem under a file-count cap, which on a large
/// repository stopped early and then reported the partial counts as if they were totals. Git
/// already maintains this list; asking for it is one process instead of a bounded walk, so the
/// cap -- and the truncation it produced -- disappears wherever there is a repository.
pub fn worktree_source_paths(root: &Path) -> Result<Vec<PathBuf>, GitError> {
    if !is_git_repo(root) {
        return Err(GitError::NotWorktree(root.to_path_buf()));
    }
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &root.to_string_lossy(),
            "ls-files",
            "-z",
            "--cached",
            "--others",
            // Same exclusion the walk applied: .gitignore, .git/info/exclude, global excludes.
            "--exclude-standard",
        ])
        .output()
        .map_err(|source| GitError::Operation(source.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Operation(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| root.join(String::from_utf8_lossy(entry).as_ref()))
        .collect())
}

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
    /// Share of the considered commits that touched both files, in millionths. Raw counts rank a
    /// file that changes in every commit above one that changes *with this file specifically*.
    pub confidence_micros: u32,
}

/// A commit above this many files is treated as a bulk edit and skipped: a mass rename, format or
/// import couples every file it touches with every other, which is noise rather than co-change.
///
/// Measured over 238 commits of a 21k-file monorepo: median 8 files, p95 104, p99 784. The two
/// commits that produced a 6,910-entry answer for a single file touched 6,989 and 20,903.
pub const DEFAULT_MAX_COMMIT_FILES: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CoChangePage {
    pub entries: Vec<CoChangeEntry>,
    pub total: usize,
    pub truncated: bool,
    pub next_cursor: Option<usize>,
    /// Commits that touched the file and were counted.
    pub commits_considered: usize,
    /// Commits that touched the file and were skipped for being bulk edits. Reported because
    /// otherwise an empty answer reads as "nothing co-changes with this file" when the truth is
    /// "every commit that touched it was a mass edit".
    pub commits_skipped_as_bulk: usize,
}

#[allow(clippy::too_many_arguments)]
pub fn cochanged(
    root: &Path,
    file: &str,
    commits: usize,
    min_cooccurrence: u32,
    max_commit_files: usize,
    limit: usize,
    cursor: usize,
) -> Result<CoChangePage, GitError> {
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
        return Ok(CoChangePage {
            entries: Vec::new(),
            total: 0,
            truncated: false,
            next_cursor: None,
            commits_considered: 0,
            commits_skipped_as_bulk: 0,
        });
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
    // Grouped per commit first: a commit has to be measured whole before deciding whether its
    // pairings mean anything.
    let mut per_commit: Vec<Vec<String>> = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line == "--" {
            if let Some(files) = current.take() {
                per_commit.push(files);
            }
            current = Some(Vec::new());
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if let Some(files) = current.as_mut() {
            files.push(line.to_owned());
        }
    }
    if let Some(files) = current.take() {
        per_commit.push(files);
    }

    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut commits_considered = 0usize;
    let mut commits_skipped_as_bulk = 0usize;
    for files in &per_commit {
        if files.len() > max_commit_files {
            commits_skipped_as_bulk += 1;
            continue;
        }
        commits_considered += 1;
        for path in files {
            if path != file {
                *counts.entry(path.clone()).or_default() += 1;
            }
        }
    }

    let considered = commits_considered.max(1) as u64;
    let mut entries: Vec<_> = counts
        .into_iter()
        .filter(|(_, count)| *count >= min_cooccurrence)
        .map(|(file, cooccurrence_count)| CoChangeEntry {
            confidence_micros: ((u64::from(cooccurrence_count) * 1_000_000) / considered) as u32,
            file,
            cooccurrence_count,
        })
        .collect();
    entries.sort_by(|left, right| {
        right
            .confidence_micros
            .cmp(&left.confidence_micros)
            .then_with(|| right.cooccurrence_count.cmp(&left.cooccurrence_count))
            .then_with(|| left.file.cmp(&right.file))
    });
    let total = entries.len();
    let page: Vec<_> = entries.into_iter().skip(cursor).take(limit).collect();
    let next = cursor + page.len();
    Ok(CoChangePage {
        entries: page,
        total,
        truncated: next < total,
        next_cursor: (next < total).then_some(next),
        commits_considered,
        commits_skipped_as_bulk,
    })
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

        let page = cochanged(dir.path(), "a.ts", 10, 1, DEFAULT_MAX_COMMIT_FILES, 20, 0).unwrap();
        assert_eq!(
            page.entries,
            vec![CoChangeEntry {
                file: "b.ts".into(),
                cooccurrence_count: 1,
                // The only considered commit touched both, so the pairing is certain.
                confidence_micros: 1_000_000,
            }]
        );
        assert_eq!(page.total, 1);
        assert!(!page.truncated);
        assert_eq!(page.commits_considered, 1);
        assert_eq!(page.commits_skipped_as_bulk, 0);
    }

    /// A bulk edit couples everything it touches with everything else.
    ///
    /// Measured on a real monorepo: a single file returned 6,910 co-changed entries, and the reason
    /// was not ranking -- the file had been touched by exactly two commits, of 6,989 and 20,903
    /// files. Counting those pairings answers a question nobody asked. Skipping them can leave
    /// nothing at all, which is why the count of skipped commits is part of the answer: an empty
    /// result must not read as "nothing co-changes with this file".
    #[test]
    fn a_bulk_commit_is_skipped_and_the_skip_is_reported() {
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
        let commit = |message: &str| {
            for args in [vec!["add", "."], vec!["commit", "--quiet", "-m", message]] {
                assert!(
                    std::process::Command::new("git")
                        .args(args)
                        .current_dir(dir.path())
                        .status()
                        .unwrap()
                        .success()
                );
            }
        };

        // A mass edit touching the subject and 30 unrelated files.
        fs::write(dir.path().join("subject.ts"), "v1").unwrap();
        for index in 0..30 {
            fs::write(dir.path().join(format!("bulk{index}.ts")), "v1").unwrap();
        }
        commit("bulk");

        // A focused edit touching the subject and one genuine partner.
        fs::write(dir.path().join("subject.ts"), "v2").unwrap();
        fs::write(dir.path().join("partner.ts"), "v1").unwrap();
        commit("focused");

        let unbounded = cochanged(
            dir.path(),
            "subject.ts",
            10,
            1,
            DEFAULT_MAX_COMMIT_FILES,
            100,
            0,
        )
        .unwrap();
        assert_eq!(
            unbounded.total, 31,
            "with no size limit the bulk commit dominates: {unbounded:?}"
        );

        let bounded = cochanged(dir.path(), "subject.ts", 10, 1, 10, 100, 0).unwrap();
        assert_eq!(bounded.commits_skipped_as_bulk, 1);
        assert_eq!(bounded.commits_considered, 1);
        assert_eq!(
            bounded
                .entries
                .iter()
                .map(|e| e.file.as_str())
                .collect::<Vec<_>>(),
            vec!["partner.ts"],
            "only the focused commit's pairing survives: {bounded:?}"
        );
    }

    #[test]
    fn cochanged_pages_without_repeating_or_skipping() {
        let dir = tempdir().unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
        }
        fs::write(dir.path().join("subject.ts"), "v1").unwrap();
        for index in 0..5 {
            fs::write(dir.path().join(format!("p{index}.ts")), "v1").unwrap();
        }
        for args in [vec!["add", "."], vec!["commit", "--quiet", "-m", "seed"]] {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
        }

        let first = cochanged(dir.path(), "subject.ts", 10, 1, 500, 2, 0).unwrap();
        assert_eq!(first.total, 5);
        assert!(first.truncated);
        assert_eq!(first.entries.len(), 2);
        let cursor = first.next_cursor.expect("more remain");
        let second = cochanged(dir.path(), "subject.ts", 10, 1, 500, 2, cursor).unwrap();
        assert_eq!(second.entries.len(), 2);
        let seen: Vec<_> = first
            .entries
            .iter()
            .chain(&second.entries)
            .map(|entry| entry.file.as_str())
            .collect();
        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            seen.len(),
            unique.len(),
            "a page repeated an entry: {seen:?}"
        );
    }
}
