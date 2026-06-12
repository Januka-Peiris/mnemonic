use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceConflict {
    pub workspace_id_a: String,
    pub workspace_id_b: String,
    pub shared_files: Vec<String>,
    pub file_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceConflicts {
    pub conflicts: Vec<WorkspaceConflict>,
    /// Deduplicated list of workspace IDs that appear in at least one conflict pair.
    pub conflicting_workspace_ids: Vec<String>,
}
