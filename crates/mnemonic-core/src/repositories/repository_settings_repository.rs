use rusqlite::{params, OptionalExtension};

use crate::db::Database;

pub fn get_value(db: &Database, repository_id: &str, key: &str) -> Result<Option<String>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                "SELECT value FROM repository_settings WHERE repository_id = ?1 AND key = ?2",
                params![repository_id, key],
                |row| row.get(0),
            )
            .optional()
    })
}

pub fn set_value(db: &Database, repository_id: &str, key: &str, value: &str) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            "INSERT INTO repository_settings (repository_id, key, value, updated_at)
             VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)
             ON CONFLICT(repository_id, key) DO UPDATE SET
                 value = excluded.value,
                 updated_at = CURRENT_TIMESTAMP",
            params![repository_id, key, value],
        )?;
        Ok(())
    })
}

pub fn delete_value(db: &Database, repository_id: &str, key: &str) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            "DELETE FROM repository_settings WHERE repository_id = ?1 AND key = ?2",
            params![repository_id, key],
        )?;
        Ok(())
    })
}
