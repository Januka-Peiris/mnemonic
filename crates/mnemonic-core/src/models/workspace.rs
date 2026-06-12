use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFile {
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchHealth {
    pub ahead_by: u32,
    pub behind_by: u32,
    pub merge_risk: String,
    pub last_rebase: String,
    pub base_branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionSummary {
    pub id: String,
    pub agent: String,
    pub status: String,
    pub model: String,
    pub token_count: u32,
    pub estimated_cost: String,
    pub last_message: String,
    pub started_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSummary {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub branch: String,
    pub agent: String,
    pub status: String,
    pub current_step: String,
    pub completed_steps: Vec<String>,
    pub changed_files: Vec<ChangedFile>,
    pub last_updated: String,
    pub pr_status: Option<String>,
    pub pr_number: Option<u32>,
    pub description: String,
    pub current_task: String,
    pub branch_health: BranchHealth,
    pub agent_session: AgentSessionSummary,
    pub repository_id: Option<String>,
    pub repository_path: Option<String>,
    pub selected_branch: Option<String>,
    pub selected_worktree_id: Option<String>,
    pub selected_worktree_path: Option<String>,
    pub workspace_root_path: Option<String>,
    pub worktree_managed_by_forge: bool,
    pub workspace_source: String,
    pub parent_workspace_id: Option<String>,
    pub source_workspace_id: Option<String>,
    pub derived_from_branch: Option<String>,
    pub linked_worktrees: Vec<LinkedWorktreeRef>,
    pub cost_limit_usd: Option<f64>,
    pub run_tests_on_create: bool,
    pub create_pr_on_complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDetail {
    #[serde(flatten)]
    pub summary: WorkspaceSummary,
    pub worktree_path: String,
    pub base_branch: String,
    pub recent_events: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceInput {
    pub name: String,
    pub repo: String,
    pub base_branch: String,
    pub branch: Option<String>,
    pub agent: String,
    pub task_prompt: String,
    pub open_in_cursor: bool,
    pub run_tests: bool,
    pub create_pr: bool,
    pub repository_id: Option<String>,
    pub selected_worktree_id: Option<String>,
    pub selected_branch: Option<String>,
    pub parent_workspace_id: Option<String>,
    pub source_workspace_id: Option<String>,
    pub derived_from_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateChildWorkspaceInput {
    pub parent_workspace_id: String,
    pub name: String,
    pub branch: Option<String>,
    pub agent: Option<String>,
    pub task_prompt: Option<String>,
    pub open_in_cursor: Option<bool>,
    pub run_tests: Option<bool>,
    pub create_pr: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedWorktreeRef {
    pub worktree_id: String,
    pub repo_id: String,
    pub repo_name: String,
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachLinkedWorktreeInput {
    pub workspace_id: String,
    pub worktree_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryWorkspaceOptions {
    pub repository: crate::models::DiscoveredRepository,
    pub branches: Vec<String>,
}
