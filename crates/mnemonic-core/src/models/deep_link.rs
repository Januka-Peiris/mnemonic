use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OpenDeepLinkInput {
    pub url: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub prompt: Option<String>,
    pub agent: Option<String>,
    pub base_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenDeepLinkResult {
    pub workspace_id: String,
    pub created: bool,
    pub prompt_sent: bool,
}
