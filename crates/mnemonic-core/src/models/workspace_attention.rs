use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceAttention {
    pub workspace_id: String,
    pub status: String,
    pub running_count: i64,
    pub unread_count: i64,
    pub queued_count: i64,
    pub last_event: Option<String>,
    pub last_event_at: Option<String>,
}
