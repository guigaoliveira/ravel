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

pub fn identify_worktree(root: &Path) -> Result<WorktreeIdentity, GitError> {
    let repo = gix::discover(root).map_err(|_| GitError::NotWorktree(root.to_path_buf()))?;
    let revision = repo
        .head_id()
        .map(|id| id.to_string())
        .unwrap_or_else(|_| "unborn".into());
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
    // Single stat: `.git` present as a directory (normal repo) or a file (linked worktree).
    root.join(".git").exists()
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
    /// on monorepos with tsc emit / build leftovers.
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
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.len() < 4 {
            continue;
        }
        let xy = &line[..2];
        let path_part = line[3..].trim();
        let path_part = path_part
            .rsplit_once(" -> ")
            .map(|(_, n)| n)
            .unwrap_or(path_part);
        if path_part.is_empty() {
            continue;
        }
        let abs = root.join(path_part);
        let untracked = xy == "??";
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
        .args(["-C", root_s.as_ref(), "diff", "--name-only", "HEAD"])
        .output()
        .map_err(|e| GitError::Operation(e.to_string()))?;
    if output.status.success() {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if !line.is_empty() {
                paths.push(root.join(line));
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
    let mut paths: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| root.join(line))
        .collect();
    paths.sort();
    Ok(paths)
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
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &root.to_string_lossy(),
            "log",
            &format!("--max-count={commits}"),
            "--name-only",
            "--pretty=format:--",
            "--",
            file,
        ])
        .output()
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
        let dir = tempdir().unwrap();
        assert!(!is_git_repo(dir.path()));
    }
}
