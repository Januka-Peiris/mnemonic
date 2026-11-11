//! Sharing workspaces with teammates over Tailscale, and following theirs.
//! Workspaces are shared one at a time. This side runs the share server
//! while any workspace is shared, keeps what it
//! serves up to date, and polls each teammate listed as a peer.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use acp_thread::{
    AcpThread, AcpThreadEvent, AgentThreadEntry, AssistantMessageChunk, ToolCall, ToolCallContent,
    ToolCallStatus,
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EntityId, Global, IntoElement, SharedString,
    Subscription, Task, WeakEntity, px,
};
use mnemonic_bridge::AgentAttention;
use mnemonic_core::models::{ChatSessionRecord, WorkspaceSummary};
use mnemonic_core::repositories::{chat_session_repository, settings_repository};
use mnemonic_core::services::team_share_server::{DiffSource, TeamServerConfig, TeamShareServer};
use mnemonic_core::services::team_share_service::{
    DEFAULT_TEAM_PORT, TailscaleWhois, TeamAgentState, TeamChat, TeamClient, TeamClientError,
    TeamEntry, TeamShareHub, TeamToolStatus, TeamWorkspace, tailscale_self,
};
use mnemonic_core::services::{git_review_service, workspace_service};
use settings::{RegisterSetting, Settings, SettingsStore};
use ui::{CommonAnimationExt as _, Tooltip, prelude::*};
use workspace::Workspace;

use crate::team_settings::TeamSettingsView;
use crate::team_view::TeamWorkspaceView;
use crate::workspace_view::{LiveWorkspaceAgents, WorkspaceHost, chat_agent_label, open_windows};

/// The `team` section of the settings file.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct TeamSettings {
    pub port: u16,
    pub display_name: String,
    pub allow: Vec<String>,
    pub peers: Vec<TeamPeer>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamPeer {
    pub name: String,
    pub host: String,
}

impl Settings for TeamSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let team = content.team.clone().unwrap_or_default();
        Self {
            port: team.port.unwrap_or(DEFAULT_TEAM_PORT),
            display_name: team.display_name.unwrap_or_default().trim().to_string(),
            allow: team.allow.unwrap_or_default(),
            peers: team
                .peers
                .unwrap_or_default()
                .into_iter()
                .filter_map(|peer| {
                    let host = peer.host?.trim().to_string();
                    if host.is_empty() {
                        return None;
                    }
                    let name = peer
                        .name
                        .map(|name| name.trim().to_string())
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| host.clone());
                    Some(TeamPeer { name, host })
                })
                .collect(),
        }
    }
}

/// How often what this machine shares is refreshed.
const PUBLISH_INTERVAL: Duration = Duration::from_secs(2);
/// How long to wait before trying to start sharing again, as when Tailscale
/// wasn't connected yet.
const RETRY_SHARING_AFTER: Duration = Duration::from_secs(30);
const POLL_ONLINE_PEER: Duration = Duration::from_secs(5);
const POLL_OFFLINE_PEER: Duration = Duration::from_secs(15);
/// Diff counts are only recomputed while a teammate has looked recently.
const VIEWER_WINDOW: Duration = Duration::from_secs(120);
const REFRESH_COUNTS_EVERY: Duration = Duration::from_secs(60);

pub(crate) enum SharingStatus {
    Off,
    Starting,
    On { address: SocketAddr },
    Failed(SharedString),
}

#[derive(Clone, PartialEq)]
struct ServerKey {
    port: u16,
    display_name: String,
    allow: Vec<String>,
}

pub(crate) enum PeerStatus {
    Connecting,
    Online,
    Failed(TeamClientError),
}

pub(crate) struct PeerState {
    pub peer: TeamPeer,
    pub status: PeerStatus,
    pub workspaces: Vec<TeamWorkspace>,
    pub client: Option<TeamClient>,
    _poll: Task<()>,
}

struct ChatWatch {
    thread_id: EntityId,
    _subscription: Subscription,
}

pub(crate) struct TeamShare {
    applied: Option<TeamSettings>,
    hub: TeamShareHub,
    server: Option<TeamShareServer>,
    server_key: Option<ServerKey>,
    pub sharing: SharingStatus,
    start_task: Option<Task<()>>,
    publish_task: Option<Task<()>>,
    chat_watches: HashMap<String, ChatWatch>,
    pub peers: Vec<PeerState>,
    pub collapsed_peers: HashSet<String>,
    /// Workspaces the user chose to share, saved in the core's settings
    /// table so the choice survives restarts.
    shared_workspaces: HashSet<String>,
}

/// The core settings key holding the ids of shared workspaces, as JSON.
const SHARED_WORKSPACES_KEY: &str = "team_shared_workspaces";

struct GlobalTeamShare(Entity<TeamShare>);

impl Global for GlobalTeamShare {}

gpui::actions!(
    team,
    [
        /// Opens the Team window: share your workspaces and pick teammates to follow.
        OpenTeamSettings
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenTeamSettings, window, cx| {
            TeamSettingsView::open(workspace, window, cx);
        });
    })
    .detach();
    let team = cx.new(|cx| {
        let mut team = TeamShare {
            applied: None,
            hub: TeamShareHub::new(),
            server: None,
            server_key: None,
            sharing: SharingStatus::Off,
            start_task: None,
            publish_task: None,
            chat_watches: HashMap::new(),
            peers: Vec::new(),
            collapsed_peers: HashSet::new(),
            shared_workspaces: load_shared_workspace_ids(),
        };
        team.apply_settings(cx);
        team
    });
    cx.set_global(GlobalTeamShare(team.clone()));
    cx.observe_global::<SettingsStore>(move |cx| {
        team.update(cx, |team, cx| team.apply_settings(cx));
    })
    .detach();
}

pub(crate) fn team_share(cx: &App) -> Option<Entity<TeamShare>> {
    cx.try_global::<GlobalTeamShare>()
        .map(|global| global.0.clone())
}

pub(crate) fn is_workspace_shared(workspace_id: &str, cx: &App) -> bool {
    team_share(cx).is_some_and(|team| team.read(cx).shared_workspaces.contains(workspace_id))
}

/// The icon on a shared workspace's sidebar row, coloured by whether
/// sharing is actually running. None when the workspace isn't shared.
pub(crate) fn render_shared_indicator(workspace_id: &str, cx: &App) -> Option<AnyElement> {
    let team = team_share(cx)?;
    let team = team.read(cx);
    if !team.shared_workspaces.contains(workspace_id) {
        return None;
    }
    let (color, tooltip) = match &team.sharing {
        SharingStatus::On { .. } => (Color::Accent, "Shared with your team".to_string()),
        SharingStatus::Starting | SharingStatus::Off => {
            (Color::Muted, "Shared with your team (starting)".to_string())
        }
        SharingStatus::Failed(err) => (
            Color::Error,
            format!("Shared with your team, but sharing isn't running: {err}"),
        ),
    };
    Some(
        div()
            .id(SharedString::from(format!("shared-{workspace_id}")))
            .child(
                Icon::new(IconName::UserGroup)
                    .size(IconSize::XSmall)
                    .color(color),
            )
            .tooltip(Tooltip::text(tooltip))
            .into_any_element(),
    )
}

pub(crate) fn set_workspace_shared(workspace_id: &str, shared: bool, cx: &mut App) {
    if let Some(team) = team_share(cx) {
        team.update(cx, |team, cx| {
            team.set_workspace_shared(workspace_id, shared, cx)
        });
    }
}

fn load_shared_workspace_ids() -> HashSet<String> {
    let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
        return HashSet::new();
    };
    match settings_repository::get_value(&state.db, SHARED_WORKSPACES_KEY) {
        Ok(Some(json)) => serde_json::from_str::<Vec<String>>(&json)
            .map(|ids| ids.into_iter().collect())
            .unwrap_or_else(|err| {
                log::error!("Couldn't read the shared workspaces: {err}");
                HashSet::new()
            }),
        Ok(None) => HashSet::new(),
        Err(err) => {
            log::error!("Couldn't read the shared workspaces: {err}");
            HashSet::new()
        }
    }
}

impl TeamShare {
    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let settings = TeamSettings::get_global(cx).clone();
        if self.applied.as_ref() == Some(&settings) {
            return;
        }
        let peers_changed = self
            .applied
            .as_ref()
            .is_none_or(|applied| applied.peers != settings.peers || applied.port != settings.port);
        self.applied = Some(settings.clone());

        self.update_sharing(cx);
        if peers_changed {
            self.connect_peers(&settings, cx);
        }
        cx.notify();
    }

    fn set_workspace_shared(&mut self, workspace_id: &str, shared: bool, cx: &mut Context<Self>) {
        let changed = if shared {
            self.shared_workspaces.insert(workspace_id.to_string())
        } else {
            self.shared_workspaces.remove(workspace_id)
        };
        if !changed {
            return;
        }
        let mut ids: Vec<String> = self.shared_workspaces.iter().cloned().collect();
        ids.sort();
        cx.background_spawn(async move {
            let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
                return;
            };
            let saved = serde_json::to_string(&ids)
                .map_err(|err| err.to_string())
                .and_then(|json| {
                    settings_repository::set_value(&state.db, SHARED_WORKSPACES_KEY, &json)
                });
            if let Err(err) = saved {
                log::error!("Couldn't save the shared workspaces: {err}");
            }
        })
        .detach();
        self.update_sharing(cx);
        // Publishes straight away rather than on the next tick, so an
        // unshared workspace disappears for teammates at once.
        if self.server.is_some() {
            self.start_publishing(cx);
        }
        cx.notify();
    }

    /// Runs the share server exactly while some workspace is shared,
    /// restarting it when the settings it was started with change.
    fn update_sharing(&mut self, cx: &mut Context<Self>) {
        let Some(settings) = self.applied.clone() else {
            return;
        };
        if self.shared_workspaces.is_empty() {
            if self.server_key.is_some() {
                self.stop_sharing();
            }
            return;
        }
        let key = ServerKey {
            port: settings.port,
            display_name: settings.display_name.clone(),
            allow: settings.allow.clone(),
        };
        if self.server_key.as_ref() != Some(&key) {
            self.start_sharing(key, cx);
        }
    }

    fn start_sharing(&mut self, key: ServerKey, cx: &mut Context<Self>) {
        self.stop_sharing();
        self.server_key = Some(key.clone());
        self.sharing = SharingStatus::Starting;
        let hub = TeamShareHub::new();
        self.hub = hub.clone();
        self.start_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let attempt = cx
                    .background_spawn({
                        let key = key.clone();
                        let hub = hub.clone();
                        async move { start_server(key, hub) }
                    })
                    .await;
                let started = this.update(cx, |this, cx| {
                    if this.server_key.as_ref() != Some(&key) {
                        return true;
                    }
                    let started = match attempt {
                        Ok(server) => {
                            this.sharing = SharingStatus::On {
                                address: server.address(),
                            };
                            this.server = Some(server);
                            this.start_publishing(cx);
                            true
                        }
                        Err(err) => {
                            log::warn!("Couldn't start sharing workspaces: {err}");
                            this.sharing = SharingStatus::Failed(err.into());
                            false
                        }
                    };
                    cx.notify();
                    started
                });
                match started {
                    Ok(true) | Err(_) => break,
                    Ok(false) => {}
                }
                cx.background_executor().timer(RETRY_SHARING_AFTER).await;
            }
        }));
    }

    fn stop_sharing(&mut self) {
        self.start_task = None;
        self.publish_task = None;
        self.server = None;
        self.server_key = None;
        self.chat_watches.clear();
        // Wakes any viewer waiting on a chat, so it hears the chat is gone
        // instead of waiting out its long-poll.
        self.hub.retain_chats(|_| false);
        self.hub.set_workspaces(Vec::new());
        self.sharing = SharingStatus::Off;
    }

    fn start_publishing(&mut self, cx: &mut Context<Self>) {
        self.publish_task = Some(cx.spawn(async move |this, cx| {
            let mut counts_refreshed_at: Option<Instant> = None;
            loop {
                let Ok(shared_ids) = this.read_with(cx, |this, _| this.shared_workspaces.clone())
                else {
                    break;
                };
                if let Some(state) = mnemonic_bridge::mnemonic_state_cloned() {
                    let records = cx
                        .background_spawn({
                            let state = state.clone();
                            async move { load_shared_records(&state, &shared_ids) }
                        })
                        .await;
                    let hub = match records {
                        Ok(records) => this.update(cx, |this, cx| {
                            this.publish(records, cx);
                            this.hub.clone()
                        }),
                        Err(err) => {
                            log::error!("Couldn't list workspaces to share: {err}");
                            this.read_with(cx, |this, _| this.hub.clone())
                        }
                    };
                    let Ok(hub) = hub else {
                        break;
                    };
                    if hub.has_viewers_within(VIEWER_WINDOW)
                        && counts_refreshed_at.is_none_or(|at| at.elapsed() >= REFRESH_COUNTS_EVERY)
                    {
                        counts_refreshed_at = Some(Instant::now());
                        let workspace_ids: Vec<String> = hub
                            .workspaces()
                            .into_iter()
                            .map(|workspace| workspace.id)
                            .collect();
                        cx.background_spawn(async move {
                            for workspace_id in workspace_ids {
                                if let Err(err) = git_review_service::get_workspace_changed_files(
                                    &state,
                                    &workspace_id,
                                ) {
                                    log::debug!(
                                        "Changed files for {workspace_id} unavailable: {err}"
                                    );
                                }
                            }
                        })
                        .await;
                    }
                }
                cx.background_executor().timer(PUBLISH_INTERVAL).await;
            }
        }));
    }

    fn publish(
        &mut self,
        mut records: Vec<(WorkspaceSummary, Vec<ChatSessionRecord>)>,
        cx: &mut Context<Self>,
    ) {
        if self.server.is_none() {
            return;
        }
        // The list was loaded off the main thread; a workspace unshared
        // meanwhile must not slip through.
        records.retain(|(summary, _)| self.shared_workspaces.contains(&summary.id));
        let mut live: HashMap<String, LiveWorkspaceAgents> = HashMap::new();
        for workspace in open_windows(cx) {
            for agents in WorkspaceHost::live_agents(workspace.read(cx), cx) {
                live.entry(agents.workspace_id.clone()).or_insert(agents);
            }
        }

        let mut open_chats = Vec::new();
        let mut workspaces = Vec::with_capacity(records.len());
        for (summary, chat_records) in records {
            let agents = live.get(&summary.id);
            let mut chats = Vec::with_capacity(chat_records.len());
            for record in chat_records {
                let live_chat = agents.and_then(|agents| {
                    agents.chats.iter().find(|chat| chat.record_id == record.id)
                });
                let thread = live_chat.and_then(|chat| chat.thread.clone());
                chats.push(TeamChat {
                    id: record.id.clone(),
                    title: live_chat
                        .map(|chat| chat.title.to_string())
                        .unwrap_or(record.title),
                    agent: chat_agent_label(&record.agent).to_string(),
                    open: thread.is_some(),
                });
                if let Some(thread) = thread {
                    open_chats.push((record.id, thread));
                }
            }
            workspaces.push(TeamWorkspace {
                pr_number: mnemonic_bridge::pr_number(&summary.id),
                pr_state: mnemonic_bridge::pr_state(&summary.id),
                changed_files: summary.changed_files.len() as u32,
                additions: summary
                    .changed_files
                    .iter()
                    .map(|file| file.additions)
                    .sum(),
                deletions: summary
                    .changed_files
                    .iter()
                    .map(|file| file.deletions)
                    .sum(),
                agent_state: agent_state(&summary.id, agents, cx),
                idle_seconds: mnemonic_bridge::last_agent_activity(&summary.id)
                    .map(|at| at.elapsed().as_secs()),
                chats,
                id: summary.id,
                project: summary.repo,
                name: summary.name,
                branch: summary.branch,
            });
        }
        self.hub.set_workspaces(workspaces);
        self.watch_chats(open_chats, cx);
    }

    fn watch_chats(
        &mut self,
        open_chats: Vec<(String, Entity<AcpThread>)>,
        cx: &mut Context<Self>,
    ) {
        let open_ids: HashSet<String> = open_chats.iter().map(|(id, _)| id.clone()).collect();
        self.chat_watches.retain(|id, _| open_ids.contains(id));
        self.hub.retain_chats(|id| open_ids.contains(id));
        for (chat_id, thread) in open_chats {
            let watched = self
                .chat_watches
                .get(&chat_id)
                .is_some_and(|watch| watch.thread_id == thread.entity_id());
            if watched && self.hub.has_chat(&chat_id) {
                continue;
            }
            self.hub
                .replace_chat(&chat_id, thread_entries(thread.read(cx), cx));
            let subscription = cx.subscribe(&thread, {
                let chat_id = chat_id.clone();
                move |this, thread, event, cx| {
                    this.publish_thread_event(&chat_id, &thread, event, cx);
                }
            });
            self.chat_watches.insert(
                chat_id,
                ChatWatch {
                    thread_id: thread.entity_id(),
                    _subscription: subscription,
                },
            );
        }
    }

    fn publish_thread_event(
        &mut self,
        chat_id: &str,
        thread: &Entity<AcpThread>,
        event: &AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        let thread = thread.read(cx);
        let entries = thread.entries();
        let published = match event {
            AcpThreadEvent::NewEntry => entries.len().checked_sub(1).is_some_and(|index| {
                self.hub
                    .set_chat_entry(chat_id, index, team_entry(&entries[index], cx))
            }),
            AcpThreadEvent::EntryUpdated(index) => entries.get(*index).is_some_and(|entry| {
                self.hub
                    .set_chat_entry(chat_id, *index, team_entry(entry, cx))
            }),
            AcpThreadEvent::EntriesRemoved(_) => false,
            // These can change a tool call's status without an entry
            // update of their own, so every entry is re-checked. Unchanged
            // entries cost a comparison and wake nobody.
            AcpThreadEvent::ToolAuthorizationRequested(_)
            | AcpThreadEvent::ToolAuthorizationReceived(_)
            | AcpThreadEvent::Stopped(_)
            | AcpThreadEvent::Error => {
                entries.iter().enumerate().all(|(index, entry)| {
                    self.hub
                        .set_chat_entry(chat_id, index, team_entry(entry, cx))
                }) && self.hub.chat_len(chat_id) == Some(entries.len())
            }
            _ => true,
        };
        if !published {
            self.hub.replace_chat(chat_id, thread_entries(thread, cx));
        }
    }

    fn connect_peers(&mut self, settings: &TeamSettings, cx: &mut Context<Self>) {
        self.peers = settings
            .peers
            .iter()
            .map(|peer| {
                let client = TeamClient::new(&peer.host, settings.port);
                let (status, poll) = match &client {
                    Ok(client) => (
                        PeerStatus::Connecting,
                        Self::poll_peer(peer.host.clone(), client.clone(), cx),
                    ),
                    Err(err) => (PeerStatus::Failed(err.clone()), Task::ready(())),
                };
                PeerState {
                    peer: peer.clone(),
                    status,
                    workspaces: Vec::new(),
                    client: client.ok(),
                    _poll: poll,
                }
            })
            .collect();
    }

    fn poll_peer(host: String, client: TeamClient, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let result = cx
                    .background_spawn({
                        let client = client.clone();
                        async move {
                            client.hello()?;
                            client.workspaces()
                        }
                    })
                    .await;
                let online = result.is_ok();
                let updated = this.update(cx, |this, cx| {
                    let Some(peer) = this.peers.iter_mut().find(|peer| peer.peer.host == host)
                    else {
                        return;
                    };
                    match result {
                        Ok(workspaces) => {
                            peer.status = PeerStatus::Online;
                            peer.workspaces = workspaces;
                        }
                        Err(err) => {
                            peer.status = PeerStatus::Failed(err);
                            peer.workspaces.clear();
                        }
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
                cx.background_executor()
                    .timer(if online {
                        POLL_ONLINE_PEER
                    } else {
                        POLL_OFFLINE_PEER
                    })
                    .await;
            }
        })
    }

    pub(crate) fn peer(&self, host: &str) -> Option<&PeerState> {
        self.peers.iter().find(|peer| peer.peer.host == host)
    }
}

fn start_server(key: ServerKey, hub: TeamShareHub) -> Result<TeamShareServer, String> {
    let me = tailscale_self()?;
    let display_name = [&key.display_name, &me.display_name, &me.login]
        .into_iter()
        .find(|name| !name.is_empty())
        .cloned()
        .unwrap_or_else(|| "Teammate".to_string());
    let mut allowed_logins = key.allow;
    // Your own login is always allowed, so your other machines can follow
    // this one.
    if !me.login.is_empty() {
        allowed_logins.push(me.login.clone());
    }
    let diff_source: DiffSource = Arc::new(|workspace_id: &str| {
        let state = mnemonic_bridge::mnemonic_state_cloned()
            .ok_or_else(|| "Mnemonic isn't ready".to_string())?;
        git_review_service::get_workspace_full_diff(&state, workspace_id)
    });
    TeamShareServer::start(
        TeamServerConfig {
            bind: IpAddr::V4(me.ip),
            port: key.port,
            display_name,
            allowed_logins,
        },
        hub,
        Arc::new(TailscaleWhois::default()),
        diff_source,
    )
}

fn load_shared_records(
    state: &mnemonic_core::state::AppState,
    shared_ids: &HashSet<String>,
) -> Result<Vec<(WorkspaceSummary, Vec<ChatSessionRecord>)>, String> {
    workspace_service::list_workspaces(state)?
        .into_iter()
        .filter(|summary| shared_ids.contains(&summary.id))
        .map(|summary| {
            let chats = chat_session_repository::list_for_workspace(&state.db, &summary.id)?;
            Ok((summary, chats))
        })
        .collect()
}

fn agent_state(
    workspace_id: &str,
    agents: Option<&LiveWorkspaceAgents>,
    cx: &App,
) -> TeamAgentState {
    let mut working = agents.is_some_and(|agents| agents.terminal_agent_busy);
    let threads = agents
        .into_iter()
        .flat_map(|agents| agents.chats.iter())
        .filter_map(|chat| chat.thread.as_ref());
    for thread in threads {
        let thread = thread.read(cx);
        let waiting = thread.entries().iter().any(|entry| {
            matches!(
                entry,
                AgentThreadEntry::ToolCall(ToolCall {
                    status: ToolCallStatus::WaitingForConfirmation { .. },
                    ..
                })
            )
        });
        if waiting {
            return TeamAgentState::WaitingForApproval;
        }
        if thread.status() == acp_thread::ThreadStatus::Generating {
            working = true;
        }
    }
    if working {
        TeamAgentState::Working
    } else if mnemonic_bridge::agent_attention(workspace_id) == Some(AgentAttention::Error) {
        TeamAgentState::Error
    } else {
        TeamAgentState::Idle
    }
}

fn thread_entries(thread: &AcpThread, cx: &App) -> Vec<TeamEntry> {
    thread
        .entries()
        .iter()
        .map(|entry| team_entry(entry, cx))
        .collect()
}

fn push_paragraph(text: &mut String, paragraph: &str) {
    if paragraph.trim().is_empty() {
        return;
    }
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(paragraph);
}

fn team_entry(entry: &AgentThreadEntry, cx: &App) -> TeamEntry {
    match entry {
        AgentThreadEntry::UserMessage(message) => TeamEntry::User {
            markdown: message.content.to_markdown(cx).to_string(),
        },
        AgentThreadEntry::AssistantMessage(message) => {
            let mut markdown = String::new();
            let mut thinking = String::new();
            for chunk in &message.chunks {
                match chunk {
                    AssistantMessageChunk::Message { block } => {
                        push_paragraph(&mut markdown, block.to_markdown(cx))
                    }
                    AssistantMessageChunk::Thought { block } => {
                        push_paragraph(&mut thinking, block.to_markdown(cx))
                    }
                }
            }
            TeamEntry::Assistant {
                markdown,
                thinking: (!thinking.is_empty()).then_some(thinking),
            }
        }
        AgentThreadEntry::ToolCall(tool_call) => {
            let status = match &tool_call.status {
                ToolCallStatus::Pending => TeamToolStatus::Pending,
                ToolCallStatus::WaitingForConfirmation { .. } => TeamToolStatus::WaitingForApproval,
                ToolCallStatus::InProgress => TeamToolStatus::Running,
                ToolCallStatus::Completed => TeamToolStatus::Done,
                ToolCallStatus::Failed => TeamToolStatus::Failed,
                ToolCallStatus::Rejected => TeamToolStatus::Rejected,
                ToolCallStatus::Canceled => TeamToolStatus::Canceled,
            };
            let output_preview = (status == TeamToolStatus::Failed)
                .then(|| tool_call_error_preview(tool_call, cx))
                .flatten();
            TeamEntry::ToolCall {
                title: tool_call.label.read(cx).source().to_string(),
                tool_kind: serde_json::to_value(tool_call.kind)
                    .ok()
                    .and_then(|kind| kind.as_str().map(str::to_string))
                    .unwrap_or_default(),
                status,
                description: tool_call_description(tool_call),
                output_preview,
            }
        }
        AgentThreadEntry::CompletedPlan(entries) => TeamEntry::Plan {
            markdown: entries
                .iter()
                .map(|entry| format!("- {}", entry.content.read(cx).source()))
                .collect::<Vec<_>>()
                .join("\n"),
        },
        AgentThreadEntry::ContextCompaction(compaction) => TeamEntry::Compaction {
            summary: compaction
                .summary
                .as_ref()
                .map(|summary| summary.read(cx).source().to_string()),
        },
    }
}

/// What the agent said a tool call is for, from the `description` it gave
/// with the call.
fn tool_call_description(tool_call: &ToolCall) -> Option<String> {
    let description = tool_call
        .raw_input
        .as_ref()?
        .get("description")?
        .as_str()?
        .trim();
    (!description.is_empty()).then(|| description.to_string())
}

/// The start of a failed tool call's output, which usually says why.
fn tool_call_error_preview(tool_call: &ToolCall, cx: &App) -> Option<String> {
    let text = tool_call
        .content
        .iter()
        .filter_map(|content| match content {
            ToolCallContent::ContentBlock(block) => Some(block.to_markdown(cx)),
            ToolCallContent::Diff(_) | ToolCallContent::Terminal(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let preview = one_line_preview(&text);
    (!preview.is_empty()).then_some(preview)
}

/// Collapses text onto one line for a preview, dropping code fences and
/// blank lines.
pub(crate) fn one_line_preview(text: &str) -> String {
    let mut preview = String::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("```") {
            continue;
        }
        if !preview.is_empty() {
            preview.push(' ');
        }
        preview.push_str(line);
        if preview.len() > 300 {
            break;
        }
    }
    preview
}

/// The sidebar's Team section: each teammate and their workspaces, with a
/// way into the Team window.
pub(crate) fn render_team_section(
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> Option<AnyElement> {
    let team = team_share(cx)?;
    let team_state = team.read(cx);
    let nothing_set_up =
        team_state.peers.is_empty() && matches!(team_state.sharing, SharingStatus::Off);
    let open_team_window = {
        let workspace = workspace.clone();
        move |_: &gpui::ClickEvent, window: &mut gpui::Window, cx: &mut App| {
            workspace
                .update(cx, |workspace, cx| {
                    TeamSettingsView::open(workspace, window, cx)
                })
                .ok();
        }
    };
    let colors = cx.theme().colors();
    let (sharing_icon, sharing_color, sharing_tooltip) = match &team_state.sharing {
        SharingStatus::Off => (
            IconName::SignalLow,
            Color::Muted,
            "Not sharing your workspaces".to_string(),
        ),
        SharingStatus::Starting => (
            IconName::SignalMedium,
            Color::Muted,
            "Starting to share your workspaces".to_string(),
        ),
        SharingStatus::On { address } => (
            IconName::SignalHigh,
            Color::Success,
            format!("Sharing your workspaces on {address}"),
        ),
        SharingStatus::Failed(err) => (
            IconName::SignalLow,
            Color::Error,
            format!("Sharing is on, but {err}"),
        ),
    };

    let mut section = v_flex().w_full().pt_2().child(
        h_flex()
            .px_2()
            .h(px(24.))
            .gap_1()
            .items_center()
            .w_full()
            .child(
                div().flex_grow(1.0).child(
                    Label::new("Team")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .when(!nothing_set_up, |row| {
                row.child(
                    div()
                        .id("team-sharing-status")
                        .child(
                            Icon::new(sharing_icon)
                                .size(IconSize::XSmall)
                                .color(sharing_color),
                        )
                        .tooltip(Tooltip::text(sharing_tooltip)),
                )
            })
            .child(
                IconButton::new("team-open-settings", IconName::Settings)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Sharing and teammates"))
                    .on_click(open_team_window.clone()),
            ),
    );
    if nothing_set_up {
        section = section.child(
            h_flex()
                .id("team-set-up")
                .px_2()
                .h(px(24.))
                .gap_1p5()
                .items_center()
                .w_full()
                .cursor_pointer()
                .rounded_md()
                .hover(|style| style.bg(colors.ghost_element_hover))
                .on_click(open_team_window)
                .child(
                    Icon::new(IconName::UserGroup)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new("Share with your team")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        );
    }

    for peer in &team_state.peers {
        let host = peer.peer.host.clone();
        let collapsed = team_state.collapsed_peers.contains(&host);
        let status_label = match &peer.status {
            PeerStatus::Connecting => Some("Connecting".to_string()),
            PeerStatus::Online if peer.workspaces.is_empty() => Some("No workspaces".to_string()),
            PeerStatus::Online => None,
            PeerStatus::Failed(err) => Some(err.to_string()),
        };
        let failure_detail = match &peer.status {
            PeerStatus::Failed(TeamClientError::Unreachable(detail)) => Some(format!(
                "Couldn't reach {host}. Their Mac may be asleep, or sharing is off. ({detail})"
            )),
            PeerStatus::Failed(TeamClientError::Incompatible { protocol }) => Some(format!(
                "{host} speaks team protocol {protocol}; update both Mnemonics to the same version."
            )),
            _ => None,
        };
        let online = matches!(peer.status, PeerStatus::Online);
        section = section.child(
            h_flex()
                .id(SharedString::from(format!("team-peer-{host}")))
                .px_2()
                .h(px(24.))
                .gap_1()
                .items_center()
                .w_full()
                .cursor_pointer()
                .hover(|style| style.bg(colors.ghost_element_hover))
                .on_click({
                    let team = team.clone();
                    let host = host.clone();
                    move |_, _, cx| {
                        team.update(cx, |team, cx| {
                            if !team.collapsed_peers.remove(&host) {
                                team.collapsed_peers.insert(host.clone());
                            }
                            cx.notify();
                        });
                    }
                })
                .when_some(failure_detail, |row, detail| {
                    row.tooltip(Tooltip::text(detail))
                })
                .child(
                    Icon::new(if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
                .child(div().size(px(6.)).rounded_full().bg(if online {
                    cx.theme().status().success
                } else {
                    colors.text_muted
                }))
                .child(
                    div().flex_grow(1.0).overflow_hidden().child(
                        Label::new(peer.peer.name.clone())
                            .size(LabelSize::Small)
                            .single_line(),
                    ),
                )
                .when_some(status_label, |row, label| {
                    row.child(
                        Label::new(label)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    )
                }),
        );
        if collapsed {
            continue;
        }
        for shared in &peer.workspaces {
            section = section.child(render_peer_workspace_row(
                &host,
                shared,
                workspace.clone(),
                cx,
            ));
        }
    }
    Some(section.into_any_element())
}

fn render_peer_workspace_row(
    host: &str,
    shared: &TeamWorkspace,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors();
    let pr_color = shared
        .pr_state
        .as_deref()
        .and_then(|state| mnemonic_bridge::pr_state_color(state, cx));
    let tooltip = match (&shared.pr_number, shared.pr_state.as_deref()) {
        (Some(number), Some(state)) => format!(
            "{} · {} · PR #{number}: {}",
            shared.project,
            shared.branch,
            mnemonic_bridge::pr_state_label(state)
        ),
        _ => format!("{} · {}", shared.project, shared.branch),
    };
    let host = host.to_string();
    let workspace_id = shared.id.clone();
    h_flex()
        .id(SharedString::from(format!("team-{host}-{}", shared.id)))
        .pl(px(28.))
        .pr_2()
        .h(px(24.))
        .gap_1p5()
        .items_center()
        .w_full()
        .cursor_pointer()
        .rounded_md()
        .hover(|style| style.bg(colors.ghost_element_hover))
        .tooltip(Tooltip::text(tooltip))
        .on_click(move |_, window, cx| {
            let host = host.clone();
            let workspace_id = workspace_id.clone();
            workspace
                .update(cx, |workspace, cx| {
                    TeamWorkspaceView::open(workspace, host, workspace_id, window, cx);
                })
                .ok();
        })
        .child(render_agent_state(&shared.id, shared.agent_state, cx))
        .child(
            Icon::new(IconName::FileGit)
                .size(IconSize::XSmall)
                .color(pr_color.map(Color::Custom).unwrap_or(Color::Muted)),
        )
        .child(
            div().flex_grow(1.0).overflow_hidden().child(
                Label::new(shared.name.clone())
                    .size(LabelSize::Default)
                    .single_line(),
            ),
        )
        .when(shared.additions > 0 || shared.deletions > 0, |row| {
            row.child(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(format!("+{}", shared.additions))
                            .size(LabelSize::XSmall)
                            .color(Color::Created),
                    )
                    .child(
                        Label::new(format!("-{}", shared.deletions))
                            .size(LabelSize::XSmall)
                            .color(Color::Deleted),
                    ),
            )
        })
        .into_any_element()
}

/// The same dot and spinner the sidebar shows for your own workspaces.
pub(crate) fn render_agent_state(id: &str, state: TeamAgentState, cx: &App) -> AnyElement {
    let dot = |color| {
        div()
            .size(px(7.))
            .rounded_full()
            .bg(color)
            .into_any_element()
    };
    let (indicator, tooltip) = match state {
        TeamAgentState::Working => (
            Icon::new(IconName::ArrowCircle)
                .size(IconSize::XSmall)
                .color(Color::Muted)
                .with_keyed_rotate_animation(SharedString::from(format!("team-running-{id}")), 2)
                .into_any_element(),
            "Agent running",
        ),
        TeamAgentState::WaitingForApproval => (
            dot(cx.theme().colors().text_accent),
            "Agent is waiting for permission",
        ),
        TeamAgentState::Error => (
            dot(cx.theme().status().error),
            "Agent stopped with an error",
        ),
        TeamAgentState::Idle => (dot(cx.theme().colors().text_muted), "Agent idle"),
    };
    h_flex()
        .id(SharedString::from(format!("team-agent-{id}")))
        .size(px(12.))
        .flex_none()
        .justify_center()
        .tooltip(Tooltip::text(tooltip))
        .child(indicator)
        .into_any_element()
}
