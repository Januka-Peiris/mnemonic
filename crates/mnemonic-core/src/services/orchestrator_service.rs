use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::models::{OrchestratorAction, QueueAgentPromptInput};
use crate::repositories::{
    activity_repository, coordination_artifact_repository, orchestrator_repository,
    terminal_repository, workspace_repository,
};
use crate::services::{
    conflict_detection_service, coordinator_service, task_lifecycle_service, terminal_service,
    workspace_health_service,
};
use crate::state::AppState;

/// How often the orchestrator evaluates workspace states.
const ORCHESTRATOR_INTERVAL_SECS: u64 = 5 * 60;
const SCHEDULER_TICK_SECS: u64 = 60;
/// Only flag an agent stuck if it has been silent for at least this long.
const STUCK_THRESHOLD_SECS: u64 = 300;

#[derive(Debug, Clone)]
struct DueJob {
    id: String,
    workspace_id: String,
    kind: String,
    interval_seconds: u64,
    jitter_pct: u64,
}

pub fn start_orchestrator_loop(state: AppState) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(10));
        loop {
            if let Err(err) = ensure_default_scheduler_jobs(&state) {
                log::warn!(target: "mnemonic_lib", "scheduler ensure jobs failed: {err}");
            }
            let now = now_secs();
            let due_jobs = list_due_scheduler_jobs(&state, now).unwrap_or_default();
            for job in due_jobs {
                let single_flight_key = scheduler_single_flight_key(&job);
                let acquired = state
                    .scheduler_job_inflight
                    .lock()
                    .map(|mut guard| {
                        if guard.contains(&single_flight_key) {
                            false
                        } else {
                            guard.insert(single_flight_key.clone());
                            true
                        }
                    })
                    .unwrap_or(false);
                if !acquired {
                    continue;
                }
                let execute_result = match job.kind.as_str() {
                    "workspace_health_check" => {
                        run_workspace_health_check_job(&state, &job.workspace_id)
                    }
                    "coordinator_autostep_watch" => {
                        run_coordinator_autostep_watch_job(&state, &job.workspace_id)
                    }
                    "review_reminder" => {
                        if state.orchestrator_enabled.load(Ordering::Relaxed) {
                            run_orchestrator_pass(&state)
                        } else {
                            Ok(())
                        }
                    }
                    _ => Ok(()),
                };
                if let Err(err) = execute_result {
                    log::warn!(target: "mnemonic_lib", "scheduler job {} failed: {err}", job.kind);
                }
                if let Err(err) = reschedule_job(&state, &job, now) {
                    log::warn!(target: "mnemonic_lib", "scheduler reschedule failed: {err}");
                }
                if let Ok(mut guard) = state.scheduler_job_inflight.lock() {
                    guard.remove(&single_flight_key);
                }
            }
            std::thread::sleep(Duration::from_secs(SCHEDULER_TICK_SECS));
        }
    });
}

fn scheduler_single_flight_key(job: &DueJob) -> String {
    if job.kind == "review_reminder" {
        job.kind.clone()
    } else {
        format!("{}:{}", job.workspace_id, job.kind)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn run_orchestrator_pass(state: &AppState) -> Result<(), String> {
    let model = state
        .orchestrator_model
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| "claude-opus-4-6".to_string());

    let workspaces = workspace_repository::list(&state.db)?;
    let active: Vec<_> = workspaces
        .into_iter()
        .filter(|w| w.status != "Merged")
        .collect();

    if active.is_empty() {
        return Ok(());
    }

    let conflicts = conflict_detection_service::detect_workspace_conflicts(&state.db)
        .unwrap_or_else(|_| crate::models::workspace_conflict::WorkspaceConflicts {
            conflicts: vec![],
            conflicting_workspace_ids: vec![],
        });

    let now = now_secs();
    let run_at = now.to_string();

    // Build context lines per workspace.
    let terminals_registry = state.terminals.lock().map_err(|e| e.to_string())?;
    let mut context_lines: Vec<String> = Vec::new();
    let workspace_ids: Vec<String> = active.iter().map(|w| w.id.clone()).collect();
    let mut task_run_ids: Vec<(String, String)> = Vec::new();
    for ws in &active {
        if let Ok(task_run_id) = task_lifecycle_service::start_task_run(
            state,
            &ws.id,
            "orchestrator",
            Some("review_reminder"),
        ) {
            task_lifecycle_service::append_task_event(
                state,
                &task_run_id,
                &ws.id,
                "orchestrator_pass_started",
                serde_json::json!({ "runAt": run_at }),
            );
            task_run_ids.push((ws.id.clone(), task_run_id));
        }
    }

    for ws in &active {
        let sessions =
            terminal_repository::list_for_workspace(&state.db, &ws.id).unwrap_or_default();

        // Find the agent session (if running).
        let agent_session = sessions
            .iter()
            .find(|s| s.session_role == "agent" && s.status == "running" && s.closed_at.is_none());

        let stuck_label = if let Some(session) = agent_session {
            let last_secs = terminals_registry
                .get(&session.id)
                .map(|a| a.last_output_at_secs.load(Ordering::Relaxed))
                .unwrap_or(0);
            if last_secs > 0 && now.saturating_sub(last_secs) >= STUCK_THRESHOLD_SECS {
                let mins = now.saturating_sub(last_secs) / 60;
                format!("STUCK ({mins}m silent)")
            } else {
                "running".to_string()
            }
        } else if sessions
            .iter()
            .any(|s| s.status == "running" && s.closed_at.is_none())
        {
            "running (non-agent session)".to_string()
        } else {
            "idle".to_string()
        };

        let files_summary = if ws.changed_files.is_empty() {
            "no changed files".to_string()
        } else {
            ws.changed_files
                .iter()
                .take(5)
                .map(|f| format!("{} (+{}/−{})", f.path, f.additions, f.deletions))
                .collect::<Vec<_>>()
                .join(", ")
        };

        let conflict_note = conflicts
            .conflicts
            .iter()
            .filter(|c| c.workspace_id_a == ws.id || c.workspace_id_b == ws.id)
            .map(|c| {
                let other = if c.workspace_id_a == ws.id {
                    &c.workspace_id_b
                } else {
                    &c.workspace_id_a
                };
                format!(
                    "⚠️ FILE CONFLICT with {}: {}",
                    other,
                    c.shared_files.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");

        let mut block = format!(
            "[WORKSPACE {}]\nName: {}\nRepo: {} | Branch: {}\nTask: {}\nSession: {}\nFiles: {}",
            ws.id,
            ws.name,
            ws.repo,
            ws.branch,
            if ws.current_task.is_empty() {
                "(no task set)"
            } else {
                &ws.current_task
            },
            stuck_label,
            files_summary,
        );
        let parent_workspace_id = ws
            .parent_workspace_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or(&ws.id)
            .to_string();
        let group_members = active
            .iter()
            .filter(|candidate| {
                candidate.id == parent_workspace_id
                    || candidate.parent_workspace_id.as_deref()
                        == Some(parent_workspace_id.as_str())
            })
            .map(|candidate| format!("{} ({})", candidate.id, candidate.repo))
            .collect::<Vec<_>>();
        if group_members.len() > 1 {
            block.push_str(&format!("\nFederated group: {}", group_members.join(", ")));
        }
        let unresolved_artifacts = coordination_artifact_repository::list_by_parent_workspace(
            &state.db,
            &parent_workspace_id,
        )
        .unwrap_or_default()
        .into_iter()
        .filter(|artifact| matches!(artifact.status.as_str(), "draft" | "active"))
        .take(5)
        .map(|artifact| {
            format!(
                "{} [{}] from {}{}: {}",
                artifact.title,
                artifact.artifact_kind,
                artifact.source_workspace_id,
                artifact
                    .target_workspace_id
                    .as_deref()
                    .map(|target| format!(" to {target}"))
                    .unwrap_or_default(),
                artifact.body
            )
        })
        .collect::<Vec<_>>();
        if !unresolved_artifacts.is_empty() {
            block.push_str(&format!(
                "\nUnresolved coordination artifacts: {}",
                unresolved_artifacts.join(" | ")
            ));
        }
        if !conflict_note.is_empty() {
            block.push('\n');
            block.push_str(&conflict_note);
        }
        context_lines.push(block);
    }

    drop(terminals_registry);

    let workspace_context = context_lines.join("\n\n");
    let pass_result = run_orchestrator_pass_inner(
        state,
        &model,
        &run_at,
        now,
        &workspace_context,
        &workspace_ids,
    );

    match &pass_result {
        Ok(actions) => {
            finalize_orchestrator_task_runs(state, &task_run_ids, "completed", actions.len(), None)
        }
        Err(error) => {
            finalize_orchestrator_task_runs(state, &task_run_ids, "failed", 0, Some(error))
        }
    }

    pass_result.map(|_| ())
}

fn run_orchestrator_pass_inner(
    state: &AppState,
    model: &str,
    run_at: &str,
    now: u64,
    workspace_context: &str,
    workspace_ids: &[String],
) -> Result<Vec<OrchestratorAction>, String> {
    let prompt = format!(
        r#"You are the Mnemonic Orchestrator — the strategic brain coordinating multiple AI coding agents (Claude, Codex, Kimi) running in parallel git worktrees.

Your role:
- Detect stuck agents and send a short actionable nudge
- Alert agents to cross-workspace file conflicts they should coordinate on
- Provide brief strategic direction when an agent seems lost

Current workspace states (unix timestamp: {now}):

{workspace_context}

Available actions — respond with a JSON array ONLY (no explanation, no markdown):
[
  {{"action": "send_prompt", "workspace_id": "...", "prompt": "short actionable message to the agent"}},
  {{"action": "notify", "workspace_id": "...", "message": "message shown to the user as a notification"}}
]

Rules:
- Be conservative — only act when clearly needed
- send_prompt goes directly into the coding agent's terminal input
- Keep prompts short (1-3 sentences), actionable, and specific
- Use federated group context when present: coordinate related workspaces as ship-together peers, not isolated tasks
- Treat unresolved coordination artifacts as first-class handoff signals
- If an artifact has a target workspace, nudge that target only when the artifact is relevant to its next action
- If an artifact has no target, use it as group-wide context; prefer notifying the user over spamming every agent
- Respect release_ordering_note artifacts when suggesting PR or merge sequencing
- If no intervention is needed, respond with exactly: []
- Do NOT include "idle" actions — just omit workspaces that need nothing"#
    );

    let response = if is_openai_model(model) {
        call_openai_api(model, &prompt)?
    } else {
        let output = std::process::Command::new("claude")
            .args(["--model", model, "-p", &prompt])
            .output()
            .map_err(|e| format!("Failed to run claude CLI: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("claude CLI failed: {stderr}"));
        }
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let actions = parse_actions(&response);

    log::info!(
        target: "mnemonic_lib",
        "orchestrator ran with model {model}: {} action(s)",
        actions.len()
    );

    // Execute actions.
    for action in &actions {
        match action.action.as_str() {
            "send_prompt" => {
                if let (Some(ws_id), Some(prompt_text)) =
                    (action.workspace_id.as_deref(), action.prompt.as_deref())
                {
                    let input = QueueAgentPromptInput {
                        session_id: None,
                        workspace_id: ws_id.to_string(),
                        prompt: prompt_text.to_string(),
                        profile: None,
                        profile_id: None,
                        extra_args: None,
                    };
                    if let Err(err) = terminal_service::queue_workspace_agent_prompt(state, input) {
                        log::warn!(
                            target: "mnemonic_lib",
                            "orchestrator: failed to send prompt to {ws_id}: {err}"
                        );
                    }
                    let ws = workspace_ids.iter().find(|id| id.as_str() == ws_id);
                    if let Some(ws_id) = ws {
                        let _ = activity_repository::record(
                            &state.db,
                            ws_id,
                            "",
                            None,
                            "Orchestrator intervention",
                            "info",
                            Some(prompt_text),
                        );
                    }
                }
            }
            "notify" => {
                let ws_id = action.workspace_id.as_deref().unwrap_or("");
                let message = action.message.as_deref().unwrap_or("");
                state.event_emitter.emit(
                    crate::events::ORCHESTRATOR_NOTIFY,
                    &serde_json::json!({
                        "workspaceId": ws_id,
                        "message": message,
                    }),
                );
            }
            _ => {}
        }
    }

    // Persist log + update state.
    let _ = orchestrator_repository::insert_log(&state.db, run_at, model, workspace_ids, &actions);

    if let (Ok(mut last_run), Ok(mut last_actions)) = (
        state.orchestrator_last_run.lock(),
        state.orchestrator_last_actions.lock(),
    ) {
        *last_run = Some(run_at.to_string());
        *last_actions = actions.clone();
    }

    Ok(actions)
}

fn finalize_orchestrator_task_runs(
    state: &AppState,
    task_run_ids: &[(String, String)],
    status: &str,
    action_count: usize,
    error: Option<&str>,
) {
    let event_name = if status == "completed" {
        "orchestrator_pass_finished"
    } else {
        "orchestrator_pass_failed"
    };
    for (workspace_id, task_run_id) in task_run_ids {
        task_lifecycle_service::append_task_event(
            state,
            task_run_id,
            workspace_id,
            event_name,
            serde_json::json!({ "actionCount": action_count, "error": error }),
        );
        let _ = task_lifecycle_service::mark_task_run_completed(state, task_run_id, status);
    }
}

pub fn list_workspace_scheduler_jobs(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<crate::models::WorkspaceSchedulerJob>, String> {
    state.db.with_connection(|connection| {
        let mut statement = connection.prepare(
            r#"
            SELECT id, workspace_id, kind, interval_seconds, next_run_at, enabled, jitter_pct
            FROM workspace_scheduler_jobs
            WHERE workspace_id = ?1
            ORDER BY kind ASC
            "#,
        )?;
        let jobs = statement
            .query_map(rusqlite::params![workspace_id], |row| {
                Ok(crate::models::WorkspaceSchedulerJob {
                    id: row.get("id")?,
                    workspace_id: row.get("workspace_id")?,
                    kind: row.get("kind")?,
                    interval_seconds: row.get::<_, i64>("interval_seconds")? as u64,
                    next_run_at: row.get::<_, i64>("next_run_at")? as u64,
                    enabled: row.get::<_, i64>("enabled")? != 0,
                    jitter_pct: row.get::<_, i64>("jitter_pct")?.max(0) as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(jobs)
    })
}

pub fn set_workspace_scheduler_job_enabled(
    state: &AppState,
    workspace_id: &str,
    job_id: &str,
    enabled: bool,
) -> Result<(), String> {
    let updated = state.db.with_connection_mut(|connection| {
        connection.execute(
            r#"
            UPDATE workspace_scheduler_jobs
            SET enabled = ?3, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?1 AND workspace_id = ?2
            "#,
            rusqlite::params![job_id, workspace_id, if enabled { 1 } else { 0 }],
        )
    })?;
    if updated == 0 {
        return Err("Scheduler job not found for workspace".to_string());
    }

    let _ = activity_repository::record(
        &state.db,
        workspace_id,
        "",
        None,
        "Scheduler job updated",
        "info",
        Some(if enabled {
            "Scheduler job resumed."
        } else {
            "Scheduler job paused."
        }),
    );
    Ok(())
}

pub fn schedule_workspace_scheduler_job_now(
    state: &AppState,
    workspace_id: &str,
    job_id: &str,
) -> Result<(), String> {
    let now = now_secs() as i64;
    let updated = state.db.with_connection_mut(|connection| {
        connection.execute(
            r#"
            UPDATE workspace_scheduler_jobs
            SET next_run_at = ?3, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?1 AND workspace_id = ?2
            "#,
            rusqlite::params![job_id, workspace_id, now],
        )
    })?;
    if updated == 0 {
        return Err("Scheduler job not found for workspace".to_string());
    }

    let _ = activity_repository::record(
        &state.db,
        workspace_id,
        "",
        None,
        "Scheduler job nudged",
        "info",
        Some("Scheduler job marked to run on the next tick."),
    );
    Ok(())
}

fn ensure_default_scheduler_jobs(state: &AppState) -> Result<(), String> {
    let workspaces = workspace_repository::list(&state.db)?;
    let active = workspaces
        .into_iter()
        .filter(|workspace| workspace.status != "Merged")
        .map(|workspace| workspace.id)
        .collect::<HashSet<_>>();
    let now = now_secs() as i64;
    state.db.with_connection_mut(|connection| {
        for workspace_id in active {
            for (kind, interval_seconds, jitter_pct) in [
                ("workspace_health_check", 300i64, 10i64),
                ("coordinator_autostep_watch", 120i64, 0i64),
                ("review_reminder", ORCHESTRATOR_INTERVAL_SECS as i64, 10i64),
            ] {
                let id = format!("job-{workspace_id}-{kind}");
                connection.execute(
                    r#"
                    INSERT INTO workspace_scheduler_jobs
                        (id, workspace_id, kind, interval_seconds, next_run_at, enabled, jitter_pct, updated_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, CURRENT_TIMESTAMP)
                    ON CONFLICT(workspace_id, kind) DO NOTHING
                    "#,
                    rusqlite::params![id, workspace_id, kind, interval_seconds, now + interval_seconds, jitter_pct],
                )?;
            }
        }
        Ok(())
    })
}

fn list_due_scheduler_jobs(state: &AppState, now: u64) -> Result<Vec<DueJob>, String> {
    state.db.with_connection(|connection| {
        let mut statement = connection.prepare(
            r#"
            SELECT id, workspace_id, kind, interval_seconds, jitter_pct
            FROM workspace_scheduler_jobs
            WHERE enabled = 1 AND next_run_at <= ?1
            ORDER BY next_run_at ASC
            LIMIT 24
            "#,
        )?;
        let rows = statement
            .query_map(rusqlite::params![now as i64], |row| {
                Ok(DueJob {
                    id: row.get("id")?,
                    workspace_id: row.get("workspace_id")?,
                    kind: row.get("kind")?,
                    interval_seconds: row.get::<_, i64>("interval_seconds")? as u64,
                    jitter_pct: row.get::<_, i64>("jitter_pct")?.max(0) as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

fn reschedule_job(state: &AppState, job: &DueJob, now: u64) -> Result<(), String> {
    let jitter = (job.interval_seconds * job.jitter_pct) / 100;
    let next_run_at = now
        .saturating_add(job.interval_seconds)
        .saturating_add(jitter / 2);
    state.db.with_connection_mut(|connection| {
        connection.execute(
            "UPDATE workspace_scheduler_jobs SET next_run_at = ?2, updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
            rusqlite::params![job.id, next_run_at as i64],
        )?;
        Ok(())
    })
}

fn run_workspace_health_check_job(state: &AppState, workspace_id: &str) -> Result<(), String> {
    let health = workspace_health_service::get_workspace_health(state, workspace_id)?;
    if health.status == "needs_attention" {
        let _ = activity_repository::record(
            &state.db,
            workspace_id,
            "",
            None,
            "Scheduler workspace health check",
            "warning",
            Some("Workspace health needs attention."),
        );
    }
    Ok(())
}

fn run_coordinator_autostep_watch_job(state: &AppState, workspace_id: &str) -> Result<(), String> {
    let status = coordinator_service::get_workspace_coordinator_status(state, workspace_id)?;
    let has_active = status.active_run.is_some();
    let running_workers = status
        .workers
        .iter()
        .filter(|worker| worker.status == "running")
        .count();
    if has_active && running_workers == 0 {
        match coordinator_service::step_workspace_coordinator(
            state,
            crate::models::StepWorkspaceCoordinatorInput {
                workspace_id: workspace_id.to_string(),
                instruction: "Continue coordinator execution".to_string(),
                brain_profile_id: None,
                coder_profile_id: None,
                brain_provider: None,
                coder_provider: None,
                brain_model: None,
                coder_model: None,
                brain_reasoning: None,
                coder_reasoning: None,
            },
        ) {
            Ok(_) => {}
            Err(err) if err.starts_with("COORDINATOR_STEP_IN_PROGRESS:") => {
                log::debug!(target: "mnemonic_lib", "coordinator autostep already running for {workspace_id}");
            }
            Err(err) => {
                log::warn!(target: "mnemonic_lib", "coordinator autostep failed for {workspace_id}: {err}");
                return Err(err);
            }
        }
    }
    Ok(())
}

fn is_openai_model(model: &str) -> bool {
    model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
}

fn call_openai_api(model: &str, prompt: &str) -> Result<String, String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY environment variable not set".to_string())?;

    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}]
    });

    let client = reqwest::blocking::Client::new();
    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .map_err(|e| format!("OpenAI request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp
            .text()
            .unwrap_or_else(|e| format!("(failed to read body: {e})"));
        return Err(format!("OpenAI API error {status}: {text}"));
    }

    let json: serde_json::Value = resp
        .json()
        .map_err(|e| format!("OpenAI response parse error: {e}"))?;
    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "OpenAI response missing content".to_string())?
        .to_string();

    Ok(content)
}

/// Extract a JSON array of actions from the model response.
/// The model is instructed to return only JSON, but may wrap it in markdown fences.
fn parse_actions(response: &str) -> Vec<OrchestratorAction> {
    let text = response.trim();
    // Strip ```json ... ``` or ``` ... ``` fences if present.
    let json_text = if let Some(start) = text.find('[') {
        if let Some(end) = text.rfind(']') {
            &text[start..=end]
        } else {
            text
        }
    } else {
        text
    };
    serde_json::from_str::<Vec<OrchestratorAction>>(json_text).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn due_job(workspace_id: &str, kind: &str) -> DueJob {
        DueJob {
            id: format!("job-{workspace_id}-{kind}"),
            workspace_id: workspace_id.to_string(),
            kind: kind.to_string(),
            interval_seconds: 300,
            jitter_pct: 0,
        }
    }

    #[test]
    fn review_reminder_uses_global_single_flight_key() {
        assert_eq!(
            scheduler_single_flight_key(&due_job("ws-1", "review_reminder")),
            scheduler_single_flight_key(&due_job("ws-2", "review_reminder")),
        );
    }

    #[test]
    fn workspace_jobs_keep_workspace_scoped_single_flight_keys() {
        assert_ne!(
            scheduler_single_flight_key(&due_job("ws-1", "workspace_health_check")),
            scheduler_single_flight_key(&due_job("ws-2", "workspace_health_check")),
        );
    }
}
