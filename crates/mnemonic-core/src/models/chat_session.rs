use serde::{Deserialize, Serialize};

/// A chat session (an ACP agent conversation) shown as a tab in a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSessionRecord {
    pub id: String,
    pub workspace_id: String,
    /// ACP agent server id, e.g. "claude-acp" or "codex-acp".
    pub agent: String,
    pub title: String,
    /// Set once the agent has created its session, so the chat can be
    /// resumed after a restart.
    pub acp_session_id: Option<String>,
    pub is_review: bool,
    /// The chat a workspace was created with. Workspaces that have one open
    /// on their chats instead of starting the main claude CLI.
    pub is_primary: bool,
    pub position: i64,
}

/// Records that an agent, rather than the user, started a chat. Kept after
/// the chat closes so limits and cleanup rights can still be worked out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStartedChat {
    pub chat_id: String,
    pub started_by_chat_id: String,
    pub workspace_id: String,
    /// True when the agent created the workspace for this chat.
    pub made_workspace: bool,
}
