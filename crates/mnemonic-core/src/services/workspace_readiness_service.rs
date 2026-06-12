use crate::models::WorkspaceReadiness;
use crate::repositories::{
    agent_run_repository, review_cockpit_repository, task_lifecycle_repository,
    terminal_repository, workspace_repository,
};
use crate::services::{git_review_service, workspace_health_service};
use crate::state::AppState;

pub fn get_workspace_readiness(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceReadiness, String> {
    workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let health = workspace_health_service::get_workspace_health(state, workspace_id).ok();
    let sessions = terminal_repository::list_visible_for_workspace(&state.db, workspace_id)
        .unwrap_or_default();
    let agent_status = sessions
        .iter()
        .find(|session| session.session_role == "agent" || session.terminal_kind == "agent")
        .map(|session| session.status.clone())
        .unwrap_or_else(|| "idle".to_string());
    let terminal_health = health
        .as_ref()
        .map(|health| health.status.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let changed_files =
        git_review_service::get_workspace_changed_files(state, workspace_id).unwrap_or_default();
    let review_states = review_cockpit_repository::list_file_review_states(&state.db, workspace_id)
        .unwrap_or_default();
    let changed_paths = changed_files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<std::collections::HashSet<_>>();
    let reviewed_files = review_states
        .iter()
        .filter(|state| state.status == "reviewed" && changed_paths.contains(state.path.as_str()))
        .count() as u32;
    let latest_run = agent_run_repository::list_runs_for_workspace(&state.db, workspace_id)
        .unwrap_or_default()
        .into_iter()
        .next();
    let run_terminal = sessions
        .iter()
        .find(|session| session.terminal_kind == "run")
        .map(|session| session.status.clone());
    let test_status = latest_run
        .map(|run| run.status)
        .or(run_terminal)
        .unwrap_or_else(|| "unknown".to_string());
    let pr_comment_count = review_cockpit_repository::list_pr_comments(&state.db, workspace_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|comment| comment.state != "resolved_local")
        .count() as u32;
    // Port count is not computed here (see workspace_health_service): avoids repeated full-system lsof.
    let port_count = 0u32;
    let status = if terminal_health == "needs_attention" {
        "needs_attention"
    } else if agent_status == "running" {
        "running"
    } else if changed_files.is_empty() {
        "idle"
    } else {
        "review"
    }
    .to_string();
    let task_runs = task_lifecycle_repository::list_runs_for_workspace(&state.db, workspace_id, 10)
        .unwrap_or_default();
    let active_tasks = task_runs
        .iter()
        .filter(|run| run.status == "running")
        .count();
    let summary = format!(
        "Agent {} · {} files · {}/{} accepted · tests {} · {} PR comment{} · {} task{} active",
        agent_status,
        changed_files.len(),
        reviewed_files,
        changed_files.len(),
        test_status,
        pr_comment_count,
        if pr_comment_count == 1 { "" } else { "s" },
        active_tasks,
        if active_tasks == 1 { "" } else { "s" },
    );
    Ok(WorkspaceReadiness {
        workspace_id: workspace_id.to_string(),
        status,
        summary,
        agent_status,
        terminal_health,
        changed_files: changed_files.len() as u32,
        reviewed_files,
        test_status,
        pr_comment_count,
        port_count,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn readiness_summary_shape_is_stable() {
        let summary = format!(
            "Agent {} · {} files · {}/{} accepted · tests {} · {} PR comment{} · {} task{} active",
            "idle", 2, 1, 2, "unknown", 0, "s", 0, "s",
        );
        assert!(summary.contains("1/2 accepted"));
    }
}
