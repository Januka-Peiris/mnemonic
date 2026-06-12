use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{
    WorkspaceCheckpoint, WorkspaceCheckpointBranchResult, WorkspaceCheckpointDeleteResult,
    WorkspaceCheckpointDiff, WorkspaceCheckpointRestorePlan, WorkspaceCheckpointRestoreResult,
};
use crate::repositories::{activity_repository, workspace_repository};
use crate::state::AppState;

pub fn create_workspace_checkpoint(
    state: &AppState,
    workspace_id: &str,
    reason: Option<&str>,
) -> Result<Option<WorkspaceCheckpoint>, String> {
    let reference =
        create_checkpoint_if_dirty(state, workspace_id, reason.unwrap_or("manual checkpoint"))?;
    match reference {
        Some(reference) => Ok(list_workspace_checkpoints(state, workspace_id)?
            .into_iter()
            .find(|checkpoint| checkpoint.reference == reference)),
        None => Ok(None),
    }
}

pub fn list_workspace_checkpoints(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<WorkspaceCheckpoint>, String> {
    let root = workspace_root_path(state, workspace_id)?;
    let pattern = format!("refs/forge/checkpoints/{workspace_id}");
    let output = git(
        &root,
        &[
            "for-each-ref",
            "--sort=-creatordate",
            "--format=%(refname)%00%(objectname:short)%00%(creatordate:unix)%00%(subject)",
            &pattern,
        ],
    )?;
    Ok(output
        .lines()
        .filter_map(|line| {
            let parts = line.split('\0').collect::<Vec<_>>();
            if parts.len() < 4 {
                return None;
            }
            Some(WorkspaceCheckpoint {
                workspace_id: workspace_id.to_string(),
                reference: parts[0].to_string(),
                short_oid: parts[1].to_string(),
                created_at: parts[2].to_string(),
                subject: parts[3].to_string(),
            })
        })
        .collect())
}

pub fn get_workspace_checkpoint_diff(
    state: &AppState,
    workspace_id: &str,
    reference: &str,
) -> Result<WorkspaceCheckpointDiff, String> {
    let root = workspace_root_path(state, workspace_id)?;
    validate_checkpoint_ref(workspace_id, reference)?;
    let diff = git(&root, &["show", "--format=", "--no-ext-diff", reference])?;
    Ok(WorkspaceCheckpointDiff {
        workspace_id: workspace_id.to_string(),
        reference: reference.to_string(),
        diff,
    })
}

pub fn get_workspace_checkpoint_restore_plan(
    state: &AppState,
    workspace_id: &str,
    reference: &str,
) -> Result<WorkspaceCheckpointRestorePlan, String> {
    let root = workspace_root_path(state, workspace_id)?;
    validate_checkpoint_ref(workspace_id, reference)?;
    let status = git(&root, &["status", "--porcelain"])?;
    let current_dirty = !status.trim().is_empty();
    let changed_file_count = status
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    let checkpoint_files = git(
        &root,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            reference,
        ],
    )?;
    let checkpoint_file_count = checkpoint_files
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();

    let mut warnings = Vec::new();
    if current_dirty {
        warnings.push("Current workspace has uncommitted changes. Create a fresh checkpoint before restoring.".to_string());
    }
    if checkpoint_file_count == 0 {
        warnings.push("Selected checkpoint does not report changed files.".to_string());
    }

    Ok(WorkspaceCheckpointRestorePlan {
        workspace_id: workspace_id.to_string(),
        reference: reference.to_string(),
        current_dirty,
        changed_file_count,
        checkpoint_file_count,
        warnings,
        steps: vec![
            "Create a new safety checkpoint for the current workspace state.".to_string(),
            format!("Apply checkpoint changes from {reference}."),
            "Refresh changed files, readiness, and the review cockpit.".to_string(),
            "Keep the original checkpoint ref for manual recovery until cleanup.".to_string(),
        ],
    })
}

pub fn restore_workspace_checkpoint(
    state: &AppState,
    workspace_id: &str,
    reference: &str,
) -> Result<WorkspaceCheckpointRestoreResult, String> {
    let root = workspace_root_path(state, workspace_id)?;
    validate_checkpoint_ref(workspace_id, reference)?;
    let plan = get_workspace_checkpoint_restore_plan(state, workspace_id, reference)?;
    if plan.current_dirty {
        return Err(
            "Refusing to restore onto a dirty workspace. Create a checkpoint and clean or commit current changes first."
                .to_string(),
        );
    }

    git(
        &root,
        &[
            "restore",
            "--source",
            reference,
            "--staged",
            "--worktree",
            ".",
        ],
    )?;

    if let Ok(Some(workspace)) = workspace_repository::get_detail(&state.db, workspace_id) {
        let _ = activity_repository::record(
            &state.db,
            workspace_id,
            &workspace.summary.repo,
            Some(&workspace.summary.branch),
            "Checkpoint restored",
            "warning",
            Some(&format!(
                "Ref: {reference}; checkpoint files: {}; mode: git restore --staged --worktree; checkpoint preserved.",
                plan.checkpoint_file_count
            )),
        );
    }

    Ok(WorkspaceCheckpointRestoreResult {
        workspace_id: workspace_id.to_string(),
        reference: reference.to_string(),
        applied: true,
        message: "Checkpoint tree restored into the workspace without committing.".to_string(),
    })
}

pub fn delete_workspace_checkpoint(
    state: &AppState,
    workspace_id: &str,
    reference: &str,
) -> Result<WorkspaceCheckpointDeleteResult, String> {
    let root = workspace_root_path(state, workspace_id)?;
    validate_checkpoint_ref(workspace_id, reference)?;
    ensure_checkpoint_ref_exists(&root, reference)?;

    git(&root, &["update-ref", "-d", reference])?;

    if let Ok(Some(workspace)) = workspace_repository::get_detail(&state.db, workspace_id) {
        let _ = activity_repository::record(
            &state.db,
            workspace_id,
            &workspace.summary.repo,
            Some(&workspace.summary.branch),
            "Checkpoint deleted",
            "warning",
            Some(&format!(
                "Deleted checkpoint ref {reference}. This removes Mnemonic's direct recovery pointer for that checkpoint."
            )),
        );
    }

    Ok(WorkspaceCheckpointDeleteResult {
        workspace_id: workspace_id.to_string(),
        reference: reference.to_string(),
        deleted: true,
        message: "Checkpoint ref deleted. Workspace files were not changed.".to_string(),
    })
}

pub fn create_branch_from_workspace_checkpoint(
    state: &AppState,
    workspace_id: &str,
    reference: &str,
    branch: &str,
) -> Result<WorkspaceCheckpointBranchResult, String> {
    let root = workspace_root_path(state, workspace_id)?;
    validate_checkpoint_ref(workspace_id, reference)?;
    ensure_checkpoint_ref_exists(&root, reference)?;
    validate_branch_name(&root, branch)?;
    ensure_branch_does_not_exist(&root, branch)?;

    let oid = git(
        &root,
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )?;
    let branch_ref = format!("refs/heads/{branch}");
    git(&root, &["update-ref", &branch_ref, &oid])?;

    if let Ok(Some(workspace)) = workspace_repository::get_detail(&state.db, workspace_id) {
        let _ = activity_repository::record(
            &state.db,
            workspace_id,
            &workspace.summary.repo,
            Some(&workspace.summary.branch),
            "Checkpoint branch created",
            "info",
            Some(&format!(
                "Created branch {branch} from checkpoint {reference} at {}. Workspace files were not changed.",
                &oid[..oid.len().min(12)]
            )),
        );
    }

    Ok(WorkspaceCheckpointBranchResult {
        workspace_id: workspace_id.to_string(),
        reference: reference.to_string(),
        branch: branch.to_string(),
        created: true,
        message: format!(
            "Branch {branch} created from checkpoint. Workspace files were not changed."
        ),
    })
}

pub fn create_checkpoint_if_dirty_in_background(
    state: AppState,
    workspace_id: String,
    reason: String,
) {
    thread::spawn(move || {
        if let Err(err) = create_checkpoint_if_dirty(&state, &workspace_id, &reason) {
            log::warn!(
                target: "mnemonic_lib",
                "failed to create background checkpoint for workspace {}: {err}",
                workspace_id
            );
        }
    });
}

pub fn create_checkpoint_if_dirty(
    state: &AppState,
    workspace_id: &str,
    reason: &str,
) -> Result<Option<String>, String> {
    let root = workspace_root_path(state, workspace_id)?;
    let status = git(&root, &["status", "--porcelain"])?;
    let changed_file_count = status
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    if changed_file_count == 0 {
        return Ok(None);
    }

    let suffix = unique_suffix();
    let checkpoint_ref = format!("refs/forge/checkpoints/{workspace_id}/{suffix}");
    let message = format!("mnemonic checkpoint: {reason}");
    let oid = git(&root, &["stash", "create", &message])?;
    let oid = oid.trim().to_string();
    if oid.is_empty() {
        return Ok(None);
    }
    git(&root, &["update-ref", &checkpoint_ref, &oid])?;

    if let Ok(Some(workspace)) = workspace_repository::get_detail(&state.db, workspace_id) {
        let _ = activity_repository::record(
            &state.db,
            workspace_id,
            &workspace.summary.repo,
            Some(&workspace.summary.branch),
            "Checkpoint created",
            "info",
            Some(&format!(
                "Reason: {reason}; files: {changed_file_count}; ref: {checkpoint_ref}; oid: {}",
                &oid[..oid.len().min(12)]
            )),
        );
    }

    Ok(Some(checkpoint_ref))
}

fn workspace_root_path(state: &AppState, workspace_id: &str) -> Result<PathBuf, String> {
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    Ok(PathBuf::from(
        workspace
            .summary
            .workspace_root_path
            .clone()
            .unwrap_or(workspace.worktree_path),
    ))
}

fn validate_checkpoint_ref(workspace_id: &str, reference: &str) -> Result<(), String> {
    let prefix = format!("refs/forge/checkpoints/{workspace_id}/");
    if !reference.starts_with(&prefix) {
        return Err("Checkpoint does not belong to this workspace".to_string());
    }
    Ok(())
}

fn ensure_checkpoint_ref_exists(root: &PathBuf, reference: &str) -> Result<(), String> {
    git(root, &["rev-parse", "--verify", "--quiet", reference]).map(|_| ())
}

fn validate_branch_name(root: &PathBuf, branch: &str) -> Result<(), String> {
    let branch = branch.trim();
    if branch.is_empty() {
        return Err("Branch name is required".to_string());
    }
    git(root, &["check-ref-format", "--branch", branch]).map(|_| ())
}

fn ensure_branch_does_not_exist(root: &PathBuf, branch: &str) -> Result<(), String> {
    let branch_ref = format!("refs/heads/{branch}");
    match git(root, &["rev-parse", "--verify", "--quiet", &branch_ref]) {
        Ok(_) => Err(format!("Branch {branch} already exists")),
        Err(_) => Ok(()),
    }
}

fn git(root: &PathBuf, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(root)
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
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn unique_suffix() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_checkpoint_ref_ownership() {
        assert!(validate_checkpoint_ref("ws-1", "refs/forge/checkpoints/ws-1/123").is_ok());
        assert!(validate_checkpoint_ref("ws-1", "refs/forge/checkpoints/ws-2/123").is_err());
        assert!(validate_checkpoint_ref("ws-1", "refs/heads/main").is_err());
    }
}
