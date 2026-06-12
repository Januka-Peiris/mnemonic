use std::collections::BTreeMap;

use crate::models::agent_profile::RawAgentProfile;
use crate::models::AgentProfile;
use crate::repositories::settings_repository;
use crate::services::workspace_script_service;
use crate::state::AppState;

const APP_AGENT_PROFILES_KEY: &str = "agent_profiles";

pub fn list_workspace_agent_profiles(
    state: &AppState,
    workspace_id: Option<&str>,
) -> Result<Vec<AgentProfile>, String> {
    let mut profiles = default_profiles()
        .into_iter()
        .map(|profile| (profile.id.clone(), profile))
        .collect::<BTreeMap<_, _>>();

    for profile in list_app_agent_profiles(state)? {
        profiles.insert(profile.id.clone(), profile);
    }

    if let Some(workspace_id) = workspace_id {
        if let Ok(config) =
            workspace_script_service::get_workspace_mnemonic_config(state, workspace_id)
        {
            for profile in config.agent_profiles {
                profiles.insert(profile.id.clone(), profile);
            }
        }
    }
    Ok(profiles.into_values().collect())
}

pub fn list_app_agent_profiles(state: &AppState) -> Result<Vec<AgentProfile>, String> {
    let Some(value) = settings_repository::get_value(&state.db, APP_AGENT_PROFILES_KEY)? else {
        return Ok(vec![]);
    };
    parse_app_agent_profiles(&value)
}

pub fn save_app_agent_profiles(
    state: &AppState,
    profiles: Vec<AgentProfile>,
) -> Result<Vec<AgentProfile>, String> {
    let mut normalized = profiles
        .into_iter()
        .filter_map(normalize_saved_profile)
        .collect::<Vec<_>>();
    normalized.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.label.cmp(&b.label)));
    normalized.dedup_by(|a, b| a.id == b.id);
    let value = serde_json::to_string(&normalized)
        .map_err(|err| format!("Failed to serialize agent profiles: {err}"))?;
    settings_repository::set_value(&state.db, APP_AGENT_PROFILES_KEY, &value)?;
    Ok(normalized)
}

pub fn resolve_agent_profile(
    state: &AppState,
    workspace_id: Option<&str>,
    profile_id: Option<&str>,
    legacy_profile: Option<&str>,
) -> Result<AgentProfile, String> {
    let profiles = list_workspace_agent_profiles(state, workspace_id)?;
    let requested = profile_id
        .filter(|value| !value.trim().is_empty())
        .or(legacy_profile)
        .map(str::trim);

    if let Some(requested) = requested {
        if let Some(profile) = profiles.iter().find(|profile| profile.id == requested) {
            return Ok(profile.clone());
        }
        if let Some(profile) = profiles.iter().find(|profile| profile.agent == requested) {
            return Ok(profile.clone());
        }
        return Err(format!(
            "Agent profile `{requested}` was not found. Add it in Settings → Agent Profiles or .forge/config.json."
        ));
    }

    profiles
        .iter()
        .find(|profile| profile.agent != "shell")
        .cloned()
        .ok_or_else(|| {
            "No non-shell agent profile is configured. Add one in Settings → Agent Profiles or .forge/config.json."
                .to_string()
        })
}

pub fn is_coordinator_eligible(profile: &AgentProfile) -> bool {
    if profile.agent == "shell" {
        return false;
    }
    profile.coordinator_eligible.unwrap_or(true)
}

pub(crate) fn raw_to_profile(raw: RawAgentProfile) -> Option<AgentProfile> {
    let id = raw.id?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let agent = normalize_agent(raw.agent.as_deref().unwrap_or(&id));
    let provider = raw
        .provider
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let endpoint = raw
        .endpoint
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let local = raw.local.unwrap_or(false)
        || provider.as_deref().map(is_local_provider).unwrap_or(false)
        || endpoint.as_deref().map(is_local_endpoint).unwrap_or(false);
    let command = raw
        .command
        .unwrap_or_else(|| default_command_for_agent(&agent));
    Some(AgentProfile {
        label: raw.label.unwrap_or_else(|| id.clone()),
        id,
        agent,
        command,
        args: raw.args,
        model: raw.model,
        reasoning: raw.reasoning,
        mode: raw.mode,
        provider,
        endpoint,
        local,
        description: raw.description,
        skills: raw.skills,
        templates: raw.templates,
        role_preference: trim_optional(raw.role_preference),
        coordinator_eligible: raw.coordinator_eligible,
    })
}

fn parse_app_agent_profiles(value: &str) -> Result<Vec<AgentProfile>, String> {
    if value.trim().is_empty() {
        return Ok(vec![]);
    }
    if let Ok(profiles) = serde_json::from_str::<Vec<AgentProfile>>(value) {
        return Ok(profiles
            .into_iter()
            .filter_map(normalize_saved_profile)
            .collect());
    }
    let raw_profiles = serde_json::from_str::<Vec<RawAgentProfile>>(value)
        .map_err(|err| format!("Invalid saved agent profiles: {err}"))?;
    Ok(raw_profiles
        .into_iter()
        .filter_map(raw_to_profile)
        .collect())
}

fn normalize_saved_profile(mut profile: AgentProfile) -> Option<AgentProfile> {
    profile.id = profile.id.trim().to_string();
    if profile.id.is_empty() {
        return None;
    }
    profile.label = profile.label.trim().to_string();
    if profile.label.is_empty() {
        profile.label = profile.id.clone();
    }
    profile.agent = normalize_agent(&profile.agent);
    profile.command = profile.command.trim().to_string();
    if profile.command.is_empty() {
        profile.command = default_command_for_agent(&profile.agent);
    }
    profile.args = profile
        .args
        .into_iter()
        .map(|arg| arg.trim().to_string())
        .filter(|arg| !arg.is_empty())
        .collect();
    profile.model = trim_optional(profile.model);
    profile.reasoning = trim_optional(profile.reasoning);
    profile.mode = trim_optional(profile.mode);
    profile.provider = trim_optional(profile.provider);
    profile.endpoint = trim_optional(profile.endpoint);
    profile.description = trim_optional(profile.description);
    profile.role_preference = trim_optional(profile.role_preference);
    profile.local = profile.local
        || profile
            .provider
            .as_deref()
            .map(is_local_provider)
            .unwrap_or(false)
        || profile
            .endpoint
            .as_deref()
            .map(is_local_endpoint)
            .unwrap_or(false);
    Some(profile)
}

pub fn resolve_profile_for_role(
    state: &AppState,
    workspace_id: Option<&str>,
    role: &str,
    requested_profile_id: Option<&str>,
) -> Result<AgentProfile, String> {
    if let Some(requested_profile_id) = requested_profile_id {
        let profile = resolve_agent_profile(state, workspace_id, Some(requested_profile_id), None)?;
        if profile.agent == "shell" {
            return Err(
                "Shell profile is not eligible for coordinator brain/coder roles".to_string(),
            );
        }
        return Ok(profile);
    }

    let profiles = list_workspace_agent_profiles(state, workspace_id)?;
    let is_eligible = |p: &&AgentProfile| is_coordinator_eligible(p);
    let role = role.to_ascii_lowercase();

    let setting_key = if role == "brain" {
        "coordinator_default_brain_profile_id"
    } else if role == "coder" {
        "coordinator_default_coder_profile_id"
    } else {
        "default_agent_profile_id"
    };
    if let Ok(Some(saved)) = settings_repository::get_value(&state.db, setting_key) {
        if let Some(profile) = profiles
            .iter()
            .find(|p| p.id == saved && is_eligible(p))
            .cloned()
        {
            return Ok(profile);
        }
    }

    if let Some(profile) = profiles
        .iter()
        .filter(is_eligible)
        .find(|p| {
            p.role_preference
                .as_deref()
                .map(|value| value.eq_ignore_ascii_case(&role))
                .unwrap_or(false)
        })
        .cloned()
    {
        return Ok(profile);
    }

    let ordered = profiles.iter().filter(is_eligible).collect::<Vec<_>>();
    ordered
        .first()
        .map(|profile| (*profile).clone())
        .ok_or_else(|| {
            format!(
                "No eligible {} profile found. Add one in Settings → Agent Profiles or .forge/config.json.",
                role
            )
        })
}

fn trim_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn default_profiles() -> Vec<AgentProfile> {
    vec![
        AgentProfile {
            id: "claude-code".to_string(),
            label: "Claude Code".to_string(),
            agent: "claude_code".to_string(),
            command: "claude".to_string(),
            args: vec![],
            model: None,
            reasoning: None,
            mode: Some("act".to_string()),
            provider: None,
            endpoint: None,
            local: false,
            description: Some("Claude Code agent".to_string()),
            skills: vec![],
            templates: vec![],
            role_preference: None,
            coordinator_eligible: Some(true),
        },
        AgentProfile {
            id: "codex".to_string(),
            label: "Codex".to_string(),
            agent: "codex".to_string(),
            command: "codex".to_string(),
            args: vec![],
            model: None,
            reasoning: None,
            mode: Some("act".to_string()),
            provider: Some("openai".to_string()),
            endpoint: None,
            local: false,
            description: Some("Codex CLI agent".to_string()),
            skills: vec![],
            templates: vec![],
            role_preference: None,
            coordinator_eligible: Some(true),
        },
        AgentProfile {
            id: "shell".to_string(),
            label: "Shell".to_string(),
            agent: "shell".to_string(),
            command: std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string()),
            args: vec![],
            model: None,
            reasoning: None,
            mode: None,
            provider: None,
            endpoint: None,
            local: true,
            description: Some("Plain shell utility terminal".to_string()),
            skills: vec![],
            templates: vec![],
            role_preference: None,
            coordinator_eligible: Some(false),
        },
    ]
}

fn normalize_agent(value: &str) -> String {
    match value.trim() {
        "claude" | "claude-code" | "claude_code" => "claude_code".to_string(),
        "codex" => "codex".to_string(),
        "openai" | "openai-api" | "openai_api" => "openai".to_string(),
        "shell" => "shell".to_string(),
        other => other.to_string(),
    }
}

fn default_command_for_agent(agent: &str) -> String {
    match agent {
        "claude_code" => "claude".to_string(),
        "codex" => "codex".to_string(),
        "openai" => "openai".to_string(),
        "shell" => "/bin/zsh".to_string(),
        other => other.to_string(),
    }
}

fn is_local_provider(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "ollama"
            | "llama.cpp"
            | "llama-cpp"
            | "llama_cpp"
            | "lmstudio"
            | "lm-studio"
            | "local"
            | "openai-compatible"
            | "openai_compatible"
    )
}

fn is_local_endpoint(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("localhost") || lower.contains("127.0.0.1") || lower.contains("[::1]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_builtin_agents_and_shell() {
        let ids = default_profiles()
            .into_iter()
            .map(|profile| profile.id)
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&"claude-code".to_string()));
        assert!(ids.contains(&"codex".to_string()));
        assert!(ids.contains(&"shell".to_string()));
    }

    #[test]
    fn normalizes_openai_aliases() {
        assert_eq!(normalize_agent("openai"), "openai");
        assert_eq!(normalize_agent("openai-api"), "openai");
        assert_eq!(normalize_agent("openai_api"), "openai");
    }

    #[test]
    fn raw_local_profile_normalizes_with_local_flag() {
        let profile = raw_to_profile(RawAgentProfile {
            id: Some("local-review".to_string()),
            label: None,
            agent: Some("ollama".to_string()),
            command: None,
            args: vec!["run".to_string(), "qwen2.5-coder".to_string()],
            model: Some("qwen2.5-coder".to_string()),
            reasoning: None,
            mode: Some("review".to_string()),
            provider: Some("ollama".to_string()),
            endpoint: Some("http://127.0.0.1:11434".to_string()),
            local: None,
            description: None,
            skills: vec![],
            templates: vec![],
            role_preference: None,
            coordinator_eligible: None,
        })
        .expect("profile");

        assert_eq!(profile.agent, "ollama");
        assert!(profile.local);
    }

    #[test]
    fn parses_saved_app_profiles_with_local_metadata() {
        let saved = r#"[{"id":" local ","label":"","agent":"lmstudio","command":"","args":[" --model ",""],"provider":"lm-studio","endpoint":"http://localhost:1234/v1","local":false,"skills":[],"templates":[]}]"#;
        let profiles = parse_app_agent_profiles(saved).expect("profiles");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "local");
        assert_eq!(profiles[0].label, "local");
        assert_eq!(profiles[0].agent, "lmstudio");
        assert_eq!(profiles[0].command, "lmstudio");
        assert_eq!(profiles[0].args, vec!["--model"]);
        assert!(profiles[0].local);
    }
}
