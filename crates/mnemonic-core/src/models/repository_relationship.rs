use serde::{Deserialize, Serialize};

pub const REPOSITORY_RELATIONSHIP_KINDS: &[&str] = &[
    "frontend_backend",
    "sdk_api",
    "shared_schema",
    "deployment_dependency",
    "event_flow",
    "depends_on",
    "related",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ForgeRepositoryRelationshipConfig {
    pub to: String,
    pub kind: String,
    pub label: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryRelationship {
    pub id: String,
    pub app_relationship_id: Option<String>,
    pub from_repo_id: String,
    pub from_repo_name: String,
    pub to_repo_id: String,
    pub to_repo_name: String,
    pub kind: String,
    pub label: Option<String>,
    pub notes: Option<String>,
    pub sources: Vec<String>,
    pub config_paths: Vec<String>,
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryRelationshipsResult {
    pub relationships: Vec<RepositoryRelationship>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateRepositoryRelationshipInput {
    pub from_repo_id: String,
    pub to_repo_id: String,
    pub kind: String,
    pub label: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRepositoryRelationshipInput {
    pub id: String,
    pub from_repo_id: String,
    pub to_repo_id: String,
    pub kind: String,
    pub label: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SuggestRelevantRepositoriesInput {
    pub source_repo_id: String,
    pub task_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryScopeSuggestion {
    pub repo_id: String,
    pub repo_name: String,
    pub repo_path: String,
    pub score: f32,
    pub selected_by_default: bool,
    pub reasons: Vec<String>,
    pub relationship_kinds: Vec<String>,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RelevantRepositoriesSuggestionResult {
    pub source_repo_id: String,
    pub suggestions: Vec<RepositoryScopeSuggestion>,
    pub warnings: Vec<String>,
}

pub fn is_valid_relationship_kind(kind: &str) -> bool {
    REPOSITORY_RELATIONSHIP_KINDS.contains(&kind)
}
