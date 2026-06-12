use serde::{Deserialize, Serialize};

/// A single decision made by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorAction {
    /// "send_prompt" | "notify" | "idle"
    pub action: String,
    pub workspace_id: Option<String>,
    /// For send_prompt: text sent to the agent terminal.
    pub prompt: Option<String>,
    /// For notify: shown as a toast to the user.
    pub message: Option<String>,
}

/// Returned by get_orchestrator_status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorStatus {
    pub enabled: bool,
    pub model: String,
    pub last_run_at: Option<String>,
    pub last_actions: Vec<OrchestratorAction>,
}
