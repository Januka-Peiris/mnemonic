use serde::{Deserialize, Serialize};

use crate::models::{
    WorkspaceChangedFile, WorkspaceFileDiff, WorkspaceMergeReadiness, WorkspaceReviewSummary,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileReviewState {
    pub workspace_id: String,
    pub path: String,
    pub status: String,
    pub reviewed_at: Option<String>,
    pub reviewed_by: String,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCockpitFile {
    pub file: WorkspaceChangedFile,
    pub review: Option<WorkspaceFileReviewState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePrComment {
    pub workspace_id: String,
    pub provider: String,
    pub comment_id: String,
    pub author: String,
    pub body: String,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub url: Option<String>,
    pub state: String,
    pub created_at: Option<String>,
    pub resolved_at: Option<String>,
    pub comment_node_id: Option<String>,
    pub thread_id: Option<String>,
    pub review_id: Option<u64>,
    pub thread_resolved: bool,
    pub thread_outdated: bool,
    pub thread_resolvable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReviewCockpit {
    pub workspace_id: String,
    pub files: Vec<ReviewCockpitFile>,
    pub selected_diff: Option<WorkspaceFileDiff>,
    pub review_summary: Option<WorkspaceReviewSummary>,
    pub merge_readiness: Option<WorkspaceMergeReadiness>,
    pub pr_comments: Vec<WorkspacePrComment>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkWorkspaceFileReviewedInput {
    pub workspace_id: String,
    pub path: String,
    pub reviewed: bool,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueReviewAgentPromptInput {
    pub workspace_id: String,
    pub path: Option<String>,
    pub comment_id: Option<String>,
    pub action: String,
    pub profile_id: Option<String>,
}
