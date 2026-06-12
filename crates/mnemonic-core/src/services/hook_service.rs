use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::models::{WorkspaceHookCommand, WorkspaceHookEvent, WorkspaceHookInspector};
use crate::repositories::{activity_repository, settings_repository, workspace_repository};
use crate::services::{command_safety_service, workspace_script_service};
use crate::state::AppState;

#[derive(Debug, Clone, Copy)]
pub enum HookPhase {
    Pre,
    Post,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn run_workspace_hooks(
    state: &AppState,
    workspace_id: &str,
    hook_kind: &str,
    phase: HookPhase,
    context: &Value,
) -> Result<(), String> {
    let config = workspace_script_service::get_workspace_mnemonic_config(state, workspace_id)?;
    let commands = match hook_kind {
        "run" => match phase {
            HookPhase::Pre => config.hooks.pre_run,
            HookPhase::Post => config.hooks.post_run,
        },
        "tool" => match phase {
            HookPhase::Pre => config.hooks.pre_tool,
            HookPhase::Post => config.hooks.post_tool,
        },
        "ship" => match phase {
            HookPhase::Pre => config.hooks.pre_ship,
            HookPhase::Post => config.hooks.post_ship,
        },
        other => {
            return Err(format!("Unsupported hook kind: {other}"));
        }
    };

    if commands.is_empty() {
        return Ok(());
    }

    let root = workspace_script_service::workspace_root_path(state, workspace_id)?;
    let ctx_json = serde_json::to_string(context).unwrap_or_else(|_| "{}".to_string());
    let blocking = matches!(phase, HookPhase::Pre);

    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found while executing hooks"))?;

    for (index, command) in commands.iter().enumerate() {
        let label = format!(
            "{}:{}:{}",
            hook_kind,
            if blocking { "pre" } else { "post" },
            index + 1
        );
        let started = now_secs();

        if command_safety_service::is_risky_command(command)
            && !workspace_script_service::risky_workspace_scripts_enabled(state)
        {
            let reason = format!(
                "Blocked hook {label}. Enable risky workspace scripts in Settings to run: {command}"
            );
            let _ = activity_repository::record(
                &state.db,
                workspace_id,
                &workspace.summary.repo,
                Some(&workspace.summary.branch),
                "Workspace hook blocked",
                if blocking { "warning" } else { "info" },
                Some(&reason),
            );
            if blocking {
                return Err(reason);
            }
            continue;
        }

        let output = std::process::Command::new("zsh")
            .args(["-lc", command])
            .current_dir(&root)
            .env("MNEMONIC_HOOK_CONTEXT", &ctx_json)
            .output();

        let ended = now_secs();
        match output {
            Ok(output) => {
                let exit_code = output.status.code().unwrap_or(-1);
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let details = format!(
                    "hook={label} exit={exit_code} duration={}s{}",
                    ended.saturating_sub(started),
                    if stderr.is_empty() {
                        String::new()
                    } else {
                        format!(" stderr={stderr}")
                    }
                );
                let _ = activity_repository::record(
                    &state.db,
                    workspace_id,
                    &workspace.summary.repo,
                    Some(&workspace.summary.branch),
                    "Workspace hook finished",
                    if output.status.success() {
                        "info"
                    } else {
                        "warning"
                    },
                    Some(&details),
                );

                if blocking && !output.status.success() {
                    return Err(format!(
                        "Pre-hook `{label}` failed (exit {exit_code}). Fix the hook or remove it from .forge/config.json hooks.{hook_kind}."
                    ));
                }
            }
            Err(err) => {
                let details = format!("hook={label} failed to start: {err}");
                let _ = activity_repository::record(
                    &state.db,
                    workspace_id,
                    &workspace.summary.repo,
                    Some(&workspace.summary.branch),
                    "Workspace hook failed",
                    "warning",
                    Some(&details),
                );
                if blocking {
                    return Err(format!("Pre-hook `{label}` failed to start: {err}"));
                }
            }
        }
    }

    Ok(())
}

pub fn get_workspace_hook_inspector(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceHookInspector, String> {
    let config = workspace_script_service::get_workspace_mnemonic_config(state, workspace_id)?;
    let risky_scripts_enabled =
        settings_repository::get_value(&state.db, "allow_risky_workspace_scripts")?
            .map(|value| value == "true")
            .unwrap_or(false);

    let commands = collect_hook_commands(&config.hooks)
        .into_iter()
        .map(
            |(id, hook_kind, phase, label, command)| WorkspaceHookCommand {
                id,
                hook_kind,
                phase,
                label,
                safety: command_safety_service::check_command_safety(&command),
                will_block_when_risky: !risky_scripts_enabled
                    && command_safety_service::is_risky_command(&command),
                command,
            },
        )
        .collect::<Vec<_>>();

    let recent_events = activity_repository::list_for_workspace(&state.db, workspace_id, 80)?
        .into_iter()
        .filter_map(map_activity_to_hook_event)
        .take(12)
        .collect::<Vec<_>>();

    Ok(WorkspaceHookInspector {
        workspace_id: workspace_id.to_string(),
        config_path: config.path,
        risky_scripts_enabled,
        commands,
        recent_events,
    })
}

fn collect_hook_commands(
    hooks: &crate::models::ForgeWorkspaceHooks,
) -> Vec<(String, String, String, String, String)> {
    let groups = [
        ("run", "pre", hooks.pre_run.clone()),
        ("run", "post", hooks.post_run.clone()),
        ("tool", "pre", hooks.pre_tool.clone()),
        ("tool", "post", hooks.post_tool.clone()),
        ("ship", "pre", hooks.pre_ship.clone()),
        ("ship", "post", hooks.post_ship.clone()),
    ];

    let mut commands = Vec::new();
    for (hook_kind, phase, entries) in groups {
        for (index, command) in entries.into_iter().enumerate() {
            let ordinal = index + 1;
            let label = format!("{hook_kind}:{phase}:{ordinal}");
            commands.push((
                format!("hook-{hook_kind}-{phase}-{ordinal}"),
                hook_kind.to_string(),
                phase.to_string(),
                label,
                command,
            ));
        }
    }
    commands
}

fn map_activity_to_hook_event(activity: crate::models::ActivityItem) -> Option<WorkspaceHookEvent> {
    let (category, event, status) = match activity.event.as_str() {
        "Workspace hook finished" => (
            "hook",
            "hook_finished",
            infer_hook_status(activity.details.as_deref()),
        ),
        "Workspace hook failed" => ("hook", "hook_failed", "failed".to_string()),
        "Workspace hook blocked" => ("guardrail", "hook_blocked", "blocked".to_string()),
        "Workspace script blocked" => (
            "guardrail",
            "workspace_script_blocked",
            "blocked".to_string(),
        ),
        "Terminal launch blocked" => (
            "guardrail",
            "terminal_launch_blocked",
            "blocked".to_string(),
        ),
        _ => return None,
    };

    Some(WorkspaceHookEvent {
        id: activity.id,
        category: category.to_string(),
        label: extract_hook_label(activity.details.as_deref()),
        event: event.to_string(),
        status,
        level: activity.level,
        detail: activity.details,
        timestamp: activity.timestamp,
    })
}

fn extract_hook_label(details: Option<&str>) -> Option<String> {
    let details = details?;
    if let Some(start) = details.find("hook=") {
        let rest = &details[start + 5..];
        let label = rest.split_whitespace().next().unwrap_or("").trim();
        if !label.is_empty() {
            return Some(label.to_string());
        }
    }
    if let Some(start) = details.find("Blocked hook ") {
        let rest = &details[start + "Blocked hook ".len()..];
        let label = rest.split('.').next().unwrap_or("").trim();
        if !label.is_empty() {
            return Some(label.to_string());
        }
    }
    None
}

fn infer_hook_status(details: Option<&str>) -> String {
    let details = details.unwrap_or_default();
    if details.contains(" exit=0 ") || details.ends_with(" exit=0") {
        "succeeded".to_string()
    } else if details.contains("exit=") {
        "failed".to_string()
    } else {
        "finished".to_string()
    }
}
