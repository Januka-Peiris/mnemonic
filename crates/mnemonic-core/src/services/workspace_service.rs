use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{
    AttachLinkedWorktreeInput, CreateChildWorkspaceInput, CreateWorkspaceInput,
    DiscoveredRepository, LinkedWorktreeRef, RepositoryWorkspaceOptions, WorkspaceDetail,
    WorkspaceSummary,
};
use crate::repositories::{repository_repository, workspace_repository};
use crate::services::{git_review_service, git_worktree_service};
use crate::state::AppState;

mod ops;

pub fn list_workspaces(state: &AppState) -> Result<Vec<WorkspaceSummary>, String> {
    let workspaces = workspace_repository::list(&state.db)?;
    spawn_changed_files_refresh(state, &workspaces);
    Ok(workspaces)
}

/// Recomputes and caches changed files for active workspaces in the
/// background so the next list_workspaces poll returns fresh diff counters
/// (get_workspace_changed_files persists what it computes). Single-flight
/// guarded; per-workspace errors (missing worktree, deleted repo) are ignored.
fn spawn_changed_files_refresh(state: &AppState, workspaces: &[WorkspaceSummary]) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REFRESHING: AtomicBool = AtomicBool::new(false);

    let ids: Vec<String> = workspaces
        .iter()
        .filter(|workspace| workspace.status != "Merged")
        .map(|workspace| workspace.id.clone())
        .collect();
    if ids.is_empty() || REFRESHING.swap(true, Ordering::SeqCst) {
        return;
    }

    let state = state.clone();
    std::thread::spawn(move || {
        for id in ids {
            let _ = git_review_service::get_workspace_changed_files(&state, &id);
        }
        REFRESHING.store(false, Ordering::SeqCst);
    });
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
    use super::parse_ahead_behind_counts;

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
