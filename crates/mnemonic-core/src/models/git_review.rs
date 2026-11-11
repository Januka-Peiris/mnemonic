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

/// Which changed files are uncommitted, and the ref committed work is
/// compared against, for splitting a workspace's changes and opening its
/// branch diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceChangeScope {
    /// Paths whose index or working tree differs from HEAD.
    pub uncommitted_paths: std::collections::HashSet<String>,
    /// `origin/<base>` when it exists, else `<base>`; None when neither
    /// resolves to a commit.
    pub base_ref: Option<String>,
    /// Commits on `base_ref` that the workspace's branch does not have yet.
    pub behind_by: u32,
}
