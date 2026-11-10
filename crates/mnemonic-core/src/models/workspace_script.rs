use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::models::agent_profile::RawAgentProfile;
use crate::models::{AgentProfile, MnemonicRepositoryRelationshipConfig};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct MnemonicWorkspaceConfig {
    pub exists: bool,
    pub path: Option<String>,
    pub setup: Vec<String>,
    pub run: Vec<String>,
    pub teardown: Vec<String>,
    pub hooks: MnemonicWorkspaceHooks,
    pub agent_profiles: Vec<AgentProfile>,
    pub mcp_servers: Vec<MnemonicMcpServerConfig>,
    pub mcp_warnings: Vec<String>,
    pub repository_relationships: Vec<MnemonicRepositoryRelationshipConfig>,
    pub repository_relationship_warnings: Vec<String>,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct MnemonicWorkspaceHooks {
    pub pre_run: Vec<String>,
    pub post_run: Vec<String>,
    pub pre_tool: Vec<String>,
    pub post_tool: Vec<String>,
    pub pre_ship: Vec<String>,
    pub post_ship: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MnemonicMcpServerConfig {
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
pub(crate) struct RawMnemonicWorkspaceConfig {
    #[serde(default)]
    pub setup: Vec<String>,
    #[serde(default)]
    pub run: Vec<String>,
    #[serde(default)]
    pub teardown: Vec<String>,
    #[serde(default)]
    pub hooks: RawMnemonicWorkspaceHooks,
    #[serde(default, alias = "agent_profiles")]
    pub agent_profiles: Vec<RawAgentProfile>,
    #[serde(default, alias = "mcpServers", alias = "mcp")]
    pub mcp_servers: Value,
    #[serde(
        default,
        alias = "repositoryRelationships",
        alias = "repository_relationships"
    )]
    pub repository_relationships: Vec<RawMnemonicRepositoryRelationshipConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawMnemonicWorkspaceHooks {
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
pub(crate) struct RawMnemonicRepositoryRelationshipConfig {
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}
