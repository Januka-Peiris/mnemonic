use crate::models::WorkspaceTaskSnapshot;
use crate::repositories::task_lifecycle_repository;
use crate::state::AppState;

pub fn timestamp() -> String {
    crate::services::terminal_service::timestamp()
}

pub fn start_task_run(
    state: &AppState,
    workspace_id: &str,
    kind: &str,
    source_id: Option<&str>,
) -> Result<String, String> {
    task_lifecycle_repository::start_or_resume_run(
        &state.db,
        workspace_id,
        kind,
        source_id,
        &timestamp(),
    )
}

pub fn append_task_event(
    state: &AppState,
    task_run_id: &str,
    workspace_id: &str,
    event_type: &str,
    payload: serde_json::Value,
) {
    let _ = task_lifecycle_repository::append_event(
        &state.db,
        task_run_id,
        workspace_id,
        &timestamp(),
        event_type,
        &payload,
    );
}

pub fn mark_task_run_completed(
    state: &AppState,
    task_run_id: &str,
    status: &str,
) -> Result<(), String> {
    task_lifecycle_repository::mark_run_status(&state.db, task_run_id, status, Some(&timestamp()))
}

pub fn get_workspace_task_snapshot(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceTaskSnapshot, String> {
    let runs = task_lifecycle_repository::list_runs_for_workspace(&state.db, workspace_id, 20)?;
    let events =
        task_lifecycle_repository::list_events_for_workspace(&state.db, workspace_id, 100)?;
    Ok(WorkspaceTaskSnapshot {
        workspace_id: workspace_id.to_string(),
        runs,
        events,
    })
}
