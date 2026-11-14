//! Sharing workspaces with teammates over Tailscale, and following theirs.
//! Workspaces are shared one at a time. This side runs the share server
//! while any workspace is shared, keeps what it
//! serves up to date, and polls each teammate listed as a peer.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant, SystemTime};

use acp_thread::{
    AcpThread, AcpThreadEvent, AgentThreadEntry, AssistantMessageChunk, PermissionOptions,
    PlanEntry, SANDBOX_AUTHORIZATION_META_KEY, SUBAGENT_SESSION_INFO_META_KEY,
    SelectedPermissionOutcome, TOOL_NAME_META_KEY, ThreadStatus, ToolCall, ToolCallContent,
    ToolCallStatus,
};
use agent_client_protocol::schema as acp;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EntityId, Global, IntoElement, SharedString,
    Subscription, Task, WeakEntity, px,
};
use mnemonic_bridge::AgentAttention;
use mnemonic_core::models::{ChatSessionRecord, WorkspaceSummary};
use mnemonic_core::repositories::{chat_session_repository, settings_repository};
use mnemonic_core::services::team_share_server::{DiffSource, TeamServerConfig, TeamShareServer};
use mnemonic_core::services::team_share_service::{
    ActionHandler, DEFAULT_TEAM_PORT, TailscaleSelf, TailscaleWhois, TeamAction, TeamActionError,
    TeamActionKind, TeamActionOutcome, TeamAgentState, TeamChat, TeamClient, TeamClientError,
    TeamEntry, TeamPermissionRequest, TeamShareHub, TeamWorkspace, saved_chat_entries,
    tailscale_self,
};
use mnemonic_core::services::{chat_transcript_service, git_review_service, workspace_service};
use settings::{RegisterSetting, Settings, SettingsStore};
use ui::{CommonAnimationExt as _, Tooltip, prelude::*};
use util::ResultExt as _;
use util::markdown::MarkdownCodeBlock;
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};

use crate::team_settings::open_team_window;
use crate::team_workspace_view::teammate_workspace_key;
use crate::workspace_panel::WorkspacePanel;
use crate::workspace_view::{
    LiveWorkspaceAgents, WorkspaceHost, chat_agent_label, find_open_chat, open_windows,
};

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
/// How often a running share checks Tailscale is still on the address and
/// account it started with, as after switching Tailscale accounts.
const CHECK_TAILSCALE_EVERY: Duration = Duration::from_secs(15);
const POLL_ONLINE_PEER: Duration = Duration::from_secs(5);
const POLL_OFFLINE_PEER: Duration = Duration::from_secs(15);
/// Diff counts are only recomputed while a teammate has looked recently.
const VIEWER_WINDOW: Duration = Duration::from_secs(120);
const REFRESH_COUNTS_EVERY: Duration = Duration::from_secs(60);
/// How long a teammate's request waits for the app to carry out its action.
const ACTION_TIMEOUT: Duration = Duration::from_secs(10);

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

/// A shared chat with a saved transcript on disk.
type SavedTranscript = (PathBuf, SystemTime);

/// A teammate's action on its way to the main thread, with where to send
/// the answer.
type PendingAction = (
    TeamAction,
    std::sync::mpsc::Sender<Result<TeamActionOutcome, TeamActionError>>,
);

/// Marker for the notices shown when a teammate acts in a chat.
struct TeamStepInToast;

/// The shared workspaces with their chats, as loaded off the main thread.
type SharedRecords = Vec<(
    WorkspaceSummary,
    Vec<(ChatSessionRecord, Option<SavedTranscript>)>,
)>;

/// A closed chat whose saved transcript is newer than what is shared.
struct TranscriptToRead {
    workspace_id: String,
    chat_id: String,
    path: PathBuf,
    modified: SystemTime,
}

pub(crate) struct TeamShare {
    applied: Option<TeamSettings>,
    hub: TeamShareHub,
    server: Option<TeamShareServer>,
    server_key: Option<ServerKey>,
    pub sharing: SharingStatus,
    start_task: Option<Task<()>>,
    publish_task: Option<Task<()>>,
    /// Restarts sharing when this machine's Tailscale address or account
    /// changes, since the server stays bound to the address it started on.
    tailscale_watch: Option<Task<()>>,
    /// Carries out teammates' actions on the main thread.
    action_task: Option<Task<()>>,
    chat_watches: HashMap<String, ChatWatch>,
    /// Chats shared from their saved transcript, as they aren't loaded in
    /// this app, with when that transcript was written.
    saved_chats: HashMap<String, SystemTime>,
    pub peers: Vec<PeerState>,
    pub collapsed_peers: HashSet<String>,
    /// A teammate's projects folded in the sidebar, as (host, project).
    collapsed_peer_projects: HashSet<(String, String)>,
    /// Workspaces the user chose to share, saved in the core's settings
    /// table so the choice survives restarts.
    shared_workspaces: HashSet<String>,
    /// Shared workspaces whose chats teammates may act in, saved the same
    /// way.
    step_in_workspaces: HashSet<String>,
}

/// The core settings key holding the ids of shared workspaces, as JSON.
const SHARED_WORKSPACES_KEY: &str = "team_shared_workspaces";
/// The core settings key holding the ids of workspaces teammates may step
/// in on, as JSON.
const STEP_IN_WORKSPACES_KEY: &str = "team_step_in_workspaces";

struct GlobalTeamShare(Entity<TeamShare>);

impl Global for GlobalTeamShare {}

gpui::actions!(
    team,
    [
        /// Opens the Team window: share your workspaces and pick teammates to follow.
        OpenTeamSettings
    ]
);

fn open_team_window_from(workspace: &Workspace, cx: &mut App) {
    open_team_window(workspace.app_state().fs.clone(), cx);
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenTeamSettings, _window, cx| {
            open_team_window_from(workspace, cx);
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
            tailscale_watch: None,
            action_task: None,
            chat_watches: HashMap::new(),
            saved_chats: HashMap::new(),
            peers: Vec::new(),
            collapsed_peers: HashSet::new(),
            collapsed_peer_projects: HashSet::new(),
            shared_workspaces: load_workspace_ids(SHARED_WORKSPACES_KEY),
            step_in_workspaces: load_workspace_ids(STEP_IN_WORKSPACES_KEY),
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

pub(crate) fn workspace_allows_step_in(workspace_id: &str, cx: &App) -> bool {
    team_share(cx).is_some_and(|team| team.read(cx).allows_step_in(workspace_id))
}

pub(crate) fn set_workspace_step_in(workspace_id: &str, allowed: bool, cx: &mut App) {
    if let Some(team) = team_share(cx) {
        team.update(cx, |team, cx| {
            team.set_workspace_step_in(workspace_id, allowed, cx)
        });
    }
}

fn load_workspace_ids(key: &str) -> HashSet<String> {
    let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
        return HashSet::new();
    };
    match settings_repository::get_value(&state.db, key) {
        Ok(Some(json)) => serde_json::from_str::<Vec<String>>(&json)
            .map(|ids| ids.into_iter().collect())
            .unwrap_or_else(|err| {
                log::error!("Couldn't read {key}: {err}");
                HashSet::new()
            }),
        Ok(None) => HashSet::new(),
        Err(err) => {
            log::error!("Couldn't read {key}: {err}");
            HashSet::new()
        }
    }
}

fn save_workspace_ids(key: &'static str, ids: &HashSet<String>, cx: &App) {
    let mut ids: Vec<String> = ids.iter().cloned().collect();
    ids.sort();
    cx.background_spawn(async move {
        let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
            return;
        };
        let saved = serde_json::to_string(&ids)
            .map_err(|err| err.to_string())
            .and_then(|json| settings_repository::set_value(&state.db, key, &json));
        if let Err(err) = saved {
            log::error!("Couldn't save {key}: {err}");
        }
    })
    .detach();
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
        save_workspace_ids(SHARED_WORKSPACES_KEY, &self.shared_workspaces, cx);
        if !shared && self.step_in_workspaces.remove(workspace_id) {
            save_workspace_ids(STEP_IN_WORKSPACES_KEY, &self.step_in_workspaces, cx);
        }
        self.update_sharing(cx);
        // Publishes straight away rather than on the next tick, so an
        // unshared workspace disappears for teammates at once.
        if self.server.is_some() {
            self.start_publishing(cx);
        }
        cx.notify();
    }

    fn set_workspace_step_in(&mut self, workspace_id: &str, allowed: bool, cx: &mut Context<Self>) {
        let changed = if allowed {
            self.shared_workspaces.contains(workspace_id)
                && self.step_in_workspaces.insert(workspace_id.to_string())
        } else {
            self.step_in_workspaces.remove(workspace_id)
        };
        if !changed {
            return;
        }
        save_workspace_ids(STEP_IN_WORKSPACES_KEY, &self.step_in_workspaces, cx);
        if self.server.is_some() {
            self.start_publishing(cx);
        }
        cx.notify();
    }

    fn allows_step_in(&self, workspace_id: &str) -> bool {
        self.shared_workspaces.contains(workspace_id)
            && self.step_in_workspaces.contains(workspace_id)
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
        let (action_sender, mut action_receiver) = mpsc::unbounded::<PendingAction>();
        self.action_task = Some(cx.spawn(async move |this, cx| {
            while let Some((action, reply)) = action_receiver.next().await {
                let outcome = cx.update(|cx| match this.upgrade() {
                    Some(team) => run_action(&team, action, cx),
                    None => Err(TeamActionError::Failed("Sharing stopped".into())),
                });
                if reply.send(outcome).is_err() {
                    log::warn!("A teammate's action finished after their request gave up");
                }
            }
        }));
        self.start_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let attempt = cx
                    .background_spawn({
                        let key = key.clone();
                        let hub = hub.clone();
                        let action_sender = action_sender.clone();
                        async move { start_server(key, hub, action_sender) }
                    })
                    .await;
                let started = this.update(cx, |this, cx| {
                    if this.server_key.as_ref() != Some(&key) {
                        return true;
                    }
                    let started = match attempt {
                        Ok((server, started_as)) => {
                            this.sharing = SharingStatus::On {
                                address: server.address(),
                            };
                            this.server = Some(server);
                            this.start_publishing(cx);
                            this.tailscale_watch =
                                Some(Self::watch_tailscale(key.clone(), started_as, cx));
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

    fn watch_tailscale(
        key: ServerKey,
        started_as: TailscaleSelf,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CHECK_TAILSCALE_EVERY).await;
                let now = cx.background_spawn(async move { tailscale_self() }).await;
                let moved = match &now {
                    Ok(now) => now.ip != started_as.ip || now.login != started_as.login,
                    Err(_) => true,
                };
                if !moved {
                    continue;
                }
                this.update(cx, |this, cx| {
                    if this.server_key.as_ref() == Some(&key) {
                        log::info!(
                            "Tailscale changed since sharing started on {}; restarting sharing",
                            started_as.ip
                        );
                        this.start_sharing(key.clone(), cx);
                        cx.notify();
                    }
                })
                .ok();
                break;
            }
        })
    }

    fn stop_sharing(&mut self) {
        self.start_task = None;
        self.publish_task = None;
        self.tailscale_watch = None;
        self.action_task = None;
        self.server = None;
        self.server_key = None;
        self.chat_watches.clear();
        self.saved_chats.clear();
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
                    let published = match records {
                        Ok(records) => this.update(cx, |this, cx| {
                            let to_read = this.publish(records, cx);
                            (this.hub.clone(), to_read)
                        }),
                        Err(err) => {
                            log::error!("Couldn't list workspaces to share: {err}");
                            this.read_with(cx, |this, _| (this.hub.clone(), Vec::new()))
                        }
                    };
                    let Ok((hub, to_read)) = published else {
                        break;
                    };
                    if !to_read.is_empty() {
                        let saved = cx
                            .background_spawn(async move { read_saved_transcripts(to_read) })
                            .await;
                        if this
                            .update(cx, |this, _| this.share_saved_chats(saved))
                            .is_err()
                        {
                            break;
                        }
                    }
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

    /// Shares the current state of the shared workspaces, and returns the
    /// saved transcripts to read for closed chats.
    fn publish(
        &mut self,
        mut records: SharedRecords,
        cx: &mut Context<Self>,
    ) -> Vec<TranscriptToRead> {
        if self.server.is_none() {
            return Vec::new();
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
        let mut step_in_chats = HashSet::new();
        let mut closed_chats = HashSet::new();
        let mut to_read = Vec::new();
        let mut workspaces = Vec::with_capacity(records.len());
        for (summary, chat_records) in records {
            let agents = live.get(&summary.id);
            let step_in = self.allows_step_in(&summary.id);
            let mut chats = Vec::with_capacity(chat_records.len());
            for (record, transcript) in chat_records {
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
                    open: thread.is_some() || self.hub.has_chat(&record.id),
                    live: thread.is_some(),
                });
                if let Some(thread) = thread {
                    if step_in {
                        step_in_chats.insert(record.id.clone());
                    }
                    open_chats.push((record.id, thread));
                    continue;
                }
                // A chat loaded earlier in this session keeps what it showed
                // then, which says more than its saved transcript.
                let shown_live =
                    self.hub.has_chat(&record.id) && !self.saved_chats.contains_key(&record.id);
                if let Some((path, modified)) = transcript {
                    if !shown_live && self.saved_chats.get(&record.id) != Some(&modified) {
                        to_read.push(TranscriptToRead {
                            workspace_id: summary.id.clone(),
                            chat_id: record.id.clone(),
                            path,
                            modified,
                        });
                    }
                }
                closed_chats.insert(record.id);
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
                step_in,
                id: summary.id,
                project: summary.repo,
                name: summary.name,
                branch: summary.branch,
            });
        }
        self.hub.set_workspaces(workspaces);
        self.hub.set_step_in_chats(step_in_chats);
        self.watch_chats(open_chats, &closed_chats, cx);
        to_read
    }

    /// Follows the loaded chats. Closed chats keep what they last showed,
    /// and chats no longer shared are dropped.
    fn watch_chats(
        &mut self,
        open_chats: Vec<(String, Entity<AcpThread>)>,
        closed_chats: &HashSet<String>,
        cx: &mut Context<Self>,
    ) {
        let open_ids: HashSet<String> = open_chats.iter().map(|(id, _)| id.clone()).collect();
        self.chat_watches.retain(|id, _| open_ids.contains(id));
        self.saved_chats.retain(|id, _| closed_chats.contains(id));
        self.hub
            .retain_chats(|id| open_ids.contains(id) || closed_chats.contains(id));
        for (chat_id, thread) in open_chats {
            let watched = self
                .chat_watches
                .get(&chat_id)
                .is_some_and(|watch| watch.thread_id == thread.entity_id());
            if watched && self.hub.has_chat(&chat_id) {
                // A step-in switch flipped since the last event changes what
                // is shared without the thread saying anything.
                self.publish_activity(&chat_id, thread.read(cx));
                continue;
            }
            self.replace_chat(&chat_id, thread.read(cx), cx);
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
        // A plan update raises no thread event of its own, so the plan is
        // re-checked on every event. An unchanged plan wakes nobody.
        let published = published
            && self.hub.set_chat_plan(chat_id, thread_plan(thread, cx))
            && self.publish_activity(chat_id, thread);
        if !published {
            self.replace_chat(chat_id, thread, cx);
        }
    }

    /// Shares closed chats from the transcripts just read, unless meanwhile
    /// a chat was loaded or its workspace unshared.
    fn share_saved_chats(&mut self, saved: Vec<(TranscriptToRead, Vec<TeamEntry>)>) {
        if self.server.is_none() {
            return;
        }
        for (transcript, entries) in saved {
            let shown_live = self.hub.has_chat(&transcript.chat_id)
                && !self.saved_chats.contains_key(&transcript.chat_id);
            if shown_live
                || self.chat_watches.contains_key(&transcript.chat_id)
                || !self.shared_workspaces.contains(&transcript.workspace_id)
            {
                continue;
            }
            self.hub.replace_chat(&transcript.chat_id, entries);
            self.saved_chats
                .insert(transcript.chat_id, transcript.modified);
        }
    }

    fn replace_chat(&self, chat_id: &str, thread: &AcpThread, cx: &App) {
        self.hub.replace_chat(chat_id, thread_entries(thread, cx));
        self.hub.set_chat_plan(chat_id, thread_plan(thread, cx));
        self.publish_activity(chat_id, thread);
    }

    /// Shares whether a chat's agent is mid-turn and, when teammates may
    /// step in, what it is waiting on. Returns false when the chat isn't
    /// known.
    fn publish_activity(&self, chat_id: &str, thread: &AcpThread) -> bool {
        let permissions = if self.hub.allows_step_in(chat_id) {
            thread_permissions(thread)
        } else {
            Vec::new()
        };
        self.hub.set_chat_activity(
            chat_id,
            thread.status() == ThreadStatus::Generating,
            permissions,
        )
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

fn start_server(
    key: ServerKey,
    hub: TeamShareHub,
    action_sender: mpsc::UnboundedSender<PendingAction>,
) -> Result<(TeamShareServer, TailscaleSelf), String> {
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
    // Runs on a request thread, which waits here while the main thread
    // carries the action out.
    let action_handler: ActionHandler = Arc::new(move |action| {
        let (reply_sender, reply_receiver) = std::sync::mpsc::channel();
        if action_sender
            .unbounded_send((action, reply_sender))
            .is_err()
        {
            return Err(TeamActionError::Failed("Sharing stopped".into()));
        }
        match reply_receiver.recv_timeout(ACTION_TIMEOUT) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => Err(TeamActionError::Failed("Timed out".into())),
            Err(RecvTimeoutError::Disconnected) => {
                Err(TeamActionError::Failed("Sharing stopped".into()))
            }
        }
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
        action_handler,
    )
    .map(|server| (server, me))
}

/// Carries out a teammate's action on the chat it names, checking again
/// here that they may.
fn run_action(
    team: &Entity<TeamShare>,
    action: TeamAction,
    cx: &mut App,
) -> Result<TeamActionOutcome, TeamActionError> {
    let Some(chat) = find_open_chat(&action.chat_id, cx) else {
        return Err(TeamActionError::NotLoaded);
    };
    if !team.read(cx).allows_step_in(&chat.workspace_id) {
        return Err(TeamActionError::NotAllowed(
            "Stepping in is off for this workspace".into(),
        ));
    }
    let Some(thread) = chat.thread else {
        return Err(TeamActionError::NotLoaded);
    };
    let idle = thread.read(cx).status() == ThreadStatus::Idle;
    match action.kind {
        TeamActionKind::Stop => {
            if idle {
                return Err(TeamActionError::NothingToDo(
                    "The agent isn't running".into(),
                ));
            }
            chat.view
                .update(cx, |view, cx| view.stop_chat(&action.chat_id, cx));
            show_step_in_notice(
                &chat.window,
                format!("{} stopped the agent in {}", action.from, chat.title),
                cx,
            );
            Ok(TeamActionOutcome { queued: false })
        }
        TeamActionKind::Message { text } => {
            let text = teammate_message_text(&action.from, &text);
            chat.view.update(cx, |view, cx| {
                view.queue_chat_message(&action.chat_id, text, cx)
            });
            Ok(TeamActionOutcome { queued: !idle })
        }
        TeamActionKind::Permission {
            tool_call_id,
            option_id,
        } => {
            let tool_title = answer_permission(&thread, &tool_call_id, &option_id, cx)?;
            let rejected = thread
                .read(cx)
                .tool_call(&acp::ToolCallId::new(tool_call_id))
                .is_some_and(|(_, tool_call)| matches!(tool_call.status, ToolCallStatus::Rejected));
            let answer = if rejected { "rejected" } else { "allowed" };
            show_step_in_notice(
                &chat.window,
                format!("{} {answer} {tool_title} in {}", action.from, chat.title),
                cx,
            );
            Ok(TeamActionOutcome { queued: false })
        }
    }
}

fn show_step_in_notice(window: &Entity<Workspace>, message: String, cx: &mut App) {
    window.update(cx, |workspace, cx| {
        workspace.show_toast(
            Toast::new(NotificationId::unique::<TeamStepInToast>(), message),
            cx,
        );
    });
}

fn teammate_message_text(from: &str, text: &str) -> String {
    format!("From {from} (teammate):\n\n{text}")
}

/// The options a teammate may pick from: only once-only answers, as an
/// "always" answer leaves a standing rule in the owner's agent, which is
/// theirs to make.
fn once_only_options(options: &PermissionOptions) -> Vec<acp::PermissionOption> {
    let candidates: Vec<&acp::PermissionOption> = match options {
        PermissionOptions::Flat(options) => options.iter().collect(),
        PermissionOptions::Dropdown(choices)
        | PermissionOptions::DropdownWithPatterns { choices, .. } => choices
            .iter()
            .flat_map(|choice| [&choice.allow, &choice.deny])
            .collect(),
    };
    let mut offered: Vec<acp::PermissionOption> = Vec::new();
    for option in candidates {
        let once_only = matches!(
            option.kind,
            acp::PermissionOptionKind::AllowOnce | acp::PermissionOptionKind::RejectOnce
        );
        if once_only
            && !offered
                .iter()
                .any(|offered| offered.option_id == option.option_id)
        {
            offered.push(option.clone());
        }
    }
    offered
}

fn thread_permissions(thread: &AcpThread) -> Vec<TeamPermissionRequest> {
    thread
        .entries()
        .iter()
        .filter_map(|entry| {
            let AgentThreadEntry::ToolCall(ToolCall {
                id,
                status: ToolCallStatus::WaitingForConfirmation { options, .. },
                ..
            }) = entry
            else {
                return None;
            };
            let options = once_only_options(options);
            (!options.is_empty()).then(|| TeamPermissionRequest {
                tool_call_id: id.0.to_string(),
                options,
            })
        })
        .collect()
}

/// Answers a waiting permission request with an option it offers
/// teammates, and returns the tool call's title.
fn answer_permission(
    thread: &Entity<AcpThread>,
    tool_call_id: &str,
    option_id: &str,
    cx: &mut App,
) -> Result<String, TeamActionError> {
    let tool_call_id = acp::ToolCallId::new(tool_call_id.to_string());
    let (option, title) = {
        let thread = thread.read(cx);
        let no_longer_waiting =
            || TeamActionError::NothingToDo("The permission request is no longer waiting".into());
        let Some((_, tool_call)) = thread.tool_call(&tool_call_id) else {
            return Err(no_longer_waiting());
        };
        let ToolCallStatus::WaitingForConfirmation { options, .. } = &tool_call.status else {
            return Err(no_longer_waiting());
        };
        let Some(option) = once_only_options(options)
            .into_iter()
            .find(|option| option.option_id.0.as_ref() == option_id)
        else {
            return Err(TeamActionError::NotAllowed(
                "That answer isn't offered to teammates".into(),
            ));
        };
        (option, tool_call_title(tool_call, cx))
    };
    thread.update(cx, |thread, cx| {
        thread.authorize_tool_call(
            tool_call_id,
            SelectedPermissionOutcome::new(option.option_id, option.kind),
            cx,
        )
    });
    Ok(title)
}

fn load_shared_records(
    state: &mnemonic_core::state::AppState,
    shared_ids: &HashSet<String>,
) -> Result<SharedRecords, String> {
    workspace_service::list_workspaces(state)?
        .into_iter()
        .filter(|summary| shared_ids.contains(&summary.id))
        .map(|summary| {
            let chats = chat_session_repository::list_for_workspace(&state.db, &summary.id)?
                .into_iter()
                .map(|record| {
                    let transcript = chat_transcript_service::saved_transcript_file(
                        state,
                        &summary.id,
                        &record.id,
                    );
                    (record, transcript)
                })
                .collect();
            Ok((summary, chats))
        })
        .collect()
}

fn read_saved_transcripts(
    to_read: Vec<TranscriptToRead>,
) -> Vec<(TranscriptToRead, Vec<TeamEntry>)> {
    to_read
        .into_iter()
        .filter_map(
            |transcript| match std::fs::read_to_string(&transcript.path) {
                Ok(markdown) => {
                    let entries = saved_chat_entries(&markdown);
                    Some((transcript, entries))
                }
                Err(err) => {
                    log::warn!("Couldn't read {}: {err}", transcript.path.display());
                    None
                }
            },
        )
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

fn thread_plan(thread: &AcpThread, cx: &App) -> Vec<acp::PlanEntry> {
    thread
        .plan()
        .entries
        .iter()
        .map(|entry| acp_plan_entry(entry, cx))
        .collect()
}

fn acp_plan_entry(entry: &PlanEntry, cx: &App) -> acp::PlanEntry {
    acp::PlanEntry::new(
        entry.content.read(cx).source().to_string(),
        entry.priority.clone(),
        entry.status.clone(),
    )
}

pub(crate) fn team_entry(entry: &AgentThreadEntry, cx: &App) -> TeamEntry {
    match entry {
        AgentThreadEntry::UserMessage(message) => TeamEntry::Updates {
            updates: message
                .chunks
                .iter()
                .map(|chunk| {
                    acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(chunk.clone()))
                })
                .collect(),
        },
        AgentThreadEntry::AssistantMessage(message) => TeamEntry::Updates {
            updates: message
                .chunks
                .iter()
                .map(|chunk| match chunk {
                    AssistantMessageChunk::Message { block } => {
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            block.to_markdown(cx).into(),
                        ))
                    }
                    AssistantMessageChunk::Thought { block } => {
                        acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(
                            block.to_markdown(cx).into(),
                        ))
                    }
                })
                .collect(),
        },
        AgentThreadEntry::ToolCall(tool_call) => TeamEntry::Updates {
            updates: vec![acp::SessionUpdate::ToolCall(acp_tool_call(tool_call, cx))],
        },
        AgentThreadEntry::CompletedPlan(entries) => TeamEntry::CompletedPlan {
            entries: entries
                .iter()
                .map(|entry| acp_plan_entry(entry, cx))
                .collect(),
        },
        AgentThreadEntry::ContextCompaction(compaction) => TeamEntry::Compaction {
            summary: compaction
                .summary
                .as_ref()
                .map(|summary| summary.read(cx).source().to_string()),
        },
    }
}

fn acp_tool_call(tool_call: &ToolCall, cx: &App) -> acp::ToolCall {
    let status = match &tool_call.status {
        ToolCallStatus::Pending | ToolCallStatus::WaitingForConfirmation { .. } => {
            acp::ToolCallStatus::Pending
        }
        ToolCallStatus::InProgress => acp::ToolCallStatus::InProgress,
        ToolCallStatus::Completed => acp::ToolCallStatus::Completed,
        ToolCallStatus::Failed | ToolCallStatus::Rejected | ToolCallStatus::Canceled => {
            acp::ToolCallStatus::Failed
        }
    };
    let content = tool_call
        .content
        .iter()
        .filter_map(|content| match content {
            ToolCallContent::ContentBlock(block) => Some(acp::ToolCallContent::Content(
                acp::Content::new(block.to_markdown(cx)),
            )),
            ToolCallContent::Diff(diff) => {
                let diff = diff.read(cx);
                let old_text = diff.base_text();
                Some(acp::ToolCallContent::Diff(
                    acp::Diff::new(diff.file_path(cx)?, diff.buffer().read(cx).text())
                        // A new file's diff has an empty base, which ACP
                        // marks as no old text.
                        .old_text((!old_text.is_empty()).then(|| old_text.to_string())),
                ))
            }
            ToolCallContent::Terminal(terminal) => {
                let output = terminal.read(cx).inner().read(cx).get_content();
                Some(acp::ToolCallContent::Content(acp::Content::new(
                    MarkdownCodeBlock {
                        tag: "",
                        text: &output,
                    }
                    .to_string(),
                )))
            }
        })
        .collect();
    acp::ToolCall::new(tool_call.id.clone(), tool_call_title(tool_call, cx))
    .kind(tool_call.kind)
    .status(status)
    .content(content)
    .locations(tool_call.locations.clone())
    .raw_input(tool_call.raw_input.clone())
    .raw_output(tool_call.raw_output.clone())
    .meta(tool_call_meta(tool_call))
}

fn tool_call_title(tool_call: &ToolCall, cx: &App) -> String {
    let label = tool_call.label.read(cx).source();
    // The thread escapes an edit's title into its label, and the viewer's
    // thread escapes what it receives, so the original title is sent.
    if tool_call.kind == acp::ToolKind::Edit {
        unescape_markdown(label)
    } else {
        label.to_string()
    }
}

/// Reverses `util::markdown::MarkdownEscaped`, which puts a backslash before
/// every punctuation character it escapes (backslashes included) and writes
/// `<` and `>` as entities. Its `&` is always escaped, so a bare `&` can only
/// start one of those two entities.
fn unescape_markdown(escaped: &str) -> String {
    let mut text = String::with_capacity(escaped.len());
    let mut rest = escaped;
    while let Some(character) = rest.chars().next() {
        if let Some(entity_rest) = rest.strip_prefix("&lt;") {
            text.push('<');
            rest = entity_rest;
            continue;
        }
        if let Some(entity_rest) = rest.strip_prefix("&gt;") {
            text.push('>');
            rest = entity_rest;
            continue;
        }
        rest = &rest[character.len_utf8()..];
        if character == '\\' {
            if let Some(escaped_character) = rest.chars().next() {
                text.push(escaped_character);
                rest = &rest[escaped_character.len_utf8()..];
                continue;
            }
        }
        text.push(character);
    }
    text
}

/// `ToolCall` keeps only the meta keys it reads, so those are rebuilt.
fn tool_call_meta(tool_call: &ToolCall) -> Option<acp::Meta> {
    let mut meta = acp::Meta::new();
    if let Some(tool_name) = &tool_call.tool_name {
        meta.insert(TOOL_NAME_META_KEY.into(), tool_name.to_string().into());
    }
    if let Some(value) = tool_call
        .subagent_session_info
        .as_ref()
        .and_then(|info| serde_json::to_value(info).log_err())
    {
        meta.insert(SUBAGENT_SESSION_INFO_META_KEY.into(), value);
    }
    if let Some(value) = tool_call
        .sandbox_authorization_details
        .as_ref()
        .and_then(|details| serde_json::to_value(details).log_err())
    {
        meta.insert(SANDBOX_AUTHORIZATION_META_KEY.into(), value);
    }
    (!meta.is_empty()).then_some(meta)
}

/// The sidebar's Team section: each teammate and their workspaces, with a
/// way into the Team window. Selecting a teammate's workspace opens it in
/// this window, the way your own workspaces open.
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
        move |_: &gpui::ClickEvent, _window: &mut gpui::Window, cx: &mut App| {
            workspace
                .update(cx, |workspace, cx| open_team_window_from(workspace, cx))
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
            PeerStatus::Failed(TeamClientError::Incompatible { .. }) => Some(format!(
                "{host} runs a different Mnemonic version; update both Mnemonics to the same version."
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
        let mut projects: Vec<(&str, Vec<&TeamWorkspace>)> = Vec::new();
        for shared in &peer.workspaces {
            match projects
                .iter_mut()
                .find(|(project, _)| *project == shared.project)
            {
                Some((_, workspaces)) => workspaces.push(shared),
                None => projects.push((&shared.project, vec![shared])),
            }
        }
        for (project, workspaces) in projects {
            let project_key = (host.clone(), project.to_string());
            let project_collapsed = team_state.collapsed_peer_projects.contains(&project_key);
            section = section.child(
                h_flex()
                    .id(SharedString::from(format!("team-{host}-project-{project}")))
                    .pl(px(22.))
                    .pr_2()
                    .h(px(24.))
                    .gap_1()
                    .items_center()
                    .w_full()
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .on_click({
                        let team = team.clone();
                        move |_, _, cx| {
                            team.update(cx, |team, cx| {
                                if !team.collapsed_peer_projects.remove(&project_key) {
                                    team.collapsed_peer_projects.insert(project_key.clone());
                                }
                                cx.notify();
                            });
                        }
                    })
                    .child(
                        Icon::new(if project_collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        div().overflow_hidden().child(
                            Label::new(project.to_string())
                                .size(LabelSize::Small)
                                .single_line(),
                        ),
                    )
                    .child(
                        Label::new(workspaces.len().to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            );
            if project_collapsed {
                continue;
            }
            for shared in workspaces {
                section = section.child(render_peer_workspace_row(
                    &host,
                    shared,
                    workspace.clone(),
                    cx,
                ));
            }
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
    let is_selected = mnemonic_bridge::ActiveWorkspace::get(workspace.entity_id(), cx)
        .is_some_and(|selected| selected == teammate_workspace_key(&host, &workspace_id));
    h_flex()
        .id(SharedString::from(format!("team-{host}-{}", shared.id)))
        .pl(px(42.))
        .pr_2()
        .h(px(24.))
        .gap_1p5()
        .items_center()
        .w_full()
        .cursor_pointer()
        .rounded_md()
        .when(is_selected, |row| row.bg(colors.ghost_element_selected))
        .hover(|style| style.bg(colors.ghost_element_hover))
        .tooltip(Tooltip::text(tooltip))
        .on_click(move |_, window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            let Some(panel) = workspace.read(cx).panel::<WorkspacePanel>(cx) else {
                return;
            };
            panel.update(cx, |panel, cx| {
                panel.select_teammate(host.clone(), workspace_id.clone(), window, cx);
            });
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

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::{
        AgentConnection as _, AuthorizationKind, PermissionOptions, RequestPermissionOutcome,
        StubAgentConnection,
    };
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use std::rc::Rc;
    use util::path_list::PathList;

    async fn new_thread(cx: &mut TestAppContext) -> Entity<AcpThread> {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let connection = Rc::new(StubAgentConnection::new());
        cx.update(|cx| connection.new_session(project, PathList::default(), cx))
            .await
            .expect("the stub connection starts a session")
    }

    #[gpui::test]
    async fn team_entries_rebuild_the_same_thread(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let shared = new_thread(cx).await;
        let tool_call_id = acp::ToolCallId::new("edit-login");
        let session_updates = vec![
            acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(
                "Fix the login page".into(),
            )),
            acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(" please".into())),
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new("Looking at".into())),
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(" the form".into())),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("I'll fix".into())),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(" the form.".into())),
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(tool_call_id.clone(), "Edit team_share.rs")
                    .kind(acp::ToolKind::Edit)
                    .status(acp::ToolCallStatus::InProgress)
                    .raw_input(serde_json::json!({ "path": "/project/login.rs" })),
            ),
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                tool_call_id,
                acp::ToolCallUpdateFields::new()
                    .status(acp::ToolCallStatus::Completed)
                    .content(vec![
                        acp::ToolCallContent::Diff(
                            acp::Diff::new("/project/login.rs", "let valid = true;\n")
                                .old_text("let valid = false;\n".to_string()),
                        ),
                        acp::ToolCallContent::Content(acp::Content::new("Saved login.rs")),
                    ]),
            )),
        ];
        shared.update(cx, |thread, cx| {
            session_updates
                .into_iter()
                .try_for_each(|update| thread.handle_session_update(update, cx))
        })
        .expect("the shared thread takes every update");
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();

        let team_entries = shared.read_with(cx, |thread, cx| thread_entries(thread, cx));
        let mirror = new_thread(cx).await;
        mirror.update(cx, |thread, cx| {
            team_entries
                .into_iter()
                .flat_map(|entry| match entry {
                    TeamEntry::Updates { updates } => updates,
                    _ => Vec::new(),
                })
                .try_for_each(|update| thread.handle_session_update(update, cx))
        })
        .expect("the mirror takes every update");
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();

        let shared_markdown = shared.read_with(cx, |thread, cx| thread.to_markdown(cx));
        let mirror_markdown = mirror.read_with(cx, |thread, cx| thread.to_markdown(cx));
        assert!(shared_markdown.contains("let valid = true;"));
        assert_eq!(mirror_markdown, shared_markdown);
    }

    /// A thread with one tool call waiting on `options`, and the task that
    /// receives the answer, which must be kept for the request to stay open.
    async fn thread_waiting_on(
        options: PermissionOptions,
        cx: &mut TestAppContext,
    ) -> (Entity<AcpThread>, Task<RequestPermissionOutcome>) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let thread = new_thread(cx).await;
        let tool_call_id = acp::ToolCallId::new("run-tests");
        thread
            .update(cx, |thread, cx| {
                thread.handle_session_update(
                    acp::SessionUpdate::ToolCall(
                        acp::ToolCall::new(tool_call_id.clone(), "Run the tests")
                            .kind(acp::ToolKind::Execute),
                    ),
                    cx,
                )
            })
            .expect("the thread takes the tool call");
        let answer = thread
            .update(cx, |thread, cx| {
                thread.request_tool_call_authorization(
                    acp::ToolCallUpdate::new(tool_call_id, acp::ToolCallUpdateFields::new()),
                    options,
                    AuthorizationKind::PermissionGrant,
                    cx,
                )
            })
            .expect("the thread asks for permission");
        (thread, answer)
    }

    fn allow_always_and_once_options() -> PermissionOptions {
        PermissionOptions::Flat(vec![
            acp::PermissionOption::new("allow", "Allow", acp::PermissionOptionKind::AllowOnce),
            acp::PermissionOption::new(
                "always",
                "Always allow",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new("reject", "Reject", acp::PermissionOptionKind::RejectOnce),
        ])
    }

    fn is_waiting(thread: &Entity<AcpThread>, cx: &mut TestAppContext) -> bool {
        thread.read_with(cx, |thread, _| {
            thread
                .tool_call(&acp::ToolCallId::new("run-tests"))
                .is_some_and(|(_, tool_call)| {
                    matches!(
                        tool_call.status,
                        ToolCallStatus::WaitingForConfirmation { .. }
                    )
                })
        })
    }

    #[gpui::test]
    async fn only_once_only_options_are_offered_to_teammates(cx: &mut TestAppContext) {
        let (thread, _answer) = thread_waiting_on(allow_always_and_once_options(), cx).await;
        let requests = thread.read_with(cx, |thread, _| thread_permissions(thread));
        let offered: Vec<Vec<String>> = requests
            .iter()
            .map(|request| {
                request
                    .options
                    .iter()
                    .map(|option| option.option_id.0.to_string())
                    .collect()
            })
            .collect();
        assert_eq!(
            offered,
            vec![vec!["allow".to_string(), "reject".to_string()]]
        );
        assert_eq!(requests[0].tool_call_id, "run-tests");

        let (always_only, _answer) = thread_waiting_on(
            PermissionOptions::Flat(vec![acp::PermissionOption::new(
                "always",
                "Always allow",
                acp::PermissionOptionKind::AllowAlways,
            )]),
            cx,
        )
        .await;
        let requests = always_only.read_with(cx, |thread, _| thread_permissions(thread));
        assert!(requests.is_empty());
    }

    #[gpui::test]
    async fn a_teammate_can_only_pick_an_offered_once_only_option(cx: &mut TestAppContext) {
        let (thread, _answer) = thread_waiting_on(allow_always_and_once_options(), cx).await;

        for option_id in ["always", "never-offered"] {
            let answered = cx.update(|cx| answer_permission(&thread, "run-tests", option_id, cx));
            assert!(
                matches!(answered, Err(TeamActionError::NotAllowed(_))),
                "{option_id} gave {answered:?}"
            );
            assert!(is_waiting(&thread, cx));
        }

        let answered = cx.update(|cx| answer_permission(&thread, "run-tests", "allow", cx));
        assert_eq!(answered, Ok("Run the tests".to_string()));
        assert!(!is_waiting(&thread, cx));

        let answered = cx.update(|cx| answer_permission(&thread, "run-tests", "allow", cx));
        assert!(matches!(answered, Err(TeamActionError::NothingToDo(_))));
    }

    #[test]
    fn a_teammates_message_carries_their_name() {
        assert_eq!(
            teammate_message_text("Dan", "try the other branch"),
            "From Dan (teammate):\n\ntry the other branch"
        );
    }
}
