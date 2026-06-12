use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalLlmModel {
    pub provider: String,
    pub name: String,
    pub size: Option<String>,
    pub modified: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalLlmProfileDiagnostic {
    pub status: String,
    pub summary: String,
    pub command_preview: String,
    pub checks: Vec<LocalLlmProfileDiagnosticCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalLlmProfileDiagnosticCheck {
    pub name: String,
    pub status: String,
    pub message: String,
}
