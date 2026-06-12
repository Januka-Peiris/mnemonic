use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentCheckItem {
    pub name: String,
    pub binary: String,
    pub status: String,
    pub fix: String,
    pub optional: bool,
    pub path: Option<String>,
}
