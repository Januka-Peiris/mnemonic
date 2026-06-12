use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::models::agent_profile::RawAgentProfile;
use crate::models::{AgentProfile, ForgeRepositoryRelationshipConfig};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ForgeWorkspaceConfig {
    pub exists: bool,
    pub path: Option<String>,
    pub setup: Vec<String>,
    pub run: Vec<String>,
    pub teardown: Vec<String>,
    pub hooks: ForgeWorkspaceHooks,
    pub agent_profiles: Vec<AgentProfile>,
    pub mcp_servers: Vec<ForgeMcpServerConfig>,
    pub mcp_warnings: Vec<String>,
    pub repository_relationships: Vec<ForgeRepositoryRelationshipConfig>,
    pub repository_relationship_warnings: Vec<String>,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ForgeWorkspaceHooks {
    pub pre_run: Vec<String>,
    pub post_run: Vec<String>,
    pub pre_tool: Vec<String>,
    pub post_tool: Vec<String>,
    pub pre_ship: Vec<String>,
    pub post_ship: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ForgeMcpServerConfig {
    pub id: String,
    pub transport: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawForgeWorkspaceConfig {
    #[serde(default)]
    pub setup: Vec<String>,
    #[serde(default)]
    pub run: Vec<String>,
    #[serde(default)]
    pub teardown: Vec<String>,
    #[serde(default)]
    pub hooks: RawForgeWorkspaceHooks,
    #[serde(default, alias = "agent_profiles")]
    pub agent_profiles: Vec<RawAgentProfile>,
    #[serde(default, alias = "mcpServers", alias = "mcp")]
    pub mcp_servers: Value,
    #[serde(
        default,
        alias = "repositoryRelationships",
        alias = "repository_relationships"
    )]
    pub repository_relationships: Vec<RawForgeRepositoryRelationshipConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawForgeWorkspaceHooks {
    #[serde(default)]
    pub pre_run: Vec<String>,
    #[serde(default)]
    pub post_run: Vec<String>,
    #[serde(default)]
    pub pre_tool: Vec<String>,
    #[serde(default)]
    pub post_tool: Vec<String>,
    #[serde(default)]
    pub pre_ship: Vec<String>,
    #[serde(default)]
    pub post_ship: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawForgeRepositoryRelationshipConfig {
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}
