use rusqlite::{params, OptionalExtension};

use crate::models::WorkspaceAttention;
use crate::repositories::{terminal_repository, workspace_repository};
use crate::state::AppState;

pub fn list_workspace_attention(state: &AppState) -> Result<Vec<WorkspaceAttention>, String> {
    let workspaces = workspace_repository::list(&state.db)?;
    let mut attention = Vec::with_capacity(workspaces.len());
    for workspace in workspaces {
        let sessions = terminal_repository::list_visible_for_workspace(&state.db, &workspace.id)?;
        let running_count = sessions
            .iter()
            .filter(|session| session.status == "running")
            .count() as i64;
        let has_error = sessions.iter().any(|session| {
            session.stale || matches!(session.status.as_str(), "failed" | "interrupted")
        });
        let has_complete = !sessions.is_empty()
            && sessions.iter().any(|session| session.status == "succeeded")
            && running_count == 0;
        let status = derive_status(&workspace.status, running_count, has_error, has_complete);
        let unread_count = unread_count_for_workspace(state, &workspace.id)?;
        let queued_count = queued_count_for_workspace(state, &workspace.id)?;
        let (last_event, last_event_at) = last_output_for_workspace(state, &workspace.id)?;
        attention.push(WorkspaceAttention {
            workspace_id: workspace.id,
            status,
            running_count,
            unread_count,
            queued_count,
            last_event,
            last_event_at,
        });
    }
    Ok(attention)
}

pub fn mark_workspace_attention_read(state: &AppState, workspace_id: &str) -> Result<(), String> {
    workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    state.db.with_connection(|connection| {
        connection.execute(
            r#"
            INSERT INTO workspace_attention_reads (workspace_id, last_read_at, updated_at)
            VALUES (?1, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT(workspace_id) DO UPDATE SET
                last_read_at = excluded.last_read_at,
                updated_at = CURRENT_TIMESTAMP
            "#,
            params![workspace_id],
        )?;
        Ok(())
    })
}

fn unread_count_for_workspace(state: &AppState, workspace_id: &str) -> Result<i64, String> {
    state.db.with_connection(|connection| {
        let count = connection.query_row(
            r#"
            SELECT COUNT(*)
            FROM terminal_output_chunks chunks
            JOIN terminal_sessions sessions ON sessions.id = chunks.session_id
            LEFT JOIN workspace_attention_reads reads ON reads.workspace_id = sessions.workspace_id
            WHERE sessions.workspace_id = ?1
              AND sessions.is_visible = 1
              AND sessions.closed_at IS NULL
              AND chunks.stream_type != 'pty_snapshot'
              AND (reads.last_read_at IS NULL OR chunks.created_at > reads.last_read_at)
            "#,
            params![workspace_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(count)
    })
}

fn queued_count_for_workspace(state: &AppState, workspace_id: &str) -> Result<i64, String> {
    state.db.with_connection(|connection| {
        let count = connection.query_row(
            r#"
            SELECT COUNT(*)
            FROM terminal_prompt_entries
            WHERE workspace_id = ?1 AND status = 'queued'
            "#,
            params![workspace_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(count)
    })
}

fn last_output_for_workspace(
    state: &AppState,
    workspace_id: &str,
) -> Result<(Option<String>, Option<String>), String> {
    state.db.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT chunks.data, chunks.created_at
                FROM terminal_output_chunks chunks
                JOIN terminal_sessions sessions ON sessions.id = chunks.session_id
                WHERE sessions.workspace_id = ?1
                ORDER BY chunks.created_at DESC, chunks.seq DESC
                LIMIT 1
                "#,
                params![workspace_id],
                |row| {
                    Ok((
                        Some(trim_event(row.get::<_, String>(0)?)),
                        Some(row.get::<_, String>(1)?),
                    ))
                },
            )
            .optional()
            .map(|row| row.unwrap_or((None, None)))
    })
}

pub fn derive_status(
    workspace_status: &str,
    running_count: i64,
    has_error: bool,
    has_complete: bool,
) -> String {
    if has_error || workspace_status == "Blocked" {
        "error".to_string()
    } else if running_count > 0 || workspace_status == "Running" {
        "running".to_string()
    } else if has_complete || workspace_status == "Review Ready" || workspace_status == "Merged" {
        "complete".to_string()
    } else if workspace_status == "Waiting" {
        "waiting".to_string()
    } else {
        "idle".to_string()
    }
}

fn trim_event(value: String) -> String {
    let clean = value
        .replace(['\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if clean.chars().count() > 140 {
        format!("{}…", clean.chars().take(139).collect::<String>())
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_attention_status() {
        assert_eq!(derive_status("Waiting", 0, false, false), "waiting");
        assert_eq!(derive_status("Waiting", 1, false, false), "running");
        assert_eq!(derive_status("Waiting", 0, true, false), "error");
        assert_eq!(derive_status("Review Ready", 0, false, false), "complete");
        assert_eq!(derive_status("Other", 0, false, false), "idle");
    }

    #[test]
    fn trims_last_event() {
        assert_eq!(trim_event("hello\r\nworld".to_string()), "hello world");
        assert!(trim_event("x".repeat(200)).len() <= 142);
    }
}
