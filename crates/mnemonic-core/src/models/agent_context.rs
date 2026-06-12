use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentContextWorktree {
    pub repo_id: String,
    pub repo_name: String,
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceAgentContext {
    pub workspace_id: String,
    pub primary_path: String,
    pub linked_worktrees: Vec<AgentContextWorktree>,
    pub prompt_preamble: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceContextPreview {
    pub workspace_id: String,
    pub repo_root: String,
    pub status: String,
    pub default_branch: String,
    pub ref_name: String,
    pub commit_hash: String,
    pub generated_at: Option<String>,
    pub approx_chars: usize,
    /// `0` when Mnemonic does not apply a character budget to repo context.
    pub max_chars: usize,
    pub trimmed: bool,
    pub items: Vec<WorkspaceContextItem>,
    pub prompt_context: String,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceContextItem {
    pub label: String,
    pub path: Option<String>,
    pub kind: String,
    pub priority: u8,
    pub chars: usize,
    pub included: bool,
    pub trimmed: bool,
}
