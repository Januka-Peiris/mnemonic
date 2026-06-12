use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityItem {
    pub id: String,
    pub workspace_id: Option<String>,
    pub repo: String,
    pub branch: Option<String>,
    pub event: String,
    pub level: String,
    pub details: Option<String>,
    pub timestamp: String,
}
