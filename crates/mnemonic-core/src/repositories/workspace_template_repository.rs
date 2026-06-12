use rusqlite::params;

use crate::db::Database;
use crate::models::WorkspaceTemplate;

pub fn list(db: &Database) -> Result<Vec<WorkspaceTemplate>, String> {
    db.with_connection(|connection| {
        let mut stmt = connection.prepare(
            "SELECT id, name, description, task_prompt, agent, created_at \
             FROM workspace_templates ORDER BY created_at DESC",
        )?;
        let templates = stmt
            .query_map([], |row| {
                Ok(WorkspaceTemplate {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    task_prompt: row.get(3)?,
                    agent: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(templates)
    })
}

pub fn create(
    db: &Database,
    id: &str,
    name: &str,
    description: &str,
    task_prompt: &str,
    agent: &str,
) -> Result<WorkspaceTemplate, String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "INSERT INTO workspace_templates (id, name, description, task_prompt, agent) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, name, description, task_prompt, agent],
        )?;
        let template = connection.query_row(
            "SELECT id, name, description, task_prompt, agent, created_at \
             FROM workspace_templates WHERE id = ?1",
            params![id],
            |row| {
                Ok(WorkspaceTemplate {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    task_prompt: row.get(3)?,
                    agent: row.get(4)?,
                    created_at: row.get(5)?,
                })
            },
        )?;
        Ok(template)
    })
}

pub fn delete(db: &Database, id: &str) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute("DELETE FROM workspace_templates WHERE id = ?1", params![id])?;
        Ok(())
    })
}
