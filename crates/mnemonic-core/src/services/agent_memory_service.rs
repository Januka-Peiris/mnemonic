use std::collections::HashMap;

use crate::repositories::{agent_memory_repository, workspace_repository};
use crate::services::workspace_script_service;
use crate::state::AppState;

pub fn sync_candidate_memories(state: &AppState, workspace_id: Option<&str>) -> Result<(), String> {
    let workspaces = workspace_repository::list(&state.db)?;
    for workspace in workspaces {
        if workspace_id.is_some() && workspace_id != Some(workspace.id.as_str()) {
            continue;
        }
        sync_workspace_candidates(state, &workspace.id)?;
    }
    Ok(())
}

fn sync_workspace_candidates(state: &AppState, workspace_id: &str) -> Result<(), String> {
    let workspace = workspace_repository::get(&state.db, workspace_id)?;

    if !workspace.current_task.trim().is_empty() {
        let _ = agent_memory_repository::upsert_candidate(
            &state.db,
            agent_memory_repository::AgentMemoryCandidateUpsert {
                workspace_id: Some(workspace_id),
                scope: Some("workspace"),
                key: "workspace-goal",
                value: workspace.current_task.trim(),
                confidence: 0.72,
                source_label: Some("Accepted workspace task"),
                source_detail: Some("Derived from the workspace's current task / plan prompt."),
                source_task_run_id: None,
            },
        )?;
    }

    let config = workspace_script_service::get_workspace_mnemonic_config(state, workspace_id)?;
    if config.exists && config.warning.is_none() {
        let summary = summarize_workspace_rules(&config);
        if !summary.is_empty() {
            let _ = agent_memory_repository::upsert_candidate(
                &state.db,
                agent_memory_repository::AgentMemoryCandidateUpsert {
                    workspace_id: Some(workspace_id),
                    scope: Some("workspace"),
                    key: "workspace-rules",
                    value: &summary,
                    confidence: 0.68,
                    source_label: Some(".forge/config.json"),
                    source_detail: Some(
                        "Derived from durable workspace setup/run/hook configuration.",
                    ),
                    source_task_run_id: None,
                },
            )?;
        }
    }

    let memories = agent_memory_repository::list_for_workspace(&state.db, workspace_id)?;
    let mut repeated_commands: HashMap<String, usize> = HashMap::new();
    for memory in memories {
        if memory.origin == "auto"
            && memory.status == "active"
            && memory.key.starts_with("run-command-")
            && !memory.value.trim().is_empty()
        {
            *repeated_commands
                .entry(memory.value.trim().to_string())
                .or_insert(0) += 1;
        }
    }

    for (command, count) in repeated_commands
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .take(3)
    {
        let key = format!("run-pattern-{}", slugify_for_key(&command));
        let value = format!("Repeated run/check pattern: {command}");
        let detail = format!("Observed in {count} run/check sessions for this workspace.");
        let _ = agent_memory_repository::upsert_candidate(
            &state.db,
            agent_memory_repository::AgentMemoryCandidateUpsert {
                workspace_id: Some(workspace_id),
                scope: Some("workspace"),
                key: &key,
                value: &value,
                confidence: 0.64,
                source_label: Some("Repeated run pattern"),
                source_detail: Some(detail.as_str()),
                source_task_run_id: None,
            },
        )?;
    }

    Ok(())
}

fn summarize_workspace_rules(config: &crate::models::ForgeWorkspaceConfig) -> String {
    let mut parts = Vec::new();
    if !config.setup.is_empty() {
        parts.push(format!(
            "Setup: {}",
            config
                .setup
                .iter()
                .take(2)
                .cloned()
                .collect::<Vec<_>>()
                .join(" → ")
        ));
    }
    if !config.run.is_empty() {
        parts.push(format!(
            "Checks: {}",
            config
                .run
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }
    if !config.teardown.is_empty() {
        parts.push(format!(
            "Teardown: {}",
            config
                .teardown
                .iter()
                .take(2)
                .cloned()
                .collect::<Vec<_>>()
                .join(" → ")
        ));
    }

    let hook_groups = [
        (!config.hooks.pre_run.is_empty(), "preRun"),
        (!config.hooks.post_run.is_empty(), "postRun"),
        (!config.hooks.pre_tool.is_empty(), "preTool"),
        (!config.hooks.post_tool.is_empty(), "postTool"),
        (!config.hooks.pre_ship.is_empty(), "preShip"),
        (!config.hooks.post_ship.is_empty(), "postShip"),
    ]
    .into_iter()
    .filter_map(|(enabled, name)| enabled.then_some(name))
    .collect::<Vec<_>>();

    if !hook_groups.is_empty() {
        parts.push(format!("Hooks: {}", hook_groups.join(", ")));
    }

    parts.join(" | ")
}

fn slugify_for_key(value: &str) -> String {
    let slug = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let compact = slug
        .split('-')
        .filter(|segment| !segment.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    if compact.is_empty() {
        "command".to_string()
    } else {
        compact
    }
}
