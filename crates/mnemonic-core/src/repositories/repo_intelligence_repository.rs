use rusqlite::{params, OptionalExtension};

use crate::db::Database;

#[derive(Debug, Clone)]
pub struct RepoIntelligenceState {
    pub repo_id: String,
    pub repository_path: String,
    pub default_branch: String,
    pub ref_name: String,
    pub indexed_commit: String,
    pub artifact_path: String,
    pub files_indexed: u32,
    pub symbol_count: u32,
    pub edge_count: u32,
    pub generated_at: String,
    pub refreshed_at: String,
    pub stale: bool,
    pub last_error: Option<String>,
}

pub fn get(db: &Database, repo_id: &str) -> Result<Option<RepoIntelligenceState>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT repo_id, repository_path, default_branch, ref_name, indexed_commit,
                       artifact_path, files_indexed, symbol_count, edge_count,
                       generated_at, refreshed_at, stale, last_error
                FROM repo_intelligence_state
                WHERE repo_id = ?1
                "#,
                params![repo_id],
                |row| {
                    Ok(RepoIntelligenceState {
                        repo_id: row.get("repo_id")?,
                        repository_path: row.get("repository_path")?,
                        default_branch: row.get("default_branch")?,
                        ref_name: row.get("ref_name")?,
                        indexed_commit: row.get("indexed_commit")?,
                        artifact_path: row.get("artifact_path")?,
                        files_indexed: row.get::<_, i64>("files_indexed")?.max(0) as u32,
                        symbol_count: row.get::<_, i64>("symbol_count")?.max(0) as u32,
                        edge_count: row.get::<_, i64>("edge_count")?.max(0) as u32,
                        generated_at: row.get("generated_at")?,
                        refreshed_at: row.get("refreshed_at")?,
                        stale: row.get::<_, i64>("stale")? != 0,
                        last_error: row.get("last_error")?,
                    })
                },
            )
            .optional()
    })
}

pub fn upsert_success(db: &Database, state: &RepoIntelligenceState) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            r#"
            INSERT INTO repo_intelligence_state (
                repo_id, repository_path, default_branch, ref_name, indexed_commit,
                artifact_path, files_indexed, symbol_count, edge_count,
                generated_at, refreshed_at, stale, last_error, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9,
                ?10, ?11, ?12, NULL, CURRENT_TIMESTAMP
            )
            ON CONFLICT(repo_id) DO UPDATE SET
                repository_path = excluded.repository_path,
                default_branch = excluded.default_branch,
                ref_name = excluded.ref_name,
                indexed_commit = excluded.indexed_commit,
                artifact_path = excluded.artifact_path,
                files_indexed = excluded.files_indexed,
                symbol_count = excluded.symbol_count,
                edge_count = excluded.edge_count,
                generated_at = excluded.generated_at,
                refreshed_at = excluded.refreshed_at,
                stale = excluded.stale,
                last_error = NULL,
                updated_at = CURRENT_TIMESTAMP
            "#,
            params![
                state.repo_id,
                state.repository_path,
                state.default_branch,
                state.ref_name,
                state.indexed_commit,
                state.artifact_path,
                state.files_indexed as i64,
                state.symbol_count as i64,
                state.edge_count as i64,
                state.generated_at,
                state.refreshed_at,
                state.stale as i64,
            ],
        )?;
        Ok(())
    })
}

pub fn mark_error(
    db: &Database,
    repo_id: &str,
    repository_path: &str,
    default_branch: &str,
    ref_name: &str,
    indexed_commit: &str,
    last_error: &str,
) -> Result<(), String> {
    let now = timestamp();
    db.with_connection(|connection| {
        connection.execute(
            r#"
            INSERT INTO repo_intelligence_state (
                repo_id, repository_path, default_branch, ref_name, indexed_commit,
                artifact_path, files_indexed, symbol_count, edge_count,
                generated_at, refreshed_at, stale, last_error, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                '', 0, 0, 0,
                ?6, ?6, 1, ?7, CURRENT_TIMESTAMP
            )
            ON CONFLICT(repo_id) DO UPDATE SET
                repository_path = excluded.repository_path,
                default_branch = excluded.default_branch,
                ref_name = excluded.ref_name,
                indexed_commit = excluded.indexed_commit,
                refreshed_at = excluded.refreshed_at,
                stale = 1,
                last_error = excluded.last_error,
                updated_at = CURRENT_TIMESTAMP
            "#,
            params![
                repo_id,
                repository_path,
                default_branch,
                ref_name,
                indexed_commit,
                now,
                last_error,
            ],
        )?;
        Ok(())
    })
}

fn timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
