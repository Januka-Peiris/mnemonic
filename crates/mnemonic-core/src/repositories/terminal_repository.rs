use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::db::Database;
use crate::models::{TerminalOutputChunk, TerminalSession};

mod output;
mod prompts;

pub use output::{
    insert_output_chunk, insert_output_chunks, list_output_chunks, next_seq, prune_output_chunks,
};
pub use prompts::{
    insert_prompt_entry, list_prompts_for_workspace, mark_all_queued_prompts_stale,
    mark_prompt_failed, mark_prompt_sent, mark_prompt_status_by_session,
};

#[derive(Debug, Clone)]
pub struct StartupStaleTerminalGroup {
    pub workspace_id: String,
    pub repo: String,
    pub branch: String,
    pub count: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSearchResult {
    pub workspace_id: String,
    pub workspace_name: String,
    pub session_id: String,
    pub timestamp: String,
    pub line: String,
}

pub fn search_output(
    db: &Database,
    query: &str,
    workspace_id: Option<&str>,
) -> Result<Vec<TerminalSearchResult>, String> {
    db.with_connection(|connection| {
        let like = format!("%{}%", query.to_lowercase());
        if let Some(ws_id) = workspace_id {
            let mut stmt = connection.prepare(
                "SELECT toc.session_id, toc.timestamp, toc.data, ts.workspace_id, w.name \
                 FROM terminal_output_chunks toc \
                 JOIN terminal_sessions ts ON ts.id = toc.session_id \
                 JOIN workspaces w ON w.id = ts.workspace_id \
                 WHERE ts.workspace_id = ?1 AND LOWER(toc.data) LIKE ?2 \
                 ORDER BY toc.timestamp DESC LIMIT 100",
            )?;
            let results = stmt
                .query_map(params![ws_id, like], |row| {
                    Ok(TerminalSearchResult {
                        session_id: row.get(0)?,
                        timestamp: row.get(1)?,
                        line: row.get(2)?,
                        workspace_id: row.get(3)?,
                        workspace_name: row.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(results)
        } else {
            let mut stmt = connection.prepare(
                "SELECT toc.session_id, toc.timestamp, toc.data, ts.workspace_id, w.name \
                 FROM terminal_output_chunks toc \
                 JOIN terminal_sessions ts ON ts.id = toc.session_id \
                 JOIN workspaces w ON w.id = ts.workspace_id \
                 WHERE LOWER(toc.data) LIKE ?1 \
                 ORDER BY toc.timestamp DESC LIMIT 100",
            )?;
            let results = stmt
                .query_map(params![like], |row| {
                    Ok(TerminalSearchResult {
                        session_id: row.get(0)?,
                        timestamp: row.get(1)?,
                        line: row.get(2)?,
                        workspace_id: row.get(3)?,
                        workspace_name: row.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(results)
        }
    })
}

pub fn insert_session(db: &Database, session: &TerminalSession) -> Result<(), String> {
    let args = serde_json::to_string(&session.args)
        .map_err(|err| format!("Failed to serialize terminal session args: {err}"))?;
    db.with_connection(|connection| {
        connection.execute(
            r#"
            INSERT INTO terminal_sessions (
                id, workspace_id, session_role, profile, cwd, status, started_at, ended_at,
                command, args, pid, stale, closed_at, backend, tmux_session_name, title,
                terminal_kind, display_order, is_visible, last_attached_at, last_captured_seq,
                claude_session_id, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, CURRENT_TIMESTAMP)
            "#,
            params![
                session.id,
                session.workspace_id,
                session.session_role,
                session.profile,
                session.cwd,
                session.status,
                session.started_at,
                session.ended_at,
                session.command,
                args,
                session.pid,
                session.stale as i64,
                session.closed_at,
                session.backend,
                session.tmux_session_name.as_deref(),
                session.title,
                session.terminal_kind,
                session.display_order,
                session.is_visible as i64,
                session.last_attached_at,
                session.last_captured_seq,
                session.claude_session_id,
            ],
        )?;
        Ok(())
    })
}

pub fn get_session(db: &Database, session_id: &str) -> Result<Option<TerminalSession>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT id, workspace_id, session_role, profile, cwd, status, started_at, ended_at,
                       command, args, pid, stale, closed_at, backend, tmux_session_name, title, terminal_kind,
                       display_order, is_visible, last_attached_at, last_captured_seq, claude_session_id
                FROM terminal_sessions
                WHERE id = ?1
                "#,
                params![session_id],
                session_from_row,
            )
            .optional()
    })
}

pub fn latest_for_workspace_role(
    db: &Database,
    workspace_id: &str,
    session_role: &str,
) -> Result<Option<TerminalSession>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT id, workspace_id, session_role, profile, cwd, status, started_at, ended_at,
                       command, args, pid, stale, closed_at, backend, tmux_session_name, title, terminal_kind,
                       display_order, is_visible, last_attached_at, last_captured_seq, claude_session_id
                FROM terminal_sessions
                WHERE workspace_id = ?1 AND session_role = ?2 AND closed_at IS NULL
                ORDER BY created_at DESC, rowid DESC
                LIMIT 1
                "#,
                params![workspace_id, session_role],
                session_from_row,
            )
            .optional()
    })
}

pub fn list_for_workspace(
    db: &Database,
    workspace_id: &str,
) -> Result<Vec<TerminalSession>, String> {
    db.with_connection(|connection| {
        let mut stmt = connection.prepare(
            r#"
            SELECT id, workspace_id, session_role, profile, cwd, status, started_at, ended_at,
                   command, args, pid, stale, closed_at, backend, tmux_session_name, title, terminal_kind,
                   display_order, is_visible, last_attached_at, last_captured_seq, claude_session_id
            FROM terminal_sessions
            WHERE workspace_id = ?1
            ORDER BY created_at DESC, rowid DESC
            "#,
        )?;
        let sessions = stmt
            .query_map(params![workspace_id], session_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(sessions)
    })
}

pub fn list_visible_for_workspace(
    db: &Database,
    workspace_id: &str,
) -> Result<Vec<TerminalSession>, String> {
    db.with_connection(|connection| {
        let mut stmt = connection.prepare(
            r#"
            SELECT id, workspace_id, session_role, profile, cwd, status, started_at, ended_at,
                   command, args, pid, stale, closed_at, backend, tmux_session_name, title, terminal_kind,
                   display_order, is_visible, last_attached_at, last_captured_seq, claude_session_id
            FROM terminal_sessions
            WHERE workspace_id = ?1 AND closed_at IS NULL AND is_visible = 1
            ORDER BY display_order ASC, created_at ASC, rowid ASC
            "#,
        )?;
        let sessions = stmt
            .query_map(params![workspace_id], session_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(sessions)
    })
}

pub fn next_display_order(db: &Database, workspace_id: &str) -> Result<i64, String> {
    db.with_connection(|connection| {
        let next = connection.query_row(
            "SELECT COALESCE(MAX(display_order) + 1, 0) FROM terminal_sessions WHERE workspace_id = ?1",
            params![workspace_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(next)
    })
}

pub fn mark_finished(
    db: &Database,
    session_id: &str,
    status: &str,
    ended_at: &str,
    stale: bool,
) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            UPDATE terminal_sessions
            SET status = ?2,
                ended_at = COALESCE(ended_at, ?3),
                stale = ?4,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?1
            "#,
            params![session_id, status, ended_at, stale as i64],
        )?;
        Ok(())
    })
}

pub fn mark_closed(db: &Database, session_id: &str, closed_at: &str) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            UPDATE terminal_sessions
            SET closed_at = COALESCE(closed_at, ?2),
                is_visible = 0,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?1
            "#,
            params![session_id, closed_at],
        )?;
        Ok(())
    })
}

pub fn update_recovery_state(
    db: &Database,
    session_id: &str,
    status: Option<&str>,
    stale: Option<bool>,
) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            UPDATE terminal_sessions
            SET status = COALESCE(?2, status),
                stale = COALESCE(?3, stale),
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?1
            "#,
            params![session_id, status, stale.map(|value| value as i64)],
        )?;
        Ok(())
    })
}

pub fn mark_stale_running_sessions(db: &Database, timestamp: &str) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            UPDATE terminal_sessions
            SET status = 'interrupted',
                ended_at = COALESCE(ended_at, ?1),
                stale = 1,
                updated_at = CURRENT_TIMESTAMP
            WHERE status = 'running' AND COALESCE(backend, 'pty') != 'tmux'
            "#,
            params![timestamp],
        )?;
        Ok(())
    })
}

pub fn list_running_session_groups(
    db: &Database,
) -> Result<Vec<StartupStaleTerminalGroup>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            r#"
            SELECT ts.workspace_id, w.repo, w.branch, COUNT(*) as session_count
            FROM terminal_sessions ts
            JOIN workspaces w ON w.id = ts.workspace_id
            WHERE ts.status = 'running' AND COALESCE(ts.backend, 'pty') != 'tmux'
            GROUP BY ts.workspace_id, w.repo, w.branch
            "#,
        )?;
        let groups = statement
            .query_map([], |row| {
                Ok(StartupStaleTerminalGroup {
                    workspace_id: row.get(0)?,
                    repo: row.get(1)?,
                    branch: row.get(2)?,
                    count: row.get::<_, i64>(3)? as u32,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(groups)
    })
}

pub(crate) fn terminal_output_chunk_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<TerminalOutputChunk> {
    Ok(TerminalOutputChunk {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        seq: row.get::<_, i64>("seq")? as u64,
        timestamp: row.get("timestamp")?,
        stream_type: row.get("stream_type")?,
        data: row.get("data")?,
    })
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TerminalSession> {
    let args_json: String = row.get("args")?;
    Ok(TerminalSession {
        id: row.get("id")?,
        workspace_id: row.get("workspace_id")?,
        session_role: row
            .get::<_, Option<String>>("session_role")?
            .unwrap_or_else(|| "agent".to_string()),
        profile: row.get("profile")?,
        cwd: row.get("cwd")?,
        status: row.get("status")?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        command: row.get("command")?,
        args: serde_json::from_str(&args_json).unwrap_or_default(),
        pid: row.get("pid")?,
        stale: row.get::<_, i64>("stale")? != 0,
        closed_at: row.get("closed_at")?,
        backend: row
            .get::<_, Option<String>>("backend")?
            .unwrap_or_else(|| "pty".to_string()),
        tmux_session_name: row.get("tmux_session_name")?,
        title: row.get::<_, Option<String>>("title")?.unwrap_or_default(),
        terminal_kind: row
            .get::<_, Option<String>>("terminal_kind")?
            .unwrap_or_else(|| "agent".to_string()),
        display_order: row.get::<_, Option<i64>>("display_order")?.unwrap_or(0),
        is_visible: row.get::<_, Option<i64>>("is_visible")?.unwrap_or(1) != 0,
        last_attached_at: row.get("last_attached_at")?,
        last_captured_seq: row.get::<_, Option<i64>>("last_captured_seq")?.unwrap_or(0),
        claude_session_id: row.get("claude_session_id").unwrap_or(None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with_workspace() -> Database {
        let db = Database::in_memory().expect("in-memory database");
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
                    'ws-1', 'Test Workspace', 'repo', 'branch', 'Codex', 'Waiting', 'Planning', '[]',
                    'now', 'desc', 'task', 'Low', 'never', 'main',
                    'agent-session-1', 'Codex', 'idle',
                    'local', '$0.00', 'none',
                    'not started', '/tmp/ws-1', '[]'
                )
                "#,
                [],
            )?;
            Ok(())
        })
        .expect("workspace insert");
        db
    }

    fn session(id: &str, status: &str) -> TerminalSession {
        TerminalSession {
            id: id.to_string(),
            workspace_id: "ws-1".to_string(),
            session_role: "agent".to_string(),
            profile: "codex".to_string(),
            cwd: "/tmp/ws-1".to_string(),
            status: status.to_string(),
            started_at: "1".to_string(),
            ended_at: None,
            command: "codex".to_string(),
            args: vec![],
            pid: Some(1234),
            stale: false,
            closed_at: None,
            backend: "pty".to_string(),
            tmux_session_name: None,
            title: "Codex".to_string(),
            terminal_kind: "agent".to_string(),
            display_order: 0,
            is_visible: true,
            last_attached_at: None,
            last_captured_seq: 0,
            claude_session_id: None,
        }
    }

    #[test]
    fn closed_sessions_are_hidden_from_latest_but_retained_in_history() {
        let db = db_with_workspace();
        insert_session(&db, &session("term-1", "stopped")).expect("insert session");

        assert_eq!(
            latest_for_workspace_role(&db, "ws-1", "agent")
                .expect("latest")
                .map(|session| session.id),
            Some("term-1".to_string())
        );

        mark_closed(&db, "term-1", "2").expect("close session");

        assert!(latest_for_workspace_role(&db, "ws-1", "agent")
            .expect("latest")
            .is_none());
        let history = list_for_workspace(&db, "ws-1").expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, "term-1");
        assert_eq!(history[0].closed_at.as_deref(), Some("2"));
    }
}
