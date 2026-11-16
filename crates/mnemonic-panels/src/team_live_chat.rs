//! A teammate's chat in a sharer's workspace: Claude runs on the teammate's
//! Mac, but its file and shell tools are the sharer's share server.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use acp_thread::{
    AcpThread, AuthorizationKind, PermissionOptions, SelectedPermissionOutcome, ToolCallStatus,
};
use agent_client_protocol::schema as acp;
use agent_servers::SessionExtras;
use gpui::{App, Entity};
use mnemonic_core::services::team_agent_tools::{AGENT_MCP_SERVER_NAME, GitAuthor};
use mnemonic_core::services::team_share_service::{
    TEAM_GIT_AUTHOR_HEADER, TEAM_PROTOCOL_VERSION, TEAM_REQUEST_HEADER, TeamClient, TeamWorkspace,
    agent_mcp_url,
};
use serde::{Deserialize, Serialize};

use crate::team::once_only_options;

const LIVE_CHATS_FILE: &str = "team_live_chats.json";

/// Claude's own tools that act on files or run commands. They would act on
/// the empty folder on this Mac, so the share server's tools replace them.
const DISALLOWED_BUILT_IN_TOOLS: [&str; 13] = [
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "Bash",
    "BashOutput",
    "KillShell",
    "Monitor",
    "PowerShell",
    "Grep",
    "Glob",
    "LS",
];

const PRE_ALLOWED_TOOLS: [&str; 3] = [
    "mcp__teammate_workspace__Read",
    "mcp__teammate_workspace__Glob",
    "mcp__teammate_workspace__Grep",
];

/// Claude's accept-edits mode only covers its built-in tools, so these are
/// answered here instead.
const TEAMMATE_EDIT_TOOLS: [&str; 2] = [
    "mcp__teammate_workspace__Edit",
    "mcp__teammate_workspace__Write",
];

const ACCEPT_EDITS_MODE_ID: &str = "acceptEdits";

const PROJECT_INSTRUCTION_FILES: [&str; 2] = ["CLAUDE.md", "AGENTS.md"];

/// Read-modify-write of the records file by several views would otherwise
/// lose one view's change.
static LIVE_CHATS_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct LiveChatRecord {
    pub id: String,
    pub host: String,
    pub workspace_id: String,
    pub title: String,
    pub acp_session_id: Option<String>,
}

fn live_chats_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LIVE_CHATS_FILE)
}

fn read_live_chats(data_dir: &Path) -> Result<Vec<LiveChatRecord>, String> {
    let path = live_chats_path(data_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("Couldn't read {}: {err}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map_err(|err| format!("Couldn't parse {}: {err}", path.display()))
}

pub(crate) fn load_live_chats(data_dir: &Path) -> Vec<LiveChatRecord> {
    read_live_chats(data_dir).unwrap_or_else(|err| {
        log::error!("{err}");
        Vec::new()
    })
}

pub(crate) fn save_live_chats(data_dir: &Path, records: &[LiveChatRecord]) -> Result<(), String> {
    std::fs::create_dir_all(data_dir)
        .map_err(|err| format!("Couldn't create {}: {err}", data_dir.display()))?;
    let path = live_chats_path(data_dir);
    let json = serde_json::to_vec_pretty(records)
        .map_err(|err| format!("Couldn't encode the chats: {err}"))?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, json)
        .map_err(|err| format!("Couldn't write {}: {err}", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .map_err(|err| format!("Couldn't write {}: {err}", path.display()))
}

/// Applies `change` to the saved records. A file that can't be read is left
/// alone rather than replaced.
pub(crate) fn update_live_chats(
    data_dir: &Path,
    change: impl FnOnce(&mut Vec<LiveChatRecord>),
) -> Result<(), String> {
    let _guard = LIVE_CHATS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut records = read_live_chats(data_dir)?;
    change(&mut records);
    save_live_chats(data_dir, &records)
}

/// Replaces the record with the same id, or adds it.
pub(crate) fn upsert_live_chat(records: &mut Vec<LiveChatRecord>, record: LiveChatRecord) {
    match records.iter_mut().find(|existing| existing.id == record.id) {
        Some(existing) => *existing = record,
        None => records.push(record),
    }
}

/// The host and workspace id come from the network, so only plain
/// characters reach the path.
fn path_slug(value: &str) -> String {
    let slug: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if slug.is_empty() {
        "_".to_string()
    } else {
        slug
    }
}

/// The chat's working directory on this Mac: an empty folder, so Claude loads
/// none of the sharer's project settings here, and a stable one, so the
/// transcript resumes. The caller creates it.
pub(crate) fn live_chat_dir(data_dir: &Path, host: &str, workspace_id: &str) -> PathBuf {
    data_dir
        .join("team_live")
        .join(path_slug(host))
        .join(path_slug(workspace_id))
}

pub(crate) fn live_session_extras(
    shared: &TeamWorkspace,
    sharer_name: &str,
    base_url: &str,
    git_author: Option<&GitAuthor>,
    instructions: &str,
) -> Option<SessionExtras> {
    if !shared.agents {
        return None;
    }
    let root_path = shared.root_path.as_deref()?;

    let mut headers = vec![acp::HttpHeader::new(
        TEAM_REQUEST_HEADER,
        TEAM_PROTOCOL_VERSION.to_string(),
    )];
    if let Some(author) = git_author {
        headers.push(acp::HttpHeader::new(
            TEAM_GIT_AUTHOR_HEADER,
            format!("{} <{}>", author.name, author.email),
        ));
    }
    let server =
        acp::McpServerHttp::new(AGENT_MCP_SERVER_NAME, agent_mcp_url(base_url, &shared.id))
            .headers(headers);

    let mut prompt = format!(
        "This workspace lives on {sharer_name}'s Mac at {root_path}. Use only the \
         mcp__{AGENT_MCP_SERVER_NAME}__* tools (Read, Write, Edit, Glob, Grep, Bash) for files and \
         commands, with absolute paths under {root_path}. Commands run on {sharer_name}'s Mac, \
         in {root_path}. Your working directory on this Mac is an empty folder; the project \
         isn't there."
    );
    if !instructions.trim().is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(instructions.trim_end());
    }

    let meta = acp::Meta::from_iter([
        (
            "claudeCode".to_string(),
            serde_json::json!({
                "options": {
                    "disallowedTools": DISALLOWED_BUILT_IN_TOOLS,
                    "allowedTools": PRE_ALLOWED_TOOLS,
                }
            }),
        ),
        (
            "systemPrompt".to_string(),
            serde_json::json!({ "append": prompt }),
        ),
    ]);
    Some(SessionExtras {
        mcp_servers: vec![acp::McpServer::Http(server)],
        meta,
    })
}

/// Creates the chats' working directory and builds their session extras,
/// reading this Mac's git identity and, when the sharer is reachable, their
/// instruction files. None when the sharer doesn't let agents work in the
/// workspace. Blocking.
pub(crate) fn prepare_live_session(
    data_dir: &Path,
    host: &str,
    client: &TeamClient,
    shared: &TeamWorkspace,
    sharer_name: &str,
    sharer_reachable: bool,
) -> Result<Option<(PathBuf, SessionExtras)>, String> {
    let Some(root_path) = shared.root_path.as_deref().filter(|_| shared.agents) else {
        return Ok(None);
    };
    let dir = live_chat_dir(data_dir, host, &shared.id);
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("Couldn't create {}: {err}", dir.display()))?;
    let instructions = if sharer_reachable {
        fetch_project_instructions(client, &shared.id, root_path)
    } else {
        String::new()
    };
    let git_author = global_git_author();
    Ok(live_session_extras(
        shared,
        sharer_name,
        client.base_url(),
        git_author.as_ref(),
        &instructions,
    )
    .map(|extras| (dir, extras)))
}

/// The sharer's project instruction files, read through the share server.
/// A file that is missing or can't be read is left out. Blocking.
fn fetch_project_instructions(client: &TeamClient, workspace_id: &str, root_path: &str) -> String {
    let root = root_path.trim_end_matches('/');
    let mut sections = Vec::new();
    for name in PROJECT_INSTRUCTION_FILES {
        let path = format!("{root}/{name}");
        match client.agent_read(workspace_id, &path) {
            Ok(Some(text)) if !text.trim().is_empty() => {
                sections.push(format!("Contents of {path}:\n\n{text}"));
            }
            Ok(_) => {}
            Err(err) => log::warn!("Couldn't read {path} from the teammate's workspace: {err}"),
        }
    }
    sections.join("\n\n")
}

/// This Mac's git identity, sent so commits the agent makes on the sharer's
/// Mac name this teammate. Blocking.
fn global_git_author() -> Option<GitAuthor> {
    let name = global_git_config("user.name")?;
    let email = global_git_config("user.email")?;
    Some(GitAuthor { name, email })
}

#[allow(clippy::disallowed_methods)]
fn global_git_config(key: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["config", "--global", key])
        .output()
        .map_err(|err| log::warn!("Couldn't run git config: {err}"))
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// The allow-once answer for a teammate-workspace edit or write while the
/// chat is in accept-edits mode.
fn accept_edits_answer(
    title: &str,
    mode_id: Option<&str>,
    options: &PermissionOptions,
) -> Option<acp::PermissionOption> {
    if mode_id != Some(ACCEPT_EDITS_MODE_ID) || !TEAMMATE_EDIT_TOOLS.contains(&title) {
        return None;
    }
    once_only_options(options)
        .into_iter()
        .find(|option| option.kind == acp::PermissionOptionKind::AllowOnce)
}

fn current_mode_id(thread: &AcpThread, cx: &App) -> Option<String> {
    let session_id = thread.session_id();
    let connection = thread.connection();
    if let Some(modes) = connection.session_modes(session_id, cx) {
        return Some(modes.current_mode().0.to_string());
    }
    connection
        .session_config_options(session_id, cx)?
        .config_options()
        .into_iter()
        .find(|option| option.category == Some(acp::SessionConfigOptionCategory::Mode))
        .and_then(|option| match option.kind {
            acp::SessionConfigKind::Select(select) => Some(select.current_value.0.to_string()),
            _ => None,
        })
}

/// Answers a waiting edit or write in the sharer's workspace when the chat is
/// in accept-edits mode, as Claude does for its own edit tools.
pub(crate) fn answer_edit_in_accept_edits(
    thread: &Entity<AcpThread>,
    tool_call_id: &acp::ToolCallId,
    cx: &mut App,
) {
    let answer = {
        let thread = thread.read(cx);
        let Some((_, tool_call)) = thread.tool_call(tool_call_id) else {
            return;
        };
        let ToolCallStatus::WaitingForConfirmation {
            options,
            kind: AuthorizationKind::PermissionGrant,
            ..
        } = &tool_call.status
        else {
            return;
        };
        let title = tool_call.label.read(cx).source().to_string();
        accept_edits_answer(&title, current_mode_id(thread, cx).as_deref(), options)
    };
    if let Some(option) = answer {
        thread.update(cx, |thread, cx| {
            thread.authorize_tool_call(
                tool_call_id.clone(),
                SelectedPermissionOutcome::new(option.option_id, option.kind),
                cx,
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_workspace(agents: bool) -> TeamWorkspace {
        serde_json::from_value(serde_json::json!({
            "id": "w1",
            "project": "mnemonic",
            "name": "Fix the sidebar",
            "branch": "jay/fix-sidebar",
            "agents": agents,
            "root_path": agents.then_some("/Users/jay/dev/mnemonic"),
        }))
        .expect("workspace parses")
    }

    #[test]
    fn live_session_extras_swap_claudes_file_and_shell_tools() {
        let author = GitAuthor {
            name: "Dan".to_string(),
            email: "dan@x".to_string(),
        };
        let extras = live_session_extras(
            &shared_workspace(true),
            "Jay",
            "http://jay.ts.net:47231",
            Some(&author),
            "Always run the tests with cargo nextest.",
        )
        .expect("extras when agents are allowed");

        let servers = serde_json::to_value(&extras.mcp_servers).expect("servers serialize");
        assert_eq!(
            servers,
            serde_json::json!([{
                "type": "http",
                "name": "teammate_workspace",
                "url": "http://jay.ts.net:47231/v1/workspaces/w1/agent",
                "headers": [
                    {"name": "X-Mnemonic-Team", "value": "5"},
                    {"name": "X-Mnemonic-Git-Author", "value": "Dan <dan@x>"},
                ],
            }])
        );

        let options = &extras.meta["claudeCode"]["options"];
        assert_eq!(
            options["disallowedTools"],
            serde_json::json!([
                "Read",
                "Write",
                "Edit",
                "MultiEdit",
                "NotebookEdit",
                "Bash",
                "BashOutput",
                "KillShell",
                "Monitor",
                "PowerShell",
                "Grep",
                "Glob",
                "LS"
            ])
        );
        assert_eq!(
            options["allowedTools"],
            serde_json::json!([
                "mcp__teammate_workspace__Read",
                "mcp__teammate_workspace__Glob",
                "mcp__teammate_workspace__Grep"
            ])
        );

        let append = extras.meta["systemPrompt"]["append"]
            .as_str()
            .expect("system prompt addition");
        assert!(append.contains("Jay's Mac"), "{append}");
        assert!(append.contains("/Users/jay/dev/mnemonic"), "{append}");
        assert!(
            append.contains("Always run the tests with cargo nextest."),
            "{append}"
        );
    }

    #[test]
    fn no_extras_when_the_sharer_disallows_agents() {
        let extras = live_session_extras(
            &shared_workspace(false),
            "Jay",
            "http://jay.ts.net:47231",
            None,
            "",
        );
        assert!(extras.is_none());
    }

    #[test]
    fn live_chat_records_round_trip() {
        let data_dir = tempfile::tempdir().expect("temp dir");
        let records = vec![
            LiveChatRecord {
                id: "c1".to_string(),
                host: "jay.ts.net".to_string(),
                workspace_id: "w1".to_string(),
                title: "Fix the sidebar".to_string(),
                acp_session_id: Some("s1".to_string()),
            },
            LiveChatRecord {
                id: "c2".to_string(),
                host: "jay.ts.net".to_string(),
                workspace_id: "w2".to_string(),
                title: "New chat".to_string(),
                acp_session_id: None,
            },
        ];
        save_live_chats(data_dir.path(), &records).expect("saved");
        assert_eq!(load_live_chats(data_dir.path()), records);
    }

    #[test]
    fn accept_edits_answers_only_teammate_edits_and_writes() {
        let options = PermissionOptions::Flat(vec![
            acp::PermissionOption::new(
                "always",
                "Always allow",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new("allow", "Allow", acp::PermissionOptionKind::AllowOnce),
            acp::PermissionOption::new("reject", "Reject", acp::PermissionOptionKind::RejectOnce),
        ]);
        let answer = |title: &str, mode: Option<&str>| {
            accept_edits_answer(title, mode, &options).map(|option| option.option_id.0.to_string())
        };
        assert_eq!(
            answer("mcp__teammate_workspace__Edit", Some("acceptEdits")),
            Some("allow".to_string())
        );
        assert_eq!(
            answer("mcp__teammate_workspace__Write", Some("acceptEdits")),
            Some("allow".to_string())
        );
        assert_eq!(
            answer("mcp__teammate_workspace__Bash", Some("acceptEdits")),
            None
        );
        assert_eq!(
            answer("mcp__teammate_workspace__Edit", Some("default")),
            None
        );
        assert_eq!(answer("mcp__teammate_workspace__Edit", None), None);
    }
}
