use std::path::{Path, PathBuf};
use std::process::Command;

use crate::models::{
    AgentSessionSummary, ArchivedWorkspace, BranchHealth, CreateChildWorkspaceInput,
    CreateWorkspaceInput, WorkspaceDetail, WorkspaceSummary,
};
use crate::repositories::{
    activity_repository, chat_session_repository, repository_repository,
    repository_settings_repository, settings_repository, workspace_repository,
};
use crate::services::{
    git_worktree_service, pr_draft_service, repo_scanner_service, terminal_service,
    workspace_script_service,
};
use crate::state::AppState;

/// Settings key holding the archived workspaces that can be restored.
const ARCHIVED_WORKSPACES_KEY: &str = "archived_workspaces";

pub(super) fn create_workspace(
    state: &AppState,
    input: CreateWorkspaceInput,
) -> Result<WorkspaceDetail, String> {
    if input.name.trim().is_empty() {
        return Err("Workspace name is required".to_string());
    }

    let next_id = workspace_repository::next_workspace_id(&state.db)?;
    let selected_repo = match input.repository_id.as_deref() {
        Some(repository_id) if !repository_id.trim().is_empty() => Some(
            repository_repository::get(&state.db, repository_id)?
                .ok_or_else(|| format!("Repository {repository_id} was not found"))?,
        ),
        _ => None,
    };

    let selected_worktree = match input.selected_worktree_id.as_deref() {
        Some(worktree_id) if !worktree_id.trim().is_empty() => {
            repository_repository::get_worktree(&state.db, worktree_id)?
        }
        _ => None,
    };

    let repo_name = selected_repo
        .as_ref()
        .map(|repo| repo.name.clone())
        .unwrap_or_else(|| input.repo.clone());
    if repo_name.trim().is_empty() {
        return Err("Repository is required".to_string());
    }

    let repo_path = selected_repo.as_ref().map(|repo| repo.path.clone());
    let mut branch = selected_worktree
        .as_ref()
        .and_then(|worktree| worktree.branch.clone())
        .or_else(|| input.selected_branch.clone())
        .or_else(|| input.branch.clone())
        .or_else(|| {
            selected_repo
                .as_ref()
                .and_then(|repo| repo.current_branch.clone())
        })
        .unwrap_or_else(|| slug_branch(&input.name));

    let selected_worktree_path;
    let workspace_root_path;
    let worktree_path;
    let worktree_managed_by_mnemonic;
    let workspace_source;

    if let Some(worktree) = &selected_worktree {
        selected_worktree_path = Some(worktree.path.clone());
        workspace_root_path = Some(worktree.path.clone());
        worktree_path = worktree.path.clone();
        worktree_managed_by_mnemonic = false;
        workspace_source = "external_worktree".to_string();
        if let Some(repo) = &selected_repo {
            git_worktree_service::ensure_mnemonic_gitignored_at(std::path::Path::new(&repo.path));
        }
    } else if let Some(repo) = &selected_repo {
        let managed_workspaces_root = settings_repository::get_managed_workspaces_root(&state.db)?;
        let created = git_worktree_service::create_mnemonic_worktree(
            &repo.path,
            &managed_workspaces_root,
            &next_id,
            &branch,
            &input.base_branch,
        )?;
        branch = created.branch;
        selected_worktree_path = Some(created.path.clone());
        workspace_root_path = Some(created.path.clone());
        worktree_path = created.path;
        worktree_managed_by_mnemonic = true;
        workspace_source = "mnemonic_managed_worktree".to_string();
    } else {
        return Err(
            "Repository selection is required to create a real branch workspace".to_string(),
        );
    }

    if worktree_managed_by_mnemonic {
        if let Some(repository_id) = selected_repo.as_ref().map(|r| r.id.as_str()) {
            if let Ok(Some(init_files_raw)) = repository_settings_repository::get_value(
                &state.db,
                repository_id,
                "workspace_init_files",
            ) {
                let file_list: Vec<String> = init_files_raw
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
                if !file_list.is_empty() {
                    if let Some(repo_path) = selected_repo.as_ref().map(|r| r.path.as_str()) {
                        match git_worktree_service::copy_files_from_repo_root(
                            repo_path,
                            &worktree_path,
                            &file_list,
                        ) {
                            Ok(copied) if !copied.is_empty() => {
                                log::info!(
                                    target: "mnemonic_lib",
                                    "Copied {} init file(s) from repo root into worktree: {:?}",
                                    copied.len(),
                                    copied
                                );
                            }
                            Ok(_) => {}
                            Err(err) => {
                                log::warn!(
                                    target: "mnemonic_lib",
                                    "Failed to copy init files into worktree: {err}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    let current_task = if input.task_prompt.trim().is_empty() {
        String::new()
    } else {
        format!("Queued: {}", input.task_prompt.trim())
    };
    let selected_agent = input.agent.clone();

    let detail = WorkspaceDetail {
        summary: WorkspaceSummary {
            id: next_id.clone(),
            name: input.name,
            repo: repo_name.clone(),
            branch: branch.clone(),
            agent: selected_agent.clone(),
            status: "Waiting".to_string(),
            current_step: "Planning".to_string(),
            completed_steps: vec![],
            changed_files: vec![],
            last_updated: "just now".to_string(),
            pr_status: None,
            pr_number: None,
            description: if worktree_managed_by_mnemonic {
                "Branch workspace created with a Mnemonic-managed Git worktree.".to_string()
            } else if selected_worktree.is_some() {
                "Branch workspace linked to an existing external Git worktree.".to_string()
            } else {
                "Workspace record created without a managed worktree.".to_string()
            },
            current_task,
            branch_health: BranchHealth {
                ahead_by: 0,
                behind_by: 0,
                merge_risk: "Low".to_string(),
                last_rebase: "not checked".to_string(),
                base_branch: input.base_branch.clone(),
            },
            agent_session: AgentSessionSummary {
                id: format!("session-{}", next_id),
                agent: detail_agent_name(&selected_agent),
                status: "idle".to_string(),
                model: "local".to_string(),
                token_count: 0,
                estimated_cost: "$0.00".to_string(),
                last_message: "No terminal session started yet".to_string(),
                started_at: "not started".to_string(),
            },
            repository_id: selected_repo.as_ref().map(|repo| repo.id.clone()),
            repository_path: repo_path.clone(),
            selected_branch: Some(branch.clone()),
            selected_worktree_id: selected_worktree
                .as_ref()
                .map(|worktree| worktree.id.clone()),
            selected_worktree_path: selected_worktree_path.clone(),
            workspace_root_path: workspace_root_path.clone(),
            worktree_managed_by_mnemonic,
            workspace_source: workspace_source.clone(),
            parent_workspace_id: input.parent_workspace_id.clone(),
            source_workspace_id: input.source_workspace_id.clone(),
            derived_from_branch: input.derived_from_branch.clone(),
            linked_worktrees: vec![],
            cost_limit_usd: None,
            run_tests_on_create: input.run_tests,
            create_pr_on_complete: input.create_pr,
        },
        worktree_path,
        base_branch: input.base_branch,
        recent_events: vec!["Workspace lifecycle record created".to_string()],
    };

    if let Err(err) = workspace_repository::insert(&state.db, &detail) {
        if detail.summary.worktree_managed_by_mnemonic {
            if let Some(repo_path) = detail.summary.repository_path.as_deref() {
                let _ = git_worktree_service::remove_mnemonic_worktree(
                    repo_path,
                    &detail.worktree_path,
                );
            }
        }
        return Err(err);
    }

    activity_repository::record(
        &state.db,
        &detail.summary.id,
        &detail.summary.repo,
        Some(&detail.summary.branch),
        "Workspace created",
        "success",
        Some(&format!("{} · {}", workspace_source, detail.worktree_path)),
    )?;

    if let Some(repository_id) = detail.summary.repository_id.as_deref() {
        repo_scanner_service::refresh_repository_by_id(state, repository_id)?;
    }

    let auto_setup_enabled = settings_repository::get_value(&state.db, "auto_run_setup_enabled")
        .ok()
        .flatten()
        .map(|value| value == "true")
        .unwrap_or(false);

    if detail.summary.worktree_managed_by_mnemonic && auto_setup_enabled {
        match workspace_script_service::run_workspace_setup(state, &detail.summary.id) {
            Ok(sessions) if !sessions.is_empty() => {
                let details = format!(
                    "Started {} setup command(s) from .mnemonic/config.json",
                    sessions.len()
                );
                let _ = activity_repository::record(
                    &state.db,
                    &detail.summary.id,
                    &detail.summary.repo,
                    Some(&detail.summary.branch),
                    "Workspace setup launched",
                    "info",
                    Some(&details),
                );
            }
            Ok(_) => {}
            Err(err) => {
                let _ = activity_repository::record(
                    &state.db,
                    &detail.summary.id,
                    &detail.summary.repo,
                    Some(&detail.summary.branch),
                    "Workspace setup warning",
                    "warning",
                    Some(&err),
                );
            }
        }
    } else if detail.summary.worktree_managed_by_mnemonic {
        let _ = activity_repository::record(
            &state.db,
            &detail.summary.id,
            &detail.summary.repo,
            Some(&detail.summary.branch),
            "Workspace setup waiting",
            "info",
            Some(
                "Automatic setup is disabled. Run setup manually from the workspace commands panel.",
            ),
        );
    }

    if detail.summary.worktree_managed_by_mnemonic {
        if let Some(repository_id) = detail.summary.repository_id.as_deref() {
            if let Ok(Some(setup_script)) =
                repository_settings_repository::get_value(&state.db, repository_id, "setup_script")
            {
                let script = setup_script.trim().to_string();
                if !script.is_empty() {
                    use crate::models::CreateWorkspaceTerminalInput;
                    match terminal_service::create_workspace_terminal(
                        state,
                        CreateWorkspaceTerminalInput {
                            workspace_id: detail.summary.id.clone(),
                            kind: "run".to_string(),
                            profile: "shell".to_string(),
                            profile_id: Some("shell".to_string()),
                            title: Some("Repo setup".to_string()),
                            command: Some("sh".to_string()),
                            args: Some(vec!["-c".to_string(), script]),
                            extra_args: None,
                            resume_claude_session_id: None,
                            cols: None,
                            rows: None,
                        },
                    ) {
                        Ok(_session) => {
                            let _ = activity_repository::record(
                                &state.db,
                                &detail.summary.id,
                                &detail.summary.repo,
                                Some(&detail.summary.branch),
                                "Repository setup script launched",
                                "info",
                                Some("Running per-repo setup script from app settings"),
                            );
                        }
                        Err(err) => {
                            let _ = activity_repository::record(
                                &state.db,
                                &detail.summary.id,
                                &detail.summary.repo,
                                Some(&detail.summary.branch),
                                "Repository setup script failed",
                                "warning",
                                Some(&err),
                            );
                        }
                    }
                }
            }
        }
    }

    if detail.summary.run_tests_on_create {
        if let Ok(config) =
            workspace_script_service::get_workspace_mnemonic_config(state, &detail.summary.id)
        {
            if !config.run.is_empty() {
                let mut started = 0usize;
                for index in 0..config.run.len() {
                    if workspace_script_service::start_workspace_run_command(
                        state,
                        &detail.summary.id,
                        index,
                    )
                    .is_ok()
                    {
                        started += 1;
                    }
                }
                if started > 0 {
                    let _ = activity_repository::record(
                        &state.db,
                        &detail.summary.id,
                        &detail.summary.repo,
                        Some(&detail.summary.branch),
                        "Workspace checks launched",
                        "info",
                        Some(&format!(
                            "Started {started} run command(s) from workspace options."
                        )),
                    );
                }
            }
        }
    }

    if detail.summary.create_pr_on_complete {
        let _ = pr_draft_service::refresh_workspace_pr_draft(state, &detail.summary.id);
    }

    Ok(detail)
}

pub(super) fn delete_workspace(state: &AppState, workspace_id: &str) -> Result<(), String> {
    delete_workspace_impl(state, workspace_id, false)
}

/// Archive: like delete, but also moves the managed worktree into the archive
/// folder so it no longer sits among active workspaces. Nothing is discarded.
pub(super) fn archive_workspace(state: &AppState, workspace_id: &str) -> Result<(), String> {
    delete_workspace_impl(state, workspace_id, true)
}

fn delete_workspace_impl(
    state: &AppState,
    workspace_id: &str,
    remove_managed_worktree: bool,
) -> Result<(), String> {
    log::info!(target: "mnemonic_lib", "delete_workspace begin: id={workspace_id}");
    let detail = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    log::info!(
        target: "mnemonic_lib",
        "delete_workspace loaded: id={workspace_id} worktree_path={} mnemonic_managed={}",
        detail.worktree_path,
        detail.summary.worktree_managed_by_mnemonic
    );

    match terminal_service::stop_workspace_terminal_session(state, workspace_id) {
        Ok(_) => log::info!(target: "mnemonic_lib", "stop agent terminal: ok id={workspace_id}"),
        Err(e) => log::warn!(target: "mnemonic_lib", "stop agent terminal: {e} id={workspace_id}"),
    }
    match terminal_service::stop_workspace_utility_terminal_session(state, workspace_id) {
        Ok(_) => log::info!(target: "mnemonic_lib", "stop utility terminal: ok id={workspace_id}"),
        Err(e) => {
            log::warn!(target: "mnemonic_lib", "stop utility terminal: {e} id={workspace_id}")
        }
    }
    match workspace_script_service::stop_workspace_run_commands(state, workspace_id) {
        Ok(stopped) => log::info!(
            target: "mnemonic_lib",
            "stop run terminals: {} stopped id={workspace_id}",
            stopped.len()
        ),
        Err(e) => log::warn!(target: "mnemonic_lib", "stop run terminals: {e} id={workspace_id}"),
    }

    let path = detail
        .summary
        .workspace_root_path
        .as_deref()
        .unwrap_or(&detail.worktree_path)
        .to_string();
    let path_on_disk = Path::new(&path);
    let remove_managed_worktree =
        remove_managed_worktree && detail.summary.worktree_managed_by_mnemonic;
    let mut worktree_cleanup_warning: Option<String> = None;
    let mut archived_path: Option<PathBuf> = None;

    if let Err(err) = terminal_service::kill_embedded_workspace_session(workspace_id) {
        log::warn!(target: "mnemonic_lib", "kill embedded tmux session: {err} id={workspace_id}");
    }

    if remove_managed_worktree {
        let repo_path_opt = detail
            .summary
            .repository_path
            .clone()
            .or_else(|| infer_repo_path_from_worktree(&path).ok());

        match repo_path_opt {
            Some(repo_path) => {
                if !path_on_disk.exists() {
                    log::info!(
                        target: "mnemonic_lib",
                        "worktree folder already absent; git worktree prune repo={} path={}",
                        repo_path,
                        path
                    );
                    if let Err(err) = git_worktree_service::prune_worktrees(Path::new(&repo_path)) {
                        log::warn!(
                            target: "mnemonic_lib",
                            "git worktree prune failed (continuing DB delete): {err}"
                        );
                    }
                } else {
                    match git_worktree_service::archive_mnemonic_worktree(&repo_path, &path) {
                        Ok(destination) => {
                            log::info!(
                                target: "mnemonic_lib",
                                "git worktree move: ok from={path} to={}",
                                destination.display()
                            );
                            archived_path = Some(destination);
                        }
                        Err(err) => {
                            return Err(format!(
                                "Could not move the worktree into the archive folder: {err}"
                            ));
                        }
                    }
                }
            }
            None => {
                if path_on_disk.exists() {
                    log::warn!(
                        target: "mnemonic_lib",
                        "delete_workspace: could not resolve repository_path for existing worktree path; continuing DB delete path={path}"
                    );
                    worktree_cleanup_warning = Some(
                        "Could not resolve the main repository path for this worktree. The workspace was removed from Mnemonic; remove the checkout folder manually if it is still on disk."
                            .to_string(),
                    );
                } else {
                    log::warn!(
                        target: "mnemonic_lib",
                        "skip git cleanup: no repository_path and worktree path missing; continuing DB delete path={path}"
                    );
                }
            }
        }
    } else {
        log::info!(
            target: "mnemonic_lib",
            "skip git worktree remove (not Mnemonic-managed or policy); path={path}"
        );
    }

    log::info!(target: "mnemonic_lib", "sqlite delete workspace row: id={workspace_id}");
    workspace_repository::delete(&state.db, workspace_id)?;

    if let Some(archived_path) = archived_path {
        if let Err(err) = remember_archived_workspace(state, &detail, &archived_path) {
            log::warn!(
                target: "mnemonic_lib",
                "archived workspace {workspace_id} cannot be restored from Mnemonic: {err}"
            );
        }
    }

    if let Some(repo_path) = detail.summary.repository_path.as_deref() {
        let _ = git_worktree_service::prune_worktrees(Path::new(repo_path));
    }

    let activity_level = if worktree_cleanup_warning.is_some() {
        "warning"
    } else {
        "success"
    };
    let activity_details = worktree_cleanup_warning.or_else(|| {
        Some(
            if remove_managed_worktree {
                "Archived workspace; worktree moved to the archive folder."
            } else {
                "Removed workspace record; on-disk worktree was not removed by Mnemonic."
            }
            .to_string(),
        )
    });

    activity_repository::insert(
        &state.db,
        &crate::models::ActivityItem {
            id: format!("act-delete-{workspace_id}"),
            workspace_id: None,
            repo: detail.summary.repo.clone(),
            branch: Some(detail.summary.branch.clone()),
            event: "Workspace forgotten".to_string(),
            level: activity_level.to_string(),
            details: activity_details,
            timestamp: "just now".to_string(),
        },
    )?;

    if let Some(repository_id) = detail.summary.repository_id.as_deref() {
        if let Err(err) = repo_scanner_service::refresh_repository_by_id(state, repository_id) {
            log::warn!(
                target: "mnemonic_lib",
                "refresh_repository_by_id after delete failed (workspace already removed from DB): repository_id={repository_id} err={err}"
            );
        }
    }

    log::info!(target: "mnemonic_lib", "delete_workspace finished: id={workspace_id}");
    Ok(())
}

fn load_archived_workspaces(state: &AppState) -> Result<Vec<ArchivedWorkspace>, String> {
    let Some(json) = settings_repository::get_value(&state.db, ARCHIVED_WORKSPACES_KEY)? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&json).map_err(|err| format!("Failed to read archived workspaces: {err}"))
}

fn save_archived_workspaces(
    state: &AppState,
    archived: &[ArchivedWorkspace],
) -> Result<(), String> {
    let json = serde_json::to_string(archived)
        .map_err(|err| format!("Failed to save archived workspaces: {err}"))?;
    settings_repository::set_value(&state.db, ARCHIVED_WORKSPACES_KEY, &json)
}

/// Records what restoring needs: the workspace row and its chats, which
/// archiving deletes. Entries whose archive folder is gone are dropped, and
/// the changed files and events are left out because they are recomputed.
fn remember_archived_workspace(
    state: &AppState,
    detail: &WorkspaceDetail,
    archived_path: &Path,
) -> Result<(), String> {
    let chat_sessions = chat_session_repository::list_for_workspace(&state.db, &detail.summary.id)?;
    let mut detail = detail.clone();
    detail.summary.changed_files.clear();
    detail.recent_events.clear();

    let mut archived = load_archived_workspaces(state)?;
    archived.retain(|entry| {
        entry.detail.summary.id != detail.summary.id && Path::new(&entry.archived_path).is_dir()
    });
    archived.push(ArchivedWorkspace {
        detail,
        chat_sessions,
        archived_path: archived_path.to_string_lossy().to_string(),
    });
    save_archived_workspaces(state, &archived)
}

/// Archived workspaces whose worktree is still in the archive folder.
pub(super) fn list_archived_workspaces(state: &AppState) -> Result<Vec<ArchivedWorkspace>, String> {
    Ok(load_archived_workspaces(state)?
        .into_iter()
        .filter(|entry| Path::new(&entry.archived_path).is_dir())
        .collect())
}

/// Moves an archived workspace's worktree back and recreates the workspace
/// and its chats as they were when it was archived.
pub(super) fn restore_archived_workspace(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceSummary, String> {
    let mut archived = load_archived_workspaces(state)?;
    let index = archived
        .iter()
        .position(|entry| entry.detail.summary.id == workspace_id)
        .ok_or_else(|| format!("Archived workspace {workspace_id} was not found"))?;
    let entry = archived.remove(index);
    let worktree_path = entry
        .detail
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or_else(|| entry.detail.worktree_path.clone());
    let repo_path = entry
        .detail
        .summary
        .repository_path
        .clone()
        .ok_or_else(|| "The archived workspace has no repository path recorded".to_string())?;
    if Path::new(&worktree_path).exists() {
        return Err(format!("Cannot restore: {worktree_path} already exists"));
    }

    git_worktree_service::restore_archived_worktree(
        &repo_path,
        &entry.archived_path,
        &worktree_path,
    )?;
    if let Err(err) = workspace_repository::insert(&state.db, &entry.detail) {
        if let Err(revert_err) = git_worktree_service::restore_archived_worktree(
            &repo_path,
            &worktree_path,
            &entry.archived_path,
        ) {
            log::error!(
                target: "mnemonic_lib",
                "failed to move {worktree_path} back into the archive after a failed restore: {revert_err}"
            );
        }
        return Err(err);
    }
    for record in &entry.chat_sessions {
        if let Err(err) = chat_session_repository::insert(&state.db, record) {
            log::error!(target: "mnemonic_lib", "failed to restore chat session {}: {err}", record.id);
        }
    }
    if let Err(err) = save_archived_workspaces(state, &archived) {
        log::error!(target: "mnemonic_lib", "failed to update archived workspaces: {err}");
    }
    if let Err(err) = activity_repository::record(
        &state.db,
        workspace_id,
        &entry.detail.summary.repo,
        Some(&entry.detail.summary.branch),
        "Workspace restored",
        "success",
        None,
    ) {
        log::error!(target: "mnemonic_lib", "failed to record workspace restore: {err}");
    }
    if let Some(repository_id) = entry.detail.summary.repository_id.as_deref() {
        if let Err(err) = repo_scanner_service::refresh_repository_by_id(state, repository_id) {
            log::warn!(
                target: "mnemonic_lib",
                "refresh_repository_by_id after restore failed: repository_id={repository_id} err={err}"
            );
        }
    }

    workspace_repository::get(&state.db, workspace_id)
}

pub(super) fn create_child_workspace(
    state: &AppState,
    input: CreateChildWorkspaceInput,
) -> Result<WorkspaceDetail, String> {
    let parent = workspace_repository::get_detail(&state.db, &input.parent_workspace_id)?
        .ok_or_else(|| {
            format!(
                "Parent workspace {} was not found",
                input.parent_workspace_id
            )
        })?;
    if let Some(branch) = input.branch.as_ref() {
        let repo_path = parent.summary.repository_path.clone().ok_or_else(|| {
            "Parent workspace has no repository path; cannot validate child branch name".to_string()
        })?;
        if git_worktree_service::local_branch_exists(Path::new(&repo_path), branch)? {
            return Err(format!(
                "Branch '{branch}' already exists in this repository. Choose a different branch name."
            ));
        }
    }
    let repository_id = parent.summary.repository_id.clone().ok_or_else(|| {
        "Parent workspace has no repository id; cannot create child workspace".to_string()
    })?;
    let parent_branch = parent
        .summary
        .selected_branch
        .clone()
        .unwrap_or(parent.summary.branch.clone());
    let parent_root = parent
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or(parent.worktree_path.clone());
    let head_ref = git_current_head_ref(&parent_root).map_err(|err| {
        format!("Cannot create child workspace: invalid parent workspace context. {err}")
    })?;

    create_workspace(
        state,
        CreateWorkspaceInput {
            name: input.name,
            repo: parent.summary.repo.clone(),
            base_branch: head_ref,
            branch: input.branch,
            agent: input.agent.unwrap_or_else(|| parent.summary.agent.clone()),
            task_prompt: input.task_prompt.unwrap_or_default(),
            open_in_cursor: input.open_in_cursor.unwrap_or(false),
            run_tests: input.run_tests.unwrap_or(true),
            create_pr: input.create_pr.unwrap_or(true),
            repository_id: Some(repository_id),
            selected_worktree_id: None,
            selected_branch: Some(parent_branch.clone()),
            parent_workspace_id: Some(parent.summary.id.clone()),
            source_workspace_id: Some(parent.summary.id),
            derived_from_branch: Some(parent_branch),
        },
    )
}

fn slug_branch(name: &str) -> String {
    let slug = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    format!("feat/{slug}")
}

fn detail_agent_name(agent: &str) -> String {
    match agent {
        "Codex" => "codex".to_string(),
        "Claude Code" => "claude_code".to_string(),
        other => other.to_lowercase(),
    }
}

fn git_current_head_ref(workspace_root: &str) -> Result<String, String> {
    let path = Path::new(workspace_root);
    if !path.exists() || !path.is_dir() {
        return Err(format!("Workspace path is unavailable: {}", path.display()));
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|err| format!("Failed to run git rev-parse HEAD: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("git rev-parse HEAD failed in {}", path.display())
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn infer_repo_path_from_worktree(worktree_path: &str) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .map_err(|err| format!("Failed to run git rev-parse --git-common-dir: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("git rev-parse --git-common-dir failed in {worktree_path}")
        } else {
            stderr
        });
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let common = Path::new(&raw);
    let resolved = if common.is_absolute() {
        common.to_path_buf()
    } else {
        Path::new(worktree_path).join(common)
    };
    let repo = resolved.parent().ok_or_else(|| {
        format!(
            "Could not derive repository root from git common dir {}",
            resolved.display()
        )
    })?;
    Ok(repo.to_string_lossy().to_string())
}
