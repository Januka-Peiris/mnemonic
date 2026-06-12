use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::models::{
    AgentPromptEntry, AttachWorkspaceTerminalInput, CommandApprovalEvent,
    CreateWorkspaceTerminalInput, QueueAgentPromptInput, StartTerminalSessionInput,
    TerminalOutputResponse, TerminalSession, TerminalSessionState,
};
use crate::repositories::settings_repository;
use crate::repositories::{
    activity_repository, agent_memory_repository, terminal_repository, workspace_repository,
};
use crate::services::{agent_profile_service, command_safety_service, task_lifecycle_service};
use crate::state::{ActiveTerminal, AppState};

mod activity;
mod decisions;
mod launch;
mod output;
mod prompts;
mod queue;
pub(crate) mod runtime;
mod tmux;

const RETAINED_OUTPUT_CHUNKS_ON_CLOSE: u32 = 5000;

use activity::{
    command_preview, record_blocked_terminal_launch_activity, record_terminal_lifecycle_activity,
    record_terminal_start_activity,
};
use launch::{
    default_terminal_title, normalize_terminal_kind, resolve_session_role, workspace_root_path,
    SessionResume, TerminalCommandSpec, TerminalProfile,
};
use output::{
    append_log_line, append_output, enriched_path as enriched_path_impl,
    unique_suffix as output_unique_suffix,
};
use queue::{
    batch_dispatch_workspace_agent_prompt as batch_dispatch_workspace_agent_prompt_impl,
    list_workspace_agent_prompts as list_workspace_agent_prompts_impl,
    queue_workspace_agent_prompt as queue_workspace_agent_prompt_impl,
};
use runtime::{
    active_for_session, active_for_workspace, detach_active_terminal,
    reconcile_orphan_running_session, send_interrupt_to_session, spawn_terminal_monitor,
    spawn_terminal_reader, TerminalReaderConfig,
};

fn agent_effective_model(
    state: &AppState,
    profile: &crate::models::AgentProfile,
) -> Option<String> {
    // Profile-level model overrides the global default.
    if profile.model.is_some() {
        return profile.model.clone();
    }
    let key = match profile.agent.as_str() {
        "claude_code" => "claude_agent_default_model",
        "codex" => "codex_agent_default_model",
        _ => "agent_default_model",
    };
    settings_repository::get_value(&state.db, key)
        .ok()
        .flatten()
        .or_else(|| {
            if key == "claude_agent_default_model" {
                settings_repository::get_value(&state.db, "agent_default_model")
                    .ok()
                    .flatten()
            } else {
                None
            }
        })
}

fn shell_friendly_cwd(real_cwd: &Path, workspace_id: &str, db: &crate::db::Database) -> PathBuf {
    let workspace = match workspace_repository::get_detail(db, workspace_id) {
        Ok(Some(w)) => w,
        _ => return real_cwd.to_path_buf(),
    };
    let label = workspace.summary.name.trim().to_string();
    if label.is_empty() {
        return real_cwd.to_path_buf();
    }
    let sanitized: String = label
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let link_dir = std::env::temp_dir().join("mnemonic-shells");
    let _ = std::fs::create_dir_all(&link_dir);
    let link_path = link_dir.join(&sanitized);
    if link_path.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(&link_path);
    }
    #[cfg(unix)]
    if std::os::unix::fs::symlink(real_cwd, &link_path).is_ok() {
        return link_path;
    }
    real_cwd.to_path_buf()
}

fn spawn_active_terminal(
    state: &AppState,
    session: &TerminalSession,
    command: &str,
    args: &[String],
    cwd: &Path,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| format!("Failed to open PTY: {err}"))?;

    let mut command_builder = CommandBuilder::new(command);
    command_builder.args(args);
    command_builder.cwd(cwd);
    command_builder.env("TERM", "xterm-256color");
    // GUI apps inherit no LANG/LC_*; without a UTF-8 locale tmux renders
    // every non-ASCII glyph as '_' and TUIs may fall back to ASCII art.
    if std::env::var("LANG").is_err() {
        command_builder.env("LANG", "en_US.UTF-8");
    }
    command_builder.env("PATH", path_for_command(command));
    if std::env::var("SHELL").is_err() {
        command_builder.env("SHELL", "/bin/zsh");
    }

    let child = pair
        .slave
        .spawn_command(command_builder)
        .map_err(|err| format!("Failed to start terminal: {err}"))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| format!("Failed to get terminal reader: {err}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|err| format!("Failed to get terminal writer: {err}"))?;
    let killer = child.clone_killer();

    let seq_counter = Arc::new(AtomicU64::new(
        terminal_repository::next_seq(&state.db, &session.id).unwrap_or(0),
    ));
    let last_output_at_secs = Arc::new(AtomicU64::new(0));
    let last_output_at_millis = Arc::new(AtomicU64::new(0));
    let active = Arc::new(ActiveTerminal {
        session_id: session.id.clone(),
        terminal_kind: session.terminal_kind.clone(),
        writer: Mutex::new(writer),
        killer: Mutex::new(killer),
        master: Mutex::new(pair.master),
        last_output_at_secs: last_output_at_secs.clone(),
        last_output_at_millis: last_output_at_millis.clone(),
        seq_counter: seq_counter.clone(),
    });
    state
        .terminals
        .lock()
        .map_err(|_| "Terminal registry lock poisoned".to_string())?
        .insert(session.id.clone(), active.clone());

    spawn_terminal_reader(TerminalReaderConfig {
        event_emitter: state.event_emitter.clone(),
        db: state.db.clone(),
        workspace_id: session.workspace_id.clone(),
        session_id: session.id.clone(),
        next_seq: seq_counter,
        last_output_at_secs,
        last_output_at_millis,
        reader,
        ws_connections: state.ws_connections.clone(),
    });
    spawn_terminal_monitor(
        state.clone(),
        session.workspace_id.clone(),
        session.session_role.clone(),
        session.id.clone(),
        session.backend.clone(),
        session.tmux_session_name.clone(),
        child,
    );
    if session.terminal_kind == "agent" || session.session_role == "agent" {
        decisions::spawn_decision_watcher(state.clone(), session.clone(), active);
    }
    Ok(())
}

fn attach_tmux_terminal(
    state: &AppState,
    session: &TerminalSession,
    cols: Option<u16>,
    rows: Option<u16>,
) -> Result<TerminalSession, String> {
    if active_for_session(state, &session.id)?.is_some() {
        return Ok(session.clone());
    }
    if session.backend != "tmux" {
        return Err(format!(
            "Terminal session {} is not tmux-backed",
            session.id
        ));
    }
    if session.status != "running" {
        return Err(format!(
            "Terminal session {} has already ended ({})",
            session.id, session.status
        ));
    }
    let tmux_name = session
        .tmux_session_name
        .as_deref()
        .ok_or_else(|| format!("Terminal session {} has no tmux session name", session.id))?;
    if !tmux::session_exists(tmux_name) {
        let ended_at = timestamp();
        terminal_repository::mark_finished(&state.db, &session.id, "interrupted", &ended_at, true)?;
        append_log_line(
            state,
            &session.workspace_id,
            &session.id,
            "system",
            "[mnemonic] Persistent tmux session was not found; start a new session\r\n",
        );
        return Err(format!(
            "Terminal session {} is no longer running — start a new one",
            session.id
        ));
    }

    let cwd = Path::new(&session.cwd);
    let (attach_command, attach_args) = tmux::attach_command(tmux_name);
    spawn_active_terminal(
        state,
        session,
        &attach_command,
        &attach_args,
        cwd,
        cols.unwrap_or(100).max(20),
        rows.unwrap_or(30).max(5),
    )?;
    append_log_line(
        state,
        &session.workspace_id,
        &session.id,
        "system",
        "\r\n[mnemonic] attached to persistent tmux session\r\n",
    );
    terminal_repository::get_session(&state.db, &session.id)?
        .ok_or_else(|| format!("Terminal session {} was not found", session.id))
}

pub fn start_workspace_terminal_session(
    state: &AppState,
    input: StartTerminalSessionInput,
) -> Result<TerminalSession, String> {
    let resolved_profile = agent_profile_service::resolve_agent_profile(
        state,
        Some(&input.workspace_id),
        Some(&input.profile),
        Some(&input.profile),
    )?;
    let session_role = resolve_session_role(input.session_role.as_deref(), &resolved_profile.agent);
    if input.replace_existing.unwrap_or(false) {
        if let Some(existing) = terminal_repository::latest_for_workspace_role(
            &state.db,
            &input.workspace_id,
            &session_role,
        )? {
            let _ = stop_workspace_terminal_session_by_id(state, &existing.id);
        }
    }
    create_workspace_terminal(
        state,
        CreateWorkspaceTerminalInput {
            workspace_id: input.workspace_id,
            kind: session_role,
            profile: input.profile,
            title: None,
            command: None,
            profile_id: Some(resolved_profile.id.clone()),
            args: None,
            extra_args: input.extra_args,
            resume_claude_session_id: None,
            cols: input.cols,
            rows: input.rows,
        },
    )
}

/// Resolve the command line for a visible, foreground agent terminal.
///
/// Record a GPUI terminal session in SQLite without spawning a PTY.
///
/// GPUI terminals use Zed's Terminal crate for PTY lifecycle, so we only
/// need the metadata record for session history and workspace tracking.
pub fn record_gpui_session(
    state: &AppState,
    workspace_id: &str,
    kind: &str,
    title: &str,
    cwd: &str,
    command: &str,
    args: &[String],
) -> Result<TerminalSession, String> {
    let session_role = if kind == "shell" || kind == "utility" || kind == "run" {
        "utility"
    } else {
        "agent"
    }
    .to_string();
    let display_order = terminal_repository::next_display_order(&state.db, workspace_id)?;
    let session_id = format!("gpui-{}", output_unique_suffix());
    let session = TerminalSession {
        id: session_id,
        workspace_id: workspace_id.to_string(),
        session_role,
        profile: kind.to_string(),
        cwd: cwd.to_string(),
        status: "running".to_string(),
        started_at: timestamp(),
        ended_at: None,
        command: command.to_string(),
        args: args.to_vec(),
        pid: None,
        stale: false,
        closed_at: None,
        backend: "gpui".to_string(),
        tmux_session_name: None,
        title: title.to_string(),
        terminal_kind: kind.to_string(),
        display_order,
        is_visible: true,
        last_attached_at: None,
        last_captured_seq: 0,
        claude_session_id: None,
    };
    terminal_repository::insert_session(&state.db, &session)?;
    Ok(session)
}

/// Mark a GPUI terminal session as finished and closed.
pub fn close_gpui_session(state: &AppState, session_id: &str) -> Result<(), String> {
    let now = timestamp();
    terminal_repository::mark_finished(&state.db, session_id, "stopped", &now, false)?;
    terminal_repository::mark_closed(&state.db, session_id, &now)?;
    Ok(())
}

/// List visible (not closed) terminal sessions for a workspace.
pub fn list_visible_workspace_sessions(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<TerminalSession>, String> {
    terminal_repository::list_visible_for_workspace(&state.db, workspace_id)
}

/// GPUI v3 owns its PTY/rendering path, so it cannot use the v2 terminal
/// service's portable-pty session registry directly. This helper still keeps
/// agent profile normalization, model defaults, binary resolution, and Claude
/// permission/session flags in one shared core path.
pub fn visible_agent_terminal_command(
    state: &AppState,
    workspace_id: &str,
    profile_id: Option<&str>,
    legacy_profile: Option<&str>,
) -> Result<(String, Vec<String>), String> {
    let resolved_profile = agent_profile_service::resolve_agent_profile(
        state,
        Some(workspace_id),
        profile_id,
        legacy_profile,
    )?;
    let effective_model = agent_effective_model(state, &resolved_profile);
    let session_resume = if resolved_profile.command.contains("claude") {
        let session_id = uuid::Uuid::new_v4().to_string();
        Some(SessionResume::New(session_id))
    } else {
        None
    };
    let profile = TerminalProfile::from_agent_profile(
        &resolved_profile,
        effective_model.as_deref(),
        session_resume.as_ref(),
        &[],
    );
    let command_spec = TerminalCommandSpec::from_input(&profile, None, None)?;
    let launch_preview = command_preview(&command_spec.command, &command_spec.args);
    if command_safety_service::is_risky_command(&launch_preview) {
        return Err(format!(
            "Refusing to launch terminal profile {} because the command looks risky: {}",
            resolved_profile.label, launch_preview
        ));
    }
    Ok((command_spec.command, command_spec.args))
}

pub fn create_workspace_terminal(
    state: &AppState,
    input: CreateWorkspaceTerminalInput,
) -> Result<TerminalSession, String> {
    let real_cwd = workspace_root_path(state, &input.workspace_id)?;
    let resolved_profile = agent_profile_service::resolve_agent_profile(
        state,
        Some(&input.workspace_id),
        input.profile_id.as_deref(),
        Some(&input.profile),
    )?;
    let effective_model = agent_effective_model(state, &resolved_profile);
    let kind = normalize_terminal_kind(&input.kind, &resolved_profile.agent);
    let cwd = if kind == "shell" {
        shell_friendly_cwd(&real_cwd, &input.workspace_id, &state.db)
    } else {
        real_cwd
    };
    let session_role = if kind == "shell" || kind == "utility" || kind == "run" {
        "utility"
    } else {
        "agent"
    }
    .to_string();
    let is_claude_agent = resolved_profile.command.contains("claude") && session_role == "agent";
    let explicit_resume_id = input
        .resume_claude_session_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let (session_resume, claude_session_id) = if is_claude_agent {
        if let Some(resume_id) = explicit_resume_id {
            if claude_session_resumable(&cwd, resume_id) {
                (
                    Some(SessionResume::Resume(resume_id.to_string())),
                    Some(resume_id.to_string()),
                )
            } else {
                // `claude --resume` with a missing transcript exits instantly;
                // start a fresh session instead of a guaranteed dead launch.
                log::warn!(
                    target: "mnemonic_lib",
                    "create_workspace_terminal: no transcript for Claude session {resume_id}; starting a new session"
                );
                let new_id = uuid::Uuid::new_v4().to_string();
                (Some(SessionResume::New(new_id.clone())), Some(new_id))
            }
        } else {
            let has_active_agent =
                runtime::active_for_workspace(state, &input.workspace_id, "agent")
                    .ok()
                    .flatten()
                    .is_some();
            if has_active_agent {
                let new_id = uuid::Uuid::new_v4().to_string();
                (Some(SessionResume::New(new_id.clone())), Some(new_id))
            } else {
                let prior = terminal_repository::latest_for_workspace_role(
                    &state.db,
                    &input.workspace_id,
                    "agent",
                )
                .ok()
                .flatten();
                let resumable_id = prior
                    .filter(|s| s.status != "failed")
                    .and_then(|s| s.claude_session_id)
                    .filter(|id| claude_session_resumable(&cwd, id));
                if let Some(prior_id) = resumable_id {
                    (
                        Some(SessionResume::Resume(prior_id.clone())),
                        Some(prior_id),
                    )
                } else {
                    let new_id = uuid::Uuid::new_v4().to_string();
                    (Some(SessionResume::New(new_id.clone())), Some(new_id))
                }
            }
        }
    } else {
        (None, None)
    };
    let profile = TerminalProfile::from_agent_profile(
        &resolved_profile,
        effective_model.as_deref(),
        session_resume.as_ref(),
        input.extra_args.as_deref().unwrap_or(&[]),
    );
    let display_order = terminal_repository::next_display_order(&state.db, &input.workspace_id)?;
    let session_id = format!("term-{}", unique_suffix());
    let title = input
        .title
        .clone()
        .unwrap_or_else(|| default_terminal_title(&kind, &profile.name));
    let mut command_spec =
        TerminalCommandSpec::from_input(&profile, input.command.as_deref(), input.args.clone())?;
    if let Some(extra) = &input.extra_args {
        command_spec.args.extend(extra.iter().cloned());
    }
    let launch_command = command_spec.command.clone();
    let launch_args = command_spec.args.clone();
    let launch_preview = command_preview(&launch_command, &launch_args);
    if command_safety_service::is_risky_command(&launch_preview) {
        record_blocked_terminal_launch_activity(
            state,
            &input.workspace_id,
            &resolved_profile,
            &launch_preview,
        );
        return Err(format!(
            "Refusing to launch terminal profile {} because the command looks risky: {}",
            resolved_profile.label, launch_preview
        ));
    }

    let rows = input.rows.unwrap_or(30).max(5);
    let cols = input.cols.unwrap_or(100).max(20);
    let started_at = timestamp();
    let use_tmux_backend = session_role == "agent" && tmux::available();
    let tmux_session_name =
        use_tmux_backend.then(|| tmux::safe_session_name(&input.workspace_id, &session_id));
    if let Some(tmux_name) = &tmux_session_name {
        tmux::start_detached(tmux_name, &cwd, &command_spec, cols, rows)?;
    }
    let session = TerminalSession {
        id: session_id.clone(),
        workspace_id: input.workspace_id.clone(),
        session_role,
        profile: resolved_profile.id.clone(),
        cwd: cwd.display().to_string(),
        status: "running".to_string(),
        started_at,
        ended_at: None,
        command: command_spec.command,
        args: command_spec.args,
        pid: None,
        stale: false,
        closed_at: None,
        backend: if use_tmux_backend { "tmux" } else { "pty" }.to_string(),
        tmux_session_name: tmux_session_name.clone(),
        title,
        terminal_kind: kind,
        display_order,
        is_visible: true,
        last_attached_at: None,
        last_captured_seq: 0,
        claude_session_id,
    };
    terminal_repository::insert_session(&state.db, &session)?;
    if let Ok(task_run_id) = task_lifecycle_service::start_task_run(
        state,
        &session.workspace_id,
        "terminal",
        Some(&session.id),
    ) {
        task_lifecycle_service::append_task_event(
            state,
            &task_run_id,
            &session.workspace_id,
            "terminal_started",
            serde_json::json!({
                "sessionId": session.id,
                "kind": session.terminal_kind,
                "profile": session.profile,
            }),
        );
        if session.terminal_kind == "run" {
            let memory_key = format!("run-command-{}", session.id);
            let memory_value = format!("{} {}", session.command, session.args.join(" "))
                .trim()
                .to_string();
            let _ = agent_memory_repository::upsert(
                &state.db,
                agent_memory_repository::AgentMemoryUpsert {
                    workspace_id: Some(&session.workspace_id),
                    scope: Some("workspace"),
                    key: &memory_key,
                    value: &memory_value,
                    origin: Some("auto"),
                    status: Some("active"),
                    confidence: Some(0.6),
                    source_task_run_id: Some(&task_run_id),
                    source_label: Some("Run command"),
                    source_detail: Some("Captured from a workspace run/check terminal start."),
                    last_used_at: Some(&timestamp()),
                },
            );
        }
    }
    record_terminal_start_activity(
        state,
        &session,
        &resolved_profile,
        &launch_command,
        &launch_args,
    );

    if let Some(tmux_name) = &tmux_session_name {
        let (attach_command, attach_args) = tmux::attach_command(tmux_name);
        spawn_active_terminal(
            state,
            &session,
            &attach_command,
            &attach_args,
            &cwd,
            cols,
            rows,
        )?;
    } else {
        spawn_active_terminal(
            state,
            &session,
            &launch_command,
            &launch_args,
            &cwd,
            cols,
            rows,
        )?;
    }

    terminal_repository::get_session(&state.db, &session.id)?
        .ok_or_else(|| format!("Terminal session {} was not found", session.id))
}

pub fn attach_workspace_terminal_session(
    state: &AppState,
    input: AttachWorkspaceTerminalInput,
) -> Result<TerminalSession, String> {
    let session = terminal_repository::get_session(&state.db, &input.session_id)?
        .ok_or_else(|| format!("Terminal session {} was not found", input.session_id))?;
    if session.workspace_id != input.workspace_id {
        return Err(format!(
            "Terminal session {} does not belong to workspace {}",
            input.session_id, input.workspace_id
        ));
    }
    // Already connected — nothing to do.
    if active_for_session(state, &session.id)?.is_some() {
        return Ok(session);
    }
    if session.backend == "tmux" {
        return attach_tmux_terminal(state, &session, input.cols, input.rows);
    }
    // PTY sessions can't be reattached once the process exits.
    if session.status != "running" {
        return Err(format!(
            "Terminal session {} has already ended ({})",
            session.id, session.status
        ));
    }
    // Session is marked running in DB but has no active PTY — it's orphaned.
    let ended_at = timestamp();
    terminal_repository::mark_finished(&state.db, &session.id, "interrupted", &ended_at, true)?;
    append_log_line(
        state,
        &session.workspace_id,
        &session.id,
        "system",
        "[mnemonic] Terminal process ended; start a new session\r\n",
    );
    Err(format!(
        "Terminal session {} is no longer running — start a new one",
        session.id
    ))
}

pub fn write_workspace_terminal_input(
    state: &AppState,
    workspace_id: &str,
    data: &str,
) -> Result<(), String> {
    if let Some(active) = active_for_workspace(state, workspace_id, "agent")? {
        return write_workspace_terminal_session_input(state, &active.session_id, data);
    }
    if let Some(session) =
        terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "agent")?
    {
        if session.backend == "tmux" && session.status == "running" {
            attach_tmux_terminal(state, &session, None, None)?;
            return write_workspace_terminal_session_input(state, &session.id, data);
        }
    }
    Err("No active terminal session for this workspace".to_string())
}

pub fn write_workspace_terminal_session_input(
    state: &AppState,
    session_id: &str,
    data: &str,
) -> Result<(), String> {
    let mut active = active_for_session(state, session_id)?;
    if active.is_none() {
        if let Some(session) = terminal_repository::get_session(&state.db, session_id)? {
            if session.backend == "tmux" && session.status == "running" {
                let _ = attach_tmux_terminal(state, &session, None, None)?;
                active = active_for_session(state, session_id)?;
            }
        }
    }
    let active = active.ok_or_else(|| format!("Terminal session {session_id} is not attached"))?;
    let is_shell_or_utility = matches!(active.terminal_kind.as_str(), "shell" | "utility");

    if is_shell_or_utility {
        let line = data.trim_end_matches(['\r', '\n']);
        if !line.is_empty() && command_safety_service::is_risky_command(line) {
            if let Ok(Some(session)) = terminal_repository::get_session(&state.db, session_id) {
                state
                    .pending_commands
                    .lock()
                    .map_err(|_| "Pending command registry lock poisoned".to_string())?
                    .insert(session_id.to_string(), data.to_string());
                state.event_emitter.emit(
                    crate::events::COMMAND_APPROVAL_REQUIRED,
                    &serde_json::json!(CommandApprovalEvent {
                        session_id: session_id.to_string(),
                        workspace_id: session.workspace_id,
                        command: line.to_string(),
                    }),
                );
                return Ok(());
            }
        }
    }
    let mut writer = active
        .writer
        .lock()
        .map_err(|_| "Terminal writer lock poisoned".to_string())?;
    writer
        .write_all(data.as_bytes())
        .map_err(|err| format!("Failed to write to terminal: {err}"))?;
    writer
        .flush()
        .map_err(|err| format!("Failed to flush terminal input: {err}"))?;
    Ok(())
}

/// Called after the user approves or denies a gated command.
pub fn approve_workspace_terminal_command(
    state: &AppState,
    session_id: &str,
    approved: bool,
) -> Result<(), String> {
    let data = state
        .pending_commands
        .lock()
        .map_err(|_| "Pending command registry lock poisoned".to_string())?
        .remove(session_id)
        .ok_or_else(|| format!("No pending command for session {session_id}"))?;

    if let Ok(Some(session)) = terminal_repository::get_session(&state.db, session_id) {
        let line = data.trim_end_matches(['\r', '\n']);
        let event = if approved {
            "Command approved"
        } else {
            "Command denied"
        };
        let _ = activity_repository::record(
            &state.db,
            &session.workspace_id,
            "",
            None,
            event,
            if approved { "warning" } else { "info" },
            Some(line),
        );
    }

    if approved {
        pty_write_raw(state, session_id, &data)
    } else {
        // Send Ctrl-C to cancel whatever the shell was about to execute.
        pty_write_raw(state, session_id, "\x03")
    }
}

/// Answers a decision dialog surfaced in chat by pressing its option key in
/// the terminal and then sending Enter. Claude Code's newer menu-style
/// dialogs require Enter to confirm the selection after typing the digit.
pub fn answer_workspace_terminal_decision(
    state: &AppState,
    session_id: &str,
    option_key: &str,
    option_label: &str,
) -> Result<(), String> {
    if option_key.is_empty()
        || option_key.len() > 2
        || !option_key.chars().all(|ch| ch.is_ascii_digit())
    {
        return Err(format!("Invalid decision option key: {option_key}"));
    }
    let session = terminal_repository::get_session(&state.db, session_id)?
        .ok_or_else(|| format!("Terminal session {session_id} was not found"))?;
    if active_for_session(state, session_id)?.is_none()
        && session.backend == "tmux"
        && session.status == "running"
    {
        let _ = attach_tmux_terminal(state, &session, None, None)?;
    }
    pty_write_raw(state, session_id, option_key)?;
    std::thread::sleep(std::time::Duration::from_millis(150));
    pty_write_raw(state, session_id, "\r")?;
    append_log_line(
        state,
        &session.workspace_id,
        session_id,
        "system",
        &format!("\r\n[mnemonic] answered \"{option_label}\" from chat\r\n"),
    );
    Ok(())
}

/// Writes bytes directly to the PTY without any safety check.
fn pty_write_raw(state: &AppState, session_id: &str, data: &str) -> Result<(), String> {
    let active = active_for_session(state, session_id)?
        .ok_or_else(|| format!("Terminal session {session_id} is not attached"))?;
    let mut writer = active
        .writer
        .lock()
        .map_err(|_| "Terminal writer lock poisoned".to_string())?;
    writer
        .write_all(data.as_bytes())
        .map_err(|err| format!("Failed to write to terminal: {err}"))?;
    writer
        .flush()
        .map_err(|err| format!("Failed to flush terminal input: {err}"))?;
    Ok(())
}

pub fn interrupt_workspace_terminal_session_by_id(
    state: &AppState,
    session_id: &str,
) -> Result<TerminalSession, String> {
    let session = terminal_repository::get_session(&state.db, session_id)?
        .ok_or_else(|| format!("Terminal session {session_id} was not found"))?;
    if active_for_session(state, session_id)?.is_none()
        && session.backend == "tmux"
        && session.status == "running"
    {
        let _ = attach_tmux_terminal(state, &session, None, None)?;
    }
    send_interrupt_to_session(state, &session)?;
    append_log_line(
        state,
        &session.workspace_id,
        session_id,
        "system",
        "\r\n[mnemonic] interrupt sent\r\n",
    );
    terminal_repository::get_session(&state.db, session_id)?
        .ok_or_else(|| format!("Terminal session {session_id} was not found"))
}

pub fn resize_workspace_terminal(
    state: &AppState,
    workspace_id: &str,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    if let Some(active) = active_for_workspace(state, workspace_id, "agent")? {
        return resize_workspace_terminal_session(state, &active.session_id, cols, rows);
    }
    if let Some(session) =
        terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "agent")?
    {
        if session.backend == "tmux" && session.status == "running" {
            attach_tmux_terminal(state, &session, Some(cols), Some(rows))?;
            return resize_workspace_terminal_session(state, &session.id, cols, rows);
        }
    }
    Err("No active terminal session for this workspace".to_string())
}

pub fn resize_workspace_terminal_session(
    state: &AppState,
    session_id: &str,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    if active_for_session(state, session_id)?.is_none() {
        if let Some(session) = terminal_repository::get_session(&state.db, session_id)? {
            if session.backend == "tmux" && session.status == "running" {
                let _ = attach_tmux_terminal(state, &session, Some(cols), Some(rows))?;
            }
        }
    }
    let active = active_for_session(state, session_id)?
        .ok_or_else(|| format!("Terminal session {session_id} is not attached"))?;
    let master = active
        .master
        .lock()
        .map_err(|_| "Terminal PTY lock poisoned".to_string())?;
    master
        .resize(PtySize {
            rows: rows.max(5),
            cols: cols.max(20),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| format!("Failed to resize terminal: {err}"))
}

pub fn stop_workspace_terminal_session(
    state: &AppState,
    workspace_id: &str,
) -> Result<TerminalSessionState, String> {
    log::info!(target: "mnemonic_lib", "stop_workspace_terminal_session: agent workspace_id={workspace_id}");
    if let Some(session) =
        terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "agent")?
    {
        stop_workspace_terminal_session_by_id(state, &session.id)?;
    }
    reconcile_orphan_running_session(state, workspace_id, "agent", "stopped")?;
    let out = get_workspace_terminal_session_state(state, workspace_id)?;
    log::info!(
        target: "mnemonic_lib",
        "stop_workspace_terminal_session: done workspace_id={workspace_id} active_session={}",
        out.active_session.as_ref().map(|s| s.id.as_str()).unwrap_or("-")
    );
    Ok(out)
}

pub fn interrupt_workspace_terminal_session(
    state: &AppState,
    workspace_id: &str,
) -> Result<TerminalSessionState, String> {
    match active_for_workspace(state, workspace_id, "agent")? {
        Some(active) => {
            let session = terminal_repository::get_session(&state.db, &active.session_id)?
                .ok_or_else(|| "Active terminal session record was not found".to_string())?;
            send_interrupt_to_session(state, &session)?;
            record_terminal_lifecycle_activity(state, &session, "Terminal session interrupted");
            let seq = Arc::new(AtomicU64::new(
                terminal_repository::next_seq(&state.db, &active.session_id).unwrap_or(0),
            ));
            append_output(
                Some(state.event_emitter.as_ref()),
                &state.db,
                workspace_id,
                &active.session_id,
                &seq,
                "system",
                "\r\n[mnemonic] interrupt sent\r\n",
            );
        }
        None => {
            if let Some(session) =
                terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "agent")?
            {
                if session.backend == "tmux" && session.status == "running" {
                    let attached = attach_tmux_terminal(state, &session, None, None)?;
                    send_interrupt_to_session(state, &attached)?;
                    append_log_line(
                        state,
                        workspace_id,
                        &attached.id,
                        "system",
                        "\r\n[mnemonic] interrupt sent\r\n",
                    );
                } else {
                    reconcile_orphan_running_session(state, workspace_id, "agent", "interrupted")?;
                }
            } else {
                reconcile_orphan_running_session(state, workspace_id, "agent", "interrupted")?;
            }
        }
    }
    get_workspace_terminal_session_state(state, workspace_id)
}

pub fn close_workspace_terminal_session(
    state: &AppState,
    workspace_id: &str,
) -> Result<TerminalSessionState, String> {
    if let Some(session) =
        terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "agent")?
    {
        close_workspace_terminal_session_by_id(state, &session.id)?;
    }
    get_workspace_terminal_session_state(state, workspace_id)
}

pub fn stop_workspace_terminal_session_by_id(
    state: &AppState,
    session_id: &str,
) -> Result<TerminalSession, String> {
    let session = terminal_repository::get_session(&state.db, session_id)?
        .ok_or_else(|| format!("Terminal session {session_id} was not found"))?;
    if let Some(tmux_name) = session.tmux_session_name.as_deref() {
        if session.backend == "tmux" && session.status == "running" {
            let _ = tmux::kill_session(tmux_name);
        }
    }
    detach_active_terminal(state, session_id);
    let _ = state
        .pending_commands
        .lock()
        .map(|mut m| m.remove(session_id));
    let ended_at = timestamp();
    terminal_repository::mark_finished(&state.db, session_id, "stopped", &ended_at, false)?;
    let task_run_id = format!("task-{}-terminal-{}", session.workspace_id, session_id);
    task_lifecycle_service::append_task_event(
        state,
        &task_run_id,
        &session.workspace_id,
        "terminal_stopped",
        serde_json::json!({ "sessionId": session_id }),
    );
    let _ = task_lifecycle_service::mark_task_run_completed(state, &task_run_id, "stopped");
    record_terminal_lifecycle_activity(state, &session, "Terminal session stopped");
    append_log_line(
        state,
        &session.workspace_id,
        session_id,
        "system",
        "[mnemonic] Terminal session stopped\r\n",
    );
    terminal_repository::get_session(&state.db, session_id)?
        .ok_or_else(|| format!("Terminal session {session_id} was not found"))
}

pub fn close_workspace_terminal_session_by_id(
    state: &AppState,
    session_id: &str,
) -> Result<TerminalSession, String> {
    let session = terminal_repository::get_session(&state.db, session_id)?
        .ok_or_else(|| format!("Terminal session {session_id} was not found"))?;
    if session.status == "running" {
        stop_workspace_terminal_session_by_id(state, session_id)?;
    }
    let closed_at = timestamp();
    terminal_repository::mark_closed(&state.db, session_id, &closed_at)?;
    let task_run_id = format!("task-{}-terminal-{}", session.workspace_id, session_id);
    task_lifecycle_service::append_task_event(
        state,
        &task_run_id,
        &session.workspace_id,
        "terminal_closed",
        serde_json::json!({ "sessionId": session_id }),
    );
    let _ = task_lifecycle_service::mark_task_run_completed(state, &task_run_id, "closed");
    record_terminal_lifecycle_activity(state, &session, "Terminal session closed");
    append_log_line(
        state,
        &session.workspace_id,
        session_id,
        "system",
        "[mnemonic] Session closed in workspace view; history retained\r\n",
    );
    let _ = terminal_repository::prune_output_chunks(
        &state.db,
        session_id,
        RETAINED_OUTPUT_CHUNKS_ON_CLOSE,
    );
    terminal_repository::get_session(&state.db, session_id)?
        .ok_or_else(|| format!("Terminal session {session_id} was not found"))
}

pub fn list_workspace_visible_terminal_sessions(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<TerminalSession>, String> {
    terminal_repository::list_visible_for_workspace(&state.db, workspace_id)
}

pub fn capture_workspace_terminal_scrollback(
    state: &AppState,
    session_id: &str,
) -> Result<TerminalOutputResponse, String> {
    let session = terminal_repository::get_session(&state.db, session_id)?
        .ok_or_else(|| format!("Terminal session {session_id} was not found"))?;
    get_workspace_terminal_output_for_session(state, &session.workspace_id, session_id, Some(0))
}

pub fn get_workspace_terminal_session_state(
    state: &AppState,
    workspace_id: &str,
) -> Result<TerminalSessionState, String> {
    let active_session = match active_for_workspace(state, workspace_id, "agent")? {
        Some(active) => terminal_repository::get_session(&state.db, &active.session_id)?,
        None => None,
    };
    let latest_session =
        terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "agent")?;

    Ok(TerminalSessionState {
        active_session,
        latest_session,
    })
}

pub fn get_workspace_terminal_output(
    state: &AppState,
    workspace_id: &str,
    since_seq: Option<u64>,
) -> Result<TerminalOutputResponse, String> {
    let session = match active_for_workspace(state, workspace_id, "agent")? {
        Some(active) => terminal_repository::get_session(&state.db, &active.session_id)?,
        None => terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "agent")?,
    };

    let Some(session) = session else {
        return Ok(TerminalOutputResponse {
            session: None,
            chunks: vec![],
            next_seq: 0,
        });
    };

    let chunks =
        terminal_repository::list_output_chunks(&state.db, &session.id, since_seq.unwrap_or(0))?;
    let next_seq = terminal_repository::next_seq(&state.db, &session.id).unwrap_or(0);

    Ok(TerminalOutputResponse {
        session: Some(session),
        chunks,
        next_seq,
    })
}

pub fn get_workspace_terminal_output_for_session(
    state: &AppState,
    workspace_id: &str,
    session_id: &str,
    since_seq: Option<u64>,
) -> Result<TerminalOutputResponse, String> {
    let _ = workspace_root_path(state, workspace_id)?;
    let session = terminal_repository::get_session(&state.db, session_id)?
        .ok_or_else(|| format!("Terminal session {session_id} was not found"))?;
    if session.workspace_id != workspace_id {
        return Err(format!(
            "Terminal session {session_id} does not belong to workspace {workspace_id}"
        ));
    }

    let chunks =
        terminal_repository::list_output_chunks(&state.db, &session.id, since_seq.unwrap_or(0))?;
    let next_seq = terminal_repository::next_seq(&state.db, &session.id).unwrap_or(0);

    Ok(TerminalOutputResponse {
        session: Some(session),
        chunks,
        next_seq,
    })
}

pub fn list_workspace_terminal_sessions(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<TerminalSession>, String> {
    terminal_repository::list_for_workspace(&state.db, workspace_id)
}

pub fn reconnect_workspace_terminal_session(
    state: &AppState,
    workspace_id: &str,
    session_id: Option<&str>,
) -> Result<TerminalSessionState, String> {
    let _ = workspace_root_path(state, workspace_id)?;
    if let Some(session_id) = session_id {
        let active = active_for_workspace(state, workspace_id, "agent")?;
        if let Some(active) = active {
            if active.session_id != session_id {
                return Err(format!(
                    "Active terminal session mismatch. Requested {session_id}, active {}",
                    active.session_id
                ));
            }
        } else {
            let latest = terminal_repository::latest_for_workspace_role(
                &state.db,
                workspace_id,
                "agent",
            )?
            .ok_or_else(|| format!("No terminal session found for workspace {workspace_id}"))?;
            if latest.id != session_id {
                return Err(format!(
                    "Session {session_id} is not the latest known session for workspace {workspace_id}"
                ));
            }
            if latest.backend == "tmux" && latest.status == "running" {
                let _ = attach_tmux_terminal(state, &latest, None, None)?;
            }
        }
    } else if let Some(latest) =
        terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "agent")?
    {
        if active_for_session(state, &latest.id)?.is_none()
            && latest.backend == "tmux"
            && latest.status == "running"
        {
            let _ = attach_tmux_terminal(state, &latest, None, None)?;
        }
    }
    get_workspace_terminal_session_state(state, workspace_id)
}

pub fn queue_workspace_agent_prompt(
    state: &AppState,
    input: QueueAgentPromptInput,
) -> Result<AgentPromptEntry, String> {
    queue_workspace_agent_prompt_impl(state, input)
}

pub fn batch_dispatch_workspace_agent_prompt(
    state: &AppState,
    input: crate::models::BatchDispatchPromptInput,
) -> Result<Vec<AgentPromptEntry>, String> {
    batch_dispatch_workspace_agent_prompt_impl(state, input)
}

pub fn list_workspace_agent_prompts(
    state: &AppState,
    workspace_id: &str,
    limit: Option<u32>,
) -> Result<Vec<AgentPromptEntry>, String> {
    list_workspace_agent_prompts_impl(state, workspace_id, limit)
}

pub fn write_workspace_utility_terminal_input(
    state: &AppState,
    workspace_id: &str,
    data: &str,
) -> Result<(), String> {
    let active = active_for_workspace(state, workspace_id, "utility")?
        .ok_or_else(|| "No active utility terminal session for this workspace".to_string())?;
    write_workspace_terminal_session_input(state, &active.session_id, data)
}

pub fn resize_workspace_utility_terminal(
    state: &AppState,
    workspace_id: &str,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let active = active_for_workspace(state, workspace_id, "utility")?
        .ok_or_else(|| "No active utility terminal session for this workspace".to_string())?;
    resize_workspace_terminal_session(state, &active.session_id, cols, rows)
}

pub fn stop_workspace_utility_terminal_session(
    state: &AppState,
    workspace_id: &str,
) -> Result<TerminalSessionState, String> {
    log::info!(target: "mnemonic_lib", "stop_workspace_utility_terminal_session: workspace_id={workspace_id}");
    if let Some(session) =
        terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "utility")?
    {
        stop_workspace_terminal_session_by_id(state, &session.id)?;
    }
    reconcile_orphan_running_session(state, workspace_id, "utility", "stopped")?;
    let out = get_workspace_utility_terminal_session_state(state, workspace_id)?;
    log::info!(
        target: "mnemonic_lib",
        "stop_workspace_utility_terminal_session: done workspace_id={workspace_id} active_session={}",
        out.active_session.as_ref().map(|s| s.id.as_str()).unwrap_or("-")
    );
    Ok(out)
}

pub fn get_workspace_utility_terminal_session_state(
    state: &AppState,
    workspace_id: &str,
) -> Result<TerminalSessionState, String> {
    let active_session = match active_for_workspace(state, workspace_id, "utility")? {
        Some(active) => terminal_repository::get_session(&state.db, &active.session_id)?,
        None => None,
    };
    let latest_session =
        terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "utility")?;

    Ok(TerminalSessionState {
        active_session,
        latest_session,
    })
}

pub fn get_workspace_utility_terminal_output(
    state: &AppState,
    workspace_id: &str,
    since_seq: Option<u64>,
) -> Result<TerminalOutputResponse, String> {
    let session = match active_for_workspace(state, workspace_id, "utility")? {
        Some(active) => terminal_repository::get_session(&state.db, &active.session_id)?,
        None => terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "utility")?,
    };

    let Some(session) = session else {
        return Ok(TerminalOutputResponse {
            session: None,
            chunks: vec![],
            next_seq: 0,
        });
    };

    let chunks =
        terminal_repository::list_output_chunks(&state.db, &session.id, since_seq.unwrap_or(0))?;
    let next_seq = chunks
        .last()
        .map(|chunk| chunk.seq.saturating_add(1))
        .unwrap_or_else(|| terminal_repository::next_seq(&state.db, &session.id).unwrap_or(0));

    Ok(TerminalOutputResponse {
        session: Some(session),
        chunks,
        next_seq,
    })
}

pub fn reconnect_workspace_utility_terminal_session(
    state: &AppState,
    workspace_id: &str,
    session_id: Option<&str>,
) -> Result<TerminalSessionState, String> {
    let _ = workspace_root_path(state, workspace_id)?;
    if let Some(session_id) = session_id {
        let active = active_for_workspace(state, workspace_id, "utility")?;
        if let Some(active) = active {
            if active.session_id != session_id {
                return Err(format!(
                    "Active utility terminal session mismatch. Requested {session_id}, active {}",
                    active.session_id
                ));
            }
        } else {
            let latest =
                terminal_repository::latest_for_workspace_role(&state.db, workspace_id, "utility")?
                    .ok_or_else(|| {
                        format!("No utility terminal session found for workspace {workspace_id}")
                    })?;
            if latest.id != session_id {
                return Err(format!(
                    "Session {session_id} is not the latest known utility session for workspace {workspace_id}"
                ));
            }
        }
    }
    get_workspace_utility_terminal_session_state(state, workspace_id)
}

pub fn timestamp() -> String {
    output::timestamp()
}

/// Build a PATH that includes common macOS binary locations.
/// When Tauri launches from Finder the inherited PATH is minimal.
pub fn enriched_path() -> String {
    enriched_path_impl()
}

/// Enriched PATH with the launch binary's own directory prepended. An
/// npm/nvm-installed `claude` is a script that needs `node` from the same bin
/// directory, which is not in the enriched PATH when launched from Finder.
fn path_for_command(command: &str) -> String {
    let base = enriched_path();
    match Path::new(command)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => format!("{}:{base}", parent.display()),
        None => base,
    }
}

/// Whether `claude --resume <id>` can succeed for this worktree. Claude Code
/// stores transcripts under `~/.claude/projects/<munged-cwd>/<id>.jsonl` and
/// exits immediately when the transcript is missing (never-used sessions,
/// pruned history). If the project directory cannot be located at all the
/// check is inconclusive and we allow the resume rather than silently
/// breaking session continuity.
fn claude_session_resumable(cwd: &Path, session_id: &str) -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return true;
    };
    let projects_dir = PathBuf::from(home).join(".claude").join("projects");
    let project_dir = projects_dir.join(claude_project_dir_name(cwd));
    if project_dir.is_dir() {
        return project_dir.join(format!("{session_id}.jsonl")).is_file();
    }
    // Munging mismatch or fresh machine: fall back to scanning project dirs.
    let Ok(entries) = std::fs::read_dir(&projects_dir) else {
        return true;
    };
    let transcript_name = format!("{session_id}.jsonl");
    entries
        .flatten()
        .any(|entry| entry.path().join(&transcript_name).is_file())
}

/// Claude Code maps a project path to a directory name by replacing every
/// non-alphanumeric character with '-': /Users/jay/dev/forge -> -Users-jay-dev-forge
fn claude_project_dir_name(cwd: &Path) -> String {
    cwd.display()
        .to_string()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn unique_suffix() -> String {
    output_unique_suffix()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AgentProfile;
    use crate::repositories::settings_repository;
    use crate::state::{AppState, NoopEventEmitter};
    use std::sync::Arc;

    fn isolated_state(label: &str) -> AppState {
        let suffix = unique_suffix();
        let root = std::env::temp_dir().join(format!(
            "mnemonic-core-{label}-{}-{suffix}",
            std::process::id()
        ));
        let data_dir = root.join("data");
        let cache_dir = root.join("cache");
        AppState::initialize(data_dir, cache_dir, Arc::new(NoopEventEmitter)).expect("app state")
    }

    #[test]
    fn formats_terminal_lifecycle_activity_details() {
        let details = activity::format_terminal_lifecycle_activity_details(&TerminalSession {
            id: "term-123".to_string(),
            workspace_id: "ws-123".to_string(),
            session_role: "agent".to_string(),
            profile: "codex".to_string(),
            cwd: "/tmp/forge-workspace".to_string(),
            status: "running".to_string(),
            started_at: "now".to_string(),
            ended_at: None,
            command: "codex".to_string(),
            args: vec![],
            pid: Some(42),
            stale: false,
            closed_at: None,
            backend: "pty".to_string(),
            tmux_session_name: None,
            title: "Codex".to_string(),
            terminal_kind: "agent".to_string(),
            display_order: 0,
            is_visible: true,
            last_attached_at: None,
            last_captured_seq: 0,
            claude_session_id: None,
        });

        assert!(details.contains("term-123"));
        assert!(details.contains("role: agent"));
        assert!(details.contains("profile: codex"));
        assert!(details.contains("pid: 42"));
        assert!(details.contains("/tmp/forge-workspace"));
    }

    #[test]
    fn formats_ollama_multiline_prompt_as_single_repl_message() {
        let session = TerminalSession {
            id: "term-local".to_string(),
            workspace_id: "ws-123".to_string(),
            session_role: "agent".to_string(),
            profile: "qwen-local".to_string(),
            cwd: "/tmp/forge-workspace".to_string(),
            status: "running".to_string(),
            started_at: "now".to_string(),
            ended_at: None,
            command: "/usr/local/bin/ollama".to_string(),
            args: vec!["run".to_string(), "qwen2.5-coder:7b".to_string()],
            pid: None,
            stale: false,
            closed_at: None,
            backend: "pty".to_string(),
            tmux_session_name: None,
            title: "Ollama qwen".to_string(),
            terminal_kind: "agent".to_string(),
            display_order: 0,
            is_visible: true,
            last_attached_at: None,
            last_captured_seq: 0,
            claude_session_id: None,
        };
        match prompts::terminal_prompt_payload_for_session(&session, "line one\nline two") {
            prompts::PromptPayload::Single(payload) => {
                assert_eq!(payload, "\"\"\"\nline one\nline two\n\"\"\"\r\n");
            }
            prompts::PromptPayload::PasteThenEnter(_) => {
                panic!("ollama prompts should be written as a single REPL message");
            }
        }
    }

    #[test]
    fn wraps_claude_prompts_in_bracketed_paste_without_trailing_enter() {
        let session = TerminalSession {
            id: "term-claude".to_string(),
            workspace_id: "ws-123".to_string(),
            session_role: "agent".to_string(),
            profile: "claude-code".to_string(),
            cwd: "/tmp/forge-workspace".to_string(),
            status: "running".to_string(),
            started_at: "now".to_string(),
            ended_at: None,
            command: "/usr/local/bin/claude".to_string(),
            args: vec![],
            pid: None,
            stale: false,
            closed_at: None,
            backend: "pty".to_string(),
            tmux_session_name: None,
            title: "Claude".to_string(),
            terminal_kind: "agent".to_string(),
            display_order: 0,
            is_visible: true,
            last_attached_at: None,
            last_captured_seq: 0,
            claude_session_id: None,
        };
        match prompts::terminal_prompt_payload_for_session(&session, "Use plan mode.\n\nfix it") {
            prompts::PromptPayload::PasteThenEnter(payload) => {
                assert_eq!(payload, "\x1b[200~Use plan mode.\n\nfix it\x1b[201~");
            }
            prompts::PromptPayload::Single(_) => {
                panic!("claude prompts must defer Enter to a separate write");
            }
        }
    }

    #[test]
    fn munges_claude_project_dir_name_like_claude_code() {
        assert_eq!(
            claude_project_dir_name(Path::new("/Users/jay/dev/forge")),
            "-Users-jay-dev-forge"
        );
        assert_eq!(
            claude_project_dir_name(Path::new("/tmp/my_repo.worktrees/feat-1")),
            "-tmp-my-repo-worktrees-feat-1"
        );
    }

    #[test]
    fn visible_claude_terminal_command_uses_shared_profile_defaults() {
        let state = isolated_state("visible-claude-defaults");
        settings_repository::set_value(&state.db, "claude_agent_default_model", "claude-sonnet")
            .expect("model setting");

        let (program, args) = visible_agent_terminal_command(
            &state,
            "missing-workspace-ok",
            Some("claude-code"),
            None,
        )
        .expect("visible command");

        assert!(
            program.ends_with("claude"),
            "expected claude program, got {program}"
        );
        assert_arg_pair(&args, "--permission-mode", "bypassPermissions");
        assert_arg_pair(&args, "--model", "claude-sonnet");
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--session-id" && uuid::Uuid::parse_str(&pair[1]).is_ok()),
            "expected fresh Claude --session-id in {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg == "--continue"),
            "visible GPUI Claude terminals should start an explicit fresh session: {args:?}"
        );
    }

    #[test]
    fn terminal_profile_does_not_duplicate_explicit_claude_permission_flags() {
        let profile = AgentProfile {
            id: "custom-claude".to_string(),
            label: "Custom Claude".to_string(),
            agent: "claude_code".to_string(),
            command: "claude".to_string(),
            args: vec![
                "--permission-mode".to_string(),
                "default".to_string(),
                "--model".to_string(),
                "profile-model".to_string(),
            ],
            model: Some("ignored-model".to_string()),
            reasoning: None,
            mode: Some("act".to_string()),
            provider: None,
            endpoint: None,
            local: false,
            description: None,
            skills: vec![],
            templates: vec![],
            role_preference: None,
            coordinator_eligible: Some(true),
        };

        let session_id = "11111111-1111-4111-8111-111111111111".to_string();
        let terminal_profile = TerminalProfile::from_agent_profile(
            &profile,
            Some("global-model"),
            Some(&SessionResume::New(session_id.clone())),
            &[],
        );

        assert_eq!(
            terminal_profile
                .args
                .iter()
                .filter(|arg| arg.as_str() == "--permission-mode")
                .count(),
            1
        );
        assert_arg_pair(&terminal_profile.args, "--permission-mode", "default");
        assert_arg_pair(&terminal_profile.args, "--model", "profile-model");
        assert_arg_pair(&terminal_profile.args, "--session-id", &session_id);
    }

    fn assert_arg_pair(args: &[String], flag: &str, value: &str) {
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == flag && pair[1] == value),
            "expected {flag} {value} in {args:?}"
        );
    }

    #[test]
    fn formats_local_terminal_start_activity_details() {
        let session = TerminalSession {
            id: "term-local".to_string(),
            workspace_id: "ws-123".to_string(),
            session_role: "agent".to_string(),
            profile: "ollama-local".to_string(),
            cwd: "/tmp/forge-workspace".to_string(),
            status: "running".to_string(),
            started_at: "now".to_string(),
            ended_at: None,
            command: "ollama".to_string(),
            args: vec!["run".to_string(), "qwen coder".to_string()],
            pid: None,
            stale: false,
            closed_at: None,
            backend: "pty".to_string(),
            tmux_session_name: None,
            title: "Ollama Local".to_string(),
            terminal_kind: "agent".to_string(),
            display_order: 0,
            is_visible: true,
            last_attached_at: None,
            last_captured_seq: 0,
            claude_session_id: None,
        };
        let profile = AgentProfile {
            id: "ollama-local".to_string(),
            label: "Ollama Local".to_string(),
            agent: "local_llm".to_string(),
            command: "ollama".to_string(),
            args: vec!["run".to_string(), "qwen coder".to_string()],
            model: Some("qwen coder".to_string()),
            reasoning: None,
            mode: Some("act".to_string()),
            provider: Some("ollama".to_string()),
            endpoint: Some("http://localhost:11434".to_string()),
            local: true,
            description: None,
            skills: vec![],
            templates: vec![],
            role_preference: None,
            coordinator_eligible: None,
        };

        let details = activity::format_terminal_start_activity_details(
            &session,
            &profile,
            "ollama",
            &["run".to_string(), "qwen coder".to_string()],
        );

        assert!(details.contains("runtime: local"));
        assert!(details.contains("provider: ollama"));
        assert!(details.contains("model: qwen coder"));
        assert!(details.contains("endpoint: http://localhost:11434"));
        assert!(details.contains("ollama run 'qwen coder'"));
    }

    #[test]
    fn formats_blocked_terminal_launch_details() {
        let profile = AgentProfile {
            id: "risky-local".to_string(),
            label: "Risky Local".to_string(),
            agent: "local_llm".to_string(),
            command: "rm".to_string(),
            args: vec!["-rf".to_string(), "/tmp/example".to_string()],
            model: None,
            reasoning: None,
            mode: Some("act".to_string()),
            provider: Some("custom".to_string()),
            endpoint: None,
            local: true,
            description: None,
            skills: vec![],
            templates: vec![],
            role_preference: None,
            coordinator_eligible: None,
        };

        let details =
            activity::format_blocked_terminal_launch_details(&profile, "rm -rf /tmp/example");
        assert!(details.contains("Risky Local"));
        assert!(details.contains("runtime: local"));
        assert!(details.contains("provider: custom"));
        assert!(details.contains("risky-command patterns"));
    }
}
