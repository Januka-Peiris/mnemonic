//! Sharing workspaces with teammates over Tailscale, and following theirs.
//! Workspaces are shared one at a time. This side runs the share server
//! while any workspace is shared, keeps what it
//! serves up to date, and polls each teammate listed as a peer.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use acp_thread::{
    AcpThread, AgentThreadEntry, AssistantMessageChunk, AuthorizationKind, PermissionOptions,
    PlanEntry, SANDBOX_AUTHORIZATION_META_KEY, SUBAGENT_SESSION_INFO_META_KEY,
    SelectedPermissionOutcome, TOOL_NAME_META_KEY, ThreadStatus, ToolCall, ToolCallContent,
    ToolCallStatus,
};
use agent_client_protocol::schema as acp;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Global, IntoElement, SharedString, Task,
    WeakEntity, px,
};
use mnemonic_bridge::AgentAttention;
use mnemonic_core::models::WorkspaceSummary;
use mnemonic_core::repositories::settings_repository;
use mnemonic_core::services::git_review_service;
use mnemonic_core::services::team_files_service::{
    self, HashCache, SkipList, TeamBatchOutcome, normalize_remote_url, scan_workspace_cached,
};
use mnemonic_core::services::team_share_server::{
    AgentWriteCheck, BundleSource, DiffSource, FileSource, SkipListSource, TeamServerConfig,
    TeamShareServer,
};
use mnemonic_core::services::team_share_service::{
    ActionHandler, BatchHandler, DEFAULT_TEAM_PORT, TailscaleSelf, TailscaleWhois, TeamAction,
    TeamActionError, TeamActionKind, TeamActionOutcome, TeamAgentState, TeamBatchRequest,
    TeamCaller, TeamClient, TeamClientError, TeamEntry, TeamPermissionRequest, TeamShareHub,
    TeamWorkspace, tailscale_self,
};
use settings::{RegisterSetting, Settings, SettingsStore};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use util::markdown::MarkdownCodeBlock;
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};

use crate::chat_feed::{
    ChatFeed, PermissionScope, SharedRecords, TranscriptToRead, WorkspaceFlags, load_records,
    read_saved_transcripts,
};
use crate::team_copy::{CopyFollow, resume_copies};
use crate::team_live_chat::live_chats_root;
use crate::team_settings::open_team_window;
use crate::team_workspace_view::teammate_workspace_key;
use crate::workspace_panel::WorkspacePanel;
use crate::workspace_view::{
    LiveWorkspaceAgents, find_open_chat, find_workspace_window, open_windows,
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
/// How long a teammate's batch of file changes waits to be applied, longer
/// than an action as a batch can carry large files.
const BATCH_TIMEOUT: Duration = Duration::from_secs(120);

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

/// A teammate's action on its way to the main thread, with where to send
/// the answer.
struct PendingAction {
    action: TeamAction,
    /// Set by whichever comes first: the main thread starting the action,
    /// or the request giving up on it. An action its requester was told
    /// failed must not run later.
    claimed: Arc<AtomicBool>,
    reply: std::sync::mpsc::Sender<Result<TeamActionOutcome, TeamActionError>>,
}

/// A teammate's agent about to write a file, waiting for the main thread to
/// say whether the sharer has unsaved edits in it.
struct PendingWriteCheck {
    root: PathBuf,
    path: PathBuf,
    display_name: String,
    claimed: Arc<AtomicBool>,
    reply: std::sync::mpsc::Sender<Result<(), TeamActionError>>,
}

/// A teammate's batch of file changes on its way to the main thread, with
/// the same claim as `PendingAction`.
struct PendingBatch {
    request: TeamBatchRequest,
    claimed: Arc<AtomicBool>,
    reply: std::sync::mpsc::Sender<Result<TeamBatchOutcome, TeamActionError>>,
}

/// Marker for the notices shown when a teammate acts in a chat.
struct TeamStepInToast;

pub(crate) struct TeamShare {
    applied: Option<TeamSettings>,
    /// Publishes shared workspaces and their chats into the hub the share
    /// server serves, replaced with each new server.
    feed: Entity<ChatFeed>,
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
    /// Applies teammates' batches of file changes, one at a time.
    batch_task: Option<Task<()>>,
    /// Checks teammates' agents' writes against unsaved editor changes.
    write_check_task: Option<Task<()>>,
    pub peers: Vec<PeerState>,
    pub collapsed_peers: HashSet<String>,
    /// A teammate's projects folded in the sidebar, as (host, project).
    collapsed_peer_projects: HashSet<(String, String)>,
    /// Workspaces the user chose to share, saved in the core's settings
    /// table so the choice survives restarts.
    shared_workspaces: HashSet<String>,
    /// What the share server's request threads read to find a workspace's
    /// files: only workspaces with Share files on, as (root, skip list).
    file_roots: FileRoots,
    /// What the share server's request threads read to find the folder a
    /// teammate's agent works in: only workspaces with agents on.
    agent_roots: AgentRoots,
    /// Each file-sharing workspace's normalized `origin`, looked up once.
    remote_urls: HashMap<String, Option<String>>,
    scan_task: Option<Task<()>>,
    /// This machine's copies of teammates' workspaces, by local workspace id.
    pub copies: HashMap<String, CopyFollow>,
    /// Bumped each time a teammate's batch is recorded, so views listing
    /// batches know to reload them.
    pub batch_generation: u64,
}

type FileRoots = Arc<Mutex<HashMap<String, (PathBuf, SkipList)>>>;
type AgentRoots = Arc<Mutex<HashMap<String, PathBuf>>>;

fn lock_agent_roots(
    agent_roots: &AgentRoots,
) -> std::sync::MutexGuard<'_, HashMap<String, PathBuf>> {
    agent_roots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_file_roots(
    file_roots: &FileRoots,
) -> std::sync::MutexGuard<'_, HashMap<String, (PathBuf, SkipList)>> {
    file_roots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

fn open_team_window_from(workspace: &Workspace, cx: &mut App) {
    open_team_window(workspace.app_state().fs.clone(), cx);
}

pub fn init(cx: &mut App) {
    if let Some(app) = mnemonic_bridge::mnemonic_state_cloned() {
        // A live chat opened any other way, such as from imported thread
        // history, would run without the share server's tools and with
        // Claude's built-in ones acting on this Mac.
        agent_servers::require_session_extras_under(live_chats_root(&app.data_dir), cx);
    }
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenTeamSettings, _window, cx| {
            open_team_window_from(workspace, cx);
        });
    })
    .detach();
    let team = cx.new(|cx| {
        let mut team = TeamShare::new(cx);
        team.apply_settings(cx);
        // Teammates' commands run in their own process group, so they would
        // outlive this app unless the server is stopped first.
        cx.on_app_quit(|team, cx| {
            team.stop_sharing(cx);
            async {}
        })
        .detach();
        team
    });
    cx.set_global(GlobalTeamShare(team.clone()));
    resume_copies(&team, cx);
    cx.observe_global::<SettingsStore>(move |cx| {
        team.update(cx, |team, cx| team.apply_settings(cx));
    })
    .detach();
}

impl TeamShare {
    fn new(cx: &mut Context<Self>) -> Self {
        let shared_workspaces = load_workspace_ids(SHARED_WORKSPACES_KEY);
        Self {
            applied: None,
            feed: new_team_feed(TeamShareHub::new(), cx),
            server: None,
            server_key: None,
            sharing: SharingStatus::Off,
            start_task: None,
            publish_task: None,
            tailscale_watch: None,
            action_task: None,
            batch_task: None,
            write_check_task: None,
            peers: Vec::new(),
            collapsed_peers: HashSet::new(),
            collapsed_peer_projects: HashSet::new(),
            shared_workspaces,
            file_roots: Arc::new(Mutex::new(HashMap::new())),
            agent_roots: Arc::new(Mutex::new(HashMap::new())),
            remote_urls: HashMap::new(),
            scan_task: None,
            copies: HashMap::new(),
            batch_generation: 0,
        }
    }
}

fn new_team_feed(hub: TeamShareHub, cx: &mut Context<TeamShare>) -> Entity<ChatFeed> {
    cx.new(|_| ChatFeed::new(hub, PermissionScope::StepInOnceOnly))
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
        if !shared {
            self.stop_agents(workspace_id);
            self.stop_sharing_files(workspace_id, cx);
        }
        self.update_sharing(cx);
        // Publishes straight away rather than on the next tick, so an
        // unshared workspace disappears for teammates at once.
        if self.server.is_some() {
            self.start_publishing(cx);
        }
        cx.notify();
    }

    /// Takes the workspace's folder away from teammates' agents and kills
    /// the commands they are running there.
    fn stop_agents(&mut self, workspace_id: &str) {
        lock_agent_roots(&self.agent_roots).remove(workspace_id);
        if let Some(server) = &self.server {
            server.kill_workspace_commands(workspace_id);
        }
    }

    /// Takes the workspace's files away from teammates at once.
    fn stop_sharing_files(&mut self, workspace_id: &str, cx: &App) {
        lock_file_roots(&self.file_roots).remove(workspace_id);
        self.remote_urls.remove(workspace_id);
        self.feed.read(cx).hub().remove_file_manifest(workspace_id);
    }

    fn shares_files(&self, workspace_id: &str) -> bool {
        self.shared_workspaces.contains(workspace_id)
    }

    fn allows_file_edits(&self, workspace_id: &str) -> bool {
        self.shared_workspaces.contains(workspace_id)
    }

    fn allows_agents(&self, workspace_id: &str) -> bool {
        self.shared_workspaces.contains(workspace_id)
    }

    fn allows_step_in(&self, workspace_id: &str) -> bool {
        self.shared_workspaces.contains(workspace_id)
    }

    /// Runs the share server exactly while some workspace is shared,
    /// restarting it when the settings it was started with change.
    fn update_sharing(&mut self, cx: &mut Context<Self>) {
        let Some(settings) = self.applied.clone() else {
            return;
        };
        if self.shared_workspaces.is_empty() {
            if self.server_key.is_some() {
                self.stop_sharing(cx);
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
        self.stop_sharing(cx);
        self.server_key = Some(key.clone());
        self.sharing = SharingStatus::Starting;
        let hub = TeamShareHub::new();
        self.feed = new_team_feed(hub.clone(), cx);
        let (action_sender, mut action_receiver) = mpsc::unbounded::<PendingAction>();
        let shared_file_roots = self.file_roots.clone();
        let shared_agent_roots = self.agent_roots.clone();
        let (write_check_sender, mut write_check_receiver) = mpsc::unbounded::<PendingWriteCheck>();
        self.write_check_task = Some(cx.spawn(async move |_, cx| {
            while let Some(pending) = write_check_receiver.next().await {
                if pending.claimed.swap(true, Ordering::SeqCst) {
                    log::info!("Skipped a write check that timed out while waiting");
                    continue;
                }
                let PendingWriteCheck {
                    root,
                    path,
                    display_name,
                    reply,
                    ..
                } = pending;
                let outcome = cx.update(|cx| check_agent_write(&root, &path, &display_name, cx));
                if reply.send(outcome).is_err() {
                    log::warn!("A write check finished after its request gave up");
                }
            }
        }));
        let (batch_sender, mut batch_receiver) = mpsc::unbounded::<PendingBatch>();
        self.batch_task = Some(cx.spawn(async move |this, cx| {
            while let Some(pending) = batch_receiver.next().await {
                if pending.claimed.swap(true, Ordering::SeqCst) {
                    log::info!("Skipped a teammate's batch that timed out while waiting");
                    continue;
                }
                let PendingBatch { request, reply, .. } = pending;
                let outcome = apply_teammate_batch(&this, request, cx).await;
                if reply.send(outcome).is_err() {
                    log::warn!("A teammate's batch finished after their request gave up");
                }
            }
        }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            while let Some(pending) = action_receiver.next().await {
                if pending.claimed.swap(true, Ordering::SeqCst) {
                    log::info!("Skipped a teammate's action that timed out while waiting");
                    continue;
                }
                let PendingAction { action, reply, .. } = pending;
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
                        let batch_sender = batch_sender.clone();
                        let file_roots = shared_file_roots.clone();
                        let agent_roots = shared_agent_roots.clone();
                        let write_check_sender = write_check_sender.clone();
                        async move {
                            start_server(
                                key,
                                hub,
                                action_sender,
                                batch_sender,
                                write_check_sender,
                                file_roots,
                                agent_roots,
                            )
                        }
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
                            this.scan_task =
                                Some(Self::watch_files(hub.clone(), this.file_roots.clone(), cx));
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

    /// Rescans each file-sharing workspace while a teammate has looked
    /// recently, in the background. The share server only hears of a change
    /// when the manifest differs.
    fn watch_files(hub: TeamShareHub, file_roots: FileRoots, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |_, cx| {
            let mut hash_caches: HashMap<String, HashCache> = HashMap::new();
            loop {
                cx.background_executor().timer(PUBLISH_INTERVAL).await;
                if !hub.has_viewers_within(VIEWER_WINDOW) {
                    continue;
                }
                let roots: Vec<(String, PathBuf, SkipList)> = lock_file_roots(&file_roots)
                    .iter()
                    .map(|(id, (root, skip_list))| (id.clone(), root.clone(), skip_list.clone()))
                    .collect();
                let mut previous_caches = std::mem::take(&mut hash_caches);
                let scans: Vec<_> = roots
                    .into_iter()
                    .map(|(id, root, skip_list)| {
                        let mut cache = previous_caches.remove(&id).unwrap_or_default();
                        let scan = cx.background_spawn(async move {
                            let scanned = scan_workspace_cached(&root, &skip_list, &mut cache);
                            (scanned, cache)
                        });
                        async move {
                            let (scanned, cache) = scan.await;
                            (id, scanned, cache)
                        }
                    })
                    .collect();
                for (id, scanned, cache) in futures::future::join_all(scans).await {
                    hash_caches.insert(id.clone(), cache);
                    match scanned {
                        Ok(manifest) => {
                            let roots = lock_file_roots(&file_roots);
                            if roots.contains_key(&id) {
                                hub.set_file_manifest(&id, manifest);
                            }
                        }
                        Err(err) => log::debug!("Couldn't scan {id}'s files: {err}"),
                    }
                }
            }
        })
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

    fn stop_sharing(&mut self, cx: &mut Context<Self>) {
        self.start_task = None;
        self.publish_task = None;
        self.tailscale_watch = None;
        self.action_task = None;
        self.batch_task = None;
        self.scan_task = None;
        self.server = None;
        self.server_key = None;
        self.feed.update(cx, |feed, _| feed.clear());
        self.sharing = SharingStatus::Off;
    }

    fn start_publishing(&mut self, cx: &mut Context<Self>) {
        self.publish_task = Some(cx.spawn(async move |this, cx| {
            let mut counts_refreshed_at: Option<Instant> = None;
            loop {
                let Ok((shared_ids, unlooked_ids)) = this.read_with(cx, |this, _| {
                    let unlooked: HashSet<String> = this
                        .shared_workspaces
                        .iter()
                        .filter(|id| !this.remote_urls.contains_key(*id))
                        .cloned()
                        .collect();
                    (this.shared_workspaces.clone(), unlooked)
                }) else {
                    break;
                };
                if let Some(state) = mnemonic_bridge::mnemonic_state_cloned() {
                    let records = cx
                        .background_spawn({
                            let state = state.clone();
                            async move { load_records(&state, Some(&shared_ids)) }
                        })
                        .await;
                    let published = match records {
                        Ok(records) => {
                            let to_look_up: Vec<(String, PathBuf)> = records
                                .iter()
                                .filter(|(summary, _)| unlooked_ids.contains(&summary.id))
                                .filter_map(|(summary, _)| {
                                    Some((summary.id.clone(), workspace_root(summary)?))
                                })
                                .collect();
                            let remotes = if to_look_up.is_empty() {
                                Vec::new()
                            } else {
                                cx.background_spawn(async move {
                                    let mut remotes = Vec::with_capacity(to_look_up.len());
                                    for (id, root) in to_look_up {
                                        remotes.push((id, origin_remote(&root).await));
                                    }
                                    remotes
                                })
                                .await
                            };
                            this.update(cx, |this, cx| {
                                for (id, remote) in remotes {
                                    if this.shared_workspaces.contains(&id) {
                                        this.remote_urls.insert(id, remote);
                                    }
                                }
                                let to_read = this.publish(records, cx);
                                (this.feed.read(cx).hub().clone(), to_read)
                            })
                        }
                        Err(err) => {
                            log::error!("Couldn't list workspaces to share: {err}");
                            this.read_with(cx, |this, cx| {
                                (this.feed.read(cx).hub().clone(), Vec::new())
                            })
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
                            .update(cx, |this, cx| this.share_saved_chats(saved, cx))
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
        let mut file_roots = HashMap::new();
        let mut agent_roots = HashMap::new();
        for (summary, _) in &records {
            let Some(root) = workspace_root(summary) else {
                continue;
            };
            if self.allows_agents(&summary.id) {
                agent_roots.insert(summary.id.clone(), root.clone());
            }
            if self.shares_files(&summary.id) {
                file_roots.insert(summary.id.clone(), (root, SkipList::default()));
            }
        }
        {
            let hub = self.feed.read(cx).hub();
            let mut roots = lock_file_roots(&self.file_roots);
            for id in roots.keys().filter(|id| !file_roots.contains_key(*id)) {
                hub.remove_file_manifest(id);
            }
            *roots = file_roots;
        }
        *lock_agent_roots(&self.agent_roots) = agent_roots;
        let flags = |workspace_id: &str| self.workspace_flags(workspace_id);
        self.feed
            .update(cx, |feed, cx| feed.publish(records, &flags, cx))
    }

    fn workspace_flags(&self, workspace_id: &str) -> WorkspaceFlags {
        WorkspaceFlags {
            step_in: self.allows_step_in(workspace_id),
            share_files: self.shares_files(workspace_id),
            edit_files: self.allows_file_edits(workspace_id),
            agents: self.allows_agents(workspace_id),
            remote_url: self.remote_urls.get(workspace_id).cloned().flatten(),
        }
    }

    /// Shares closed chats from the transcripts just read, unless meanwhile
    /// a chat was loaded or its workspace unshared.
    fn share_saved_chats(
        &mut self,
        mut saved: Vec<(TranscriptToRead, Vec<TeamEntry>)>,
        cx: &mut Context<Self>,
    ) {
        if self.server.is_none() {
            return;
        }
        saved.retain(|(transcript, _)| self.shared_workspaces.contains(&transcript.workspace_id));
        self.feed
            .update(cx, |feed, _| feed.share_saved_chats(saved));
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
    batch_sender: mpsc::UnboundedSender<PendingBatch>,
    write_check_sender: mpsc::UnboundedSender<PendingWriteCheck>,
    file_roots: FileRoots,
    agent_roots: AgentRoots,
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
    let diff_source = workspace_diff_source();
    // Runs on a request thread, which waits here while the main thread
    // carries the action out.
    let action_handler: ActionHandler = Arc::new(move |action| {
        let (reply_sender, reply_receiver) = std::sync::mpsc::channel();
        let claimed = Arc::new(AtomicBool::new(false));
        let pending = PendingAction {
            action,
            claimed: claimed.clone(),
            reply: reply_sender,
        };
        if action_sender.unbounded_send(pending).is_err() {
            return Err(TeamActionError::Failed("Sharing stopped".into()));
        }
        wait_for_reply(&reply_receiver, &claimed, ACTION_TIMEOUT)
    });
    let batch_handler: BatchHandler = Arc::new(move |request| {
        let (reply_sender, reply_receiver) = std::sync::mpsc::channel();
        let claimed = Arc::new(AtomicBool::new(false));
        let pending = PendingBatch {
            request,
            claimed: claimed.clone(),
            reply: reply_sender,
        };
        if batch_sender.unbounded_send(pending).is_err() {
            return Err(TeamActionError::Failed("Sharing stopped".into()));
        }
        wait_for_reply(&reply_receiver, &claimed, BATCH_TIMEOUT)
    });
    let agent_write_check: AgentWriteCheck = Arc::new({
        let agent_roots = agent_roots.clone();
        let display_name = display_name.clone();
        move |workspace_id: &str, path: &Path| {
            let root = lock_agent_roots(&agent_roots)
                .get(workspace_id)
                .cloned()
                .ok_or_else(|| "Teammates' agents can't work in this workspace".to_string())?;
            let (reply_sender, reply_receiver) = std::sync::mpsc::channel();
            let claimed = Arc::new(AtomicBool::new(false));
            let pending = PendingWriteCheck {
                root,
                path: path.to_path_buf(),
                display_name: display_name.clone(),
                claimed: claimed.clone(),
                reply: reply_sender,
            };
            if write_check_sender.unbounded_send(pending).is_err() {
                return Err("Sharing stopped".to_string());
            }
            wait_for_reply(&reply_receiver, &claimed, ACTION_TIMEOUT).map_err(|err| match err {
                TeamActionError::Invalid(message)
                | TeamActionError::NotAllowed(message)
                | TeamActionError::NothingToDo(message)
                | TeamActionError::Failed(message) => message,
                TeamActionError::NotLoaded => "That file isn't loaded".to_string(),
            })
        }
    });
    let files: FileSource = Arc::new({
        let file_roots = file_roots.clone();
        move |workspace_id: &str| {
            lock_file_roots(&file_roots)
                .get(workspace_id)
                .map(|(root, _)| root.clone())
        }
    });
    let bundle_source: BundleSource = Arc::new({
        let file_roots = file_roots.clone();
        move |workspace_id: &str, have: &[String]| {
            let root = lock_file_roots(&file_roots)
                .get(workspace_id)
                .map(|(root, _)| root.clone())
                .ok_or_else(|| "That workspace isn't sharing files".to_string())?;
            team_files_service::bundle_for(&root, have)
        }
    });
    let skip_list: SkipListSource = Arc::new(move |workspace_id: &str| {
        lock_file_roots(&file_roots)
            .get(workspace_id)
            .map(|(_, skip_list)| skip_list.clone())
            .unwrap_or_default()
    });
    TeamShareServer::start(
        TeamServerConfig {
            bind: IpAddr::V4(me.ip),
            port: key.port,
            display_name,
            allowed_logins,
            files,
            bundle_source,
            skip_list,
            batch_handler,
            agent_roots: Arc::new(move |workspace_id: &str| {
                lock_agent_roots(&agent_roots).get(workspace_id).cloned()
            }),
            agent_write_check,
        },
        hub,
        Arc::new(TailscaleWhois::default()),
        diff_source,
        action_handler,
    )
    .map(|server| (server, me))
}

pub(crate) fn workspace_diff_source() -> DiffSource {
    Arc::new(|workspace_id: &str| {
        let state = mnemonic_bridge::mnemonic_state_cloned()
            .ok_or_else(|| "Mnemonic isn't ready".to_string())?;
        git_review_service::get_workspace_full_diff(&state, workspace_id)
    })
}

/// Waits on a request thread for the main thread's answer. Once the main
/// thread has claimed the work, it is waited for to the end, as it will run
/// whatever the requester is told.
pub(crate) fn wait_for_reply<T>(
    reply_receiver: &std::sync::mpsc::Receiver<Result<T, TeamActionError>>,
    claimed: &AtomicBool,
    timeout: Duration,
) -> Result<T, TeamActionError> {
    let outcome = match reply_receiver.recv_timeout(timeout) {
        Err(RecvTimeoutError::Timeout) if claimed.swap(true, Ordering::SeqCst) => reply_receiver
            .recv()
            .map_err(|_| RecvTimeoutError::Disconnected),
        outcome => outcome,
    };
    match outcome {
        Ok(outcome) => outcome,
        Err(RecvTimeoutError::Timeout) => Err(TeamActionError::Failed("Timed out".into())),
        Err(RecvTimeoutError::Disconnected) => {
            Err(TeamActionError::Failed("Sharing stopped".into()))
        }
    }
}

fn check_agent_write(
    root: &Path,
    path: &Path,
    display_name: &str,
    cx: &App,
) -> Result<(), TeamActionError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        TeamActionError::NotAllowed(format!("{} is outside the workspace", path.display()))
    })?;
    let relative = relative
        .to_str()
        .ok_or_else(|| {
            TeamActionError::NotAllowed(format!("{} isn't a UTF-8 path", relative.display()))
        })?
        .replace('\\', "/");
    if unsaved_paths(root, cx).contains(&relative) {
        return Err(TeamActionError::Failed(format!(
            "{relative} has unsaved changes in {display_name}'s editor"
        )));
    }
    Ok(())
}

/// The paths, relative to `root`, of files with unsaved edits in any open
/// editor.
pub(crate) fn unsaved_paths(root: &Path, cx: &App) -> HashSet<String> {
    let mut unsaved = HashSet::new();
    for window in open_windows(cx) {
        let project = window.read(cx).project().read(cx);
        for project_path in project.dirty_buffers(cx) {
            let Some(absolute_path) = project.absolute_path(&project_path, cx) else {
                continue;
            };
            if let Some(relative) = absolute_path
                .strip_prefix(root)
                .ok()
                .and_then(|relative| relative.to_str())
            {
                unsaved.insert(relative.replace('\\', "/"));
            }
        }
    }
    unsaved
}

/// Applies a teammate's batch to the workspace it names, checking again
/// here that they may, records it, and tells the owner.
async fn apply_teammate_batch(
    team: &WeakEntity<TeamShare>,
    request: TeamBatchRequest,
    cx: &mut gpui::AsyncApp,
) -> Result<TeamBatchOutcome, TeamActionError> {
    let workspace_id = request.workspace_id.clone();
    let actor = actor_name(&request.from, &request.login);
    let checked = cx.update(|cx| {
        let team = team.upgrade()?;
        let team = team.read(cx);
        let workspace_name = team
            .feed
            .read(cx)
            .hub()
            .workspaces()
            .into_iter()
            .find(|workspace| workspace.id == workspace_id)
            .map(|workspace| workspace.name)
            .unwrap_or_else(|| workspace_id.clone());
        let allowed = if !team.allows_file_edits(&workspace_id) {
            Err(TeamActionError::NotAllowed(
                "Editing files is off for this workspace".into(),
            ))
        } else {
            lock_file_roots(&team.file_roots)
                .get(&workspace_id)
                .cloned()
                .ok_or_else(|| {
                    TeamActionError::NotAllowed("Sharing files is off for this workspace".into())
                })
        };
        let allowed =
            allowed.map(|(root, skip_list)| (unsaved_paths(&root, cx), root, skip_list));
        Some((workspace_name, allowed))
    });
    let Some((workspace_name, allowed)) = checked else {
        return Err(TeamActionError::Failed("Sharing stopped".into()));
    };
    let (unsaved, root, skip_list) = match allowed {
        Ok(allowed) => allowed,
        Err(err) => {
            show_batch_notice(&workspace_id, batch_refused(&actor, &workspace_name, &err), cx);
            return Err(err);
        }
    };
    let caller = TeamCaller {
        login: request.login.clone(),
        name: request.from.clone(),
    };
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let applied = cx
        .background_spawn({
            let workspace_id = workspace_id.clone();
            async move {
                let lock = team_files_service::workspace_files_lock(&workspace_id);
                let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let record = team_files_service::apply_batch(
                    &root,
                    &skip_list,
                    &request.batch,
                    &unsaved,
                    &caller,
                    &now,
                )
                .map_err(TeamActionError::Invalid)?;
                let saved = mnemonic_bridge::mnemonic_state_cloned()
                    .ok_or_else(|| "Mnemonic isn't ready".to_string())
                    .and_then(|app| {
                        team_files_service::save_batch_record(&app.data_dir, &workspace_id, &record)
                    });
                if let Err(err) = &saved {
                    log::error!("Couldn't save a teammate's batch {}: {err}", record.id);
                }
                Ok((record, saved.err()))
            }
        })
        .await;
    let (outcome, message) = match applied {
        Ok((record, save_error)) => {
            if let Err(err) = team.update(cx, |team, cx| {
                team.batch_generation += 1;
                cx.notify();
            }) {
                log::warn!("Couldn't announce a teammate's batch: {err}");
            }
            let outcome = record.outcome();
            let mut message = batch_notice(
                &actor,
                outcome.applied.len(),
                outcome.held.len(),
                &workspace_name,
            );
            if let Some(err) = save_error {
                message.push_str(&format!(" (couldn't record it: {err})"));
            }
            (Ok(outcome), message)
        }
        Err(err) => {
            let message = batch_refused(&actor, &workspace_name, &err);
            (Err(err), message)
        }
    };
    show_batch_notice(&workspace_id, message, cx);
    outcome
}

fn batch_refused(actor: &str, workspace_name: &str, err: &TeamActionError) -> String {
    let reason = match err {
        TeamActionError::Invalid(reason)
        | TeamActionError::NotAllowed(reason)
        | TeamActionError::NothingToDo(reason)
        | TeamActionError::Failed(reason) => reason.clone(),
        TeamActionError::NotLoaded => "the workspace isn't open".to_string(),
    };
    format!("{actor}'s changes to {workspace_name} weren't applied: {reason}")
}

fn show_batch_notice(workspace_id: &str, message: String, cx: &mut gpui::AsyncApp) {
    cx.update(|cx| {
        if let Some(window) =
            find_workspace_window(workspace_id, cx).or_else(|| open_windows(cx).into_iter().next())
        {
            show_step_in_notice(&window, message, cx);
        }
    });
}

fn batch_notice(actor: &str, applied: usize, held: usize, workspace_name: &str) -> String {
    let files = |count: usize| if count == 1 { "file" } else { "files" };
    if applied == 0 && held > 0 {
        return format!(
            "{actor} sent {held} {} in {workspace_name}, all held for you",
            files(held)
        );
    }
    let mut message = format!(
        "{actor} changed {applied} {} in {workspace_name}",
        files(applied)
    );
    if held > 0 {
        message.push_str(&format!(", {held} held for you"));
    }
    message
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
    // None while the chat's agent is still connecting.
    let idle = chat
        .thread
        .as_ref()
        .map(|thread| thread.read(cx).status() == ThreadStatus::Idle);
    match action.kind {
        TeamActionKind::Stop => {
            let Some(idle) = idle else {
                return Err(TeamActionError::NotLoaded);
            };
            if idle {
                return Err(TeamActionError::NothingToDo(
                    "The agent isn't running".into(),
                ));
            }
            chat.view
                .update(cx, |view, cx| view.stop_chat(&action.chat_id, cx));
            show_step_in_notice(
                &chat.window,
                format!("{} stopped the agent in {}", actor(&action), chat.title),
                cx,
            );
            Ok(TeamActionOutcome {})
        }
        TeamActionKind::Permission {
            ref tool_call_id,
            ref option_id,
        } => {
            let Some(thread) = chat.thread else {
                return Err(TeamActionError::NotLoaded);
            };
            let (tool_title, chosen) = answer_permission(
                &thread,
                tool_call_id,
                option_id,
                PermissionScope::StepInOnceOnly,
                cx,
            )?;
            let answer = match chosen {
                acp::PermissionOptionKind::RejectOnce | acp::PermissionOptionKind::RejectAlways => {
                    "rejected"
                }
                _ => "allowed",
            };
            show_step_in_notice(
                &chat.window,
                format!("{} {answer} {tool_title} in {}", actor(&action), chat.title),
                cx,
            );
            Ok(TeamActionOutcome {})
        }
    }
}

fn actor(action: &TeamAction) -> String {
    actor_name(&action.from, &action.login)
}

/// Who acted, as the owner's notices show it. A display name is whatever
/// the teammate chose, so their login is shown with it.
fn actor_name(from: &str, login: &str) -> String {
    if from == login {
        from.to_string()
    } else {
        format!("{from} ({login})")
    }
}

pub(crate) fn show_step_in_notice(window: &Entity<Workspace>, message: String, cx: &mut App) {
    window.update(cx, |workspace, cx| {
        workspace.show_toast(
            Toast::new(NotificationId::unique::<TeamStepInToast>(), message),
            cx,
        );
    });
}

/// The options a teammate may pick from: only once-only answers, as an
/// "always" answer leaves a standing rule in the owner's agent, which is
/// theirs to make.
pub(crate) fn once_only_options(options: &PermissionOptions) -> Vec<acp::PermissionOption> {
    let (candidates, from_dropdown): (Vec<&acp::PermissionOption>, bool) = match options {
        PermissionOptions::Flat(options) => (options.iter().collect(), false),
        PermissionOptions::Dropdown(choices)
        | PermissionOptions::DropdownWithPatterns { choices, .. } => (
            choices
                .iter()
                .flat_map(|choice| [&choice.allow, &choice.deny])
                .collect(),
            true,
        ),
    };
    let mut offered: Vec<acp::PermissionOption> = Vec::new();
    for option in candidates {
        let name = match option.kind {
            acp::PermissionOptionKind::AllowOnce => "Allow once",
            acp::PermissionOptionKind::RejectOnce => "Deny once",
            _ => continue,
        };
        if offered
            .iter()
            .any(|offered| offered.option_id == option.option_id)
        {
            continue;
        }
        let mut option = option.clone();
        // A dropdown names its choice, not the allow and deny inside it,
        // so both would show as the same button.
        if from_dropdown {
            option.name = name.to_string();
        }
        offered.push(option);
    }
    offered
}

/// Every option the agent offers, a dropdown contributing each choice's
/// allow and deny.
fn all_options(options: &PermissionOptions) -> Vec<acp::PermissionOption> {
    let choices = match options {
        PermissionOptions::Flat(options) => return options.clone(),
        PermissionOptions::Dropdown(choices)
        | PermissionOptions::DropdownWithPatterns { choices, .. } => choices,
    };
    let mut offered: Vec<acp::PermissionOption> = Vec::new();
    for choice in choices {
        for (verb, option) in [("Allow", &choice.allow), ("Deny", &choice.deny)] {
            if offered
                .iter()
                .any(|offered| offered.option_id == option.option_id)
            {
                continue;
            }
            let mut option = option.clone();
            // A choice's allow and deny carry the choice's name, so both
            // would show as the same button.
            option.name = format!("{verb}: {}", option.name);
            offered.push(option);
        }
    }
    offered
}

/// Only requests for permission are shared with teammates. An action
/// choice, such as saving or discarding a file's unsaved changes, is the
/// owner's to make: with `All` it is listed without options, since the
/// wire can't otherwise tell a tool call waiting on the Mac from one that
/// hasn't started, and the phone shows it as one to answer on the Mac.
pub(crate) fn thread_permissions(
    thread: &AcpThread,
    scope: PermissionScope,
) -> Vec<TeamPermissionRequest> {
    thread
        .entries()
        .iter()
        .filter_map(|entry| {
            let AgentThreadEntry::ToolCall(ToolCall {
                id,
                status: ToolCallStatus::WaitingForConfirmation { options, kind, .. },
                ..
            }) = entry
            else {
                return None;
            };
            let options = match (scope, kind) {
                (PermissionScope::StepInOnceOnly, AuthorizationKind::PermissionGrant) => {
                    let options = once_only_options(options);
                    if options.is_empty() {
                        return None;
                    }
                    options
                }
                (PermissionScope::StepInOnceOnly, AuthorizationKind::ActionChoice) => return None,
                (PermissionScope::All, AuthorizationKind::PermissionGrant) => all_options(options),
                (PermissionScope::All, AuthorizationKind::ActionChoice) => Vec::new(),
            };
            Some(TeamPermissionRequest {
                tool_call_id: id.0.to_string(),
                options,
            })
        })
        .collect()
}

/// Answers a waiting permission request with an option `scope` offers, and
/// returns the tool call's title and the kind of answer.
pub(crate) fn answer_permission(
    thread: &Entity<AcpThread>,
    tool_call_id: &str,
    option_id: &str,
    scope: PermissionScope,
    cx: &mut App,
) -> Result<(String, acp::PermissionOptionKind), TeamActionError> {
    let tool_call_id = acp::ToolCallId::new(tool_call_id.to_string());
    let (outcome, title) = {
        let thread = thread.read(cx);
        let no_longer_waiting =
            || TeamActionError::NothingToDo("The permission request is no longer waiting".into());
        let Some((_, tool_call)) = thread.tool_call(&tool_call_id) else {
            return Err(no_longer_waiting());
        };
        let ToolCallStatus::WaitingForConfirmation { options, kind, .. } = &tool_call.status else {
            return Err(no_longer_waiting());
        };
        if *kind != AuthorizationKind::PermissionGrant {
            return Err(TeamActionError::NotAllowed(match scope {
                PermissionScope::StepInOnceOnly => "Only the owner can answer this".into(),
                PermissionScope::All => "Answer this on the Mac".into(),
            }));
        }
        let outcome = match scope {
            PermissionScope::StepInOnceOnly => once_only_options(options)
                .into_iter()
                .find(|option| option.option_id.0.as_ref() == option_id)
                .map(|option| SelectedPermissionOutcome::new(option.option_id, option.kind)),
            PermissionScope::All => outcome_for_option(options, option_id),
        };
        let Some(outcome) = outcome else {
            return Err(TeamActionError::NotAllowed(match scope {
                PermissionScope::StepInOnceOnly => "That answer isn't offered to teammates".into(),
                PermissionScope::All => "That answer isn't offered".into(),
            }));
        };
        (outcome, tool_call_title(tool_call, cx))
    };
    let chosen = outcome.option_kind;
    thread.update(cx, |thread, cx| {
        thread.authorize_tool_call(tool_call_id, outcome, cx)
    });
    Ok((title, chosen))
}

/// The answer the Mac would give for the option offered as `option_id`. A
/// dropdown's choice is built the way its own dropdown builds it, from the
/// first choice holding that option, the one `all_options` offers.
fn outcome_for_option(
    options: &PermissionOptions,
    option_id: &str,
) -> Option<SelectedPermissionOutcome> {
    let choices = match options {
        PermissionOptions::Flat(options) => {
            return options
                .iter()
                .find(|option| option.option_id.0.as_ref() == option_id)
                .map(|option| {
                    SelectedPermissionOutcome::new(option.option_id.clone(), option.kind)
                });
        }
        PermissionOptions::Dropdown(choices)
        | PermissionOptions::DropdownWithPatterns { choices, .. } => choices,
    };
    choices.iter().find_map(|choice| {
        if choice.allow.option_id.0.as_ref() == option_id {
            Some(choice.build_outcome(true))
        } else if choice.deny.option_id.0.as_ref() == option_id {
            Some(choice.build_outcome(false))
        } else {
            None
        }
    })
}

pub(crate) fn workspace_root(summary: &WorkspaceSummary) -> Option<PathBuf> {
    summary
        .workspace_root_path
        .as_deref()
        .or(summary.selected_worktree_path.as_deref())
        .map(PathBuf::from)
}

/// The normalized `origin` of the repository at `root`, None when it has no
/// such remote.
async fn origin_remote(root: &Path) -> Option<String> {
    let output = util::command::new_command("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(root)
        .output()
        .await
        .log_err()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout);
    Some(normalize_remote_url(&url)).filter(|url| !url.is_empty())
}

pub(crate) fn agent_state(
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

pub(crate) fn thread_entries(thread: &AcpThread, cx: &App) -> Vec<TeamEntry> {
    thread
        .entries()
        .iter()
        .map(|entry| team_entry(entry, cx))
        .collect()
}

pub(crate) fn thread_plan(thread: &AcpThread, cx: &App) -> Vec<acp::PlanEntry> {
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
            // Not animated: an animation re-renders the whole panel on every
            // frame for as long as any agent runs.
            Icon::new(IconName::ArrowCircle)
                .size(IconSize::XSmall)
                .color(Color::Muted)
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
    use crate::chat_feed::test_support::{
        allow_always_and_once_options, is_waiting, new_thread, save_or_discard_options,
        thread_waiting_on,
    };
    use PermissionScope::StepInOnceOnly;
    use acp_thread::{AuthorizationKind, PermissionOptions};
    use gpui::TestAppContext;
    use settings::SettingsStore;

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

    #[gpui::test]
    async fn only_once_only_options_are_offered_to_teammates(cx: &mut TestAppContext) {
        let (thread, _answer) = thread_waiting_on(
            allow_always_and_once_options(),
            AuthorizationKind::PermissionGrant,
            cx,
        )
        .await;
        let requests = thread.read_with(cx, |thread, _| {
            thread_permissions(thread, PermissionScope::StepInOnceOnly)
        });
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
            AuthorizationKind::PermissionGrant,
            cx,
        )
        .await;
        let requests = always_only.read_with(cx, |thread, _| {
            thread_permissions(thread, PermissionScope::StepInOnceOnly)
        });
        assert!(requests.is_empty());

        let (dirty_buffer, _answer) = thread_waiting_on(
            save_or_discard_options(),
            AuthorizationKind::ActionChoice,
            cx,
        )
        .await;
        let requests = dirty_buffer.read_with(cx, |thread, _| {
            thread_permissions(thread, PermissionScope::StepInOnceOnly)
        });
        assert!(requests.is_empty());
    }

    #[gpui::test]
    async fn a_teammate_can_only_pick_an_offered_once_only_option(cx: &mut TestAppContext) {
        let (thread, _answer) = thread_waiting_on(
            allow_always_and_once_options(),
            AuthorizationKind::PermissionGrant,
            cx,
        )
        .await;

        for option_id in ["always", "never-offered"] {
            let answered = cx.update(|cx| {
                answer_permission(&thread, "run-tests", option_id, StepInOnceOnly, cx)
            });
            assert!(
                matches!(answered, Err(TeamActionError::NotAllowed(_))),
                "{option_id} gave {answered:?}"
            );
            assert!(is_waiting(&thread, cx));
        }

        let answered =
            cx.update(|cx| answer_permission(&thread, "run-tests", "allow", StepInOnceOnly, cx));
        assert_eq!(
            answered,
            Ok((
                "Run the tests".to_string(),
                acp::PermissionOptionKind::AllowOnce
            ))
        );
        assert!(!is_waiting(&thread, cx));

        let answered =
            cx.update(|cx| answer_permission(&thread, "run-tests", "allow", StepInOnceOnly, cx));
        assert!(matches!(answered, Err(TeamActionError::NothingToDo(_))));

        let (dirty_buffer, _answer) = thread_waiting_on(
            save_or_discard_options(),
            AuthorizationKind::ActionChoice,
            cx,
        )
        .await;
        let answered = cx.update(|cx| {
            answer_permission(&dirty_buffer, "run-tests", "discard", StepInOnceOnly, cx)
        });
        assert!(
            matches!(answered, Err(TeamActionError::NotAllowed(_))),
            "discard gave {answered:?}"
        );
        assert!(is_waiting(&dirty_buffer, cx));
    }

    #[gpui::test]
    async fn sharing_turns_on_step_in_files_and_edits(cx: &mut TestAppContext) {
        let team = cx.new(TeamShare::new);
        team.update(cx, |team, cx| {
            team.set_workspace_shared("workspace", true, cx);
            assert!(team.allows_step_in("workspace"));
            assert!(team.shares_files("workspace"));
            assert!(team.allows_file_edits("workspace"));
        });
    }

    #[gpui::test]
    async fn unsharing_takes_the_folder_and_files_from_the_server(cx: &mut TestAppContext) {
        let team = cx.new(TeamShare::new);
        team.update(cx, |team, cx| {
            team.set_workspace_shared("workspace", true, cx);
            assert!(team.allows_agents("workspace"));
            lock_agent_roots(&team.agent_roots).insert("workspace".into(), "/work".into());
            lock_file_roots(&team.file_roots)
                .insert("workspace".into(), (PathBuf::from("/root"), SkipList::default()));
            team.set_workspace_shared("workspace", false, cx);
            assert!(!team.allows_agents("workspace"));
            assert!(!lock_agent_roots(&team.agent_roots).contains_key("workspace"));
            assert!(lock_file_roots(&team.file_roots).is_empty());
        });
    }

    #[gpui::test]
    fn agent_writes_outside_the_root_or_not_utf8_are_refused(cx: &mut TestAppContext) {
        use std::os::unix::ffi::OsStrExt as _;
        let root = Path::new("/work/root");
        cx.update(|cx| {
            assert!(check_agent_write(root, &root.join("src/main.rs"), "Jay", cx).is_ok());
            assert!(check_agent_write(root, Path::new("/elsewhere/main.rs"), "Jay", cx).is_err());
            let not_utf8 = root.join(std::ffi::OsStr::from_bytes(b"bad\xff.rs"));
            assert!(check_agent_write(root, &not_utf8, "Jay", cx).is_err());
        });
    }
}
