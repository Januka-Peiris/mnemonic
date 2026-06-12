use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewItem {
    pub id: String,
    pub workspace_id: String,
    pub workspace_name: String,
    pub repo: String,
    pub branch: String,
    pub risk: String,
    pub files_changed: u32,
    pub additions: u32,
    pub deletions: u32,
    pub ai_summary: String,
    pub author: String,
    pub created_at: String,
}
