use rusqlite::params;

use crate::db::Database;
use crate::models::OrchestratorAction;

pub fn insert_log(
    db: &Database,
    run_at: &str,
    model: &str,
    workspace_ids: &[String],
    actions: &[OrchestratorAction],
) -> Result<(), String> {
    let ws_json = serde_json::to_string(workspace_ids).unwrap_or_else(|_| "[]".to_string());
    let actions_json = serde_json::to_string(actions).unwrap_or_else(|_| "[]".to_string());
    let id = format!("orch-{run_at}");
    db.with_connection_mut(|connection| {
        connection.execute(
            "INSERT OR REPLACE INTO orchestrator_log (id, run_at, model, workspace_ids, actions)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, run_at, model, ws_json, actions_json],
        )?;
        Ok(())
    })
}

pub fn get_last_run(db: &Database) -> Result<Option<(String, Vec<OrchestratorAction>)>, String> {
    use rusqlite::OptionalExtension;
    db.with_connection(|connection| {
        let result: Option<(String, String)> = connection
            .query_row(
                "SELECT run_at, actions FROM orchestrator_log ORDER BY run_at DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(result.map(|(run_at, actions_json)| {
            let actions: Vec<OrchestratorAction> =
                serde_json::from_str(&actions_json).unwrap_or_default();
            (run_at, actions)
        }))
    })
}

pub fn save_setting(db: &Database, key: &str, value: &str) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, CURRENT_TIMESTAMP)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP",
            params![key, value],
        )?;
        Ok(())
    })
}

pub fn load_setting(db: &Database, key: &str) -> Result<Option<String>, String> {
    use rusqlite::OptionalExtension;
    db.with_connection(|connection| {
        connection
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
    })
}
