use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredRepository {
    pub id: String,
    pub name: String,
    pub path: String,
    pub current_branch: Option<String>,
    pub head: Option<String>,
    pub is_dirty: bool,
    pub worktrees: Vec<DiscoveredWorktree>,
    pub last_scanned_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredWorktree {
    pub id: String,
    pub repo_id: String,
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub is_dirty: bool,
    pub is_detached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanRepositoriesResult {
    pub repo_roots: Vec<String>,
    pub repositories: Vec<DiscoveredRepository>,
    pub scanned_at: String,
    pub warnings: Vec<String>,
}
