use serde::{Deserialize, Serialize};

use super::repository::DiscoveredRepository;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub repo_roots: Vec<String>,
    pub discovered_repositories: Vec<DiscoveredRepository>,
    pub has_completed_env_check: bool,
    pub managed_workspaces_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveRepoRootsInput {
    pub repo_roots: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiModelSettings {
    /// Model used by Claude Code coding agents (e.g. "claude-opus-4-6[1m]").
    pub agent_model: String,
    /// Alias for `agent_model`, exposed for explicit provider-aware clients.
    pub claude_agent_model: String,
    /// Model used by Codex coding agents (e.g. "gpt-5.4").
    pub codex_agent_model: String,
    /// Model used by Kimi Code coding agents (e.g. "kimi-for-coding").
    pub kimi_agent_model: String,
    /// Model used by the Opus orchestrator brain (e.g. "claude-opus-4-6").
    pub orchestrator_model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveAiModelSettingsInput {
    pub agent_model: String,
    pub claude_agent_model: String,
    pub codex_agent_model: String,
    pub kimi_agent_model: String,
    pub orchestrator_model: String,
}
