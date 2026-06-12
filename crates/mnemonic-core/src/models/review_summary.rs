use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileReviewInsight {
    pub path: String,
    pub status: String,
    pub risk_level: String,
    pub reasons: Vec<String>,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReviewSummary {
    pub workspace_id: String,
    pub summary: String,
    pub risk_level: String,
    pub risk_reasons: Vec<String>,
    pub files_changed: u32,
    pub files_flagged: u32,
    pub additions: u32,
    pub deletions: u32,
    pub generated_at: String,
    pub file_insights: Vec<FileReviewInsight>,
}
