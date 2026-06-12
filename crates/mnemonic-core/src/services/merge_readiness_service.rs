use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{PreFlightCheck, WorkspaceMergeReadiness};
use crate::repositories::{agent_run_repository, merge_readiness_repository, workspace_repository};
use crate::services::{git_review_service, review_summary_service};
use crate::state::AppState;

pub fn get_workspace_merge_readiness(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceMergeReadiness, String> {
    if let Some(readiness) = merge_readiness_repository::get(&state.db, workspace_id)? {
        return Ok(readiness);
    }
    refresh_workspace_merge_readiness(state, workspace_id)
}

pub fn refresh_workspace_merge_readiness(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceMergeReadiness, String> {
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let root = workspace
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or_else(|| workspace.worktree_path.clone());

    let mut reasons = Vec::new();
    let mut warnings = Vec::new();
    let mut blockers = Vec::new();
    let mut ahead_count = None;
    let mut behind_count = None;
    let mut pre_flight_checks = Vec::new();

    let root_path = Path::new(&root);
    if !root_path.exists() {
        blockers.push(format!("Workspace path does not exist: {root}"));
    } else if git(root_path, &["rev-parse", "--is-inside-work-tree"])
        .map(|value| value.trim() == "true")
        .unwrap_or(false)
    {
        reasons.push("Workspace path is a valid git worktree.".to_string());

        // --- 1. Git Diff Check ---
        let git_check = git(root_path, &["diff", "--check"]);
        pre_flight_checks.push(PreFlightCheck {
            id: "git_diff_check".to_string(),
            label: "Git Check".to_string(),
            status: if git_check.is_ok() {
                "pass".to_string()
            } else {
                "fail".to_string()
            },
            message: git_check.unwrap_or_else(|e| e),
        });

        let base = workspace.summary.branch_health.base_branch.clone();
        if !base.trim().is_empty() {
            if let Ok((ahead, behind)) = ahead_behind(root_path, &base) {
                ahead_count = Some(ahead);
                behind_count = Some(behind);
                if ahead > 0 {
                    reasons.push(format!("Branch is {ahead} commit(s) ahead of {base}."));
                } else {
                    warnings.push(format!("Branch has no commits ahead of {base}."));
                }
                if behind > 0 {
                    warnings.push(format!("Branch is {behind} commit(s) behind {base}."));
                }
            } else {
                warnings.push(format!("Could not compare branch with base '{base}'."));
            }
        }
        if has_unmerged_conflicts(root_path) {
            blockers.push("Git reports unresolved merge conflicts.".to_string());
            pre_flight_checks.push(PreFlightCheck {
                id: "merge_conflicts".to_string(),
                label: "Merge Conflicts".to_string(),
                status: "fail".to_string(),
                message: "Unresolved conflicts detected. Fix them before shipping.".to_string(),
            });
        } else {
            pre_flight_checks.push(PreFlightCheck {
                id: "merge_conflicts".to_string(),
                label: "Merge Conflicts".to_string(),
                status: "pass".to_string(),
                message: "No unresolved conflicts.".to_string(),
            });
        }
    } else {
        blockers.push(format!("Workspace path is not a git worktree: {root}"));
    }

    // --- 2. Lint Detection (Simple) ---
    if root_path.join("package.json").exists() {
        pre_flight_checks.push(PreFlightCheck {
            id: "lint_check".to_string(),
            label: "Lint Available".to_string(),
            status: "warning".to_string(),
            message: "Detected Node project. Manual linting recommended before ship.".to_string(),
        });
    }

    let changed_files = git_review_service::get_workspace_changed_files(state, workspace_id)
        .unwrap_or_else(|err| {
            warnings.push(format!("Could not inspect changed files: {err}"));
            Vec::new()
        });
    if changed_files.is_empty() {
        warnings.push("No changed files detected.".to_string());
    } else {
        reasons.push(format!("{} changed file(s) detected.", changed_files.len()));
    }

    let latest_run = agent_run_repository::list_runs_for_workspace(&state.db, workspace_id)?
        .into_iter()
        .next();
    let active_run_status = latest_run.as_ref().map(|run| run.status.clone());
    match active_run_status.as_deref() {
        Some("running") => blockers.push("An agent run is still running.".to_string()),
        Some("failed") => warnings.push("Latest agent run failed.".to_string()),
        Some("stopped") => {
            warnings.push("Latest agent run was stopped before completion.".to_string())
        }
        Some("succeeded") => reasons.push("Latest agent run succeeded.".to_string()),
        Some(other) => warnings.push(format!("Latest agent run status is {other}.")),
        None => warnings.push("No agent run has been recorded for this workspace.".to_string()),
    }

    let review_summary =
        review_summary_service::refresh_workspace_review_summary(state, workspace_id).ok();
    let review_risk_level = review_summary
        .as_ref()
        .map(|summary| summary.risk_level.clone());
    match review_risk_level.as_deref() {
        Some("high") => warnings.push("Review summary reports high risk.".to_string()),
        Some("medium") => warnings.push("Review summary reports medium risk.".to_string()),
        Some("low") => reasons.push("Review summary reports low risk.".to_string()),
        _ => warnings.push("No review risk score is available.".to_string()),
    }

    reasons.extend(blockers.iter().cloned());
    reasons.sort();
    reasons.dedup();
    warnings.sort();
    warnings.dedup();

    let readiness_level = if !blockers.is_empty() {
        "blocked"
    } else if warnings.is_empty()
        || warnings
            .iter()
            .all(|warning| warning.contains("No agent run"))
    {
        "ready"
    } else {
        "caution"
    }
    .to_string();

    let readiness = WorkspaceMergeReadiness {
        workspace_id: workspace_id.to_string(),
        merge_ready: readiness_level == "ready",
        readiness_level,
        reasons,
        warnings,
        ahead_count,
        behind_count,
        active_run_status,
        review_risk_level,
        pre_flight_checks,
        generated_at: timestamp(),
    };
    merge_readiness_repository::upsert(&state.db, &readiness)?;
    Ok(readiness)
}

fn ahead_behind(root: &Path, base: &str) -> Result<(u32, u32), String> {
    let output = git(
        root,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{base}...HEAD"),
        ],
    )?;
    let mut parts = output.split_whitespace();
    let behind = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let ahead = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    Ok((ahead, behind))
}

fn has_unmerged_conflicts(root: &Path) -> bool {
    git(root, &["diff", "--name-only", "--diff-filter=U"])
        .map(|output| !output.trim().is_empty())
        .unwrap_or(false)
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

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
