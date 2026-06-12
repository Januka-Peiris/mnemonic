use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceHookCommand {
    pub id: String,
    pub hook_kind: String,
    pub phase: String,
    pub label: String,
    pub command: String,
    pub safety: crate::services::command_safety_service::CommandSafetyResult,
    pub will_block_when_risky: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceHookEvent {
    pub id: String,
    pub category: String,
    pub label: Option<String>,
    pub event: String,
    pub status: String,
    pub level: String,
    pub detail: Option<String>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceHookInspector {
    pub workspace_id: String,
    pub config_path: Option<String>,
    pub risky_scripts_enabled: bool,
    pub commands: Vec<WorkspaceHookCommand>,
    pub recent_events: Vec<WorkspaceHookEvent>,
}
