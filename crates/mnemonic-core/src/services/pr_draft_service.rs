use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::models::{WorkspacePrCheck, WorkspacePrDraft, WorkspacePrResult, WorkspacePrStatus};
use crate::repositories::{
    activity_repository, agent_run_repository, pr_draft_repository, workspace_repository,
};
use crate::services::{
    environment_service, git_review_service, hook_service, review_summary_service,
};
use crate::state::AppState;

pub fn get_workspace_pr_draft(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspacePrDraft, String> {
    if let Some(draft) = pr_draft_repository::get(&state.db, workspace_id)? {
        return Ok(draft);
    }
    refresh_workspace_pr_draft(state, workspace_id)
}

pub fn refresh_workspace_pr_draft(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspacePrDraft, String> {
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let changed_files =
        git_review_service::get_workspace_changed_files(state, workspace_id).unwrap_or_default();
    let review_summary =
        review_summary_service::refresh_workspace_review_summary(state, workspace_id).ok();
    let latest_run = agent_run_repository::list_runs_for_workspace(&state.db, workspace_id)?
        .into_iter()
        .next();

    let title = workspace.summary.name.trim().to_string();
    let title = if title.is_empty() {
        format!("Update {}", workspace.summary.repo)
    } else {
        title
    };

    let task_context = workspace.summary.current_task.trim();
    let summary = review_summary
        .as_ref()
        .map(|summary| {
            if task_context.is_empty() {
                summary.summary.clone()
            } else {
                format!(
                    "{}

Workspace task: {}",
                    summary.summary, task_context
                )
            }
        })
        .unwrap_or_else(|| {
            let task_sentence = if task_context.is_empty() {
                String::new()
            } else {
                format!(" Task context: {task_context}.")
            };
            format!(
                "Updates {} on branch {}. {} changed file(s) detected.{}",
                workspace.summary.repo,
                workspace.summary.branch,
                changed_files.len(),
                task_sentence,
            )
        });

    let mut key_changes = changed_files
        .iter()
        .take(8)
        .map(|file| {
            let churn = match (file.additions, file.deletions) {
                (Some(a), Some(d)) => format!(" (+{a} -{d})"),
                _ => String::new(),
            };
            format!("{} {}{}", file.status, file.path, churn)
        })
        .collect::<Vec<_>>();
    if key_changes.is_empty() {
        key_changes.push("No local git changes detected yet.".to_string());
    }

    let mut risks = review_summary
        .as_ref()
        .map(|summary| {
            summary
                .risk_reasons
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if risks.is_empty() {
        risks.push("No specific risks flagged by local deterministic review.".to_string());
    }

    let mut testing_notes = Vec::new();
    match latest_run.as_ref().map(|run| run.status.as_str()) {
        Some("succeeded") => {
            testing_notes.push("Latest Mnemonic agent run completed successfully.".to_string())
        }
        Some("failed") => testing_notes
            .push("Latest Mnemonic agent run failed; review logs before merging.".to_string()),
        Some("running") => testing_notes.push("A Mnemonic agent run is still running.".to_string()),
        Some(status) => testing_notes.push(format!("Latest Mnemonic agent run status: {status}.")),
        None => testing_notes.push(
            "No Mnemonic agent run recorded. Add manual testing notes before opening PR."
                .to_string(),
        ),
    }
    testing_notes
        .push("Review the changed files and diff in Mnemonic before creating a PR.".to_string());

    let draft = WorkspacePrDraft {
        workspace_id: workspace_id.to_string(),
        title,
        summary,
        key_changes,
        risks,
        testing_notes,
        generated_at: timestamp(),
    };
    pr_draft_repository::upsert(&state.db, &draft)?;
    Ok(draft)
}

fn gh_binary() -> String {
    environment_service::find_gh_path().unwrap_or_else(|| "gh".to_string())
}

fn run_in_dir(dir: &str, program: &str, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("Failed to run {program}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_git_authed(dir: &str, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(super::environment_service::git_credential_args())
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(dir)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn workspace_work_dir(state: &AppState, workspace_id: &str) -> Result<String, String> {
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} not found"))?;
    if !workspace.worktree_path.is_empty() {
        return Ok(workspace.worktree_path);
    }
    workspace
        .summary
        .workspace_root_path
        .ok_or_else(|| "Workspace has no worktree path".to_string())
}

/// One-click ship: commit any uncommitted changes, push the branch, then
/// create the PR. Deterministic fallback used when no live agent session is
/// available to do this with better judgment.
pub fn ship_workspace_pr(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspacePrResult, String> {
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} not found"))?;
    let work_dir = workspace_work_dir(state, workspace_id)?;

    // Commit anything outstanding so the PR reflects the working tree.
    let dirty = !run_in_dir(&work_dir, "git", &["status", "--porcelain"])?.is_empty();
    if dirty {
        run_in_dir(&work_dir, "git", &["add", "-A"])?;
        let task = workspace.summary.current_task.trim().to_string();
        let message = if task.is_empty() {
            format!("Checkpoint {} work", workspace.summary.name)
        } else {
            task
        };
        run_in_dir(&work_dir, "git", &["commit", "-m", &message])?;
    }

    let branch = run_in_dir(&work_dir, "git", &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "HEAD" {
        return Err("Workspace is on a detached HEAD; check out a branch first.".to_string());
    }
    run_git_authed(&work_dir, &["push", "-u", "origin", &branch])?;

    match create_workspace_pr(state, workspace_id) {
        Ok(result) => Ok(result),
        Err(err) if err.contains("already exists") => {
            // Branch already has an open PR; the push above updated it.
            let pr_url = run_in_dir(
                &work_dir,
                &gh_binary(),
                &["pr", "view", "--json", "url", "--jq", ".url"],
            )?;
            let pr_number = pr_url
                .rsplit('/')
                .next()
                .and_then(|n| n.parse::<i64>().ok())
                .unwrap_or(0);
            workspace_repository::update_pr_status(
                &state.db,
                workspace_id,
                "open",
                Some(pr_number),
            )?;
            Ok(WorkspacePrResult {
                workspace_id: workspace_id.to_string(),
                pr_url,
                pr_number,
                title: workspace.summary.name,
            })
        }
        Err(err) => Err(err),
    }
}

pub fn create_workspace_pr(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspacePrResult, String> {
    let draft = refresh_workspace_pr_draft(state, workspace_id)?;
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} not found"))?;

    let key_changes_md = draft
        .key_changes
        .iter()
        .map(|c| format!("- {c}"))
        .collect::<Vec<_>>()
        .join("\n");
    let risks_md = draft
        .risks
        .iter()
        .map(|r| format!("- {r}"))
        .collect::<Vec<_>>()
        .join("\n");
    let testing_md = draft
        .testing_notes
        .iter()
        .map(|n| format!("- {n}"))
        .collect::<Vec<_>>()
        .join("\n");

    let body = format!(
        "## Summary\n{}\n\n## Key Changes\n{}\n\n## Risks\n{}\n\n## Testing\n{}\n\n🤖 Generated with Mnemonic",
        draft.summary, key_changes_md, risks_md, testing_md
    );
    let hook_context = serde_json::json!({
        "workspaceId": workspace_id,
        "actionKind": "create_pr",
        "title": draft.title,
    });
    hook_service::run_workspace_hooks(
        state,
        workspace_id,
        "ship",
        hook_service::HookPhase::Pre,
        &hook_context,
    )?;

    let work_dir = if !workspace.worktree_path.is_empty() {
        workspace.worktree_path.clone()
    } else {
        workspace
            .summary
            .workspace_root_path
            .clone()
            .ok_or_else(|| "Workspace has no worktree path".to_string())?
    };

    let output = std::process::Command::new(gh_binary())
        .args(["pr", "create", "--title", &draft.title, "--body", &body])
        .current_dir(&work_dir)
        .output()
        .map_err(|e| format!("Failed to run gh: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh pr create failed: {stderr}"));
    }

    let pr_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let pr_number = pr_url
        .rsplit('/')
        .next()
        .and_then(|n| n.parse::<i64>().ok())
        .unwrap_or(0);

    workspace_repository::update_pr_status(&state.db, workspace_id, "open", Some(pr_number))?;
    let _ = activity_repository::record(
        &state.db,
        workspace_id,
        &workspace.summary.repo,
        Some(&workspace.summary.branch),
        "PR created",
        "success",
        Some(&format!("#{pr_number} — {}", draft.title)),
    );

    let result = WorkspacePrResult {
        workspace_id: workspace_id.to_string(),
        pr_url,
        pr_number,
        title: draft.title,
    };
    let _ = hook_service::run_workspace_hooks(
        state,
        workspace_id,
        "ship",
        hook_service::HookPhase::Post,
        &hook_context,
    );
    Ok(result)
}

pub fn get_workspace_pr_status(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspacePrStatus, String> {
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} not found"))?;
    let work_dir = if !workspace.worktree_path.is_empty() {
        workspace.worktree_path.clone()
    } else {
        workspace
            .summary
            .workspace_root_path
            .clone()
            .ok_or_else(|| "Workspace has no worktree path".to_string())?
    };

    let output = std::process::Command::new(gh_binary())
        .args([
            "pr",
            "view",
            "--json",
            "number,title,url,state,isDraft,reviewDecision,mergeable,mergeStateStatus,statusCheckRollup",
        ])
        .current_dir(&work_dir)
        .output();

    let output = match output {
        Ok(output) => output,
        Err(err) => {
            return Ok(pr_status_warning(
                workspace_id,
                format!("GitHub CLI unavailable: {err}"),
            ));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Ok(WorkspacePrStatus {
            workspace_id: workspace_id.to_string(),
            found: false,
            number: None,
            title: None,
            url: None,
            state: None,
            is_draft: false,
            review_decision: None,
            mergeable: None,
            merge_state_status: None,
            checks_summary: "No PR found for this branch".to_string(),
            checks: vec![],
            warning: if stderr.is_empty() {
                None
            } else {
                Some(stderr)
            },
        });
    }

    let value = serde_json::from_slice::<Value>(&output.stdout)
        .map_err(|err| format!("Failed to parse gh pr view output: {err}"))?;
    let checks = parse_status_checks(value.get("statusCheckRollup"));
    let checks_summary = summarize_checks(&checks);

    Ok(WorkspacePrStatus {
        workspace_id: workspace_id.to_string(),
        found: true,
        number: value.get("number").and_then(Value::as_i64),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
        url: value.get("url").and_then(Value::as_str).map(str::to_string),
        state: value
            .get("state")
            .and_then(Value::as_str)
            .map(str::to_string),
        is_draft: value
            .get("isDraft")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        review_decision: value
            .get("reviewDecision")
            .and_then(Value::as_str)
            .map(str::to_string),
        mergeable: value
            .get("mergeable")
            .and_then(Value::as_str)
            .map(str::to_string),
        merge_state_status: value
            .get("mergeStateStatus")
            .and_then(Value::as_str)
            .map(str::to_string),
        checks_summary,
        checks,
        warning: None,
    })
}

fn pr_status_warning(workspace_id: &str, warning: String) -> WorkspacePrStatus {
    WorkspacePrStatus {
        workspace_id: workspace_id.to_string(),
        found: false,
        number: None,
        title: None,
        url: None,
        state: None,
        is_draft: false,
        review_decision: None,
        mergeable: None,
        merge_state_status: None,
        checks_summary: "PR status unavailable".to_string(),
        checks: vec![],
        warning: Some(warning),
    }
}

fn parse_status_checks(value: Option<&Value>) -> Vec<WorkspacePrCheck> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    let name = item
                        .get("name")
                        .or_else(|| item.get("context"))
                        .or_else(|| item.get("workflowName"))
                        .and_then(Value::as_str)
                        .unwrap_or("check")
                        .to_string();
                    let status = item
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("UNKNOWN")
                        .to_string();
                    let conclusion = item
                        .get("conclusion")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let url = item
                        .get("targetUrl")
                        .or_else(|| item.get("detailsUrl"))
                        .or_else(|| item.get("url"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    WorkspacePrCheck {
                        name,
                        status,
                        conclusion,
                        url,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn summarize_checks(checks: &[WorkspacePrCheck]) -> String {
    if checks.is_empty() {
        return "No CI checks reported".to_string();
    }
    let failed = checks
        .iter()
        .filter(|check| {
            check.conclusion.as_deref().is_some_and(|value| {
                matches!(
                    value,
                    "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
                )
            })
        })
        .count();
    let pending = checks
        .iter()
        .filter(|check| {
            check.conclusion.is_none() && !matches!(check.status.as_str(), "COMPLETED" | "SUCCESS")
        })
        .count();
    if failed > 0 {
        format!("{failed} failing of {} checks", checks.len())
    } else if pending > 0 {
        format!("{pending} pending of {} checks", checks.len())
    } else {
        format!("{} checks passing", checks.len())
    }
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
