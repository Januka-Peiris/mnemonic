use rusqlite::{params, OptionalExtension};

use crate::db::Database;
use crate::models::{CreateRepositoryRelationshipInput, UpdateRepositoryRelationshipInput};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRepositoryRelationshipRow {
    pub id: String,
    pub from_repo_id: String,
    pub to_repo_id: String,
    pub kind: String,
    pub label: Option<String>,
    pub notes: Option<String>,
}

pub fn list(db: &Database) -> Result<Vec<AppRepositoryRelationshipRow>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            r#"
            SELECT id, from_repo_id, to_repo_id, kind, label, notes
            FROM app_repository_relationships
            ORDER BY updated_at DESC, created_at DESC
            "#,
        )?;
        let rows = statement
            .query_map([], row_from_query)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

pub fn get(db: &Database, id: &str) -> Result<Option<AppRepositoryRelationshipRow>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT id, from_repo_id, to_repo_id, kind, label, notes
                FROM app_repository_relationships
                WHERE id = ?1
                "#,
                params![id],
                row_from_query,
            )
            .optional()
    })
}

pub fn insert(
    db: &Database,
    id: &str,
    input: &CreateRepositoryRelationshipInput,
) -> Result<AppRepositoryRelationshipRow, String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            r#"
            INSERT INTO app_repository_relationships
                (id, from_repo_id, to_repo_id, kind, label, notes, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT(from_repo_id, to_repo_id, kind) DO UPDATE SET
                label = excluded.label,
                notes = excluded.notes,
                updated_at = CURRENT_TIMESTAMP
            "#,
            params![
                id,
                input.from_repo_id,
                input.to_repo_id,
                input.kind,
                input.label,
                input.notes,
            ],
        )?;
        Ok(())
    })?;
    find_by_unique(db, &input.from_repo_id, &input.to_repo_id, &input.kind)?
        .ok_or_else(|| "Repository relationship was not saved".to_string())
}

pub fn update(
    db: &Database,
    input: &UpdateRepositoryRelationshipInput,
) -> Result<AppRepositoryRelationshipRow, String> {
    let updated = db.with_connection_mut(|connection| {
        connection.execute(
            r#"
            UPDATE app_repository_relationships
            SET from_repo_id = ?2,
                to_repo_id = ?3,
                kind = ?4,
                label = ?5,
                notes = ?6,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?1
            "#,
            params![
                input.id,
                input.from_repo_id,
                input.to_repo_id,
                input.kind,
                input.label,
                input.notes,
            ],
        )
    })?;
    if updated == 0 {
        return Err("Repository relationship not found".to_string());
    }
    get(db, &input.id)?.ok_or_else(|| "Repository relationship not found".to_string())
}

pub fn delete(db: &Database, id: &str) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "DELETE FROM app_repository_relationships WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    })
}

fn find_by_unique(
    db: &Database,
    from_repo_id: &str,
    to_repo_id: &str,
    kind: &str,
) -> Result<Option<AppRepositoryRelationshipRow>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT id, from_repo_id, to_repo_id, kind, label, notes
                FROM app_repository_relationships
                WHERE from_repo_id = ?1 AND to_repo_id = ?2 AND kind = ?3
                "#,
                params![from_repo_id, to_repo_id, kind],
                row_from_query,
            )
            .optional()
    })
}

fn row_from_query(row: &rusqlite::Row<'_>) -> rusqlite::Result<AppRepositoryRelationshipRow> {
    Ok(AppRepositoryRelationshipRow {
        id: row.get("id")?,
        from_repo_id: row.get("from_repo_id")?,
        to_repo_id: row.get("to_repo_id")?,
        kind: row.get("kind")?,
        label: row.get("label")?,
        notes: row.get("notes")?,
    })
}
