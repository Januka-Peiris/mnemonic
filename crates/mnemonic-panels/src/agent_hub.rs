//! The `mnemonic` MCP tools that let a chat's agent start, brief, wait on
//! and stop other chats. Requests arrive on the hub server's threads and are
//! carried out here, on the main thread.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use agent_client_protocol::schema as acp;
use agent_servers::SessionToolProvider;
use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{App, AppContext as _, AsyncApp, Global, Task};
use mnemonic_core::models::{AgentStartedChat, WorkspaceSummary};
use mnemonic_core::repositories::{
    agent_started_chat_repository, chat_session_repository, repository_repository,
    workspace_repository,
};
use mnemonic_core::services::agent_hub_server::{
    AGENT_HUB_KEY_HEADER, AGENT_HUB_SERVER_NAME, AgentHubHandler, AgentHubServer, HubCaller,
    HubKeys,
};
use mnemonic_core::services::agent_hub_service::{
    ChatPlace, ChatReport, ChatState, ListedWorkspace, MAX_START_DEPTH, Named, RecentStarts,
    StartChatRequest, check_can_archive, check_can_start, check_can_stop, parse_start_chat,
    parse_wait, render_list, render_wait, resolve, tool_definitions,
};
use mnemonic_core::services::mcp_http::required_str;
use mnemonic_core::services::workspace_service;
use mnemonic_core::state::AppState;

use crate::team::workspace_root;
use crate::workspace_panel::{
    WorkspacePanel, branch_slug_from_message, generate_workspace_name, unique_workspace_name,
};
use crate::workspace_view::{
    ChatAgent, WorkspaceHost, chat_agent_label, chat_record_for_session, chat_state,
    clean_chat_title, find_open_chat, latest_reply, open_windows, send_to_chat,
    take_typed_messages, window_handle_of,
};

/// How long a request waits for the main thread to take up a call other than
/// `wait`. Making a workspace runs git, so this is generous.
const CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// Extra time a `wait` request allows beyond the wait itself.
const WAIT_MARGIN: Duration = Duration::from_secs(30);
const DEFINITIONS_TIMEOUT: Duration = Duration::from_secs(10);
const FIRST_CHECK_INTERVAL: Duration = Duration::from_millis(500);
const MAX_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// How often a wait notices that its call was cancelled.
const CANCEL_CHECK_INTERVAL: Duration = Duration::from_millis(500);

struct AgentHub {
    _server: AgentHubServer,
    _calls: Task<()>,
}

impl Global for AgentHub {}

pub fn init(cx: &mut App) {
    let keys = HubKeys::default();
    let (sender, receiver) = mpsc::unbounded::<PendingHubWork>();
    let server = match AgentHubServer::start(keys.clone(), Arc::new(HubHandler { sender })) {
        Ok(server) => server,
        Err(err) => {
            log::error!("{err}; chats run without the Mnemonic tools");
            return;
        }
    };
    let url = server.url();
    agent_servers::set_session_tool_provider(Rc::new(HubToolProvider { keys, url }), cx);
    let calls = cx.spawn(async move |cx| serve_calls(receiver, cx).await);
    cx.set_global(AgentHub {
        _server: server,
        _calls: calls,
    });
}

struct HubToolProvider {
    keys: HubKeys,
    url: String,
}

impl SessionToolProvider for HubToolProvider {
    fn tools_for_session(&self, _cwd: &Path) -> Option<(Vec<acp::McpServer>, String)> {
        let key = self.keys.mint();
        let server =
            acp::McpServerHttp::new(AGENT_HUB_SERVER_NAME, self.url.clone()).headers(vec![
                acp::HttpHeader::new(AGENT_HUB_KEY_HEADER, key.clone()),
            ]);
        Some((vec![acp::McpServer::Http(server)], key))
    }

    fn bind(&self, key: &str, session_id: &acp::SessionId) {
        self.keys.bind(key, &session_id.0);
    }
}

enum HubWork {
    CanStart {
        reply: std::sync::mpsc::Sender<bool>,
    },
    Call {
        tool: String,
        arguments: serde_json::Value,
        cancelled: Arc<AtomicBool>,
        reply: std::sync::mpsc::Sender<Result<String, String>>,
    },
}

struct PendingHubWork {
    caller: HubCaller,
    work: HubWork,
    /// Set by whichever comes first: the main thread taking the work up, or
    /// the request giving up on it. A call its agent was told failed must
    /// not run later.
    claimed: Arc<AtomicBool>,
}

struct HubHandler {
    sender: mpsc::UnboundedSender<PendingHubWork>,
}

impl HubHandler {
    fn send<T>(
        &self,
        caller: &HubCaller,
        work: HubWork,
        reply_receiver: &std::sync::mpsc::Receiver<T>,
        timeout: Duration,
    ) -> Option<T> {
        let claimed = Arc::new(AtomicBool::new(false));
        let pending = PendingHubWork {
            caller: caller.clone(),
            work,
            claimed: claimed.clone(),
        };
        if self.sender.unbounded_send(pending).is_err() {
            return None;
        }
        match reply_receiver.recv_timeout(timeout) {
            Ok(reply) => Some(reply),
            Err(RecvTimeoutError::Timeout) if claimed.swap(true, Ordering::SeqCst) => {
                reply_receiver.recv().ok()
            }
            Err(_) => None,
        }
    }
}

impl AgentHubHandler for HubHandler {
    fn definitions(&self, caller: Option<&HubCaller>) -> serde_json::Value {
        let Some(caller) = caller else {
            return tool_definitions(true);
        };
        let (reply, reply_receiver) = std::sync::mpsc::channel();
        // `start_chat` checks the depth again when called, so offering it
        // when the check can't run here does no harm.
        let can_start = self
            .send(
                caller,
                HubWork::CanStart { reply },
                &reply_receiver,
                DEFINITIONS_TIMEOUT,
            )
            .unwrap_or(true);
        tool_definitions(can_start)
    }

    fn call(
        &self,
        caller: &HubCaller,
        tool: &str,
        arguments: serde_json::Value,
        cancelled: Arc<AtomicBool>,
    ) -> Result<String, String> {
        let timeout = if tool == "wait" {
            parse_wait(&arguments)?.timeout + WAIT_MARGIN
        } else {
            CALL_TIMEOUT
        };
        let (reply, reply_receiver) = std::sync::mpsc::channel();
        let work = HubWork::Call {
            tool: tool.to_string(),
            arguments,
            cancelled,
            reply,
        };
        self.send(caller, work, &reply_receiver, timeout)
            .unwrap_or_else(|| Err("Mnemonic didn't answer in time.".to_string()))
    }
}

#[derive(Default)]
struct HubState {
    recent_starts: RefCell<RecentStarts>,
}

async fn serve_calls(mut receiver: mpsc::UnboundedReceiver<PendingHubWork>, cx: &mut AsyncApp) {
    let hub = Rc::new(HubState::default());
    while let Some(pending) = receiver.next().await {
        if pending.claimed.swap(true, Ordering::SeqCst) {
            log::info!("Skipped an agent hub call that timed out while waiting");
            continue;
        }
        let PendingHubWork { caller, work, .. } = pending;
        match work {
            HubWork::CanStart { reply } => {
                let can_start = cx.update(|cx| caller_can_start(&caller, cx));
                if reply.send(can_start).is_err() {
                    log::warn!("An agent hub tool list finished after its request gave up");
                }
            }
            HubWork::Call {
                tool,
                arguments,
                cancelled,
                reply,
            } => {
                // Each call runs on its own, so a long `wait` holds up no
                // other call.
                let hub = hub.clone();
                cx.spawn(async move |cx| {
                    let outcome = run_tool(hub, caller, tool, arguments, cancelled, cx).await;
                    if reply.send(outcome).is_err() {
                        log::warn!("An agent hub call finished after its request gave up");
                    }
                })
                .detach();
            }
        }
    }
}

fn caller_can_start(caller: &HubCaller, cx: &App) -> bool {
    let Some(caller_chat) = chat_record_for_session(&caller.session_id, cx) else {
        return true;
    };
    let state = mnemonic_bridge::mnemonic_state(cx);
    match agent_started_chat_repository::depth(&state.db, &caller_chat) {
        Ok(depth) => depth <= MAX_START_DEPTH,
        Err(err) => {
            log::error!("Failed to read who started chat {caller_chat}: {err}");
            true
        }
    }
}

async fn run_tool(
    hub: Rc<HubState>,
    caller: HubCaller,
    tool: String,
    arguments: serde_json::Value,
    cancelled: Arc<AtomicBool>,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let caller_chat = cx
        .update(|cx| chat_record_for_session(&caller.session_id, cx))
        .ok_or_else(|| "This chat isn't open in Mnemonic.".to_string())?;
    match tool.as_str() {
        "start_chat" => start_chat(&hub, &caller_chat, &arguments, cx).await,
        "send" => cx.update(|cx| send(&arguments, cx)),
        "wait" => wait(&caller_chat, &arguments, &cancelled, cx).await,
        "list" => cx.update(|cx| list(&caller_chat, cx)),
        "stop_chat" => cx.update(|cx| stop_chat(&arguments, cx)),
        "archive_workspace" => archive_workspace(&arguments, cx).await,
        other => Err(format!("Unknown tool: {other}")),
    }
}

/// A saved chat, open or not.
#[derive(Clone)]
struct KnownChat {
    id: String,
    title: String,
    agent: &'static str,
    workspace_path: Option<String>,
    branch: String,
}

fn workspace_path(summary: &WorkspaceSummary) -> Option<String> {
    workspace_root(summary).map(|path| path.display().to_string())
}

fn known_chats(cx: &App) -> Result<Vec<KnownChat>, String> {
    let state = mnemonic_bridge::mnemonic_state(cx);
    let mut chats = Vec::new();
    for workspace in workspace_repository::list(&state.db)? {
        for record in chat_session_repository::list_for_workspace(&state.db, &workspace.id)? {
            // A chat's tab is renamed as it goes, and that is the name the
            // user knows it by.
            let title = find_open_chat(&record.id, cx)
                .map(|chat| chat.title.to_string())
                .unwrap_or(record.title);
            chats.push(KnownChat {
                id: record.id,
                title,
                agent: chat_agent_label(&record.agent),
                workspace_path: workspace_path(&workspace),
                branch: workspace.branch.clone(),
            });
        }
    }
    Ok(chats)
}

fn resolve_chat<'a>(chats: &'a [KnownChat], text: &str) -> Result<&'a KnownChat, String> {
    let named: Vec<Named> = chats
        .iter()
        .map(|chat| Named {
            id: &chat.id,
            name: &chat.title,
        })
        .collect();
    let id = resolve("chat", &named, text)?;
    chats
        .iter()
        .find(|chat| chat.id == id)
        .ok_or_else(|| format!("No chat found matching '{text}'"))
}

fn resolve_workspace<'a>(
    workspaces: &'a [WorkspaceSummary],
    text: &str,
) -> Result<&'a WorkspaceSummary, String> {
    let named: Vec<Named> = workspaces
        .iter()
        .map(|workspace| Named {
            id: &workspace.id,
            name: &workspace.name,
        })
        .collect();
    let id = resolve("workspace", &named, text)?;
    workspaces
        .iter()
        .find(|workspace| workspace.id == id)
        .ok_or_else(|| format!("No workspace found matching '{text}'"))
}

async fn start_chat(
    hub: &HubState,
    caller_chat: &str,
    arguments: &serde_json::Value,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let request = parse_start_chat(arguments)?;
    let state = cx.update(|cx| mnemonic_bridge::mnemonic_state(cx));
    let earlier = hub
        .recent_starts
        .borrow_mut()
        .earlier(caller_chat, &request, Instant::now());
    if let Some(chat_id) = earlier {
        return describe_started(&state, &chat_id);
    }
    let (caller_window, window_handle) = cx.update(|cx| {
        let busy_children = agent_started_chat_repository::started_by(&state.db, caller_chat)?
            .iter()
            .filter(|child| {
                matches!(
                    chat_state(&child.chat_id, cx),
                    ChatState::Starting | ChatState::Replying
                )
            })
            .count();
        check_can_start(&state.db, caller_chat, busy_children)?;
        let window = find_open_chat(caller_chat, cx)
            .map(|chat| chat.window)
            .ok_or_else(|| "This chat isn't open in Mnemonic.".to_string())?;
        let handle = window_handle_of(&window, cx)
            .ok_or_else(|| "This chat's window is closed.".to_string())?;
        Ok::<_, String>((window, handle))
    })?;

    let (workspace_id, made_workspace) = match &request.place {
        ChatPlace::Existing { workspace } => {
            let workspaces = workspace_repository::list(&state.db)?;
            (resolve_workspace(&workspaces, workspace)?.id.clone(), false)
        }
        ChatPlace::New { repo, branch, base } => {
            let workspace_id =
                create_workspace(&state, &request, repo, branch.clone(), base.clone(), cx).await?;
            (workspace_id, true)
        }
    };

    let agent = ChatAgent::from_choice(request.agent);
    let title = clean_chat_title(
        request.title.as_deref().unwrap_or(&request.task),
        agent.label(),
    )
    .unwrap_or_else(|| agent.label().to_string());
    let chat_id = cx
        .update_window(window_handle, |_, window, cx| {
            caller_window.update(cx, |workspace, cx| {
                WorkspaceHost::open_chat_in_background(
                    workspace,
                    &workspace_id,
                    agent,
                    title,
                    request.task.clone(),
                    Some(caller_chat.to_string()),
                    window,
                    cx,
                )
            })
        })
        .map_err(|_| "This chat's window is closed.".to_string())??;
    let link = AgentStartedChat {
        chat_id: chat_id.clone(),
        started_by_chat_id: caller_chat.to_string(),
        workspace_id: workspace_id.clone(),
        made_workspace,
    };
    if let Err(err) = agent_started_chat_repository::insert(&state.db, &link) {
        log::error!("Failed to record that chat {caller_chat} started {chat_id}: {err}");
    }
    hub.recent_starts
        .borrow_mut()
        .remember(caller_chat, &request, &chat_id, Instant::now());
    describe_started(&state, &chat_id)
}

async fn create_workspace(
    state: &AppState,
    request: &StartChatRequest,
    repo: &str,
    branch: Option<String>,
    base: Option<String>,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let repositories = repository_repository::list(&state.db)?;
    let named: Vec<Named> = repositories
        .iter()
        .map(|repository| Named {
            id: &repository.id,
            name: &repository.name,
        })
        .collect();
    let repository_id = resolve("repository", &named, repo)?;
    let repository = repositories
        .iter()
        .find(|repository| repository.id == repository_id)
        .cloned()
        .ok_or_else(|| format!("No repository found matching '{repo}'"))?;
    let existing_names: Vec<String> = workspace_repository::list(&state.db)?
        .into_iter()
        .map(|workspace| workspace.name)
        .collect();
    let name = match branch_slug_from_message(request.title.as_deref().unwrap_or(&request.task)) {
        Some(slug) => unique_workspace_name(&slug, &existing_names),
        None => generate_workspace_name(&existing_names),
    };
    // Making the worktree runs git, so it stays off the main thread.
    let detail = cx
        .background_spawn({
            let state = state.clone();
            async move {
                let input = workspace_service::new_workspace_input(
                    &state,
                    &repository,
                    name,
                    branch,
                    base,
                )?;
                workspace_service::create_workspace(&state, input)
            }
        })
        .await?;
    cx.update(|cx| {
        for window in open_windows(cx) {
            if let Some(panel) = window.read(cx).panel::<WorkspacePanel>(cx) {
                panel.update(cx, |panel, cx| panel.refresh(cx));
            }
        }
    });
    Ok(detail.summary.id)
}

fn describe_started(state: &AppState, chat_id: &str) -> Result<String, String> {
    let Some(link) = agent_started_chat_repository::get(&state.db, chat_id)? else {
        return Ok(format!(
            "Started chat {chat_id}. Collect its reply with `wait`."
        ));
    };
    let workspace = workspace_repository::get(&state.db, &link.workspace_id)?;
    Ok(format!(
        "Started chat {chat_id} in workspace {} \"{}\"{}. Collect its reply with `wait`.",
        workspace.id,
        workspace.name,
        workspace_path(&workspace)
            .map(|path| format!(" at {path}"))
            .unwrap_or_default()
    ))
}

fn send(arguments: &serde_json::Value, cx: &mut App) -> Result<String, String> {
    let chat_text = required_str(arguments, "chat")?;
    let text = required_str(arguments, "text")?;
    if text.trim().is_empty() {
        return Err("text is empty".to_string());
    }
    let chats = known_chats(cx)?;
    let chat = resolve_chat(&chats, chat_text)?;
    send_to_chat(&chat.id, text.to_string(), cx)?;
    Ok(format!(
        "Sent to chat {} \"{}\". Collect the reply with `wait`.",
        chat.id, chat.title
    ))
}

fn report(chat: &KnownChat, cx: &App) -> ChatReport {
    ChatReport {
        id: chat.id.clone(),
        title: chat.title.clone(),
        agent: chat.agent.to_string(),
        state: chat_state(&chat.id, cx),
        workspace_path: chat.workspace_path.clone(),
        branch: Some(chat.branch.clone()),
        reply: latest_reply(&chat.id, cx),
    }
}

async fn wait(
    caller_chat: &str,
    arguments: &serde_json::Value,
    cancelled: &AtomicBool,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let request = parse_wait(arguments)?;
    let targets = cx.update(|cx| {
        let chats = known_chats(cx)?;
        request
            .chats
            .iter()
            .map(|text| resolve_chat(&chats, text).cloned())
            .collect::<Result<Vec<_>, String>>()
    })?;
    let finished = |report: &ChatReport| {
        matches!(
            report.state,
            ChatState::Replied | ChatState::Stopped | ChatState::Closed
        )
    };
    let mut waited = Duration::ZERO;
    let mut next_check = Duration::ZERO;
    let mut interval = FIRST_CHECK_INTERVAL;
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Err("The wait was cancelled.".to_string());
        }
        if waited >= next_check {
            let (typed, reports) = cx.update(|cx| {
                let typed = take_typed_messages(caller_chat, cx);
                let reports: Vec<ChatReport> =
                    targets.iter().map(|chat| report(chat, cx)).collect();
                (typed, reports)
            });
            let done = if request.any {
                reports.iter().any(finished)
            } else {
                reports.iter().all(finished)
            };
            if !typed.is_empty() || done {
                return Ok(render_wait(&reports, false, &typed));
            }
            if waited >= request.timeout {
                return Ok(render_wait(&reports, true, &typed));
            }
            next_check = (waited + interval).min(request.timeout);
            interval = (interval * 2).min(MAX_CHECK_INTERVAL);
        }
        let step = CANCEL_CHECK_INTERVAL.min(next_check - waited);
        cx.background_executor().timer(step).await;
        waited += step;
    }
}

fn list(caller_chat: &str, cx: &App) -> Result<String, String> {
    let state = mnemonic_bridge::mnemonic_state(cx);
    let repositories: Vec<(String, String)> = repository_repository::list(&state.db)?
        .into_iter()
        .map(|repository| {
            let default_branch = workspace_service::default_base_branch(&state, &repository.id);
            (repository.name, default_branch)
        })
        .collect();
    let mut workspaces = Vec::new();
    for workspace in workspace_repository::list(&state.db)? {
        let chats = chat_session_repository::list_for_workspace(&state.db, &workspace.id)?
            .into_iter()
            .map(|record| {
                let title = find_open_chat(&record.id, cx)
                    .map(|chat| chat.title.to_string())
                    .unwrap_or(record.title);
                let agent = chat_agent_label(&record.agent).to_string();
                let state = chat_state(&record.id, cx);
                (record.id, title, agent, state)
            })
            .collect();
        workspaces.push(ListedWorkspace {
            path: workspace_path(&workspace),
            id: workspace.id,
            name: workspace.name,
            repo: workspace.repo,
            branch: workspace.branch,
            chats,
        });
    }
    let caller_workspace = find_open_chat(caller_chat, cx)
        .map(|chat| chat.workspace_id)
        .unwrap_or_default();
    let started: Vec<String> = agent_started_chat_repository::started_by(&state.db, caller_chat)?
        .into_iter()
        .map(|link| link.chat_id)
        .collect();
    Ok(render_list(
        &repositories,
        &workspaces,
        caller_chat,
        &caller_workspace,
        &started,
    ))
}

fn stop_chat(arguments: &serde_json::Value, cx: &mut App) -> Result<String, String> {
    let chats = known_chats(cx)?;
    let chat = resolve_chat(&chats, required_str(arguments, "chat")?)?;
    let state = mnemonic_bridge::mnemonic_state(cx);
    check_can_stop(&state.db, &chat.id)?;
    let open_chat =
        find_open_chat(&chat.id, cx).ok_or_else(|| "That chat isn't open.".to_string())?;
    open_chat
        .view
        .update(cx, |view, cx| view.stop_chat(&chat.id, cx));
    Ok(format!("Stopped chat {} \"{}\".", chat.id, chat.title))
}

async fn archive_workspace(
    arguments: &serde_json::Value,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let text = required_str(arguments, "workspace")?;
    let state = cx.update(|cx| mnemonic_bridge::mnemonic_state(cx));
    let workspaces = workspace_repository::list(&state.db)?;
    let workspace = resolve_workspace(&workspaces, text)?.clone();
    let root = workspace_root(&workspace)
        .ok_or_else(|| "The workspace has no folder to archive.".to_string())?;
    cx.background_spawn({
        let state = state.clone();
        let workspace_id = workspace.id.clone();
        async move { check_can_archive(&state.db, &workspace_id, &root) }
    })
    .await?;
    // Its chats and run commands work in the worktree that is about to move.
    cx.update(|cx| {
        for window in open_windows(cx) {
            window.update(cx, |window, cx| {
                WorkspaceHost::kill_run_commands_for_workspace(window, &workspace.id, cx);
                WorkspaceHost::remove_workspace_view(window, &workspace.id, cx);
            });
        }
    });
    cx.background_spawn({
        let state = state.clone();
        let workspace_id = workspace.id.clone();
        async move {
            workspace_service::archive_workspace(&state, &workspace_id)?;
            if let Err(err) =
                chat_session_repository::delete_for_workspace(&state.db, &workspace_id)
            {
                log::error!("Failed to delete chat sessions for {workspace_id}: {err}");
            }
            Ok::<(), String>(())
        }
    })
    .await?;
    cx.update(|cx| {
        for window in open_windows(cx) {
            if let Some(panel) = window.read(cx).panel::<WorkspacePanel>(cx) {
                panel.update(cx, |panel, cx| panel.forget_workspace(&workspace.id, cx));
            }
        }
    });
    Ok(format!(
        "Archived workspace {} \"{}\".",
        workspace.id, workspace.name
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_view::tests::{StubServer, TestChatServer, window_showing_workspace_a};
    use crate::workspace_view::{ChatAgent, WorkspaceHost, find_open_chat};
    use agent_client_protocol::schema as acp;
    use mnemonic_core::repositories::agent_started_chat_repository;
    use serde_json::json;

    fn echo(text: &str) -> Vec<acp::SessionUpdate> {
        vec![acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(format!("echo: {text}").into()),
        )]
    }

    fn spawn_call(
        hub: &Rc<HubState>,
        caller: &HubCaller,
        tool: &str,
        arguments: serde_json::Value,
        cx: &mut gpui::VisualTestContext,
    ) -> Task<Result<String, String>> {
        let task = cx.cx.spawn({
            let hub = hub.clone();
            let caller = caller.clone();
            let tool = tool.to_string();
            move |mut cx| async move {
                run_tool(hub, caller, tool, arguments, Arc::default(), &mut cx).await
            }
        });
        cx.run_until_parked();
        task
    }

    async fn call(
        hub: &Rc<HubState>,
        caller: &HubCaller,
        tool: &str,
        arguments: serde_json::Value,
        cx: &mut gpui::VisualTestContext,
    ) -> Result<String, String> {
        spawn_call(hub, caller, tool, arguments, cx).await
    }

    #[gpui::test]
    async fn start_send_wait_round_trip(cx: &mut gpui::TestAppContext) {
        let (workspace, state, _data_dir, cx) = window_showing_workspace_a(cx).await;
        let connection = acp_thread::StubAgentConnection::new();
        cx.update(|_, cx| {
            cx.set_global(TestChatServer(Rc::new(StubServer(connection.clone()))));
        });

        // The calling chat is mid-turn while it calls tools; the stub holds
        // a turn open when no reply is set.
        let caller_chat = workspace
            .update_in(cx, |workspace, window, cx| {
                WorkspaceHost::open_chat_in_background(
                    workspace,
                    "ws-a",
                    ChatAgent::Claude,
                    "Lead".to_string(),
                    "Split the work".to_string(),
                    None,
                    window,
                    cx,
                )
            })
            .expect("the calling chat opens");
        cx.run_until_parked();
        let session_id = cx
            .read(|cx| {
                let thread = find_open_chat(&caller_chat, cx)?.thread?;
                Some(thread.read(cx).session_id().0.to_string())
            })
            .expect("the calling chat is connected");
        let caller = HubCaller { session_id };
        let hub = Rc::new(HubState::default());

        connection.set_next_prompt_updates(echo("Look at the tests"));
        let started = call(
            &hub,
            &caller,
            "start_chat",
            json!({"task": "Look at the tests", "workspace": "ws-b", "title": "Helper"}),
            cx,
        )
        .await
        .expect("the chat starts");
        let started_chat = agent_started_chat_repository::started_by(&state.db, &caller_chat)
            .expect("started chats are listed")
            .into_iter()
            .next()
            .expect("the new chat records who started it");
        assert_eq!(started_chat.workspace_id, "ws-b");
        assert!(!started_chat.made_workspace);
        assert!(started.contains(&started_chat.chat_id), "{started}");

        let report = call(
            &hub,
            &caller,
            "wait",
            json!({"chats": [started_chat.chat_id]}),
            cx,
        )
        .await
        .expect("the wait returns");
        assert!(report.contains("\"Helper\""), "{report}");
        assert!(report.contains(": replied"), "{report}");
        assert!(report.contains("echo: Look at the tests"), "{report}");

        connection.set_next_prompt_updates(echo("again"));
        call(
            &hub,
            &caller,
            "send",
            json!({"chat": "Helper", "text": "again"}),
            cx,
        )
        .await
        .expect("the message is sent");
        let report = call(&hub, &caller, "wait", json!({"chats": ["Helper"]}), cx)
            .await
            .expect("the wait returns");
        assert!(report.contains(": replied"), "{report}");
        assert!(report.contains("echo: again"), "{report}");
        assert!(!report.contains("echo: Look at the tests"), "{report}");

        let refused = call(
            &hub,
            &caller,
            "stop_chat",
            json!({"chat": "ws-b-primary"}),
            cx,
        )
        .await;
        assert!(refused.is_err(), "a chat the user opened can't be stopped");

        // With no reply set, the stub holds the turn open.
        call(
            &hub,
            &caller,
            "send",
            json!({"chat": "Helper", "text": "hold"}),
            cx,
        )
        .await
        .expect("the message is sent");
        let waiting = spawn_call(
            &hub,
            &caller,
            "wait",
            json!({"chats": ["Helper"], "timeout_seconds": 1}),
            cx,
        );
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        let report = waiting.await.expect("the wait returns");
        assert!(report.contains(": replying"), "{report}");
        assert!(report.contains("Timed out."), "{report}");
    }
}
