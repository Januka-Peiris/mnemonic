use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePort {
    pub port: u16,
    pub pid: u32,
    pub command: String,
    pub user: Option<String>,
    pub protocol: String,
    pub address: String,
    pub cwd: Option<String>,
    pub workspace_matched: bool,
}
