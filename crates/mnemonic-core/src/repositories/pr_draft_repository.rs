use rusqlite::{params, OptionalExtension};

use crate::db::Database;
use crate::models::WorkspacePrDraft;

pub fn get(db: &Database, workspace_id: &str) -> Result<Option<WorkspacePrDraft>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT workspace_id, title, summary, key_changes, risks, testing_notes, generated_at
                FROM pr_drafts
                WHERE workspace_id = ?1
                "#,
                params![workspace_id],
                |row| {
                    let key_changes: String = row.get("key_changes")?;
                    let risks: String = row.get("risks")?;
                    let testing_notes: String = row.get("testing_notes")?;
                    Ok(WorkspacePrDraft {
                        workspace_id: row.get("workspace_id")?,
                        title: row.get("title")?,
                        summary: row.get("summary")?,
                        key_changes: serde_json::from_str(&key_changes).unwrap_or_default(),
                        risks: serde_json::from_str(&risks).unwrap_or_default(),
                        testing_notes: serde_json::from_str(&testing_notes).unwrap_or_default(),
                        generated_at: row.get("generated_at")?,
                    })
                },
            )
            .optional()
    })
}

pub fn upsert(db: &Database, draft: &WorkspacePrDraft) -> Result<(), String> {
    let key_changes = serde_json::to_string(&draft.key_changes)
        .map_err(|err| format!("Failed to serialize PR key changes: {err}"))?;
    let risks = serde_json::to_string(&draft.risks)
        .map_err(|err| format!("Failed to serialize PR risks: {err}"))?;
    let testing_notes = serde_json::to_string(&draft.testing_notes)
        .map_err(|err| format!("Failed to serialize PR testing notes: {err}"))?;

    db.with_connection(|connection| {
        connection.execute(
            r#"
            INSERT INTO pr_drafts (
                workspace_id, title, summary, key_changes, risks, testing_notes, generated_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CURRENT_TIMESTAMP)
            ON CONFLICT(workspace_id) DO UPDATE SET
                title = excluded.title,
                summary = excluded.summary,
                key_changes = excluded.key_changes,
                risks = excluded.risks,
                testing_notes = excluded.testing_notes,
                generated_at = excluded.generated_at,
                updated_at = CURRENT_TIMESTAMP
            "#,
            params![
                draft.workspace_id,
                draft.title,
                draft.summary,
                key_changes,
                risks,
                testing_notes,
                draft.generated_at,
            ],
        )?;
        Ok(())
    })
}
