use rusqlite::{params, OptionalExtension, Row};

use crate::db::Database;
use crate::models::{
    AgentSessionSummary, BranchHealth, ChangedFile, LinkedWorktreeRef, WorkspaceDetail,
    WorkspaceSummary,
};

pub fn list(db: &Database) -> Result<Vec<WorkspaceSummary>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            r#"
            SELECT *
            FROM workspaces
            ORDER BY rowid DESC
            "#,
        )?;

        let workspaces = statement
            .query_map([], |row| workspace_summary_from_row(row, connection))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(workspaces)
    })
}

pub fn get_detail(db: &Database, id: &str) -> Result<Option<WorkspaceDetail>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                "SELECT * FROM workspaces WHERE id = ?1",
                params![id],
                |row| workspace_detail_from_row(row, connection),
            )
            .optional()
    })
}

pub fn get(db: &Database, id: &str) -> Result<WorkspaceSummary, String> {
    db.with_connection(|connection| {
        connection.query_row(
            "SELECT * FROM workspaces WHERE id = ?1",
            params![id],
            |row| workspace_summary_from_row(row, connection),
        )
    })
}

pub fn set_cost_limit(
    db: &Database,
    workspace_id: &str,
    limit_usd: Option<f64>,
) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "UPDATE workspaces SET cost_limit_usd = ?1 WHERE id = ?2",
            params![limit_usd, workspace_id],
        )?;
        Ok(())
    })
}

pub fn delete(db: &Database, id: &str) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute("DELETE FROM workspaces WHERE id = ?1", params![id])?;
        Ok(())
    })
}

/// Update token count and estimated cost on the workspace's agent session fields.
/// Called from the terminal reader thread whenever cost output is detected.
pub fn update_agent_session_cost(
    db: &Database,
    workspace_id: &str,
    token_count: u32,
    estimated_cost: &str,
) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            r#"UPDATE workspaces
               SET agent_session_token_count = ?1,
                   agent_session_estimated_cost = ?2
               WHERE id = ?3"#,
            params![token_count, estimated_cost, workspace_id],
        )?;
        Ok(())
    })
}

pub fn update_last_rebase(
    db: &Database,
    workspace_id: &str,
    timestamp: &str,
) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "UPDATE workspaces SET last_rebase = ?1 WHERE id = ?2",
            params![timestamp, workspace_id],
        )?;
        Ok(())
    })
}

pub fn update_pr_status(
    db: &Database,
    workspace_id: &str,
    pr_status: &str,
    pr_number: Option<i64>,
) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            r#"UPDATE workspaces SET pr_status = ?1, pr_number = ?2 WHERE id = ?3"#,
            params![pr_status, pr_number, workspace_id],
        )?;
        Ok(())
    })
}

pub fn next_workspace_id(_db: &Database) -> Result<String, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(format!("ws-{now}"))
}

pub fn insert(db: &Database, detail: &WorkspaceDetail) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        let transaction = connection.transaction()?;
        insert_with_transaction(&transaction, detail)?;
        transaction.commit()
    })
}

/// Replace the cached changed-files rows for a workspace with freshly computed
/// ones (see git_review_service::get_workspace_changed_files). Keeps sidebar
/// counters and conflict detection in sync with the real git state.
pub fn replace_changed_files(
    db: &Database,
    workspace_id: &str,
    files: &[ChangedFile],
) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        let transaction = connection.transaction()?;
        write_changed_files(&transaction, workspace_id, files)?;
        transaction.commit()
    })
}

fn write_changed_files(
    transaction: &rusqlite::Transaction<'_>,
    workspace_id: &str,
    files: &[ChangedFile],
) -> rusqlite::Result<()> {
    transaction.execute(
        "DELETE FROM changed_files WHERE workspace_id = ?1",
        params![workspace_id],
    )?;

    for (index, file) in files.iter().enumerate() {
        transaction.execute(
            r#"
            INSERT INTO changed_files (workspace_id, path, additions, deletions, status, sort_order)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                workspace_id,
                file.path,
                file.additions,
                file.deletions,
                file.status,
                index as i64,
            ],
        )?;
    }

    Ok(())
}

fn insert_with_transaction(
    transaction: &rusqlite::Transaction<'_>,
    detail: &WorkspaceDetail,
) -> rusqlite::Result<()> {
    let summary = &detail.summary;
    let completed_steps =
        serde_json::to_string(&summary.completed_steps).unwrap_or_else(|_| "[]".to_string());
    let recent_events =
        serde_json::to_string(&detail.recent_events).unwrap_or_else(|_| "[]".to_string());

    transaction.execute(
        r#"
        INSERT OR REPLACE INTO workspaces (
            id, name, repo, branch, agent, status, current_step, completed_steps,
            last_updated, pr_status, pr_number, description, current_task,
            ahead_by, behind_by, merge_risk, last_rebase, base_branch,
            agent_session_id, agent_session_agent, agent_session_status, agent_session_model,
            agent_session_token_count, agent_session_estimated_cost, agent_session_last_message,
            agent_session_started_at, repository_id, repository_path, selected_branch,
            selected_worktree_id, selected_worktree_path, workspace_root_path,
            worktree_managed_by_forge, workspace_source, parent_workspace_id, source_workspace_id,
            derived_from_branch, run_tests_on_create, create_pr_on_complete,
            worktree_path, recent_events, updated_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
            ?9, ?10, ?11, ?12, ?13,
            ?14, ?15, ?16, ?17, ?18,
            ?19, ?20, ?21, ?22,
            ?23, ?24, ?25,
            ?26, ?27, ?28, ?29,
            ?30, ?31, ?32,
            ?33, ?34, ?35, ?36, ?37, ?38, ?39,
            ?40, ?41, CURRENT_TIMESTAMP
        )
        "#,
        params![
            summary.id,
            summary.name,
            summary.repo,
            summary.branch,
            summary.agent,
            summary.status,
            summary.current_step,
            completed_steps,
            summary.last_updated,
            summary.pr_status,
            summary.pr_number,
            summary.description,
            summary.current_task,
            summary.branch_health.ahead_by,
            summary.branch_health.behind_by,
            summary.branch_health.merge_risk,
            summary.branch_health.last_rebase,
            summary.branch_health.base_branch,
            summary.agent_session.id,
            summary.agent_session.agent,
            summary.agent_session.status,
            summary.agent_session.model,
            summary.agent_session.token_count,
            summary.agent_session.estimated_cost,
            summary.agent_session.last_message,
            summary.agent_session.started_at,
            summary.repository_id,
            summary.repository_path,
            summary.selected_branch,
            summary.selected_worktree_id,
            summary.selected_worktree_path,
            summary.workspace_root_path,
            summary.worktree_managed_by_forge as i64,
            summary.workspace_source,
            summary.parent_workspace_id,
            summary.source_workspace_id,
            summary.derived_from_branch,
            summary.run_tests_on_create as i64,
            summary.create_pr_on_complete as i64,
            detail.worktree_path,
            recent_events,
        ],
    )?;

    write_changed_files(transaction, &summary.id, &summary.changed_files)?;

    Ok(())
}

fn workspace_summary_from_row(
    row: &Row<'_>,
    connection: &rusqlite::Connection,
) -> rusqlite::Result<WorkspaceSummary> {
    let id: String = row.get("id")?;
    let completed_steps_json: String = row.get("completed_steps")?;
    let completed_steps = serde_json::from_str(&completed_steps_json).unwrap_or_default();

    Ok(WorkspaceSummary {
        id: id.clone(),
        name: row.get("name")?,
        repo: row.get("repo")?,
        branch: row.get("branch")?,
        agent: row.get("agent")?,
        status: row.get("status")?,
        current_step: row.get("current_step")?,
        completed_steps,
        changed_files: list_changed_files(connection, &id)?,
        last_updated: row.get("last_updated")?,
        pr_status: row.get("pr_status")?,
        pr_number: row.get("pr_number")?,
        description: row.get("description")?,
        current_task: row.get("current_task")?,
        branch_health: BranchHealth {
            ahead_by: row.get("ahead_by")?,
            behind_by: row.get("behind_by")?,
            merge_risk: row.get("merge_risk")?,
            last_rebase: row.get("last_rebase")?,
            base_branch: row.get("base_branch")?,
        },
        agent_session: AgentSessionSummary {
            id: row.get("agent_session_id")?,
            agent: row.get("agent_session_agent")?,
            status: row.get("agent_session_status")?,
            model: row.get("agent_session_model")?,
            token_count: row.get("agent_session_token_count")?,
            estimated_cost: row.get("agent_session_estimated_cost")?,
            last_message: row.get("agent_session_last_message")?,
            started_at: row.get("agent_session_started_at")?,
        },
        repository_id: row.get("repository_id")?,
        repository_path: row.get("repository_path")?,
        selected_branch: row.get("selected_branch")?,
        selected_worktree_id: row.get("selected_worktree_id")?,
        selected_worktree_path: row.get("selected_worktree_path")?,
        workspace_root_path: row.get("workspace_root_path")?,
        worktree_managed_by_forge: row.get::<_, i64>("worktree_managed_by_forge")? != 0,
        workspace_source: row.get("workspace_source")?,
        parent_workspace_id: row.get("parent_workspace_id")?,
        source_workspace_id: row.get("source_workspace_id")?,
        derived_from_branch: row.get("derived_from_branch")?,
        linked_worktrees: list_linked_worktrees(connection, &id)?,
        cost_limit_usd: row.get("cost_limit_usd")?,
        run_tests_on_create: row.get::<_, i64>("run_tests_on_create")? != 0,
        create_pr_on_complete: row.get::<_, i64>("create_pr_on_complete")? != 0,
    })
}

fn workspace_detail_from_row(
    row: &Row<'_>,
    connection: &rusqlite::Connection,
) -> rusqlite::Result<WorkspaceDetail> {
    let recent_events_json: String = row.get("recent_events")?;
    let recent_events = serde_json::from_str(&recent_events_json).unwrap_or_default();

    Ok(WorkspaceDetail {
        summary: workspace_summary_from_row(row, connection)?,
        worktree_path: row.get("worktree_path")?,
        base_branch: row.get("base_branch")?,
        recent_events,
    })
}

fn list_changed_files(
    connection: &rusqlite::Connection,
    workspace_id: &str,
) -> rusqlite::Result<Vec<ChangedFile>> {
    let mut statement = connection.prepare(
        r#"
        SELECT path, additions, deletions, status
        FROM changed_files
        WHERE workspace_id = ?1
        ORDER BY sort_order ASC, id ASC
        "#,
    )?;

    let files = statement
        .query_map(params![workspace_id], |row| {
            Ok(ChangedFile {
                path: row.get("path")?,
                additions: row.get("additions")?,
                deletions: row.get("deletions")?,
                status: row.get("status")?,
            })
        })?
        .collect();

    files
}

fn list_linked_worktrees(
    connection: &rusqlite::Connection,
    workspace_id: &str,
) -> rusqlite::Result<Vec<LinkedWorktreeRef>> {
    let mut statement = connection.prepare(
        r#"
        SELECT wlw.worktree_id, dw.repo_id, COALESCE(r.name, dw.repo_id) AS repo_name,
               dw.path, dw.branch, dw.head
        FROM workspace_linked_worktrees wlw
        JOIN discovered_worktrees dw ON dw.id = wlw.worktree_id
        LEFT JOIN repositories r ON r.id = dw.repo_id
        WHERE wlw.workspace_id = ?1
        ORDER BY wlw.id DESC
        "#,
    )?;
    let rows = statement
        .query_map(params![workspace_id], |row| {
            Ok(LinkedWorktreeRef {
                worktree_id: row.get("worktree_id")?,
                repo_id: row.get("repo_id")?,
                repo_name: row.get("repo_name")?,
                path: row.get("path")?,
                branch: row.get("branch")?,
                head: row.get("head")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn attach_linked_worktree(
    db: &Database,
    workspace_id: &str,
    worktree_id: &str,
) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            "INSERT OR IGNORE INTO workspace_linked_worktrees (workspace_id, worktree_id) VALUES (?1, ?2)",
            params![workspace_id, worktree_id],
        )?;
        Ok(())
    })
}

pub fn detach_linked_worktree(
    db: &Database,
    workspace_id: &str,
    worktree_id: &str,
) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            "DELETE FROM workspace_linked_worktrees WHERE workspace_id = ?1 AND worktree_id = ?2",
            params![workspace_id, worktree_id],
        )?;
        Ok(())
    })
}

pub fn list_linked_worktrees_for_workspace(
    db: &Database,
    workspace_id: &str,
) -> Result<Vec<LinkedWorktreeRef>, String> {
    db.with_connection(|connection| list_linked_worktrees(connection, workspace_id))
}
