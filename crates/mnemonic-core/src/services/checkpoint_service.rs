use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{
    WorkspaceCheckpoint, WorkspaceCheckpointBranchResult, WorkspaceCheckpointDeleteResult,
    WorkspaceCheckpointDiff, WorkspaceCheckpointRestorePlan, WorkspaceCheckpointRestoreResult,
};
use crate::repositories::{activity_repository, workspace_repository};
use crate::state::AppState;

const CHECKPOINT_SUBJECT_MARKER: &str = "mnemonic checkpoint: ";
const TURN_LABEL_MAX_CHARS: usize = 72;

/// Serialises snapshot and restore work so concurrent sends can't race the
/// "same as the newest checkpoint" check or restore mid-snapshot.
static CHECKPOINT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, PartialEq, Eq)]
enum SnapshotMode {
    OnlyIfDirty,
    Always,
}

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
    let pattern = format!("refs/mnemonic/checkpoints/{workspace_id}");
    let output = git(
        &root,
        &[
            "for-each-ref",
            "--sort=-refname",
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
    ensure_checkpoint_ref_exists(&root, reference)?;
    let status = git(&root, &["status", "--porcelain"])?;
    let current_dirty = !status.trim().is_empty();
    let changed_file_count = non_empty_lines(&status).count();
    let checkpoint_file_count = non_empty_lines(&git(
        &root,
        &["diff", "--name-only", &format!("{reference}^1"), reference],
    )?)
    .count();
    let files_to_restore = non_empty_lines(&git(
        &root,
        &["diff", "--relative", "--name-only", reference],
    )?)
    .map(str::to_string)
    .collect::<Vec<_>>();
    let untracked = untracked_files(&root)?;
    let overwritten = untracked_files_in_checkpoint(&root, reference, &untracked)?;

    let mut warnings = Vec::new();
    if !overwritten.is_empty() {
        warnings.push(format!(
            "Restore is blocked: untracked files would be overwritten ({}). Move or commit them first.",
            overwritten.join(", ")
        ));
    }
    if !untracked.is_empty() {
        warnings.push(format!(
            "{} untracked file(s) are not part of checkpoints and will be left as they are.",
            untracked.len()
        ));
    }
    if files_to_restore.is_empty() {
        warnings.push("Tracked files already match this checkpoint.".to_string());
    }

    Ok(WorkspaceCheckpointRestorePlan {
        workspace_id: workspace_id.to_string(),
        reference: reference.to_string(),
        current_dirty,
        changed_file_count,
        checkpoint_file_count,
        files_to_restore,
        overwritten_untracked_files: overwritten,
        warnings,
        steps: vec![
            "Save current uncommitted changes to tracked files as a new checkpoint.".to_string(),
            format!("Restore tracked files and the index from {reference}."),
            "Leave untracked files, commits, and the branch as they are.".to_string(),
            "Keep every checkpoint ref for manual recovery.".to_string(),
        ],
    })
}

/// Rewrites tracked files and the index to the checkpoint's tree. Current
/// uncommitted tracked changes are saved as a safety checkpoint first, and
/// restore refuses when it would overwrite an untracked file, so nothing is
/// lost that can't be restored again.
pub fn restore_workspace_checkpoint(
    state: &AppState,
    workspace_id: &str,
    reference: &str,
) -> Result<WorkspaceCheckpointRestoreResult, String> {
    let root = workspace_root_path(state, workspace_id)?;
    let safety_reference = {
        let _guard = lock_checkpoints();
        restore_checkpoint_at(&root, workspace_id, reference)?
    };

    if let Ok(Some(workspace)) = workspace_repository::get_detail(&state.db, workspace_id) {
        if let Some(safety_reference) = &safety_reference {
            record_activity(
                state,
                workspace_id,
                &workspace.summary.repo,
                &workspace.summary.branch,
                "Checkpoint created",
                "info",
                &format!("Reason: before restoring {reference}; ref: {safety_reference}"),
            );
        }
        record_activity(
            state,
            workspace_id,
            &workspace.summary.repo,
            &workspace.summary.branch,
            "Checkpoint restored",
            "warning",
            &format!(
                "Ref: {reference}; mode: git restore --staged --worktree; checkpoint preserved."
            ),
        );
    }

    let message = match safety_reference {
        Some(_) => "Checkpoint restored. Your previous changes were saved as a new checkpoint.",
        None => "Checkpoint restored.",
    };
    Ok(WorkspaceCheckpointRestoreResult {
        workspace_id: workspace_id.to_string(),
        reference: reference.to_string(),
        applied: true,
        message: message.to_string(),
    })
}

fn restore_checkpoint_at(
    root: &Path,
    workspace_id: &str,
    reference: &str,
) -> Result<Option<String>, String> {
    validate_checkpoint_ref(workspace_id, reference)?;
    ensure_checkpoint_ref_exists(root, reference)?;
    let untracked = untracked_files(root)?;
    let overwritten = untracked_files_in_checkpoint(root, reference, &untracked)?;
    if !overwritten.is_empty() {
        return Err(format!(
            "Refusing to restore: untracked files would be overwritten ({}). Move or commit them first.",
            overwritten.join(", ")
        ));
    }

    let safety_reference = snapshot_worktree(
        root,
        workspace_id,
        &format!("before restoring {reference}"),
        SnapshotMode::OnlyIfDirty,
    )?;

    git(
        root,
        &[
            "restore",
            "--source",
            reference,
            "--staged",
            "--worktree",
            ".",
        ],
    )?;
    Ok(safety_reference)
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
    let reference = {
        let _guard = lock_checkpoints();
        snapshot_worktree(&root, workspace_id, reason, SnapshotMode::OnlyIfDirty)?
    };
    if let Some(reference) = &reference {
        record_checkpoint_created(state, workspace_id, reason, reference);
    }
    Ok(reference)
}

/// Checkpoints the worktree before an agent turn, labelled with a preview of
/// the message. Unlike `create_checkpoint_if_dirty`, a clean worktree is
/// checkpointed too (as its HEAD tree) so the first turn can be reverted.
/// Returns None when tracked files match the newest checkpoint.
pub fn create_turn_checkpoint(
    state: &AppState,
    workspace_id: &str,
    message: &str,
) -> Result<Option<String>, String> {
    let root = workspace_root_path(state, workspace_id)?;
    let label = turn_label(message);
    let reference = {
        let _guard = lock_checkpoints();
        snapshot_worktree(&root, workspace_id, &label, SnapshotMode::Always)?
    };
    if let Some(reference) = &reference {
        record_checkpoint_created(state, workspace_id, &label, reference);
    }
    Ok(reference)
}

/// The user-facing label stored in a checkpoint commit's subject.
pub fn checkpoint_label(subject: &str) -> &str {
    subject
        .find(CHECKPOINT_SUBJECT_MARKER)
        .and_then(|index| subject.get(index + CHECKPOINT_SUBJECT_MARKER.len()..))
        .unwrap_or(subject)
}

/// Everything that changed in tracked files since the checkpoint, as a
/// patch against the current worktree (what a restore would undo).
pub fn get_workspace_changes_since_checkpoint(
    state: &AppState,
    workspace_id: &str,
    reference: &str,
) -> Result<WorkspaceCheckpointDiff, String> {
    let root = workspace_root_path(state, workspace_id)?;
    validate_checkpoint_ref(workspace_id, reference)?;
    ensure_checkpoint_ref_exists(&root, reference)?;
    let diff = git(&root, &["diff", "--relative", "--no-ext-diff", reference])?;
    Ok(WorkspaceCheckpointDiff {
        workspace_id: workspace_id.to_string(),
        reference: reference.to_string(),
        diff,
    })
}

fn turn_label(message: &str) -> String {
    let first_line = message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("agent turn");
    let mut label = first_line
        .chars()
        .take(TURN_LABEL_MAX_CHARS)
        .collect::<String>();
    if first_line.chars().count() > TURN_LABEL_MAX_CHARS || message.trim().lines().count() > 1 {
        label.push_str("...");
    }
    label
}

fn lock_checkpoints() -> std::sync::MutexGuard<'static, ()> {
    CHECKPOINT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Stores the worktree's tracked state under a new checkpoint ref. Dirty
/// worktrees are captured with `git stash create` (no refs, index, or files
/// are changed); a clean one, with `SnapshotMode::Always`, as a commit of the
/// HEAD tree. Untracked files are never captured.
fn snapshot_worktree(
    root: &Path,
    workspace_id: &str,
    reason: &str,
    mode: SnapshotMode,
) -> Result<Option<String>, String> {
    let message = format!("{CHECKPOINT_SUBJECT_MARKER}{reason}");
    let stash_oid = git(root, &["stash", "create", &message])?;
    let oid = if !stash_oid.is_empty() {
        stash_oid
    } else if mode == SnapshotMode::Always {
        git(
            root,
            &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", &message],
        )?
    } else {
        return Ok(None);
    };

    let tree = git(root, &["rev-parse", &format!("{oid}^{{tree}}")])?;
    if newest_checkpoint_tree(root, workspace_id)?.as_deref() == Some(tree.as_str()) {
        return Ok(None);
    }

    let checkpoint_ref = format!(
        "refs/mnemonic/checkpoints/{workspace_id}/{}",
        unique_suffix()
    );
    git(root, &["update-ref", &checkpoint_ref, &oid])?;
    Ok(Some(checkpoint_ref))
}

fn newest_checkpoint_tree(root: &Path, workspace_id: &str) -> Result<Option<String>, String> {
    let output = git(
        root,
        &[
            "for-each-ref",
            "--count=1",
            "--sort=-refname",
            "--sort=-creatordate",
            "--format=%(tree)",
            &format!("refs/mnemonic/checkpoints/{workspace_id}"),
        ],
    )?;
    let tree = non_empty_lines(&output).next().map(str::to_string);
    Ok(tree)
}

fn untracked_files(root: &Path) -> Result<Vec<String>, String> {
    Ok(
        non_empty_lines(&git(root, &["ls-files", "--others", "--exclude-standard"])?)
            .map(str::to_string)
            .collect(),
    )
}

fn untracked_files_in_checkpoint(
    root: &Path,
    reference: &str,
    untracked: &[String],
) -> Result<Vec<String>, String> {
    if untracked.is_empty() {
        return Ok(Vec::new());
    }
    let checkpoint_paths = git(root, &["ls-tree", "-r", "--name-only", reference])?;
    let checkpoint_paths = non_empty_lines(&checkpoint_paths).collect::<HashSet<_>>();
    Ok(untracked
        .iter()
        .filter(|path| checkpoint_paths.contains(path.as_str()))
        .cloned()
        .collect())
}

fn non_empty_lines(output: &str) -> impl Iterator<Item = &str> {
    output.lines().filter(|line| !line.trim().is_empty())
}

fn record_checkpoint_created(state: &AppState, workspace_id: &str, reason: &str, reference: &str) {
    if let Ok(Some(workspace)) = workspace_repository::get_detail(&state.db, workspace_id) {
        record_activity(
            state,
            workspace_id,
            &workspace.summary.repo,
            &workspace.summary.branch,
            "Checkpoint created",
            "info",
            &format!("Reason: {reason}; ref: {reference}"),
        );
    }
}

fn record_activity(
    state: &AppState,
    workspace_id: &str,
    repo: &str,
    branch: &str,
    title: &str,
    level: &str,
    detail: &str,
) {
    if let Err(err) = activity_repository::record(
        &state.db,
        workspace_id,
        repo,
        Some(branch),
        title,
        level,
        Some(detail),
    ) {
        log::warn!(
            target: "mnemonic_lib",
            "failed to record checkpoint activity for workspace {workspace_id}: {err}"
        );
    }
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
    let prefix = format!("refs/mnemonic/checkpoints/{workspace_id}/");
    if !reference.starts_with(&prefix) {
        return Err("Checkpoint does not belong to this workspace".to_string());
    }
    Ok(())
}

fn ensure_checkpoint_ref_exists(root: &Path, reference: &str) -> Result<(), String> {
    git(root, &["rev-parse", "--verify", "--quiet", reference]).map(|_| ())
}

fn validate_branch_name(root: &Path, branch: &str) -> Result<(), String> {
    let branch = branch.trim();
    if branch.is_empty() {
        return Err("Branch name is required".to_string());
    }
    git(root, &["check-ref-format", "--branch", branch]).map(|_| ())
}

fn ensure_branch_does_not_exist(root: &Path, branch: &str) -> Result<(), String> {
    let branch_ref = format!("refs/heads/{branch}");
    match git(root, &["rev-parse", "--verify", "--quiet", &branch_ref]) {
        Ok(_) => Err(format!("Branch {branch} already exists")),
        Err(_) => Ok(()),
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
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
        assert!(validate_checkpoint_ref("ws-1", "refs/mnemonic/checkpoints/ws-1/123").is_ok());
        assert!(validate_checkpoint_ref("ws-1", "refs/mnemonic/checkpoints/ws-2/123").is_err());
        assert!(validate_checkpoint_ref("ws-1", "refs/heads/main").is_err());
    }

    #[test]
    fn restore_reverts_a_turn_and_keeps_current_changes_recoverable() -> Result<(), String> {
        let root = test_repo("restore")?;
        std::fs::write(root.join("a.txt"), "before turn").map_err(|err| err.to_string())?;
        git(&root, &["add", "a.txt"])?;
        git(&root, &["commit", "-m", "initial"])?;

        let before_turn = snapshot_worktree(&root, "ws-1", "first turn", SnapshotMode::Always)?
            .ok_or("a clean worktree should still be checkpointed before a turn")?;
        assert_eq!(
            snapshot_worktree(&root, "ws-1", "second turn", SnapshotMode::Always)?,
            None,
            "an unchanged worktree should not add a duplicate checkpoint"
        );

        std::fs::write(root.join("a.txt"), "agent edit").map_err(|err| err.to_string())?;
        std::fs::write(root.join("new.txt"), "agent file").map_err(|err| err.to_string())?;
        let safety = restore_checkpoint_at(&root, "ws-1", &before_turn)?
            .ok_or("restoring a dirty worktree should save it as a checkpoint first")?;

        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).map_err(|err| err.to_string())?,
            "before turn"
        );
        assert!(
            root.join("new.txt").exists(),
            "untracked files are left alone"
        );
        assert_eq!(
            git(&root, &["show", &format!("{safety}:a.txt")])?,
            "agent edit"
        );
        std::fs::remove_dir_all(&root).map_err(|err| err.to_string())
    }

    #[test]
    fn restore_refuses_to_overwrite_untracked_files() -> Result<(), String> {
        let root = test_repo("untracked")?;
        std::fs::write(root.join("a.txt"), "committed").map_err(|err| err.to_string())?;
        git(&root, &["add", "a.txt"])?;
        git(&root, &["commit", "-m", "initial"])?;
        std::fs::write(root.join("b.txt"), "staged").map_err(|err| err.to_string())?;
        git(&root, &["add", "b.txt"])?;
        let checkpoint = snapshot_worktree(&root, "ws-1", "turn", SnapshotMode::Always)?
            .ok_or("a staged file should produce a checkpoint")?;

        git(&root, &["rm", "--cached", "--quiet", "b.txt"])?;
        std::fs::write(root.join("b.txt"), "local only").map_err(|err| err.to_string())?;

        let error = restore_checkpoint_at(&root, "ws-1", &checkpoint)
            .err()
            .ok_or("restore should refuse")?;
        assert!(error.contains("b.txt"), "unexpected error: {error}");
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).map_err(|err| err.to_string())?,
            "local only"
        );
        std::fs::remove_dir_all(&root).map_err(|err| err.to_string())
    }

    fn test_repo(tag: &str) -> Result<PathBuf, String> {
        let root = std::env::temp_dir().join(format!(
            "mnemonic-checkpoint-test-{tag}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&root).map_err(|err| err.to_string())?;
        git(&root, &["init", "--quiet"])?;
        git(
            &root,
            &["config", "user.email", "mnemonic-tests@example.com"],
        )?;
        git(&root, &["config", "user.name", "Mnemonic Tests"])?;
        Ok(root)
    }
}
