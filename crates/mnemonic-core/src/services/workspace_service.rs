use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{
    ArchivedWorkspace, AttachLinkedWorktreeInput, CreateChildWorkspaceInput, CreateWorkspaceInput,
    DiscoveredRepository, LinkedWorktreeRef, RepositoryWorkspaceOptions, WorkspaceDetail,
    WorkspaceSummary,
};
use crate::repositories::{
    activity_repository, repository_repository, settings_repository, workspace_repository,
};
use crate::services::{git_review_service, git_worktree_service, repo_scanner_service};
use crate::state::AppState;

mod ops;

pub fn list_workspaces(state: &AppState) -> Result<Vec<WorkspaceSummary>, String> {
    workspace_repository::list(&state.db)
}

/// Re-diffs one workspace against its base and caches the result, so the
/// next list_workspaces returns fresh changed files and line counts for it.
/// Only the workspace being looked at is refreshed: diffing every worktree
/// on every poll kept git busy for no one.
pub fn refresh_workspace_changed_files(state: &AppState, workspace_id: &str) -> Result<(), String> {
    let workspace = workspace_repository::get(&state.db, workspace_id)?;
    let base = workspace.branch_health.base_branch.trim();
    let root = workspace
        .repository_path
        .as_deref()
        .or(workspace.workspace_root_path.as_deref());
    if let Some(root) = root.filter(|_| !base.is_empty()) {
        fetch_remote_base_if_stale(Path::new(root), base);
    }
    git_review_service::get_workspace_changed_files(state, workspace_id)?;
    Ok(())
}

/// Changed files are diffed against `origin/<base>`, which only moves on a
/// fetch. Without this, merged work keeps showing as changed until someone
/// fetches by hand. Throttled per repo and base because the selected workspace is
/// refreshed on a timer.
fn fetch_remote_base_if_stale(root: &Path, base: &str) {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static LAST_FETCH: Mutex<Option<HashMap<(String, String), Instant>>> = Mutex::new(None);
    const MIN_INTERVAL: Duration = Duration::from_secs(60);

    let key = (root.to_string_lossy().to_string(), base.to_string());
    {
        let Ok(mut guard) = LAST_FETCH.lock() else {
            return;
        };
        let last_fetch = guard.get_or_insert_with(HashMap::new);
        if let Some(when) = last_fetch.get(&key) {
            if when.elapsed() < MIN_INTERVAL {
                return;
            }
        }
        last_fetch.insert(key, Instant::now());
    }

    // An explicit refspec keeps origin/<base> current even when the remote's
    // configured fetch refspec does not cover it.
    let refspec = format!("+refs/heads/{base}:refs/remotes/origin/{base}");
    if let Err(err) = run_git(root, &["fetch", "--quiet", "--no-tags", "origin", &refspec]) {
        log::warn!(
            target: "mnemonic_lib",
            "fetch origin/{base} for changed files failed in {}: {err}",
            root.display()
        );
    }
}

pub fn get_workspace_detail(state: &AppState, id: &str) -> Result<Option<WorkspaceDetail>, String> {
    workspace_repository::get_detail(&state.db, id)
}

pub fn list_repositories_for_workspace_creation(
    state: &AppState,
) -> Result<Vec<DiscoveredRepository>, String> {
    repository_repository::list(&state.db)
}

pub fn get_repository_workspace_options(
    state: &AppState,
    repository_id: &str,
) -> Result<RepositoryWorkspaceOptions, String> {
    let repository = repository_repository::get(&state.db, repository_id)?
        .ok_or_else(|| format!("Repository {repository_id} was not found"))?;
    let branches = git_worktree_service::list_branches(Path::new(&repository.path));

    Ok(RepositoryWorkspaceOptions {
        repository,
        branches,
    })
}

pub fn create_workspace(
    state: &AppState,
    input: CreateWorkspaceInput,
) -> Result<WorkspaceDetail, String> {
    ops::create_workspace(state, input)
}

pub fn delete_workspace(state: &AppState, workspace_id: &str) -> Result<(), String> {
    ops::delete_workspace(state, workspace_id)
}

pub fn archive_workspace(state: &AppState, workspace_id: &str) -> Result<(), String> {
    ops::archive_workspace(state, workspace_id)
}

pub fn create_child_workspace(
    state: &AppState,
    input: CreateChildWorkspaceInput,
) -> Result<WorkspaceDetail, String> {
    ops::create_child_workspace(state, input)
}

pub fn attach_workspace_linked_worktree(
    state: &AppState,
    input: AttachLinkedWorktreeInput,
) -> Result<Vec<LinkedWorktreeRef>, String> {
    let workspace = workspace_repository::get_detail(&state.db, &input.workspace_id)?
        .ok_or_else(|| format!("Workspace {} was not found", input.workspace_id))?;
    let target = repository_repository::get_worktree(&state.db, &input.worktree_id)?
        .ok_or_else(|| format!("Worktree {} was not found", input.worktree_id))?;
    if let Some(primary_repo_id) = workspace.summary.repository_id.as_ref() {
        if primary_repo_id == &target.repo_id {
            return Err(
                "Linked worktrees must come from a different repository than the primary workspace"
                    .to_string(),
            );
        }
    }
    let primary_path = workspace
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or(workspace.worktree_path);
    if primary_path == target.path {
        return Err("Cannot link the workspace primary worktree to itself".to_string());
    }
    workspace_repository::attach_linked_worktree(
        &state.db,
        &input.workspace_id,
        &input.worktree_id,
    )?;
    workspace_repository::list_linked_worktrees_for_workspace(&state.db, &input.workspace_id)
}

pub fn detach_workspace_linked_worktree(
    state: &AppState,
    workspace_id: &str,
    worktree_id: &str,
) -> Result<Vec<LinkedWorktreeRef>, String> {
    workspace_repository::detach_linked_worktree(&state.db, workspace_id, worktree_id)?;
    workspace_repository::list_linked_worktrees_for_workspace(&state.db, workspace_id)
}

pub fn list_workspace_linked_worktrees(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<LinkedWorktreeRef>, String> {
    workspace_repository::list_linked_worktrees_for_workspace(&state.db, workspace_id)
}

pub fn open_in_cursor(state: &AppState, workspace_id: &str) -> Result<(), String> {
    let detail = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let target_path = detail
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or(detail.worktree_path.clone());
    let path = Path::new(&target_path);
    if !path.exists() || !path.is_dir() {
        return Err(format!(
            "Cannot open workspace in Cursor because path is unavailable: {}",
            path.display()
        ));
    }

    let output = Command::new("cursor").arg(path).output().map_err(|err| {
        format!("Failed to launch Cursor. Ensure 'cursor' CLI is installed and on PATH: {err}")
    })?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            "Cursor CLI returned a non-zero exit code while opening the workspace".to_string()
        } else {
            format!("Cursor failed to open workspace: {stderr}")
        })
    }
}

pub fn open_worktree_in_cursor(path: &str) -> Result<(), String> {
    let path = Path::new(path);
    if !path.exists() || !path.is_dir() {
        return Err(format!(
            "Cannot open linked worktree in Cursor because path is unavailable: {}",
            path.display()
        ));
    }
    let output = Command::new("cursor")
        .arg(path)
        .output()
        .map_err(|err| format!("Failed to launch Cursor: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            "Cursor CLI returned a non-zero exit code".to_string()
        } else {
            format!("Cursor failed to open linked worktree: {stderr}")
        })
    }
}

/// Renames a Mnemonic-managed workspace and its branch to
/// `<same prefix>/<new_name>`, adding `-2`, `-3`, ... when that branch name is
/// taken. Returns the updated summary, or None when renaming is not safe: the
/// stored name is no longer `expected_name`, a PR is recorded, the worktree
/// has moved off the workspace branch, or the branch has been pushed.
pub fn rename_unpublished_workspace(
    state: &AppState,
    workspace_id: &str,
    expected_name: &str,
    new_name: &str,
    branch_slug: &str,
) -> Result<Option<WorkspaceSummary>, String> {
    let summary = workspace_repository::get(&state.db, workspace_id)?;
    if summary.name != expected_name
        || !summary.worktree_managed_by_mnemonic
        || summary.pr_status.is_some()
        || summary.pr_number.is_some()
    {
        return Ok(None);
    }
    let Some(root) = summary
        .workspace_root_path
        .as_deref()
        .map(Path::new)
        .filter(|path| path.is_dir())
    else {
        return Ok(None);
    };
    let old_branch = summary.branch.clone();
    if git_worktree_service::current_branch(root)?.as_deref() != Some(old_branch.as_str()) {
        return Ok(None);
    }
    if git_worktree_service::branch_is_published(root, &old_branch)? {
        return Ok(None);
    }

    let prefix = old_branch
        .rsplit_once('/')
        .map(|(prefix, _)| format!("{prefix}/"))
        .unwrap_or_default();
    let candidates = std::iter::once(branch_slug.to_string())
        .chain((2..100).map(|suffix| format!("{branch_slug}-{suffix}")));
    let mut chosen_slug = None;
    for candidate in candidates {
        if !git_worktree_service::branch_name_taken(root, &format!("{prefix}{candidate}"))? {
            chosen_slug = Some(candidate);
            break;
        }
    }
    let Some(slug) = chosen_slug else {
        return Ok(None);
    };
    let new_branch = format!("{prefix}{slug}");

    git_worktree_service::rename_branch(root, &old_branch, &new_branch)?;
    if let Err(err) =
        workspace_repository::update_name_and_branch(&state.db, workspace_id, new_name, &new_branch)
    {
        if let Err(revert_err) = git_worktree_service::rename_branch(root, &new_branch, &old_branch)
        {
            log::error!("Failed to restore branch {old_branch} after rename failed: {revert_err}");
        }
        return Err(err);
    }

    if let Err(err) = activity_repository::record(
        &state.db,
        workspace_id,
        &summary.repo,
        Some(&new_branch),
        "Branch renamed",
        "info",
        Some(&format!("{old_branch} -> {new_branch}")),
    ) {
        log::error!("Failed to record branch rename: {err}");
    }
    if let Some(repository_id) = summary.repository_id.as_deref() {
        if let Err(err) = repo_scanner_service::refresh_repository_by_id(state, repository_id) {
            log::error!("Failed to refresh repository after branch rename: {err}");
        }
    }

    workspace_repository::get(&state.db, workspace_id).map(Some)
}

/// Renames a workspace by hand. The branch is renamed with it while that is
/// still safe (see `rename_unpublished_workspace`); once the branch is pushed
/// or has a PR, only the name Mnemonic shows changes.
pub fn rename_workspace(
    state: &AppState,
    workspace_id: &str,
    new_name: &str,
) -> Result<WorkspaceSummary, String> {
    let name = new_name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err("A workspace name can't be empty".to_string());
    }
    let summary = workspace_repository::get(&state.db, workspace_id)?;
    if name == summary.name {
        return Ok(summary);
    }
    let slug = branch_slug(&name);
    if !slug.is_empty() {
        if let Some(renamed) =
            rename_unpublished_workspace(state, workspace_id, &summary.name, &name, &slug)?
        {
            return Ok(renamed);
        }
    }
    workspace_repository::update_name_and_branch(&state.db, workspace_id, &name, &summary.branch)?;
    workspace_repository::get(&state.db, workspace_id)
}

/// A workspace name as a branch's last segment: lowercase letters and
/// digits joined by dashes, as in "Fix the login page" to "fix-the-login-page".
pub fn branch_slug(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Settings key holding the pairs of workspaces that are linked together.
const WORKSPACE_LINKS_KEY: &str = "workspace_links";

/// Every link between two workspaces. A link has no direction: the pair is
/// stored once and applies to both.
pub fn list_workspace_links(state: &AppState) -> Result<Vec<(String, String)>, String> {
    let Some(json) = settings_repository::get_value(&state.db, WORKSPACE_LINKS_KEY)? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&json).map_err(|err| format!("Failed to read workspace links: {err}"))
}

fn save_workspace_links(state: &AppState, links: &[(String, String)]) -> Result<(), String> {
    let json = serde_json::to_string(links)
        .map_err(|err| format!("Failed to save workspace links: {err}"))?;
    settings_repository::set_value(&state.db, WORKSPACE_LINKS_KEY, &json)
}

fn is_link_between(link: &(String, String), workspace_id: &str, other_workspace_id: &str) -> bool {
    (link.0 == workspace_id && link.1 == other_workspace_id)
        || (link.0 == other_workspace_id && link.1 == workspace_id)
}

/// The ids of the workspaces linked to `workspace_id`.
pub fn linked_workspace_ids<'a>(links: &'a [(String, String)], workspace_id: &str) -> Vec<&'a str> {
    links
        .iter()
        .filter_map(|(first, second)| {
            if first == workspace_id {
                Some(second.as_str())
            } else if second == workspace_id {
                Some(first.as_str())
            } else {
                None
            }
        })
        .collect()
}

/// Links two workspaces, so the agents working in each can read and edit the
/// other's files. Typically a frontend and a backend changing together.
pub fn link_workspaces(
    state: &AppState,
    workspace_id: &str,
    other_workspace_id: &str,
) -> Result<(), String> {
    if workspace_id == other_workspace_id {
        return Err("A workspace cannot be linked to itself".to_string());
    }
    workspace_repository::get(&state.db, workspace_id)?;
    workspace_repository::get(&state.db, other_workspace_id)?;

    let mut links = list_workspace_links(state)?;
    if links
        .iter()
        .any(|link| is_link_between(link, workspace_id, other_workspace_id))
    {
        return Ok(());
    }
    links.push((workspace_id.to_string(), other_workspace_id.to_string()));
    save_workspace_links(state, &links)
}

pub fn unlink_workspaces(
    state: &AppState,
    workspace_id: &str,
    other_workspace_id: &str,
) -> Result<(), String> {
    let mut links = list_workspace_links(state)?;
    links.retain(|link| !is_link_between(link, workspace_id, other_workspace_id));
    save_workspace_links(state, &links)
}

/// The workspaces linked to this one. A link to a workspace that no longer
/// exists is skipped rather than removed, so restoring an archived workspace
/// brings its links back.
pub fn linked_workspaces(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<WorkspaceSummary>, String> {
    let links = list_workspace_links(state)?;
    let linked_ids = linked_workspace_ids(&links, workspace_id);
    if linked_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(workspace_repository::list(&state.db)?
        .into_iter()
        .filter(|workspace| linked_ids.contains(&workspace.id.as_str()))
        .collect())
}

pub fn list_archived_workspaces(state: &AppState) -> Result<Vec<ArchivedWorkspace>, String> {
    ops::list_archived_workspaces(state)
}

pub fn restore_archived_workspace(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceSummary, String> {
    ops::restore_archived_workspace(state, workspace_id)
}

pub fn pull_workspace_branch(state: &AppState, workspace_id: &str) -> Result<String, String> {
    let detail = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let root = detail
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or(detail.worktree_path);
    let root_path = Path::new(&root);
    if !root_path.exists() || !root_path.is_dir() {
        return Err(format!(
            "Workspace path is unavailable: {}",
            root_path.display()
        ));
    }
    if has_unmerged_conflicts(root_path)? {
        return Err(
            "Cannot pull because Git reports unresolved conflicts. Resolve or abort first."
                .to_string(),
        );
    }
    if is_dirty(root_path)? {
        return Err(
            "Cannot pull because the workspace has uncommitted changes. Commit/stash first."
                .to_string(),
        );
    }
    let base_branch = detail.base_branch;
    run_git(root_path, &["fetch", "origin"])?;
    let behind = behind_count(root_path, &base_branch)?;
    if behind == 0 {
        return Ok(format!("Already up to date with origin/{base_branch}."));
    }
    if let Err(err) = run_git(root_path, &["rebase", &format!("origin/{base_branch}")]) {
        let _ = run_git(root_path, &["rebase", "--abort"]);
        return Err(format!("Pull/rebase failed: {err}"));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());
    let _ = workspace_repository::update_last_rebase(&state.db, workspace_id, &now);
    Ok(format!(
        "Rebased onto origin/{base_branch} ({behind} commit(s) behind)."
    ))
}

fn run_git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(super::environment_service::git_credential_args())
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|err| format!("Failed to run git {}: {err}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            stderr
        })
    }
}

fn has_unmerged_conflicts(root: &Path) -> Result<bool, String> {
    let output = run_git(root, &["diff", "--name-only", "--diff-filter=U"])?;
    Ok(!output.trim().is_empty())
}

fn is_dirty(root: &Path) -> Result<bool, String> {
    let output = run_git(root, &["status", "--porcelain"])?;
    Ok(!output.trim().is_empty())
}

fn behind_count(root: &Path, base_branch: &str) -> Result<u32, String> {
    let output = run_git(
        root,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...origin/{base_branch}"),
        ],
    )?;
    let (_, behind) = parse_ahead_behind_counts(&output);
    Ok(behind)
}

fn parse_ahead_behind_counts(output: &str) -> (u32, u32) {
    let mut parts = output.split_whitespace();
    let ahead = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let behind = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    (ahead, behind)
}

#[cfg(test)]
mod tests {
    use super::{linked_workspace_ids, parse_ahead_behind_counts};

    #[test]
    fn a_link_is_seen_from_both_workspaces() {
        let links = vec![
            ("frontend".to_string(), "backend".to_string()),
            ("docs".to_string(), "frontend".to_string()),
        ];
        assert_eq!(linked_workspace_ids(&links, "frontend"), ["backend", "docs"]);
        assert_eq!(linked_workspace_ids(&links, "backend"), ["frontend"]);
        assert!(linked_workspace_ids(&links, "unrelated").is_empty());
    }

    #[test]
    fn parses_ahead_behind_counts() {
        assert_eq!(parse_ahead_behind_counts("2 5"), (2, 5));
        assert_eq!(parse_ahead_behind_counts("0\t1"), (0, 1));
    }

    #[test]
    fn invalid_ahead_behind_output_defaults_to_zero() {
        assert_eq!(parse_ahead_behind_counts("n/a"), (0, 0));
        assert_eq!(parse_ahead_behind_counts(""), (0, 0));
    }
}
