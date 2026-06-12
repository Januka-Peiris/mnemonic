use std::collections::HashMap;
use std::path::Path;

use crate::models::{
    CreateWorkspaceInput, OpenDeepLinkInput, OpenDeepLinkResult, QueueAgentPromptInput,
};
use crate::repositories::{repository_repository, workspace_repository};
use crate::services::{terminal_service, workspace_service};
use crate::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedDeepLink {
    repo: Option<String>,
    branch: Option<String>,
    prompt: Option<String>,
    agent: Option<String>,
    base_branch: Option<String>,
}

pub fn open_deep_link(
    state: &AppState,
    input: OpenDeepLinkInput,
) -> Result<OpenDeepLinkResult, String> {
    let parsed = merge_input(input)?;
    let repo_key = parsed
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Deep link requires repo".to_string())?;
    let repo = resolve_repository(state, repo_key)?;
    let branch = parsed
        .branch
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            repo.current_branch
                .clone()
                .unwrap_or_else(|| "main".to_string())
        });

    if let Some(existing) = workspace_repository::list(&state.db)?
        .into_iter()
        .find(|workspace| {
            workspace.repository_id.as_deref() == Some(repo.id.as_str())
                && (workspace.selected_branch.as_deref() == Some(branch.as_str())
                    || workspace.branch == branch)
        })
    {
        let prompt_sent = send_prompt_if_present(
            state,
            &existing.id,
            parsed.prompt.as_deref(),
            parsed.agent.as_deref(),
        )?;
        return Ok(OpenDeepLinkResult {
            workspace_id: existing.id,
            created: false,
            prompt_sent,
        });
    }

    let name = format!("{} · {}", repo.name, branch.replace('/', "-"));
    let detail = workspace_service::create_workspace(
        state,
        CreateWorkspaceInput {
            name,
            repo: repo.name.clone(),
            base_branch: parsed.base_branch.unwrap_or_else(|| {
                repo.current_branch
                    .clone()
                    .unwrap_or_else(|| "main".to_string())
            }),
            branch: Some(branch),
            agent: agent_label(parsed.agent.as_deref()),
            task_prompt: parsed.prompt.clone().unwrap_or_default(),
            open_in_cursor: false,
            run_tests: true,
            create_pr: true,
            repository_id: Some(repo.id),
            selected_worktree_id: None,
            selected_branch: None,
            parent_workspace_id: None,
            source_workspace_id: None,
            derived_from_branch: None,
        },
    )?;
    let prompt_sent = send_prompt_if_present(
        state,
        &detail.summary.id,
        parsed.prompt.as_deref(),
        parsed.agent.as_deref(),
    )?;
    Ok(OpenDeepLinkResult {
        workspace_id: detail.summary.id,
        created: true,
        prompt_sent,
    })
}

fn resolve_repository(
    state: &AppState,
    key: &str,
) -> Result<crate::models::DiscoveredRepository, String> {
    let repositories = repository_repository::list(&state.db)?;
    if let Some(repo) = repositories
        .iter()
        .find(|repo| repo.id == key || repo.name == key || repo.path == key)
    {
        return Ok(repo.clone());
    }
    let path = Path::new(key);
    if path.exists() && path.is_dir() {
        let canonical = path
            .canonicalize()
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .to_string();
        if let Some(repo) = repositories.iter().find(|repo| {
            Path::new(&repo.path)
                .canonicalize()
                .unwrap_or_else(|_| Path::new(&repo.path).to_path_buf())
                .to_string_lossy()
                == canonical
        }) {
            return Ok(repo.clone());
        }
    }
    Err(format!(
        "No discovered repository matched '{key}'. Add/scan it in Settings first."
    ))
}

fn send_prompt_if_present(
    state: &AppState,
    workspace_id: &str,
    prompt: Option<&str>,
    agent: Option<&str>,
) -> Result<bool, String> {
    let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    terminal_service::queue_workspace_agent_prompt(
        state,
        QueueAgentPromptInput {
            session_id: None,
            workspace_id: workspace_id.to_string(),
            prompt: prompt.to_string(),
            profile: Some(agent_profile(agent)),
            profile_id: None,
            extra_args: None,
        },
    )?;
    Ok(true)
}

fn merge_input(input: OpenDeepLinkInput) -> Result<ParsedDeepLink, String> {
    let mut parsed = input
        .url
        .as_deref()
        .map(parse_deep_link_url)
        .transpose()?
        .unwrap_or(ParsedDeepLink {
            repo: None,
            branch: None,
            prompt: None,
            agent: None,
            base_branch: None,
        });
    parsed.repo = input.repo.or(parsed.repo);
    parsed.branch = input.branch.or(parsed.branch);
    parsed.prompt = input.prompt.or(parsed.prompt);
    parsed.agent = input.agent.or(parsed.agent);
    parsed.base_branch = input.base_branch.or(parsed.base_branch);
    Ok(parsed)
}

fn parse_deep_link_url(url: &str) -> Result<ParsedDeepLink, String> {
    let trimmed = url.trim();
    if !trimmed.starts_with("mn://open") {
        return Err("Unsupported Mnemonic deep link. Expected mn://open?...".to_string());
    }
    let query = trimmed
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or("");
    let params = parse_query(query);
    Ok(ParsedDeepLink {
        repo: params.get("repo").cloned(),
        branch: params.get("branch").cloned(),
        prompt: params.get("prompt").cloned(),
        agent: params.get("agent").cloned(),
        base_branch: params
            .get("baseBranch")
            .or_else(|| params.get("base_branch"))
            .cloned(),
    })
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            Some((percent_decode(key)?, percent_decode(value)?))
        })
        .collect()
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn agent_profile(agent: Option<&str>) -> String {
    match agent.unwrap_or("codex").trim().to_lowercase().as_str() {
        "claude" | "claude_code" | "claude-code" | "claude code" => "claude_code".to_string(),
        "shell" => "shell".to_string(),
        _ => "codex".to_string(),
    }
}

fn agent_label(agent: Option<&str>) -> String {
    match agent_profile(agent).as_str() {
        "claude_code" => "Claude Code".to_string(),
        "shell" => "Shell".to_string(),
        _ => "Codex".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mn_open_url() {
        let parsed = parse_deep_link_url(
            "mn://open?repo=my-repo&branch=feat%2Fthing&prompt=Fix+tests&agent=claude",
        )
        .unwrap();
        assert_eq!(parsed.repo.as_deref(), Some("my-repo"));
        assert_eq!(parsed.branch.as_deref(), Some("feat/thing"));
        assert_eq!(parsed.prompt.as_deref(), Some("Fix tests"));
        assert_eq!(parsed.agent.as_deref(), Some("claude"));
    }

    #[test]
    fn rejects_other_urls() {
        assert!(parse_deep_link_url("https://example.com").is_err());
    }

    #[test]
    fn normalizes_agents() {
        assert_eq!(agent_profile(Some("claude-code")), "claude_code");
        assert_eq!(agent_profile(Some("codex")), "codex");
        assert_eq!(agent_profile(Some("unknown")), "codex");
    }
}
