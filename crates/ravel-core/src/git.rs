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
                        // Relative to the `.git` file, which sits above `root` when `root` is a
                        // directory inside a submodule or a linked worktree.
                        dot_git.parent().unwrap_or(root).join(path)
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

/// Where `root` sits below the top of its worktree: `packages/app` for a package in a monorepo,
/// empty at the top. Git prints status, diff and show paths from the top whatever `-C` says.
fn worktree_prefix(root: &Path) -> PathBuf {
    git_marker(root)
        .and_then(|marker| {
            let top = marker.parent()?;
            root.strip_prefix(top).ok().map(Path::to_path_buf)
        })
        .unwrap_or_default()
}

/// A path git printed (relative to the top of the worktree) made relative to `root` instead, or
/// `None` when it lies outside `root`.
fn relative_to_root(prefix: &Path, printed: &[u8]) -> Option<PathBuf> {
    let printed = PathBuf::from(git_path(printed));
    let relative = printed.strip_prefix(prefix).ok()?;
    (!relative.as_os_str().is_empty()).then(|| relative.to_path_buf())
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
    /// Include untracked files (`??`). Default true: a file an agent just created is dirty in
    /// every sense that matters, and listing untracked files costs `git status` ~20 ms more on a
    /// 20k-file tree (62 ms when all 20k are untracked). Off, such a file is invisible to
    /// discovery until it is committed.
    pub include_untracked: bool,
    pub skip_sibling_emit: bool,
    pub sibling_emit: Vec<SiblingEmitRule>,
}

impl Default for DirtyDiscovery {
    fn default() -> Self {
        Self {
            include_untracked: true,
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
pub fn worktree_source_paths(root: &Path, gitignore: bool) -> Result<Vec<PathBuf>, GitError> {
    if !is_git_repo(root) {
        return Err(GitError::NotWorktree(root.to_path_buf()));
    }
    let mut command = std::process::Command::new("git");
    // The source walk ignores global git excludes. The configured ignore chain filters tracked
    // paths and .ravelignore below, while git prunes ignored untracked directories cheaply.
    command.args([
        "-C",
        &root.to_string_lossy(),
        "-c",
        "core.excludesFile=",
        "ls-files",
        "-z",
        "--cached",
        "--others",
    ]);
    if gitignore {
        command.arg("--exclude-standard");
    }
    let output = command
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
        .map(|entry| root.join(git_path(entry)))
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
    // `-u` lists every untracked file; emit leftovers are filtered below and by the ignore chain.
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
    Ok(parse_porcelain(root, discovery, &output.stdout))
}

/// Dirty paths among `relative` (workspace-relative, `/`-separated): what [`changed_paths_with`]
/// reports for those paths, without making git look at the rest of the tree.
///
/// `git status` stats every tracked file and reads every directory, so one answer costs time
/// proportional to the whole worktree -- ~35 ms on 20k files -- even when the caller only wants to
/// know about the file it just edited. Limited to a pathspec it reads the index and looks at that
/// path alone (~8 ms). Any outcome other than a clean answer falls back to the whole-tree query, so
/// this can only be faster, never different.
pub fn changed_paths_among(
    root: &Path,
    discovery: &DirtyDiscovery,
    relative: &[String],
) -> Result<Vec<PathBuf>, GitError> {
    if !is_git_repo(root) {
        return Err(GitError::NotWorktree(root.to_path_buf()));
    }
    if relative.is_empty() {
        return Ok(Vec::new());
    }
    match status_among(root, discovery, relative) {
        Some(paths) => Ok(paths),
        None => changed_paths_with(root, discovery),
    }
}

/// `git status` limited to `relative`, or `None` when that query cannot answer: too many paths, a
/// pathspec git refuses (one that crosses into a submodule, say), or a git too old to know
/// `--literal-pathspecs`.
fn status_among(
    root: &Path,
    discovery: &DirtyDiscovery,
    relative: &[String],
) -> Option<Vec<PathBuf>> {
    /// Far below any argument-length limit; a bigger batch is a pull or a rebase, where the
    /// whole-tree answer is the cheaper one anyway.
    const MAX_PATHSPECS: usize = 64;
    if relative.len() > MAX_PATHSPECS {
        return None;
    }
    let output = std::process::Command::new("git")
        // Names are literal: `[id].ts` and `*.ts` are files here, not patterns.
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "--no-renames"])
        .arg(if discovery.include_untracked {
            "-u"
        } else {
            "--untracked-files=no"
        })
        .arg("--")
        .args(relative)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse_porcelain(root, discovery, &output.stdout))
}

/// The paths in `git status --porcelain=v1 -z` output, filtered the way discovery has always
/// filtered them and returned sorted.
fn parse_porcelain(root: &Path, discovery: &DirtyDiscovery, stdout: &[u8]) -> Vec<PathBuf> {
    let prefix = worktree_prefix(root);
    let mut paths = Vec::new();
    for record in stdout.split(|byte| *byte == 0) {
        if record.len() < 4 {
            continue;
        }
        let xy = &record[..2];
        let path_part = &record[3..];
        let Some(abs) = relative_to_root(&prefix, path_part).map(|path| root.join(path)) else {
            continue;
        };
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
    paths
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
        let prefix = worktree_prefix(root);
        paths.extend(
            output
                .stdout
                .split(|byte| *byte == 0)
                .filter_map(|path| relative_to_root(&prefix, path))
                .map(|path| root.join(path)),
        );
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
    // Refs reach here from MCP clients. Git would read one that starts with `-` as an option, and
    // `--output=<file>` writes a file. No ref name may start with `-`, so refuse it outright.
    if let Some(option) = [from, to]
        .into_iter()
        .flatten()
        .find(|r| r.starts_with('-'))
    {
        return Err(GitError::Operation(format!(
            "invalid revision '{option}': a revision cannot start with '-'"
        )));
    }
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
    // The range is a revision, never a path.
    args.push("--".into());
    let output = std::process::Command::new("git")
        .args(&args)
        .output()
        .map_err(|error| GitError::Operation(error.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Operation(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let prefix = worktree_prefix(root);
    let mut paths: Vec<_> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|path| relative_to_root(&prefix, path))
        .map(|path| root.join(path))
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
    // The spelling the paths git prints get below: relative to `root`, `/`-separated, no `./`.
    let file = {
        let path = Path::new(file);
        path.strip_prefix(root)
            .unwrap_or(path)
            .components()
            .filter(|component| !matches!(component, std::path::Component::CurDir))
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    };
    // First select commits that touched `file`. A pathspec on the later
    // `--name-only` command would hide every co-changed path and always return
    // an empty result.
    let revisions = std::process::Command::new("git")
        // Names are literal: `[id].ts` is a file here, not a pattern.
        .arg("--literal-pathspecs")
        .args([
            "-C",
            &root.to_string_lossy(),
            "log",
            &format!("--max-count={commits}"),
            "--format=%H",
            "--",
            &file,
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
            "-z",
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
    // Keep commit boundaries and NUL-delimited names: paths may contain newlines or Unicode.
    // Count the whole commit before filtering to a nested workspace, so bulk edits stay bulk.
    let mut per_commit: Vec<Vec<&[u8]>> = Vec::new();
    let mut current: Option<Vec<&[u8]>> = None;
    for record in output.stdout.split(|byte| *byte == 0) {
        let record = if record == b"--" || record.starts_with(b"--\n") {
            if let Some(files) = current.take() {
                per_commit.push(files);
            }
            current = Some(Vec::new());
            record.strip_prefix(b"--\n").unwrap_or_default()
        } else {
            record
        };
        if !record.is_empty()
            && let Some(files) = current.as_mut()
        {
            files.push(record);
        }
    }
    if let Some(files) = current.take() {
        per_commit.push(files);
    }

    let prefix = worktree_prefix(root);
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut commits_considered = 0usize;
    let mut commits_skipped_as_bulk = 0usize;
    for files in &per_commit {
        if files.len() > max_commit_files {
            commits_skipped_as_bulk += 1;
            continue;
        }
        commits_considered += 1;
        for record in files {
            if let Some(path) = relative_to_root(&prefix, record) {
                let path = path.to_string_lossy().into_owned();
                if path != file {
                    *counts.entry(path).or_default() += 1;
                }
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
    fn a_file_created_since_the_last_commit_is_dirty_by_default() {
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
        fs::write(dir.path().join("tracked.ts"), "export {}\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "--quiet", "-m", "initial"]] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        fs::create_dir_all(dir.path().join("src/feature")).unwrap();
        fs::write(
            dir.path().join("src/feature/new.ts"),
            "export const fresh = 1;\n",
        )
        .unwrap();
        // Agents create files faster than they commit them; discovery must see those files
        // without a watcher, and it must still skip emit leftovers.
        fs::write(dir.path().join("src/feature/new.d.ts"), "export {};\n").unwrap();
        let dirty = changed_paths_with(dir.path(), &DirtyDiscovery::default()).unwrap();
        let names: Vec<_> = dirty
            .iter()
            .map(|path| {
                path.strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(names, ["src/feature/new.ts"]);
        let tracked_only = changed_paths_with(
            dir.path(),
            &DirtyDiscovery {
                include_untracked: false,
                ..DirtyDiscovery::default()
            },
        )
        .unwrap();
        assert!(tracked_only.is_empty());
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

#[cfg(test)]
mod among_tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;
    use tempfile::{TempDir, tempdir};

    fn run(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git must be available for this test");
        assert!(status.success(), "git {args:?} failed");
    }

    fn write(dir: &Path, relative: &str, text: &str) {
        let path = dir.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A repository whose worktree has every kind of entry `git status` can report, with names
    /// that would be wrong as patterns.
    fn messy_repo() -> (TempDir, Vec<String>) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q", "."]);
        let tracked = [
            "src/clean.ts",
            "src/edited.ts",
            "src/staged.ts",
            "src/staged_then_edited.ts",
            "src/deleted.ts",
            "src/with space.ts",
            "src/[id].ts",
            "src/{x,y}.ts",
            "src/!bang.ts",
            "src/café.ts",
            "-dash.ts",
            "pkg/a/index.ts",
        ];
        for name in tracked {
            write(root, name, "export const v = 1;\n");
        }
        write(root, ".gitignore", "ignored/\n");
        run(root, &["add", "-A"]);
        run(root, &["commit", "-qm", "seed"]);

        for name in [
            "src/edited.ts",
            "src/staged.ts",
            "src/staged_then_edited.ts",
            "src/with space.ts",
            "src/[id].ts",
            "src/{x,y}.ts",
            "src/!bang.ts",
            "src/café.ts",
            "-dash.ts",
        ] {
            write(root, name, "export const v = 2;\n");
        }
        run(root, &["add", "src/staged.ts", "src/staged_then_edited.ts"]);
        write(root, "src/staged_then_edited.ts", "export const v = 3;\n");
        fs::remove_file(root.join("src/deleted.ts")).unwrap();
        // Untracked: a new file in a tracked directory, one in a brand new directory, a declaration
        // file and a source map (both skipped by discovery), a sibling emit, and an ignored file.
        write(root, "src/brand_new.ts", "export const n = 1;\n");
        write(root, "fresh/dir/deep/new.ts", "export const n = 1;\n");
        write(root, "src/gen.d.ts", "export {};\n");
        write(root, "src/gen.js.map", "{}");
        write(root, "src/emit.ts", "export const e = 1;\n");
        write(root, "src/emit.js", "exports.e = 1;\n");
        write(root, "ignored/skipped.ts", "export const i = 1;\n");

        let mut universe: Vec<String> = tracked.iter().map(|name| (*name).to_owned()).collect();
        universe.extend(
            [
                "src/brand_new.ts",
                "fresh/dir/deep/new.ts",
                "src/gen.d.ts",
                "src/gen.js.map",
                "src/emit.ts",
                "src/emit.js",
                "ignored/skipped.ts",
                "src/never_existed.ts",
                "nowhere/at/all.ts",
            ]
            .map(String::from),
        );
        (dir, universe)
    }

    fn relative(root: &Path, paths: Vec<PathBuf>) -> BTreeSet<String> {
        paths
            .into_iter()
            .map(|path| {
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn asking_about_some_paths_gives_the_whole_tree_answer_restricted_to_them() {
        let (dir, universe) = messy_repo();
        let root = dir.path();
        for discovery in [
            DirtyDiscovery::default(),
            DirtyDiscovery {
                include_untracked: false,
                ..DirtyDiscovery::default()
            },
        ] {
            let whole = relative(root, changed_paths_with(root, &discovery).unwrap());
            assert!(
                whole.len() >= 8,
                "the fixture must exercise many entry kinds, got {whole:?}"
            );
            let mut subsets: Vec<Vec<String>> = vec![universe.clone(), Vec::new()];
            subsets.extend(universe.iter().map(|name| vec![name.clone()]));
            subsets.extend(universe.windows(3).map(<[String]>::to_vec));
            subsets.extend(
                universe
                    .iter()
                    .step_by(2)
                    .map(|name| vec![name.clone(), "src/clean.ts".into()]),
            );
            for subset in subsets {
                let wanted: BTreeSet<String> = subset.iter().cloned().collect();
                let expected: BTreeSet<String> = whole.intersection(&wanted).cloned().collect();
                let answered = if subset.is_empty() {
                    Vec::new()
                } else {
                    // The pathspec query itself, so a silent fallback to the whole tree cannot
                    // make this pass.
                    status_among(root, &discovery, &subset)
                        .unwrap_or_else(|| panic!("the pathspec query refused {subset:?}"))
                };
                assert_eq!(
                    relative(root, answered.clone()),
                    relative(
                        root,
                        changed_paths_among(root, &discovery, &subset).unwrap()
                    )
                );
                let among = relative(root, answered);
                // Whatever else git volunteers, the entries for the asked-for paths must match
                // the whole-tree answer exactly: none missing, none invented.
                let among_wanted: BTreeSet<String> = among.intersection(&wanted).cloned().collect();
                assert_eq!(
                    among_wanted, expected,
                    "untracked={} subset={subset:?}",
                    discovery.include_untracked
                );
            }
        }
    }

    #[test]
    fn a_batch_past_the_pathspec_limit_still_answers() {
        let (dir, universe) = messy_repo();
        let root = dir.path();
        let discovery = DirtyDiscovery::default();
        let whole = relative(root, changed_paths_with(root, &discovery).unwrap());
        let mut many = universe.clone();
        many.extend((0..100).map(|n| format!("src/pad{n}.ts")));
        let among = relative(root, changed_paths_among(root, &discovery, &many).unwrap());
        let wanted: BTreeSet<String> = many.into_iter().collect();
        assert_eq!(
            among
                .intersection(&wanted)
                .cloned()
                .collect::<BTreeSet<_>>(),
            whole
                .intersection(&wanted)
                .cloned()
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn a_path_inside_a_nested_repository_agrees_with_the_whole_tree_query() {
        let (dir, _) = messy_repo();
        let root = dir.path();
        // A repository inside the worktree: git refuses pathspecs that cross into it, and the
        // answer must then come from the whole-tree query rather than being lost.
        let nested = root.join("vendor/lib");
        fs::create_dir_all(&nested).unwrap();
        run(&nested, &["init", "-q", "."]);
        write(&nested, "inner.ts", "export const i = 1;\n");
        let discovery = DirtyDiscovery::default();
        let whole = relative(root, changed_paths_with(root, &discovery).unwrap());
        let ask = vec!["vendor/lib/inner.ts".to_owned(), "src/edited.ts".to_owned()];
        let wanted: BTreeSet<String> = ask.iter().cloned().collect();
        let among = relative(root, changed_paths_among(root, &discovery, &ask).unwrap());
        assert_eq!(
            among
                .intersection(&wanted)
                .cloned()
                .collect::<BTreeSet<_>>(),
            whole
                .intersection(&wanted)
                .cloned()
                .collect::<BTreeSet<_>>()
        );
        assert!(among.contains("src/edited.ts"));
    }

    #[test]
    fn outside_a_repository_it_says_so_like_the_whole_tree_query() {
        let dir = tempdir().unwrap();
        let discovery = DirtyDiscovery::default();
        assert!(matches!(
            changed_paths_among(dir.path(), &discovery, &["a.ts".to_owned()]),
            Err(GitError::NotWorktree(_))
        ));
        assert!(matches!(
            changed_paths_with(dir.path(), &discovery),
            Err(GitError::NotWorktree(_))
        ));
    }

    /// A package of a monorepo: git prints paths from the top of the repository, not from `-C`.
    fn monorepo() -> (TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let top = dir.path();
        run(top, &["init", "-q", "."]);
        write(top, "packages/app/src/a.ts", "export const a = 1;\n");
        write(top, "other.ts", "export const other = 1;\n");
        run(top, &["add", "-A"]);
        run(top, &["commit", "-q", "-m", "one"]);
        let app = top.join("packages/app");
        (dir, app)
    }

    #[test]
    fn a_root_below_the_top_of_the_repository_sees_its_own_paths_only() {
        let (dir, app) = monorepo();
        write(&app, "src/a.ts", "export const a = 2;\n");
        write(&app, "src/new.ts", "export const fresh = 1;\n");
        write(dir.path(), "other.ts", "export const other = 2;\n");
        let discovery = DirtyDiscovery::default();
        let expected = vec![app.join("src/a.ts"), app.join("src/new.ts")];
        assert_eq!(changed_paths_with(&app, &discovery).unwrap(), expected);
        let asked = ["src/a.ts".to_owned(), "src/new.ts".to_owned()];
        assert_eq!(
            changed_paths_among(&app, &discovery, &asked).unwrap(),
            expected
        );
        assert_eq!(dirty_tracked_diff(&app).unwrap(), [app.join("src/a.ts")]);

        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-q", "-m", "two"]);
        assert_eq!(
            changed_paths_between(&app, Some("HEAD~1"), None).unwrap(),
            expected
        );
    }

    #[test]
    fn cochanged_names_paths_from_the_root_however_the_file_is_spelled() {
        let (dir, app) = monorepo();
        write(&app, "src/a.ts", "export const a = 2;\n");
        write(&app, "src/café.ts", "export const cafe = 1;\n");
        write(dir.path(), "other.ts", "export const other = 2;\n");
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-q", "-m", "two"]);
        let absolute = app.join("src/a.ts").to_string_lossy().into_owned();
        for spelling in ["src/a.ts", "./src/a.ts", absolute.as_str()] {
            let page = cochanged(&app, spelling, 10, 1, DEFAULT_MAX_COMMIT_FILES, 20, 0).unwrap();
            assert_eq!(
                page.entries,
                [CoChangeEntry {
                    file: "src/café.ts".into(),
                    cooccurrence_count: 1,
                    confidence_micros: 500_000,
                }],
                "asked as {spelling}"
            );
        }
        let bounded = cochanged(&app, "src/a.ts", 10, 1, 2, 20, 0).unwrap();
        assert!(bounded.entries.is_empty());
        assert_eq!(bounded.commits_considered, 1);
        assert_eq!(bounded.commits_skipped_as_bulk, 1);
    }

    #[test]
    fn a_revision_that_looks_like_an_option_is_refused() {
        let (dir, app) = monorepo();
        let target = dir.path().join("written-by-git");
        let option = format!("--output={}", target.display());
        assert!(changed_paths_between(&app, Some(&option), None).is_err());
        assert!(changed_paths_between(&app, Some("HEAD"), Some(&option)).is_err());
        assert!(!target.exists());
        assert!(!dir.path().join(format!("{option}...HEAD")).exists());
    }

    #[test]
    fn a_relative_gitdir_is_read_from_the_directory_of_the_git_file() {
        // A submodule's `.git` is a file whose relative `gitdir:` starts where that file is, not
        // where the indexed root is.
        let dir = tempdir().unwrap();
        let submodule = dir.path().join("sub");
        write(&submodule, ".git", "gitdir: ../modules/sub\n");
        let git_dir = dir.path().join("modules/sub");
        write(&git_dir, "HEAD", "ref: refs/heads/main\n");
        write(
            &git_dir,
            "refs/heads/main",
            "1111111111111111111111111111111111111111\n",
        );
        let root = submodule.join("packages/app");
        fs::create_dir_all(&root).unwrap();
        let before = metadata_fingerprint(&root);
        write(
            &git_dir,
            "refs/heads/main",
            "2222222222222222222222222222222222222222\n",
        );
        assert_ne!(before, metadata_fingerprint(&root));
    }
}
