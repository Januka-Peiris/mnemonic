use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::models::{ChangedFile, WorkspaceChangedFile, WorkspaceFileDiff};
use crate::repositories::workspace_repository;
use crate::state::AppState;

const MAX_UNTRACKED_PREVIEW_BYTES: usize = 200_000;

/// Changed files for a workspace: committed work vs the base branch's
/// merge-base plus uncommitted working-tree changes, merged per path. This is
/// what keeps diff counts meaningful after the agent commits/pushes for a PR.
pub fn get_workspace_changed_files(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<WorkspaceChangedFile>, String> {
    let started = Instant::now();
    let (root, base_branch) = workspace_root_and_base(state, workspace_id)?;
    ensure_git_worktree(&root)?;
    let porcelain = git(&root, &["status", "--porcelain=v1", "-z"])?;
    let mut working = parse_porcelain(workspace_id, &porcelain);

    for file in &mut working {
        let (additions, deletions) = diff_counts(&root, file);
        file.additions = additions;
        file.deletions = deletions;
    }

    let committed = merge_base(&root, &base_branch)
        .map(|sha| committed_changed_files(workspace_id, &root, &sha))
        .unwrap_or_default();
    let files = merge_changed_files(working, committed);

    // Best-effort cache so sidebar counters and conflict detection reflect
    // the real git state; review flows never fail on a DB hiccup.
    let cached: Vec<ChangedFile> = files
        .iter()
        .map(|file| ChangedFile {
            path: file.path.clone(),
            additions: file.additions.unwrap_or(0),
            deletions: file.deletions.unwrap_or(0),
            status: file.status.clone(),
        })
        .collect();
    if let Err(err) = workspace_repository::replace_changed_files(&state.db, workspace_id, &cached)
    {
        log::warn!(
            target: "mnemonic_lib",
            "failed to cache changed files for {workspace_id}: {err}"
        );
    }

    log::debug!(
        target: "mnemonic_lib",
        "get_workspace_changed_files workspace={} files={} elapsed_ms={}",
        workspace_id,
        files.len(),
        started.elapsed().as_millis()
    );
    Ok(files)
}

pub fn get_workspace_file_diff(
    state: &AppState,
    workspace_id: &str,
    path: &str,
) -> Result<WorkspaceFileDiff, String> {
    let (root, base_branch) = workspace_root_and_base(state, workspace_id)?;
    ensure_git_worktree(&root)?;
    let changed_files = get_workspace_changed_files(state, workspace_id)?;
    let changed = changed_files
        .iter()
        .find(|file| file.path == path)
        .cloned()
        .unwrap_or_else(|| WorkspaceChangedFile {
            workspace_id: workspace_id.to_string(),
            path: path.to_string(),
            old_path: None,
            status: "modified".to_string(),
            staged: false,
            unstaged: true,
            additions: None,
            deletions: None,
        });

    let (diff, source, is_binary) = if changed.status == "untracked" {
        untracked_preview(&root, &changed.path)?
    } else {
        let mut parts = Vec::new();
        if changed.staged {
            if let Ok(staged) = git(&root, &["diff", "--cached", "--", &changed.path]) {
                if !staged.trim().is_empty() {
                    parts.push(staged);
                }
            }
        }
        if changed.unstaged || parts.is_empty() {
            if let Ok(unstaged) = git(&root, &["diff", "--", &changed.path]) {
                if !unstaged.trim().is_empty() {
                    parts.push(unstaged);
                }
            }
        }
        if parts.is_empty() && changed.status == "deleted" {
            if let Ok(deleted) = git(&root, &["diff", "HEAD", "--", &changed.path]) {
                if !deleted.trim().is_empty() {
                    parts.push(deleted);
                }
            }
        }
        if parts.is_empty() {
            // Committed-only change (clean working tree): diff vs the base
            // branch's merge-base so reviewed work stays visible after a PR.
            if let Some(sha) = merge_base(&root, &base_branch) {
                if let Ok(committed) = git(&root, &["diff", &sha, "HEAD", "--", &changed.path]) {
                    if !committed.trim().is_empty() {
                        parts.push(committed);
                    }
                }
            }
        }

        let combined = parts.join("\n");
        let is_binary = combined.contains("Binary files") || combined.contains("GIT binary patch");
        let diff = if combined.trim().is_empty() {
            "No text diff available for this file.".to_string()
        } else {
            combined
        };
        (diff, "git_diff".to_string(), is_binary)
    };

    Ok(WorkspaceFileDiff {
        workspace_id: workspace_id.to_string(),
        path: changed.path,
        old_path: changed.old_path,
        status: changed.status,
        diff,
        is_binary,
        source,
    })
}

fn workspace_root_and_base(
    state: &AppState,
    workspace_id: &str,
) -> Result<(PathBuf, String), String> {
    let detail = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let base_branch = detail.summary.branch_health.base_branch.clone();
    let root = detail
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or(detail.worktree_path);
    let path = PathBuf::from(root);
    if !path.exists() {
        return Err(format!(
            "Workspace root path does not exist: {}",
            path.display()
        ));
    }
    Ok((path, base_branch))
}

/// Merge-base between HEAD and the base branch, preferring the remote ref.
/// None when neither ref resolves (missing remote, detached state) or when
/// the branch has no commits of its own (merge-base == HEAD).
fn merge_base(root: &Path, base_branch: &str) -> Option<String> {
    let base = base_branch.trim();
    if base.is_empty() {
        return None;
    }
    let remote_ref = format!("origin/{base}");
    let sha = git(root, &["merge-base", "HEAD", &remote_ref])
        .or_else(|_| git(root, &["merge-base", "HEAD", base]))
        .ok()?;
    let sha = sha.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    let head = git(root, &["rev-parse", "HEAD"]).ok()?;
    if head.trim() == sha {
        return None;
    }
    Some(sha)
}

/// Files changed by commits between the merge-base and HEAD, with per-file
/// counts. Best-effort: parse failures simply drop counts, not files.
fn committed_changed_files(
    workspace_id: &str,
    root: &Path,
    merge_base_sha: &str,
) -> Vec<WorkspaceChangedFile> {
    let Ok(name_status) = git(
        root,
        &["diff", "--name-status", "-z", "-M", merge_base_sha, "HEAD"],
    ) else {
        return Vec::new();
    };

    let mut files = parse_name_status(workspace_id, &name_status);
    if files.is_empty() {
        return files;
    }

    if let Ok(numstat) = git(
        root,
        &["diff", "--numstat", "-z", "-M", merge_base_sha, "HEAD"],
    ) {
        let counts = parse_numstat(&numstat);
        for file in &mut files {
            if let Some((additions, deletions)) = counts
                .iter()
                .find_map(|(path, counts)| (path == &file.path).then_some(*counts))
            {
                file.additions = additions;
                file.deletions = deletions;
            }
        }
    }

    files
}

/// Parses `git diff --name-status -z -M` output: STATUS NUL path NUL, with a
/// second path entry for renames/copies (old NUL new NUL).
fn parse_name_status(workspace_id: &str, raw: &str) -> Vec<WorkspaceChangedFile> {
    let entries: Vec<&str> = raw.split('\0').filter(|entry| !entry.is_empty()).collect();
    let mut files = Vec::new();
    let mut index = 0;

    while index < entries.len() {
        let status_code = entries[index];
        let kind = status_code.chars().next().unwrap_or(' ');
        let status = match kind {
            'A' => "added",
            'D' => "deleted",
            'R' | 'C' => "renamed",
            _ => "modified",
        }
        .to_string();

        let mut old_path = None;
        let path;
        if kind == 'R' || kind == 'C' {
            old_path = entries.get(index + 1).map(|entry| entry.to_string());
            path = entries
                .get(index + 2)
                .copied()
                .unwrap_or_default()
                .to_string();
            index += 3;
        } else {
            path = entries
                .get(index + 1)
                .copied()
                .unwrap_or_default()
                .to_string();
            index += 2;
        }
        if path.is_empty() {
            continue;
        }

        files.push(WorkspaceChangedFile {
            workspace_id: workspace_id.to_string(),
            path,
            old_path,
            status,
            staged: false,
            unstaged: false,
            additions: None,
            deletions: None,
        });
    }

    files
}

type LineChangeCount = (Option<u32>, Option<u32>);
type NumstatEntry = (String, LineChangeCount);

/// Parses `git diff --numstat -z -M` output into `(path, (additions, deletions))`.
/// Binary files report "-" and stay None. Renames emit an empty path field
/// followed by old NUL new NUL; the new path gets the counts.
fn parse_numstat(raw: &str) -> Vec<NumstatEntry> {
    let entries: Vec<&str> = raw.split('\0').filter(|entry| !entry.is_empty()).collect();
    let mut counts = Vec::new();
    let mut index = 0;

    while index < entries.len() {
        let record = entries[index];
        let mut parts = record.split('\t');
        let additions = parts.next().and_then(|value| value.parse::<u32>().ok());
        let deletions = parts.next().and_then(|value| value.parse::<u32>().ok());
        let inline_path = parts.next().unwrap_or_default();

        if inline_path.is_empty() {
            // Rename record: counts NUL old NUL new NUL.
            let new_path = entries.get(index + 2).copied().unwrap_or_default();
            if !new_path.is_empty() {
                counts.push((new_path.to_string(), (additions, deletions)));
            }
            index += 3;
        } else {
            counts.push((inline_path.to_string(), (additions, deletions)));
            index += 1;
        }
    }

    counts
}

/// Merges working-tree changes with committed-vs-base changes per path:
/// working-tree status/staging wins, counts are summed (uncommitted counts
/// are vs HEAD, committed ones vs merge-base).
fn merge_changed_files(
    working: Vec<WorkspaceChangedFile>,
    committed: Vec<WorkspaceChangedFile>,
) -> Vec<WorkspaceChangedFile> {
    let mut files = working;
    for committed_file in committed {
        if let Some(existing) = files
            .iter_mut()
            .find(|file| file.path == committed_file.path)
        {
            existing.additions = sum_counts(existing.additions, committed_file.additions);
            existing.deletions = sum_counts(existing.deletions, committed_file.deletions);
        } else {
            files.push(committed_file);
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

fn sum_counts(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

fn ensure_git_worktree(root: &Path) -> Result<(), String> {
    let inside = git(root, &["rev-parse", "--is-inside-work-tree"])?;
    if inside.trim() == "true" {
        Ok(())
    } else {
        Err(format!("Path is not a git worktree: {}", root.display()))
    }
}

fn parse_porcelain(workspace_id: &str, porcelain: &str) -> Vec<WorkspaceChangedFile> {
    let entries = porcelain
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    let mut files = Vec::new();
    let mut index = 0;

    while index < entries.len() {
        let entry = entries[index];
        if entry.len() < 4 {
            index += 1;
            continue;
        }

        let x = entry.as_bytes()[0] as char;
        let y = entry.as_bytes()[1] as char;
        let raw_path = entry[3..].to_string();
        let staged = x != ' ' && x != '?';
        let unstaged = y != ' ' || x == '?';
        let status = status_from_xy(x, y);
        let mut old_path = None;
        let path;

        if x == 'R' || x == 'C' {
            old_path = Some(raw_path);
            index += 1;
            path = entries.get(index).copied().unwrap_or_default().to_string();
        } else {
            path = raw_path;
        }

        files.push(WorkspaceChangedFile {
            workspace_id: workspace_id.to_string(),
            path,
            old_path,
            status,
            staged,
            unstaged,
            additions: None,
            deletions: None,
        });
        index += 1;
    }

    files
}

fn status_from_xy(x: char, y: char) -> String {
    if x == '?' {
        "untracked"
    } else if x == 'R' || y == 'R' {
        "renamed"
    } else if x == 'A' || y == 'A' {
        "added"
    } else if x == 'D' || y == 'D' {
        "deleted"
    } else {
        "modified"
    }
    .to_string()
}

fn diff_counts(root: &Path, file: &WorkspaceChangedFile) -> (Option<u32>, Option<u32>) {
    let output = if file.status == "untracked" {
        fs::read_to_string(root.join(&file.path))
            .ok()
            .map(|content| format!("0\t{}\t{}", content.lines().count(), file.path))
    } else if file.staged {
        git(root, &["diff", "--cached", "--numstat", "--", &file.path]).ok()
    } else {
        git(root, &["diff", "--numstat", "--", &file.path]).ok()
    };

    let Some(output) = output else {
        return (None, None);
    };
    let first = output.lines().next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let additions = parts.next().and_then(|value| value.parse::<u32>().ok());
    let deletions = parts.next().and_then(|value| value.parse::<u32>().ok());
    (additions, deletions)
}

fn untracked_preview(root: &Path, path: &str) -> Result<(String, String, bool), String> {
    let full_path = root.join(path);
    let metadata = fs::metadata(&full_path).map_err(|err| {
        format!(
            "Failed to inspect untracked file {}: {err}",
            full_path.display()
        )
    })?;
    if metadata.len() as usize > MAX_UNTRACKED_PREVIEW_BYTES {
        return Ok((
            format!(
                "Untracked file is too large to preview ({} bytes).",
                metadata.len()
            ),
            "untracked_preview".to_string(),
            false,
        ));
    }

    let bytes = fs::read(&full_path).map_err(|err| {
        format!(
            "Failed to read untracked file {}: {err}",
            full_path.display()
        )
    })?;
    if bytes.contains(&0) {
        return Ok((
            "Binary untracked file; no text diff available.".to_string(),
            "untracked_preview".to_string(),
            true,
        ));
    }

    let content = String::from_utf8_lossy(&bytes);
    let mut diff = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n"
    );
    for line in content.lines() {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    Ok((diff, "untracked_preview".to_string(), false))
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|err| format!("failed to run git in {}: {err}", root.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("git command failed in {}", root.display())
        } else {
            stderr
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, additions: Option<u32>, deletions: Option<u32>) -> WorkspaceChangedFile {
        WorkspaceChangedFile {
            workspace_id: "ws-1".to_string(),
            path: path.to_string(),
            old_path: None,
            status: "modified".to_string(),
            staged: false,
            unstaged: true,
            additions,
            deletions,
        }
    }

    #[test]
    fn merge_sums_counts_for_shared_paths() {
        let working = vec![file("src/a.rs", Some(3), Some(1))];
        let committed = vec![file("src/a.rs", Some(10), Some(2))];
        let merged = merge_changed_files(working, committed);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].additions, Some(13));
        assert_eq!(merged[0].deletions, Some(3));
        // Working-tree staging flags win for shared paths.
        assert!(merged[0].unstaged);
    }

    #[test]
    fn merge_appends_committed_only_paths_sorted() {
        let working = vec![file("src/z.rs", Some(1), Some(0))];
        let committed = vec![file("src/a.rs", Some(5), Some(5))];
        let merged = merge_changed_files(working, committed);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].path, "src/a.rs");
        assert_eq!(merged[1].path, "src/z.rs");
    }

    #[test]
    fn merge_keeps_none_counts_when_both_unknown() {
        let working = vec![file("bin/blob", None, None)];
        let committed = vec![file("bin/blob", None, None)];
        let merged = merge_changed_files(working, committed);
        assert_eq!(merged[0].additions, None);
        assert_eq!(merged[0].deletions, None);
    }

    #[test]
    fn parses_name_status_with_rename() {
        let raw = "M\0src/a.rs\0R100\0src/old.rs\0src/new.rs\0A\0src/b.rs\0";
        let files = parse_name_status("ws-1", raw);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].status, "modified");
        assert_eq!(files[1].path, "src/new.rs");
        assert_eq!(files[1].old_path.as_deref(), Some("src/old.rs"));
        assert_eq!(files[1].status, "renamed");
        assert_eq!(files[2].status, "added");
    }

    #[test]
    fn parses_numstat_with_rename_and_binary() {
        let raw = concat!(
            "10\t2\tsrc/a.rs\0",
            "-\t-\tassets/logo.png\0",
            "5\t1\t\0src/old.rs\0src/new.rs\0",
        );
        let counts = parse_numstat(raw);
        assert_eq!(counts.len(), 3);
        assert_eq!(counts[0], ("src/a.rs".to_string(), (Some(10), Some(2))));
        assert_eq!(counts[1], ("assets/logo.png".to_string(), (None, None)));
        assert_eq!(counts[2], ("src/new.rs".to_string(), (Some(5), Some(1))));
    }
}
