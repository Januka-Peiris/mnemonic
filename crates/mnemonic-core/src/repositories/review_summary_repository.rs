use rusqlite::{params, OptionalExtension};

use crate::db::Database;
use crate::models::{FileReviewInsight, WorkspaceReviewSummary};

pub fn get(db: &Database, workspace_id: &str) -> Result<Option<WorkspaceReviewSummary>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT workspace_id, summary, risk_level, risk_reasons, files_changed,
                       files_flagged, additions, deletions, generated_at, file_insights
                FROM review_summaries
                WHERE workspace_id = ?1
                "#,
                params![workspace_id],
                |row| {
                    let risk_reasons_json: String = row.get("risk_reasons")?;
                    let file_insights_json: String = row.get("file_insights")?;
                    Ok(WorkspaceReviewSummary {
                        workspace_id: row.get("workspace_id")?,
                        summary: row.get("summary")?,
                        risk_level: row.get("risk_level")?,
                        risk_reasons: serde_json::from_str(&risk_reasons_json).unwrap_or_default(),
                        files_changed: row.get("files_changed")?,
                        files_flagged: row.get("files_flagged")?,
                        additions: row.get("additions")?,
                        deletions: row.get("deletions")?,
                        generated_at: row.get("generated_at")?,
                        file_insights: serde_json::from_str::<Vec<FileReviewInsight>>(
                            &file_insights_json,
                        )
                        .unwrap_or_default(),
                    })
                },
            )
            .optional()
    })
}

pub fn upsert(db: &Database, summary: &WorkspaceReviewSummary) -> Result<(), String> {
    let risk_reasons = serde_json::to_string(&summary.risk_reasons)
        .map_err(|err| format!("Failed to serialize risk reasons: {err}"))?;
    let file_insights = serde_json::to_string(&summary.file_insights)
        .map_err(|err| format!("Failed to serialize file insights: {err}"))?;

    db.with_connection(|connection| {
        connection.execute(
            r#"
            INSERT INTO review_summaries (
                workspace_id, summary, risk_level, risk_reasons, files_changed,
                files_flagged, additions, deletions, generated_at, file_insights, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, CURRENT_TIMESTAMP)
            ON CONFLICT(workspace_id) DO UPDATE SET
                summary = excluded.summary,
                risk_level = excluded.risk_level,
                risk_reasons = excluded.risk_reasons,
                files_changed = excluded.files_changed,
                files_flagged = excluded.files_flagged,
                additions = excluded.additions,
                deletions = excluded.deletions,
                generated_at = excluded.generated_at,
                file_insights = excluded.file_insights,
                updated_at = CURRENT_TIMESTAMP
            "#,
            params![
                summary.workspace_id,
                summary.summary,
                summary.risk_level,
                risk_reasons,
                summary.files_changed,
                summary.files_flagged,
                summary.additions,
                summary.deletions,
                summary.generated_at,
                file_insights,
            ],
        )?;
        Ok(())
    })
}
