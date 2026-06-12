use rusqlite::params;

use crate::db::Database;
use crate::models::AgentPromptEntry;

pub fn insert_prompt_entry(db: &Database, entry: &AgentPromptEntry) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            INSERT INTO terminal_prompt_entries (
                id, workspace_id, session_id, profile, prompt, status, created_at, sent_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, CURRENT_TIMESTAMP)
            "#,
            params![
                entry.id,
                entry.workspace_id,
                entry.session_id,
                entry.profile,
                entry.prompt,
                entry.status,
                entry.created_at,
                entry.sent_at,
            ],
        )?;
        Ok(())
    })
}

pub fn mark_prompt_sent(
    db: &Database,
    prompt_id: &str,
    session_id: &str,
    sent_at: &str,
) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            UPDATE terminal_prompt_entries
            SET status = 'sent',
                session_id = ?2,
                sent_at = ?3,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?1
            "#,
            params![prompt_id, session_id, sent_at],
        )?;
        Ok(())
    })
}

pub fn mark_prompt_status_by_session(
    db: &Database,
    session_id: &str,
    status: &str,
) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            UPDATE terminal_prompt_entries
            SET status = ?2,
                updated_at = CURRENT_TIMESTAMP
            WHERE session_id = ?1 AND status IN ('sent', 'running')
            "#,
            params![session_id, status],
        )?;
        Ok(())
    })
}

pub fn list_prompts_for_workspace(
    db: &Database,
    workspace_id: &str,
    limit: Option<u32>,
) -> Result<Vec<AgentPromptEntry>, String> {
    db.with_connection(|connection| {
        let limit = limit.unwrap_or(50) as i64;
        let mut statement = connection.prepare(
            r#"
            SELECT id, workspace_id, session_id, profile, prompt, status, created_at, sent_at
            FROM terminal_prompt_entries
            WHERE workspace_id = ?1
            ORDER BY created_at DESC
            LIMIT ?2
            "#,
        )?;
        let entries = statement
            .query_map(params![workspace_id, limit], |row| {
                Ok(AgentPromptEntry {
                    id: row.get("id")?,
                    workspace_id: row.get("workspace_id")?,
                    session_id: row.get("session_id")?,
                    profile: row.get("profile")?,
                    prompt: row.get("prompt")?,
                    status: row.get("status")?,
                    created_at: row.get("created_at")?,
                    sent_at: row.get("sent_at")?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(entries)
    })
}

pub fn mark_prompt_failed(db: &Database, prompt_id: &str) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            UPDATE terminal_prompt_entries
            SET status = 'failed',
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?1
            "#,
            params![prompt_id],
        )?;
        Ok(())
    })
}

/// Prompts no longer wait in an app-level queue (agent TUIs queue natively).
/// Mark any leftover 'queued' rows from older builds as stale on startup so
/// they never auto-fire and never show up in attention badges.
pub fn mark_all_queued_prompts_stale(db: &Database) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            UPDATE terminal_prompt_entries
            SET status = 'stale',
                updated_at = CURRENT_TIMESTAMP
            WHERE status = 'queued'
            "#,
            [],
        )?;
        Ok(())
    })
}
