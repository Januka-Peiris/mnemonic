use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{
    CoordinatorAction, CoordinatorResultArtifact, CoordinatorResultPayload, CoordinatorWorker,
    QueueAgentPromptInput, ReplayWorkspaceCoordinatorActionInput, StartWorkspaceCoordinatorInput,
    StepWorkspaceCoordinatorInput, WorkspaceCoordinatorStatus,
};
use crate::repositories::{
    activity_repository, coordinator_repository, settings_repository, terminal_repository,
    workspace_repository,
};
use crate::services::{
    agent_profile_service, environment_service, task_lifecycle_service, terminal_service,
};
use crate::state::AppState;

const COORDINATOR_STEP_IN_PROGRESS_ERROR: &str =
    "COORDINATOR_STEP_IN_PROGRESS: A coordinator step is already running for this workspace";
const MAX_ACTIONS_PER_STEP: usize = 8;
const MAX_ACTION_PROMPT_CHARS: usize = 12_000;
const MAX_ACTION_MESSAGE_CHARS: usize = 4_000;
const MAX_WORKERS_PER_RUN: usize = 24;

struct CoordinatorStepGuard {
    workspace_id: String,
    registry: crate::state::CoordinatorStepRegistry,
}

impl Drop for CoordinatorStepGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.registry.lock() {
            guard.remove(&self.workspace_id);
        }
    }
}

fn acquire_workspace_step_guard(
    state: &AppState,
    workspace_id: &str,
) -> Result<CoordinatorStepGuard, String> {
    mark_workspace_step_inflight(&state.coordinator_step_inflight, workspace_id)
}

fn mark_workspace_step_inflight(
    registry: &crate::state::CoordinatorStepRegistry,
    workspace_id: &str,
) -> Result<CoordinatorStepGuard, String> {
    let mut inflight = registry
        .lock()
        .map_err(|_| "Coordinator step lock is poisoned".to_string())?;
    if inflight.contains(workspace_id) {
        return Err(COORDINATOR_STEP_IN_PROGRESS_ERROR.to_string());
    }
    inflight.insert(workspace_id.to_string());
    Ok(CoordinatorStepGuard {
        workspace_id: workspace_id.to_string(),
        registry: registry.clone(),
    })
}

fn normalize_prompt_override(value: Option<String>) -> Result<Option<String>, String> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Replay promptOverride must not be empty".to_string());
    }
    if trimmed.chars().count() > MAX_ACTION_PROMPT_CHARS {
        return Err(format!(
            "Replay promptOverride exceeds max length ({MAX_ACTION_PROMPT_CHARS} chars)"
        ));
    }
    Ok(Some(trimmed.to_string()))
}

fn trim_non_empty(input: Option<&str>) -> Option<String> {
    input
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
}

fn normalize_coordinator_provider(input: Option<&str>) -> Option<String> {
    let value = input?.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    if let Some(rest) = value.strip_prefix("builtin-brain-") {
        return normalize_coordinator_provider(Some(rest));
    }
    if let Some(rest) = value.strip_prefix("builtin-coder-") {
        return normalize_coordinator_provider(Some(rest));
    }
    match value.as_str() {
        "claude" | "claude_code" | "claude-code" => Some("claude_code".to_string()),
        "codex" => Some("codex".to_string()),
        "openai" | "openai_api" | "openai-api" => Some("openai".to_string()),
        _ => None,
    }
}

fn provider_label(provider: &str) -> &'static str {
    match provider {
        "claude_code" => "Claude",
        "codex" => "Codex",
        "openai" => "OpenAI",
        _ => "Provider",
    }
}

fn normalize_scale(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "low" | "medium" | "high" => value.trim().to_ascii_lowercase(),
        _ => "medium".to_string(),
    }
}

fn serialize_result_payload(
    payload: CoordinatorResultPayload,
    llm_raw_snippet: Option<&str>,
) -> Option<String> {
    let mut envelope = serde_json::json!({ "result": payload });
    if let Some(snippet) = llm_raw_snippet {
        if !snippet.trim().is_empty() {
            envelope["llmRawSnippet"] = serde_json::Value::String(snippet.to_string());
        }
    }
    serde_json::to_string(&envelope).ok()
}

#[allow(clippy::too_many_arguments)]
fn default_result_for_action(
    run_goal: &str,
    action_kind: &str,
    decision: &str,
    next_action: Option<&str>,
    confidence: &str,
    impact: &str,
    status: &str,
    evidence: Vec<String>,
    risks: Vec<String>,
    artifacts: Vec<CoordinatorResultArtifact>,
) -> CoordinatorResultPayload {
    CoordinatorResultPayload {
        goal: run_goal.to_string(),
        decision: decision.trim().to_string(),
        evidence,
        risks,
        next_action: next_action.map(|value| value.to_string()),
        confidence: normalize_scale(confidence),
        impact: normalize_scale(impact),
        status: if status.trim().is_empty() {
            action_kind.to_string()
        } else {
            status.to_string()
        },
        artifacts,
    }
}

fn planner_result_payload(
    run_goal: &str,
    planner_message: &str,
    planned_actions: &[CoordinatorAction],
    planner_error: Option<&str>,
    llm_raw_snippet: Option<&str>,
) -> Option<String> {
    let mut evidence = vec![
        format!("Planner summary: {planner_message}"),
        format!("Planned actions: {}", planned_actions.len()),
    ];
    if let Some(snippet) = llm_raw_snippet {
        if !snippet.trim().is_empty() {
            evidence.push("LLM raw snippet captured in metadata.".to_string());
        }
    }
    let mut risks = vec![];
    if let Some(error) = planner_error {
        risks.push(error.to_string());
    }
    let next_action = if planned_actions
        .iter()
        .any(|item| matches!(item.action.as_str(), "spawn_worker" | "message_worker"))
    {
        Some("review_diff")
    } else if planned_actions
        .iter()
        .any(|item| item.action.as_str() == "complete")
    {
        Some("create_pr")
    } else {
        Some("review_diff")
    };
    let payload = default_result_for_action(
        run_goal,
        "planner",
        "Planner evaluated coordinator instruction and produced next actions.",
        next_action,
        if planner_error.is_some() {
            "low"
        } else {
            "medium"
        },
        "medium",
        "planned",
        evidence,
        risks,
        vec![],
    );
    serialize_result_payload(payload, llm_raw_snippet)
}

fn notify_result_payload(run_goal: &str, message: &str) -> Option<String> {
    let payload = default_result_for_action(
        run_goal,
        "notify_user",
        if message.trim().is_empty() {
            "Coordinator sent an update for review."
        } else {
            message
        },
        Some("review_diff"),
        "medium",
        "medium",
        "needs_review",
        vec!["Coordinator requested user-facing review.".to_string()],
        vec![],
        vec![],
    );
    serialize_result_payload(payload, None)
}

fn complete_result_payload(
    run_goal: &str,
    message: Option<&str>,
    prompt: Option<&str>,
) -> Option<String> {
    let mut evidence = vec!["Coordinator marked run complete.".to_string()];
    if let Some(value) = prompt.map(str::trim).filter(|value| !value.is_empty()) {
        evidence.push(format!("Completion prompt: {value}"));
    }
    let payload = default_result_for_action(
        run_goal,
        "complete",
        message
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("Coordinator marked this run complete and ready for manual review."),
        Some("review_diff"),
        "medium",
        "high",
        "completed",
        evidence,
        vec![],
        vec![],
    );
    serialize_result_payload(payload, None)
}

fn synthesize_coordinator_profile(
    role: &str,
    provider: &str,
    model: Option<&str>,
    reasoning: Option<&str>,
) -> Result<crate::models::AgentProfile, String> {
    let (agent, command, provider_name, endpoint, local) = match provider {
        "claude_code" => ("claude_code", "claude", Some("claude"), None, false),
        "codex" => ("codex", "codex", Some("openai"), None, false),
        "openai" => (
            "openai",
            "openai",
            Some("openai"),
            Some("https://api.openai.com/v1"),
            false,
        ),
        other => return Err(format!("Unsupported coordinator provider: {other}")),
    };
    let role = role.to_ascii_lowercase();
    let role_label = if role == "coder" { "Coder" } else { "Brain" };
    Ok(crate::models::AgentProfile {
        id: format!("builtin-{}-{}", role, provider.replace('_', "-")),
        label: format!("{} {}", provider_label(provider), role_label),
        agent: agent.to_string(),
        command: command.to_string(),
        args: vec![],
        model: trim_non_empty(model),
        reasoning: trim_non_empty(reasoning),
        mode: Some("act".to_string()),
        provider: provider_name.map(str::to_string),
        endpoint: endpoint.map(str::to_string),
        local,
        description: Some(format!("Built-in coordinator {} provider profile", role)),
        skills: vec![],
        templates: vec![],
        role_preference: Some(role.clone()),
        coordinator_eligible: Some(true),
    })
}

#[allow(clippy::too_many_arguments)]
fn resolve_coordinator_role_profile(
    state: &AppState,
    workspace_id: &str,
    role: &str,
    requested_profile_id: Option<&str>,
    requested_provider: Option<&str>,
    requested_model: Option<&str>,
    requested_reasoning: Option<&str>,
    fallback_profile_id: Option<&str>,
) -> Result<crate::models::AgentProfile, String> {
    let mut profile = if let Some(profile_id) = trim_non_empty(requested_profile_id).as_deref() {
        agent_profile_service::resolve_profile_for_role(
            state,
            Some(workspace_id),
            role,
            Some(profile_id),
        )?
    } else if let Some(provider) = normalize_coordinator_provider(requested_provider) {
        synthesize_coordinator_profile(role, &provider, requested_model, requested_reasoning)?
    } else if let Some(fallback) = trim_non_empty(fallback_profile_id).as_deref() {
        if let Some(provider) = normalize_coordinator_provider(Some(fallback)) {
            synthesize_coordinator_profile(role, &provider, requested_model, requested_reasoning)?
        } else {
            agent_profile_service::resolve_profile_for_role(
                state,
                Some(workspace_id),
                role,
                Some(fallback),
            )?
        }
    } else {
        synthesize_coordinator_profile(role, "claude_code", requested_model, requested_reasoning)?
    };
    if let Some(model) = trim_non_empty(requested_model) {
        profile.model = Some(model);
    }
    if let Some(reasoning) = trim_non_empty(requested_reasoning) {
        profile.reasoning = Some(reasoning);
    }
    Ok(profile)
}

pub fn get_workspace_coordinator_status(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceCoordinatorStatus, String> {
    workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let status = coordinator_repository::workspace_status(&state.db, workspace_id)?;
    reconcile_worker_runtime_status(state, status)
}

pub fn start_workspace_coordinator(
    state: &AppState,
    input: StartWorkspaceCoordinatorInput,
) -> Result<WorkspaceCoordinatorStatus, String> {
    let workspace = workspace_repository::get_detail(&state.db, &input.workspace_id)?
        .ok_or_else(|| format!("Workspace {} was not found", input.workspace_id))?;
    let goal = input.goal.trim();
    if goal.is_empty() {
        return Err("Coordinator goal is required".to_string());
    }
    let brain = resolve_coordinator_role_profile(
        state,
        &input.workspace_id,
        "brain",
        input.brain_profile_id.as_deref(),
        input.brain_provider.as_deref(),
        input.brain_model.as_deref(),
        input.brain_reasoning.as_deref(),
        None,
    )?;
    let coder = resolve_coordinator_role_profile(
        state,
        &input.workspace_id,
        "coder",
        input.coder_profile_id.as_deref(),
        input.coder_provider.as_deref(),
        input.coder_model.as_deref(),
        input.coder_reasoning.as_deref(),
        None,
    )?;
    let run_id = format!("coord-{}-{}", input.workspace_id, unique_suffix());
    coordinator_repository::create_run(
        &state.db,
        &run_id,
        &input.workspace_id,
        &brain.id,
        &coder.id,
        goal,
    )?;
    if let Ok(task_run_id) = task_lifecycle_service::start_task_run(
        state,
        &input.workspace_id,
        "coordinator",
        Some(&run_id),
    ) {
        task_lifecycle_service::append_task_event(
            state,
            &task_run_id,
            &input.workspace_id,
            "coordinator_started",
            serde_json::json!({
                "runId": run_id,
                "brainProfileId": brain.id,
                "coderProfileId": coder.id,
            }),
        );
    }
    let details = format!(
        "Coordinator started in {} with brain={} and coder={}",
        workspace.summary.name, brain.label, coder.label
    );
    let _ = activity_repository::record(
        &state.db,
        &input.workspace_id,
        &workspace.summary.repo,
        Some(&workspace.summary.branch),
        "Coordinator started",
        "info",
        Some(&details),
    );
    get_workspace_coordinator_status(state, &input.workspace_id)
}

pub fn stop_workspace_coordinator(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceCoordinatorStatus, String> {
    let run = coordinator_repository::active_run_for_workspace(&state.db, workspace_id)?;
    if let Some(run) = run {
        coordinator_repository::finish_run(&state.db, &run.id, "stopped")?;
        let task_run_id = format!("task-{workspace_id}-coordinator-{}", run.id);
        task_lifecycle_service::append_task_event(
            state,
            &task_run_id,
            workspace_id,
            "coordinator_stopped",
            serde_json::json!({ "runId": run.id }),
        );
        let _ = task_lifecycle_service::mark_task_run_completed(state, &task_run_id, "stopped");
        let _ = activity_repository::record(
            &state.db,
            workspace_id,
            "",
            None,
            "Coordinator stopped",
            "info",
            Some("Stopped by user"),
        );
    }
    get_workspace_coordinator_status(state, workspace_id)
}

pub fn replay_workspace_coordinator_action(
    state: &AppState,
    input: ReplayWorkspaceCoordinatorActionInput,
) -> Result<WorkspaceCoordinatorStatus, String> {
    let workspace_id = input.workspace_id.as_str();
    let action_id = input.action_id.as_str();
    let prompt_override = normalize_prompt_override(input.prompt_override)?;
    let replay_kind = if prompt_override.is_some() {
        Some("prompt_override")
    } else {
        Some("exact")
    };
    let action = coordinator_repository::get_action_by_id(&state.db, workspace_id, action_id)?
        .ok_or_else(|| format!("Coordinator action not found: {action_id}"))?;

    match action.action_kind.as_str() {
        "planner" => {
            let instruction = prompt_override
                .as_deref()
                .or(action.prompt.as_deref())
                .unwrap_or("Continue coordinator execution");
            if instruction.chars().count() > MAX_ACTION_PROMPT_CHARS {
                return Err(format!(
                    "Replay planner prompt exceeds max length ({MAX_ACTION_PROMPT_CHARS} chars)"
                ));
            }
            step_workspace_coordinator(
                state,
                StepWorkspaceCoordinatorInput {
                    workspace_id: workspace_id.to_string(),
                    instruction: instruction.to_string(),
                    brain_profile_id: None,
                    coder_profile_id: None,
                    brain_provider: None,
                    coder_provider: None,
                    brain_model: None,
                    coder_model: None,
                    brain_reasoning: None,
                    coder_reasoning: None,
                },
            )?;
            coordinator_repository::insert_action_with_metadata(
                &state.db,
                &action.run_id,
                workspace_id,
                "replay_planner",
                replay_kind,
                Some(action_id),
                action.worker_id.as_deref(),
                Some(instruction),
                Some(if prompt_override.is_some() {
                    "Replayed planner action with prompt override"
                } else {
                    "Replayed planner action"
                }),
                None,
            )?;
        }
        "spawn_worker" | "message_worker" => {
            let prompt = prompt_override
                .as_deref()
                .or(action.prompt.as_deref())
                .ok_or_else(|| format!("Action {action_id} has no prompt to replay"))?
                .trim()
                .to_string();
            if prompt.is_empty() {
                return Err(format!("Action {action_id} has empty prompt"));
            }
            if prompt.chars().count() > MAX_ACTION_PROMPT_CHARS {
                return Err(format!(
                    "Replay prompt exceeds max length ({MAX_ACTION_PROMPT_CHARS} chars)"
                ));
            }
            let profile_id = if let Some(worker_id) = action.worker_id.as_deref() {
                if let Some(worker) =
                    coordinator_repository::get_worker_by_id(&state.db, worker_id)?
                {
                    worker.profile_id
                } else {
                    agent_profile_service::resolve_profile_for_role(
                        state,
                        Some(workspace_id),
                        "coder",
                        None,
                    )?
                    .id
                }
            } else {
                agent_profile_service::resolve_profile_for_role(
                    state,
                    Some(workspace_id),
                    "coder",
                    None,
                )?
                .id
            };
            let queued = terminal_service::queue_workspace_agent_prompt(
                state,
                QueueAgentPromptInput {
                    workspace_id: workspace_id.to_string(),
                    session_id: None,
                    prompt: prompt.clone(),
                    profile: None,
                    profile_id: Some(profile_id.clone()),
                    extra_args: None,
                },
            )?;
            coordinator_repository::insert_action_with_metadata(
                &state.db,
                &action.run_id,
                workspace_id,
                "replay_worker_prompt",
                replay_kind,
                Some(action_id),
                action.worker_id.as_deref(),
                Some(&prompt),
                Some(&format!(
                    "Replayed {} using profile {} (session {})",
                    action.action_kind,
                    profile_id,
                    queued.session_id.as_deref().unwrap_or("unknown")
                )),
                None,
            )?;
        }
        "stop_worker" => {
            if let Some(worker_id) = action.worker_id.as_deref() {
                if let Some(worker) =
                    coordinator_repository::get_worker_by_id(&state.db, worker_id)?
                {
                    if let Some(session_id) = worker.last_session_id.as_deref() {
                        let _ = terminal_service::stop_workspace_terminal_session_by_id(
                            state, session_id,
                        );
                    }
                }
            }
            coordinator_repository::insert_action_with_metadata(
                &state.db,
                &action.run_id,
                workspace_id,
                "replay_stop_worker",
                replay_kind,
                Some(action_id),
                action.worker_id.as_deref(),
                None,
                Some("Replayed stop_worker"),
                None,
            )?;
        }
        other => {
            return Err(format!("Replay is not supported for action kind: {other}"));
        }
    }

    get_workspace_coordinator_status(state, workspace_id)
}

pub fn reconcile_all_active_runs_on_startup(state: &AppState) -> Result<(), String> {
    let runs = coordinator_repository::list_active_runs(&state.db)?;
    let mut workspaces = std::collections::BTreeSet::new();
    for run in runs {
        workspaces.insert(run.workspace_id);
    }
    for workspace_id in workspaces {
        let _ = get_workspace_coordinator_status(state, &workspace_id)?;
    }
    Ok(())
}

pub fn step_workspace_coordinator(
    state: &AppState,
    input: StepWorkspaceCoordinatorInput,
) -> Result<WorkspaceCoordinatorStatus, String> {
    let _step_guard = acquire_workspace_step_guard(state, &input.workspace_id)?;
    let instruction = input.instruction.trim();
    if instruction.is_empty() {
        return Err("Coordinator instruction is required".to_string());
    }
    let mut run = coordinator_repository::active_run_for_workspace(&state.db, &input.workspace_id)?;
    if run.is_none() {
        start_workspace_coordinator(
            state,
            StartWorkspaceCoordinatorInput {
                workspace_id: input.workspace_id.clone(),
                goal: instruction.to_string(),
                brain_profile_id: input.brain_profile_id.clone(),
                coder_profile_id: input.coder_profile_id.clone(),
                brain_provider: input.brain_provider.clone(),
                coder_provider: input.coder_provider.clone(),
                brain_model: input.brain_model.clone(),
                coder_model: input.coder_model.clone(),
                brain_reasoning: input.brain_reasoning.clone(),
                coder_reasoning: input.coder_reasoning.clone(),
            },
        )?;
        run = coordinator_repository::active_run_for_workspace(&state.db, &input.workspace_id)?;
    }
    let mut run = run.ok_or_else(|| "Failed to initialize coordinator run".to_string())?;
    let brain = resolve_coordinator_role_profile(
        state,
        &input.workspace_id,
        "brain",
        input.brain_profile_id.as_deref(),
        input.brain_provider.as_deref(),
        input.brain_model.as_deref(),
        input.brain_reasoning.as_deref(),
        Some(&run.brain_profile_id),
    )?;
    let coder = resolve_coordinator_role_profile(
        state,
        &input.workspace_id,
        "coder",
        input.coder_profile_id.as_deref(),
        input.coder_provider.as_deref(),
        input.coder_model.as_deref(),
        input.coder_reasoning.as_deref(),
        Some(&run.coder_profile_id),
    )?;
    if run.brain_profile_id != brain.id || run.coder_profile_id != coder.id {
        coordinator_repository::update_run_profiles(&state.db, &run.id, &brain.id, &coder.id)?;
        run.brain_profile_id = brain.id.clone();
        run.coder_profile_id = coder.id.clone();
    }
    let mut workers = coordinator_repository::list_workers_for_run(&state.db, &run.id)?;
    let plan = plan_actions(state, &run, &brain, &workers, instruction);
    let actions = plan.actions.clone();
    let raw_response = plan.raw_response.clone();
    let planner_error = plan.planner_error.clone();
    let planner_message = format!(
        "adapter={} parse={} fallback={}",
        plan.adapter,
        plan.parse_mode,
        if planner_error.is_some() { "yes" } else { "no" }
    );
    let raw_snippet = raw_response
        .as_deref()
        .map(|value| value.chars().take(4000).collect::<String>());
    let planner_result_json = planner_result_payload(
        &run.goal,
        &planner_message,
        &actions,
        planner_error.as_deref(),
        raw_snippet.as_deref(),
    );
    coordinator_repository::insert_action(
        &state.db,
        coordinator_repository::CoordinatorActionInsert {
            run_id: &run.id,
            workspace_id: &input.workspace_id,
            action_kind: "planner",
            worker_id: None,
            prompt: None,
            message: Some(&planner_message),
            raw_json: planner_result_json.as_deref(),
        },
    )?;
    let actions = match validate_actions(&workers, actions) {
        Ok(actions) => actions,
        Err(validation_error) => {
            coordinator_repository::insert_action(
                &state.db,
                coordinator_repository::CoordinatorActionInsert {
                    run_id: &run.id,
                    workspace_id: &input.workspace_id,
                    action_kind: "validation_error",
                    worker_id: None,
                    prompt: None,
                    message: Some(&validation_error),
                    raw_json: None,
                },
            )?;
            coordinator_repository::mark_run_result(
                &state.db,
                &run.id,
                raw_response.as_deref(),
                Some(&validation_error),
            )?;
            return get_workspace_coordinator_status(state, &input.workspace_id);
        }
    };
    if actions.is_empty() {
        coordinator_repository::mark_run_result(
            &state.db,
            &run.id,
            raw_response.as_deref(),
            planner_error.as_deref(),
        )?;
        return get_workspace_coordinator_status(state, &input.workspace_id);
    }

    for action in &actions {
        match action.action.as_str() {
            "spawn_worker" => {
                let worker_id = action
                    .worker_id
                    .clone()
                    .unwrap_or_else(|| format!("worker-{}", unique_suffix()));
                let prompt = action
                    .prompt
                    .as_deref()
                    .unwrap_or(instruction)
                    .trim()
                    .to_string();
                let queued = terminal_service::queue_workspace_agent_prompt(
                    state,
                    QueueAgentPromptInput {
                        workspace_id: input.workspace_id.clone(),
                        session_id: None,
                        prompt: prompt.clone(),
                        profile: None,
                        profile_id: Some(coder.id.clone()),
                        extra_args: None,
                    },
                )?;
                let worker = CoordinatorWorker {
                    id: worker_id.clone(),
                    run_id: run.id.clone(),
                    workspace_id: input.workspace_id.clone(),
                    profile_id: coder.id.clone(),
                    status: "running".to_string(),
                    last_prompt: Some(prompt.clone()),
                    last_session_id: queued.session_id.clone(),
                    notified_status: None,
                    created_at: now_string(),
                    updated_at: now_string(),
                };
                coordinator_repository::upsert_worker(&state.db, &worker)?;
                workers.push(worker);
                coordinator_repository::insert_action(
                    &state.db,
                    coordinator_repository::CoordinatorActionInsert {
                        run_id: &run.id,
                        workspace_id: &input.workspace_id,
                        action_kind: "spawn_worker",
                        worker_id: Some(&worker_id),
                        prompt: Some(&prompt),
                        message: None,
                        raw_json: None,
                    },
                )?;
            }
            "message_worker" => {
                let chosen = resolve_worker_for_action(&workers, action.worker_id.as_deref())?;
                let prompt = action
                    .prompt
                    .as_deref()
                    .unwrap_or(instruction)
                    .trim()
                    .to_string();
                let queued = terminal_service::queue_workspace_agent_prompt(
                    state,
                    QueueAgentPromptInput {
                        workspace_id: input.workspace_id.clone(),
                        session_id: None,
                        prompt: prompt.clone(),
                        profile: None,
                        profile_id: Some(chosen.profile_id.clone()),
                        extra_args: None,
                    },
                )?;
                let mut worker = chosen.clone();
                worker.status = "running".to_string();
                worker.last_prompt = Some(prompt.clone());
                worker.last_session_id = queued.session_id.clone();
                worker.notified_status = None;
                worker.updated_at = now_string();
                coordinator_repository::upsert_worker(&state.db, &worker)?;
                coordinator_repository::insert_action(
                    &state.db,
                    coordinator_repository::CoordinatorActionInsert {
                        run_id: &run.id,
                        workspace_id: &input.workspace_id,
                        action_kind: "message_worker",
                        worker_id: Some(&worker.id),
                        prompt: Some(&prompt),
                        message: None,
                        raw_json: None,
                    },
                )?;
            }
            "stop_worker" => {
                let chosen = resolve_worker_for_action(&workers, action.worker_id.as_deref())?;
                if let Some(session_id) = chosen.last_session_id.as_deref() {
                    let _ =
                        terminal_service::stop_workspace_terminal_session_by_id(state, session_id);
                }
                let mut worker = chosen.clone();
                worker.status = "stopped".to_string();
                worker.notified_status = Some("stopped".to_string());
                worker.updated_at = now_string();
                coordinator_repository::upsert_worker(&state.db, &worker)?;
                coordinator_repository::insert_action(
                    &state.db,
                    coordinator_repository::CoordinatorActionInsert {
                        run_id: &run.id,
                        workspace_id: &input.workspace_id,
                        action_kind: "stop_worker",
                        worker_id: Some(&worker.id),
                        prompt: None,
                        message: None,
                        raw_json: None,
                    },
                )?;
            }
            "notify_user" => {
                if let Some(message) = action.message.as_deref() {
                    state.event_emitter.emit(
                        crate::events::COORDINATOR_NOTIFY,
                        &serde_json::json!({
                            "workspaceId": input.workspace_id,
                            "message": message,
                        }),
                    );
                    coordinator_repository::insert_action(
                        &state.db,
                        coordinator_repository::CoordinatorActionInsert {
                            run_id: &run.id,
                            workspace_id: &input.workspace_id,
                            action_kind: "notify_user",
                            worker_id: None,
                            prompt: None,
                            message: Some(message),
                            raw_json: notify_result_payload(&run.goal, message).as_deref(),
                        },
                    )?;
                }
            }
            "complete" => {
                coordinator_repository::finish_run(&state.db, &run.id, "completed")?;
                let task_run_id = format!("task-{}-coordinator-{}", input.workspace_id, run.id);
                task_lifecycle_service::append_task_event(
                    state,
                    &task_run_id,
                    &input.workspace_id,
                    "coordinator_completed",
                    serde_json::json!({ "runId": run.id }),
                );
                let _ = task_lifecycle_service::mark_task_run_completed(
                    state,
                    &task_run_id,
                    "completed",
                );
                coordinator_repository::insert_action(
                    &state.db,
                    coordinator_repository::CoordinatorActionInsert {
                        run_id: &run.id,
                        workspace_id: &input.workspace_id,
                        action_kind: "complete",
                        worker_id: action.worker_id.as_deref(),
                        prompt: action.prompt.as_deref(),
                        message: action.message.as_deref(),
                        raw_json: complete_result_payload(
                            &run.goal,
                            action.message.as_deref(),
                            action.prompt.as_deref(),
                        )
                        .as_deref(),
                    },
                )?;
            }
            _ => {}
        }
    }

    coordinator_repository::mark_run_result(
        &state.db,
        &run.id,
        raw_response.as_deref(),
        planner_error.as_deref(),
    )?;
    get_workspace_coordinator_status(state, &input.workspace_id)
}

fn reconcile_worker_runtime_status(
    state: &AppState,
    mut status: WorkspaceCoordinatorStatus,
) -> Result<WorkspaceCoordinatorStatus, String> {
    let Some(active_run) = status.active_run.clone() else {
        return Ok(status);
    };

    let mut changed = false;
    for worker in status.workers.clone() {
        if worker.status != "running" {
            continue;
        }
        let Some(session_id) = worker.last_session_id.as_deref() else {
            continue;
        };
        let Some(session) = terminal_repository::get_session(&state.db, session_id)? else {
            continue;
        };
        let Some(runtime_update) = derive_runtime_worker_update(&worker, &session.status) else {
            continue;
        };

        let mut next = worker.clone();
        next.status = runtime_update.next_status.clone();
        next.updated_at = now_string();
        if runtime_update.should_notify {
            let message = format!("Worker {} {}", worker.id, runtime_update.next_status);
            state.event_emitter.emit(
                crate::events::COORDINATOR_NOTIFY,
                &serde_json::json!({
                    "workspaceId": worker.workspace_id,
                    "message": message,
                }),
            );
            coordinator_repository::insert_action(
                &state.db,
                coordinator_repository::CoordinatorActionInsert {
                    run_id: &active_run.id,
                    workspace_id: &worker.workspace_id,
                    action_kind: "worker_update",
                    worker_id: Some(&worker.id),
                    prompt: None,
                    message: Some(&format!(
                        "worker={} status={} session={}",
                        worker.id, runtime_update.next_status, session.id
                    )),
                    raw_json: None,
                },
            )?;
            next.notified_status = Some(runtime_update.next_status);
        }
        coordinator_repository::upsert_worker(&state.db, &next)?;
        changed = true;
    }

    if changed {
        status = coordinator_repository::workspace_status(&state.db, &status.workspace_id)?;
    }
    Ok(status)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeWorkerUpdate {
    next_status: String,
    should_notify: bool,
}

fn derive_runtime_worker_update(
    worker: &CoordinatorWorker,
    session_status: &str,
) -> Option<RuntimeWorkerUpdate> {
    if worker.status != "running" || session_status == "running" {
        return None;
    }
    Some(RuntimeWorkerUpdate {
        next_status: session_status.to_string(),
        should_notify: worker.notified_status.as_deref() != Some(session_status),
    })
}

fn resolve_worker_for_action<'a>(
    workers: &'a [CoordinatorWorker],
    worker_id: Option<&str>,
) -> Result<&'a CoordinatorWorker, String> {
    if let Some(worker_id) = worker_id {
        return workers
            .iter()
            .find(|worker| worker.id == worker_id)
            .ok_or_else(|| format!("Unknown coordinator worker: {worker_id}"));
    }
    workers
        .iter()
        .rev()
        .find(|worker| worker.status == "running")
        .or_else(|| workers.last())
        .ok_or_else(|| "No coordinator worker exists yet".to_string())
}

fn validate_actions(
    workers: &[CoordinatorWorker],
    actions: Vec<CoordinatorAction>,
) -> Result<Vec<CoordinatorAction>, String> {
    if actions.len() > MAX_ACTIONS_PER_STEP {
        return Err("Coordinator returned too many actions in one step".to_string());
    }
    if actions.len() > 1 && actions.iter().any(|action| action.action == "complete") {
        return Err("complete must be the only coordinator action in a step".to_string());
    }
    let mut out = Vec::with_capacity(actions.len());
    let existing_worker_count = workers.len();
    let mut additional_workers = 0usize;
    for action in actions {
        match action.action.as_str() {
            "spawn_worker" => {
                let prompt = action
                    .prompt
                    .as_deref()
                    .map(str::trim)
                    .ok_or_else(|| "spawn_worker requires non-empty prompt".to_string())?;
                if prompt.is_empty() {
                    return Err("spawn_worker requires non-empty prompt".to_string());
                }
                if prompt.chars().count() > MAX_ACTION_PROMPT_CHARS {
                    return Err(format!(
                        "spawn_worker prompt exceeds max length ({MAX_ACTION_PROMPT_CHARS} chars)"
                    ));
                }
                additional_workers = additional_workers.saturating_add(1);
            }
            "message_worker" => {
                let prompt = action
                    .prompt
                    .as_deref()
                    .map(str::trim)
                    .ok_or_else(|| "message_worker requires non-empty prompt".to_string())?;
                if prompt.is_empty() {
                    return Err("message_worker requires non-empty prompt".to_string());
                }
                if prompt.chars().count() > MAX_ACTION_PROMPT_CHARS {
                    return Err(format!(
                        "message_worker prompt exceeds max length ({MAX_ACTION_PROMPT_CHARS} chars)"
                    ));
                }
            }
            "stop_worker" => {}
            "notify_user" => {
                let message = action
                    .message
                    .as_deref()
                    .map(str::trim)
                    .ok_or_else(|| "notify_user requires message".to_string())?;
                if message.is_empty() {
                    return Err("notify_user requires message".to_string());
                }
                if message.chars().count() > MAX_ACTION_MESSAGE_CHARS {
                    return Err(format!(
                        "notify_user message exceeds max length ({MAX_ACTION_MESSAGE_CHARS} chars)"
                    ));
                }
            }
            "complete" => {
                if let Some(message) = action.message.as_deref() {
                    if message.trim().is_empty() {
                        return Err("complete message must not be empty when provided".to_string());
                    }
                    if message.chars().count() > MAX_ACTION_MESSAGE_CHARS {
                        return Err(format!(
                            "complete message exceeds max length ({MAX_ACTION_MESSAGE_CHARS} chars)"
                        ));
                    }
                }
            }
            other => return Err(format!("Unsupported coordinator action: {other}")),
        }
        out.push(action);
    }
    if existing_worker_count.saturating_add(additional_workers) > MAX_WORKERS_PER_RUN {
        return Err(format!(
            "Coordinator worker limit exceeded (max {MAX_WORKERS_PER_RUN} per run)"
        ));
    }
    Ok(out)
}

fn plan_actions(
    state: &AppState,
    run: &crate::models::CoordinatorRun,
    brain_profile: &crate::models::AgentProfile,
    workers: &[CoordinatorWorker],
    instruction: &str,
) -> PlanResult {
    let prompt = format!(
        "You are Mnemonic workspace coordinator.\n\
         Return a JSON array only (no markdown) using action objects:\n\
         [{{\"action\":\"spawn_worker\",\"prompt\":\"...\"}},\
         {{\"action\":\"message_worker\",\"workerId\":\"...\",\"prompt\":\"...\"}},\
         {{\"action\":\"stop_worker\",\"workerId\":\"...\"}},\
         {{\"action\":\"notify_user\",\"message\":\"...\"}},\
         {{\"action\":\"complete\",\"message\":\"...\"}}]\n\
         Keep actions short and safe.\n\
         Workspace goal: {}\n\
         Worker count: {}\n\
         Current instruction: {}",
        run.goal,
        workers.len(),
        instruction
    );
    let workspace_cwd = resolve_workspace_cwd(state, &run.workspace_id);

    let adapter = brain_profile.agent.clone();
    let provider_result = match brain_profile.agent.as_str() {
        "claude_code" => call_claude_brain(state, brain_profile, workspace_cwd.as_deref(), &prompt),
        "codex" => call_codex_brain(state, brain_profile, workspace_cwd.as_deref(), &prompt),
        "openai" => call_openai_brain(state, brain_profile, &prompt),
        _ => Err(format!(
            "No direct coordinator adapter for brain profile provider: {}",
            brain_profile.agent
        )),
    };
    if let Ok(response) = provider_result {
        if let Ok((actions, parse_mode)) = extract_actions_from_response(&response) {
            return PlanResult {
                actions,
                raw_response: Some(response),
                planner_error: None,
                adapter,
                parse_mode,
            };
        }
        return PlanResult {
            actions: heuristic_actions(workers, instruction),
            raw_response: Some(response),
            planner_error: Some(
                "Brain response was not parseable as coordinator action JSON; used local fallback"
                    .to_string(),
            ),
            adapter,
            parse_mode: "fallback_heuristic".to_string(),
        };
    }

    PlanResult {
        actions: heuristic_actions(workers, instruction),
        raw_response: None,
        planner_error: Some(
            "Selected brain profile adapter failed or is unavailable; used local planner fallback"
                .to_string(),
        ),
        adapter: adapter_if_available(brain_profile),
        parse_mode: "fallback_heuristic".to_string(),
    }
}

fn heuristic_actions(workers: &[CoordinatorWorker], instruction: &str) -> Vec<CoordinatorAction> {
    if workers.is_empty() {
        return vec![CoordinatorAction {
            action: "spawn_worker".to_string(),
            worker_id: None,
            prompt: Some(instruction.trim().to_string()),
            message: None,
        }];
    }
    let worker = workers
        .iter()
        .rev()
        .find(|worker| worker.status == "running")
        .or_else(|| workers.last());
    vec![CoordinatorAction {
        action: "message_worker".to_string(),
        worker_id: worker.map(|worker| worker.id.clone()),
        prompt: Some(instruction.trim().to_string()),
        message: None,
    }]
}

fn call_claude_brain(
    state: &AppState,
    profile: &crate::models::AgentProfile,
    cwd: Option<&str>,
    prompt: &str,
) -> Result<String, String> {
    let model = profile_default_model(state, profile);
    let command_path = resolve_profile_command(&profile.command)?;
    let mut command = std::process::Command::new(command_path);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if let Some(model) = model.as_deref().filter(|value| !value.trim().is_empty()) {
        command.args(["--model", model.trim()]);
    }
    let output = command
        .args(["-p", prompt])
        .output()
        .map_err(|err| format!("Failed to run claude CLI for coordinator: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("claude CLI failed for coordinator: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn call_codex_brain(
    state: &AppState,
    profile: &crate::models::AgentProfile,
    cwd: Option<&str>,
    prompt: &str,
) -> Result<String, String> {
    let command_path = resolve_profile_command(&profile.command)?;
    let mut command = std::process::Command::new(command_path);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.args(["exec", "--json"]);
    if let Some(model) =
        profile_default_model(state, profile).filter(|value| !value.trim().is_empty())
    {
        command.args(["-c", &format!("model=\"{}\"", model.replace('"', "\\\""))]);
    }
    if let Some(reasoning) =
        normalize_codex_reasoning(profile.reasoning.as_deref().unwrap_or("medium"))
    {
        command.args(["-c", &format!("model_reasoning_effort=\"{reasoning}\"")]);
    }
    let output = command
        .args(if let Some(cwd) = cwd {
            vec!["--cd", cwd]
        } else {
            vec![]
        })
        .arg(prompt)
        .output()
        .map_err(|err| format!("Failed to run codex CLI for coordinator: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("codex CLI failed for coordinator: {stderr}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(extract_text_from_jsonl_stdout(&stdout).unwrap_or(stdout))
}

fn resolve_workspace_cwd(state: &AppState, workspace_id: &str) -> Option<String> {
    workspace_repository::get_detail(&state.db, workspace_id)
        .ok()
        .flatten()
        .map(|workspace| {
            workspace
                .summary
                .workspace_root_path
                .unwrap_or(workspace.worktree_path)
        })
}

fn call_openai_brain(
    state: &AppState,
    profile: &crate::models::AgentProfile,
    prompt: &str,
) -> Result<String, String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY environment variable not set".to_string())?;
    let model = profile_default_model(state, profile)
        .or_else(|| profile.model.clone())
        .unwrap_or_else(|| "gpt-5.4".to_string());
    let base_url = profile
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("https://api.openai.com/v1");
    let url = if base_url.ends_with("/chat/completions") {
        base_url.to_string()
    } else {
        format!("{}/chat/completions", base_url.trim_end_matches('/'))
    };
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}]
    });

    let response = reqwest::blocking::Client::new()
        .post(url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .and_then(|res| res.error_for_status())
        .map_err(|err| format!("OpenAI coordinator request failed: {err}"))?;

    let json: serde_json::Value = response
        .json()
        .map_err(|err| format!("Failed to parse OpenAI coordinator response JSON: {err}"))?;
    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|value| value.to_string())
        .ok_or_else(|| "OpenAI coordinator response did not include message content".to_string())
}

fn resolve_profile_command(command: &str) -> Result<String, String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err("Brain profile command is empty".to_string());
    }
    if trimmed.contains('/') {
        return Ok(trimmed.to_string());
    }
    environment_service::find_binary(trimmed)
        .map_err(|err| format!("Failed to resolve brain command {trimmed}: {err}"))?
        .map(|path| path.display().to_string())
        .ok_or_else(|| format!("Brain command not found on PATH: {trimmed}"))
}

fn profile_default_model(
    state: &AppState,
    profile: &crate::models::AgentProfile,
) -> Option<String> {
    if let Some(model) = profile
        .model
        .clone()
        .filter(|value| !value.trim().is_empty())
    {
        return Some(model);
    }
    match profile.agent.as_str() {
        "claude_code" => settings_repository::get_value(&state.db, "claude_agent_default_model")
            .ok()
            .flatten()
            .or_else(|| {
                settings_repository::get_value(&state.db, "agent_default_model")
                    .ok()
                    .flatten()
            }),
        "codex" => settings_repository::get_value(&state.db, "codex_agent_default_model")
            .ok()
            .flatten(),
        "openai" => settings_repository::get_value(&state.db, "openai_agent_default_model")
            .ok()
            .flatten(),
        _ => None,
    }
}

fn normalize_codex_reasoning(input: &str) -> Option<&'static str> {
    match input.trim().to_ascii_lowercase().as_str() {
        "low" => Some("low"),
        "medium" | "default" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "extra high" | "extra_high" | "max" => Some("xhigh"),
        _ => None,
    }
}

fn extract_actions_from_response(
    response: &str,
) -> Result<(Vec<CoordinatorAction>, String), String> {
    if let Ok(actions) = serde_json::from_str::<Vec<CoordinatorAction>>(response.trim()) {
        return Ok((actions, "direct_json".to_string()));
    }

    if let Some(fenced) = extract_fenced_json_block(response) {
        if let Ok(actions) = serde_json::from_str::<Vec<CoordinatorAction>>(fenced.trim()) {
            return Ok((actions, "fenced_json".to_string()));
        }
    }

    if let Some(array_slice) = extract_first_json_array_slice(response) {
        if let Ok(actions) = serde_json::from_str::<Vec<CoordinatorAction>>(array_slice.trim()) {
            return Ok((actions, "embedded_array".to_string()));
        }
    }

    Err("No parseable coordinator action array found".to_string())
}

fn adapter_if_available(profile: &crate::models::AgentProfile) -> String {
    profile.agent.clone()
}

fn extract_fenced_json_block(input: &str) -> Option<&str> {
    let start = input.find("```json").or_else(|| input.find("```"))?;
    let rest = &input[start..];
    let first_newline = rest.find('\n')?;
    let payload = &rest[first_newline + 1..];
    let end = payload.find("```")?;
    Some(&payload[..end])
}

fn extract_first_json_array_slice(input: &str) -> Option<&str> {
    let start = input.find('[')?;
    let mut depth = 0usize;
    let mut end_idx = None;
    for (idx, ch) in input[start..].char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    end_idx = Some(start + idx + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    end_idx.map(|end| &input[start..end])
}

fn extract_text_from_jsonl_stdout(stdout: &str) -> Option<String> {
    let mut chunks = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(text) = value.get("message").and_then(|v| v.as_str()) {
            chunks.push(text.to_string());
            continue;
        }
        if let Some(text) = value.get("content").and_then(|v| v.as_str()) {
            chunks.push(text.to_string());
            continue;
        }
        if let Some(text) = value
            .get("message")
            .and_then(|v| v.get("content"))
            .and_then(|v| v.as_str())
        {
            chunks.push(text.to_string());
            continue;
        }
        if let Some(text) = value
            .get("delta")
            .and_then(|v| v.get("text"))
            .and_then(|v| v.as_str())
        {
            chunks.push(text.to_string());
            continue;
        }
    }
    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join("\n"))
    }
}

fn unique_suffix() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
        .to_string()
}

fn now_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

struct PlanResult {
    actions: Vec<CoordinatorAction>,
    raw_response: Option<String>,
    planner_error: Option<String>,
    adapter: String,
    parse_mode: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(action: &str, prompt: Option<&str>, message: Option<&str>) -> CoordinatorAction {
        CoordinatorAction {
            action: action.to_string(),
            worker_id: None,
            prompt: prompt.map(ToOwned::to_owned),
            message: message.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn parses_direct_json_action_array() {
        let input = r#"[{"action":"notify_user","message":"ok"}]"#;
        let (actions, parse_mode) = extract_actions_from_response(input).expect("actions");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action, "notify_user");
        assert_eq!(parse_mode, "direct_json");
    }

    #[test]
    fn parses_fenced_json_action_array() {
        let input = "Here:\n```json\n[{\"action\":\"complete\",\"message\":\"done\"}]\n```";
        let (actions, parse_mode) = extract_actions_from_response(input).expect("actions");
        assert_eq!(actions[0].action, "complete");
        assert_eq!(parse_mode, "fenced_json");
    }

    #[test]
    fn parses_jsonl_message_text() {
        let stdout =
            r#"{"type":"message","message":"[{\"action\":\"notify_user\",\"message\":\"hi\"}]"}"#;
        let text = extract_text_from_jsonl_stdout(stdout).expect("text");
        assert!(text.contains("notify_user"));
    }

    #[test]
    fn rejects_action_limits_and_empty_prompts() {
        let workers = Vec::<CoordinatorWorker>::new();
        let mut too_many = Vec::new();
        for _ in 0..(MAX_ACTIONS_PER_STEP + 1) {
            too_many.push(action("notify_user", None, Some("ok")));
        }
        let err = validate_actions(&workers, too_many).expect_err("should reject max actions");
        assert!(err.contains("too many actions"));

        let err = validate_actions(&workers, vec![action("spawn_worker", Some("   "), None)])
            .expect_err("should reject blank prompt");
        assert!(err.contains("non-empty prompt"));
    }

    #[test]
    fn rejects_complete_mixed_with_other_actions() {
        let workers = Vec::<CoordinatorWorker>::new();
        let err = validate_actions(
            &workers,
            vec![
                action("complete", None, Some("done")),
                action("spawn_worker", Some("do more"), None),
            ],
        )
        .expect_err("complete should be exclusive");
        assert!(err.contains("complete must be the only"));
    }

    #[test]
    fn enforces_worker_limit() {
        let workers = (0..MAX_WORKERS_PER_RUN)
            .map(|index| CoordinatorWorker {
                id: format!("w-{index}"),
                run_id: "run".to_string(),
                workspace_id: "ws".to_string(),
                profile_id: "coder".to_string(),
                status: "running".to_string(),
                last_prompt: None,
                last_session_id: None,
                notified_status: None,
                created_at: "0".to_string(),
                updated_at: "0".to_string(),
            })
            .collect::<Vec<_>>();
        let err = validate_actions(&workers, vec![action("spawn_worker", Some("do"), None)])
            .expect_err("should reject worker overflow");
        assert!(err.contains("worker limit exceeded"));
    }

    #[test]
    fn replay_prompt_override_validation_rejects_blank() {
        let err = normalize_prompt_override(Some(" \n\t".to_string())).expect_err("blank override");
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn runtime_update_dedupes_notified_status() {
        let worker = CoordinatorWorker {
            id: "w1".to_string(),
            run_id: "run".to_string(),
            workspace_id: "ws".to_string(),
            profile_id: "coder".to_string(),
            status: "running".to_string(),
            last_prompt: None,
            last_session_id: Some("session".to_string()),
            notified_status: None,
            created_at: "0".to_string(),
            updated_at: "0".to_string(),
        };
        let first = derive_runtime_worker_update(&worker, "succeeded").expect("first update");
        assert!(first.should_notify);
        assert_eq!(first.next_status, "succeeded");

        let mut updated = worker.clone();
        updated.status = "succeeded".to_string();
        updated.notified_status = Some("succeeded".to_string());
        let second = derive_runtime_worker_update(&updated, "succeeded");
        assert!(
            second.is_none(),
            "repeated reconciliation should be idempotent"
        );
    }

    #[test]
    fn single_flight_guard_blocks_parallel_workspace_steps() {
        let registry: crate::state::CoordinatorStepRegistry =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let guard = mark_workspace_step_inflight(&registry, "ws").expect("first lock");
        let err = match mark_workspace_step_inflight(&registry, "ws") {
            Ok(_) => panic!("should block second lock"),
            Err(err) => err,
        };
        assert!(err.starts_with("COORDINATOR_STEP_IN_PROGRESS"));
        drop(guard);
        mark_workspace_step_inflight(&registry, "ws").expect("lock released after drop");
    }
}
