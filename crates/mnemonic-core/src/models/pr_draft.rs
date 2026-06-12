use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePrDraft {
    pub workspace_id: String,
    pub title: String,
    pub summary: String,
    pub key_changes: Vec<String>,
    pub risks: Vec<String>,
    pub testing_notes: Vec<String>,
    pub generated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePrResult {
    pub workspace_id: String,
    pub pr_url: String,
    pub pr_number: i64,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePrStatus {
    pub workspace_id: String,
    pub found: bool,
    pub number: Option<i64>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub state: Option<String>,
    pub is_draft: bool,
    pub review_decision: Option<String>,
    pub mergeable: Option<String>,
    pub merge_state_status: Option<String>,
    pub checks_summary: String,
    pub checks: Vec<WorkspacePrCheck>,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePrCheck {
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub url: Option<String>,
}
