use rusqlite::params;

use crate::db::Database;
use crate::models::WorkspaceRun;

#[derive(Debug, Clone)]
pub struct StartupAbandonedRunGroup {
    pub workspace_id: String,
    pub repo: String,
    pub branch: String,
    pub count: u32,
}

pub fn list_runs_for_workspace(
    db: &Database,
    workspace_id: &str,
) -> Result<Vec<WorkspaceRun>, String> {
    db.with_connection(|connection| {
        let mut stmt = connection.prepare(
            r#"
            SELECT id, workspace_id, agent_type, command, args, cwd, status, pid,
                   started_at, finished_at, exit_code, error_message
            FROM workspace_runs
            WHERE workspace_id = ?1
            ORDER BY created_at DESC, rowid DESC
            "#,
        )?;
        let runs = stmt
            .query_map(params![workspace_id], run_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(runs)
    })
}

pub fn mark_stale_running_abandoned(db: &Database, timestamp: &str) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            UPDATE workspace_runs
            SET status = 'abandoned',
                finished_at = ?1,
                error_message = 'Mnemonic restarted before this run completed',
                updated_at = CURRENT_TIMESTAMP
            WHERE status = 'running'
            "#,
            params![timestamp],
        )?;
        Ok(())
    })
}

pub fn list_running_run_groups(db: &Database) -> Result<Vec<StartupAbandonedRunGroup>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            r#"
            SELECT wr.workspace_id, w.repo, w.branch, COUNT(*) as run_count
            FROM workspace_runs wr
            JOIN workspaces w ON w.id = wr.workspace_id
            WHERE wr.status = 'running'
            GROUP BY wr.workspace_id, w.repo, w.branch
            "#,
        )?;
        let groups = statement
            .query_map([], |row| {
                Ok(StartupAbandonedRunGroup {
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

fn run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRun> {
    let args_json: String = row.get("args")?;
    Ok(WorkspaceRun {
        id: row.get("id")?,
        workspace_id: row.get("workspace_id")?,
        agent_type: row.get("agent_type")?,
        command: row.get("command")?,
        args: serde_json::from_str(&args_json).unwrap_or_default(),
        cwd: row.get("cwd")?,
        status: row.get("status")?,
        pid: row.get("pid")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        exit_code: row.get("exit_code")?,
        error_message: row.get("error_message")?,
    })
}
