use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::models::{WorkspacePrCheck, WorkspacePrDraft, WorkspacePrResult, WorkspacePrStatus};
use crate::repositories::{
    activity_repository, agent_run_repository, pr_draft_repository, settings_repository,
    workspace_repository,
};
use crate::services::{
    environment_service, git_review_service, hook_service, review_cockpit_service,
    review_summary_service,
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

/// Re-runs the failed GitHub Actions runs behind this workspace's failing PR
/// checks. Run ids are extracted from the check detail URLs.
pub fn rerun_failed_workspace_checks(
    state: &AppState,
    workspace_id: &str,
) -> Result<String, String> {
    let status = get_workspace_pr_status(state, workspace_id)?;
    if !status.found {
        return Err("No PR found for this branch".to_string());
    }

    // Each run is re-run in the repository its check URL names, since gh
    // would otherwise look for it in the upstream of a fork.
    let mut run_ids: Vec<(String, String)> = Vec::new();
    for check in &status.checks {
        let failed = matches!(
            check.conclusion.as_deref(),
            Some("FAILURE") | Some("TIMED_OUT") | Some("CANCELLED") | Some("ACTION_REQUIRED")
        );
        if !failed {
            continue;
        }
        let Some(url) = check.url.as_deref() else {
            continue;
        };
        let Some((repo_url, rest)) = url.split_once("/actions/runs/") else {
            continue;
        };
        let Some(repo) = repo_url.strip_prefix("https://github.com/") else {
            continue;
        };
        let run_id: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !run_id.is_empty() && !run_ids.iter().any(|(_, id)| id == &run_id) {
            run_ids.push((repo.to_string(), run_id));
        }
    }

    if run_ids.is_empty() {
        return Err("No failed runs with re-runnable URLs found".to_string());
    }

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

    let mut errors = Vec::new();
    let mut rerun_count = 0;
    for (repo, run_id) in &run_ids {
        let output = std::process::Command::new(gh_binary())
            .args(["run", "rerun", run_id, "--failed", "--repo", repo])
            .current_dir(&work_dir)
            .output()
            .map_err(|err| format!("Failed to invoke gh: {err}"))?;
        if output.status.success() {
            rerun_count += 1;
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            errors.push(format!("run {run_id}: {stderr}"));
        }
    }

    if rerun_count == 0 {
        return Err(format!("Could not re-run any runs: {}", errors.join("; ")));
    }
    Ok(format!("Re-ran {rerun_count} run(s)"))
}

const MERGE_METHOD_KEY: &str = "merge_method";
const DEFAULT_MERGE_METHOD: &str = "squash";

/// The merge method from the `merge_method` setting: "squash" (default),
/// "merge" or "rebase".
pub fn configured_merge_method(state: &AppState) -> String {
    match settings_repository::get_value(&state.db, MERGE_METHOD_KEY) {
        Ok(Some(value)) if merge_method_flag(value.trim()).is_some() => value.trim().to_string(),
        Ok(Some(value)) => {
            log::warn!("Ignoring unknown merge_method setting {value:?}; using squash");
            DEFAULT_MERGE_METHOD.to_string()
        }
        Ok(None) => DEFAULT_MERGE_METHOD.to_string(),
        Err(err) => {
            log::warn!("Failed to read merge_method setting: {err}");
            DEFAULT_MERGE_METHOD.to_string()
        }
    }
}

fn merge_method_flag(method: &str) -> Option<&'static str> {
    match method {
        "squash" => Some("--squash"),
        "merge" => Some("--merge"),
        "rebase" => Some("--rebase"),
        _ => None,
    }
}

/// Merges the workspace PR with `gh pr merge`, pinned to `head_ref_oid` so
/// GitHub refuses if the PR head moved since the caller read its status. The
/// branch is never deleted because the workspace worktree still uses it.
pub fn merge_workspace_pr(
    state: &AppState,
    workspace_id: &str,
    number: i64,
    pr_url: Option<&str>,
    head_ref_oid: &str,
    method: &str,
) -> Result<String, String> {
    let method_flag =
        merge_method_flag(method).ok_or_else(|| format!("Unknown merge method {method:?}"))?;
    if head_ref_oid.trim().is_empty() {
        return Err(
            "The PR head commit is unknown; refresh the PR status and try again.".to_string(),
        );
    }
    let work_dir = workspace_work_dir(state, workspace_id)?;
    // The URL names the repository as well as the PR. A bare number is
    // resolved in whichever repository gh treats as the base, which for a
    // fork is the upstream and so a different PR.
    let number_text = number.to_string();
    let selector = pr_url.unwrap_or(&number_text);
    let gh = gh_binary();
    let output = std::process::Command::new(&gh)
        .args([
            "pr",
            "merge",
            selector,
            method_flag,
            "--match-head-commit",
            head_ref_oid,
        ])
        .current_dir(&work_dir)
        .output()
        .map_err(|err| format!("Failed to run gh: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(if detail.is_empty() {
            format!("gh pr merge {number} failed")
        } else {
            detail
        });
    }

    // gh exits successfully when it only queues the PR or enables auto-merge,
    // so confirm the actual state before telling the user it merged.
    match run_in_dir(
        &work_dir,
        &gh,
        &[
            "pr",
            "view",
            selector,
            "--json",
            "state",
            "--jq",
            ".state",
        ],
    ) {
        Ok(pr_state) if pr_state == "MERGED" => Ok(format!("Merged PR #{number} ({method})")),
        Ok(pr_state) => Ok(format!(
            "GitHub accepted the merge for PR #{number} but it is still {}; it may be queued or waiting on auto-merge",
            pr_state.to_lowercase()
        )),
        Err(err) => {
            log::warn!("Merged PR #{number} but could not confirm its state: {err}");
            Ok(format!("Merge requested for PR #{number}"))
        }
    }
}

const PR_STATUS_FIELDS: &str = "number,title,url,state,isDraft,reviewDecision,mergeable,mergeStateStatus,headRefOid,statusCheckRollup";

/// Runs `gh pr view` for the branch checked out in `work_dir`. gh looks in
/// the repository it treats as the base, which for a fork is the upstream, so
/// a PR opened against the fork itself is only found by naming `origin`.
fn view_branch_pr(work_dir: &str) -> std::io::Result<std::process::Output> {
    let gh = gh_binary();
    let output = std::process::Command::new(&gh)
        .args(["pr", "view", "--json", PR_STATUS_FIELDS])
        .current_dir(work_dir)
        .output()?;
    if output.status.success() {
        return Ok(output);
    }

    let Some((owner, repo)) =
        review_cockpit_service::git_remote_owner_repo(std::path::Path::new(work_dir))
    else {
        return Ok(output);
    };
    let branch = match run_in_dir(work_dir, "git", &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Ok(branch) => branch,
        Err(err) => {
            log::debug!("Could not read the current branch for the PR lookup: {err}");
            return Ok(output);
        }
    };
    let origin_output = std::process::Command::new(&gh)
        .args([
            "pr",
            "view",
            &branch,
            "--repo",
            &format!("{owner}/{repo}"),
            "--json",
            PR_STATUS_FIELDS,
        ])
        .current_dir(work_dir)
        .output()?;
    // When neither lookup finds a PR, gh's first answer is the one to report.
    Ok(if origin_output.status.success() {
        origin_output
    } else {
        output
    })
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

    let output = view_branch_pr(&work_dir);

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
            head_ref_oid: None,
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
        head_ref_oid: value
            .get("headRefOid")
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
        head_ref_oid: None,
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
