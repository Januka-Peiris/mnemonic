use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMemory {
    pub id: String,
    pub workspace_id: Option<String>,
    pub scope: String,
    pub key: String,
    pub value: String,
    pub origin: String,
    pub status: String,
    pub confidence: f64,
    pub source_task_run_id: Option<String>,
    pub source_label: Option<String>,
    pub source_detail: Option<String>,
    pub last_used_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetAgentMemoryInput {
    pub workspace_id: Option<String>,
    pub scope: Option<String>,
    pub key: String,
    pub value: String,
    pub origin: Option<String>,
    pub status: Option<String>,
    pub confidence: Option<f64>,
    pub source_task_run_id: Option<String>,
    pub source_label: Option<String>,
    pub source_detail: Option<String>,
    pub last_used_at: Option<String>,
}
