use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCheckpoint {
    pub workspace_id: String,
    pub reference: String,
    pub short_oid: String,
    pub created_at: String,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCheckpointDiff {
    pub workspace_id: String,
    pub reference: String,
    pub diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCheckpointRestorePlan {
    pub workspace_id: String,
    pub reference: String,
    pub current_dirty: bool,
    pub changed_file_count: usize,
    pub checkpoint_file_count: usize,
    pub warnings: Vec<String>,
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCheckpointRestoreResult {
    pub workspace_id: String,
    pub reference: String,
    pub applied: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCheckpointDeleteResult {
    pub workspace_id: String,
    pub reference: String,
    pub deleted: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCheckpointBranchResult {
    pub workspace_id: String,
    pub reference: String,
    pub branch: String,
    pub created: bool,
    pub message: String,
}
