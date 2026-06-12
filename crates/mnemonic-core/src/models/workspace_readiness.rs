use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReadiness {
    pub workspace_id: String,
    pub status: String,
    pub summary: String,
    pub agent_status: String,
    pub terminal_health: String,
    pub changed_files: u32,
    pub reviewed_files: u32,
    pub test_status: String,
    pub pr_comment_count: u32,
    pub port_count: u32,
}
