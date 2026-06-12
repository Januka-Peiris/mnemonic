use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentProfile {
    pub id: String,
    pub label: String,
    pub agent: String,
    pub command: String,
    pub args: Vec<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub mode: Option<String>,
    pub provider: Option<String>,
    pub endpoint: Option<String>,
    pub local: bool,
    pub description: Option<String>,
    pub skills: Vec<String>,
    pub templates: Vec<String>,
    #[serde(default)]
    pub role_preference: Option<String>,
    #[serde(default)]
    pub coordinator_eligible: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawAgentProfile {
    pub id: Option<String>,
    pub label: Option<String>,
    pub agent: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub mode: Option<String>,
    pub provider: Option<String>,
    pub endpoint: Option<String>,
    #[serde(default)]
    pub local: Option<bool>,
    pub description: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub templates: Vec<String>,
    #[serde(default)]
    pub role_preference: Option<String>,
    #[serde(default)]
    pub coordinator_eligible: Option<bool>,
}
