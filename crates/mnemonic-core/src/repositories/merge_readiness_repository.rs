use rusqlite::{params, OptionalExtension};

use crate::db::Database;
use crate::models::WorkspaceMergeReadiness;

pub fn get(db: &Database, workspace_id: &str) -> Result<Option<WorkspaceMergeReadiness>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT workspace_id, merge_ready, readiness_level, reasons, warnings,
                       ahead_count, behind_count, active_run_status, review_risk_level, 
                       pre_flight_checks, generated_at
                FROM merge_readiness
                WHERE workspace_id = ?1
                "#,
                params![workspace_id],
                |row| {
                    let reasons: String = row.get("reasons")?;
                    let warnings: String = row.get("warnings")?;
                    let pre_flight_checks: String = row.get("pre_flight_checks")?;
                    Ok(WorkspaceMergeReadiness {
                        workspace_id: row.get("workspace_id")?,
                        merge_ready: row.get::<_, i64>("merge_ready")? != 0,
                        readiness_level: row.get("readiness_level")?,
                        reasons: serde_json::from_str(&reasons).unwrap_or_default(),
                        warnings: serde_json::from_str(&warnings).unwrap_or_default(),
                        ahead_count: row.get("ahead_count")?,
                        behind_count: row.get("behind_count")?,
                        active_run_status: row.get("active_run_status")?,
                        review_risk_level: row.get("review_risk_level")?,
                        pre_flight_checks: serde_json::from_str(&pre_flight_checks)
                            .unwrap_or_default(),
                        generated_at: row.get("generated_at")?,
                    })
                },
            )
            .optional()
    })
}

pub fn upsert(db: &Database, readiness: &WorkspaceMergeReadiness) -> Result<(), String> {
    let reasons = serde_json::to_string(&readiness.reasons)
        .map_err(|err| format!("Failed to serialize readiness reasons: {err}"))?;
    let warnings = serde_json::to_string(&readiness.warnings)
        .map_err(|err| format!("Failed to serialize readiness warnings: {err}"))?;
    let pre_flight_checks = serde_json::to_string(&readiness.pre_flight_checks)
        .map_err(|err| format!("Failed to serialize pre_flight_checks: {err}"))?;

    db.with_connection(|connection| {
        connection.execute(
            r#"
            INSERT INTO merge_readiness (
                workspace_id, merge_ready, readiness_level, reasons, warnings,
                ahead_count, behind_count, active_run_status, review_risk_level, 
                pre_flight_checks, generated_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, CURRENT_TIMESTAMP)
            ON CONFLICT(workspace_id) DO UPDATE SET
                merge_ready = excluded.merge_ready,
                readiness_level = excluded.readiness_level,
                reasons = excluded.reasons,
                warnings = excluded.warnings,
                ahead_count = excluded.ahead_count,
                behind_count = excluded.behind_count,
                active_run_status = excluded.active_run_status,
                review_risk_level = excluded.review_risk_level,
                pre_flight_checks = excluded.pre_flight_checks,
                generated_at = excluded.generated_at,
                updated_at = CURRENT_TIMESTAMP
            "#,
            params![
                readiness.workspace_id,
                readiness.merge_ready as i64,
                readiness.readiness_level,
                reasons,
                warnings,
                readiness.ahead_count,
                readiness.behind_count,
                readiness.active_run_status,
                readiness.review_risk_level,
                pre_flight_checks,
                readiness.generated_at,
            ],
        )?;
        Ok(())
    })
}
