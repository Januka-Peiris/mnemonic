use crate::models::{
    AgentContextWorktree, WorkspaceAgentContext, WorkspaceContextItem, WorkspaceContextPreview,
};
use crate::repositories::workspace_repository;
use crate::services::repo_intelligence_service;
use crate::state::AppState;

pub fn get_workspace_agent_context(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceAgentContext, String> {
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let primary_path = workspace
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or_else(|| workspace.worktree_path.clone());
    let linked_worktrees = linked_worktrees(state, workspace_id)?;
    let prompt_preamble = format_prompt_preamble(&primary_path, &linked_worktrees);
    Ok(WorkspaceAgentContext {
        workspace_id: workspace_id.to_string(),
        primary_path,
        linked_worktrees,
        prompt_preamble,
    })
}

pub fn get_workspace_context_preview(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceContextPreview, String> {
    let cfg = crate::context::schema::SelectConfig::default();
    let preview = if repo_intelligence_service::repo_intelligence_enabled(state) {
        match repo_intelligence_service::build_workspace_context_preview(
            state,
            workspace_id,
            None,
            &cfg,
        ) {
            Ok(preview) => preview,
            Err(err) => {
                let mut fallback = crate::context::preview::build_context_preview(
                    state,
                    workspace_id,
                    None,
                    &cfg,
                )?;
                fallback.warning = match fallback.warning {
                    Some(current) => Some(format!("{current} | Repo intelligence fallback: {err}")),
                    None => Some(format!("Repo intelligence fallback: {err}")),
                };
                fallback
            }
        }
    } else {
        crate::context::preview::build_context_preview(state, workspace_id, None, &cfg)?
    };

    // Map new ContextPreview → old WorkspaceContextPreview for frontend compat
    let items = preview
        .included
        .iter()
        .map(|seg| WorkspaceContextItem {
            label: seg.path.clone(),
            path: Some(seg.path.clone()),
            kind: seg.tier.clone(),
            priority: if seg.tier == "mandatory" { 10 } else { 30 },
            chars: seg.content.chars().count(),
            included: true,
            trimmed: false,
        })
        .collect();

    let prompt_context = preview
        .included
        .iter()
        .map(|s| s.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let approx_chars = prompt_context.chars().count();

    // Get branch info for display
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} not found"))?;
    let primary_path = workspace
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or_else(|| workspace.worktree_path.clone());
    let root = std::path::Path::new(&primary_path);
    let (repo_root_str, default_branch, ref_name, commit_hash) =
        crate::context::discovery::resolve_default_ref(root)
            .map(|r| {
                (
                    primary_path.clone(),
                    r.branch.clone(),
                    r.ref_name.clone(),
                    r.commit_hash.clone(),
                )
            })
            .unwrap_or_else(|_| {
                (
                    primary_path.clone(),
                    "unknown".to_string(),
                    "unknown".to_string(),
                    String::new(),
                )
            });

    let status = if preview.stale_map { "stale" } else { "fresh" };

    Ok(WorkspaceContextPreview {
        workspace_id: workspace_id.to_string(),
        repo_root: repo_root_str,
        status: status.to_string(),
        default_branch,
        ref_name,
        commit_hash,
        generated_at: None,
        approx_chars,
        max_chars: (cfg.soft_repo_context_tokens * 4) as usize,
        trimmed: !preview.excluded.is_empty(),
        items,
        prompt_context,
        warning: preview.warning,
    })
}

pub fn refresh_workspace_repo_context(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceContextPreview, String> {
    if repo_intelligence_service::repo_intelligence_enabled(state) {
        let _ = repo_intelligence_service::ensure_workspace_repo_intelligence(
            state,
            workspace_id,
            true,
        );
    } else {
        let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
            .ok_or_else(|| format!("Workspace {workspace_id} not found"))?;
        let primary_path = workspace
            .summary
            .workspace_root_path
            .clone()
            .unwrap_or_else(|| workspace.worktree_path.clone());
        let root = std::path::Path::new(&primary_path);
        let _ = crate::context::discovery::build_repo_map(root, true, &state.db);
    }
    get_workspace_context_preview(state, workspace_id)
}

pub fn get_context_preview_with_hint(
    state: &AppState,
    workspace_id: &str,
    prompt_hint: Option<&str>,
) -> Result<crate::context::schema::ContextPreview, String> {
    let cfg = crate::context::schema::SelectConfig::default();
    if repo_intelligence_service::repo_intelligence_enabled(state) {
        match repo_intelligence_service::build_workspace_context_preview(
            state,
            workspace_id,
            prompt_hint,
            &cfg,
        ) {
            Ok(preview) => Ok(preview),
            Err(err) => {
                let mut fallback = crate::context::preview::build_context_preview(
                    state,
                    workspace_id,
                    prompt_hint,
                    &cfg,
                )?;
                fallback.warning = match fallback.warning {
                    Some(current) => Some(format!("{current} | Repo intelligence fallback: {err}")),
                    None => Some(format!("Repo intelligence fallback: {err}")),
                };
                Ok(fallback)
            }
        }
    } else {
        crate::context::preview::build_context_preview(state, workspace_id, prompt_hint, &cfg)
    }
}

pub fn format_prompt_preamble(primary_path: &str, linked: &[AgentContextWorktree]) -> String {
    if linked.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "Mnemonic linked repository context:".to_string(),
        format!("- Primary writable workspace: {primary_path}"),
        "- Linked repositories are read-only context unless the user explicitly asks to edit them:"
            .to_string(),
    ];
    for item in linked {
        let branch = item.branch.as_deref().unwrap_or("detached");
        lines.push(format!("  - {} ({branch}): {}", item.repo_name, item.path));
    }
    lines.push("Use these paths for cross-repo understanding and mention before making assumptions across repos.".to_string());
    lines.join("\n")
}

fn linked_worktrees(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<AgentContextWorktree>, String> {
    Ok(
        workspace_repository::list_linked_worktrees_for_workspace(&state.db, workspace_id)?
            .into_iter()
            .map(|linked| AgentContextWorktree {
                repo_id: linked.repo_id,
                repo_name: linked.repo_name,
                path: linked.path,
                branch: linked.branch,
                head: linked.head,
            })
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_linked_context_has_no_preamble() {
        assert_eq!(format_prompt_preamble("/tmp/app", &[]), "");
    }

    #[test]
    fn formats_linked_context_as_read_only() {
        let preamble = format_prompt_preamble(
            "/tmp/frontend",
            &[AgentContextWorktree {
                repo_id: "repo-backend".to_string(),
                repo_name: "backend".to_string(),
                path: "/tmp/backend".to_string(),
                branch: Some("main".to_string()),
                head: None,
            }],
        );
        assert!(preamble.contains("Primary writable workspace: /tmp/frontend"));
        assert!(preamble.contains("backend (main): /tmp/backend"));
        assert!(preamble.contains("read-only context"));
    }
}
