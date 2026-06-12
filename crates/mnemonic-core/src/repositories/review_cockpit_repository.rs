use rusqlite::{params, OptionalExtension};

use crate::db::Database;
use crate::models::{WorkspaceFileReviewState, WorkspacePrComment};

pub fn set_file_reviewed(
    db: &Database,
    workspace_id: &str,
    path: &str,
    reviewed: bool,
    reviewed_at: &str,
    notes: Option<&str>,
) -> Result<(), String> {
    db.with_connection(|connection| {
        if reviewed {
            connection.execute(
                r#"
                INSERT INTO workspace_file_reviews (workspace_id, path, status, reviewed_at, reviewed_by, notes, updated_at)
                VALUES (?1, ?2, 'reviewed', ?3, 'local', ?4, CURRENT_TIMESTAMP)
                ON CONFLICT(workspace_id, path) DO UPDATE SET
                    status = 'reviewed', reviewed_at = excluded.reviewed_at,
                    reviewed_by = 'local', notes = excluded.notes, updated_at = CURRENT_TIMESTAMP
                "#,
                params![workspace_id, path, reviewed_at, notes],
            )?;
        } else {
            connection.execute(
                r#"
                INSERT INTO workspace_file_reviews (workspace_id, path, status, reviewed_at, reviewed_by, notes, updated_at)
                VALUES (?1, ?2, 'unreviewed', NULL, 'local', ?3, CURRENT_TIMESTAMP)
                ON CONFLICT(workspace_id, path) DO UPDATE SET
                    status = 'unreviewed', reviewed_at = NULL,
                    reviewed_by = 'local', notes = excluded.notes, updated_at = CURRENT_TIMESTAMP
                "#,
                params![workspace_id, path, notes],
            )?;
        }
        Ok(())
    })
}

pub fn list_file_review_states(
    db: &Database,
    workspace_id: &str,
) -> Result<Vec<WorkspaceFileReviewState>, String> {
    db.with_connection(|connection| {
        let mut stmt = connection.prepare(
            r#"
            SELECT workspace_id, path, status, reviewed_at, reviewed_by, notes
            FROM workspace_file_reviews
            WHERE workspace_id = ?1
            ORDER BY path ASC
            "#,
        )?;
        let rows = stmt
            .query_map(params![workspace_id], file_review_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

#[cfg(test)]
pub fn get_file_review_state(
    db: &Database,
    workspace_id: &str,
    path: &str,
) -> Result<Option<WorkspaceFileReviewState>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT workspace_id, path, status, reviewed_at, reviewed_by, notes
                FROM workspace_file_reviews
                WHERE workspace_id = ?1 AND path = ?2
                "#,
                params![workspace_id, path],
                file_review_from_row,
            )
            .optional()
    })
}

pub fn upsert_pr_comments(
    db: &Database,
    workspace_id: &str,
    comments: &[WorkspacePrComment],
) -> Result<(), String> {
    db.with_connection(|connection| {
        let tx = connection.unchecked_transaction()?;
        for comment in comments {
            tx.execute(
                r#"
                INSERT INTO workspace_pr_comments (
                    workspace_id, provider, comment_id, author, body, path, line, url, state, created_at_remote, resolved_at,
                    comment_node_id, thread_id, review_id, thread_resolved, thread_outdated, thread_resolvable, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, CURRENT_TIMESTAMP)
                ON CONFLICT(workspace_id, provider, comment_id) DO UPDATE SET
                    author = excluded.author, body = excluded.body, path = excluded.path,
                    line = excluded.line, url = excluded.url, state = excluded.state,
                    created_at_remote = excluded.created_at_remote,
                    resolved_at = COALESCE(excluded.resolved_at, workspace_pr_comments.resolved_at),
                    comment_node_id = excluded.comment_node_id,
                    thread_id = excluded.thread_id,
                    review_id = excluded.review_id,
                    thread_resolved = excluded.thread_resolved,
                    thread_outdated = excluded.thread_outdated,
                    thread_resolvable = excluded.thread_resolvable,
                    updated_at = CURRENT_TIMESTAMP
                "#,
                params![
                    workspace_id,
                    comment.provider,
                    comment.comment_id,
                    comment.author,
                    comment.body,
                    comment.path,
                    comment.line,
                    comment.url,
                    comment.state,
                    comment.created_at,
                    comment.resolved_at,
                    comment.comment_node_id,
                    comment.thread_id,
                    comment.review_id,
                    comment.thread_resolved as i64,
                    comment.thread_outdated as i64,
                    comment.thread_resolvable as i64,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

pub fn list_pr_comments(
    db: &Database,
    workspace_id: &str,
) -> Result<Vec<WorkspacePrComment>, String> {
    db.with_connection(|connection| {
        let mut stmt = connection.prepare(
            r#"
            SELECT workspace_id, provider, comment_id, author, body, path, line, url, state, created_at_remote, resolved_at,
                   comment_node_id, thread_id, review_id, thread_resolved, thread_outdated, thread_resolvable
            FROM workspace_pr_comments
            WHERE workspace_id = ?1
            ORDER BY COALESCE(path, ''), line, created_at_remote DESC, comment_id
            "#,
        )?;
        let rows = stmt
            .query_map(params![workspace_id], pr_comment_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

pub fn get_pr_comment(
    db: &Database,
    workspace_id: &str,
    comment_id: &str,
) -> Result<Option<WorkspacePrComment>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT workspace_id, provider, comment_id, author, body, path, line, url, state, created_at_remote, resolved_at,
                       comment_node_id, thread_id, review_id, thread_resolved, thread_outdated, thread_resolvable
                FROM workspace_pr_comments
                WHERE workspace_id = ?1 AND comment_id = ?2
                LIMIT 1
                "#,
                params![workspace_id, comment_id],
                pr_comment_from_row,
            )
            .optional()
    })
}

pub fn mark_pr_comment_resolved_local(
    db: &Database,
    workspace_id: &str,
    comment_id: &str,
    resolved_at: &str,
) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            UPDATE workspace_pr_comments
            SET state = 'resolved_local', resolved_at = ?3, updated_at = CURRENT_TIMESTAMP
            WHERE workspace_id = ?1 AND comment_id = ?2
            "#,
            params![workspace_id, comment_id, resolved_at],
        )?;
        Ok(())
    })
}

fn file_review_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceFileReviewState> {
    Ok(WorkspaceFileReviewState {
        workspace_id: row.get("workspace_id")?,
        path: row.get("path")?,
        status: row.get("status")?,
        reviewed_at: row.get("reviewed_at")?,
        reviewed_by: row.get("reviewed_by")?,
        notes: row.get("notes")?,
    })
}

fn pr_comment_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspacePrComment> {
    Ok(WorkspacePrComment {
        workspace_id: row.get("workspace_id")?,
        provider: row.get("provider")?,
        comment_id: row.get("comment_id")?,
        author: row.get("author")?,
        body: row.get("body")?,
        path: row.get("path")?,
        line: row
            .get::<_, Option<i64>>("line")?
            .map(|value| value.max(0) as u32),
        url: row.get("url")?,
        state: row.get("state")?,
        created_at: row.get("created_at_remote")?,
        resolved_at: row.get("resolved_at")?,
        comment_node_id: row.get("comment_node_id")?,
        thread_id: row.get("thread_id")?,
        review_id: row
            .get::<_, Option<i64>>("review_id")?
            .map(|value| value.max(0) as u64),
        thread_resolved: row.get::<_, i64>("thread_resolved")? != 0,
        thread_outdated: row.get::<_, i64>("thread_outdated")? != 0,
        thread_resolvable: row.get::<_, i64>("thread_resolvable")? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upserts_review_state() {
        let db = Database::in_memory().expect("db");
        db.with_connection(|connection| {
            connection.execute(
                "INSERT INTO workspaces (id, name, repo, branch, agent, status, current_step, completed_steps, last_updated, description, current_task, merge_risk, last_rebase, base_branch, agent_session_id, agent_session_agent, agent_session_status, agent_session_model, agent_session_estimated_cost, agent_session_last_message, agent_session_started_at, worktree_path, recent_events) VALUES ('ws', 'WS', 'repo', 'b', 'Codex', 'Waiting', 'Planning', '[]', 'now', '', '', 'Low', '', 'main', '', '', '', '', '', '', '', '/tmp', '[]')",
                [],
            )?;
            Ok(())
        }).expect("workspace");
        set_file_reviewed(&db, "ws", "src/lib.rs", true, "1", None).expect("reviewed");
        let state = get_file_review_state(&db, "ws", "src/lib.rs")
            .expect("state")
            .expect("some");
        assert_eq!(state.status, "reviewed");
        assert_eq!(state.reviewed_at.as_deref(), Some("1"));
        set_file_reviewed(&db, "ws", "src/lib.rs", false, "2", Some("later")).expect("unreviewed");
        let state = get_file_review_state(&db, "ws", "src/lib.rs")
            .expect("state")
            .expect("some");
        assert_eq!(state.status, "unreviewed");
        assert_eq!(state.reviewed_at, None);
    }
}
