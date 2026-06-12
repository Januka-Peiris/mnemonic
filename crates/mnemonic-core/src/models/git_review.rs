use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceChangedFile {
    pub workspace_id: String,
    pub path: String,
    pub old_path: Option<String>,
    pub status: String,
    pub staged: bool,
    pub unstaged: bool,
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileDiff {
    pub workspace_id: String,
    pub path: String,
    pub old_path: Option<String>,
    pub status: String,
    pub diff: String,
    pub is_binary: bool,
    pub source: String,
}
