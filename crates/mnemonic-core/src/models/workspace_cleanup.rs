use serde::{Deserialize, Serialize};

use crate::models::{WorkspaceHealth, WorkspacePort};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupWorkspaceInput {
    pub workspace_id: String,
    pub kill_ports: Option<bool>,
    pub remove_managed_worktree: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupWorkspaceResult {
    pub workspace_id: String,
    pub stopped_sessions: u32,
    pub teardown_sessions: u32,
    pub remaining_ports: Vec<WorkspacePort>,
    pub killed_ports: u32,
    pub health: Option<WorkspaceHealth>,
    pub workspace_deleted: bool,
    pub warnings: Vec<String>,
}
