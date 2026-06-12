use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{TerminalOutputChunk, TerminalOutputEvent};
use crate::repositories::terminal_repository;
use crate::state::{AppEventEmitter, AppState};

const OUTPUT_RETENTION_CHUNKS: u32 = 5000;
const OUTPUT_PRUNE_INTERVAL: u64 = 500;

/// Emit a chunk to the frontend immediately (no database write).
/// Used by the fast display path so the UI renders output with zero delay.
pub(super) fn emit_output(
    event_emitter: &dyn AppEventEmitter,
    workspace_id: &str,
    session_id: &str,
    next_seq: &AtomicU64,
    stream_type: &str,
    data: &str,
) -> TerminalOutputChunk {
    let seq = next_seq.fetch_add(1, Ordering::SeqCst);
    let chunk = TerminalOutputChunk {
        id: format!("term-out-{}-{seq}", unique_suffix()),
        session_id: session_id.to_string(),
        seq,
        timestamp: timestamp(),
        stream_type: stream_type.to_string(),
        data: data.to_string(),
    };
    event_emitter.emit(
        crate::events::TERMINAL_OUTPUT,
        &serde_json::json!(TerminalOutputEvent {
            workspace_id: workspace_id.to_string(),
            chunk: chunk.clone(),
        }),
    );
    chunk
}

/// Persist chunks to SQLite in batch (background persistence path).
pub(super) fn persist_output_chunks(
    db: &crate::db::Database,
    session_id: &str,
    chunks: &[TerminalOutputChunk],
) {
    if chunks.is_empty() {
        return;
    }
    let _ = terminal_repository::insert_output_chunks(db, chunks);
    if let Some(last) = chunks.last() {
        if last.seq != 0 && last.seq.is_multiple_of(OUTPUT_PRUNE_INTERVAL) {
            let _ =
                terminal_repository::prune_output_chunks(db, session_id, OUTPUT_RETENTION_CHUNKS);
        }
    }
}

/// Emit to frontend and persist to database (used for system messages and
/// non-hot-path output where immediate persistence is acceptable).
pub(super) fn append_output(
    event_emitter: Option<&dyn AppEventEmitter>,
    db: &crate::db::Database,
    workspace_id: &str,
    session_id: &str,
    next_seq: &AtomicU64,
    stream_type: &str,
    data: &str,
) {
    let seq = next_seq.fetch_add(1, Ordering::SeqCst);
    let chunk = TerminalOutputChunk {
        id: format!("term-out-{}-{seq}", unique_suffix()),
        session_id: session_id.to_string(),
        seq,
        timestamp: timestamp(),
        stream_type: stream_type.to_string(),
        data: data.to_string(),
    };
    let _ = terminal_repository::insert_output_chunk(db, &chunk);
    if seq != 0 && seq.is_multiple_of(OUTPUT_PRUNE_INTERVAL) {
        let _ = terminal_repository::prune_output_chunks(db, session_id, OUTPUT_RETENTION_CHUNKS);
    }
    if let Some(event_emitter) = event_emitter {
        event_emitter.emit(
            crate::events::TERMINAL_OUTPUT,
            &serde_json::json!(TerminalOutputEvent {
                workspace_id: workspace_id.to_string(),
                chunk,
            }),
        );
    }
}

pub(super) fn append_log_line(
    state: &AppState,
    workspace_id: &str,
    session_id: &str,
    stream_type: &str,
    data: &str,
) {
    // Prefer the live counter from the active terminal so system messages
    // never collide with in-flight reader output. Fall back to DB when the
    // session has already been removed from the registry.
    let seq_counter = state
        .terminals
        .lock()
        .ok()
        .and_then(|registry| registry.get(session_id).map(|a| a.seq_counter.clone()))
        .unwrap_or_else(|| {
            Arc::new(AtomicU64::new(
                terminal_repository::next_seq(&state.db, session_id).unwrap_or(0),
            ))
        });
    append_output(
        Some(state.event_emitter.as_ref()),
        &state.db,
        workspace_id,
        session_id,
        &seq_counter,
        stream_type,
        data,
    );
}

pub(super) fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

pub(super) fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{nanos}")
}

pub(super) fn enriched_path() -> String {
    let base = std::env::var("PATH").unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp"));
    let extras = [
        format!("{home}/.local/bin"),
        format!("{home}/.cargo/bin"),
        format!("{home}/.nvm/current/bin"),
        String::from("/opt/homebrew/bin"),
        String::from("/opt/homebrew/sbin"),
        String::from("/usr/local/bin"),
        String::from("/usr/bin"),
        String::from("/bin"),
        String::from("/usr/sbin"),
        String::from("/sbin"),
    ];
    let mut seen = HashSet::new();
    let mut parts = Vec::new();
    for entry in base.split(':').chain(extras.iter().map(|s| s.as_str())) {
        if !entry.is_empty() && seen.insert(entry.to_string()) {
            parts.push(entry.to_string());
        }
    }
    parts.join(":")
}
