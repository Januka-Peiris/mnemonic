use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, OptionalExtension};

use crate::db::Database;
use crate::models::{
    CoordinatorActionLog, CoordinatorResultPayload, CoordinatorRun, CoordinatorWorker,
    WorkspaceCoordinatorStatus,
};

pub struct CoordinatorActionInsert<'a> {
    pub run_id: &'a str,
    pub workspace_id: &'a str,
    pub action_kind: &'a str,
    pub worker_id: Option<&'a str>,
    pub prompt: Option<&'a str>,
    pub message: Option<&'a str>,
    pub raw_json: Option<&'a str>,
}

fn run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CoordinatorRun> {
    Ok(CoordinatorRun {
        id: row.get("id")?,
        workspace_id: row.get("workspace_id")?,
        status: row.get("status")?,
        brain_profile_id: row.get("brain_profile_id")?,
        coder_profile_id: row.get("coder_profile_id")?,
        goal: row.get("goal")?,
        last_response: row.get("last_response")?,
        last_error: row.get("last_error")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        completed_at: row.get("completed_at")?,
    })
}

fn worker_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CoordinatorWorker> {
    Ok(CoordinatorWorker {
        id: row.get("id")?,
        run_id: row.get("run_id")?,
        workspace_id: row.get("workspace_id")?,
        profile_id: row.get("profile_id")?,
        status: row.get("status")?,
        last_prompt: row.get("last_prompt")?,
        last_session_id: row.get("last_session_id")?,
        notified_status: row.get("notified_status")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn action_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CoordinatorActionLog> {
    let raw_json: Option<String> = row.get("raw_json")?;
    Ok(CoordinatorActionLog {
        id: row.get("id")?,
        run_id: row.get("run_id")?,
        workspace_id: row.get("workspace_id")?,
        action_kind: row.get("action_kind")?,
        replay_kind: row.get("replay_kind")?,
        replayed_from_action_id: row.get("replayed_from_action_id")?,
        worker_id: row.get("worker_id")?,
        prompt: row.get("prompt")?,
        message: row.get("message")?,
        result: parse_result_payload(raw_json.as_deref()),
        raw_json,
        created_at: row.get("created_at")?,
    })
}

fn parse_result_payload(raw_json: Option<&str>) -> Option<CoordinatorResultPayload> {
    let raw = raw_json?;
    let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    if let Some(result) = value.get("result") {
        return serde_json::from_value::<CoordinatorResultPayload>(result.clone()).ok();
    }
    serde_json::from_value::<CoordinatorResultPayload>(value).ok()
}

pub fn active_run_for_workspace(
    db: &Database,
    workspace_id: &str,
) -> Result<Option<CoordinatorRun>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                "SELECT id, workspace_id, status, brain_profile_id, coder_profile_id, goal, last_response, last_error, created_at, updated_at, completed_at
                 FROM workspace_coordinator_runs
                 WHERE workspace_id = ?1 AND status = 'running'
                 ORDER BY updated_at DESC
                 LIMIT 1",
                params![workspace_id],
                run_from_row,
            )
            .optional()
    })
}

pub fn list_active_runs(db: &Database) -> Result<Vec<CoordinatorRun>, String> {
    db.with_connection(|connection| {
        let mut stmt = connection.prepare(
            "SELECT id, workspace_id, status, brain_profile_id, coder_profile_id, goal, last_response, last_error, created_at, updated_at, completed_at
             FROM workspace_coordinator_runs
             WHERE status = 'running'
             ORDER BY updated_at DESC",
        )?;
        let rows = stmt
            .query_map([], run_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

pub fn create_run(
    db: &Database,
    run_id: &str,
    workspace_id: &str,
    brain_profile_id: &str,
    coder_profile_id: &str,
    goal: &str,
) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "UPDATE workspace_coordinator_runs
             SET status = 'stopped', completed_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP
             WHERE workspace_id = ?1 AND status = 'running'",
            params![workspace_id],
        )?;
        connection.execute(
            "INSERT INTO workspace_coordinator_runs (
                id, workspace_id, status, brain_profile_id, coder_profile_id, goal, created_at, updated_at
             ) VALUES (?1, ?2, 'running', ?3, ?4, ?5, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
            params![run_id, workspace_id, brain_profile_id, coder_profile_id, goal],
        )?;
        Ok(())
    })
}

pub fn mark_run_result(
    db: &Database,
    run_id: &str,
    last_response: Option<&str>,
    last_error: Option<&str>,
) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "UPDATE workspace_coordinator_runs
             SET last_response = COALESCE(?2, last_response),
                 last_error = ?3,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1",
            params![run_id, last_response, last_error],
        )?;
        Ok(())
    })
}

pub fn update_run_profiles(
    db: &Database,
    run_id: &str,
    brain_profile_id: &str,
    coder_profile_id: &str,
) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "UPDATE workspace_coordinator_runs
             SET brain_profile_id = ?2,
                 coder_profile_id = ?3,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1",
            params![run_id, brain_profile_id, coder_profile_id],
        )?;
        Ok(())
    })
}

pub fn finish_run(db: &Database, run_id: &str, status: &str) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "UPDATE workspace_coordinator_runs
             SET status = ?2, completed_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1",
            params![run_id, status],
        )?;
        connection.execute(
            "UPDATE workspace_coordinator_workers
             SET status = CASE WHEN status = 'running' THEN 'stopped' ELSE status END,
                 updated_at = CURRENT_TIMESTAMP
             WHERE run_id = ?1",
            params![run_id],
        )?;
        Ok(())
    })
}

pub fn list_workers_for_run(db: &Database, run_id: &str) -> Result<Vec<CoordinatorWorker>, String> {
    db.with_connection(|connection| {
        let mut stmt = connection.prepare(
            "SELECT id, run_id, workspace_id, profile_id, status, last_prompt, last_session_id, notified_status, created_at, updated_at
             FROM workspace_coordinator_workers
             WHERE run_id = ?1
             ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id], worker_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

pub fn upsert_worker(db: &Database, worker: &CoordinatorWorker) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "INSERT INTO workspace_coordinator_workers (
                id, run_id, workspace_id, profile_id, status, last_prompt, last_session_id, notified_status, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, COALESCE(?9, CURRENT_TIMESTAMP), CURRENT_TIMESTAMP)
             ON CONFLICT(id) DO UPDATE SET
                status = excluded.status,
                last_prompt = excluded.last_prompt,
                last_session_id = excluded.last_session_id,
                notified_status = excluded.notified_status,
                updated_at = CURRENT_TIMESTAMP",
            params![
                worker.id,
                worker.run_id,
                worker.workspace_id,
                worker.profile_id,
                worker.status,
                worker.last_prompt,
                worker.last_session_id,
                worker.notified_status,
                worker.created_at
            ],
        )?;
        Ok(())
    })
}

pub fn insert_action(db: &Database, input: CoordinatorActionInsert<'_>) -> Result<(), String> {
    insert_action_with_metadata(
        db,
        input.run_id,
        input.workspace_id,
        input.action_kind,
        None,
        None,
        input.worker_id,
        input.prompt,
        input.message,
        input.raw_json,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn insert_action_with_metadata(
    db: &Database,
    run_id: &str,
    workspace_id: &str,
    action_kind: &str,
    replay_kind: Option<&str>,
    replayed_from_action_id: Option<&str>,
    worker_id: Option<&str>,
    prompt: Option<&str>,
    message: Option<&str>,
    raw_json: Option<&str>,
) -> Result<(), String> {
    let id = format!(
        "coord-action-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or_default()
    );
    db.with_connection_mut(|connection| {
        connection.execute(
            "INSERT INTO workspace_coordinator_actions (
                id, run_id, workspace_id, action_kind, replay_kind, replayed_from_action_id, worker_id, prompt, message, raw_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, CURRENT_TIMESTAMP)",
            params![
                id,
                run_id,
                workspace_id,
                action_kind,
                replay_kind,
                replayed_from_action_id,
                worker_id,
                prompt,
                message,
                raw_json
            ],
        )?;
        Ok(())
    })
}

pub fn list_recent_actions_for_workspace(
    db: &Database,
    workspace_id: &str,
    limit: u32,
) -> Result<Vec<CoordinatorActionLog>, String> {
    db.with_connection(|connection| {
        let mut stmt = connection.prepare(
            "SELECT id, run_id, workspace_id, action_kind, replay_kind, replayed_from_action_id, worker_id, prompt, message, raw_json, created_at
             FROM workspace_coordinator_actions
             WHERE workspace_id = ?1
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![workspace_id, limit], action_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

pub fn get_action_by_id(
    db: &Database,
    workspace_id: &str,
    action_id: &str,
) -> Result<Option<CoordinatorActionLog>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                "SELECT id, run_id, workspace_id, action_kind, replay_kind, replayed_from_action_id, worker_id, prompt, message, raw_json, created_at
                 FROM workspace_coordinator_actions
                 WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id, action_id],
                action_from_row,
            )
            .optional()
    })
}

pub fn get_worker_by_id(
    db: &Database,
    worker_id: &str,
) -> Result<Option<CoordinatorWorker>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                "SELECT id, run_id, workspace_id, profile_id, status, last_prompt, last_session_id, notified_status, created_at, updated_at
                 FROM workspace_coordinator_workers
                 WHERE id = ?1",
                params![worker_id],
                worker_from_row,
            )
            .optional()
    })
}

pub fn workspace_status(
    db: &Database,
    workspace_id: &str,
) -> Result<WorkspaceCoordinatorStatus, String> {
    let active_run = active_run_for_workspace(db, workspace_id)?;
    let workers = if let Some(run) = active_run.as_ref() {
        list_workers_for_run(db, &run.id)?
    } else {
        vec![]
    };
    let recent_actions = list_recent_actions_for_workspace(db, workspace_id, 25)?;
    let mut planner_adapter = None;
    let mut planner_parse_mode = None;
    let mut planner_fallback = None;
    let mut planner_last_message = None;
    if let Some(planner) = recent_actions
        .iter()
        .find(|action| action.action_kind == "planner")
    {
        planner_last_message = planner.message.clone();
        if let Some(message) = planner.message.as_deref() {
            for part in message.split_whitespace() {
                if let Some(value) = part.strip_prefix("adapter=") {
                    planner_adapter = Some(value.to_string());
                } else if let Some(value) = part.strip_prefix("parse=") {
                    planner_parse_mode = Some(value.to_string());
                } else if let Some(value) = part.strip_prefix("fallback=") {
                    planner_fallback = Some(value.eq_ignore_ascii_case("yes"));
                }
            }
        }
    }
    Ok(WorkspaceCoordinatorStatus {
        workspace_id: workspace_id.to_string(),
        mode: if active_run.is_some() {
            "coordinator".to_string()
        } else {
            "direct".to_string()
        },
        active_run,
        workers,
        recent_actions,
        planner_adapter,
        planner_parse_mode,
        planner_fallback,
        planner_last_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_workspace(db: &Database, workspace_id: &str) {
        db.with_connection_mut(|connection| {
            connection.execute(
                "INSERT INTO workspaces (
                    id, name, repo, branch, agent, status, current_step, completed_steps,
                    last_updated, description, current_task, merge_risk, last_rebase, base_branch,
                    agent_session_id, agent_session_agent, agent_session_status, agent_session_model,
                    agent_session_estimated_cost, agent_session_last_message, agent_session_started_at, worktree_path
                 ) VALUES (?1, 'WS', 'repo', 'main', 'Codex', 'Waiting', 'Planning', '[]',
                           '0', '', '', 'low', '', 'main', '', '', '', '', '', '', '', '/tmp')",
                params![workspace_id],
            )?;
            Ok(())
        })
        .expect("insert workspace");
    }

    #[test]
    fn action_replay_metadata_round_trips() {
        let db = crate::db::Database::in_memory().expect("db");
        insert_workspace(&db, "ws");
        create_run(&db, "run-1", "ws", "brain", "coder", "goal").expect("run");
        insert_action_with_metadata(
            &db,
            "run-1",
            "ws",
            "replay_worker_prompt",
            Some("prompt_override"),
            Some("source-action-1"),
            Some("worker-1"),
            Some("do work"),
            Some("replayed"),
            None,
        )
        .expect("insert action");

        let action = list_recent_actions_for_workspace(&db, "ws", 10)
            .expect("list actions")
            .into_iter()
            .next()
            .expect("one action");
        assert_eq!(action.replay_kind.as_deref(), Some("prompt_override"));
        assert_eq!(
            action.replayed_from_action_id.as_deref(),
            Some("source-action-1")
        );
    }
}
