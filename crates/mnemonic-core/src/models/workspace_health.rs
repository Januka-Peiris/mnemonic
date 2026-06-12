use serde::{Deserialize, Serialize};

use crate::models::WorkspacePort;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTerminalHealth {
    pub session_id: String,
    pub title: String,
    pub kind: String,
    pub profile: String,
    pub status: String,
    pub backend: String,
    pub attached: bool,
    pub stale: bool,
    pub recovery_status: String,
    pub last_output_at: Option<String>,
    pub recommended_action: String,
    /// Unix timestamp (seconds) when the agent was first detected as having no output.
    /// Only set for running agent sessions silent for > STUCK_THRESHOLD_SECS.
    pub stuck_since: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyWorkspaceSessionRecoveryInput {
    pub workspace_id: String,
    pub session_id: String,
    pub action: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceHealth {
    pub workspace_id: String,
    pub status: String,
    pub terminals: Vec<WorkspaceTerminalHealth>,
    pub ports: Vec<WorkspacePort>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSessionRecoveryResult {
    pub workspace_id: String,
    pub closed_sessions: u32,
    pub skipped_sessions: u32,
    pub actions: Vec<WorkspaceSessionRecoveryAction>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSessionRecoveryAction {
    pub session_id: String,
    pub title: String,
    pub action: String,
    pub reason: String,
}
