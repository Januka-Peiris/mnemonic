use std::time::{Duration, SystemTime, UNIX_EPOCH};


use crate::repositories::{activity_repository, settings_repository, workspace_repository};
use crate::state::AppState;

/// How often to attempt auto-rebase across all active workspaces.
const REBASE_INTERVAL_SECS: u64 = 30 * 60;

/// Spawn a background thread that periodically rebases every active workspace
/// against its base branch. On conflict the rebase is aborted and a
/// `mn://workspace-rebase-conflict` event is emitted.
pub fn start_auto_rebase_loop(state: AppState) {
    std::thread::spawn(move || {
        // Initial delay so startup is unaffected.
        std::thread::sleep(Duration::from_secs(REBASE_INTERVAL_SECS));
        loop {
            if let Err(err) = run_rebase_pass(&state) {
                log::warn!(target: "mnemonic_lib", "auto-rebase pass error: {err}");
            }
            std::thread::sleep(Duration::from_secs(REBASE_INTERVAL_SECS));
        }
    });
}

fn run_rebase_pass(state: &AppState) -> Result<(), String> {
    let enabled = settings_repository::get_value(&state.db, "auto_rebase_enabled")
        .ok()
        .flatten()
        .map(|value| value == "true")
        .unwrap_or(false);
    if !enabled {
        return Ok(());
    }

    let workspaces = workspace_repository::list(&state.db)?;

    for workspace in workspaces {
        if workspace.status == "Merged" {
            continue;
        }

        let detail = match workspace_repository::get_detail(&state.db, &workspace.id)? {
            Some(d) => d,
            None => continue,
        };

        if detail.worktree_path.is_empty() {
            continue;
        }

        let base_branch = workspace.branch_health.base_branch.clone();
        let base_branch = if base_branch.is_empty() {
            "main".to_string()
        } else {
            base_branch
        };

        let fetch_ok = std::process::Command::new("git")
            .args(super::environment_service::git_credential_args())
            .args(["fetch", "origin"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&detail.worktree_path)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);

        if !fetch_ok {
            log::warn!(
                target: "mnemonic_lib",
                "auto-rebase: git fetch failed for workspace {} ({})",
                workspace.id,
                detail.worktree_path,
            );
            let _ = activity_repository::record(
                &state.db,
                &workspace.id,
                &workspace.repo,
                Some(&workspace.branch),
                "Auto-rebase skipped",
                "warning",
                Some("Could not fetch origin before rebase. No git changes were made."),
            );
            continue;
        }

        let dirty_file_count = dirty_file_count(&detail.worktree_path).unwrap_or(0);
        if dirty_file_count > 0 {
            let _ = activity_repository::record(
                &state.db,
                &workspace.id,
                &workspace.repo,
                Some(&workspace.branch),
                "Auto-rebase skipped",
                "warning",
                Some(&format!(
                    "Workspace has {dirty_file_count} uncommitted file(s). No background git changes were made."
                )),
            );
            continue;
        }

        let previous_head =
            git_short_head(&detail.worktree_path).unwrap_or_else(|| "unknown".to_string());

        let rebase_output = std::process::Command::new("git")
            .args(["rebase", &format!("origin/{base_branch}")])
            .current_dir(&detail.worktree_path)
            .output();

        match rebase_output {
            Ok(out) if out.status.success() => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs().to_string())
                    .unwrap_or_else(|_| "0".to_string());
                log::info!(
                    target: "mnemonic_lib",
                    "auto-rebase: workspace {} rebased onto origin/{base_branch}",
                    workspace.id,
                );
                let _ = workspace_repository::update_last_rebase(&state.db, &workspace.id, &now);
                let new_head =
                    git_short_head(&detail.worktree_path).unwrap_or_else(|| "unknown".to_string());
                let _ = activity_repository::record(
                    &state.db,
                    &workspace.id,
                    &workspace.repo,
                    Some(&workspace.branch),
                    "Auto-rebase succeeded",
                    "success",
                    Some(&format_auto_rebase_success_details(
                        &base_branch,
                        &previous_head,
                        &new_head,
                    )),
                );
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                log::warn!(
                    target: "mnemonic_lib",
                    "auto-rebase: conflict for workspace {}: {stderr}",
                    workspace.id,
                );
                let _ = std::process::Command::new("git")
                    .args(["rebase", "--abort"])
                    .current_dir(&detail.worktree_path)
                    .output();
                let _ = activity_repository::record(
                    &state.db,
                    &workspace.id,
                    &workspace.repo,
                    Some(&workspace.branch),
                    "Auto-rebase conflict",
                    "warning",
                    Some(&format!(
                        "Conflict rebasing onto origin/{base_branch} from {previous_head} — rebase aborted."
                    )),
                );
                state.event_emitter.emit(
                    crate::events::WORKSPACE_REBASE_CONFLICT,
                    &serde_json::json!({
                        "workspaceId": workspace.id,
                        "workspaceName": workspace.name,
                        "branch": workspace.branch,
                        "baseBranch": base_branch,
                    }),
                );
            }
            Err(err) => {
                log::warn!(
                    target: "mnemonic_lib",
                    "auto-rebase: git rebase failed to launch for workspace {}: {err}",
                    workspace.id,
                );
            }
        }
    }

    Ok(())
}

fn dirty_file_count(worktree_path: &str) -> Result<usize, String> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_path)
        .output()
        .map_err(|err| format!("Failed to inspect workspace status: {err}"))?;
    if !output.status.success() {
        return Err("git status failed".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count())
}

fn git_short_head(worktree_path: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(worktree_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if head.is_empty() {
        None
    } else {
        Some(head)
    }
}

fn format_auto_rebase_success_details(
    base_branch: &str,
    previous_head: &str,
    new_head: &str,
) -> String {
    if previous_head == new_head {
        format!("Already up to date with origin/{base_branch}; head stayed at {new_head}.")
    } else {
        format!(
            "Rebased onto origin/{base_branch}; head moved {previous_head} → {new_head}. To reverse manually, run `git reset --hard {previous_head}` from the workspace."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_noop_auto_rebase_as_unchanged() {
        let details = format_auto_rebase_success_details("main", "abc123", "abc123");
        assert!(details.contains("Already up to date"));
        assert!(details.contains("abc123"));
    }

    #[test]
    fn formats_moving_auto_rebase_with_manual_reversal_hint() {
        let details = format_auto_rebase_success_details("main", "abc123", "def456");
        assert!(details.contains("abc123"));
        assert!(details.contains("def456"));
        assert!(details.contains("git reset --hard abc123"));
    }
}
