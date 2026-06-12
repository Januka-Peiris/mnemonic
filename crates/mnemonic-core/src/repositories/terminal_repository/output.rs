use rusqlite::params;

use crate::db::Database;
use crate::models::TerminalOutputChunk;

use super::terminal_output_chunk_from_row;

pub fn insert_output_chunk(db: &Database, chunk: &TerminalOutputChunk) -> Result<(), String> {
    insert_output_chunks(db, std::slice::from_ref(chunk))
}

pub fn insert_output_chunks(db: &Database, chunks: &[TerminalOutputChunk]) -> Result<(), String> {
    if chunks.is_empty() {
        return Ok(());
    }
    db.with_connection_mut(|connection| {
        let transaction = connection.transaction()?;
        {
            let mut stmt = transaction.prepare(
                r#"
                INSERT OR IGNORE INTO terminal_output_chunks (
                    id, session_id, seq, timestamp, stream_type, data
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                "#,
            )?;
            for chunk in chunks {
                stmt.execute(params![
                    chunk.id,
                    chunk.session_id,
                    chunk.seq as i64,
                    chunk.timestamp,
                    chunk.stream_type,
                    chunk.data,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    })
}

pub fn prune_output_chunks(db: &Database, session_id: &str, keep: u32) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            r#"
            DELETE FROM terminal_output_chunks
            WHERE session_id = ?1
              AND seq < (
                SELECT COALESCE(MAX(seq) - ?2 + 1, 0)
                FROM terminal_output_chunks
                WHERE session_id = ?1
              )
            "#,
            params![session_id, keep],
        )?;
        Ok(())
    })
}

pub fn list_output_chunks(
    db: &Database,
    session_id: &str,
    since_seq: u64,
) -> Result<Vec<TerminalOutputChunk>, String> {
    const INITIAL_TAIL_LIMIT: i64 = 2000;
    const INCREMENTAL_LIMIT: i64 = 1000;

    db.with_connection(|connection| {
        let rows = if since_seq == 0 {
            let mut stmt = connection.prepare(
                r#"
                SELECT id, session_id, seq, timestamp, stream_type, data
                FROM terminal_output_chunks
                WHERE session_id = ?1
                ORDER BY seq DESC
                LIMIT ?2
                "#,
            )?;
            let mut chunks = stmt
                .query_map(
                    params![session_id, INITIAL_TAIL_LIMIT],
                    terminal_output_chunk_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            chunks.reverse();
            chunks
        } else {
            let mut stmt = connection.prepare(
                r#"
                SELECT id, session_id, seq, timestamp, stream_type, data
                FROM terminal_output_chunks
                WHERE session_id = ?1 AND seq >= ?2
                ORDER BY seq ASC
                LIMIT ?3
                "#,
            )?;
            let chunks = stmt
                .query_map(
                    params![session_id, since_seq as i64, INCREMENTAL_LIMIT],
                    terminal_output_chunk_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            chunks
        };
        Ok(rows)
    })
}

pub fn next_seq(db: &Database, session_id: &str) -> Result<u64, String> {
    db.with_connection(|connection| {
        let next: i64 = connection.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM terminal_output_chunks WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(next.max(0) as u64)
    })
}
