use serde::{Deserialize, Serialize};

pub const COORDINATION_ARTIFACT_KINDS: &[&str] = &[
    "api_diff",
    "schema_change",
    "decision_summary",
    "dependency_note",
    "release_ordering_note",
];

pub const COORDINATION_ARTIFACT_STATUSES: &[&str] = &["draft", "active", "resolved", "dismissed"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CoordinationArtifact {
    pub id: String,
    pub parent_workspace_id: String,
    pub source_workspace_id: String,
    pub target_workspace_id: Option<String>,
    pub artifact_kind: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateCoordinationArtifactInput {
    pub source_workspace_id: String,
    pub target_workspace_id: Option<String>,
    pub artifact_kind: String,
    pub title: String,
    pub body: String,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCoordinationArtifactInput {
    pub id: String,
    pub target_workspace_id: Option<String>,
    pub artifact_kind: String,
    pub title: String,
    pub body: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCoordinationArtifactStatusInput {
    pub id: String,
    pub status: String,
}

pub fn is_valid_coordination_artifact_kind(kind: &str) -> bool {
    COORDINATION_ARTIFACT_KINDS.contains(&kind)
}

pub fn is_valid_coordination_artifact_status(status: &str) -> bool {
    COORDINATION_ARTIFACT_STATUSES.contains(&status)
}
