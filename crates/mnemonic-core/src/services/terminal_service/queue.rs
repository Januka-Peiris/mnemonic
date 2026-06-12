use std::io::Write;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::models::{AgentPromptEntry, QueueAgentPromptInput, TerminalSession};
use crate::repositories::terminal_repository;
use crate::services::{agent_profile_service, checkpoint_service, hook_service, terminal_service};
use crate::state::{ActiveTerminal, AppState};

use super::output::{append_log_line, unique_suffix};
use super::prompts::{terminal_prompt_payload_for_session, PromptPayload};
use super::runtime::{active_for_session, active_for_workspace, ensure_agent_session_for_prompt};

/// How long PTY output must stay quiet before a freshly spawned TUI is
/// considered ready to receive a prompt.
const READY_IDLE_MILLIS: u64 = 600;
/// Upper bound on the readiness wait; after this we dispatch best-effort.
const READY_TIMEOUT_MILLIS: u64 = 12_000;
/// Delay between writing the prompt text and the Enter keypress. A CR in the
/// same chunk as the text is treated as pasted content by TUI inputs.
const ENTER_DELAY_MILLIS: u64 = 250;

pub(super) fn queue_workspace_agent_prompt(
    state: &AppState,
    input: QueueAgentPromptInput,
) -> Result<AgentPromptEntry, String> {
    let user_prompt = input.prompt.trim().to_string();
    if user_prompt.is_empty() {
        return Err("Prompt is required".to_string());
    }

    let resolved_profile = agent_profile_service::resolve_agent_profile(
        state,
        Some(&input.workspace_id),
        input.profile_id.as_deref(),
        input.profile.as_deref(),
    )?;

    let prompt = user_prompt;

    let profile = resolved_profile.id.clone();
    let mut entry = AgentPromptEntry {
        id: format!("prompt-{}", unique_suffix()),
        workspace_id: input.workspace_id.clone(),
        session_id: input.session_id.clone(),
        profile,
        prompt,
        status: "queued".to_string(),
        created_at: terminal_service::timestamp(),
        sent_at: None,
    };
    terminal_repository::insert_prompt_entry(&state.db, &entry)?;

    // Prompts always dispatch immediately. Interactive agent TUIs (Claude
    // Code, Codex) natively queue input submitted while they are mid-turn, so
    // app-level queueing is redundant — and with a persistent TUI session
    // there is no reliable "idle" signal to key it on anyway.
    let hook_context = serde_json::json!({
        "workspaceId": input.workspace_id,
        "actionKind": "queue_agent_prompt",
        "profileId": entry.profile,
    });
    let dispatch_result = hook_service::run_workspace_hooks(
        state,
        &entry.workspace_id,
        "tool",
        hook_service::HookPhase::Pre,
        &hook_context,
    )
    .and_then(|()| dispatch_prompt_entry(state, &mut entry, input.extra_args.as_deref()));
    if let Err(err) = dispatch_result {
        // Nothing re-dispatches stored prompts anymore, so a row left in
        // 'queued' would linger forever; record the failure instead.
        let _ = terminal_repository::mark_prompt_failed(&state.db, &entry.id);
        return Err(err);
    }
    let _ = hook_service::run_workspace_hooks(
        state,
        &entry.workspace_id,
        "tool",
        hook_service::HookPhase::Post,
        &hook_context,
    );
    Ok(entry)
}

pub(super) fn batch_dispatch_workspace_agent_prompt(
    state: &AppState,
    input: crate::models::BatchDispatchPromptInput,
) -> Result<Vec<AgentPromptEntry>, String> {
    if input.prompt.trim().is_empty() {
        return Err("Prompt is required".to_string());
    }
    let mut entries = Vec::with_capacity(input.workspace_ids.len());
    for workspace_id in &input.workspace_ids {
        let result = queue_workspace_agent_prompt(
            state,
            QueueAgentPromptInput {
                workspace_id: workspace_id.clone(),
                session_id: None,
                prompt: input.prompt.clone(),
                profile: None,
                profile_id: input.profile_id.clone(),
                extra_args: None,
            },
        );
        match result {
            Ok(entry) => entries.push(entry),
            Err(err) => log::warn!(
                target: "mnemonic_lib",
                "batch_dispatch: failed for workspace {workspace_id}: {err}"
            ),
        }
    }
    Ok(entries)
}

pub(super) fn list_workspace_agent_prompts(
    state: &AppState,
    workspace_id: &str,
    limit: Option<u32>,
) -> Result<Vec<AgentPromptEntry>, String> {
    terminal_repository::list_prompts_for_workspace(&state.db, workspace_id, limit)
}

fn dispatch_prompt_entry(
    state: &AppState,
    entry: &mut AgentPromptEntry,
    extra_args: Option<&[String]>,
) -> Result<(), String> {
    checkpoint_service::create_checkpoint_if_dirty_in_background(
        state.clone(),
        entry.workspace_id.clone(),
        "before agent prompt".to_string(),
    );

    let mut is_new_session = false;
    let session = if let Some(session_id) = entry.session_id.as_deref() {
        let session = terminal_repository::get_session(&state.db, session_id)?
            .ok_or_else(|| format!("Terminal session {session_id} was not found"))?;
        if session.workspace_id != entry.workspace_id {
            return Err(format!(
                "Terminal session {session_id} does not belong to workspace {}",
                entry.workspace_id
            ));
        }
        if session.status != "running" {
            return Err(format!(
                "Terminal session {session_id} has already ended ({})",
                session.status
            ));
        }
        if active_for_session(state, &session.id)?.is_none() && session.backend == "tmux" {
            terminal_service::attach_workspace_terminal_session(
                state,
                crate::models::AttachWorkspaceTerminalInput {
                    workspace_id: entry.workspace_id.clone(),
                    session_id: session.id.clone(),
                    cols: None,
                    rows: None,
                },
            )?;
        }
        session
    } else {
        let (session, created) = ensure_agent_session_for_prompt(
            state,
            &entry.workspace_id,
            &entry.profile,
            extra_args,
        )?;
        is_new_session = created;
        session
    };

    let active = active_for_session(state, &session.id)?
        .or_else(|| {
            active_for_workspace(state, &entry.workspace_id, "agent")
                .ok()
                .flatten()
                .filter(|active| active.session_id == session.id)
        })
        .ok_or_else(|| format!("Terminal session {} is not attached", session.id))?;

    if is_new_session {
        wait_for_session_ready(state, &session, &active)?;
    } else {
        handle_blocking_dialogs(state, &session, &active)?;
    }

    match terminal_prompt_payload_for_session(&session, &entry.prompt) {
        PromptPayload::Single(payload) => {
            write_to_terminal(&active, payload.as_bytes())?;
        }
        PromptPayload::PasteThenEnter(payload) => {
            write_to_terminal(&active, payload.as_bytes())?;
            // Release the writer between the text and the Enter so the TUI
            // sees them as separate input events and treats Enter as submit.
            thread::sleep(Duration::from_millis(ENTER_DELAY_MILLIS));
            write_to_terminal(&active, b"\r")?;
        }
    }

    let sent_at = terminal_service::timestamp();
    terminal_repository::mark_prompt_sent(&state.db, &entry.id, &session.id, &sent_at)?;
    entry.session_id = Some(session.id.clone());
    entry.status = "sent".to_string();
    entry.sent_at = Some(sent_at);
    Ok(())
}

fn write_to_terminal(active: &Arc<ActiveTerminal>, data: &[u8]) -> Result<(), String> {
    let mut writer = active
        .writer
        .lock()
        .map_err(|_| "Terminal writer lock poisoned".to_string())?;
    writer
        .write_all(data)
        .map_err(|err| format!("Failed to write prompt to terminal: {err}"))?;
    writer
        .flush()
        .map_err(|err| format!("Failed to flush prompt to terminal: {err}"))?;
    Ok(())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Wait until a freshly spawned agent TUI has rendered (output seen, then
/// quiet) before sending the prompt, answering the folder-trust dialog along
/// the way. Falls through after a timeout rather than failing the dispatch.
fn wait_for_session_ready(
    state: &AppState,
    session: &TerminalSession,
    active: &Arc<ActiveTerminal>,
) -> Result<(), String> {
    let started = now_millis();
    let mut trust_answered = false;
    loop {
        thread::sleep(Duration::from_millis(150));
        if active_for_session(state, &session.id)?.is_none() {
            return Err(format!(
                "Terminal session {} ended before the prompt could be sent",
                session.id
            ));
        }
        let last_output = active
            .last_output_at_millis
            .load(std::sync::atomic::Ordering::Relaxed);
        let now = now_millis();
        if last_output != 0 && now.saturating_sub(last_output) >= READY_IDLE_MILLIS {
            let tail = recent_output_tail(state, &session.id);
            if !trust_answered && looks_like_trust_dialog(&tail) {
                // Forge created this worktree from the user's own repository at
                // their request, so accepting the folder-trust prompt is safe.
                write_to_terminal(active, b"1\r")?;
                append_log_line(
                    state,
                    &session.workspace_id,
                    &session.id,
                    "system",
                    "\r\n[mnemonic] accepted Claude folder trust prompt for this worktree\r\n",
                );
                trust_answered = true;
                continue;
            }
            if looks_like_bypass_permissions_dialog(&tail) {
                return Err(
                    "Claude is waiting for you to accept Bypass Permissions mode in the terminal. \
                     Accept it there, then send the prompt again (it is still queued)."
                        .to_string(),
                );
            }
            return Ok(());
        }
        if now.saturating_sub(started) >= READY_TIMEOUT_MILLIS {
            return Ok(());
        }
    }
}

/// For already-running sessions: refuse to type into a blocking dialog, and
/// auto-answer the trust prompt if one is showing.
fn handle_blocking_dialogs(
    state: &AppState,
    session: &TerminalSession,
    active: &Arc<ActiveTerminal>,
) -> Result<(), String> {
    let tail = recent_output_tail(state, &session.id);
    if looks_like_trust_dialog(&tail) {
        write_to_terminal(active, b"1\r")?;
        append_log_line(
            state,
            &session.workspace_id,
            &session.id,
            "system",
            "\r\n[mnemonic] accepted Claude folder trust prompt for this worktree\r\n",
        );
        thread::sleep(Duration::from_millis(800));
        return Ok(());
    }
    if looks_like_bypass_permissions_dialog(&tail) {
        return Err(
            "Claude is waiting for you to accept Bypass Permissions mode in the terminal. \
             Accept it there, then send the prompt again (it is still queued)."
                .to_string(),
        );
    }
    Ok(())
}

/// Normalized (ANSI-stripped, lowercased, whitespace-collapsed) tail of the
/// session's recent output, capped to the last ~1200 characters.
fn recent_output_tail(state: &AppState, session_id: &str) -> String {
    let next_seq = terminal_repository::next_seq(&state.db, session_id).unwrap_or(0);
    let since = next_seq.saturating_sub(60);
    let chunks =
        terminal_repository::list_output_chunks(&state.db, session_id, since).unwrap_or_default();
    let raw: String = chunks.iter().map(|chunk| chunk.data.as_str()).collect();
    let normalized = normalize_terminal_text(&raw);
    let tail_start = normalized.len().saturating_sub(1200);
    // Avoid slicing mid-UTF-8-codepoint.
    let mut start = tail_start;
    while start < normalized.len() && !normalized.is_char_boundary(start) {
        start += 1;
    }
    normalized[start..].to_string()
}

fn looks_like_trust_dialog(tail: &str) -> bool {
    tail.contains("trust this folder")
}

fn looks_like_bypass_permissions_dialog(tail: &str) -> bool {
    tail.contains("bypass permissions mode") && tail.contains("yes, i accept")
}

/// Strips ANSI escape sequences (CSI, OSC, and lone ESC sequences), replacing
/// them with spaces, then lowercases and collapses whitespace. Terminal UIs
/// position words with cursor moves instead of spaces, so the replacement
/// keeps word boundaries intact for substring matching.
fn normalize_terminal_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
                out.push(' ');
            }
            Some(']') => {
                chars.next();
                let mut prev = ' ';
                for next in chars.by_ref() {
                    if next == '\u{7}' || (prev == '\u{1b}' && next == '\\') {
                        break;
                    }
                    prev = next;
                }
                out.push(' ');
            }
            _ => {
                chars.next();
                out.push(' ');
            }
        }
    }
    out.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_cursor_positioned_dialog_text() {
        let raw = "\u{1b}[2GQuick\u{1b}[8Gsafety\u{1b}[15Gcheck:\r\n\u{1b}[38;2;177;185;249mYes,\u{1b}[12GI\u{1b}[14Gtrust\u{1b}[20Gthis\u{1b}[25Gfolder\u{1b}[39m";
        let normalized = normalize_terminal_text(raw);
        assert!(normalized.contains("quick safety check:"));
        assert!(looks_like_trust_dialog(&normalized));
    }

    #[test]
    fn bypass_dialog_requires_both_markers() {
        assert!(looks_like_bypass_permissions_dialog(
            "warning: claude code running in bypass permissions mode 1. no, exit 2. yes, i accept"
        ));
        assert!(!looks_like_bypass_permissions_dialog(
            "running in bypass permissions mode"
        ));
    }
}
