use rusqlite::{params, OptionalExtension};

use crate::db::Database;

const REPO_ROOTS_KEY: &str = "repo_roots";
const ENV_CHECK_KEY: &str = "has_completed_env_check";
const MANAGED_WORKSPACES_ROOT_KEY: &str = "managed_workspaces_root";

pub fn get_value(db: &Database, key: &str) -> Result<Option<String>, String> {
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

pub fn set_value(db: &Database, key: &str, value: &str) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, CURRENT_TIMESTAMP)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP",
            params![key, value],
        )?;
        Ok(())
    })
}

pub fn ensure_default_value(db: &Database, key: &str, value: &str) -> Result<(), String> {
    db.with_connection(|connection| {
        connection.execute(
            "INSERT INTO settings (key, value, updated_at)
             SELECT ?1, ?2, CURRENT_TIMESTAMP
             WHERE NOT EXISTS (SELECT 1 FROM settings WHERE key = ?1)",
            params![key, value],
        )?;
        Ok(())
    })
}

pub fn get_managed_workspaces_root(db: &Database) -> Result<String, String> {
    let saved = get_value(db, MANAGED_WORKSPACES_ROOT_KEY)?;
    Ok(saved
        .map(|value| normalize_path(&value))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(default_managed_workspaces_root))
}

pub fn save_managed_workspaces_root(db: &Database, path: &str) -> Result<String, String> {
    let normalized = normalize_path(path);
    if normalized.trim().is_empty() {
        return Err("Managed workspace location is required".to_string());
    }
    set_value(db, MANAGED_WORKSPACES_ROOT_KEY, &normalized)?;
    Ok(normalized)
}

fn default_managed_workspaces_root() -> String {
    if let Some(home) = std::env::var_os("HOME") {
        return format!("{}/Mnemonic/workspaces", home.to_string_lossy());
    }
    "Mnemonic/workspaces".to_string()
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    expand_home(trimmed)
}

pub fn get_repo_roots(db: &Database) -> Result<Vec<String>, String> {
    db.with_connection(|connection| {
        let value: Option<String> = connection
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![REPO_ROOTS_KEY],
                |row| row.get(0),
            )
            .optional()?;

        let roots = value
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
            .unwrap_or_default();

        Ok(roots)
    })
}

pub fn save_repo_roots(db: &Database, repo_roots: &[String]) -> Result<Vec<String>, String> {
    let normalized = normalize_repo_roots(repo_roots);
    let value = serde_json::to_string(&normalized)
        .map_err(|err| format!("Failed to serialize repo roots: {err}"))?;

    db.with_connection(|connection| {
        connection.execute(
            r#"
            INSERT INTO settings (key, value, updated_at)
            VALUES (?1, ?2, CURRENT_TIMESTAMP)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = CURRENT_TIMESTAMP
            "#,
            params![REPO_ROOTS_KEY, value],
        )?;

        Ok(normalized)
    })
}

fn normalize_repo_roots(repo_roots: &[String]) -> Vec<String> {
    let mut roots = repo_roots
        .iter()
        .map(|root| root.trim())
        .filter(|root| !root.is_empty())
        .map(normalize_path)
        .collect::<Vec<_>>();

    roots.sort();
    roots.dedup();
    roots
}

fn expand_home(path: &str) -> String {
    if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return home.to_string_lossy().to_string();
        }
    }

    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{}", home.to_string_lossy(), rest);
        }
    }

    path.to_string()
}

pub fn get_has_completed_env_check(db: &Database) -> Result<bool, String> {
    db.with_connection(|connection| {
        let value: Option<String> = connection
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![ENV_CHECK_KEY],
                |row| row.get(0),
            )
            .optional()?;

        Ok(value
            .and_then(|json| serde_json::from_str::<bool>(&json).ok())
            .unwrap_or(false))
    })
}

pub fn save_has_completed_env_check(db: &Database, completed: bool) -> Result<bool, String> {
    let value = serde_json::to_string(&completed)
        .map_err(|err| format!("Failed to serialize environment check flag: {err}"))?;

    db.with_connection(|connection| {
        connection.execute(
            r#"
            INSERT INTO settings (key, value, updated_at)
            VALUES (?1, ?2, CURRENT_TIMESTAMP)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = CURRENT_TIMESTAMP
            "#,
            params![ENV_CHECK_KEY, value],
        )?;

        Ok(completed)
    })
}
