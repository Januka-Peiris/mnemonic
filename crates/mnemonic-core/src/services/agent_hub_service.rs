use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::db::Database;
use crate::repositories::agent_started_chat_repository;
use crate::services::mcp_http::{optional_str, optional_u64, required_str};
use crate::services::workspace_service::run_git;

pub const MAX_BUSY_CHILDREN: usize = 8;
pub const MAX_START_DEPTH: u32 = 1;
pub const REPLY_CHARS: usize = 6000;
pub const WAIT_DEFAULT_SECONDS: u64 = 1200;
pub const WAIT_MAX_SECONDS: u64 = 3600;
pub const DEDUPE_WINDOW: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatAgentChoice {
    Claude,
    Codex,
}

impl ChatAgentChoice {
    pub fn agent_id(&self) -> &'static str {
        match self {
            Self::Claude => "claude-acp",
            Self::Codex => "codex-acp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatPlace {
    Existing {
        workspace: String,
    },
    New {
        repo: String,
        branch: Option<String>,
        base: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartChatRequest {
    pub task: String,
    pub agent: ChatAgentChoice,
    pub place: ChatPlace,
    pub title: Option<String>,
}

fn non_empty<'a>(arguments: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    optional_str(arguments, key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub fn parse_start_chat(arguments: &serde_json::Value) -> Result<StartChatRequest, String> {
    let task = required_str(arguments, "task")?.trim();
    if task.is_empty() {
        return Err("task is empty: brief the new chat fully".to_string());
    }
    let agent = match non_empty(arguments, "agent")
        .map(str::to_lowercase)
        .as_deref()
    {
        None | Some("claude") => ChatAgentChoice::Claude,
        Some("codex") => ChatAgentChoice::Codex,
        Some(other) => return Err(format!("unknown agent '{other}': use claude or codex")),
    };
    let place = match (
        non_empty(arguments, "workspace"),
        non_empty(arguments, "repo"),
    ) {
        (Some(workspace), None) => ChatPlace::Existing {
            workspace: workspace.to_string(),
        },
        (None, Some(repo)) => ChatPlace::New {
            repo: repo.to_string(),
            branch: non_empty(arguments, "branch").map(str::to_string),
            base: non_empty(arguments, "base").map(str::to_string),
        },
        (Some(_), Some(_)) => {
            return Err("give either workspace or repo, not both".to_string());
        }
        (None, None) => {
            return Err(
                "say where the chat runs: workspace (an existing one) or repo (a new one)"
                    .to_string(),
            );
        }
    };
    Ok(StartChatRequest {
        task: task.to_string(),
        agent,
        place,
        title: non_empty(arguments, "title").map(str::to_string),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitRequest {
    pub chats: Vec<String>,
    pub any: bool,
    pub timeout: Duration,
}

pub fn parse_wait(arguments: &serde_json::Value) -> Result<WaitRequest, String> {
    let chats: Vec<String> = arguments
        .get("chats")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "missing or invalid argument: chats".to_string())?
        .iter()
        .filter_map(|value| value.as_str())
        .map(str::to_string)
        .collect();
    if chats.is_empty() {
        return Err("chats is empty: name at least one chat to wait for".to_string());
    }
    let seconds = optional_u64(arguments, "timeout_seconds")
        .unwrap_or(WAIT_DEFAULT_SECONDS)
        .min(WAIT_MAX_SECONDS);
    Ok(WaitRequest {
        chats,
        any: crate::services::mcp_http::flag(arguments, "any"),
        timeout: Duration::from_secs(seconds),
    })
}

#[derive(Debug, Clone, Copy)]
pub struct Named<'a> {
    pub id: &'a str,
    pub name: &'a str,
}

/// An id always wins over a name; a name must match exactly one item.
pub fn resolve<'a>(kind: &str, items: &[Named<'a>], text: &str) -> Result<&'a str, String> {
    let text = text.trim();
    if let Some(item) = items.iter().find(|item| item.id == text) {
        return Ok(item.id);
    }
    let matches: Vec<&Named<'a>> = items.iter().filter(|item| item.name == text).collect();
    match matches.as_slice() {
        [only] => Ok(only.id),
        [] => Err(format!("No {kind} found matching '{text}'")),
        several => {
            let ids: Vec<&str> = several.iter().map(|item| item.id).collect();
            Err(format!(
                "More than one {kind} is named '{text}': use an id instead ({})",
                ids.join(", ")
            ))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatState {
    Starting,
    Replying,
    WaitingForApproval,
    Replied,
    Stopped,
    Closed,
}

impl ChatState {
    fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Replying => "replying",
            Self::WaitingForApproval => "waiting for approval from the user",
            Self::Replied => "replied",
            Self::Stopped => "stopped",
            Self::Closed => "closed",
        }
    }

    fn is_busy(&self) -> bool {
        matches!(self, Self::Starting | Self::Replying)
    }
}

#[derive(Debug, Clone)]
pub struct ChatReport {
    pub id: String,
    pub title: String,
    pub agent: String,
    pub state: ChatState,
    pub workspace_path: Option<String>,
    pub branch: Option<String>,
    pub reply: Option<String>,
}

fn cut_reply(reply: &str) -> String {
    let total = reply.chars().count();
    if total <= REPLY_CHARS {
        return reply.to_string();
    }
    let kept: String = reply.chars().take(REPLY_CHARS).collect();
    format!("{kept}\n[reply cut: {REPLY_CHARS} of {total} characters shown]")
}

pub fn render_wait(reports: &[ChatReport], timed_out: bool, typed: &[String]) -> String {
    let mut sections = Vec::new();
    if !typed.is_empty() {
        let mut section = String::from("The user wrote to you while you were waiting:\n");
        for message in typed {
            section.push_str(&format!("- {message}\n"));
        }
        section.push_str("Answer them now, before waiting again.");
        sections.push(section);
    }
    for report in reports {
        let mut section = format!(
            "Chat {} \"{}\" ({}): {}",
            report.id,
            report.title,
            report.agent,
            report.state.label()
        );
        if let Some(path) = &report.workspace_path {
            section.push_str(&format!("\nWorkspace: {path}"));
        }
        if let Some(branch) = &report.branch {
            section.push_str(&format!("\nBranch: {branch}"));
        }
        if report.state != ChatState::Closed {
            if let Some(reply) = &report.reply {
                section.push_str(&format!("\nReply:\n{}", cut_reply(reply)));
            }
        }
        sections.push(section);
    }
    if timed_out {
        let pending: Vec<&str> = reports
            .iter()
            .filter(|report| report.state.is_busy())
            .map(|report| report.id.as_str())
            .collect();
        sections.push(format!(
            "Timed out. Still replying: {}. Call `wait` again to keep waiting.",
            pending.join(", ")
        ));
    }
    sections.join("\n\n")
}

pub struct ListedWorkspace {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub branch: String,
    pub path: Option<String>,
    /// id, title, agent, state
    pub chats: Vec<(String, String, String, ChatState)>,
}

/// `repos` are (name, default base branch) pairs.
pub fn render_list(
    repos: &[(String, String)],
    workspaces: &[ListedWorkspace],
    caller_chat_id: &str,
    caller_workspace_id: &str,
    started: &[String],
) -> String {
    let mut lines = vec!["Repositories (pass the name as `repo` to start_chat):".to_string()];
    if repos.is_empty() {
        lines.push("  none".to_string());
    }
    for (name, default_branch) in repos {
        lines.push(format!("  {name} (default branch {default_branch})"));
    }
    lines.push(String::new());
    lines.push("Workspaces:".to_string());
    if workspaces.is_empty() {
        lines.push("  none".to_string());
    }
    for workspace in workspaces {
        lines.push(format!(
            "  {} \"{}\" ({}, branch {}){}{}",
            workspace.id,
            workspace.name,
            workspace.repo,
            workspace.branch,
            workspace
                .path
                .as_ref()
                .map(|path| format!(" at {path}"))
                .unwrap_or_default(),
            if workspace.id == caller_workspace_id {
                " [your workspace]"
            } else {
                ""
            }
        ));
        for (id, title, agent, state) in &workspace.chats {
            let mut marks = String::new();
            if id == caller_chat_id {
                marks.push_str(" [you]");
            }
            if started.iter().any(|started_id| started_id == id) {
                marks.push_str(" [started by you]");
            }
            lines.push(format!(
                "    chat {id} \"{title}\" ({agent}): {}{marks}",
                state.label()
            ));
        }
    }
    lines.join("\n")
}

pub fn tool_definitions(can_start: bool) -> serde_json::Value {
    let string =
        |description: &str| serde_json::json!({"type": "string", "description": description});
    let mut tools = Vec::new();
    if can_start {
        tools.push(serde_json::json!({
            "name": "start_chat",
            "description": "Start a chat with another agent, in an existing workspace or in a new one on a repository. The new chat starts with none of your context, so brief it fully in `task`: the goal, the files and constraints, and what to report back. This returns at once with the chat id; collect the reply with `wait`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": string("The full brief for the new chat."),
                    "agent": {"type": "string", "enum": ["claude", "codex"], "description": "Which agent runs the chat. Defaults to claude."},
                    "workspace": string("An existing workspace, by id or exact name. Give this or repo."),
                    "repo": string("A repository name from `list`; a new workspace is made on it. Give this or workspace."),
                    "branch": string("With repo: the branch to use. An existing branch is checked out; a new name is created."),
                    "base": string("With repo: the branch a new branch starts from."),
                    "title": string("A short title for the chat.")
                },
                "required": ["task"]
            }
        }));
    }
    tools.push(serde_json::json!({
        "name": "send",
        "description": "Send a message to a chat you started, or to another open chat. Returns at once; collect the reply with `wait`.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "chat": string("The chat id or exact title."),
                "text": string("The message. The chat has none of your context, so say what you need in full.")
            },
            "required": ["chat", "text"]
        }
    }));
    tools.push(serde_json::json!({
        "name": "wait",
        "description": "Wait for replies from chats. Returns when all of them have replied, or when any has if `any` is true, or when the timeout passes. It returns early when the user writes to you; answer them, then wait again. A long wait may also return before its timeout; if chats are still replying, call `wait` again.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "chats": {"type": "array", "items": {"type": "string"}, "description": "Chat ids or exact titles to wait for."},
                "any": {"type": "boolean", "description": "Return as soon as one chat has replied."},
                "timeout_seconds": {"type": "integer", "description": "How long to wait. Defaults to 1200, at most 3600."}
            },
            "required": ["chats"]
        }
    }));
    tools.push(serde_json::json!({
        "name": "list",
        "description": "List the repositories, workspaces and chats, and which chats you started.",
        "inputSchema": {"type": "object", "properties": {}}
    }));
    tools.push(serde_json::json!({
        "name": "stop_chat",
        "description": "Stop a chat you started while it is replying. The chat stays open and can be sent more messages.",
        "inputSchema": {
            "type": "object",
            "properties": {"chat": string("The chat id or exact title.")},
            "required": ["chat"]
        }
    }));
    tools.push(serde_json::json!({
        "name": "archive_workspace",
        "description": "Archive a workspace that was made for a chat you started, once its work is committed. Refused if it has uncommitted changes or was not made by an agent.",
        "inputSchema": {
            "type": "object",
            "properties": {"workspace": string("The workspace id or exact name.")},
            "required": ["workspace"]
        }
    }));
    serde_json::Value::Array(tools)
}

#[derive(Default)]
pub struct RecentStarts {
    starts: HashMap<(String, String), (Instant, String)>,
}

fn normalized_request(request: &StartChatRequest) -> String {
    format!(
        "{}|{}|{:?}|{}",
        request.task.trim(),
        request.agent.agent_id(),
        request.place,
        request.title.as_deref().unwrap_or("")
    )
}

impl RecentStarts {
    pub fn earlier(
        &mut self,
        caller: &str,
        request: &StartChatRequest,
        now: Instant,
    ) -> Option<String> {
        self.starts
            .retain(|_, (when, _)| now.saturating_duration_since(*when) < DEDUPE_WINDOW);
        self.starts
            .get(&(caller.to_string(), normalized_request(request)))
            .map(|(_, chat_id)| chat_id.clone())
    }

    pub fn remember(
        &mut self,
        caller: &str,
        request: &StartChatRequest,
        chat_id: &str,
        now: Instant,
    ) {
        self.starts.insert(
            (caller.to_string(), normalized_request(request)),
            (now, chat_id.to_string()),
        );
    }
}

pub fn check_can_start(
    db: &Database,
    caller_chat_id: &str,
    busy_children: usize,
) -> Result<(), String> {
    if agent_started_chat_repository::depth(db, caller_chat_id)? > MAX_START_DEPTH {
        return Err(
            "This chat was started by another agent's chat and cannot start chats itself."
                .to_string(),
        );
    }
    if busy_children >= MAX_BUSY_CHILDREN {
        return Err(format!(
            "You already have {MAX_BUSY_CHILDREN} chats replying, which is the limit. Wait for some to finish first."
        ));
    }
    Ok(())
}

pub fn check_can_stop(db: &Database, chat_id: &str) -> Result<(), String> {
    match agent_started_chat_repository::get(db, chat_id)? {
        Some(_) => Ok(()),
        None => Err("Only chats started by an agent can be stopped here.".to_string()),
    }
}

pub fn check_can_archive(
    db: &Database,
    workspace_id: &str,
    worktree_path: &Path,
) -> Result<(), String> {
    if !agent_started_chat_repository::workspace_made_by_agent(db, workspace_id)? {
        return Err("Only workspaces made by an agent can be archived here.".to_string());
    }
    if !run_git(worktree_path, &["status", "--porcelain"])?.is_empty() {
        return Err(
            "The workspace has uncommitted changes. Commit or discard them before archiving."
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AgentStartedChat;
    use crate::repositories::agent_started_chat_repository;
    use serde_json::json;

    fn link(
        db: &Database,
        chat: &str,
        by: &str,
        workspace: &str,
        made: bool,
    ) -> Result<(), String> {
        agent_started_chat_repository::insert(
            db,
            &AgentStartedChat {
                chat_id: chat.to_string(),
                started_by_chat_id: by.to_string(),
                workspace_id: workspace.to_string(),
                made_workspace: made,
            },
        )
    }

    fn request(task: &str) -> StartChatRequest {
        StartChatRequest {
            task: task.to_string(),
            agent: ChatAgentChoice::Claude,
            place: ChatPlace::Existing {
                workspace: "ws".to_string(),
            },
            title: None,
        }
    }

    fn report(id: &str, state: ChatState, reply: Option<String>) -> ChatReport {
        ChatReport {
            id: id.to_string(),
            title: format!("title-{id}"),
            agent: "claude-acp".to_string(),
            state,
            workspace_path: None,
            branch: None,
            reply,
        }
    }

    #[test]
    fn parse_start_chat_needs_a_place() {
        assert!(parse_start_chat(&json!({"task": "t"})).is_err());
        assert!(parse_start_chat(&json!({"task": "t", "workspace": "a", "repo": "b"})).is_err());
        let error = parse_start_chat(&json!({"task": "t", "repo": "b", "agent": "gemini"}))
            .err()
            .unwrap_or_default();
        assert!(
            error.contains("claude") && error.contains("codex"),
            "{error}"
        );
        let parsed = parse_start_chat(&json!({"task": "t", "repo": "b", "agent": "codex"}));
        assert!(matches!(
            parsed,
            Ok(StartChatRequest {
                agent: ChatAgentChoice::Codex,
                place: ChatPlace::New { .. },
                ..
            })
        ));
    }

    #[test]
    fn parse_wait_caps_the_timeout() -> Result<(), String> {
        let capped = parse_wait(&json!({"chats": ["a"], "timeout_seconds": 99999}))?;
        assert_eq!(capped.timeout, Duration::from_secs(3600));
        let default = parse_wait(&json!({"chats": ["a"]}))?;
        assert_eq!(default.timeout, Duration::from_secs(1200));
        assert!(parse_wait(&json!({"chats": []})).is_err());
        Ok(())
    }

    #[test]
    fn resolve_refuses_ambiguous_name() -> Result<(), String> {
        let items = [
            Named {
                id: "id-1",
                name: "api",
            },
            Named {
                id: "id-2",
                name: "api",
            },
            Named {
                id: "api",
                name: "other",
            },
        ];
        let error = resolve("chat", &items[..2], "api")
            .err()
            .unwrap_or_default();
        assert!(error.contains("id-1") && error.contains("id-2"), "{error}");
        assert_eq!(resolve("chat", &items, "api")?, "api");
        let missing = resolve("chat", &items, "nope").err().unwrap_or_default();
        assert!(missing.contains("No chat"), "{missing}");
        Ok(())
    }

    #[test]
    fn render_wait_reports_closed_chat() {
        let closed = render_wait(&[report("c1", ChatState::Closed, None)], false, &[]);
        assert!(closed.contains("closed"), "{closed}");

        let long = "x".repeat(7000);
        let cut = render_wait(&[report("c1", ChatState::Replied, Some(long))], false, &[]);
        assert!(cut.contains(&"x".repeat(6000)));
        assert!(!cut.contains(&"x".repeat(6001)));
        assert!(cut.contains("cut"), "{cut}");

        let typed = render_wait(
            &[report("c1", ChatState::Replied, Some("done".to_string()))],
            false,
            &["please stop".to_string()],
        );
        assert!(
            typed.starts_with("The user wrote to you while you were waiting"),
            "{typed}"
        );
        assert!(typed.find("please stop") < typed.find("done"));
        assert!(typed.contains("before waiting again"), "{typed}");

        let timed_out = render_wait(&[report("c1", ChatState::Replying, None)], true, &[]);
        assert!(
            timed_out.contains("c1") && timed_out.contains("`wait`"),
            "{timed_out}"
        );
    }

    #[test]
    fn start_limits() -> Result<(), String> {
        let db = Database::in_memory()?;
        link(&db, "child", "root", "ws", false)?;
        link(&db, "grandchild", "child", "ws", false)?;
        assert!(check_can_start(&db, "root", 0).is_ok());
        assert!(check_can_start(&db, "child", 7).is_ok());
        let busy = check_can_start(&db, "child", 8).err().unwrap_or_default();
        assert!(busy.contains('8'), "{busy}");
        assert!(check_can_start(&db, "grandchild", 0).is_err());
        let names = |value: serde_json::Value| -> Vec<String> {
            value
                .as_array()
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        assert!(!names(tool_definitions(false)).contains(&"start_chat".to_string()));
        assert!(names(tool_definitions(true)).contains(&"start_chat".to_string()));
        Ok(())
    }

    #[test]
    fn recent_starts_dedupes_within_two_minutes() {
        let mut recent = RecentStarts::default();
        let start = Instant::now();
        recent.remember("caller", &request("task"), "chat-1", start);
        let at = |seconds| start + Duration::from_secs(seconds);
        assert_eq!(
            recent.earlier("caller", &request("task"), at(119)),
            Some("chat-1".to_string())
        );
        assert_eq!(recent.earlier("caller", &request("task"), at(121)), None);
        assert_eq!(recent.earlier("caller", &request("other"), at(10)), None);
    }

    #[test]
    fn archive_rules() -> Result<(), String> {
        let db = Database::in_memory()?;
        let directory = tempfile::tempdir().map_err(|err| err.to_string())?;
        run_git(directory.path(), &["init", "-q"])?;

        assert!(check_can_archive(&db, "ws-user", directory.path()).is_err());

        link(&db, "chat", "root", "ws-agent", true)?;
        std::fs::write(directory.path().join("new.txt"), "x").map_err(|err| err.to_string())?;
        let dirty = check_can_archive(&db, "ws-agent", directory.path())
            .err()
            .unwrap_or_default();
        assert!(dirty.contains("uncommitted changes"), "{dirty}");

        std::fs::remove_file(directory.path().join("new.txt")).map_err(|err| err.to_string())?;
        check_can_archive(&db, "ws-agent", directory.path())?;

        assert!(check_can_stop(&db, "chat").is_ok());
        assert!(check_can_stop(&db, "not-agent-started").is_err());
        Ok(())
    }

    #[test]
    fn render_list_marks_caller_and_shows_default_branches() {
        let workspace =
            |id: &str, chats: Vec<(String, String, String, ChatState)>| ListedWorkspace {
                id: id.to_string(),
                name: format!("name-{id}"),
                repo: "app".to_string(),
                branch: "b".to_string(),
                path: None,
                chats,
            };
        let chat = |id: &str| {
            (
                id.to_string(),
                "t".to_string(),
                "claude-acp".to_string(),
                ChatState::Replied,
            )
        };
        let text = render_list(
            &[("app".to_string(), "develop".to_string())],
            &[
                workspace("ws-mine", vec![chat("me"), chat("kid")]),
                workspace("ws-other", vec![chat("else")]),
            ],
            "me",
            "ws-mine",
            &["kid".to_string()],
        );
        let line = |needle: &str| {
            text.lines()
                .find(|line| line.contains(needle))
                .unwrap_or("")
        };
        assert!(line("app (").contains("develop"), "{text}");
        assert!(line("ws-mine").contains("[your workspace]"), "{text}");
        assert!(!line("ws-other").contains("[your workspace]"), "{text}");
        assert!(line("chat me").contains("[you]"), "{text}");
        assert!(line("chat kid").contains("[started by you]"), "{text}");
        assert!(!line("chat else").contains('['), "{text}");
    }
}
