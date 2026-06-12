use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use super::migrations;

#[derive(Debug, Clone)]
pub struct Database {
    connection: Arc<Mutex<Connection>>,
    path: PathBuf,
}

fn configure_connection(connection: &Connection) -> Result<(), String> {
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|err| format!("Failed to enable SQLite foreign keys: {err}"))?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|err| format!("Failed to enable SQLite WAL mode: {err}"))?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(|err| format!("Failed to set SQLite synchronous mode: {err}"))?;
    // Checkpoint every ~400KB of WAL writes (100 pages × 4KB) so the WAL never
    // grows large enough to slow down reads.
    connection
        .pragma_update(None, "wal_autocheckpoint", "100")
        .map_err(|err| format!("Failed to set SQLite WAL autocheckpoint: {err}"))?;
    connection
        .busy_timeout(std::time::Duration::from_millis(5_000))
        .map_err(|err| format!("Failed to set SQLite busy timeout: {err}"))?;
    Ok(())
}

impl Database {
    pub fn initialize(data_dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(data_dir)
            .map_err(|err| format!("Failed to create app data directory: {err}"))?;

        let path = data_dir.join("forge.sqlite3");
        let connection = Connection::open(&path).map_err(|err| {
            format!(
                "Failed to open SQLite database at {}: {err}",
                path.display()
            )
        })?;

        configure_connection(&connection)?;

        migrations::run(&connection)?;

        // Merge any WAL left over from a previous run into the main database file.
        let _ = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path,
        })
    }

    pub fn with_connection<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "SQLite connection lock poisoned".to_string())?;

        f(&connection).map_err(|err| format!("SQLite query failed: {err}"))
    }

    pub fn with_connection_mut<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> Result<T, String> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "SQLite connection lock poisoned".to_string())?;

        f(&mut connection).map_err(|err| format!("SQLite write failed: {err}"))
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn prune_old_data(&self) -> Result<(), String> {
        self.with_connection_mut(|connection| {
            let transaction = connection.transaction()?;

            // 7-day TTL for heavy tables
            transaction.execute(
                "DELETE FROM activity_items WHERE created_at < datetime('now', '-7 days')",
                [],
            )?;
            transaction.execute(
                "DELETE FROM terminal_output_chunks WHERE created_at < datetime('now', '-7 days')",
                [],
            )?;
            transaction.execute(
                "DELETE FROM workspace_run_logs WHERE created_at < datetime('now', '-7 days')",
                [],
            )?;

            transaction.commit()?;
            Ok(())
        })?;

        // Drop old tree-sitter / symbol cache rows (blob-keyed; safe to prune aggressively)
        crate::context::cache::prune_old(self, 45);

        Ok(())
    }

    /// Run VACUUM to reclaim free pages after a prune. This rewrites the entire
    /// database and should only be called from a background thread — never on the
    /// startup path — because it holds the connection mutex for several seconds.
    #[allow(dead_code)]
    pub fn vacuum(&self) -> Result<(), String> {
        self.with_connection(|connection| connection.execute("VACUUM", []).map(|_| ()))
            .map_err(|err| format!("Failed to vacuum database: {err}"))
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self, String> {
        let connection = Connection::open_in_memory()
            .map_err(|err| format!("Failed to open in-memory SQLite database: {err}"))?;
        configure_connection(&connection)?;
        migrations::run(&connection)?;

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path: PathBuf::from(":memory:"),
        })
    }
}
