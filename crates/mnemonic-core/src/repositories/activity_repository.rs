use rusqlite::params;

use crate::db::Database;
use crate::models::ActivityItem;

fn item_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActivityItem> {
    Ok(ActivityItem {
        id: row.get("id")?,
        workspace_id: row.get("workspace_id")?,
        repo: row.get("repo")?,
        branch: row.get("branch")?,
        event: row.get("event")?,
        level: row.get("level")?,
        details: row.get("details")?,
        timestamp: row.get("timestamp")?,
    })
}

pub fn list(db: &Database) -> Result<Vec<ActivityItem>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, workspace_id, repo, branch, event, level, details, timestamp
             FROM activity_items ORDER BY rowid DESC",
        )?;
        let items = statement.query_map([], item_from_row)?.collect();
        items
    })
}

pub fn list_for_workspace(
    db: &Database,
    workspace_id: &str,
    limit: u32,
) -> Result<Vec<ActivityItem>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, workspace_id, repo, branch, event, level, details, timestamp
             FROM activity_items WHERE workspace_id = ?1
             ORDER BY rowid DESC LIMIT ?2",
        )?;
        let items = statement
            .query_map(params![workspace_id, limit], item_from_row)?
            .collect();
        items
    })
}

pub fn record(
    db: &Database,
    workspace_id: &str,
    repo: &str,
    branch: Option<&str>,
    event: &str,
    level: &str,
    details: Option<&str>,
) -> Result<(), String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let ts = duration.as_secs();
    let id = format!("act-{workspace_id}-{ts}-{}", duration.as_nanos());
    insert(
        db,
        &ActivityItem {
            id,
            workspace_id: Some(workspace_id.to_string()),
            repo: repo.to_string(),
            branch: branch.map(str::to_string),
            event: event.to_string(),
            level: level.to_string(),
            details: details.map(str::to_string),
            timestamp: ts.to_string(),
        },
    )
}

pub fn insert(db: &Database, item: &ActivityItem) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            INSERT OR REPLACE INTO activity_items (
                id, workspace_id, repo, branch, event, level, details, timestamp
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                item.id,
                item.workspace_id,
                item.repo,
                item.branch,
                item.event,
                item.level,
                item.details,
                item.timestamp,
            ],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_does_not_replace_same_second_workspace_events() {
        let db = Database::in_memory().expect("db");
        db.with_connection(|connection| {
            connection.execute(
                r#"
                INSERT INTO workspaces (
                    id, name, repo, branch, agent, status, current_step, completed_steps,
                    last_updated, description, current_task, merge_risk, last_rebase, base_branch,
                    agent_session_id, agent_session_agent, agent_session_status,
                    agent_session_model, agent_session_estimated_cost, agent_session_last_message,
                    agent_session_started_at, worktree_path, recent_events
                ) VALUES (
                    'ws-activity', 'Activity Workspace', 'repo', 'branch', 'Codex', 'Waiting', 'Planning', '[]',
                    'now', 'desc', 'task', 'Low', 'never', 'main',
                    'agent-session-1', 'Codex', 'idle',
                    'local', '$0.00', 'none',
                    'not started', '/tmp/ws-activity', '[]'
                )
                "#,
                [],
            )?;
            Ok(())
        })
        .expect("workspace insert");
        record(
            &db,
            "ws-activity",
            "repo",
            Some("branch"),
            "First",
            "info",
            None,
        )
        .unwrap();
        record(
            &db,
            "ws-activity",
            "repo",
            Some("branch"),
            "Second",
            "warning",
            None,
        )
        .unwrap();

        let items = list_for_workspace(&db, "ws-activity", 10).unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|item| item.event == "First"));
        assert!(items.iter().any(|item| item.event == "Second"));
    }
}
