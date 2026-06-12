use crate::context::schema::RepoSymbol;
use crate::db::Database;

pub struct CachedEntry {
    pub symbols: Vec<RepoSymbol>,
    pub imports_internal: Vec<String>,
    pub summary: String,
}

pub fn get(db: &Database, blob_oid: &str, parser_version: &str) -> Option<CachedEntry> {
    let result: Result<(String, String, String), String> = db.with_connection(|conn| {
        conn.query_row(
            "SELECT symbols_json, imports_json, summary FROM context_symbol_cache WHERE blob_oid = ?1 AND parser_version = ?2",
            rusqlite::params![blob_oid, parser_version],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
    });
    let (symbols_json, imports_json, summary) = result.ok()?;
    let symbols: Vec<RepoSymbol> = serde_json::from_str(&symbols_json).ok()?;
    let imports_internal: Vec<String> = serde_json::from_str(&imports_json).ok()?;
    Some(CachedEntry {
        symbols,
        imports_internal,
        summary,
    })
}

pub fn put(db: &Database, blob_oid: &str, parser_version: &str, entry: &CachedEntry) {
    let Ok(symbols_json) = serde_json::to_string(&entry.symbols) else {
        return;
    };
    let Ok(imports_json) = serde_json::to_string(&entry.imports_internal) else {
        return;
    };
    let now = unix_now();
    let _ = db.with_connection(|conn| {
        conn.execute(
            "INSERT OR REPLACE INTO context_symbol_cache (blob_oid, parser_version, symbols_json, imports_json, summary, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![blob_oid, parser_version, symbols_json, imports_json, entry.summary, now],
        )
    });
}

pub fn prune_old(db: &Database, keep_days: u32) {
    let cutoff = keep_days as i64 * 86400;
    let _ = db.with_connection(|conn| {
        conn.execute(
            "DELETE FROM context_symbol_cache WHERE CAST(strftime('%s', 'now') AS INTEGER) - CAST(created_at AS INTEGER) > ?1",
            rusqlite::params![cutoff],
        )
    });
}

fn unix_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
