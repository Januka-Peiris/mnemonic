use crate::models::{AgentProfile, TerminalSession};
use crate::repositories::{activity_repository, workspace_repository};
use crate::state::AppState;

pub(super) fn record_terminal_lifecycle_activity(
    state: &AppState,
    session: &TerminalSession,
    event: &str,
) {
    let workspace = match workspace_repository::get_detail(&state.db, &session.workspace_id) {
        Ok(Some(workspace)) => workspace,
        _ => return,
    };
    let details = format_terminal_lifecycle_activity_details(session);
    let _ = activity_repository::record(
        &state.db,
        &session.workspace_id,
        &workspace.summary.repo,
        Some(&workspace.summary.branch),
        event,
        "info",
        Some(&details),
    );
}

pub(super) fn record_terminal_start_activity(
    state: &AppState,
    session: &TerminalSession,
    profile: &AgentProfile,
    command: &str,
    args: &[String],
) {
    let workspace = match workspace_repository::get_detail(&state.db, &session.workspace_id) {
        Ok(Some(workspace)) => workspace,
        _ => return,
    };
    let details = format_terminal_start_activity_details(session, profile, command, args);
    let _ = activity_repository::record(
        &state.db,
        &session.workspace_id,
        &workspace.summary.repo,
        Some(&workspace.summary.branch),
        "Terminal session started",
        "info",
        Some(&details),
    );
}

pub(super) fn record_blocked_terminal_launch_activity(
    state: &AppState,
    workspace_id: &str,
    profile: &AgentProfile,
    command_preview: &str,
) {
    let workspace = match workspace_repository::get_detail(&state.db, workspace_id) {
        Ok(Some(workspace)) => workspace,
        _ => return,
    };
    let details = format_blocked_terminal_launch_details(profile, command_preview);
    let _ = activity_repository::record(
        &state.db,
        workspace_id,
        &workspace.summary.repo,
        Some(&workspace.summary.branch),
        "Terminal launch blocked",
        "warning",
        Some(&details),
    );
}

pub(super) fn format_blocked_terminal_launch_details(
    profile: &AgentProfile,
    command_preview: &str,
) -> String {
    let mut details = vec![
        format!("profile: {} ({})", profile.label, profile.id),
        format!("command: {command_preview}"),
    ];
    if profile.local {
        details.push("runtime: local".to_string());
    }
    if let Some(provider) = profile
        .provider
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        details.push(format!("provider: {provider}"));
    }
    format!(
        "{}. Command matched Mnemonic risky-command patterns.",
        details.join("; ")
    )
}

pub(super) fn format_terminal_start_activity_details(
    session: &TerminalSession,
    profile: &AgentProfile,
    command: &str,
    args: &[String],
) -> String {
    let mut parts = vec![
        format!("Session {}", session.id),
        format!("role: {}", session.session_role),
        format!("kind: {}", session.terminal_kind),
        format!("profile: {} ({})", profile.label, profile.id),
        format!("command: {}", command_preview(command, args)),
    ];
    if profile.local {
        parts.push("runtime: local".to_string());
    }
    if let Some(provider) = profile
        .provider
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("provider: {provider}"));
    }
    if let Some(model) = profile.model.as_deref().filter(|value| !value.is_empty()) {
        parts.push(format!("model: {model}"));
    }
    if let Some(endpoint) = profile
        .endpoint
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("endpoint: {endpoint}"));
    }
    parts.push(format!("cwd: {}", session.cwd));
    format!("{}.", parts.join("; "))
}

pub(super) fn format_terminal_lifecycle_activity_details(session: &TerminalSession) -> String {
    let pid = session
        .pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "Session {}; role: {}; kind: {}; title: {}; profile: {}; pid: {}; cwd: {}.",
        session.id,
        session.session_role,
        session.terminal_kind,
        session.title,
        session.profile,
        pid,
        session.cwd
    )
}

pub(super) fn command_preview(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .map(quote_arg_if_needed)
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_arg_if_needed(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    if !arg
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '\'' | '"' | '\\'))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}
