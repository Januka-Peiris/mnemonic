use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSchedulerJob {
    pub id: String,
    pub workspace_id: String,
    pub kind: String,
    pub interval_seconds: u64,
    pub next_run_at: u64,
    pub enabled: bool,
    pub jitter_pct: u64,
}
