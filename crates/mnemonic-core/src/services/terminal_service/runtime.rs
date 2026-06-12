use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::models::{StartTerminalSessionInput, TerminalSession};
use crate::repositories::terminal_repository;
use crate::services::terminal_service;
use crate::state::{ActiveTerminal, AppState};

use super::output::{append_output, emit_output, persist_output_chunks};

const RETAINED_OUTPUT_CHUNKS_ON_EXIT: u32 = 5000;

pub(super) struct TerminalReaderConfig {
    pub event_emitter: std::sync::Arc<dyn crate::state::AppEventEmitter>,
    pub db: crate::db::Database,
    pub workspace_id: String,
    pub session_id: String,
    pub next_seq: Arc<AtomicU64>,
    pub last_output_at_secs: Arc<AtomicU64>,
    pub last_output_at_millis: Arc<AtomicU64>,
    pub reader: Box<dyn Read + Send>,
    pub ws_connections: crate::state::WsConnectionRegistry,
}

pub(super) fn active_for_workspace(
    state: &AppState,
    workspace_id: &str,
    session_role: &str,
) -> Result<Option<Arc<ActiveTerminal>>, String> {
    let Some(session) =
        terminal_repository::latest_for_workspace_role(&state.db, workspace_id, session_role)?
    else {
        return Ok(None);
    };
    active_for_session(state, &session.id)
}

pub(crate) fn active_for_session(
    state: &AppState,
    session_id: &str,
) -> Result<Option<Arc<ActiveTerminal>>, String> {
    let registry = state
        .terminals
        .lock()
        .map_err(|_| "Terminal registry lock poisoned".to_string())?;
    Ok(registry.get(session_id).cloned())
}

pub(super) fn detach_active_terminal(state: &AppState, session_id: &str) {
    let active = state
        .terminals
        .lock()
        .ok()
        .and_then(|mut registry| registry.remove(session_id));
    if let Some(active) = active {
        if let Ok(mut killer) = active.killer.lock() {
            let _ = killer.kill();
        }
    }
}

pub(super) fn send_interrupt_to_session(
    state: &AppState,
    session: &TerminalSession,
) -> Result<(), String> {
    if let Some(active) = active_for_session(state, &session.id)? {
        // Interactive agent TUIs (Claude Code, Codex) interrupt the current
        // turn with Esc. Ctrl-C there clears the input — or exits the whole
        // app on a double-press, killing the session. Ctrl-C remains correct
        // for shell/run sessions.
        let is_agent_tui = session.terminal_kind == "agent" || session.session_role == "agent";
        let interrupt_bytes: &[u8] = if is_agent_tui { b"\x1b" } else { b"\x03" };
        let mut writer = active
            .writer
            .lock()
            .map_err(|_| "Terminal writer lock poisoned".to_string())?;
        writer
            .write_all(interrupt_bytes)
            .map_err(|err| format!("Failed to interrupt terminal: {err}"))?;
        writer
            .flush()
            .map_err(|err| format!("Failed to flush interrupt: {err}"))?;
        return Ok(());
    }

    Err(format!("Terminal session {} is not attached", session.id))
}

pub(super) fn reconcile_orphan_running_session(
    state: &AppState,
    workspace_id: &str,
    session_role: &str,
    status: &str,
) -> Result<(), String> {
    let Some(latest) =
        terminal_repository::latest_for_workspace_role(&state.db, workspace_id, session_role)?
    else {
        return Ok(());
    };
    if latest.status != "running" {
        return Ok(());
    }
    if latest.backend == "tmux"
        && latest
            .tmux_session_name
            .as_deref()
            .map(super::tmux::session_exists)
            .unwrap_or(false)
    {
        return Ok(());
    }
    let ended_at = terminal_service::timestamp();
    log::info!(
        target: "mnemonic_lib",
        "reconcile_orphan_running_session: session_id={} workspace_id={} role={} -> {} (no active PTY)",
        latest.id,
        workspace_id,
        session_role,
        status,
    );
    terminal_repository::mark_finished(&state.db, &latest.id, status, &ended_at, true)?;
    let seq = Arc::new(AtomicU64::new(
        terminal_repository::next_seq(&state.db, &latest.id).unwrap_or(0),
    ));
    append_output(
        Some(state.event_emitter.as_ref()),
        &state.db,
        workspace_id,
        &latest.id,
        &seq,
        "system",
        &format!("Terminal session {status} (reconciled; no active PTY)\r\n"),
    );
    let _ = terminal_repository::mark_prompt_status_by_session(&state.db, &latest.id, status);
    Ok(())
}

pub(super) fn spawn_terminal_reader(config: TerminalReaderConfig) {
    let TerminalReaderConfig {
        event_emitter,
        db,
        workspace_id,
        session_id,
        next_seq,
        last_output_at_secs,
        last_output_at_millis,
        mut reader,
        ws_connections,
    } = config;

    let (tx, rx) = mpsc::sync_channel::<Result<Vec<u8>, String>>(128);
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(Ok(buffer[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(format!(
                        "\r\n[mnemonic] terminal read failed: {err}\r\n"
                    )));
                    break;
                }
            }
        }
    });

    thread::spawn(move || {
        use crate::models::TerminalOutputChunk;

        const PERSIST_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
        const MAX_PENDING_BYTES: usize = 256 * 1024;
        let mut display_pending = Vec::<u8>::with_capacity(4096);
        let mut persist_queue: Vec<TerminalOutputChunk> = Vec::new();
        let mut last_persist = std::time::Instant::now();

        let emit_and_queue = |buf: &mut Vec<u8>, queue: &mut Vec<TerminalOutputChunk>| {
            if buf.is_empty() {
                return;
            }
            let data = String::from_utf8_lossy(buf).to_string();
            buf.clear();
            let chunk = emit_output(
                event_emitter.as_ref(),
                &workspace_id,
                &session_id,
                &next_seq,
                "pty",
                &data,
            );
            queue.push(chunk);
        };

        loop {
            match rx.recv_timeout(PERSIST_FLUSH_INTERVAL) {
                Ok(Ok(bytes)) => {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default();
                    last_output_at_secs.store(now.as_secs(), Ordering::Relaxed);
                    last_output_at_millis.store(now.as_millis() as u64, Ordering::Relaxed);

                    display_pending.extend_from_slice(&bytes);

                    // Drain any additional data already in the channel so we
                    // coalesce rapid-fire reads into a single emit.
                    while let Ok(Ok(more)) = rx.try_recv() {
                        display_pending.extend_from_slice(&more);
                    }

                    if display_pending.len() > MAX_PENDING_BYTES {
                        let keep_from = display_pending.len().saturating_sub(64 * 1024);
                        display_pending.drain(..keep_from);
                    }

                    // Immediate display: send via WebSocket if connected,
                    // otherwise fall back to Tauri event emission.
                    if !display_pending.is_empty() {
                        let sent_via_ws = ws_connections
                            .lock()
                            .ok()
                            .map(|conns| {
                                if let Some(senders) = conns.get(&session_id) {
                                    if !senders.is_empty() {
                                        let raw = display_pending.clone();
                                        for sender in senders {
                                            let _ = sender.try_send(raw.clone());
                                        }
                                        return true;
                                    }
                                }
                                false
                            })
                            .unwrap_or(false);

                        if sent_via_ws {
                            // WS delivered raw bytes directly. Build chunk
                            // for persistence only (no Tauri event emit).
                            let data = String::from_utf8_lossy(&display_pending).to_string();
                            display_pending.clear();
                            let seq = next_seq.fetch_add(1, Ordering::SeqCst);
                            persist_queue.push(crate::models::TerminalOutputChunk {
                                id: format!("term-out-{}-{seq}", super::output::unique_suffix()),
                                session_id: session_id.clone(),
                                seq,
                                timestamp: super::output::timestamp(),
                                stream_type: "pty".to_string(),
                                data,
                            });
                        } else {
                            // No WS: emit via Tauri event (existing path).
                            emit_and_queue(&mut display_pending, &mut persist_queue);
                        }
                    }

                    // Background persistence: flush to SQLite periodically.
                    if last_persist.elapsed() >= PERSIST_FLUSH_INTERVAL {
                        persist_output_chunks(&db, &session_id, &persist_queue);
                        persist_queue.clear();
                        last_persist = std::time::Instant::now();
                    }
                }
                Ok(Err(message)) => {
                    emit_and_queue(&mut display_pending, &mut persist_queue);
                    persist_output_chunks(&db, &session_id, &persist_queue);
                    persist_queue.clear();
                    append_output(
                        Some(event_emitter.as_ref()),
                        &db,
                        &workspace_id,
                        &session_id,
                        &next_seq,
                        "system",
                        &message,
                    );
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !persist_queue.is_empty() {
                        persist_output_chunks(&db, &session_id, &persist_queue);
                        persist_queue.clear();
                        last_persist = std::time::Instant::now();
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    emit_and_queue(&mut display_pending, &mut persist_queue);
                    persist_output_chunks(&db, &session_id, &persist_queue);
                    persist_queue.clear();
                    break;
                }
            }
        }
    });
}

pub(super) fn spawn_terminal_monitor(
    state: AppState,
    workspace_id: String,
    _session_role: String,
    session_id: String,
    backend: String,
    tmux_session_name: Option<String>,
    mut child: Box<dyn portable_pty::Child + Send>,
) {
    thread::spawn(move || {
        let wait_result = child.wait();
        let ended_at = terminal_service::timestamp();

        let (was_active, seq_counter) = match state.terminals.lock() {
            Ok(mut registry) => {
                let active = registry.get(&session_id).cloned();
                registry.remove(&session_id);
                (active.is_some(), active.map(|a| a.seq_counter.clone()))
            }
            Err(err) => {
                log::error!("Terminal registry lock poisoned in monitor for {session_id}: {err}");
                (false, None)
            }
        };
        // Use the captured counter so exit log messages sequence after all
        // buffered reader output. Fall back to DB if counter wasn't captured.
        let seq_counter = seq_counter.unwrap_or_else(|| {
            Arc::new(AtomicU64::new(
                terminal_repository::next_seq(&state.db, &session_id).unwrap_or(0),
            ))
        });
        let _ = state
            .pending_commands
            .lock()
            .map(|mut m| m.remove(&session_id));

        if terminal_repository::get_session(&state.db, &session_id)
            .ok()
            .flatten()
            .map(|session| session.status != "running")
            .unwrap_or(false)
        {
            return;
        }

        if backend == "tmux"
            && tmux_session_name
                .as_deref()
                .map(super::tmux::session_exists)
                .unwrap_or(false)
        {
            append_output(
                Some(state.event_emitter.as_ref()),
                &state.db,
                &workspace_id,
                &session_id,
                &seq_counter,
                "system",
                "\r\n[mnemonic] detached from persistent tmux session; it is still running\r\n",
            );
            return;
        }

        match wait_result {
            Ok(exit_status) => {
                let status = if was_active && exit_status.success() {
                    "succeeded"
                } else if was_active {
                    "failed"
                } else {
                    "stopped"
                };
                let _ = terminal_repository::mark_finished(
                    &state.db,
                    &session_id,
                    status,
                    &ended_at,
                    false,
                );
                append_output(
                    Some(state.event_emitter.as_ref()),
                    &state.db,
                    &workspace_id,
                    &session_id,
                    &seq_counter,
                    "system",
                    &format!("\r\n[mnemonic] terminal exited: {exit_status:?}\r\n"),
                );
                let _ = terminal_repository::mark_prompt_status_by_session(
                    &state.db,
                    &session_id,
                    status,
                );
                let _ = terminal_repository::prune_output_chunks(
                    &state.db,
                    &session_id,
                    RETAINED_OUTPUT_CHUNKS_ON_EXIT,
                );
            }
            Err(err) => {
                let status = if was_active { "failed" } else { "stopped" };
                let _ = terminal_repository::mark_finished(
                    &state.db,
                    &session_id,
                    status,
                    &ended_at,
                    false,
                );
                append_output(
                    Some(state.event_emitter.as_ref()),
                    &state.db,
                    &workspace_id,
                    &session_id,
                    &seq_counter,
                    "system",
                    &format!("\r\n[mnemonic] terminal wait failed: {err}\r\n"),
                );
                let _ = terminal_repository::mark_prompt_status_by_session(
                    &state.db,
                    &session_id,
                    status,
                );
                let _ = terminal_repository::prune_output_chunks(
                    &state.db,
                    &session_id,
                    RETAINED_OUTPUT_CHUNKS_ON_EXIT,
                );
            }
        }
    });
}

pub(super) fn ensure_agent_session_for_prompt(
    state: &AppState,
    workspace_id: &str,
    profile: &str,
    extra_args: Option<&[String]>,
) -> Result<(TerminalSession, bool), String> {
    if let Some(active) = active_for_workspace(state, workspace_id, "agent")? {
        let session = terminal_repository::get_session(&state.db, &active.session_id)?
            .ok_or_else(|| "Active agent session record was not found".to_string())?;
        return Ok((session, false));
    }

    if let Some(session) =
        terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "agent")?
    {
        if session.backend == "tmux" && session.status == "running" {
            let attached = terminal_service::attach_workspace_terminal_session(
                state,
                crate::models::AttachWorkspaceTerminalInput {
                    workspace_id: workspace_id.to_string(),
                    session_id: session.id.clone(),
                    cols: None,
                    rows: None,
                },
            )?;
            return Ok((attached, false));
        }
    }

    terminal_service::start_workspace_terminal_session(
        state,
        StartTerminalSessionInput {
            workspace_id: workspace_id.to_string(),
            profile: profile.to_string(),
            session_role: Some("agent".to_string()),
            cols: None,
            rows: None,
            replace_existing: Some(false),
            extra_args: extra_args.map(<[String]>::to_vec),
        },
    )
    .map(|s| (s, true))
}
